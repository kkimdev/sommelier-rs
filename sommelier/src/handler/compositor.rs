/*
Copyright 2026 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use crate::allocator::{Allocator, DmabufFence};
use crate::handler::aura_objects;
use crate::handler::shm::{copy_shm_damage, damage_reaches_buffer, ShmCopyError, ShmCopyLayout};
use crate::handler::window_placement;
use crate::protocols::viewporter::{wp_viewport, wp_viewporter};
use crate::protocols::wayland::wl_compositor::WlCompositorHandler;
use crate::protocols::wayland::wl_region::WlRegionHandler;
use crate::protocols::wayland::wl_shm;
use crate::protocols::wayland::wl_subcompositor::WlSubcompositorHandler;
use crate::protocols::wayland::wl_subsurface::WlSubsurfaceHandler;
use crate::protocols::wayland::wl_surface::{self, WlSurfaceHandler, REQ_OFFSET_SINCE};
use crate::protocols::xdg_shell::xdg_toplevel;
use crate::state::{
    is_valid_transform, BufferRect, BufferSpace, BufferState, Context, DamageRegion, FenceWait,
    PostDispatch, ShadowTable, SurfaceAttachment, SurfaceCommit, SurfaceMapping, SurfaceRect,
    SurfaceState, ViewportState,
};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{queue_or_drop, Action, Outgoing};
use log::trace;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

pub struct CompositorHandler;

/// RAII guard for the VirtWL dma-buf CPU-write interval.
///
/// A copy path can fail while mapping a BO or while validating a damage
/// rectangle. Keeping the matching END ioctl in `Drop` prevents a panic or a
/// future early-return path from leaving the host dma-buf permanently locked.
struct DmabufWriteSync<'a> {
    channel: &'a crate::virtwl_channel::VirtWaylandChannel,
    fd: BorrowedFd<'a>,
    ended: bool,
}

impl<'a> DmabufWriteSync<'a> {
    fn begin(
        channel: Option<&'a Arc<crate::virtwl_channel::VirtWaylandChannel>>,
        fd: Option<BorrowedFd<'a>>,
    ) -> Option<Self> {
        let channel = channel?;
        let fd = fd?;
        channel
            .sync(
                fd,
                crate::virtwl::DMA_BUF_SYNC_START | crate::virtwl::DMA_BUF_SYNC_WRITE,
            )
            .map(|()| Self {
                channel,
                fd,
                ended: false,
            })
            .map_err(|error| {
                log::warn!("VirtWL dma-buf begin-write sync failed: {}", error);
            })
            .ok()
    }

    fn end(mut self) -> bool {
        self.ended = true;
        match self.channel.sync(
            self.fd,
            crate::virtwl::DMA_BUF_SYNC_END | crate::virtwl::DMA_BUF_SYNC_WRITE,
        ) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("VirtWL dma-buf end-write sync failed: {}", error);
                false
            }
        }
    }
}

impl Drop for DmabufWriteSync<'_> {
    fn drop(&mut self) {
        if !self.ended {
            if let Err(error) = self.channel.sync(
                self.fd,
                crate::virtwl::DMA_BUF_SYNC_END | crate::virtwl::DMA_BUF_SYNC_WRITE,
            ) {
                log::warn!(
                    "VirtWL dma-buf end-write sync failed while unwinding: {}",
                    error
                );
            }
            self.ended = true;
        }
    }
}

/// The fence a commit of native buffer `guest_buffer_id` waits for while
/// the guest GPU still writes the buffer, checked without waiting.
fn pending_native_fence(
    ctx: &mut Context,
    guest_buffer_id: u32,
) -> std::io::Result<Option<FenceWait>> {
    if ctx.native_buffer_uses_implicit_sync(guest_buffer_id) {
        return Ok(None);
    }
    let Some(planes) = ctx.native_buffer_sync_fds(guest_buffer_id) else {
        return Ok(None);
    };
    // A buffer has at most four planes.
    let plane_count = planes.len() as u32;
    let mut needs_implicit_sync = false;
    for plane in planes {
        match plane_fence(ctx, plane.as_fd())? {
            DmabufFence::Signalled => {}
            DmabufFence::Pending(fence) => {
                let timeout = fence.timeout() * plane_count;
                return Ok(Some(FenceWait { fence, timeout }));
            }
            DmabufFence::ImplicitSyncFallback => {
                needs_implicit_sync = true;
                break;
            }
        }
    }

    if needs_implicit_sync {
        if !ctx.enable_native_buffer_implicit_sync(guest_buffer_id) {
            return Err(std::io::Error::other(
                "native dma-buf disappeared while enabling implicit synchronization",
            ));
        }
        log::warn!(
            "Native dma-buf synchronization ioctls are unavailable for guest buffer {}; using implicit synchronization",
            guest_buffer_id
        );
    }
    Ok(None)
}

/// Whether the guest GPU has finished writing one plane of a native buffer.
fn plane_fence(ctx: &Context, plane: BorrowedFd<'_>) -> std::io::Result<DmabufFence> {
    #[cfg(test)]
    if let Some(stand_in) = ctx.plane_fences_for_test {
        use crate::allocator::PendingFence;
        use crate::state::PlaneFenceForTest;
        return Ok(
            match crate::allocator::sync_file_fence(plane.try_clone_to_owned()?)? {
                DmabufFence::Pending(_) if matches!(stand_in, PlaneFenceForTest::Busy) => {
                    DmabufFence::Pending(PendingFence::Busy)
                }
                fence => fence,
            },
        );
    }
    crate::allocator::dmabuf_fence(plane, ctx.allocator.as_ref())
}

impl WlCompositorHandler for CompositorHandler {}

pub(crate) fn native_wayland_app_id(vm_identifier: &str, app_id: &str) -> String {
    format!("org.chromium.guest_os.{}.wayland.{}", vm_identifier, app_id)
}

/// Record the XDG role of a forwarded `xdg_surface.get_toplevel`
/// ([`PostDispatch::XdgToplevelCreated`]). Running only after the forward
/// avoids stale application/surface state when validation rejects the
/// request. The placement Aura toplevel is attached only after the role is
/// recorded, so it never exists for a rejected request, and the forward
/// has mapped the toplevel's host ID that it extends.
pub(crate) fn record_xdg_toplevel(
    ctx: &mut Context,
    xdg_surface_guest_id: u32,
    xdg_toplevel_guest_id: u32,
) {
    debug_assert!(
        ctx.shadow_table
            .get_host_id(xdg_toplevel_guest_id)
            .is_some(),
        "a forwarded get_toplevel has mapped its new ID"
    );
    let Some(wl_surface_guest_id) = ctx
        .xdg_roles
        .add_toplevel(xdg_surface_guest_id, xdg_toplevel_guest_id)
    else {
        return;
    };
    window_placement::attach_toplevel(ctx, xdg_toplevel_guest_id, wl_surface_guest_id);
}

/// The size of the buffer `state` shows, which outlives a destroyed
/// `wl_buffer`, or `None` without one.
fn shown_buffer_size(ctx: &Context, state: &SurfaceState) -> Option<(i32, i32)> {
    state
        .current_buffer_id()
        .and_then(|buffer_id| ctx.buffer_dimensions(buffer_id))
        .or(state.current_buffer_dimensions())
}

/// Log, once per surface, `problem`: a viewport rule the proxy leaves
/// unenforced, as Exo does (see INVARIANTS, "Protocol errors"). Called once
/// the commit can no longer roll back, which would clear the flag and repeat
/// the log on every frame.
fn log_unenforced_viewport_rule(
    ctx: &mut Context,
    surface_id: u32,
    problem: &str,
    mapping: &SurfaceMapping,
) {
    let Some(surface) = ctx.surfaces.get_mut(&surface_id) else {
        return;
    };
    if !std::mem::replace(&mut surface.logged_viewport_rule, true) {
        log::warn!(
            "wl_surface {} has {}, which Exo accepts, so the commit is forwarded: {:?}",
            surface_id,
            problem,
            mapping
        );
    }
}

/// Build the host damage requests and the commit for one guest commit, or
/// `None` when the surface has no host object. The host takes surface-local
/// damage only, so buffer damage goes through `mapping`.
fn build_surface_commit_messages(
    ctx: &Context,
    surface_id: u32,
    surface_damage: &[SurfaceRect],
    buffer_damage: &[BufferRect],
    mapping: &SurfaceMapping,
    force_full_damage: bool,
) -> Option<Vec<Outgoing>> {
    let host_surface_id = ctx.shadow_table.get_host_id(surface_id)?;
    let mut damage: Vec<SurfaceRect> = Vec::new();
    if force_full_damage {
        damage.push(mapping.surface_rect());
    }
    damage.extend(surface_damage.iter().map(|&rect| rect.outset()));
    damage.extend(buffer_damage.iter().map(|&rect| mapping.to_surface(rect)));

    // The host compositor treats damage as a unioned region. Coalescing here
    // keeps a GTK frame with many overlapping requests from becoming a long
    // sequence of redundant Wayland messages as well as redundant copies.
    let mut messages: Vec<Outgoing> = crate::handler::shm::coalesce_damage_rects(&damage)
        .into_iter()
        .map(|rect| {
            wl_surface::req::damage(host_surface_id, rect.x, rect.y, rect.width, rect.height)
        })
        .collect();
    messages.push(wl_surface::req::commit(host_surface_id));
    Some(messages)
}

/// The buffer whose host copy `commit` refreshes before it reaches the host.
///
/// An attached buffer always is. A commit without an attach still damages
/// the surface's current buffer, which the host samples again, so the SHM
/// Commit Copy Invariant refreshes its copy too, while the host holds the
/// buffer. Once the host has released it, the client owns the storage and
/// may be drawing its next frame there (`wl_buffer.release`): reading it
/// could tear, and the host no longer samples it.
fn buffer_to_copy(ctx: &Context, commit: &SurfaceCommit) -> Option<u32> {
    match commit.attachment {
        SurfaceAttachment::Attach(buffer_id) => Some(buffer_id),
        SurfaceAttachment::Unchanged => commit
            .state
            .current_buffer_id()
            .filter(|&buffer_id| ctx.buffer_is_submitted(buffer_id)),
        SurfaceAttachment::Detach => None,
    }
}

/// Copy what a locally bridged SHM buffer's host copy lacks before it is
/// shown on `surface_id`.
///
/// Native linux-dmabuf buffers have no local copy state and are immediately
/// ready. Stale pixels are cleared only by a successful copy, so a failed
/// one loses nothing; it still stales the whole host copy, as a
/// conservative reset after the host storage misbehaved.
fn copy_surface_buffer(
    ctx: &mut Context,
    surface_id: u32,
    buffer_id: u32,
) -> Result<(), ShmCopyError> {
    let Some(host_id) = ctx.render_buffer_host_id(buffer_id) else {
        return Ok(());
    };
    let Some(resources) = ctx.local_buffer_copy_resources(host_id) else {
        return Ok(());
    };
    let buffer = resources.buffer;
    let damage = buffer.stale.to_copy_on(surface_id);
    // Nothing to copy needs no dma-buf sync or BO map either. Skipping them
    // also keeps a commit without damage, such as one that acknowledges a
    // configure, from being dropped when the host storage misbehaves.
    if !damage_reaches_buffer(&damage, buffer) {
        buffer.stale.copied_on(surface_id);
        return Ok(());
    }
    let result = copy_to_host_storage(buffer, resources.allocator, resources.channel, &damage);
    match &result {
        Ok(()) => {
            buffer.stale.copied_on(surface_id);
            // A 4K frame can reach this path every refresh. Keep the
            // per-frame diagnostic at trace level.
            trace!(
                "Copied damaged SHM buffer: dst_stride={}, src_stride={}, \
                 width={}, height={}, format={:#010x}, buffer.offset={}, \
                 dest_size={}",
                buffer.bo_stride,
                buffer.stride,
                buffer.width,
                buffer.height,
                buffer.format,
                buffer.offset,
                buffer.dest_size
            );
        }
        Err(error) => {
            log::warn!("SHM copy for guest buffer {} failed: {}", buffer_id, error);
            buffer.stale.invalidate();
        }
    }
    result
}

/// Copy `damage` from `buffer`'s pool into its host storage, inside the
/// VirtWL dma-buf CPU-write interval when the storage needs one.
fn copy_to_host_storage(
    buffer: &mut BufferState,
    allocator: Option<&Allocator>,
    channel: Option<&Arc<VirtWaylandChannel>>,
    damage: &DamageRegion<BufferSpace>,
) -> Result<(), ShmCopyError> {
    let Some(layout) = ShmCopyLayout::for_buffer(buffer) else {
        return Err(ShmCopyError::Destination);
    };
    let sync_guard = if buffer.dmabuf_sync {
        let fd = buffer.dmabuf_fd.as_ref().map(AsFd::as_fd);
        let Some(guard) = DmabufWriteSync::begin(channel, fd) else {
            log::warn!("VirtWL dma-buf buffer has no channel or backing fd");
            return Err(ShmCopyError::Destination);
        };
        Some(guard)
    } else {
        None
    };
    let result = if !buffer.dest_ptr.is_null() {
        copy_shm_damage(&buffer.pool_file, buffer.dest_ptr, layout, damage)
    } else if let (Some(allocator), Some(bo)) = (allocator, buffer.bo.as_mut()) {
        // `layout` holds the buffer's non-negative i32 dimensions, which fit
        // in u32.
        match bo.map_mut(
            &allocator.device,
            0,
            0,
            layout.width as u32,
            layout.height as u32,
            |mapped| {
                let layout = ShmCopyLayout {
                    dest_size: mapped.buffer().len(),
                    dst_stride: mapped.stride() as usize,
                    ..layout
                };
                copy_shm_damage(
                    &buffer.pool_file,
                    mapped.buffer_mut().as_mut_ptr(),
                    layout,
                    damage,
                )
            },
        ) {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                log::warn!("GBM BO map failed: {}", error);
                Err(ShmCopyError::Destination)
            }
            Err(_) => {
                log::warn!("GBM BO belongs to a different allocator device");
                Err(ShmCopyError::Destination)
            }
        }
    } else {
        // No destination mapping is available. This can happen only for a
        // malformed or unsupported allocation path.
        Err(ShmCopyError::Destination)
    };
    let sync_ended = sync_guard.is_none_or(DmabufWriteSync::end);
    result.and(if sync_ended {
        Ok(())
    } else {
        Err(ShmCopyError::Destination)
    })
}

/// Release the host Aura objects of a wl_surface that is ending: the Aura
/// toplevels of its xdg roles, which end with it, and its Aura surface.
///
/// They must reach the host before the wl_surface destructor (see
/// `aura_objects::release`), so the destructor's handler releases them
/// before it queues that, and [`forget_surface`] finds nothing left to
/// release.
fn release_surface_aura_objects(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
    wl_surface_host_id: u32,
) {
    // xdg objects are separate guest objects, but their roles resolve back
    // to this wl_surface. End them now so a later client ID reuse cannot
    // associate a new toplevel with the destroyed surface.
    for xdg_toplevel_id in ctx.xdg_roles.remove_wl_surface(wl_surface_guest_id) {
        window_placement::release_toplevel(ctx, xdg_toplevel_id);
    }
    aura_objects::release_host_zaura_surface(ctx, wl_surface_host_id);
}

/// Forget guest wl_surface `wl_surface_guest_id` and its host object
/// `wl_surface_host_id`, which ended (see `crate::state::lifecycle`): its
/// roles and Aura objects, its ARC identity, outputs, GTK surfaces,
/// viewports and pixel state, the buffer uses it held, and its keyboard
/// focus, which ends with a text-input leave while the guest surface ID is
/// still valid.
pub(crate) fn forget_surface(ctx: &mut Context, wl_surface_guest_id: u32, wl_surface_host_id: u32) {
    release_surface_aura_objects(ctx, wl_surface_guest_id, wl_surface_host_id);
    ctx.window_placement.forget_surface(wl_surface_guest_id);
    ctx.outputs.surface_destroyed(wl_surface_host_id);
    let gtk_surface_ids = ctx
        .gtk_surfaces
        .iter()
        .filter_map(|(&gtk_surface_id, state)| {
            (state.wl_surface_id == wl_surface_guest_id).then_some(gtk_surface_id)
        })
        .collect::<Vec<_>>();
    for gtk_surface_id in gtk_surface_ids {
        if let Some(state) = ctx.gtk_surfaces.remove(&gtk_surface_id) {
            if let Some(shell) = ctx.gtk_shells.get_mut(&state.shell_id) {
                shell.surfaces.remove(&gtk_surface_id);
            }
        }
    }
    // A submitted buffer may have been detached from another surface and
    // still be waiting for wl_buffer.release; only buffers that this host
    // surface actually held may be retired merely because its destructor
    // is ordered ahead of the buffer destructor.
    let destroyed_surface_buffer = ctx
        .surfaces
        .remove(&wl_surface_guest_id)
        .and_then(|surface| surface.current_buffer_id());
    ctx.forget_surface_buffer_copies(wl_surface_guest_id);
    crate::handler::shm::clear_buffer_uses_after_surface_destroy(ctx, destroyed_surface_buffer);
    // Pending-only objects do not receive a host release event. Retire
    // every buffer made eligible by removing this surface in one pass.
    crate::handler::shm::retire_eligible_buffers(ctx);
    ctx.viewport_to_wl_surface
        .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
    ctx.key_generations
        .clear_peek_watermarks_for_surface(wl_surface_guest_id);
    // Surface destruction can race the host's wl_keyboard.leave event.
    // Transition the authoritative registry first, then emit the mandatory
    // v3 leave while the guest surface ID is still valid. A later host leave
    // matches no live generation and is idempotent.
    let focus_update = ctx.keyboard_focus.destroy_surface(wl_surface_guest_id);
    crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);
    crate::handler::text_input::repair_destroyed_surface_focus(ctx, wl_surface_guest_id);
    for keyboard_id in focus_update.retired_keyboards {
        ctx.key_generations.clear_keyboard(keyboard_id);
    }
}

impl WlSurfaceHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let wl_surface_guest_id = ctx.last_sender_id;
        let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(wl_surface_guest_id) else {
            return ctx.fail(format_args!(
                "Unable to destroy wl_surface {wl_surface_guest_id} without its host generation"
            ));
        };
        // The generated dispatcher cannot express the ordering required by
        // the aura-shell integration: the Aura objects' releases precede the
        // wl_surface destructor, and what ending the surface sends follows
        // it (see `forget_surface`).
        release_surface_aura_objects(ctx, wl_surface_guest_id, wl_surface_host_id);
        ctx.client_to_host_queue
            .push(wl_surface::req::destroy(wl_surface_host_id));
        // Retain the numeric mapping until the host acknowledges the
        // destructor with wl_display.delete_id, so the guest can safely
        // reuse the ID only after that acknowledgement.
        ctx.guest_destroyed(wl_surface_guest_id);
        Action::Drop
    }

    /// An enter or leave naming a surface or output the guest has destroyed
    /// never gets here: the proxy suppresses events from a destroyed sender,
    /// and generated validation drops events naming a destroyed object.
    fn on_enter(&mut self, ctx: &mut Context, output: u32) -> Action {
        // An output whose global was removed is still a valid guest object,
        // so the event is forwarded; the registry ignores it for placement.
        ctx.outputs.surface_entered(ctx.last_sender_id, output);
        Action::Forward
    }

    fn on_leave(&mut self, ctx: &mut Context, output: u32) -> Action {
        ctx.outputs.surface_left(ctx.last_sender_id, output);
        Action::Forward
    }

    fn on_attach(&mut self, ctx: &mut Context, buffer: u32, x: i32, y: i32) -> Action {
        let surface_id = ctx.last_sender_id;
        // Objects of unknown version keep the permissive pre-offset rules
        // (see INVARIANTS); in practice only test fixtures have one.
        let version = ctx.shadow_table.guest_object_version_or_unknown(surface_id);
        let has_offset_request =
            !ShadowTable::is_unknown_version(version) && version >= REQ_OFFSET_SINCE;
        if has_offset_request && (x != 0 || y != 0) {
            // wl_surface v5 made non-zero attach offsets a protocol error;
            // clients must use wl_surface.offset instead. Do not let an
            // invalid request reach the host compositor.
            log::warn!(
                "Rejecting non-zero wl_surface.attach offset ({}, {}) for v5+ surface {}",
                x,
                y,
                surface_id
            );
            return ctx.reject(
                surface_id,
                wl_surface::ERROR_INVALID_OFFSET,
                "wl_surface.attach offset is non-zero for version 5+",
            );
        }
        let pending = &mut ctx.surface_mut(surface_id).pending;
        pending.attachment = if buffer == 0 {
            SurfaceAttachment::Detach
        } else {
            SurfaceAttachment::Attach(buffer)
        };
        pending.attach_offset = None;
        // Since wl_surface version 5, attach's x/y arguments are ignored
        // (and non-zero values were rejected above). Keep them out of the
        // legacy pending-offset slot so a legal attach(buffer, 0, 0) cannot
        // reset a previously committed wl_surface.offset.
        if buffer != 0 && !has_offset_request {
            pending.attach_offset = Some((x, y));
        }
        // A pending native buffer can be destroyed before the commit that
        // attached it. Replacing that pending attach (including attach(NULL))
        // means the compositor will never emit wl_buffer.release for the
        // superseded object, so retire its host proxy after the new attach
        // request in the ordered queue.
        crate::handler::shm::retire_eligible_buffers(ctx);
        Action::Forward
    }

    fn on_damage(&mut self, ctx: &mut Context, x: i32, y: i32, width: i32, height: i32) -> Action {
        ctx.surface_mut(ctx.last_sender_id)
            .pending
            .surface_damage
            .push(SurfaceRect::new(x, y, width, height));
        // The host compositor only understands surface-coordinate damage. Keep
        // this request local so the commit path can preserve ordering and
        // account for a concurrently pending viewport/scale change.
        Action::Drop
    }

    fn on_damage_buffer(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        ctx.surface_mut(ctx.last_sender_id)
            .pending
            .buffer_damage
            .push(BufferRect::new(x, y, width, height));
        // ChromiumOS Sommelier implements damage_buffer by translating it to
        // wl_surface.damage because the host compositor may only expose v3.
        Action::Drop
    }

    fn on_set_buffer_scale(&mut self, ctx: &mut Context, scale: i32) -> Action {
        if scale <= 0 {
            log::warn!("Rejecting invalid wl_surface buffer scale {}", scale);
            return ctx.reject(
                ctx.last_sender_id,
                wl_surface::ERROR_INVALID_SCALE,
                "invalid wl_surface buffer scale",
            );
        }
        ctx.surface_mut(ctx.last_sender_id).pending.buffer_scale = Some(scale);
        Action::Forward
    }

    fn on_set_buffer_transform(&mut self, ctx: &mut Context, transform: i32) -> Action {
        if !is_valid_transform(transform) {
            log::warn!(
                "Rejecting invalid wl_surface buffer transform {}",
                transform
            );
            return ctx.reject(
                ctx.last_sender_id,
                wl_surface::ERROR_INVALID_TRANSFORM,
                "invalid wl_surface buffer transform",
            );
        }
        ctx.surface_mut(ctx.last_sender_id).pending.buffer_transform = Some(transform);
        Action::Forward
    }

    fn on_offset(&mut self, ctx: &mut Context, x: i32, y: i32) -> Action {
        ctx.surface_mut(ctx.last_sender_id).pending.offset = Some((x, y));
        Action::Forward
    }

    fn on_commit(&mut self, ctx: &mut Context) -> Action {
        let surface_id = ctx.last_sender_id;

        // A native dma-buf's fence belongs to the contents an attach
        // submits, so an attach(NULL) or damage-only commit waits for
        // nothing, as in ChromiumOS Sommelier, whose sync point is set at
        // attach. The wait comes before the commit changes anything.
        let attached = ctx
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.pending.attachment.buffer_id());
        if let Some(buffer_id) = attached {
            // Wait for guest GPU writes before the host compositor samples
            // the buffer, matching ChromiumOS Sommelier's sync_point path,
            // but without stalling the event loop: the commit is
            // dispatched again once its fence may have signalled.
            match pending_native_fence(ctx, buffer_id) {
                Ok(None) => {}
                Ok(Some(wait)) => return ctx.wait_for_fence(wait),
                // wl_surface.commit has no recoverable failure response.
                // Disconnect rather than silently dropping a request the
                // guest reasonably believes was accepted.
                Err(error) => {
                    return ctx.fail(format_args!(
                        "Native dma-buf synchronization failed for surface {surface_id}: {error}"
                    ))
                }
            }
        }

        let mut default_surface = SurfaceState::default();
        let commit = ctx.surfaces.get_mut(&surface_id).map_or_else(
            || default_surface.prepare_commit(),
            SurfaceState::prepare_commit,
        );
        let attached_buffer_id = commit.attached_buffer_id();
        let copied_buffer_id = buffer_to_copy(ctx, &commit);

        let mapping = commit.state.mapping(shown_buffer_size(ctx, &commit.state));
        // wp_viewport.bad_size and out_of_buffer are deliberately not raised:
        // Exo accepts both, so ending the session would break apps that work
        // today. They are logged instead, once the commit is final.
        let viewport_problem = if commit.has_invalid_fractional_viewport() {
            Some("a fractional wp_viewport source without a destination")
        } else if !mapping.source_fits_buffer() {
            Some("a wp_viewport source outside its buffer")
        } else {
            None
        };
        let attached_full_mapping =
            attached_buffer_id.is_some() && (commit.moves_buffer() || !mapping.is_identity());
        let surface_damage = commit.surface_damage.rects();
        let buffer_damage = commit.buffer_damage.rects();
        ctx.stale_surface_pixels(surface_id, &commit, &mapping);
        // A buffer that copies everything shows the host a whole new image.
        let needs_full_damage = copied_buffer_id
            .and_then(|buffer_id| ctx.local_buffer(buffer_id))
            .is_some_and(|buffer| buffer.stale.to_copy_on(surface_id).is_full());
        let force_full_damage = needs_full_damage
            || commit.has_full_damage()
            || (commit.buffer_offset != (0, 0)
                && (!surface_damage.is_empty() || !buffer_damage.is_empty()))
            || (attached_full_mapping && surface_damage.is_empty() && buffer_damage.is_empty());
        let Some(mut host_messages) = build_surface_commit_messages(
            ctx,
            surface_id,
            surface_damage,
            buffer_damage,
            &mapping,
            force_full_damage,
        ) else {
            return ctx.fail(format_args!(
                "Cannot commit wl_surface {surface_id} without its host object"
            ));
        };

        // A SHM-backed buffer is copied into host storage before the host
        // commit is queued. Forwarding the commit first would let the host
        // compositor sample stale or uninitialised pixels if dma-buf sync,
        // mmap, or GBM mapping fails. A failed copy stales the whole host
        // copy, so a later commit retries the complete image.
        if let Some(buffer_id) = copied_buffer_id {
            if let Err(error) = copy_surface_buffer(ctx, surface_id, buffer_id) {
                match error {
                    // The client broke its pool, for example by truncating
                    // the file. Like libwayland's SIGBUS guard, report it on
                    // the buffer and end the session.
                    ShmCopyError::Source(_) => {
                        return ctx.reject(
                            buffer_id,
                            wl_shm::ERROR_INVALID_FD,
                            "error accessing SHM buffer",
                        )
                    }
                    ShmCopyError::Destination => log::warn!(
                        "Skipping host wl_surface.commit for {} because its SHM copy failed",
                        surface_id
                    ),
                }
                if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                    commit.rollback(surface);
                }
                crate::handler::shm::retire_eligible_buffers(ctx);
                return Action::Drop;
            }
        }

        if let Some((previous_buffer, next_buffer)) = commit.attachment_transition() {
            // The pending replacement has now passed every copy and fence
            // check. Apply its content snapshot and both buffer lifecycle
            // edges before appending any host message. The registry
            // validates both generations before changing either one, so a
            // failed replacement remains completely rollback-safe.
            let snapshot_updated = match next_buffer {
                Some(buffer_id) => {
                    let dimensions = ctx.buffer_dimensions(buffer_id);
                    ctx.surfaces.get_mut(&surface_id).is_some_and(|surface| {
                        surface.set_current_buffer_dimensions(buffer_id, dimensions)
                    })
                }
                None => ctx.surfaces.contains_key(&surface_id),
            };
            let use_updated =
                snapshot_updated && ctx.finalize_surface_attachment(previous_buffer, next_buffer);
            if !snapshot_updated || !use_updated {
                let why = format!(
                    "Render-buffer replacement could not finalize surface {}: \
                     previous guest={:?} host={:?} use={:?}, \
                     next guest={:?} host={:?} use={:?}, snapshot_updated={}",
                    surface_id,
                    previous_buffer,
                    previous_buffer.and_then(|id| ctx.render_buffer_host_id(id)),
                    previous_buffer.and_then(|id| ctx.host_buffer_use(id)),
                    next_buffer,
                    next_buffer.and_then(|id| ctx.render_buffer_host_id(id)),
                    next_buffer.and_then(|id| ctx.host_buffer_use(id)),
                    snapshot_updated,
                );
                return ctx.fail(why);
            }
        }

        if let Some(problem) = viewport_problem {
            log_unenforced_viewport_rule(ctx, surface_id, problem, &mapping);
        }
        // Damage and commit were built as one batch before local state was
        // finalized. Append them together, in order, only once every check
        // has passed, so the host never sees a partial transaction.
        ctx.client_to_host_queue.append(&mut host_messages);
        crate::handler::shm::retire_eligible_buffers(ctx);
        Action::Drop
    }
}

impl WlRegionHandler for CompositorHandler {}
impl WlSubcompositorHandler for CompositorHandler {}
impl WlSubsurfaceHandler for CompositorHandler {}

impl wp_viewporter::WpViewporterHandler for CompositorHandler {
    fn on_get_viewport(&mut self, ctx: &mut Context, id: u32, surface: u32) -> Action {
        if ctx
            .viewport_to_wl_surface
            .values()
            .any(|&owner| owner == surface)
        {
            log::warn!("Rejecting duplicate wp_viewport for wl_surface {}", surface);
            return ctx.reject(
                ctx.last_sender_id,
                wp_viewporter::ERROR_VIEWPORT_EXISTS,
                "wl_surface already has a wp_viewport",
            );
        }
        ctx.viewport_to_wl_surface.insert(id, surface);
        ctx.surface_mut(surface).pending.viewport = Some(Some(ViewportState::new()));
        Action::Forward
    }
}

impl wp_viewport::WpViewportHandler for CompositorHandler {
    fn on_set_source(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let Some(&surface_id) = ctx.viewport_to_wl_surface.get(&ctx.last_sender_id) else {
            return ctx.reject(
                ctx.last_sender_id,
                wp_viewport::ERROR_NO_SURFACE,
                "wp_viewport has no associated wl_surface",
            );
        };
        let unset = (x, y, width, height) == (-256, -256, -256, -256);
        if !unset && (x < 0 || y < 0 || width <= 0 || height <= 0) {
            log::warn!("Rejecting invalid wp_viewport source rectangle");
            return ctx.reject(
                ctx.last_sender_id,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport source",
            );
        }
        ctx.surface_mut(surface_id).pending_viewport_mut().source =
            (!unset).then_some((x, y, width, height));
        Action::Forward
    }

    fn on_set_destination(&mut self, ctx: &mut Context, width: i32, height: i32) -> Action {
        let Some(&surface_id) = ctx.viewport_to_wl_surface.get(&ctx.last_sender_id) else {
            return ctx.reject(
                ctx.last_sender_id,
                wp_viewport::ERROR_NO_SURFACE,
                "wp_viewport has no associated wl_surface",
            );
        };
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            log::warn!(
                "Rejecting invalid wp_viewport destination {}x{}",
                width,
                height
            );
            return ctx.reject(
                ctx.last_sender_id,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport destination",
            );
        }
        ctx.surface_mut(surface_id)
            .pending_viewport_mut()
            .destination = (!unset).then_some((width, height));
        Action::Forward
    }
}

/// Forget guest `wp_viewport` `viewport_id`, which ended (see
/// `crate::state::lifecycle`): its surface's next commit removes the
/// viewport.
pub(crate) fn forget_viewport(ctx: &mut Context, viewport_id: u32) {
    if let Some(surface_id) = ctx.viewport_to_wl_surface.remove(&viewport_id) {
        if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
            surface.pending.viewport = Some(None);
        }
    }
}

// --- XDG Shell → wl_surface tracking for zaura_shell integration ---
//
// ChromeOS needs a zaura_surface (from zaura_shell) to set the application ID
// that the shelf uses for icon matching. But the app ID arrives via
// xdg_toplevel::set_app_id, which doesn't carry a wl_surface reference.
//
// `crate::state::XdgRoles` bridges this gap: these handlers record the
// chain get_xdg_surface(xdg_surface, wl_surface) → get_toplevel(xdg_toplevel)
// and end it in the destructors, so set_app_id can resolve the underlying
// wl_surface and create or reuse its host zaura_surface.

impl crate::protocols::xdg_shell::xdg_wm_base::XdgWmBaseHandler for CompositorHandler {
    fn on_get_xdg_surface(&mut self, ctx: &mut Context, id: u32, surface: u32) -> Action {
        ctx.xdg_roles.add_xdg_surface(id, surface);
        Action::Forward
    }
}

/// Forget guest `xdg_surface` `xdg_surface_id`, which ended (see
/// `crate::state::lifecycle`), with the role of its toplevel.
///
/// Destroying an xdg_surface before its xdg_toplevel is a protocol error
/// (`xdg_surface.defunct_role_object`) that ends the session, so the order
/// of the forwarded destructor and the queued Aura release does not matter.
/// The role ends anyway, so the toplevel's later requests cannot reach an
/// unrelated surface after ID reuse.
pub(crate) fn forget_xdg_surface(ctx: &mut Context, xdg_surface_id: u32) {
    if let Some(xdg_toplevel_id) = ctx.xdg_roles.remove_xdg_surface(xdg_surface_id) {
        window_placement::release_toplevel(ctx, xdg_toplevel_id);
    }
}

/// Forget guest `xdg_toplevel` `xdg_toplevel_id`, which ended (see
/// `crate::state::lifecycle`), and release its Aura toplevel.
pub(crate) fn forget_xdg_toplevel(ctx: &mut Context, xdg_toplevel_id: u32) {
    window_placement::release_toplevel(ctx, xdg_toplevel_id);
    ctx.xdg_roles.remove_toplevel(xdg_toplevel_id);
}

impl crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler for CompositorHandler {
    fn on_configure(&mut self, ctx: &mut Context, serial: u32) -> Action {
        let host_xdg_surface_id = ctx.last_sender_id;
        let Some(guest_xdg_surface_id) = ctx.shadow_table.get_guest_id(host_xdg_surface_id) else {
            return Action::Drop;
        };
        if window_placement::hold_surface_configure(ctx, guest_xdg_surface_id, serial) {
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_get_toplevel(&mut self, ctx: &mut Context, id: u32) -> Action {
        ctx.defer_until_forwarded(PostDispatch::XdgToplevelCreated {
            xdg_surface: ctx.last_sender_id,
            xdg_toplevel: id,
        });
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_toplevel_id = ctx.last_sender_id;
        let Some(host_xdg_toplevel_id) = ctx.shadow_table.get_host_id(xdg_toplevel_id) else {
            return ctx.fail(format_args!(
                "Unable to destroy xdg_toplevel {xdg_toplevel_id} without its host generation"
            ));
        };
        // Ending the toplevel releases its Aura toplevel (see
        // `forget_xdg_toplevel`), which goes before the role destructor (see
        // `aura_objects::release`). The guest ID is retained until the host
        // acknowledges the role destructor.
        ctx.guest_destroyed(xdg_toplevel_id);
        ctx.client_to_host_queue
            .push(xdg_toplevel::req::destroy(host_xdg_toplevel_id));
        Action::Drop
    }

    fn on_set_app_id(&mut self, ctx: &mut Context, app_id: &String) -> Action {
        let xdg_toplevel_id = ctx.last_sender_id;

        let Some(xdg_toplevel_host_id) = ctx.shadow_table.get_host_id(xdg_toplevel_id) else {
            log::warn!(
                "Cannot namespace app ID for unmapped xdg_toplevel {}",
                xdg_toplevel_id
            );
            return Action::Drop;
        };
        // Keep the host XDG role in the Guest OS namespace.  ARC is only a
        // permission hint on the Aura surface; applying it to the role makes
        // ChromeOS classify every guest window as the same generic ARC app and
        // loses the client's shelf/icon identity.
        let native_app_id = native_wayland_app_id(&ctx.vm_identifier, app_id);

        // The host xdg_toplevel carries the app ID used by ordinary Exo
        // shelf/application matching. Keep that request namespaced as in the
        // reference Sommelier path rather than forwarding the guest's
        // unqualified ID directly. An ID too long for one message reaches
        // neither the role nor the Aura surface.
        if !queue_or_drop(
            &mut ctx.client_to_host_queue,
            xdg_toplevel::req::set_app_id(xdg_toplevel_host_id, &native_app_id),
            format_args!("the application ID of xdg_toplevel {xdg_toplevel_id}"),
        ) {
            return Action::Drop;
        }

        // Resolve xdg_toplevel → wl_surface (guest) → wl_surface (host).
        if let Some(wl_surface_guest_id) = ctx.xdg_roles.wl_surface_of_toplevel(xdg_toplevel_id) {
            window_placement::set_aura_application_id(ctx, wl_surface_guest_id, &native_app_id);
        }
        Action::Drop
    }

    fn on_configure(
        &mut self,
        ctx: &mut Context,
        width: i32,
        height: i32,
        states: &[u8],
    ) -> Action {
        let host_xdg_toplevel_id = ctx.last_sender_id;
        let Some(guest_xdg_toplevel_id) = ctx.shadow_table.get_guest_id(host_xdg_toplevel_id)
        else {
            return Action::Drop;
        };
        if window_placement::hold_toplevel_configure(
            ctx,
            guest_xdg_toplevel_id,
            width,
            height,
            states,
        ) {
            return Action::Drop;
        }
        Action::Forward
    }
}

impl crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler for CompositorHandler {
    fn on_configure(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        states: &[u8],
    ) -> Action {
        // The placement module delivers the translated event itself.
        window_placement::translate_aura_configure(
            ctx,
            ctx.last_sender_id,
            x,
            y,
            width,
            height,
            states,
        );
        Action::Drop
    }

    fn on_origin_change(&mut self, ctx: &mut Context, x: i32, y: i32) -> Action {
        log::debug!(
            "zaura_toplevel.origin_change host={} origin=({}, {})",
            ctx.last_sender_id,
            x,
            y
        );
        // xdg_toplevel has no origin-change event and the proxy keeps no
        // window origin, so the event is always dropped, whether or not a
        // placement barrier is active. A subsequent Aura configure carries
        // the authoritative size and state.
        Action::Drop
    }
}

#[cfg(test)]
// Tests build and close raw descriptors to check who owns them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE;
    use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE;
    use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
    use crate::protocols::viewporter::wp_viewport::WpViewportHandler;
    use crate::protocols::viewporter::wp_viewporter::WpViewporterHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::wl_surface::{REQ_COMMIT, REQ_DAMAGE, REQ_DESTROY};
    use crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::REQ_SET_APP_ID;
    use crate::state::ARC_APPLICATION_ID_PREFIX;
    use crate::state::{BufferState, Context, RenderBufferLifecycle, RenderBufferUse, StalePixels};
    use crate::test_support::{
        msg_opcode, msg_sender, protocol_error, protocol_error_code, queued_headers, shm_pool_file,
    };
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::FileExt;

    fn setup_ctx() -> (Context, u32, u32, u32) {
        setup_ctx_with(crate::placement_config::PlacementConfig::for_test(
            Vec::new(),
        ))
    }

    /// [`setup_ctx`] with an explicit placement configuration.
    fn setup_ctx_with(
        placement: crate::placement_config::PlacementConfig,
    ) -> (Context, u32, u32, u32) {
        let mut ctx = Context::with_placement_for_test(placement);
        let wl_surface_guest = 100u32;
        let wl_surface_host = 200u32;
        let zaura_shell_host = 300u32;
        let xdg_toplevel_id = 400u32;
        let xdg_surface_id = 500u32;

        ctx.shadow_table
            .insert_paired_for_test(wl_surface_guest, wl_surface_host, "wl_surface");
        ctx.shadow_table.insert_paired_for_test(
            xdg_surface_id,
            xdg_surface_id + 100,
            "xdg_surface",
        );
        ctx.shadow_table.insert_paired_for_test(
            xdg_toplevel_id,
            xdg_toplevel_id + 100,
            "xdg_toplevel",
        );
        ctx.bind_aura_shell_for_test(zaura_shell_host);
        ctx.xdg_roles
            .add_toplevel_role_for_test(xdg_surface_id, wl_surface_guest, xdg_toplevel_id);

        (ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host)
    }

    /// A request naming an inert output (whose bind crossed its global's
    /// removal) reaches the host with no output, as if the guest named none.
    #[test]
    fn set_fullscreen_on_an_inert_output_is_forwarded_without_an_output() {
        use crate::protocols::xdg_shell::xdg_toplevel::{dispatch_request, REQ_SET_FULLSCREEN};
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let (toplevel_guest, toplevel_host, inert_output) = (400, 500, 10);
        ctx.shadow_table.insert_paired_with_version_for_test(
            toplevel_guest,
            toplevel_host,
            "xdg_toplevel",
            3,
        );
        ctx.shadow_table.create_local(inert_output, "wl_output", 3);

        ctx.last_sender_id = toplevel_guest;
        let payload = inert_output.to_ne_bytes();
        let mut request =
            crate::wire::WireMessage::new(toplevel_guest, REQ_SET_FULLSCREEN, &payload);
        let (message, _) = dispatch_request(&mut request, &mut CompositorHandler, &mut ctx)
            .expect("valid request")
            .expect("forwarded");
        assert_eq!(msg_sender(&message), toplevel_host);
        assert_eq!(msg_opcode(&message), REQ_SET_FULLSCREEN);
        assert_eq!(message[8..], 0u32.to_ne_bytes());
    }

    #[test]
    fn surface_enter_leave_tracks_the_last_entered_output() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.outputs.bind_for_test(20, Default::default());
        ctx.outputs.bind_for_test(30, Default::default());
        ctx.last_sender_id = 100;
        let event = |ctx: &mut Context, enter: bool, output: u32| {
            let action = if enter {
                WlSurfaceHandler::on_enter(&mut CompositorHandler, ctx, output)
            } else {
                WlSurfaceHandler::on_leave(&mut CompositorHandler, ctx, output)
            };
            assert_eq!(action, Action::Forward);
            ctx.outputs.entered_for_test(100)
        };
        assert_eq!(event(&mut ctx, true, 20), Some(20));
        // Exo moves a surface with leave(old) before enter(new).
        assert_eq!(event(&mut ctx, false, 20), None);
        assert_eq!(event(&mut ctx, true, 30), Some(30));
        // A leave of another output, in either order, changes nothing.
        assert_eq!(event(&mut ctx, true, 20), Some(20));
        assert_eq!(event(&mut ctx, false, 30), Some(20));
    }

    /// The enter and leave handlers rely on generated validation to drop an
    /// event that names a released output, which stays mapped until the
    /// host's `delete_id`.
    #[test]
    fn surface_events_naming_a_released_output_never_reach_the_handlers() {
        use crate::protocols::wayland::wl_surface::{dispatch_event, EVT_ENTER, EVT_LEAVE};
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let (surface_guest, surface_host, output_guest, output_host) = (100, 200, 10, 20);
        ctx.shadow_table.insert_paired_with_version_for_test(
            surface_guest,
            surface_host,
            "wl_surface",
            4,
        );
        ctx.shadow_table.insert_paired_with_version_for_test(
            output_guest,
            output_host,
            "wl_output",
            3,
        );
        ctx.outputs.bind_for_test(output_host, Default::default());
        ctx.guest_destroyed(output_guest);

        let payload = output_host.to_ne_bytes();
        for opcode in [EVT_ENTER, EVT_LEAVE] {
            let mut event = crate::wire::WireMessage::new(surface_host, opcode, &payload);
            let dispatched = dispatch_event(&mut event, &mut CompositorHandler, &mut ctx);
            assert!(matches!(dispatched, Ok(None)), "opcode {opcode}");
            assert_eq!(ctx.outputs.entered_for_test(surface_host), None);
        }
    }

    #[test]
    fn surface_enter_of_a_removed_output_is_forwarded_but_not_recorded() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.outputs.bind_for_test(20, Default::default());
        let removed_aura_outputs = ctx.outputs.remove_global(crate::state::GlobalBinding {
            name: 20,
            generation: 1,
        });
        assert!(removed_aura_outputs.is_empty());
        ctx.last_sender_id = 100;
        assert_eq!(
            WlSurfaceHandler::on_enter(&mut CompositorHandler, &mut ctx, 20),
            Action::Forward
        );
        assert_eq!(ctx.outputs.entered_for_test(100), None);
    }

    fn active_text_input_state(
        host_v1_id: u32,
        guest_seat: u32,
        active_surface: u32,
    ) -> crate::state::TextInputState {
        let mut state =
            crate::state::TextInputState::new(host_v1_id, None, guest_seat, Some(active_surface));
        state.pending = crate::state::V3Pending {
            enabled: Some(true),
            surrounding_text: Some(Some(("한".to_string(), 3, 3))),
            content_type: Some((1, 1)),
            cursor_rect: Some((1, 2, 3, 4)),
        };
        state.committed.enabled = true;
        state.committed.surrounding_text = Some(("한".to_string(), 3, 3));
        state.committed.content_type = (1, 1);
        state.current_preedit = "한".to_string();
        state.set_guest_commits(1);
        state.pending_preedit_cursor = Some(1);
        state.pending_preedit_selection = Some((0, 1));
        state.pending_deletes = vec![(1, 1)];
        state.pending_cursor_position = Some((1, 1));
        state.host_activation = crate::state::HostActivationState::Active;
        state
    }

    fn mapped_test_buffer(
        _guest_buffer_id: u32,
        width: i32,
        height: i32,
        stride: u32,
        stale: StalePixels,
    ) -> BufferState {
        let source_size = usize::try_from(height)
            .unwrap()
            .checked_mul(stride as usize)
            .unwrap();
        let destination_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                source_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(destination_ptr, libc::MAP_FAILED);
        BufferState {
            pool_file: Arc::new(shm_pool_file(&vec![0; source_size])),
            offset: 0,
            width,
            height,
            stride,
            format: 0,
            bo: None,
            dmabuf_fd: None,
            bo_stride: stride,
            dmabuf_plane1_offset: 0,
            dmabuf_plane1_stride: 0,
            dmabuf_sync: false,
            dest_ptr: destination_ptr as *mut u8,
            dest_size: source_size,
            stale,
        }
    }

    /// The pool file of local-copy buffer `guest_id`, which a test writes
    /// as the client would draw.
    fn test_pool_file(ctx: &Context, guest_id: u32) -> Arc<std::fs::File> {
        Arc::clone(&ctx.local_buffer(guest_id).unwrap().pool_file)
    }

    /// The host copy of local-copy buffer `guest_id`.
    fn test_destination(ctx: &Context, guest_id: u32) -> Vec<u8> {
        let buffer = ctx.local_buffer(guest_id).unwrap();
        unsafe { std::slice::from_raw_parts(buffer.dest_ptr, buffer.dest_size) }.to_vec()
    }

    fn register_test_local(ctx: &mut Context, guest_id: u32, backing: BufferState) {
        let host_id = ctx.shadow_table.get_host_id(guest_id).unwrap_or_else(|| {
            let host_id = guest_id + 1;
            ctx.shadow_table
                .insert_paired_with_version_for_test(guest_id, host_id, "wl_buffer", 1);
            host_id
        });
        assert!(ctx.register_local_buffer(guest_id, host_id, backing));
    }

    fn register_test_native(ctx: &mut Context, guest_id: u32, size: (i32, i32)) {
        register_test_native_with_sync_fds(ctx, guest_id, size, Vec::new());
    }

    fn register_test_native_with_sync_fds(
        ctx: &mut Context,
        guest_id: u32,
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    ) {
        let host_id = ctx.shadow_table.get_host_id(guest_id).unwrap_or_else(|| {
            let host_id = guest_id + 1;
            ctx.shadow_table
                .insert_paired_with_version_for_test(guest_id, host_id, "wl_buffer", 1);
            host_id
        });
        assert!(ctx.register_native_buffer(host_id, size, sync_fds));
    }

    fn unsupported_dmabuf_sync_fd() -> OwnedFd {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::close(pipe_fds[1]) }, 0);
        unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) }
    }

    fn invalid_ioctl_fd() -> OwnedFd {
        let path = std::ffi::CString::new("/proc/self/exe").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn set_test_surface_content(
        ctx: &mut Context,
        surface_id: u32,
        buffer_id: Option<u32>,
        dimensions: Option<(i32, i32)>,
    ) {
        ctx.surface_mut(surface_id)
            .set_current_buffer_for_test(buffer_id, dimensions);
    }

    #[test]
    fn unsupported_native_sync_uses_cached_implicit_fallback_and_commits() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let surface = 100;
        let buffer = 50;
        register_test_native_with_sync_fds(
            &mut ctx,
            buffer,
            (1, 1),
            vec![unsupported_dmabuf_sync_fd()],
        );

        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface;
        assert_eq!(handler.on_attach(&mut ctx, buffer, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(!ctx.is_fatal());
        assert!(ctx.native_buffer_uses_implicit_sync(buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| (msg_sender(message), msg_opcode(message))),
            Some((wl_surface_host, REQ_COMMIT))
        );

        ctx.client_to_host_queue.clear();
        assert_eq!(handler.on_attach(&mut ctx, buffer, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(!ctx.is_fatal());
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| (msg_sender(message), msg_opcode(message))),
            Some((wl_surface_host, REQ_COMMIT)),
            "the cached compatibility mode must keep later commits live"
        );
    }

    #[test]
    fn unexpected_native_sync_failure_is_fatal_and_never_commits() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface = 100;
        let buffer = 50;
        let sync_fd = invalid_ioctl_fd();
        let error = crate::allocator::dmabuf_fence(sync_fd.as_fd(), None).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        register_test_native_with_sync_fds(&mut ctx, buffer, (1, 1), vec![sync_fd]);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface;
        assert_eq!(handler.on_attach(&mut ctx, buffer, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.is_fatal());
        assert!(!ctx.native_buffer_uses_implicit_sync(buffer));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| msg_opcode(message) != REQ_COMMIT),
            "a commit whose synchronization failed must not reach the host"
        );
    }

    /// A commit of a buffer the guest GPU still writes waits before it
    /// changes anything, and goes through once the fence signals.
    #[test]
    fn a_commit_waits_for_a_pending_fence_before_it_changes_anything() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.plane_fences_for_test = Some(crate::state::PlaneFenceForTest::SyncFile);
        let buffer = 50;
        let (fence, signal) = nix::unistd::pipe().unwrap();
        register_test_native_with_sync_fds(&mut ctx, buffer, (1, 1), vec![fence]);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_attach(&mut ctx, buffer, 0, 0), Action::Forward);
        let before = ctx.surfaces[&TEST_SURFACE].clone();

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        let effects = ctx.take_effects();
        assert!(effects.end.is_none());
        assert!(effects.to_host.is_empty() && effects.to_guest.is_empty());
        let wait = effects.wait.expect("the commit waits for its fence");
        assert_eq!(wait.timeout, crate::allocator::SYNC_FILE_TIMEOUT);
        assert_eq!(
            ctx.surfaces[&TEST_SURFACE], before,
            "a commit that waits changes nothing, so it can be dispatched again"
        );

        assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        let effects = ctx.take_effects();
        assert!(effects.end.is_none() && effects.wait.is_none());
        assert_eq!(
            queued_headers(&effects.to_host).last(),
            Some(&(wl_surface_host, REQ_COMMIT))
        );
    }

    /// The wl_surface that `setup_ctx` maps to a host surface.
    const TEST_SURFACE: u32 = 100;
    /// The side of the square ARGB8888 frames of the copy tests.
    const FRAME_SIDE: usize = 4;
    const FRAME_STRIDE: usize = FRAME_SIDE * 4;

    /// Register a 4x4 ARGB8888 local-copy buffer and commit it once on
    /// `surface`, so that its host copy holds a frame and later commits copy
    /// only what they must.
    fn commit_test_frame(ctx: &mut Context, surface: u32, buffer: u32) {
        let side = FRAME_SIDE as i32;
        register_test_local(
            ctx,
            buffer,
            mapped_test_buffer(buffer, side, side, FRAME_STRIDE as u32, StalePixels::all()),
        );
        attach_and_commit(ctx, surface, buffer);
    }

    fn attach_and_commit(ctx: &mut Context, surface: u32, buffer: u32) {
        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface;
        assert_eq!(handler.on_attach(ctx, buffer, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(ctx), Action::Drop);
        assert!(!ctx.is_fatal());
    }

    /// Commit buffer scale `scale` on its own, as a HiDPI client does once,
    /// so that later frames keep the mapping.
    fn commit_test_buffer_scale(ctx: &mut Context, scale: i32) {
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_set_buffer_scale(ctx, scale), Action::Forward);
        assert_eq!(handler.on_commit(ctx), Action::Drop);
        assert!(!ctx.is_fatal());
    }

    /// Draw the client's next frame into `buffer`: every pool byte becomes
    /// `pixel`. The host copy is cleared, so `copied_pixels` then shows
    /// exactly what the next commit copies.
    fn draw_test_frame(ctx: &Context, buffer: u32, pixel: u8) {
        let local = ctx.local_buffer(buffer).unwrap();
        let bytes = vec![pixel; local.dest_size];
        local.pool_file.write_all_at(&bytes, 0).unwrap();
        unsafe { std::ptr::write_bytes(local.dest_ptr, 0, local.dest_size) };
    }

    /// The (x, y) pixels of `buffer`'s host copy that hold `pixel`.
    fn copied_pixels(ctx: &Context, buffer: u32, pixel: u8) -> Vec<(usize, usize)> {
        let host_copy = test_destination(ctx, buffer);
        (0..FRAME_SIDE)
            .flat_map(|y| (0..FRAME_SIDE).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let start = y * FRAME_STRIDE + x * 4;
                host_copy[start..start + 4]
                    .iter()
                    .all(|&byte| byte == pixel)
            })
            .collect()
    }

    /// The (x, y) pixels of a rectangle, in `copied_pixels` order.
    fn rect_pixels(x: usize, y: usize, width: usize, height: usize) -> Vec<(usize, usize)> {
        (y..y + height)
            .flat_map(|y| (x..x + width).map(move |x| (x, y)))
            .collect()
    }

    #[test]
    fn buffer_damage_under_a_buffer_scale_copies_only_its_pixels() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        commit_test_buffer_scale(&mut ctx, 2);
        draw_test_frame(&ctx, 42, 0x22);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 1, 1, 1, 2), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), rect_pixels(1, 1, 1, 2));
    }

    #[test]
    fn surface_damage_under_an_integer_buffer_scale_copies_its_scaled_pixels() {
        // A HiDPI client: buffer scale 2, transform 0, no viewport. Its
        // surface damage maps exactly to buffer pixels, so a small update
        // must not copy the whole buffer every frame.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        commit_test_buffer_scale(&mut ctx, 2);
        draw_test_frame(&ctx, 42, 0x22);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage(&mut ctx, 1, 0, 1, 1), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), rect_pixels(2, 0, 2, 2));
    }

    #[test]
    fn surface_damage_under_a_buffer_transform_copies_the_whole_buffer() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        draw_test_frame(&ctx, 42, 0x22);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        // WL_OUTPUT_TRANSFORM_90: the bridge does not rotate surface damage.
        assert_eq!(
            handler.on_set_buffer_transform(&mut ctx, 1),
            Action::Forward
        );
        assert_eq!(handler.on_damage(&mut ctx, 1, 1, 1, 1), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x22),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    #[test]
    fn destination_only_viewport_copies_only_the_damaged_pixels() {
        // A fractional-scale client: the 4x4 buffer fills an 8x8 surface,
        // so a surface unit is half a buffer pixel. It has no damage_buffer
        // (the host offers wl_compositor v3) and damages surface units.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, TEST_SURFACE),
            Action::Forward
        );
        ctx.last_sender_id = 700;
        assert_eq!(handler.on_set_destination(&mut ctx, 8, 8), Action::Forward);
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        draw_test_frame(&ctx, 42, 0x22);
        assert_eq!(handler.on_damage(&mut ctx, 0, 0, 2, 2), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        // Surface units 0..2 are pixels 0..1, grown by a pixel.
        assert_eq!(copied_pixels(&ctx, 42, 0x22), rect_pixels(0, 0, 2, 2));
    }

    #[test]
    fn commit_without_damage_copies_nothing() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        draw_test_frame(&ctx, 42, 0x22);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), []);
    }

    #[test]
    fn damage_beyond_the_buffer_is_clipped_to_it() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        draw_test_frame(&ctx, 42, 0x22);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, 0, 0, i32::MAX, i32::MAX),
            Action::Drop
        );
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x22),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    fn buffer_lifecycle(ctx: &Context, guest_id: u32) -> Option<RenderBufferLifecycle> {
        let host_id = ctx.render_buffer_host_id(guest_id)?;
        ctx.render_buffer_lifecycle_for_host(host_id)
    }

    fn buffer_is_guest_destroyed(ctx: &Context, guest_id: u32) -> bool {
        buffer_lifecycle(ctx, guest_id).is_some_and(|lifecycle| lifecycle.is_guest_destroyed())
    }

    fn buffer_host_destroy_is_queued(ctx: &Context, guest_id: u32) -> bool {
        buffer_lifecycle(ctx, guest_id) == Some(RenderBufferLifecycle::HostDestroyQueued)
    }

    fn mark_test_buffer_guest_destroyed(ctx: &mut Context, guest_id: u32) {
        assert!(ctx.mark_buffer_guest_destroyed(guest_id));
        ctx.guest_destroyed_in_use(guest_id);
    }

    /// Only a paired or local object can send a request or be named in one:
    /// an ID that names neither is rejected before any handler runs.
    #[test]
    fn an_id_that_names_no_object_is_rejected() {
        use crate::protocols::wayland::wl_surface;
        use crate::wire::ProtocolError;
        let dispatch = |ctx: &mut Context, (request, _): crate::wire::Outgoing| {
            let (header, payload) = crate::test_support::frames(&request)
                .next()
                .expect("a whole request");
            let mut request =
                crate::wire::WireMessage::new(header.sender_id, header.opcode, payload);
            crate::test_support::raw_dispatch(wl_surface::dispatch_request(
                &mut request,
                &mut CompositorHandler,
                ctx,
            ))
        };
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_for_test(22, 32, "wl_surface");

        assert_eq!(
            dispatch(&mut ctx, wl_surface::req::commit(20)),
            Err(ProtocolError::InvalidSender(20))
        );
        assert_eq!(
            dispatch(&mut ctx, wl_surface::req::attach(22, Some(21), 0, 0)),
            Err(ProtocolError::InvalidObjectId(21))
        );
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn set_app_id_creates_zaura_surface_and_sets_app_id() {
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"my_app".to_string());
        assert_eq!(action, Action::Drop);

        assert_eq!(ctx.client_to_host_queue.len(), 3);

        let msg0 = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(msg0), xdg_toplevel_id + 100);
        assert_eq!(msg_opcode(msg0), REQ_SET_APP_ID);

        let msg1 = &ctx.client_to_host_queue[1].0;
        assert_eq!(msg_sender(msg1), zaura_shell_host);
        assert_eq!(msg_opcode(msg1), REQ_GET_AURA_SURFACE);

        let msg2 = &ctx.client_to_host_queue[2].0;
        assert_eq!(msg_opcode(msg2), REQ_SET_APPLICATION_ID);
        let payload = &msg2[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let app_id_str = String::from_utf8(payload[4..4 + str_len - 1].to_vec()).unwrap();
        assert!(app_id_str.starts_with("org.chromium.guest_os."));
        assert!(app_id_str.ends_with(".wayland.my_app"));
    }

    #[test]
    fn set_app_id_reuses_existing_zaura_surface() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.last_sender_id = xdg_toplevel_id;

        // An Aura surface created earlier, tracked the way
        // ensure_host_zaura_surface records it.
        let zaura_surface_host = 99u32;
        ctx.shadow_table
            .insert_host_only_for_test(zaura_surface_host, "zaura_surface", 38);
        ctx.aura_surfaces
            .remember(wl_surface_host, zaura_surface_host);

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"reused".to_string());
        assert_eq!(action, Action::Drop);

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
        assert_eq!(
            msg_sender(&ctx.client_to_host_queue[0].0),
            xdg_toplevel_id + 100
        );
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            REQ_SET_APPLICATION_ID
        );
        assert_eq!(
            msg_sender(&ctx.client_to_host_queue[1].0),
            zaura_surface_host
        );
    }

    #[test]
    fn set_app_id_uses_arc_namespace_when_bounds_policy_is_enabled() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) =
            setup_ctx_with(crate::placement_config::PlacementConfig::arc_bounds_for_test(""));
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_app_id(&mut ctx, &"com.example.Terminal".to_string()),
            Action::Drop
        );
        let xdg_message = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| {
                msg_sender(message) == xdg_toplevel_id + 100
                    && msg_opcode(message) == REQ_SET_APP_ID
            })
            .expect("native XDG application ID request");
        let payload = &xdg_message.0[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        assert_eq!(
            std::str::from_utf8(&payload[4..4 + str_len - 1]).unwrap(),
            native_wayland_app_id(&ctx.vm_identifier, "com.example.Terminal",)
        );

        let zaura_surface_host = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_GET_AURA_SURFACE)
            .map(|(message, _)| u32::from_ne_bytes(message[8..12].try_into().unwrap()))
            .expect("Aura surface request must allocate a host ID");
        let aura_message = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| {
                msg_sender(message) == zaura_surface_host
                    && msg_opcode(message) == REQ_SET_APPLICATION_ID
            })
            .expect("ARC Aura application ID request");
        let payload = &aura_message.0[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let aura_app_id = std::str::from_utf8(&payload[4..4 + str_len - 1]).unwrap();
        assert!(aura_app_id.starts_with(ARC_APPLICATION_ID_PREFIX));
        assert!(!aura_app_id.starts_with("org.chromium.arc.session."));
        assert!(aura_app_id
            .strip_prefix(ARC_APPLICATION_ID_PREFIX)
            .unwrap()
            .parse::<u32>()
            .is_ok());
        assert_eq!(
            Some(aura_app_id),
            ctx.window_placement.arc_application_id(100).as_deref()
        );
    }

    #[test]
    fn set_app_id_noop_when_no_zaura_shell() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell = None;
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"no_shell".to_string());
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
    }

    #[test]
    fn oversized_namespaced_app_id_is_dropped_before_wire_encoding() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.last_sender_id = xdg_toplevel_id;
        let oversized = "x".repeat(4_084);

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_set_app_id(&mut ctx, &oversized), Action::Drop);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "an app ID exceeding the Wayland message limit must not be queued"
        );
    }

    #[test]
    fn namespaced_app_id_wire_size_boundary_is_checked() {
        // Exercise the exact 4096-byte message boundary independently of the
        // namespace prefix used by the production path.
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_083)).is_ok());
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_084)).is_err());
    }

    #[test]
    fn commit_without_attach_keeps_the_current_buffer() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        register_test_native(&mut ctx, 42, (1, 1));

        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(SurfaceState::current_buffer_id),
            Some(42)
        );

        // A commit without an attach keeps the surface contents, but it does
        // not submit or copy the wl_buffer object again.
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(SurfaceState::current_buffer_id),
            Some(42)
        );

        // An explicit attach(NULL) is different from omitting attach and must
        // clear the current buffer.
        assert_eq!(handler.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(SurfaceState::current_buffer_id),
            None
        );
    }

    #[test]
    fn damage_only_commit_preserves_released_local_use() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        assert!(ctx.mark_buffer_submitted(buffer_id));
        assert!(ctx.mark_buffer_released(buffer_id));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.host_buffer_use(buffer_id),
            Some(RenderBufferUse::Released),
            "only committing an explicit attach may begin a compositor use interval"
        );
    }

    #[test]
    fn damage_only_commit_does_not_resubmit_released_current_buffer() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        assert!(ctx.mark_buffer_submitted(buffer_id));
        assert!(ctx.mark_buffer_released(buffer_id));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            !ctx.buffer_is_submitted(buffer_id),
            "damage-only commit must not mark the wl_buffer as submitted again"
        );
    }

    #[test]
    fn damage_only_commit_preserves_released_native_use() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        register_test_native(&mut ctx, buffer_id, (1, 1));
        assert!(ctx.mark_buffer_submitted(buffer_id));
        assert!(ctx.mark_buffer_released(buffer_id));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            ctx.buffer_is_released(buffer_id),
            "damage-only commit must not clear the prior native-buffer release"
        );
        assert!(!ctx.buffer_is_submitted(buffer_id));
    }

    #[test]
    fn native_buffer_damage_uses_recorded_dimensions() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_native(&mut ctx, buffer_id, (100, 50));
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, 10, 10, 20, 20),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let damage = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("native damage must be forwarded before commit");
        let width = i32::from_ne_bytes(damage.0[16..20].try_into().unwrap());
        let height = i32::from_ne_bytes(damage.0[20..24].try_into().unwrap());
        assert!(
            width > 1 && height > 1,
            "native damage must not be clipped to the fallback 1x1 dimensions"
        );
    }

    #[test]
    fn destroyed_buffer_damage_uses_committed_content_dimensions() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let mut surface = SurfaceState::default();
        surface.current_buffer_transform = 1;
        ctx.surfaces.insert(surface_id, surface);
        set_test_surface_content(&mut ctx, surface_id, None, Some((100, 50)));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, 10, 10, 20, 20),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let damage = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("committed content metadata must survive wl_buffer destruction");
        let width = i32::from_ne_bytes(damage.0[16..20].try_into().unwrap());
        let height = i32::from_ne_bytes(damage.0[20..24].try_into().unwrap());
        assert!(
            width > 1 && height > 1,
            "damage must use the 100x50 content snapshot instead of a 1x1 fallback"
        );
    }

    #[test]
    fn native_release_damage_commit_destroy_does_not_wait_for_another_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, buffer_id, (1, 1));
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.mark_buffer_submitted(buffer_id);

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Forward
        );
        assert!(ctx.buffer_is_released(buffer_id));

        // A commit without attach changes surface state but does not begin
        // another wl_buffer access interval.
        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.buffer_is_released(buffer_id));
        assert!(!ctx.buffer_is_submitted(buffer_id));

        // The prior release is still authoritative, so guest destroy queues
        // the host destructor immediately instead of waiting forever for an
        // event that cannot arrive.
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(
            ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer_id),
            "host buffer destroy must use the completed release interval"
        );
    }

    #[test]
    fn release_owed_after_surface_destroy_is_expected() {
        // A GTK client's exit: the surface showing a submitted buffer is
        // destroyed first, and Exo then releases the buffer it no longer
        // shows while the guest still owns it.
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, buffer_id, (1, 1));
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        assert!(ctx.mark_buffer_submitted(buffer_id));

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut compositor, &mut ctx),
            Action::Drop
        );
        assert!(
            !ctx.buffer_is_submitted(buffer_id),
            "the ordered surface destructor ends the buffer's use"
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Forward,
            "the guest still gets its wl_buffer.release"
        );
        assert!(
            ctx.buffer_is_released(buffer_id),
            "the owed release is recorded instead of being ignored as unexpected"
        );
        assert!(
            !ctx.mark_buffer_released(buffer_id),
            "a second release is still unexpected"
        );
    }

    #[test]
    fn attaching_idle_shm_buffer_without_commit_keeps_release_edge() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.mark_buffer_submitted(buffer_id);

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Forward
        );
        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        // No commit follows the attach yet. Keep the host object alive until
        // the pending state is resolved; otherwise a later commit could race
        // the queued destructor.
        ctx.last_sender_id = buffer_id;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        // Replacing the pending attach proves that it will never commit, so
        // the host destructor can now be ordered safely.
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(buffer_host_destroy_is_queued(&ctx, buffer_id));
        assert!(ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));
    }

    #[test]
    fn attaching_idle_native_buffer_without_commit_keeps_release_edge() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, buffer_id, (1, 1));
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.mark_buffer_submitted(buffer_id);

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Forward
        );
        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        ctx.last_sender_id = buffer_id;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(buffer_host_destroy_is_queued(&ctx, buffer_id));
        assert!(ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));
    }

    #[test]
    fn delayed_shm_release_before_pending_commit_keeps_host_buffer_alive() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, StalePixels::all()),
        );
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.mark_buffer_submitted(buffer_id);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));

        // This release belongs to the old committed interval. It must not
        // destroy the host object while the pending attach can still commit.
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(ctx.buffer_is_submitted(buffer_id));

        // A release for the newly committed interval is the one that permits
        // the host destructor.
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, buffer_id));
        assert!(ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));
    }

    #[test]
    fn delayed_native_release_before_pending_commit_keeps_host_buffer_alive() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, buffer_id, (1, 1));
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), None);
        ctx.mark_buffer_submitted(buffer_id);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));

        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(ctx.buffer_is_released(buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.buffer_is_submitted(buffer_id));
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert!(!ctx.buffer_is_released(buffer_id));

        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, buffer_id));
        assert!(ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));
    }

    #[test]
    fn replacing_pending_destroyed_native_buffer_does_not_wait_for_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let old_buffer = 42;
        let old_host_buffer = 43;
        let new_buffer = 44;
        ctx.shadow_table.insert_paired_with_version_for_test(
            old_buffer,
            old_host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, old_buffer, (1, 1));

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, old_buffer, 0, 0),
            Action::Forward
        );
        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = old_buffer;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(
            buffer_is_guest_destroyed(&ctx, old_buffer),
            "a pending attach keeps the native buffer alive until replaced"
        );

        // The old pending attach is superseded before any commit reaches the
        // compositor, so no wl_buffer.release event can arrive for it.
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, new_buffer, 0, 0),
            Action::Forward
        );
        assert!(buffer_host_destroy_is_queued(&ctx, old_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_sender(message)),
            Some(old_host_buffer)
        );
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_opcode(message)),
            Some(0),
            "superseded pending buffer must be destroyed after the replacement attach"
        );
    }

    #[test]
    fn replacing_pending_destroyed_shm_buffer_does_not_wait_for_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let old_buffer = 42;
        let old_host_buffer = 43;
        let new_buffer = 44;
        ctx.shadow_table.insert_paired_with_version_for_test(
            old_buffer,
            old_host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            old_buffer,
            mapped_test_buffer(old_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, old_buffer, 0, 0),
            Action::Forward
        );
        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = old_buffer;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(buffer_is_guest_destroyed(&ctx, old_buffer));

        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, new_buffer, 0, 0),
            Action::Forward
        );
        assert!(buffer_host_destroy_is_queued(&ctx, old_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_sender(message)),
            Some(old_host_buffer)
        );
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_opcode(message)),
            Some(0),
            "superseded pending SHM buffer must be destroyed after replacement attach"
        );
    }

    #[test]
    fn replacing_detached_submitted_buffer_waits_for_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let guest_buffer = 42;
        let host_buffer = 43;
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        // The buffer was committed by an earlier surface state and destroyed
        // by the guest, but its host use interval is still in flight. It is
        // intentionally no longer referenced by the surface below: a later
        // attach must not mistake that state for an uncommitted pending attach.
        ctx.mark_buffer_submitted(guest_buffer);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 44, 0, 0), Action::Forward);
        assert!(buffer_is_guest_destroyed(&ctx, guest_buffer));
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "a submitted buffer must wait for wl_buffer.release after detach"
        );
    }

    #[test]
    fn commit_from_a_truncated_pool_is_a_protocol_error_not_a_crash() {
        use crate::protocols::wayland::wl_shm::WlShmHandler;

        const BUFFER_BYTES: usize = 4;
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let shm_id = 10;
        let pool_id = 20;
        let buffer_id = 42;
        let surface_id = 100;
        let client_file = shm_pool_file(&[0x11; BUFFER_BYTES]);
        ctx.last_sender_id = shm_id;
        assert_eq!(
            crate::handler::shm::ShmHandler.on_create_pool(
                &mut ctx,
                pool_id,
                client_file.as_fd(),
                BUFFER_BYTES as i32,
            ),
            Action::Drop
        );
        assert!(!ctx.is_fatal());
        let mut buffer =
            mapped_test_buffer(buffer_id, 1, 1, BUFFER_BYTES as u32, StalePixels::all());
        buffer.pool_file = Arc::clone(&ctx.pools[&pool_id].file);
        register_test_local(&mut ctx, buffer_id, buffer);

        // The client shrinks its pool file behind the proxy's back. Reading
        // the pixels through a mapping of it raised SIGBUS and killed the
        // proxy with every guest GUI app.
        client_file.set_len(0).unwrap();
        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(
            protocol_error(&ctx),
            (buffer_id, wl_shm::ERROR_INVALID_FD),
            "the error names the buffer whose pool failed, as libwayland does"
        );
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "the commit must not reach the host"
        );
    }

    #[test]
    fn reattached_buffer_copies_the_damage_of_the_frames_it_missed() {
        // Damage is relative to the surface's current contents. A client
        // alternating A, B, A damages only what changed since B, so A's host
        // copy must also receive what B's frame changed.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        commit_test_frame(&mut ctx, TEST_SURFACE, 44);
        let mut handler = CompositorHandler;

        draw_test_frame(&ctx, 44, 0x22);
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 0, 0, 2, 1), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 44);
        assert_eq!(copied_pixels(&ctx, 44, 0x22), rect_pixels(0, 0, 2, 1));

        // A's next frame holds B's change too, but damages only its own.
        draw_test_frame(&ctx, 42, 0x33);
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 2, 3, 1, 1), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        let mut expected = rect_pixels(0, 0, 2, 1);
        expected.extend(rect_pixels(2, 3, 1, 1));
        assert_eq!(copied_pixels(&ctx, 42, 0x33), expected);
    }

    fn damage_test_buffer(ctx: &mut Context, surface: u32, rect: (i32, i32, i32, i32)) {
        ctx.last_sender_id = surface;
        let (x, y, width, height) = rect;
        assert_eq!(
            CompositorHandler.on_damage_buffer(ctx, x, y, width, height),
            Action::Drop
        );
    }

    #[test]
    fn reattached_buffer_copies_the_damage_of_every_frame_it_missed() {
        // A, B, C, A: A must also receive what both B's and C's frames changed.
        let (mut ctx, ..) = setup_ctx();
        for buffer in [42, 44, 46] {
            commit_test_frame(&mut ctx, TEST_SURFACE, buffer);
        }
        // B changes pixel (0, 0), then C changes pixel (1, 0).
        for (buffer, x) in [(44, 0), (46, 1)] {
            damage_test_buffer(&mut ctx, TEST_SURFACE, (x, 0, 1, 1));
            attach_and_commit(&mut ctx, TEST_SURFACE, buffer);
        }
        draw_test_frame(&ctx, 42, 0x44);
        damage_test_buffer(&mut ctx, TEST_SURFACE, (3, 3, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x44), [(0, 0), (1, 0), (3, 3)]);
    }

    #[test]
    fn damage_skipped_on_a_released_buffer_is_copied_at_its_next_attach() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        assert!(ctx.mark_buffer_released(42));
        draw_test_frame(&ctx, 42, 0x22);
        damage_test_buffer(&mut ctx, TEST_SURFACE, (1, 2, 2, 1));
        assert_eq!(CompositorHandler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), []);

        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), rect_pixels(1, 2, 2, 1));
    }

    #[test]
    fn a_buffer_moving_between_surfaces_copies_everything() {
        // Its host copy relates to the contents of one surface only.
        let (mut ctx, ..) = setup_ctx();
        let other_surface = 101;
        ctx.shadow_table
            .insert_paired_for_test(other_surface, 201, "wl_surface");
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        for surface in [other_surface, TEST_SURFACE] {
            draw_test_frame(&ctx, 42, 0x22);
            damage_test_buffer(&mut ctx, surface, (0, 0, 1, 1));
            attach_and_commit(&mut ctx, surface, 42);
            assert_eq!(
                copied_pixels(&ctx, 42, 0x22),
                rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
            );
        }
    }

    #[test]
    fn a_failed_copy_stales_the_whole_host_copy() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        let set_dmabuf_sync = |ctx: &mut Context, enabled| {
            let host_id = ctx.render_buffer_host_id(42).unwrap();
            ctx.local_buffer_copy_resources(host_id)
                .unwrap()
                .buffer
                .dmabuf_sync = enabled;
        };
        // Without a VirtWL channel the dma-buf sync, and so the copy, fails.
        set_dmabuf_sync(&mut ctx, true);
        draw_test_frame(&ctx, 42, 0x22);
        damage_test_buffer(&mut ctx, TEST_SURFACE, (0, 0, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(copied_pixels(&ctx, 42, 0x22), []);

        set_dmabuf_sync(&mut ctx, false);
        damage_test_buffer(&mut ctx, TEST_SURFACE, (0, 0, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x22),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    #[test]
    fn a_mapping_change_stales_the_other_host_copies() {
        // B's frame is rotated 180 degrees: its pixel damage does not mark
        // the same pixels of A, which was shown unrotated.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        commit_test_frame(&mut ctx, TEST_SURFACE, 44);
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            CompositorHandler.on_set_buffer_transform(&mut ctx, 2),
            Action::Forward
        );
        damage_test_buffer(&mut ctx, TEST_SURFACE, (0, 0, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 44);

        draw_test_frame(&ctx, 42, 0x33);
        damage_test_buffer(&mut ctx, TEST_SURFACE, (0, 0, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x33),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    #[test]
    fn a_buffer_attached_with_a_new_mapping_copies_everything() {
        // A's host copy holds its old frame, unrotated. The client rotates A
        // by 180 degrees and damages one pixel: the rest of A's host copy no
        // longer matches what the surface shows.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        commit_test_frame(&mut ctx, TEST_SURFACE, 44);
        draw_test_frame(&ctx, 42, 0x33);
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            CompositorHandler.on_set_buffer_transform(&mut ctx, 2),
            Action::Forward
        );
        damage_test_buffer(&mut ctx, TEST_SURFACE, (0, 0, 1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x33),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    #[test]
    fn a_frame_of_another_size_stales_the_whole_host_copy() {
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        register_test_local(
            &mut ctx,
            46,
            mapped_test_buffer(46, 2, 2, 8, StalePixels::all()),
        );
        attach_and_commit(&mut ctx, TEST_SURFACE, 46);

        draw_test_frame(&ctx, 42, 0x33);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 0, 0, 1, 1), Action::Drop);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(
            copied_pixels(&ctx, 42, 0x33),
            rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
        );
    }

    #[test]
    fn destroying_a_surface_stales_the_host_copies_it_showed() {
        // A new surface may reuse the ID; nothing relates to its contents.
        let (mut ctx, ..) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        let in_sync = |ctx: &Context| {
            ctx.local_buffer(42)
                .unwrap()
                .stale
                .to_copy_on(TEST_SURFACE)
                .is_empty()
        };
        assert!(in_sync(&ctx));

        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert!(!in_sync(&ctx));
    }

    #[test]
    fn damage_only_commit_copies_the_mutated_current_shm_buffer() {
        // The SHM Commit Copy Invariant: the host samples the current buffer
        // again for a commit without an attach, so its damage must be copied
        // before the commit is forwarded.
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        ctx.client_to_host_queue.clear();
        draw_test_frame(&ctx, 42, 0x22);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 1, 2, 2, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(copied_pixels(&ctx, 42, 0x22), rect_pixels(1, 2, 2, 1));
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue).last(),
            Some(&(wl_surface_host, REQ_COMMIT))
        );
    }

    #[test]
    fn damage_only_commit_does_not_read_a_released_shm_buffer() {
        // After wl_buffer.release the client owns the storage and may be
        // drawing its next frame there; the host no longer samples it.
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        assert!(ctx.mark_buffer_released(42));
        ctx.client_to_host_queue.clear();
        draw_test_frame(&ctx, 42, 0x22);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 1, 2, 2, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(copied_pixels(&ctx, 42, 0x22), []);
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue).last(),
            Some(&(wl_surface_host, REQ_COMMIT))
        );
    }

    #[test]
    fn commit_without_damage_on_a_held_buffer_skips_the_copy() {
        // A commit that only acknowledges a configure must reach the host
        // even when the buffer's dma-buf sync would fail (here: no channel).
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        commit_test_frame(&mut ctx, TEST_SURFACE, 42);
        let host_id = ctx.render_buffer_host_id(42).unwrap();
        ctx.local_buffer_copy_resources(host_id)
            .unwrap()
            .buffer
            .dmabuf_sync = true;
        ctx.client_to_host_queue.clear();

        // No damage at all, then damage that clips to nothing.
        for damage in [None, Some((FRAME_SIDE as i32, 0, 1, 1))] {
            ctx.last_sender_id = TEST_SURFACE;
            if let Some((x, y, width, height)) = damage {
                assert_eq!(
                    CompositorHandler.on_damage_buffer(&mut ctx, x, y, width, height),
                    Action::Drop
                );
            }
            assert_eq!(CompositorHandler.on_commit(&mut ctx), Action::Drop);
            assert!(!ctx.is_fatal());
            assert_eq!(
                queued_headers(&ctx.client_to_host_queue).last(),
                Some(&(wl_surface_host, REQ_COMMIT))
            );
            ctx.client_to_host_queue.clear();
        }
    }

    #[test]
    fn destroyed_pending_shm_attach_survives_until_commit() {
        const BUFFER_BYTES: usize = 4;
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.set_guest_version_for_test(surface_id, 5);
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer_id,
            host_buffer_id,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, BUFFER_BYTES as u32, StalePixels::all()),
        );
        let pool = test_pool_file(&ctx, buffer_id);
        let destination = ctx.local_buffer(buffer_id).unwrap().dest_ptr;
        // This buffer has completed a previous compositor-use interval. A new
        // attach must reopen that interval even when the guest destroys the
        // object before commit.
        assert!(ctx.mark_buffer_submitted(buffer_id));
        assert!(ctx.mark_buffer_released(buffer_id));

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        // Wayland permits destroying the guest object after attach and before
        // commit. The host attach is already queued, so the local backing and
        // pending surface reference must survive until commit.
        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(buffer_is_guest_destroyed(&ctx, buffer_id));
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.pending.attachment),
            Some(SurfaceAttachment::Attach(buffer_id))
        );

        pool.write_all_at(&[0x22; BUFFER_BYTES], 0).unwrap();
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        // The guest object is gone, but the local backing is not.
        assert_eq!(
            unsafe { std::slice::from_raw_parts(destination, BUFFER_BYTES) },
            [0x22; BUFFER_BYTES],
            "destroying the guest buffer before commit must not lose its frame"
        );
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(SurfaceState::current_buffer_id),
            Some(buffer_id)
        );
        assert!(ctx.buffer_is_submitted(buffer_id));
    }

    #[test]
    fn rejects_invalid_scale_and_transform_without_mutating_pending_state() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        // A client gets one error, so each rejection starts afresh.
        let mut rejected = |reject: &dyn Fn(&mut CompositorHandler, &mut Context) -> Action,
                            code: u32| {
            ctx.host_to_client_queue.clear();
            ctx.clear_session_end_for_test();
            assert_eq!(reject(&mut handler, &mut ctx), Action::Drop);
            assert_eq!(protocol_error_code(&ctx), code);
        };
        // wl_surface.invalid_scale for 0 and negative scales, and
        // invalid_transform outside wl_output.transform's 0..=7.
        rejected(&|handler, ctx| handler.on_set_buffer_scale(ctx, 0), 0);
        rejected(&|handler, ctx| handler.on_set_buffer_scale(ctx, -2), 0);
        rejected(&|handler, ctx| handler.on_set_buffer_transform(ctx, 8), 1);
        assert!(
            ctx.surfaces.get(&surface_id).is_none_or(|surface| {
                surface.pending.buffer_scale.is_none() && surface.pending.buffer_transform.is_none()
            }),
            "invalid requests must not create or mutate pending surface state"
        );
    }

    #[test]
    fn v5_attach_rejects_nonzero_offsets_but_legacy_attach_tracks_them() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table.set_guest_version_for_test(surface_id, 5);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.pending.attachment)
                .filter(|attachment| *attachment != SurfaceAttachment::Unchanged),
            None
        );
        assert_eq!(protocol_error_code(&ctx), 3);
        assert!(ctx.is_fatal());

        ctx.host_to_client_queue.clear();
        ctx.clear_session_end_for_test();
        ctx.shadow_table.set_guest_version_for_test(surface_id, 4);
        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending.attach_offset),
            Some((3, 4))
        );
    }

    #[test]
    fn v5_zero_attach_does_not_create_an_attach_offset() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table.set_guest_version_for_test(surface_id, 5);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_offset(&mut ctx, 7, 8), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending.offset),
            None,
            "commit must consume the one-shot offset"
        );

        // Version 5+ ignores attach's zero coordinates, so the new attachment
        // must not acquire a synthetic legacy offset.
        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending.attach_offset),
            None
        );
    }

    #[test]
    fn offset_request_wins_over_legacy_attach_offset_at_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table.set_guest_version_for_test(surface_id, 4);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        // Simulate a v5+ offset request in the state path; the commit logic
        // must prefer it over the legacy attach coordinates.
        ctx.surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .pending
            .offset = Some((8, 9));
        let commit = ctx
            .surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .prepare_commit();
        assert_eq!(
            commit.buffer_offset,
            (8, 9),
            "an explicit offset must win over legacy attach coordinates"
        );
    }

    #[test]
    fn consumed_attach_offset_is_not_reapplied_on_later_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table.set_guest_version_for_test(surface_id, 4);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        ctx.surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .pending
            .offset = Some((8, 9));
        let surface = ctx.surfaces.get_mut(&surface_id).expect("surface state");
        assert_eq!(surface.prepare_commit().buffer_offset, (8, 9));
        assert_eq!(
            surface.prepare_commit().buffer_offset,
            (0, 0),
            "the old explicit and legacy offsets must both be consumed"
        );
    }

    #[test]
    fn damage_buffer_is_translated_to_host_damage_before_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_damage_buffer(&mut ctx, 4, 5, 6, 7), Action::Drop);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "damage_buffer must not be forwarded with its guest opcode"
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(200, REQ_DAMAGE), (200, REQ_COMMIT)],
            "translated damage must be queued before the replacement commit"
        );
        assert_eq!(host_damage(&ctx), [(3, 4, 8, 9)]);
    }

    #[test]
    fn first_shm_commit_without_explicit_damage_queues_full_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, StalePixels::all()),
        );

        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(200, REQ_DAMAGE), (200, REQ_COMMIT)]
        );
        assert_eq!(host_damage(&ctx), [(0, 0, 8, 6)]);
    }

    #[test]
    fn failed_shm_copy_does_not_queue_a_host_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let mut buffer = mapped_test_buffer(buffer_id, 8, 6, 32, StalePixels::all());
        let destination_ptr = buffer.dest_ptr;
        buffer.dest_ptr = std::ptr::null_mut();
        buffer.dest_size = 0;
        unsafe {
            libc::munmap(destination_ptr as *mut libc::c_void, 32 * 6);
        }
        register_test_local(&mut ctx, buffer_id, buffer);

        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "stale or uninitialised host pixels must never be committed"
        );
        assert!(
            ctx.local_buffer(buffer_id)
                .is_some_and(|buffer| buffer.stale.to_copy_on(surface_id).is_full()),
            "a failed copy must force a complete retry on the next commit"
        );
    }

    #[test]
    fn missing_host_surface_ends_the_session_without_a_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(handler.on_damage(&mut ctx, 1, 2, 3, 4), Action::Drop);
        assert_eq!(handler.on_offset(&mut ctx, 5, 6), Action::Forward);
        ctx.release_deleted_host_id(200);

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.is_fatal());
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn terminal_buffer_cannot_partially_commit_a_new_attach() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_native(&mut ctx, buffer_id, (8, 6));
        let host_id = ctx.render_buffer_host_id(buffer_id).unwrap();
        assert!(ctx.complete_queued_buffer_destroy(buffer_id, host_id));

        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.is_fatal());
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            ctx.render_buffer_lifecycle_for_host(host_id),
            Some(RenderBufferLifecycle::HostDestroyQueued)
        );
    }

    #[test]
    fn invalid_damage_spam_does_not_escalate_to_full_frame_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        for _ in 0..300 {
            assert_eq!(handler.on_damage(&mut ctx, 0, 0, 0, 10), Action::Drop);
            assert_eq!(
                handler.on_damage_buffer(&mut ctx, 0, 0, 10, -1),
                Action::Drop
            );
        }
        let surface = ctx.surfaces.get(&surface_id).unwrap();
        assert!(surface.pending.surface_damage.is_empty());
        assert!(surface.pending.buffer_damage.is_empty());

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_COMMIT);
    }

    #[test]
    fn bounded_damage_overflow_uses_the_exact_content_extent() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_native(&mut ctx, buffer_id, (8, 6));
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        for index in 0..=256 {
            assert_eq!(
                handler.on_damage_buffer(&mut ctx, index, index, 1, 1),
                Action::Drop
            );
        }

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(200, REQ_DAMAGE), (200, REQ_COMMIT)]
        );
        assert_eq!(host_damage(&ctx), [(0, 0, 8, 6)]);
    }

    #[test]
    fn surface_damage_is_outset_before_host_forwarding() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_damage(&mut ctx, 10, 20, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(host_damage(&ctx), [(9, 19, 3, 3)]);
    }

    /// The rectangles of the host `wl_surface.damage` requests queued so far.
    fn host_damage(ctx: &Context) -> Vec<(i32, i32, i32, i32)> {
        ctx.client_to_host_queue
            .iter()
            .filter(|(message, _)| msg_sender(message) == 200 && msg_opcode(message) == REQ_DAMAGE)
            .map(|(message, _)| {
                let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &message[8..]);
                let mut next = || wire.read_i32().unwrap();
                (next(), next(), next(), next())
            })
            .collect()
    }

    /// A surface whose committed state shows its buffer at `scale` through
    /// a viewport with `source` (in 24.8 fixed point) and `destination`.
    fn viewed_surface(
        scale: i32,
        source: Option<(i32, i32, i32, i32)>,
        destination: Option<(i32, i32)>,
    ) -> SurfaceState {
        let mut surface = SurfaceState::default();
        surface.current_buffer_scale = scale;
        surface.viewport = Some(ViewportState {
            source,
            destination,
        });
        surface
    }

    /// The host damage of a commit with buffer damage `rect` on `surface`,
    /// which shows a buffer of `size`.
    fn host_damage_for(
        surface: SurfaceState,
        size: (i32, i32),
        rect: (i32, i32, i32, i32),
    ) -> Vec<(i32, i32, i32, i32)> {
        let (mut ctx, ..) = setup_ctx();
        ctx.surfaces.insert(TEST_SURFACE, surface);
        set_test_surface_content(&mut ctx, TEST_SURFACE, None, Some(size));
        ctx.last_sender_id = TEST_SURFACE;
        let (x, y, width, height) = rect;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, x, y, width, height),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        host_damage(&ctx)
    }

    /// Every host damage rectangle, partial or full, keeps both edges within
    /// the headroom, however far a viewport stretches the buffer.
    #[test]
    fn host_damage_is_clamped_on_both_edges() {
        const MAX: i32 = i32::MAX / 10;
        // A 1x1 buffer stretched to 1000x1000 puts damage at buffer pixel
        // 1000000 past the headroom: it shrinks to the headroom's last unit.
        assert_eq!(
            host_damage_for(
                viewed_surface(1, None, Some((1000, 1000))),
                (1, 1),
                (1_000_000, 0, 1, 1)
            ),
            [(MAX - 1, -1000, 1, 3000)]
        );
        // Stretched to i32::MAX units, the left edge of the last pixel would
        // leave no room for its width.
        assert_eq!(
            host_damage_for(
                viewed_surface(1, Some((0, 0, 256, 256)), Some((i32::MAX, 256))),
                (1, 1),
                (i32::MAX, 0, 1, 1)
            ),
            [(MAX - 1, -256, 1, 768)]
        );
        // A buffer attached under a viewport without damage damages the
        // whole destination.
        let (mut ctx, ..) = setup_ctx();
        ctx.surfaces.insert(
            TEST_SURFACE,
            viewed_surface(1, None, Some((i32::MAX, i32::MAX))),
        );
        register_test_native(&mut ctx, 42, (1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(host_damage(&ctx), [(0, 0, MAX, MAX)]);
    }

    #[test]
    fn damage_buffer_mapping_applies_buffer_scale_and_filtering_outset() {
        let mut surface = SurfaceState::default();
        surface.current_buffer_scale = 2;

        let mapped = surface
            .mapping(Some((100, 100)))
            .to_surface(BufferRect::new(20, 10, 4, 6));

        assert_eq!(mapped, SurfaceRect::new(9, 4, 4, 5));
    }

    #[test]
    fn full_mapping_without_explicit_damage_still_damages_the_host_surface() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, StalePixels::in_sync_on(TEST_SURFACE)),
        );

        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_buffer_transform(&mut ctx, 1),
            Action::Forward
        );
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(host_damage(&ctx), [(0, 0, 6, 8)]);
    }

    #[test]
    fn damage_buffer_mapping_does_not_include_surface_offset() {
        let mut surface = SurfaceState::default();
        surface.pending.offset = Some((80, 90));
        let commit = surface.prepare_commit();
        assert_eq!(commit.buffer_offset, (80, 90));

        let mapped = commit
            .state
            .mapping(Some((100, 100)))
            .to_surface(BufferRect::new(4, 5, 6, 7));

        assert_eq!(
            mapped,
            SurfaceRect::new(3, 4, 8, 9),
            "wl_surface.attach/offset changes content placement, not buffer damage coordinates"
        );
    }

    #[test]
    fn offset_without_explicit_damage_does_not_invent_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        let mut surface = SurfaceState::default();
        surface.pending.offset = Some((3, 0));
        ctx.surfaces.insert(surface_id, surface);
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), Some((8, 6)));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| msg_opcode(message) != REQ_DAMAGE),
            "offset-only state must not manufacture a damage request"
        );
    }

    #[test]
    fn nonzero_surface_offset_with_explicit_damage_forces_full_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        let mut surface = SurfaceState::default();
        surface.pending.offset = Some((3, 0));
        ctx.surfaces.insert(surface_id, surface);
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), Some((8, 6)));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 1, 1, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(
            host_damage(&ctx),
            [(0, 0, 8, 6)],
            "offset plus explicit damage must include complete host damage"
        );
    }

    #[test]
    fn extreme_surface_damage_is_clamped_before_host_encoding() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.last_sender_id = 100;
        let mut handler = CompositorHandler;

        assert_eq!(
            handler.on_damage(&mut ctx, i32::MIN, 0, i32::MAX, 1),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(host_damage(&ctx), [(i32::MIN / 10, -1, 214_748_364, 3)]);
    }

    #[test]
    fn rotated_buffer_damage_covers_the_transformed_surface() {
        let mut surface = SurfaceState::default();
        surface.current_buffer_transform = 1;
        let mapping = surface.mapping(Some((8, 6)));

        assert_eq!(
            mapping.surface_rect(),
            SurfaceRect::new(0, 0, 6, 8),
            "90-degree transforms expose a height-by-width surface extent"
        );
        assert_eq!(
            mapping.to_surface(BufferRect::new(1, 1, 1, 1)),
            mapping.surface_rect(),
            "the bridge does not rotate damage, so it damages the whole surface"
        );
    }

    #[test]
    fn rotated_scaled_buffer_full_damage_uses_logical_extent() {
        let mut surface = SurfaceState::default();
        surface.current_buffer_scale = 2;
        surface.current_buffer_transform = 1;

        assert_eq!(
            surface.mapping(Some((8, 6))).surface_rect(),
            SurfaceRect::new(0, 0, 3, 4),
            "buffer scale converts the rotated extent to surface coordinates"
        );
    }

    #[test]
    fn viewport_damage_uses_committed_crop_and_destination() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, surface_id),
            Action::Forward
        );
        ctx.last_sender_id = 700;
        assert_eq!(
            handler.on_set_source(&mut ctx, 65_536, 131_072, 262_144, 262_144),
            Action::Forward
        );
        assert_eq!(
            handler.on_set_destination(&mut ctx, 200, 100),
            Action::Forward
        );

        // The viewport becomes current at its own commit, as a client sets
        // it once; a later frame's buffer damage is converted from buffer
        // pixels into destination coordinates. (A buffer attached with a new
        // mapping damages the whole host surface.)
        ctx.last_sender_id = surface_id;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        ctx.client_to_host_queue.clear();
        register_test_local(
            &mut ctx,
            42,
            mapped_test_buffer(42, 2048, 2048, 8192, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, 256, 512, 256, 256),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        // The damage, grown by a buffer pixel, starts just left of and above
        // the source, so its mapped edges round outward to -1.
        assert_eq!(host_damage(&ctx), [(-1, -1, 52, 27)]);
    }

    #[test]
    fn buffer_damage_maps_through_buffer_scale_before_the_viewport() {
        // Scale 2 makes the 100-pixel buffer a 50-unit surface, which the
        // destination stretches back to 100 units: a pixel is one unit.
        assert_eq!(
            host_damage_for(
                viewed_surface(2, None, Some((100, 100))),
                (100, 100),
                (80, 80, 20, 20)
            ),
            [(79, 79, 22, 22)]
        );
    }

    #[test]
    fn whole_buffer_damage_covers_the_whole_destination() {
        assert_eq!(
            host_damage_for(
                viewed_surface(2, None, Some((100, 100))),
                (100, 100),
                (0, 0, 100, 100)
            ),
            [(-1, -1, 102, 102)]
        );
    }

    #[test]
    fn buffer_damage_subtracts_the_source_offset_in_surface_units() {
        // The source starts 10 units, which are 20 buffer pixels, into the
        // 200-pixel buffer at scale 2.
        let source = Some((10 * 256, 10 * 256, 80 * 256, 80 * 256));
        assert_eq!(
            host_damage_for(
                viewed_surface(2, source, Some((80, 80))),
                (200, 200),
                (100, 100, 20, 20)
            ),
            [(39, 39, 12, 12)]
        );
    }

    #[test]
    fn source_only_viewport_maps_buffer_damage() {
        // Without a destination the source crops without scaling, and its
        // origin keeps its fraction: x starts at 10.5 units.
        let source = Some((10 * 256 + 128, 10 * 256, 80 * 256, 80 * 256));
        assert_eq!(
            host_damage_for(
                viewed_surface(2, source, None),
                (200, 200),
                (100, 100, 20, 20)
            ),
            [(39, 39, 11, 12)]
        );
    }

    #[test]
    fn destroying_viewport_is_double_buffered_until_surface_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let mut handler = CompositorHandler;

        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, surface_id),
            Action::Forward
        );
        // The generated dispatcher pairs the forwarded viewport.
        ctx.shadow_table
            .insert_paired_for_test(700, 701, "wp_viewport");
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(ctx
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.viewport)
            .is_some());

        ctx.guest_destroyed(700);
        assert!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.viewport)
                .is_some(),
            "destroy only changes pending viewport state"
        );

        ctx.last_sender_id = surface_id;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.viewport)
                .is_none(),
            "the viewport is removed at the following surface commit"
        );
    }

    #[test]
    fn invalid_viewport_values_and_dead_surface_are_fatal() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let mut handler = CompositorHandler;

        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, surface_id),
            Action::Forward
        );
        ctx.last_sender_id = 700;
        assert_eq!(handler.on_set_source(&mut ctx, -1, 0, 10, 10), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.is_fatal());

        ctx.host_to_client_queue.clear();
        ctx.clear_session_end_for_test();
        assert_eq!(handler.on_set_destination(&mut ctx, 0, 10), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.is_fatal());

        ctx.host_to_client_queue.clear();
        ctx.clear_session_end_for_test();
        ctx.viewport_to_wl_surface.remove(&700);
        assert_eq!(handler.on_set_destination(&mut ctx, 10, 10), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 3);
        assert!(ctx.is_fatal());
    }

    #[test]
    fn duplicate_viewport_is_a_fatal_viewporter_error() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let mut handler = CompositorHandler;

        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, surface_id),
            Action::Forward
        );
        ctx.host_to_client_queue.clear();
        ctx.clear_session_end_for_test();
        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 701, surface_id),
            Action::Drop
        );
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.is_fatal());
    }

    #[test]
    fn fractional_source_without_destination_is_logged_and_forwarded() {
        // Exo takes the fractional source size as the surface size, and the
        // host damage covers it, rounded up.
        let (mut ctx, ..) = setup_ctx();
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, TEST_SURFACE),
            Action::Forward
        );
        set_test_viewport_source(&mut ctx, (0, 0, 128, 256));
        register_test_native(&mut ctx, 42, (1, 1));
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        assert_eq!(forwarded_and_logged(&ctx), (true, true));
        assert_eq!(host_damage(&ctx), [(0, 0, 1, 1)]);
    }

    /// Set the viewport source of `TEST_SURFACE`, whose viewport is 700.
    fn set_test_viewport_source(ctx: &mut Context, source: (i32, i32, i32, i32)) {
        let mut handler = CompositorHandler;
        ctx.last_sender_id = 700;
        let (x, y, width, height) = source;
        assert_eq!(
            handler.on_set_source(ctx, x, y, width, height),
            Action::Forward
        );
        ctx.last_sender_id = TEST_SURFACE;
    }

    /// Commit a buffer of `size` at scale 2 through a viewport with
    /// `source` (24.8 fixed point) and a destination, which lets the
    /// source be fractional.
    fn commit_viewport_source(size: (i32, i32), source: (i32, i32, i32, i32)) -> Context {
        let (mut ctx, ..) = setup_ctx();
        let mut handler = CompositorHandler;
        ctx.last_sender_id = TEST_SURFACE;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, TEST_SURFACE),
            Action::Forward
        );
        assert_eq!(handler.on_set_buffer_scale(&mut ctx, 2), Action::Forward);
        set_test_viewport_source(&mut ctx, source);
        ctx.last_sender_id = 700;
        assert_eq!(
            handler.on_set_destination(&mut ctx, 10, 10),
            Action::Forward
        );
        register_test_native(&mut ctx, 42, size);
        attach_and_commit(&mut ctx, TEST_SURFACE, 42);
        ctx
    }

    /// Whether the last commit reached the host without ending the session,
    /// and whether the surface's source leaving its buffer was logged.
    fn forwarded_and_logged(ctx: &Context) -> (bool, bool) {
        let forwarded = !ctx.is_fatal()
            && queued_headers(&ctx.client_to_host_queue).last() == Some(&(200, REQ_COMMIT));
        (forwarded, ctx.surfaces[&TEST_SURFACE].logged_viewport_rule)
    }

    #[test]
    fn viewport_source_outside_the_buffer_is_logged_and_forwarded() {
        const ONE: i32 = 256;
        // At scale 2 an 8x6 buffer is a 4x3 surface, the source's units.
        let whole = (0, 0, 4 * ONE, 3 * ONE);
        let ctx = commit_viewport_source((8, 6), whole);
        assert_eq!(forwarded_and_logged(&ctx), (true, false));

        // Half a unit to the right the source leaves the buffer. Exo accepts
        // that, so the commit is forwarded and only logged.
        let ctx = commit_viewport_source((8, 6), (ONE / 2, 0, 4 * ONE, 3 * ONE));
        assert_eq!(forwarded_and_logged(&ctx), (true, true));

        // A later, smaller buffer under the unchanged viewport.
        let mut ctx = commit_viewport_source((8, 6), whole);
        register_test_native(&mut ctx, 43, (6, 6));
        attach_and_commit(&mut ctx, TEST_SURFACE, 43);
        assert_eq!(forwarded_and_logged(&ctx), (true, true));

        // Detaching the buffer in the same commit exempts any source.
        let mut ctx = commit_viewport_source((8, 6), whole);
        let mut handler = CompositorHandler;
        assert_eq!(handler.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        set_test_viewport_source(&mut ctx, (0, 0, 100 * ONE, 100 * ONE));
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(forwarded_and_logged(&ctx), (true, false));

        // An odd buffer ends at a fractional unit, 3.5x2.5, which the source
        // may reach exactly.
        let edge = (0, 0, 3 * ONE + ONE / 2, 2 * ONE + ONE / 2);
        let ctx = commit_viewport_source((7, 5), edge);
        assert_eq!(forwarded_and_logged(&ctx), (true, false));
    }

    #[test]
    fn xdg_toplevel_destroy_cleans_up_map() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();

        let mut handler = CompositorHandler;
        ctx.last_sender_id = xdg_toplevel_id;

        assert!(ctx
            .xdg_roles
            .wl_surface_of_toplevel(xdg_toplevel_id)
            .is_some());

        ctx.last_sender_id = xdg_toplevel_id;
        let action = XdgToplevelHandler::on_destroy(&mut handler, &mut ctx);
        assert_eq!(action, Action::Drop);

        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(xdg_toplevel_id), None);
    }

    #[test]
    fn wl_surface_destroy_releases_zaura_surface() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();

        ctx.last_sender_id = xdg_toplevel_id;
        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"app".to_string());

        let zaura_surface_host = ctx.aura_surfaces.get(wl_surface_host).unwrap();

        let wl_surface_guest = 100u32;
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = wl_surface_guest;
        let action = WlSurfaceHandler::on_destroy(&mut handler, &mut ctx);
        assert_eq!(action, Action::Drop);
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "queued opcodes: {:?}",
            ctx.client_to_host_queue
                .iter()
                .map(|(msg, _)| msg_opcode(msg))
                .collect::<Vec<_>>()
        );
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_RELEASE);
        assert_eq!(
            msg_sender(&ctx.client_to_host_queue[0].0),
            zaura_surface_host
        );
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[1].0), REQ_DESTROY);
        assert_eq!(msg_sender(&ctx.client_to_host_queue[1].0), wl_surface_host);

        assert_eq!(ctx.aura_surfaces.get(wl_surface_host), None);
    }

    #[test]
    fn wl_surface_destroy_retires_deferred_native_buffer_after_surface_destroy() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 60u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, guest_buffer, (1, 1));
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        set_test_surface_content(&mut ctx, wl_surface_guest, Some(guest_buffer), None);

        ctx.last_sender_id = wl_surface_guest;
        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert!(!ctx.buffer_is_submitted(guest_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| msg_sender(message))
                .collect::<Vec<_>>(),
            vec![wl_surface_host, host_buffer],
            "the host surface must be destroyed before the deferred buffer"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_opcode(message)),
            Some(0),
            "the final queued request must be wl_buffer.destroy"
        );
    }

    #[test]
    fn shared_buffer_waits_for_its_last_surface_destroy() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, first_surface_host) = setup_ctx();
        let first_surface = 100u32;
        let second_surface = 101u32;
        let second_surface_host = 201u32;
        let guest_buffer = 50u32;
        let host_buffer = 60u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            second_surface,
            second_surface_host,
            "wl_surface",
            5,
        );
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, guest_buffer, (1, 1));
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        for surface in [first_surface, second_surface] {
            set_test_surface_content(&mut ctx, surface, Some(guest_buffer), None);
        }

        let mut handler = CompositorHandler;
        ctx.last_sender_id = first_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(buffer_is_guest_destroyed(&ctx, guest_buffer));
        assert!(ctx.buffer_is_submitted(guest_buffer));
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "one surface cannot retire a buffer still owned by another"
        );

        ctx.last_sender_id = second_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert!(ctx.host_buffer_use(guest_buffer).is_none());
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| msg_sender(message))
                .collect::<Vec<_>>(),
            vec![first_surface_host, second_surface_host, host_buffer],
            "the buffer destructor must follow both owning surface destructors"
        );
    }

    #[test]
    fn pending_attach_preserves_backing_but_not_destroyed_surface_use() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, first_surface_host) = setup_ctx();
        let first_surface = 100u32;
        let second_surface = 101u32;
        let second_surface_host = 201u32;
        let guest_buffer = 50u32;
        let host_buffer = 60u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            second_surface,
            second_surface_host,
            "wl_surface",
            5,
        );
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, guest_buffer, (1, 1));
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        assert!(ctx.mark_buffer_submitted(guest_buffer));
        set_test_surface_content(&mut ctx, first_surface, Some(guest_buffer), None);
        ctx.surface_mut(second_surface).pending.attachment =
            SurfaceAttachment::Attach(guest_buffer);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = first_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            buffer_lifecycle(&ctx, guest_buffer),
            Some(RenderBufferLifecycle::GuestDestroyed(
                RenderBufferUse::ReleaseOwed
            )),
            "destroying the only committed surface closes its compositor-use interval"
        );
        assert!(
            !buffer_host_destroy_is_queued(&ctx, guest_buffer),
            "the unresolved pending attach must preserve the buffer backing"
        );

        ctx.last_sender_id = second_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| msg_sender(message))
                .collect::<Vec<_>>(),
            vec![first_surface_host, second_surface_host, host_buffer],
            "the buffer destructor must follow both surface destructors without waiting for release"
        );
    }

    #[test]
    fn wl_surface_destroy_retires_submitted_shm_buffer_after_surface_destroy() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        set_test_surface_content(&mut ctx, wl_surface_guest, Some(guest_buffer), None);

        ctx.last_sender_id = wl_surface_guest;
        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert!(!ctx.buffer_is_submitted(guest_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| msg_sender(message))
                .collect::<Vec<_>>(),
            vec![wl_surface_host, host_buffer],
            "the host surface must be destroyed before the retired SHM buffer"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| msg_opcode(message)),
            Some(0),
            "the final queued request must be wl_buffer.destroy"
        );
    }

    #[test]
    fn destroying_surface_then_live_buffer_does_not_wait_for_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        ctx.mark_buffer_submitted(guest_buffer);
        set_test_surface_content(&mut ctx, wl_surface_guest, Some(guest_buffer), None);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = wl_surface_guest;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(
            !ctx.buffer_is_submitted(guest_buffer),
            "the destroyed surface was the only compositor-use reference"
        );

        // The guest is allowed to keep the wl_buffer alive after destroying
        // its surface. Once it eventually destroys the buffer, host teardown
        // must be ordered after the already-queued surface destructor instead
        // of waiting forever for a release that may never arrive.
        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap()))
                .eq([wl_surface_host, host_buffer]),
            "surface destroy must precede the live buffer destroy"
        );
    }

    #[test]
    fn destroying_unrelated_surface_keeps_detached_submitted_buffer() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
        );
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        // This is a committed buffer whose surface was detached before the
        // release edge arrived. It is not owned by surface 100, even though
        // the submitted marker is global.
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = 100;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(buffer_is_guest_destroyed(&ctx, guest_buffer));
        assert!(ctx.buffer_is_submitted(guest_buffer));
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "an unrelated surface destroy must not preempt wl_buffer.release"
        );
    }

    fn assert_detached_use_survives_destroy_of_new_current_surface(native: bool) {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _first_surface_host) = setup_ctx();
        let first_surface = 100u32;
        let second_surface = 101u32;
        let second_surface_host = 201u32;
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            second_surface,
            second_surface_host,
            "wl_surface",
            5,
        );
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        if native {
            register_test_native(&mut ctx, guest_buffer, (1, 1));
        } else {
            register_test_local(
                &mut ctx,
                guest_buffer,
                mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
            );
        }

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = first_surface;
        assert_eq!(
            compositor.on_attach(&mut ctx, guest_buffer, 0, 0),
            Action::Forward
        );
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);

        // Replacing B on S0 begins an unattributed outstanding use: S0 no
        // longer names B, but only wl_buffer.release can prove the host has
        // stopped sampling that detached attachment.
        assert_eq!(compositor.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.host_buffer_use(guest_buffer),
            Some(RenderBufferUse::AwaitingRelease {
                has_detached_use: true
            })
        );

        // A later current use must preserve, not overwrite, the detached
        // latch from S0.
        ctx.last_sender_id = second_surface;
        assert_eq!(
            compositor.on_attach(&mut ctx, guest_buffer, 0, 0),
            Action::Forward
        );
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.host_buffer_use(guest_buffer),
            Some(RenderBufferUse::AwaitingRelease {
                has_detached_use: true
            })
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut shm, &mut ctx),
            Action::Drop
        );
        ctx.client_to_host_queue.clear();

        ctx.last_sender_id = second_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut compositor, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            buffer_lifecycle(&ctx, guest_buffer),
            Some(RenderBufferLifecycle::GuestDestroyed(
                RenderBufferUse::AwaitingRelease {
                    has_detached_use: true
                }
            )),
            "destroying S1 proves only S1's current use, not S0's detached use"
        );
        assert!(
            !buffer_host_destroy_is_queued(&ctx, guest_buffer),
            "the detached S0 use must keep backing alive until host release"
        );

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(
            buffer_host_destroy_is_queued(&ctx, guest_buffer),
            "the global host release closes every outstanding use"
        );
    }

    #[test]
    fn detached_native_use_survives_destroy_of_new_current_surface() {
        assert_detached_use_survives_destroy_of_new_current_surface(true);
    }

    #[test]
    fn detached_local_use_survives_destroy_of_new_current_surface() {
        assert_detached_use_survives_destroy_of_new_current_surface(false);
    }

    fn assert_destroying_pending_only_surface_keeps_awaiting_release(native: bool) {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        if native {
            register_test_native(&mut ctx, guest_buffer, (1, 1));
        } else {
            register_test_local(
                &mut ctx,
                guest_buffer,
                mapped_test_buffer(guest_buffer, 1, 1, 4, StalePixels::in_sync_on(TEST_SURFACE)),
            );
        }

        // The release belongs to an earlier committed use. Merely attaching
        // the same buffer to another surface does not transfer ownership of
        // that outstanding release edge until the new surface commits.
        assert!(ctx.mark_buffer_submitted(guest_buffer));
        let mut compositor = CompositorHandler;
        ctx.last_sender_id = wl_surface_guest;
        assert_eq!(
            compositor.on_attach(&mut ctx, guest_buffer, 0, 0),
            Action::Forward
        );
        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut shm, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            buffer_lifecycle(&ctx, guest_buffer),
            Some(RenderBufferLifecycle::GuestDestroyed(
                RenderBufferUse::AwaitingRelease {
                    has_detached_use: false
                }
            ))
        );

        ctx.last_sender_id = wl_surface_guest;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut compositor, &mut ctx),
            Action::Drop
        );

        assert_eq!(
            buffer_lifecycle(&ctx, guest_buffer),
            Some(RenderBufferLifecycle::GuestDestroyed(
                RenderBufferUse::AwaitingRelease {
                    has_detached_use: false
                }
            )),
            "destroying a pending-only surface must not consume another use's release edge"
        );
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "the host buffer must remain alive until its real release"
        );

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| msg_sender(message))
                .collect::<Vec<_>>(),
            vec![wl_surface_host, host_buffer],
            "the real release must retire the buffer after the surface destructor"
        );
    }

    #[test]
    fn destroying_pending_only_surface_keeps_shm_awaiting_release() {
        assert_destroying_pending_only_surface_keeps_awaiting_release(false);
    }

    #[test]
    fn destroying_pending_only_surface_keeps_native_awaiting_release() {
        assert_destroying_pending_only_surface_keeps_awaiting_release(true);
    }

    #[test]
    fn wl_surface_destroy_removes_stale_xdg_surface_links() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let wl_surface_guest_id = 100u32;
        ctx.xdg_roles.add_xdg_surface(601, wl_surface_guest_id);
        ctx.xdg_roles
            .add_toplevel_role_for_test(603, wl_surface_guest_id, 602);
        ctx.last_sender_id = wl_surface_guest_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.xdg_roles.toplevel_of_xdg_surface(603), None);
        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(602), None);
        // The setup role is also ended because it is on the same surface;
        // no stale association should survive destruction.
        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(xdg_toplevel_id), None);
        assert!(ctx.xdg_roles.is_empty());
    }

    #[test]
    fn wl_surface_destroy_removes_keyboard_focus_links() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let wl_surface_guest_id = 100u32;
        let host_keyboard_id = crate::state::HostId(700);
        ctx.keyboard_focus
            .set_for_test(host_keyboard_id, 1, wl_surface_guest_id, _wl_surface_host);
        let sequence = ctx
            .key_generations
            .observe_peek_press(host_keyboard_id, 14, 1, 123, true);
        ctx.key_generations
            .cancel_backspace_repeat(host_keyboard_id, 14);
        ctx.keyboard_repeatable_keys
            .insert(host_keyboard_id, [14].into_iter().collect());
        ctx.key_generations.record_latest_peek(
            1,
            Some(wl_surface_guest_id),
            host_keyboard_id,
            sequence,
        );
        ctx.key_generations
            .record_latest_peek(2, Some(999), crate::state::HostId(701), 2);
        assert!(ctx.claim_guest_key(
            host_keyboard_id,
            14,
            crate::state::GuestKeyOwner::ImeRecovery
        ));
        assert!(ctx
            .key_generations
            .claim_text_input_owner(host_keyboard_id, 30, 1));
        ctx.key_generations
            .observe_physical_state(host_keyboard_id, 30, 0);
        ctx.key_generations
            .observe_physical_state(host_keyboard_id, 30, 1);
        ctx.keyboard_focus
            .set_for_test(crate::state::HostId(701), 2, 999, 1999);
        ctx.last_sender_id = wl_surface_guest_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(
            ctx.keyboard_focus
                .focus_for_keyboard(crate::state::HostId(700))
                .is_none(),
            "destroying a surface must retire per-keyboard focus"
        );
        assert!(
            !ctx.key_generations.physically_held(host_keyboard_id, 14)
                && !ctx
                    .key_generations
                    .backspace_repeat_cancelled(host_keyboard_id, 14)
                && ctx.key_generations.peek(host_keyboard_id, 14).is_none()
                && ctx.guest_key_owner(host_keyboard_id, 14).is_none(),
            "destroying a surface must retire all per-keyboard input state"
        );
        assert!(
            !ctx.key_generations
                .take_pending_text_input_release(host_keyboard_id, 30, 2),
            "surface destruction must discard retired guest releases"
        );
        assert!(
            ctx.keyboard_repeatable_keys.contains_key(&host_keyboard_id),
            "surface focus teardown must preserve keymap-derived capabilities"
        );
        assert!(
            ctx.key_generations
                .latest_peek_sequence(1, Some(wl_surface_guest_id))
                .is_none(),
            "destroying a surface must retire its peek watermark"
        );
        assert_eq!(
            ctx.key_generations.latest_peek_sequence(2, Some(999)),
            Some(2),
            "another live seat's watermark must remain intact"
        );
        assert_eq!(
            ctx.keyboard_focus
                .focus_for_keyboard(crate::state::HostId(701))
                .map(|focus| focus.guest_surface),
            Some(999),
            "focus for another live surface must remain intact"
        );
    }

    #[test]
    fn wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface = 100;
        let text_input = 700;
        let guest_keyboard = 800;
        let host_keyboard = 900;

        ctx.shadow_table.insert_paired_for_test(1, 2, "wl_seat");
        ctx.shadow_table
            .insert_paired_for_test(text_input, 701, "zwp_text_input_v3");
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard, 1);
        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(host_keyboard),
            1,
            surface,
            _wl_surface_host,
        );
        ctx.text_inputs.insert(text_input, {
            let mut state = crate::state::TextInputState::new(702, None, 1, Some(surface));
            state.current_preedit = "한".to_string();
            state.host_activation = crate::state::HostActivationState::Active;
            state
        });

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut compositor, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.host_to_client_queue[0].0), text_input);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue[0].0), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap()),
            surface
        );
        assert!(ctx.text_inputs[&text_input].active_surface.is_none());

        // The host may report the old keyboard leave after surface teardown.
        // It must clear only stale keyboard state and not emit a second v3
        // leave for the already-destroyed surface.
        let mut keyboard = crate::handler::keyboard::KeyboardHandler::new();
        ctx.last_sender_id = host_keyboard;
        assert_eq!(
            WlKeyboardHandler::on_leave(&mut keyboard, &mut ctx, 1, 200),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
    }

    #[test]
    fn wl_surface_destroy_transitions_all_focused_seats_and_preserves_others() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, destroyed_host_surface) = setup_ctx();
        let destroyed_surface = 100;
        let live_surface = 101;
        let live_host_surface = 201;
        let seats = [(1, 2), (3, 4), (5, 6)];
        let keyboards = [(800, 900), (801, 901), (802, 902)];
        let text_inputs = [(700, 701, 702), (710, 711, 712), (720, 721, 722)];

        ctx.shadow_table
            .insert_paired_for_test(live_surface, live_host_surface, "wl_surface");
        for (guest_seat, host_seat) in seats {
            ctx.shadow_table
                .insert_paired_for_test(guest_seat, host_seat, "wl_seat");
        }
        for ((guest_keyboard, host_keyboard), (guest_seat, _)) in keyboards.into_iter().zip(seats) {
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
            ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        }
        for (((guest_text_input, host_object, host_v1), (guest_seat, _)), active_surface) in
            text_inputs.into_iter().zip(seats).zip([
                destroyed_surface,
                destroyed_surface,
                live_surface,
            ])
        {
            ctx.shadow_table.insert_paired_for_test(
                guest_text_input,
                host_object,
                "zwp_text_input_v3",
            );
            ctx.text_inputs.insert(
                guest_text_input,
                active_text_input_state(host_v1, guest_seat, active_surface),
            );
        }

        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(900),
            1,
            destroyed_surface,
            destroyed_host_surface,
        );
        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(901),
            3,
            destroyed_surface,
            destroyed_host_surface,
        );
        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(902),
            5,
            live_surface,
            live_host_surface,
        );
        for (sequence, (guest_seat, surface, keyboard)) in [
            (1, (1, destroyed_surface, crate::state::HostId(900))),
            (2, (3, destroyed_surface, crate::state::HostId(901))),
            (3, (5, live_surface, crate::state::HostId(902))),
        ] {
            ctx.key_generations
                .record_latest_peek(guest_seat, Some(surface), keyboard, sequence);
        }

        ctx.last_sender_id = destroyed_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );

        for guest_text_input in [700, 710] {
            let state = &ctx.text_inputs[&guest_text_input];
            assert_eq!(state.active_surface, None);
            assert!(!state.committed.enabled);
            assert!(!state.host_is_active());
        }
        let unaffected = &ctx.text_inputs[&720];
        assert_eq!(unaffected.active_surface, Some(live_surface));
        assert!(unaffected.committed.enabled);
        assert!(unaffected.host_is_active());
        assert_eq!(unaffected.current_preedit, "한");

        assert_eq!(ctx.host_to_client_queue.len(), 2);
        for guest_text_input in [700, 710] {
            assert!(ctx.host_to_client_queue.iter().any(|(message, _)| {
                msg_sender(message) == guest_text_input && msg_opcode(message) == 1
            }));
        }
        assert!(ctx
            .host_to_client_queue
            .iter()
            .all(|(message, _)| msg_sender(message) != 720));
        for host_v1 in [702, 712] {
            assert!(ctx.client_to_host_queue.iter().any(|(message, _)| {
                msg_sender(message) == host_v1 && msg_opcode(message) == 1
            }));
        }
        assert!(ctx
            .client_to_host_queue
            .iter()
            .all(|(message, _)| msg_sender(message) != 722));
        assert!(ctx
            .key_generations
            .latest_peek_sequence(1, Some(destroyed_surface))
            .is_none());
        assert!(ctx
            .key_generations
            .latest_peek_sequence(3, Some(destroyed_surface))
            .is_none());
        assert_eq!(
            ctx.key_generations
                .latest_peek_sequence(5, Some(live_surface)),
            Some(3)
        );
        assert_eq!(ctx.keyboard_focus.surface_for_seat(5), Some(live_surface));

        let guest_queue_len = ctx.host_to_client_queue.len();
        let host_queue_len = ctx.client_to_host_queue.len();
        let mut keyboard = crate::handler::keyboard::KeyboardHandler::new();
        for host_keyboard in [900, 901] {
            ctx.last_sender_id = host_keyboard;
            assert_eq!(
                WlKeyboardHandler::on_leave(&mut keyboard, &mut ctx, 10, destroyed_host_surface,),
                Action::Drop
            );
        }
        assert_eq!(ctx.host_to_client_queue.len(), guest_queue_len);
        assert_eq!(ctx.client_to_host_queue.len(), host_queue_len);
    }

    #[test]
    fn wl_surface_destroy_repairs_only_text_inputs_on_the_stale_surface() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let destroyed_surface = 100;
        let replacement_surface = 101;
        let replacement_host_surface = 201;
        let seat_with_replacement = 1;
        let seat_without_owner = 3;
        let stale_with_replacement = 700;
        let healthy_on_replacement = 710;
        let stale_without_owner = 720;

        for (guest_seat, host_seat) in [(seat_with_replacement, 2), (seat_without_owner, 4)] {
            ctx.shadow_table
                .insert_paired_for_test(guest_seat, host_seat, "wl_seat");
        }
        ctx.shadow_table.insert_paired_for_test(
            replacement_surface,
            replacement_host_surface,
            "wl_surface",
        );
        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(900),
            seat_with_replacement,
            replacement_surface,
            replacement_host_surface,
        );
        for (guest_text_input, host_object, state) in [
            (
                stale_with_replacement,
                701,
                active_text_input_state(702, seat_with_replacement, destroyed_surface),
            ),
            (
                healthy_on_replacement,
                711,
                active_text_input_state(712, seat_with_replacement, replacement_surface),
            ),
            (
                stale_without_owner,
                721,
                active_text_input_state(722, seat_without_owner, destroyed_surface),
            ),
        ] {
            ctx.shadow_table.insert_paired_for_test(
                guest_text_input,
                host_object,
                "zwp_text_input_v3",
            );
            ctx.text_inputs.insert(guest_text_input, state);
        }
        ctx.key_generations.record_latest_peek(
            seat_with_replacement,
            Some(destroyed_surface),
            crate::state::HostId(801),
            1,
        );
        ctx.key_generations.record_latest_peek(
            seat_with_replacement,
            Some(replacement_surface),
            crate::state::HostId(802),
            2,
        );
        ctx.key_generations.record_latest_peek(
            seat_without_owner,
            Some(destroyed_surface),
            crate::state::HostId(803),
            3,
        );
        assert!(
            ctx.keyboard_focus
                .surface_for_seat(seat_without_owner)
                .is_none(),
            "one stale projection must have no remaining keyboard owner"
        );

        ctx.last_sender_id = destroyed_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );

        let repaired = &ctx.text_inputs[&stale_with_replacement];
        assert_eq!(repaired.active_surface, Some(replacement_surface));
        assert!(!repaired.committed.enabled);
        assert!(!repaired.host_is_active());
        assert!(repaired.current_preedit.is_empty());

        let repaired_without_owner = &ctx.text_inputs[&stale_without_owner];
        assert_eq!(repaired_without_owner.active_surface, None);
        assert!(!repaired_without_owner.committed.enabled);
        assert!(!repaired_without_owner.host_is_active());

        let healthy = &ctx.text_inputs[&healthy_on_replacement];
        assert_eq!(healthy.active_surface, Some(replacement_surface));
        assert!(healthy.committed.enabled);
        assert!(healthy.host_is_active());
        assert_eq!(healthy.current_preedit, "한");

        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert!(ctx
            .host_to_client_queue
            .iter()
            .all(|(message, _)| { msg_sender(message) != healthy_on_replacement }));
        for host_text_input in [702, 722] {
            assert!(ctx.client_to_host_queue.iter().any(|(message, _)| {
                msg_sender(message) == host_text_input && msg_opcode(message) == 1
            }));
        }
        assert!(ctx
            .client_to_host_queue
            .iter()
            .all(|(message, _)| { msg_sender(message) != 712 }));
        assert!(ctx
            .key_generations
            .latest_peek_sequence(seat_with_replacement, Some(destroyed_surface))
            .is_none());
        assert!(ctx
            .key_generations
            .latest_peek_sequence(seat_without_owner, Some(destroyed_surface))
            .is_none());
        assert_eq!(
            ctx.key_generations
                .latest_peek_sequence(seat_with_replacement, Some(replacement_surface)),
            Some(2)
        );
    }

    #[test]
    fn old_aura_surface_stays_reserved_without_release_request() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell
            .as_mut()
            .expect("bound zaura_shell")
            .version = 37;
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"legacy".to_string());
        let zaura_surface_host = ctx
            .aura_surfaces
            .get(wl_surface_host)
            .expect("set_app_id must create the aura surface");

        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = 100;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| msg_opcode(message) != REQ_RELEASE),
            "old aura-shell versions must not receive release"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(zaura_surface_host),
            None,
            "legacy aura objects must retire dispatch metadata after surface destroy"
        );
        assert_ne!(
            ctx.shadow_table.create_host_only("wl_callback", 1),
            zaura_surface_host
        );
    }
}
