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

use crate::allocator::DmabufFence;
use crate::handler::aura_objects;
use crate::handler::shm::{copy_shm_damage, damage_reaches_buffer, ShmCopyError, ShmCopyLayout};
use crate::handler::window_placement;
use crate::handler::Handlers;
use crate::protocols::viewporter::{wp_viewport, wp_viewporter};
use crate::protocols::wayland::wl_compositor::WlCompositorHandler;
use crate::protocols::wayland::wl_region::WlRegionHandler;
use crate::protocols::wayland::wl_shm;
use crate::protocols::wayland::wl_subcompositor::WlSubcompositorHandler;
use crate::protocols::wayland::wl_subsurface::WlSubsurfaceHandler;
use crate::protocols::wayland::wl_surface::{self, WlSurfaceHandler};
use crate::protocols::xdg_shell::xdg_toplevel;
use crate::state::{
    is_valid_transform, BufferRect, BufferSpace, BufferState, Context, DamageRegion, EventObject,
    EventSender, FenceWait, GuestId, HostId, HostMapping, Link, PostDispatch, RenderBufferUse,
    RequestObject, RequestSender, SurfaceAttachment, SurfaceCommit, SurfaceMapping, SurfaceRect,
    SurfaceState, ViewportState,
};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{Action, Destructor, ToHost};
use log::trace;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

/// RAII guard for the VirtWL dma-buf CPU-write interval.
///
/// A copy can fail while reading the guest pool or while validating a damage
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
    guest_buffer_id: GuestId,
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
    crate::allocator::dmabuf_fence(plane, ctx.virtgpu())
}

impl WlCompositorHandler for Handlers {
    fn on_create_surface(ctx: &mut Context, _sender: RequestSender, id: GuestId) -> Action {
        ctx.create_surface(id);
        Action::Forward
    }
}

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
    xdg_surface_guest_id: GuestId,
    xdg_toplevel_guest_id: GuestId,
) {
    debug_assert!(
        ctx.shadow_table
            .get_host_id(xdg_toplevel_guest_id)
            .is_some(),
        "a forwarded get_toplevel has mapped its new ID"
    );
    if ctx
        .xdg_roles
        .add_toplevel(xdg_surface_guest_id, xdg_toplevel_guest_id)
        .is_some()
    {
        window_placement::attach_toplevel(ctx, xdg_toplevel_guest_id);
    }
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
    surface_id: GuestId,
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

/// The size of `buffer_id`'s host copy if it is a SHM buffer shown for the
/// first time, which its commit copies in full, or `None`. Only counted
/// while the debug log is on.
fn new_host_copy_size(ctx: &Context, buffer_id: GuestId) -> Option<u64> {
    if !log::log_enabled!(log::Level::Debug)
        || ctx.host_buffer_use(buffer_id) != Some(RenderBufferUse::NeverSubmitted)
    {
        return None;
    }
    let host_copy = ctx.local_buffer(buffer_id)?.host_copy.as_ref()?;
    Some(host_copy.len() as u64)
}

/// Count a new host copy of `bytes` that `surface_id` showed, and log the
/// count at most once a second (see `NewHostCopies`).
fn log_new_host_copy(ctx: &mut Context, surface_id: GuestId, bytes: u64) {
    let Some(surface) = ctx.surfaces.get_mut(&surface_id) else {
        return;
    };
    if let Some((count, bytes, elapsed)) = surface
        .new_host_copies
        .record(bytes, std::time::Instant::now())
    {
        log::debug!(
            "wl_surface {surface_id} showed {count} new SHM buffers in {elapsed:.1?}, \
             each a new host allocation copied in full: {bytes} bytes"
        );
    }
}

/// Build the host damage requests and the commit for one guest commit, or
/// `None` when the surface has no host object. The host takes surface-local
/// damage only, so buffer damage goes through `mapping`.
fn build_surface_commit_messages(
    ctx: &Context,
    surface_id: GuestId,
    surface_damage: &[SurfaceRect],
    buffer_damage: &[BufferRect],
    mapping: &SurfaceMapping,
    force_full_damage: bool,
) -> Option<Vec<ToHost>> {
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
    let mut messages: Vec<ToHost> = crate::handler::shm::coalesce_damage_rects(&damage)
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
fn buffer_to_copy(ctx: &Context, commit: &SurfaceCommit) -> Option<GuestId> {
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
    surface_id: GuestId,
    buffer_id: GuestId,
) -> Result<(), ShmCopyError> {
    let Some(host_id) = ctx.render_buffer_host_id(buffer_id) else {
        return Ok(());
    };
    let Some(resources) = ctx.local_buffer_copy_resources(host_id) else {
        return Ok(());
    };
    let buffer = resources.buffer;
    let damage = buffer.stale.to_copy_on(surface_id);
    // Nothing to copy needs no dma-buf sync either. Skipping it also keeps
    // a commit without damage, such as one that acknowledges a configure,
    // from being dropped when the host storage misbehaves.
    if !damage_reaches_buffer(damage, buffer) {
        buffer.stale.copied_on(surface_id);
        return Ok(());
    }
    let result = copy_to_host_storage(buffer, resources.channel, damage);
    match &result {
        Ok(()) => {
            buffer.stale.copied_on(surface_id);
            // A 4K frame can reach this path every refresh. Keep the
            // per-frame diagnostic at trace level.
            trace!(
                "Copied damaged SHM buffer: dst_stride={}, src_stride={}, \
                 width={}, height={}, format={:#010x}, buffer.offset={}, \
                 host_copy_size={}",
                buffer.dest_stride,
                buffer.stride,
                buffer.width,
                buffer.height,
                buffer.format,
                buffer.offset,
                buffer.host_copy.as_ref().map_or(0, HostMapping::len)
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
    buffer: &BufferState,
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
    let result = if let Some(host_copy) = &buffer.host_copy {
        copy_shm_damage(&buffer.pool_file, host_copy, layout, damage)
    } else {
        // Both backings map their storage when the buffer is created, so
        // only a test fixture without a mapping reaches this.
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
    wl_surface_guest_id: GuestId,
    wl_surface_host_id: HostId,
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
/// roles and Aura objects, outputs, GTK surfaces,
/// viewports and pixel state, the buffer uses it held, and its keyboard
/// focus, which ends with a text-input leave while the guest surface ID is
/// still valid.
pub(crate) fn forget_surface(
    ctx: &mut Context,
    wl_surface_guest_id: GuestId,
    wl_surface_host_id: HostId,
) {
    release_surface_aura_objects(ctx, wl_surface_guest_id, wl_surface_host_id);
    ctx.outputs.surface_destroyed(wl_surface_host_id);
    crate::handler::gtk_shell::forget_wl_surface(ctx, wl_surface_guest_id);
    // A submitted buffer may have been detached from another surface and
    // still be waiting for wl_buffer.release; only buffers that this host
    // surface actually held may be retired merely because its destructor
    // is ordered ahead of the buffer destructor.
    let destroyed_surface_buffer = ctx
        .surfaces
        .remove(&wl_surface_guest_id)
        .and_then(|surface| surface.current_buffer_id());
    ctx.forget_surface_buffer_copies(wl_surface_guest_id);
    crate::handler::buffer::clear_buffer_uses_after_surface_destroy(ctx, destroyed_surface_buffer);
    // Pending-only objects do not receive a host release event. Retire
    // every buffer made eligible by removing this surface in one pass.
    crate::handler::buffer::retire_eligible_buffers(ctx);
    for (_, viewport) in ctx.shadow_table.data_of_mut::<Viewport>() {
        if viewport.surface == Some(wl_surface_guest_id) {
            viewport.surface = None;
        }
    }
    ctx.pointer_focus.forget_surface(wl_surface_guest_id);
    // Surface destruction can race the host's wl_keyboard.leave event. The
    // keyboard core moves the focus off the surface first, and the IME core
    // sends the mandatory v3 leave while the guest surface ID is still
    // valid; a later host leave matches no live generation and is
    // idempotent. The IME core hears of the destroy before the focus
    // changes it caused, so that no field commits its preedit into the
    // destroyed surface, as one the focus leaves does.
    let destroyed = crate::keys::KeyInput::SurfaceDestroyed(wl_surface_guest_id);
    crate::handler::keyboard::keys(ctx, destroyed);
}

impl WlSurfaceHandler for Handlers {
    fn on_destroy(ctx: &mut Context, sender: RequestSender) -> Destructor {
        // The Aura objects' releases precede the wl_surface destructor, and
        // what ending the surface sends follows it (see `forget_surface`).
        // The numeric mapping stays until the host acknowledges the
        // destructor with wl_display.delete_id, so the guest can safely
        // reuse the ID only after that acknowledgement.
        if let Some(wl_surface_host_id) = sender.host_id() {
            release_surface_aura_objects(ctx, sender.guest, wl_surface_host_id);
        }
        Destructor::AfterOutput
    }

    /// An enter or leave naming a surface or output the guest has destroyed
    /// never gets here: the proxy suppresses events from a destroyed sender,
    /// and generated validation drops events naming a destroyed object.
    fn on_enter(ctx: &mut Context, sender: EventSender, output: EventObject) -> Action {
        // An output whose global was removed is still a valid guest object,
        // so the event is forwarded; the registry ignores it for placement.
        ctx.outputs.surface_entered(sender.host, output.host);
        Action::Forward
    }

    fn on_leave(ctx: &mut Context, sender: EventSender, output: EventObject) -> Action {
        ctx.outputs.surface_left(sender.host, output.host);
        Action::Forward
    }

    fn on_attach(
        ctx: &mut Context,
        sender: RequestSender,
        buffer: Option<RequestObject>,
        x: i32,
        y: i32,
    ) -> Action {
        let surface_id = sender.guest;
        // An inert buffer, from a pool whose host wl_shm was removed, has
        // no host object, so it reaches the host as no buffer.
        let attached = buffer
            .filter(|buffer| buffer.host != Link::Local)
            .map(|buffer| buffer.guest);
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        let pending = &mut surface.pending;
        pending.attachment = match attached {
            Some(buffer) => SurfaceAttachment::Attach(buffer),
            None => SurfaceAttachment::Detach,
        };
        // A surface is at most v4 (`WL_COMPOSITOR_VERSION`), where attach
        // carries the offset.
        pending.attach_offset = attached.is_some().then_some((x, y));
        // A pending native buffer can be destroyed before the commit that
        // attached it. Replacing that pending attach (including attach(NULL))
        // means the compositor will never emit wl_buffer.release for the
        // superseded object, so retire its host proxy after the new attach
        // request in the ordered queue.
        crate::handler::buffer::retire_eligible_buffers(ctx);
        Action::Forward
    }

    fn on_damage(
        ctx: &mut Context,
        sender: RequestSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = sender.guest;
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface
            .pending
            .surface_damage
            .push(SurfaceRect::new(x, y, width, height));
        // The host compositor only understands surface-coordinate damage. Keep
        // this request local so the commit path can preserve ordering and
        // account for a concurrently pending viewport/scale change.
        Action::Drop
    }

    fn on_damage_buffer(
        ctx: &mut Context,
        sender: RequestSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = sender.guest;
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface
            .pending
            .buffer_damage
            .push(BufferRect::new(x, y, width, height));
        // ChromiumOS Sommelier implements damage_buffer by translating it to
        // wl_surface.damage because the host compositor may only expose v3.
        Action::Drop
    }

    fn on_set_buffer_scale(ctx: &mut Context, sender: RequestSender, scale: i32) -> Action {
        if scale <= 0 {
            log::warn!("Rejecting invalid wl_surface buffer scale {}", scale);
            return ctx.reject(
                sender.guest,
                wl_surface::ERROR_INVALID_SCALE,
                "invalid wl_surface buffer scale",
            );
        }
        let surface_id = sender.guest;
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending.buffer_scale = Some(scale);
        Action::Forward
    }

    fn on_set_buffer_transform(ctx: &mut Context, sender: RequestSender, transform: i32) -> Action {
        if !is_valid_transform(transform) {
            log::warn!(
                "Rejecting invalid wl_surface buffer transform {}",
                transform
            );
            return ctx.reject(
                sender.guest,
                wl_surface::ERROR_INVALID_TRANSFORM,
                "invalid wl_surface buffer transform",
            );
        }
        let surface_id = sender.guest;
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending.buffer_transform = Some(transform);
        Action::Forward
    }

    fn on_commit(ctx: &mut Context, sender: RequestSender) -> Action {
        let surface_id = sender.guest;

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

        let Some(surface) = ctx.surfaces.get_mut(&surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        let commit = surface.prepare_commit();
        let attached_buffer_id = commit.attached_buffer_id();
        let copied_buffer_id = buffer_to_copy(ctx, &commit);

        let shown_buffer = shown_buffer_size(ctx, &commit.state);
        let mapping = commit.state.mapping(shown_buffer);
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
        // `None`: full damage, which sends the whole surface instead of
        // any rectangle.
        let damage = commit
            .surface_damage
            .rects()
            .zip(commit.buffer_damage.rects());
        let (surface_damage, buffer_damage) = damage.unwrap_or_default();
        ctx.stale_surface_pixels(surface_id, &commit, &mapping);
        // A buffer that copies everything shows the host a whole new image.
        let needs_full_damage = copied_buffer_id
            .and_then(|buffer_id| ctx.local_buffer(buffer_id))
            .is_some_and(|buffer| buffer.stale.to_copy_on(surface_id).is_full());
        let force_full_damage = needs_full_damage
            || damage.is_none()
            || (commit.buffer_offset != (0, 0)
                && (!surface_damage.is_empty() || !buffer_damage.is_empty()))
            || (attached_full_mapping && surface_damage.is_empty() && buffer_damage.is_empty());
        let Some(host_messages) = build_surface_commit_messages(
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
        // compositor sample stale or uninitialised pixels if dma-buf sync
        // or the copy fails. A failed copy stales the whole host
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
                crate::handler::buffer::retire_eligible_buffers(ctx);
                return Action::Drop;
            }
        }

        if let Some((previous_buffer, next_buffer)) = commit.attachment_transition() {
            // The pending replacement has now passed every copy and fence
            // check. Apply its content snapshot and both buffer lifecycle
            // edges before appending any host message. The registry
            // validates both generations before changing either one, so a
            // failed replacement remains completely rollback-safe.
            let new_host_copy =
                next_buffer.and_then(|buffer_id| new_host_copy_size(ctx, buffer_id));
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
            if let Some(bytes) = new_host_copy {
                log_new_host_copy(ctx, surface_id, bytes);
            }
        }

        if let Some(problem) = viewport_problem {
            log_unenforced_viewport_rule(ctx, surface_id, problem, &mapping);
        }
        // Damage and commit were built as one batch before local state was
        // finalized. Append them together, in order, only once every check
        // has passed, so the host never sees a partial transaction.
        ctx.send_host_all(host_messages);
        crate::handler::buffer::retire_eligible_buffers(ctx);
        Action::Drop
    }
}

impl WlRegionHandler for Handlers {}
impl WlSubcompositorHandler for Handlers {}
impl WlSubsurfaceHandler for Handlers {}

/// A guest `wp_viewport`; its table entry keeps it.
#[derive(Debug)]
pub(crate) struct Viewport {
    /// The `wl_surface` it scales, until that ends: then its requests
    /// raise `no_surface`, and a later surface under the ID is not its.
    surface: Option<GuestId>,
}

#[cfg(test)]
impl Viewport {
    /// The surface it scales, for the orphan check.
    pub(crate) fn surface_for_test(&self) -> Option<GuestId> {
        self.surface
    }
}

/// The surface of live viewport `viewport`, or the error a request of one
/// that has none ends the session with.
fn viewport_surface(ctx: &mut Context, viewport: GuestId) -> Result<GuestId, Action> {
    let surface = ctx.shadow_table.data::<Viewport>(viewport);
    surface
        .and_then(|viewport| viewport.surface)
        .ok_or_else(|| {
            ctx.reject(
                viewport,
                wp_viewport::ERROR_NO_SURFACE,
                "wp_viewport has no associated wl_surface",
            )
        })
}

impl wp_viewporter::WpViewporterHandler for Handlers {
    fn on_get_viewport(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        surface: RequestObject,
    ) -> Action {
        let surface = surface.guest;
        if ctx
            .shadow_table
            .data_of::<Viewport>()
            .any(|(_, viewport)| viewport.surface == Some(surface))
        {
            log::warn!("Rejecting duplicate wp_viewport for wl_surface {}", surface);
            return ctx.reject(
                sender.guest,
                wp_viewporter::ERROR_VIEWPORT_EXISTS,
                "wl_surface already has a wp_viewport",
            );
        }
        let Some(state) = ctx.surface_mut(surface) else {
            return ctx.missing_surface_state(surface);
        };
        state.pending.viewport = Some(Some(ViewportState::new()));
        let viewport = Viewport {
            surface: Some(surface),
        };
        ctx.defer_until_forwarded(PostDispatch::Attach {
            object: id,
            data: viewport.into(),
        });
        Action::Forward
    }
}

impl wp_viewport::WpViewportHandler for Handlers {
    fn on_set_source(
        ctx: &mut Context,
        sender: RequestSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = match viewport_surface(ctx, sender.guest) {
            Ok(surface) => surface,
            Err(action) => return action,
        };
        let unset = (x, y, width, height) == (-256, -256, -256, -256);
        if !unset && (x < 0 || y < 0 || width <= 0 || height <= 0) {
            log::warn!("Rejecting invalid wp_viewport source rectangle");
            return ctx.reject(
                sender.guest,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport source",
            );
        }
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending_viewport_mut().source = (!unset).then_some((x, y, width, height));
        Action::Forward
    }

    fn on_set_destination(
        ctx: &mut Context,
        sender: RequestSender,
        width: i32,
        height: i32,
    ) -> Action {
        let surface_id = match viewport_surface(ctx, sender.guest) {
            Ok(surface) => surface,
            Err(action) => return action,
        };
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            log::warn!(
                "Rejecting invalid wp_viewport destination {}x{}",
                width,
                height
            );
            return ctx.reject(
                sender.guest,
                wp_viewport::ERROR_BAD_VALUE,
                "invalid wp_viewport destination",
            );
        }
        let Some(surface) = ctx.surface_mut(surface_id) else {
            return ctx.missing_surface_state(surface_id);
        };
        surface.pending_viewport_mut().destination = (!unset).then_some((width, height));
        Action::Forward
    }
}

/// Forget guest `wp_viewport` `viewport`, which ended (see
/// `crate::state::lifecycle`): its surface's next commit removes the
/// viewport.
pub(crate) fn forget_viewport(ctx: &mut Context, viewport: Viewport) {
    if let Some(surface) = viewport.surface.and_then(|id| ctx.surfaces.get_mut(&id)) {
        surface.pending.viewport = Some(None);
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

impl crate::protocols::xdg_shell::xdg_wm_base::XdgWmBaseHandler for Handlers {
    fn on_get_xdg_surface(
        ctx: &mut Context,
        _sender: RequestSender,
        id: GuestId,
        surface: RequestObject,
    ) -> Action {
        ctx.xdg_roles.add_xdg_surface(id, surface.guest);
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
pub(crate) fn forget_xdg_surface(ctx: &mut Context, xdg_surface_id: GuestId) {
    if let Some(xdg_toplevel_id) = ctx.xdg_roles.remove_xdg_surface(xdg_surface_id) {
        window_placement::release_toplevel(ctx, xdg_toplevel_id);
    }
}

/// Forget guest `xdg_toplevel` `xdg_toplevel_id`, which ended (see
/// `crate::state::lifecycle`), and release its Aura toplevel.
pub(crate) fn forget_xdg_toplevel(ctx: &mut Context, xdg_toplevel_id: GuestId) {
    window_placement::release_toplevel(ctx, xdg_toplevel_id);
    ctx.xdg_roles.remove_toplevel(xdg_toplevel_id);
}

impl crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler for Handlers {
    fn on_configure(ctx: &mut Context, sender: EventSender, serial: u32) -> Action {
        let Some(guest_xdg_surface_id) = sender.guest_id() else {
            return Action::Drop;
        };
        window_placement::surface_configured(ctx, guest_xdg_surface_id, serial);
        Action::Forward
    }

    fn on_get_toplevel(ctx: &mut Context, sender: RequestSender, id: GuestId) -> Action {
        ctx.defer_until_forwarded(PostDispatch::XdgToplevelCreated {
            xdg_surface: sender.guest,
            xdg_toplevel: id,
        });
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler for Handlers {
    fn on_destroy(_ctx: &mut Context, _sender: RequestSender) -> Destructor {
        // Ending the toplevel releases its Aura toplevel (see
        // `forget_xdg_toplevel`), which goes before the role destructor (see
        // `aura_objects::release`). The guest ID is retained until the host
        // acknowledges the role destructor.
        Destructor::AfterEnd
    }

    fn on_set_app_id(ctx: &mut Context, sender: RequestSender, app_id: &String) -> Action {
        let xdg_toplevel_id = sender.guest;

        let Some(xdg_toplevel_host_id) = sender.host_id() else {
            log::warn!(
                "Cannot namespace app ID for unmapped xdg_toplevel {}",
                xdg_toplevel_id
            );
            return Action::Drop;
        };
        // The host XDG role and the Aura surface both get the ID in the Guest
        // OS namespace.
        let native_app_id = native_wayland_app_id(&ctx.settings().vm_id, app_id);

        // The host xdg_toplevel carries the app ID used by ordinary Exo
        // shelf/application matching. Keep that request namespaced as in the
        // reference Sommelier path rather than forwarding the guest's
        // unqualified ID directly. An ID too long for one message reaches
        // neither the role nor the Aura surface.
        if !ctx.send_host_or_drop(
            xdg_toplevel::req::set_app_id(xdg_toplevel_host_id, &native_app_id),
            format_args!("the application ID of xdg_toplevel {xdg_toplevel_id}"),
        ) {
            return Action::Drop;
        }

        // Resolve xdg_toplevel → wl_surface (guest) → wl_surface (host).
        if let Some(wl_surface_guest_id) = ctx.xdg_roles.wl_surface_of_toplevel(xdg_toplevel_id) {
            aura_objects::set_application_id(ctx, wl_surface_guest_id, &native_app_id);
        }
        Action::Drop
    }

    fn on_configure(
        _ctx: &mut Context,
        sender: EventSender,
        _width: i32,
        _height: i32,
        _states: &[u8],
    ) -> Action {
        if sender.guest.is_none() {
            return Action::Drop;
        }
        Action::Forward
    }
}

impl crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler for Handlers {
    fn on_configure(
        ctx: &mut Context,
        sender: EventSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        states: &[u8],
    ) -> Action {
        // The placement module delivers the translated event itself.
        window_placement::translate_aura_configure(ctx, sender.host, x, y, width, height, states);
        Action::Drop
    }

    fn on_origin_change(ctx: &mut Context, sender: EventSender, x: i32, y: i32) -> Action {
        // xdg_toplevel has no origin-change event: placement keeps the
        // origin, and the guest gets nothing.
        window_placement::origin_changed(ctx, sender.host, x, y);
        Action::Drop
    }
}

#[cfg(test)]
// Tests build and close raw descriptors to check who owns them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::handler::test_support::{toplevel_ctx, SURFACE};
    use crate::protocols::aura_shell::{zaura_shell, zaura_surface};
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
    use crate::protocols::wayland::wl_surface::REQ_COMMIT;
    use crate::protocols::wayland::{
        wl_buffer, wl_display, wl_keyboard, wl_output, wl_registry, wl_shm_pool,
    };
    use crate::protocols::Iface;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session, Toplevel};
    use crate::state::{PlaneFenceForTest, RenderBufferUse};
    use crate::test_support::text_input::{self, Host};
    use crate::test_support::{queued_headers, shm_pool_file, BufferKind, BUFFER_KINDS};
    use crate::wire::{Outgoing, WireId, WL_DISPLAY_ID};
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::FileExt;

    /// A host damage rectangle: x, y, width and height.
    type Rect = (i32, i32, i32, i32);

    /// A whole proxy session with GPU access, whose guest has a surface and
    /// makes buffers of either kind: SHM ones from its `wl_shm`, which the
    /// proxy copies to host storage, and native ones from the host's
    /// linux-dmabuf, which the guest gets with GPU access.
    struct Screen {
        s: Session,
        surface: Object,
        shm: u32,
        dmabuf: u32,
        viewporter: Option<u32>,
    }

    /// A guest SHM buffer, the guest's pool file it draws in, and the host
    /// pool file the proxy copies its pixels into, which the host shows.
    struct Shm {
        buffer: Object,
        width: usize,
        guest: std::fs::File,
        host: std::fs::File,
    }

    impl Shm {
        /// The client draws its next frame: every pool byte becomes
        /// `pixel`. The host copy is cleared, so [`Self::copied`] then
        /// shows exactly what the next commit copies.
        fn draw(&self, pixel: u8) {
            let size = self.guest.metadata().unwrap().len() as usize;
            self.guest.write_all_at(&vec![pixel; size], 0).unwrap();
            self.host.write_all_at(&vec![0; size], 0).unwrap();
        }

        /// The (x, y) pixels of the host copy that hold `pixel`.
        fn copied(&self, pixel: u8) -> Vec<(usize, usize)> {
            let size = self.host.metadata().unwrap().len() as usize;
            let mut host = vec![0; size];
            self.host.read_exact_at(&mut host, 0).unwrap();
            let stride = self.width * 4;
            (0..size / stride)
                .flat_map(|y| (0..self.width).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let start = y * stride + x * 4;
                    host[start..start + 4].iter().all(|&byte| byte == pixel)
                })
                .collect()
        }
    }

    impl Screen {
        async fn new() -> Self {
            let mut s = Session::with_context(Context::new_for_test(true, false, Vec::new()));
            let surface = s.surface().await;
            let shm = s.bind("wl_shm", wl_shm::VERSION).await;
            let dmabuf = s.bind_paired("zwp_linux_dmabuf_v1", 3).await.id;
            // What the guest learns of the host's formats.
            s.guest_received();
            s.host_received();
            Self {
                s,
                surface,
                shm,
                dmabuf,
                viewporter: None,
            }
        }

        /// A new 4x4 SHM buffer that `surface` shows, which copies it
        /// whole, so that later commits copy only what they must.
        async fn frame(&mut self, surface: Object) -> Shm {
            let frame = self.shm(FRAME_SIDE as i32, FRAME_SIDE as i32).await;
            let side = FRAME_SIDE as i32;
            assert_eq!(self.show(surface, frame.buffer).await, [(0, 0, side, side)]);
            frame
        }

        /// The guest sends `request` of `surface`, which the host gets at
        /// once.
        async fn forwarded(&mut self, surface: Object, request: Outgoing<u32>) {
            self.s.guest([request]).await;
            let host = self.s.host_received();
            assert!(
                matches!(&host[..], [(sender, _)] if *sender == surface.host),
                "the host did not get the request alone: {host:?}"
            );
        }

        /// The guest sets `scale` on `surface` and commits it on its own,
        /// as a HiDPI client does once, so that later frames keep it;
        /// returns the commit's damage.
        async fn commit_scale(&mut self, surface: Object, scale: i32) -> Vec<Rect> {
            let request = wl_surface::req::set_buffer_scale(surface.id, scale);
            self.forwarded(surface, request).await;
            self.commit(surface).await
        }

        /// A new `wp_viewport` of `surface`, from a `wp_viewporter` the
        /// guest binds on first use.
        async fn viewport(&mut self, surface: Object) -> Object {
            use crate::protocols::viewporter::wp_viewporter::req::get_viewport;
            let viewporter = match self.viewporter {
                Some(viewporter) => viewporter,
                None => {
                    let viewporter = self.s.bind("wp_viewporter", 1).await;
                    *self.viewporter.insert(viewporter)
                }
            };
            let id = self.s.new_id();
            let host = self
                .s
                .create(get_viewport(viewporter, id, surface.id))
                .await;
            Object { id, host }
        }

        /// Inject a fault no peer can cause: a failed dma-buf sync, which
        /// fails a copy into `buffer`'s host storage while `fails`.
        fn fail_copies(&mut self, buffer: Object, fails: bool) {
            let host = buffer.host_id();
            let resources = self.s.ctx().local_buffer_copy_resources(host);
            // Without a VirtWL channel the dma-buf sync, and so the copy,
            // fails.
            resources.expect("a local buffer").buffer.dmabuf_sync = fails;
        }

        /// A new `width`x`height` native buffer, whose plane is a pipe, which
        /// has no dma-buf fence: its first commit falls back to implicit
        /// synchronization.
        async fn native(&mut self, width: i32, height: i32) -> Object {
            let (plane, _) = crate::test_support::pipe();
            self.s
                .native_buffer(self.dmabuf, plane, width, height)
                .await
        }

        /// A new `width`x`height` ARGB8888 SHM buffer, alone in its pool,
        /// whose pixels are all 0.
        async fn shm(&mut self, width: i32, height: i32) -> Shm {
            let size = usize::try_from(4 * width * height).unwrap();
            let guest = shm_pool_file(&vec![0; size]);
            let pool = self.s.shm_pool(self.shm, guest.try_clone().unwrap()).await;
            let (buffer, host) = self.s.shm_buffer_with_host_pool(pool, width, height).await;
            Shm {
                buffer,
                width: usize::try_from(width).unwrap(),
                guest,
                host,
            }
        }

        /// A new 4x4 buffer of `kind`.
        async fn buffer(&mut self, kind: BufferKind) -> Object {
            match kind {
                BufferKind::Shm => self.shm(4, 4).await.buffer,
                BufferKind::Native => self.native(4, 4).await,
            }
        }

        /// The guest attaches `buffer`, or no buffer, to `surface`, which
        /// the host gets at once.
        async fn attach(&mut self, surface: Object, buffer: Option<Object>) {
            self.attach_at(surface, buffer, (0, 0)).await;
        }

        /// [`Self::attach`] at `offset`.
        async fn attach_at(&mut self, surface: Object, buffer: Option<Object>, offset: (i32, i32)) {
            let (x, y) = offset;
            let id = buffer.map(|buffer| buffer.id);
            self.s
                .guest([wl_surface::req::attach(surface.id, id, x, y)])
                .await;
            let host = buffer.map_or(0, |buffer| buffer.host);
            let attach = wl_surface::Request::Attach { buffer: host, x, y };
            assert_eq!(self.s.host_received(), [(surface.host, attach.into())]);
        }

        /// The guest sends `requests`, which reach no peer before a commit.
        async fn pending(&mut self, requests: impl IntoIterator<Item = Outgoing<u32>>) {
            self.s.guest(requests).await;
            assert_eq!(self.s.host_received(), []);
            assert_eq!(self.s.guest_received(), []);
        }

        /// The guest commits `surface`; returns the damage the host gets
        /// with the commit, which reaches the host.
        async fn commit(&mut self, surface: Object) -> Vec<Rect> {
            self.s.guest([wl_surface::req::commit(surface.id)]).await;
            committed(surface, self.s.host_received())
        }

        /// The guest attaches `buffer` to `surface` and commits it; returns
        /// the damage the host gets with the commit.
        async fn show(&mut self, surface: Object, buffer: Object) -> Vec<Rect> {
            self.attach(surface, Some(buffer)).await;
            self.commit(surface).await
        }

        /// The host releases `buffer`, which the guest gets.
        async fn release(&mut self, buffer: Object) {
            self.s.host([wl_buffer::evt::release(buffer.host)]).await;
            let release = wl_buffer::Event::Release {};
            assert_eq!(self.s.guest_received(), [(buffer.id, release.into())]);
        }

        /// The host releases `buffer`, which the guest has destroyed, so it
        /// gets nothing; returns what the proxy sends the host for it.
        async fn release_destroyed(&mut self, buffer: Object) -> Vec<(u32, AnyRequest)> {
            self.s.host([wl_buffer::evt::release(buffer.host)]).await;
            assert_eq!(self.s.guest_received(), []);
            self.s.host_received()
        }

        /// The guest destroys `buffer`; returns what the proxy sends the
        /// host for it.
        async fn destroy(&mut self, buffer: Object) -> Vec<(u32, AnyRequest)> {
            self.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
            assert_eq!(self.s.guest_received(), []);
            self.s.host_received()
        }

        /// The guest destroys `surface`; returns what the proxy sends the
        /// host for it.
        async fn destroy_surface(&mut self, surface: Object) -> Vec<(u32, AnyRequest)> {
            self.s.guest([wl_surface::req::destroy(surface.id)]).await;
            assert_eq!(self.s.guest_received(), []);
            self.s.host_received()
        }
    }

    /// The rectangles of the host `wl_surface.damage` requests of `host`,
    /// which must be all `surface` gets but the commit after them.
    fn committed(surface: Object, host: Vec<(u32, AnyRequest)>) -> Vec<Rect> {
        let Some((commit, damage)) = host.split_last() else {
            panic!("the host got no commit");
        };
        assert_eq!(
            *commit,
            (surface.host, wl_surface::Request::Commit {}.into())
        );
        damage
            .iter()
            .map(|request| match request {
                (
                    sender,
                    AnyRequest::WlSurface(wl_surface::Request::Damage {
                        x,
                        y,
                        width,
                        height,
                    }),
                ) if *sender == surface.host => (*x, *y, *width, *height),
                _ => panic!("the host got more than damage before the commit: {host:?}"),
            })
            .collect()
    }

    /// The host `wl_buffer.destroy` of `buffer`.
    fn buffer_destroy(buffer: Object) -> (u32, AnyRequest) {
        (buffer.host, wl_buffer::Request::Destroy {}.into())
    }

    /// The host `wl_surface.destroy` of `surface`.
    fn surface_destroy(surface: Object) -> (u32, AnyRequest) {
        (surface.host, wl_surface::Request::Destroy {}.into())
    }

    /// A `wl_output` the guest binds after the host removed its global, as
    /// when the bind and the removal cross: an inert output, which has no
    /// host object.
    async fn inert_output(s: &mut Session) -> u32 {
        let registry = s.registry().await;
        let name = s.advertise("wl_output", 3).await;
        s.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        let output = s.new_id();
        s.guest([wl_registry::req::bind(registry.id, name, "wl_output", 3, output).unwrap()])
            .await;
        let removed = wl_registry::Event::GlobalRemove { name };
        assert_eq!(s.guest_received()[1..], [(registry.id, removed.into())]);
        assert_eq!(s.host_received(), []);
        output
    }

    /// A request naming an inert output (whose bind crossed its global's
    /// removal) reaches the host with no output, as if the guest named none.
    #[tokio::test]
    async fn set_fullscreen_on_an_inert_output_is_forwarded_without_an_output() {
        let mut s = Session::new();
        let toplevel = s.toplevel().await.toplevel;
        let output = inert_output(&mut s).await;
        s.guest([xdg_toplevel::req::set_fullscreen(toplevel.id, Some(output))])
            .await;
        let fullscreen = xdg_toplevel::Request::SetFullscreen { output: 0 };
        assert_eq!(s.host_received(), [(toplevel.host, fullscreen.into())]);
    }

    /// The host's `enter` and `leave` of `surface` and `output`, which the
    /// guest gets.
    async fn enter_or_leave(s: &mut Session, surface: Object, enter: bool, output: Object) {
        let (event, forwarded) = if enter {
            let event = wl_surface::evt::enter(surface.host, output.host);
            (event, wl_surface::Event::Enter { output: output.id })
        } else {
            let event = wl_surface::evt::leave(surface.host, output.host);
            (event, wl_surface::Event::Leave { output: output.id })
        };
        s.host([event]).await;
        assert_eq!(s.guest_received(), [(surface.id, forwarded.into())]);
    }

    #[tokio::test]
    async fn surface_enter_leave_tracks_the_last_entered_output() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let first = s.bind_paired("wl_output", 3).await;
        let second = s.bind_paired("wl_output", 3).await;
        let event = async |s: &mut Session, enter: bool, output: Object| {
            enter_or_leave(s, surface, enter, output).await;
            // Reads state no peer observes here: the output a placement
            // chord places the window on, which window_placement's tests
            // drive.
            s.ctx().outputs.entered_for_test(surface.host_id())
        };
        assert_eq!(event(&mut s, true, first).await, Some(first.host_id()));
        // Exo moves a surface with leave(old) before enter(new).
        assert_eq!(event(&mut s, false, first).await, None);
        assert_eq!(event(&mut s, true, second).await, Some(second.host_id()));
        // A leave of another output, in either order, changes nothing.
        assert_eq!(event(&mut s, true, first).await, Some(first.host_id()));
        assert_eq!(event(&mut s, false, second).await, Some(first.host_id()));
    }

    /// The enter and leave handlers rely on generated validation to drop an
    /// event that names a released output, which stays mapped until the
    /// host's `delete_id`: had one reached them, they would forward it.
    #[tokio::test]
    async fn surface_events_naming_a_released_output_never_reach_the_handlers() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let output = s.bind_paired("wl_output", 3).await;
        s.guest([wl_output::req::release(output.id)]).await;
        let release = wl_output::Request::Release {};
        assert_eq!(s.host_received(), [(output.host, release.into())]);

        s.host([
            wl_surface::evt::enter(surface.host, output.host),
            wl_surface::evt::leave(surface.host, output.host),
        ])
        .await;
        assert_eq!(s.guest_received(), []);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, output.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: output.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
    }

    #[tokio::test]
    async fn surface_enter_of_a_removed_output_is_forwarded_but_not_recorded() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let registry = s.registry().await;
        let name = s.advertise("wl_output", 3).await;
        let id = s.new_id();
        let bind = wl_registry::req::bind(registry.id, name, "wl_output", 3, id).unwrap();
        let output = Object {
            id,
            host: s.create(bind).await,
        };
        s.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        let removed = wl_registry::Event::GlobalRemove { name };
        assert_eq!(s.guest_received()[1..], [(registry.id, removed.into())]);

        enter_or_leave(&mut s, surface, true, output).await;
        // Reads state no peer observes here, as in
        // surface_enter_leave_tracks_the_last_entered_output.
        assert_eq!(s.ctx().outputs.entered_for_test(surface.host_id()), None);
    }

    /// An enabled text input of `seat` focused on `surface`, whose host is
    /// active, composing over committed surrounding text with state
    /// pending. One text input on a connection is enabled at a time.
    fn composing(host: HostId, seat: GuestId, surface: GuestId) -> text_input::Active<'static> {
        text_input::Active {
            surrounding_text: Some(("한", 3, 3)),
            preedit: Some("한"),
            pending: true,
            ..text_input::Active::new(host, seat, Some(surface))
        }
    }

    /// A text input of `seat` focused on `surface` that the guest has not
    /// enabled, with state pending.
    fn disabled(host: HostId, seat: GuestId, surface: GuestId) -> text_input::Active<'static> {
        text_input::Active {
            enabled: false,
            pending: true,
            ..text_input::Active::new(host, seat, Some(surface))
        }
    }

    fn invalid_ioctl_fd() -> OwnedFd {
        let path = std::ffi::CString::new("/proc/self/exe").unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        assert!(fd >= 0);
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    #[tokio::test]
    async fn unsupported_native_sync_uses_cached_implicit_fallback_and_commits() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        // Its plane is a pipe, which has no dma-buf fence.
        let buffer = sc.native(1, 1).await;
        assert_eq!(sc.show(surface, buffer).await, []);
        // Reads state no peer can observe: the fallback is cached, so the
        // later commits do not probe the plane again.
        assert!(sc
            .s
            .ctx()
            .native_buffer_uses_implicit_sync(buffer.guest_id()));
        assert_eq!(
            sc.show(surface, buffer).await,
            [],
            "the cached compatibility mode must keep later commits live"
        );
    }

    #[tokio::test]
    async fn unexpected_native_sync_failure_is_fatal_and_never_commits() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let sync_fd = invalid_ioctl_fd();
        let error = crate::allocator::dmabuf_fence(sync_fd.as_fd(), None).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        let buffer = sc.s.native_buffer(sc.dmabuf, sync_fd, 1, 1).await;

        sc.attach(surface, Some(buffer)).await;
        sc.s.guest_fatal([wl_surface::req::commit(surface.id)])
            .await;
        assert_eq!(
            sc.s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(
            sc.s.host_received(),
            [],
            "a commit whose synchronization failed must not reach the host"
        );
    }

    /// A commit of a buffer the guest GPU still writes waits before it
    /// changes anything, and goes through once the fence signals: with the
    /// damage it was sent with, which a first dispatch that had taken the
    /// pending state would have lost.
    #[tokio::test]
    async fn a_commit_waits_for_a_pending_fence_before_it_changes_anything() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        // Injects what no peer can provide: the plane stands in for its own
        // sync_file, which a readable pipe is once the guest GPU is done.
        sc.s.ctx().plane_fences_for_test = Some(PlaneFenceForTest::SyncFile);
        let (fence, signal) = crate::test_support::pipe();
        let buffer = sc.s.native_buffer(sc.dmabuf, fence, 2, 2).await;
        sc.attach(surface, Some(buffer)).await;
        sc.s.guest([
            wl_surface::req::damage_buffer(surface.id, 1, 1, 1, 1),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        assert!(sc.s.parked(), "the commit waits for its fence");
        assert_eq!(sc.s.host_received(), []);
        assert_eq!(sc.s.guest_received(), []);

        assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
        sc.s.resume().await;
        assert!(!sc.s.parked());
        assert_eq!(committed(surface, sc.s.host_received()), [(0, 0, 3, 3)]);
    }

    /// A live surface without its state is an internal divergence that ends
    /// the session, not a state the proxy recreates.
    #[tokio::test]
    async fn a_live_surface_without_state_ends_the_session() {
        use crate::protocols::viewporter::wp_viewporter::req::get_viewport;
        type Request = fn(surface: u32, viewporter: u32, viewport: u32, new: u32) -> Outgoing<u32>;
        let requests: [(&str, Request); 9] = [
            ("attach", |surface, _, _, _| {
                wl_surface::req::attach(surface, None, 0, 0)
            }),
            ("damage", |surface, _, _, _| {
                wl_surface::req::damage(surface, 0, 0, 1, 1)
            }),
            ("damage_buffer", |surface, _, _, _| {
                wl_surface::req::damage_buffer(surface, 0, 0, 1, 1)
            }),
            ("set_buffer_scale", |surface, _, _, _| {
                wl_surface::req::set_buffer_scale(surface, 1)
            }),
            ("commit", |surface, _, _, _| {
                wl_surface::req::commit(surface)
            }),
            ("set_buffer_transform", |surface, _, _, _| {
                wl_surface::req::set_buffer_transform(surface, 0)
            }),
            ("get_viewport", |surface, viewporter, _, new| {
                get_viewport(viewporter, new, surface)
            }),
            ("set_source", |_, _, viewport, _| {
                wp_viewport::req::set_source(viewport, 0, 0, 256, 256)
            }),
            ("set_destination", |_, _, viewport, _| {
                wp_viewport::req::set_destination(viewport, 1, 1)
            }),
        ];
        for (name, request) in requests {
            let mut s = Session::new();
            let surface = s.surface().await;
            let other = s.surface().await;
            let viewporter = s.bind_paired("wp_viewporter", 1).await.id;
            // The viewport of the other surface, which the requests that
            // need one name.
            let viewport = s.new_id();
            s.create(get_viewport(viewporter, viewport, other.id)).await;
            // Injects a fault no peer can cause: both live surfaces lose
            // their state.
            s.ctx().surfaces.clear();
            let new = s.new_id();
            s.guest_fatal([request(surface.id, viewporter, viewport, new)])
                .await;
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
                "{name}"
            );
            assert_eq!(s.host_received(), [], "{name}");
            assert!(s.ctx().surfaces.is_empty(), "{name} recreated the state");
        }
    }

    /// A `get_viewport` for a surface without state records no viewport:
    /// the error path changes nothing.
    #[tokio::test]
    async fn get_viewport_without_surface_state_records_no_viewport() {
        use crate::protocols::viewporter::wp_viewporter::req::get_viewport;
        let mut s = Session::new();
        let surface = s.surface().await;
        let viewporter = s.bind_paired("wp_viewporter", 1).await.id;
        // Injects a fault no peer can cause: a live surface without state.
        s.ctx().surfaces.clear();
        let viewport = s.new_id();
        s.guest_fatal([get_viewport(viewporter, viewport, surface.id)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(s.host_received(), []);
        // Reads state no peer can observe once the session has ended.
        assert!(s.ctx().shadow_table.data_of::<Viewport>().next().is_none());
    }

    /// The side of the square ARGB8888 frames of the copy tests.
    const FRAME_SIDE: usize = 4;

    /// The (x, y) pixels of a rectangle, in [`Shm::copied`] order.
    fn rect_pixels(x: usize, y: usize, width: usize, height: usize) -> Vec<(usize, usize)> {
        (y..y + height)
            .flat_map(|y| (x..x + width).map(move |x| (x, y)))
            .collect()
    }

    /// Every pixel of a frame.
    fn whole_frame() -> Vec<(usize, usize)> {
        rect_pixels(0, 0, FRAME_SIDE, FRAME_SIDE)
    }

    #[tokio::test]
    async fn buffer_damage_under_a_buffer_scale_copies_only_its_pixels() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.commit_scale(surface, 2).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 1, 1, 1, 2)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), rect_pixels(1, 1, 1, 2));
    }

    #[tokio::test]
    async fn surface_damage_under_an_integer_buffer_scale_copies_its_scaled_pixels() {
        // A HiDPI client: buffer scale 2, transform 0, no viewport. Its
        // surface damage maps exactly to buffer pixels, so a small update
        // must not copy the whole buffer every frame.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.commit_scale(surface, 2).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage(surface.id, 1, 0, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), rect_pixels(2, 0, 2, 2));
    }

    #[tokio::test]
    async fn surface_damage_under_a_buffer_transform_copies_the_whole_buffer() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        a.draw(0x22);
        // WL_OUTPUT_TRANSFORM_90: the bridge does not rotate surface damage.
        let transform = wl_surface::req::set_buffer_transform(surface.id, 1);
        sc.forwarded(surface, transform).await;
        sc.pending([wl_surface::req::damage(surface.id, 1, 1, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), whole_frame());
    }

    #[tokio::test]
    async fn destination_only_viewport_copies_only_the_damaged_pixels() {
        // A fractional-scale client: the 4x4 buffer fills an 8x8 surface,
        // so a surface unit is half a buffer pixel. It damages surface
        // units, as a client that does not use damage_buffer does.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        let viewport = sc.viewport(surface).await;
        let destination = wp_viewport::req::set_destination(viewport.id, 8, 8);
        sc.forwarded(viewport, destination).await;
        sc.commit(surface).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage(surface.id, 0, 0, 2, 2)])
            .await;
        sc.show(surface, a.buffer).await;
        // Surface units 0..2 are pixels 0..1, grown by a pixel.
        assert_eq!(a.copied(0x22), rect_pixels(0, 0, 2, 2));
    }

    #[tokio::test]
    async fn commit_without_damage_copies_nothing() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        a.draw(0x22);
        assert_eq!(sc.show(surface, a.buffer).await, []);
        assert_eq!(a.copied(0x22), []);
    }

    #[tokio::test]
    async fn damage_beyond_the_buffer_is_clipped_to_it() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        a.draw(0x22);
        let damage = wl_surface::req::damage_buffer(surface.id, 0, 0, i32::MAX, i32::MAX);
        sc.pending([damage]).await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), whole_frame());
    }

    #[tokio::test]
    async fn reattached_buffer_copies_the_damage_of_the_frames_it_missed() {
        // Damage is relative to the surface's current contents. A client
        // alternating A, B, A damages only what changed since B, so A's host
        // copy must also receive what B's frame changed.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        let b = sc.frame(surface).await;

        b.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 2, 1)])
            .await;
        sc.show(surface, b.buffer).await;
        assert_eq!(b.copied(0x22), rect_pixels(0, 0, 2, 1));

        // A's next frame holds B's change too, but damages only its own.
        a.draw(0x33);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 2, 3, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        let mut expected = rect_pixels(0, 0, 2, 1);
        expected.extend(rect_pixels(2, 3, 1, 1));
        assert_eq!(a.copied(0x33), expected);
    }

    #[tokio::test]
    async fn reattached_buffer_copies_the_damage_of_every_frame_it_missed() {
        // A, B, C, A: A must also receive what both B's and C's frames changed.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        let b = sc.frame(surface).await;
        let c = sc.frame(surface).await;
        // B changes pixel (0, 0), then C changes pixel (1, 0).
        for (frame, x) in [(&b, 0), (&c, 1)] {
            sc.pending([wl_surface::req::damage_buffer(surface.id, x, 0, 1, 1)])
                .await;
            sc.show(surface, frame.buffer).await;
        }
        a.draw(0x44);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 3, 3, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x44), [(0, 0), (1, 0), (3, 3)]);
    }

    #[tokio::test]
    async fn damage_skipped_on_a_released_buffer_is_copied_at_its_next_attach() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.release(a.buffer).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 1, 2, 2, 1)])
            .await;
        sc.commit(surface).await;
        assert_eq!(a.copied(0x22), []);

        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), rect_pixels(1, 2, 2, 1));
    }

    #[tokio::test]
    async fn a_buffer_moving_between_surfaces_copies_everything() {
        // Its host copy relates to the contents of one surface only.
        let mut sc = Screen::new().await;
        let first = sc.surface;
        let other = sc.s.surface().await;
        let a = sc.frame(first).await;
        for surface in [other, first] {
            a.draw(0x22);
            sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
                .await;
            sc.show(surface, a.buffer).await;
            assert_eq!(a.copied(0x22), whole_frame());
        }
    }

    #[tokio::test]
    async fn a_failed_copy_stales_the_whole_host_copy() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.fail_copies(a.buffer, true);
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.attach(surface, Some(a.buffer)).await;
        sc.s.guest([wl_surface::req::commit(surface.id)]).await;
        assert_eq!(sc.s.host_received(), [], "the failed commit is dropped");
        assert_eq!(a.copied(0x22), []);

        sc.fail_copies(a.buffer, false);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        // The whole frame, which covers the damage, grown by a pixel.
        let side = FRAME_SIDE as i32;
        let whole = (-1, -1, side + 1, side + 1);
        assert_eq!(sc.show(surface, a.buffer).await, [whole]);
        assert_eq!(a.copied(0x22), whole_frame());
    }

    #[tokio::test]
    async fn a_mapping_change_stales_the_other_host_copies() {
        // B's frame is rotated 180 degrees: its pixel damage does not mark
        // the same pixels of A, which was shown unrotated.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        let b = sc.frame(surface).await;
        let transform = wl_surface::req::set_buffer_transform(surface.id, 2);
        sc.forwarded(surface, transform).await;
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.show(surface, b.buffer).await;

        a.draw(0x33);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x33), whole_frame());
    }

    #[tokio::test]
    async fn a_buffer_attached_with_a_new_mapping_copies_everything() {
        // A's host copy holds its old frame, unrotated. The client rotates A
        // by 180 degrees and damages one pixel: the rest of A's host copy no
        // longer matches what the surface shows.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.frame(surface).await;
        a.draw(0x33);
        let transform = wl_surface::req::set_buffer_transform(surface.id, 2);
        sc.forwarded(surface, transform).await;
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x33), whole_frame());
    }

    #[tokio::test]
    async fn a_frame_of_another_size_stales_the_whole_host_copy() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        let small = sc.shm(2, 2).await;
        sc.show(surface, small.buffer).await;

        a.draw(0x33);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x33), whole_frame());
    }

    /// The guest destroys its surface, which the host acknowledges, and
    /// creates a new one under the same ID: nothing relates the frame the
    /// old one showed to the new one's contents.
    #[tokio::test]
    async fn destroying_a_surface_stales_the_host_copies_it_showed() {
        use crate::protocols::wayland::wl_compositor;
        let mut sc = Screen::new().await;
        let old = sc.surface;
        let a = sc.frame(old).await;
        assert_eq!(sc.destroy_surface(old).await, [surface_destroy(old)]);
        sc.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, old.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: old.id };
        assert_eq!(sc.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        let compositor = sc.s.bind("wl_compositor", wl_compositor::VERSION).await;
        let create = wl_compositor::req::create_surface(compositor, old.id);
        let surface = Object {
            id: old.id,
            host: sc.s.create(create).await,
        };

        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.show(surface, a.buffer).await;
        assert_eq!(a.copied(0x22), whole_frame());
    }

    #[tokio::test]
    async fn damage_only_commit_copies_the_mutated_current_shm_buffer() {
        // The SHM Commit Copy Invariant: the host samples the current buffer
        // again for a commit without an attach, so its damage must be copied
        // before the commit is forwarded.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 1, 2, 2, 1)])
            .await;
        sc.commit(surface).await;
        assert_eq!(a.copied(0x22), rect_pixels(1, 2, 2, 1));
    }

    #[tokio::test]
    async fn damage_only_commit_does_not_read_a_released_shm_buffer() {
        // After wl_buffer.release the client owns the storage and may be
        // drawing its next frame there; the host no longer samples it.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.release(a.buffer).await;
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 1, 2, 2, 1)])
            .await;
        sc.commit(surface).await;
        assert_eq!(a.copied(0x22), []);
    }

    #[tokio::test]
    async fn commit_without_damage_on_a_held_buffer_skips_the_copy() {
        // A commit that only acknowledges a configure must reach the host
        // even when the buffer's dma-buf sync would fail (here: no channel).
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.frame(surface).await;
        sc.fail_copies(a.buffer, true);

        // No damage at all, then damage that clips to nothing.
        assert_eq!(sc.commit(surface).await, []);
        let side = FRAME_SIDE as i32;
        sc.pending([wl_surface::req::damage_buffer(surface.id, side, 0, 1, 1)])
            .await;
        assert_eq!(sc.commit(surface).await, [(side - 1, -1, 3, 3)]);
    }

    #[tokio::test]
    async fn destroyed_pending_shm_attach_survives_until_commit() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.shm(1, 1).await;
        // This buffer has completed a previous compositor-use interval. A new
        // attach must reopen that interval even when the guest destroys the
        // object before commit.
        sc.show(surface, a.buffer).await;
        sc.release(a.buffer).await;
        sc.attach(surface, Some(a.buffer)).await;

        // Wayland permits destroying the guest object after attach and before
        // commit. The host attach is already sent, so the local backing and
        // pending surface reference must survive until commit.
        assert_eq!(sc.destroy(a.buffer).await, []);
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        sc.commit(surface).await;
        assert_eq!(
            a.copied(0x22),
            [(0, 0)],
            "destroying the guest buffer before commit must not lose its frame"
        );
        // The commit began a use that only the host's release ends.
        assert_eq!(
            sc.release_destroyed(a.buffer).await,
            [buffer_destroy(a.buffer)]
        );
    }

    #[tokio::test]
    async fn first_shm_commit_without_explicit_damage_queues_full_host_damage() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.shm(8, 6).await;
        assert_eq!(sc.show(surface, a.buffer).await, [(0, 0, 8, 6)]);
    }

    #[tokio::test]
    async fn failed_shm_copy_does_not_queue_a_host_commit() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.shm(8, 6).await;
        sc.fail_copies(a.buffer, true);
        sc.attach(surface, Some(a.buffer)).await;
        sc.s.guest([wl_surface::req::commit(surface.id)]).await;
        assert_eq!(
            sc.s.host_received(),
            [],
            "stale or uninitialised host pixels must never be committed"
        );

        // A failed copy must force a complete retry on the next commit.
        sc.fail_copies(a.buffer, false);
        a.draw(0x22);
        sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
            .await;
        // The whole buffer, which covers the damage, grown by a pixel.
        assert_eq!(sc.commit(surface).await, [(-1, -1, 9, 7)]);
        assert_eq!(a.copied(0x22), rect_pixels(0, 0, 8, 6));
    }

    /// Only a paired or local object can send a request or be named in one:
    /// an ID that names neither is rejected before any handler runs.
    #[tokio::test]
    async fn an_id_that_names_no_object_is_rejected() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let unknown = s.new_id();
        s.guest_fatal([wl_surface::req::commit(unknown)]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_OBJECT))
        );
        assert_eq!(s.host_received(), []);

        // libwayland-server reports an argument naming no object as an
        // invalid method.
        let mut s = Session::new();
        let surface_too = s.surface().await;
        assert_eq!(surface_too.id, surface.id);
        s.guest_fatal([wl_surface::req::attach(surface.id, Some(unknown), 0, 0)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
        assert_eq!(s.host_received(), []);
    }

    #[tokio::test]
    async fn rejects_invalid_scale_and_transform_without_mutating_pending_state() {
        // wl_surface.invalid_scale for 0 and negative scales, and
        // invalid_transform outside wl_output.transform's 0..=7. A client
        // gets one error, so each rejection starts afresh; the rejected
        // request reaches no host.
        type Request = fn(surface: u32) -> Outgoing<u32>;
        let requests: [(Request, u32); 3] = [
            (
                |surface| wl_surface::req::set_buffer_scale(surface, 0),
                wl_surface::ERROR_INVALID_SCALE,
            ),
            (
                |surface| wl_surface::req::set_buffer_scale(surface, -2),
                wl_surface::ERROR_INVALID_SCALE,
            ),
            (
                |surface| wl_surface::req::set_buffer_transform(surface, 8),
                wl_surface::ERROR_INVALID_TRANSFORM,
            ),
        ];
        for (request, code) in requests {
            let mut s = Session::new();
            let surface = s.surface().await;
            s.guest_fatal([request(surface.id)]).await;
            assert_eq!(s.guest_received_and_error(), (vec![], (surface.id, code)));
            assert_eq!(s.host_received(), []);
        }
    }

    /// An attach's offset applies to its commit only.
    #[tokio::test]
    async fn consumed_attach_offset_is_not_reapplied_on_later_commit() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(8, 6).await;
        sc.show(surface, buffer).await;
        let damage = || wl_surface::req::damage_buffer(surface.id, 1, 1, 1, 1);

        // A non-zero offset with damage damages the whole surface.
        sc.attach_at(surface, Some(buffer), (3, 4)).await;
        sc.pending([damage()]).await;
        assert_eq!(sc.commit(surface).await, [(0, 0, 8, 6)]);
        sc.pending([damage()]).await;
        assert_eq!(
            sc.commit(surface).await,
            [(0, 0, 3, 3)],
            "the attach offset must be consumed"
        );
    }

    #[tokio::test]
    async fn damage_buffer_is_translated_to_host_damage_before_commit() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        // damage_buffer must not be forwarded with its guest opcode.
        sc.pending([wl_surface::req::damage_buffer(surface.id, 4, 5, 6, 7)])
            .await;
        assert_eq!(sc.commit(surface).await, [(3, 4, 8, 9)]);
    }

    /// An offset without explicit damage invents none. Only an attach sets
    /// an offset in a v4 surface, and a commit of an attach with an offset
    /// damages the whole surface for the attach, so no peer can see this
    /// decision alone; it stays a unit test, for the day v5's
    /// `wl_surface.offset` sets an offset without an attach.
    #[test]
    fn offset_without_explicit_damage_does_not_invent_host_damage() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        crate::handler::test_support::add_surface(&mut ctx);
        ctx.surface_for_test(SURFACE.guest_id())
            .pending
            .attach_offset = Some((3, 0));
        let sender = ctx
            .shadow_table
            .resolve_request(SURFACE.guest)
            .expect("the surface");

        assert_eq!(Handlers::on_commit(&mut ctx, sender), Action::Drop);
        assert_eq!(
            queued_headers(ctx.host_queue_for_test()),
            [(SURFACE.host, REQ_COMMIT)],
            "offset-only state must not manufacture a damage request"
        );
    }

    #[tokio::test]
    async fn nonzero_surface_offset_with_explicit_damage_forces_full_host_damage() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(8, 6).await;
        sc.show(surface, buffer).await;
        sc.attach_at(surface, Some(buffer), (3, 0)).await;
        sc.pending([wl_surface::req::damage_buffer(surface.id, 1, 1, 1, 1)])
            .await;
        assert_eq!(
            sc.commit(surface).await,
            [(0, 0, 8, 6)],
            "offset plus explicit damage must include complete host damage"
        );
    }

    /// A commit of a surface whose host object the host destroyed on its
    /// own reaches nothing: the guest's surface ended with it.
    #[tokio::test]
    async fn missing_host_surface_ends_the_session_without_a_commit() {
        let mut s = Session::new();
        let surface = s.surface().await;
        s.host_may_end(|_, interface| interface == "wl_surface");
        s.guest([wl_surface::req::damage(surface.id, 1, 2, 3, 4)])
            .await;
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: surface.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        s.guest_fatal([wl_surface::req::commit(surface.id)]).await;
        let (events, (_, code)) = s.guest_received_and_error();
        assert_eq!((events, code), (vec![], wl_display::ERROR_INVALID_OBJECT));
        assert_eq!(s.host_received(), []);
    }

    #[tokio::test]
    async fn invalid_damage_spam_does_not_escalate_to_full_frame_damage() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let spam = (0..300).flat_map(|_| {
            [
                wl_surface::req::damage(surface.id, 0, 0, 0, 10),
                wl_surface::req::damage_buffer(surface.id, 0, 0, 10, -1),
            ]
        });
        sc.pending(spam).await;
        assert_eq!(sc.commit(surface).await, []);
    }

    #[tokio::test]
    async fn bounded_damage_overflow_uses_the_exact_content_extent() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(8, 6).await;
        sc.attach(surface, Some(buffer)).await;
        let damage =
            (0..=256).map(|index| wl_surface::req::damage_buffer(surface.id, index, index, 1, 1));
        sc.pending(damage).await;
        assert_eq!(sc.commit(surface).await, [(0, 0, 8, 6)]);
    }

    #[tokio::test]
    async fn surface_damage_is_outset_before_host_forwarding() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.pending([wl_surface::req::damage(surface.id, 10, 20, 1, 1)])
            .await;
        assert_eq!(sc.commit(surface).await, [(9, 19, 3, 3)]);
    }

    impl Screen {
        /// The guest has `surface` show its buffer at `scale` through a new
        /// viewport with `source` (in 24.8 fixed point) and `destination`,
        /// which the host gets at once; returns the viewport.
        async fn view(
            &mut self,
            surface: Object,
            scale: i32,
            source: Option<Rect>,
            destination: Option<(i32, i32)>,
        ) -> Object {
            if scale != 1 {
                let request = wl_surface::req::set_buffer_scale(surface.id, scale);
                self.forwarded(surface, request).await;
            }
            let viewport = self.viewport(surface).await;
            if let Some((x, y, width, height)) = source {
                let request = wp_viewport::req::set_source(viewport.id, x, y, width, height);
                self.forwarded(viewport, request).await;
            }
            if let Some((width, height)) = destination {
                let request = wp_viewport::req::set_destination(viewport.id, width, height);
                self.forwarded(viewport, request).await;
            }
            viewport
        }
    }

    /// The host damage of a commit with buffer damage `rect` on a surface
    /// that shows a native buffer of `size` at `scale` through a viewport
    /// with `source` (24.8 fixed point) and `destination`.
    async fn host_damage_for(
        scale: i32,
        source: Option<Rect>,
        destination: Option<(i32, i32)>,
        size: (i32, i32),
        rect: Rect,
    ) -> Vec<Rect> {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.view(surface, scale, source, destination).await;
        let buffer = sc.native(size.0, size.1).await;
        sc.show(surface, buffer).await;
        let (x, y, width, height) = rect;
        sc.pending([wl_surface::req::damage_buffer(
            surface.id, x, y, width, height,
        )])
        .await;
        sc.commit(surface).await
    }

    /// Every host damage rectangle, partial or full, keeps both edges within
    /// the headroom, however far a viewport stretches the buffer.
    #[tokio::test]
    async fn host_damage_is_clamped_on_both_edges() {
        const MAX: i32 = i32::MAX / 10;
        // A 1x1 buffer stretched to 1000x1000 puts damage at buffer pixel
        // 1000000 past the headroom: it shrinks to the headroom's last unit.
        assert_eq!(
            host_damage_for(1, None, Some((1000, 1000)), (1, 1), (1_000_000, 0, 1, 1)).await,
            [(MAX - 1, -1000, 1, 3000)]
        );
        // Stretched to i32::MAX units, the left edge of the last pixel would
        // leave no room for its width.
        assert_eq!(
            host_damage_for(
                1,
                Some((0, 0, 256, 256)),
                Some((i32::MAX, 256)),
                (1, 1),
                (i32::MAX, 0, 1, 1)
            )
            .await,
            [(MAX - 1, -256, 1, 768)]
        );
        // A buffer attached under a viewport without damage damages the
        // whole destination.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.view(surface, 1, None, Some((i32::MAX, i32::MAX))).await;
        let buffer = sc.native(1, 1).await;
        assert_eq!(sc.show(surface, buffer).await, [(0, 0, MAX, MAX)]);
    }

    #[tokio::test]
    async fn full_mapping_without_explicit_damage_still_damages_the_host_surface() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        // A native buffer, which has no host copy to make the damage full.
        let buffer = sc.native(8, 6).await;
        let transform = wl_surface::req::set_buffer_transform(surface.id, 1);
        sc.forwarded(surface, transform).await;
        assert_eq!(sc.show(surface, buffer).await, [(0, 0, 6, 8)]);
    }

    #[tokio::test]
    async fn extreme_surface_damage_is_clamped_before_host_encoding() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.pending([wl_surface::req::damage(
            surface.id,
            i32::MIN,
            0,
            i32::MAX,
            1,
        )])
        .await;
        assert_eq!(
            sc.commit(surface).await,
            [(i32::MIN / 10, -1, 214_748_364, 3)]
        );
    }

    #[tokio::test]
    async fn viewport_damage_uses_committed_crop_and_destination() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let source = (65_536, 131_072, 262_144, 262_144);
        sc.view(surface, 1, Some(source), Some((200, 100))).await;

        // The viewport becomes current at its own commit, as a client sets
        // it once; a later frame's buffer damage is converted from buffer
        // pixels into destination coordinates.
        sc.commit(surface).await;
        let buffer = sc.native(2048, 2048).await;
        sc.attach(surface, Some(buffer)).await;
        sc.pending([wl_surface::req::damage_buffer(
            surface.id, 256, 512, 256, 256,
        )])
        .await;
        // The damage, grown by a buffer pixel, starts just left of and above
        // the source, so its mapped edges round outward to -1.
        assert_eq!(sc.commit(surface).await, [(-1, -1, 52, 27)]);
    }

    #[tokio::test]
    async fn buffer_damage_maps_through_buffer_scale_before_the_viewport() {
        // Scale 2 makes the 100-pixel buffer a 50-unit surface, which the
        // destination stretches back to 100 units: a pixel is one unit.
        assert_eq!(
            host_damage_for(2, None, Some((100, 100)), (100, 100), (80, 80, 20, 20)).await,
            [(79, 79, 22, 22)]
        );
    }

    #[tokio::test]
    async fn whole_buffer_damage_covers_the_whole_destination() {
        assert_eq!(
            host_damage_for(2, None, Some((100, 100)), (100, 100), (0, 0, 100, 100)).await,
            [(-1, -1, 102, 102)]
        );
    }

    #[tokio::test]
    async fn buffer_damage_subtracts_the_source_offset_in_surface_units() {
        // The source starts 10 units, which are 20 buffer pixels, into the
        // 200-pixel buffer at scale 2.
        let source = Some((10 * 256, 10 * 256, 80 * 256, 80 * 256));
        assert_eq!(
            host_damage_for(2, source, Some((80, 80)), (200, 200), (100, 100, 20, 20)).await,
            [(39, 39, 12, 12)]
        );
    }

    #[tokio::test]
    async fn source_only_viewport_maps_buffer_damage() {
        // Without a destination the source crops without scaling, and its
        // origin keeps its fraction: x starts at 10.5 units.
        let source = Some((10 * 256 + 128, 10 * 256, 80 * 256, 80 * 256));
        assert_eq!(
            host_damage_for(2, source, None, (200, 200), (100, 100, 20, 20)).await,
            [(39, 39, 11, 12)]
        );
    }

    /// A destroyed viewport is removed at the surface's next commit, as
    /// its state is double-buffered.
    #[tokio::test]
    async fn destroying_viewport_is_double_buffered_until_surface_commit() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        // The 4x4 buffer fills an 8x8 surface: a pixel is two units.
        let viewport = sc.view(surface, 1, None, Some((8, 8))).await;
        let buffer = sc.native(4, 4).await;
        sc.show(surface, buffer).await;
        let damage = || wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1);
        sc.pending([damage()]).await;
        assert_eq!(sc.commit(surface).await, [(-2, -2, 6, 6)]);

        sc.forwarded(viewport, wp_viewport::req::destroy(viewport.id))
            .await;
        // Reads state no peer can observe: until the commit, the surface
        // keeps its viewport, as the host does.
        let state = &sc.s.ctx().surfaces[&surface.guest_id()];
        assert!(
            state.viewport.is_some(),
            "destroy only changes pending viewport state"
        );
        sc.pending([damage()]).await;
        assert_eq!(
            sc.commit(surface).await,
            [(-1, -1, 3, 3)],
            "the viewport is removed at the following surface commit"
        );
    }

    #[tokio::test]
    async fn invalid_viewport_values_and_dead_surface_are_fatal() {
        type Request = fn(viewport: u32) -> Outgoing<u32>;
        let requests: [(Request, bool, u32); 3] = [
            (
                |viewport| wp_viewport::req::set_source(viewport, -1, 0, 10, 10),
                false,
                wp_viewport::ERROR_BAD_VALUE,
            ),
            (
                |viewport| wp_viewport::req::set_destination(viewport, 0, 10),
                false,
                wp_viewport::ERROR_BAD_VALUE,
            ),
            // After its surface is destroyed.
            (
                |viewport| wp_viewport::req::set_destination(viewport, 10, 10),
                true,
                wp_viewport::ERROR_NO_SURFACE,
            ),
        ];
        for (request, destroy_surface, code) in requests {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let viewport = sc.viewport(surface).await;
            if destroy_surface {
                sc.destroy_surface(surface).await;
            }
            sc.s.guest_fatal([request(viewport.id)]).await;
            assert_eq!(
                sc.s.guest_received_and_error(),
                (vec![], (viewport.id, code))
            );
            assert_eq!(sc.s.host_received(), []);
        }
    }

    #[tokio::test]
    async fn duplicate_viewport_is_a_fatal_viewporter_error() {
        use crate::protocols::viewporter::wp_viewporter::req::get_viewport;
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.viewport(surface).await;
        let viewporter = sc.viewporter.unwrap();
        let viewport = sc.s.new_id();
        sc.s.guest_fatal([get_viewport(viewporter, viewport, surface.id)])
            .await;
        assert_eq!(
            sc.s.guest_received_and_error(),
            (vec![], (viewporter, wp_viewporter::ERROR_VIEWPORT_EXISTS))
        );
        assert_eq!(sc.s.host_received(), []);
    }

    impl Screen {
        /// Whether a viewport rule of `surface` that the proxy leaves
        /// unenforced, as Exo does, was logged. Reads state no peer can
        /// observe: the log.
        fn logged(&mut self, surface: Object) -> bool {
            self.s.ctx().surfaces[&surface.guest_id()].logged_viewport_rule
        }
    }

    #[tokio::test]
    async fn fractional_source_without_destination_is_logged_and_forwarded() {
        // Exo takes the fractional source size as the surface size, and the
        // host damage covers it, rounded up.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        sc.view(surface, 1, Some((0, 0, 128, 256)), None).await;
        let buffer = sc.native(1, 1).await;
        assert_eq!(sc.show(surface, buffer).await, [(0, 0, 1, 1)]);
        assert!(sc.logged(surface));
    }

    /// A buffer of `size` that the surface shows at scale 2 through a
    /// viewport with `source` (24.8 fixed point) and a destination, which
    /// lets the source be fractional; the commit reaches the host.
    async fn commit_viewport_source(size: (i32, i32), source: Rect) -> (Screen, Object) {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let viewport = sc.view(surface, 2, Some(source), Some((10, 10))).await;
        let buffer = sc.native(size.0, size.1).await;
        sc.show(surface, buffer).await;
        (sc, viewport)
    }

    #[tokio::test]
    async fn viewport_source_outside_the_buffer_is_logged_and_forwarded() {
        const ONE: i32 = 256;
        // At scale 2 an 8x6 buffer is a 4x3 surface, the source's units.
        let whole = (0, 0, 4 * ONE, 3 * ONE);
        let (mut sc, _) = commit_viewport_source((8, 6), whole).await;
        let surface = sc.surface;
        assert!(!sc.logged(surface));

        // Half a unit to the right the source leaves the buffer. Exo accepts
        // that, so the commit is forwarded and only logged.
        let (mut sc, _) = commit_viewport_source((8, 6), (ONE / 2, 0, 4 * ONE, 3 * ONE)).await;
        assert!(sc.logged(surface));

        // A later, smaller buffer under the unchanged viewport.
        let (mut sc, _) = commit_viewport_source((8, 6), whole).await;
        let smaller = sc.native(6, 6).await;
        sc.show(surface, smaller).await;
        assert!(sc.logged(surface));

        // Detaching the buffer in the same commit exempts any source.
        let (mut sc, viewport) = commit_viewport_source((8, 6), whole).await;
        sc.attach(surface, None).await;
        let source = wp_viewport::req::set_source(viewport.id, 0, 0, 100 * ONE, 100 * ONE);
        sc.forwarded(viewport, source).await;
        sc.commit(surface).await;
        assert!(!sc.logged(surface));

        // An odd buffer ends at a fractional unit, 3.5x2.5, which the source
        // may reach exactly.
        let edge = (0, 0, 3 * ONE + ONE / 2, 2 * ONE + ONE / 2);
        let (mut sc, _) = commit_viewport_source((7, 5), edge).await;
        assert!(!sc.logged(surface));
    }

    /// A session with `ctx` whose host has `zaura_shell`, which the proxy
    /// binds for itself, and whose guest has a toplevel; returns the session,
    /// the host `zaura_shell` and the toplevel.
    async fn shelled(ctx: Context) -> (Session, u32, Toplevel) {
        let mut s = Session::with_context(ctx);
        let shell = s.aura_shell().await;
        let toplevel = s.toplevel().await;
        (s, shell, toplevel)
    }

    /// The guest sets `app_id` on `toplevel`; returns what the host gets.
    async fn set_app_id(s: &mut Session, toplevel: Object, app_id: &str) -> Vec<(u32, AnyRequest)> {
        s.guest([xdg_toplevel::req::set_app_id(toplevel.id, app_id).unwrap()])
            .await;
        assert_eq!(s.guest_received(), []);
        s.host_received()
    }

    /// The host `xdg_toplevel.set_app_id` of `toplevel`, of guest OS app ID
    /// `app_id`.
    fn namespaced(toplevel: Object, app_id: &str) -> (u32, AnyRequest) {
        let app_id = format!("org.chromium.guest_os.termina.wayland.{app_id}");
        (
            toplevel.host,
            xdg_toplevel::Request::SetAppId { app_id }.into(),
        )
    }

    /// What the host gets for the guest's first `set_app_id` of `app_id` on
    /// `toplevel` while the proxy has `shell`: the namespaced ID, and a new
    /// Aura surface of the toplevel's surface, which gets it too; returns
    /// the Aura surface.
    fn first_app_id(
        host: &[(u32, AnyRequest)],
        shell: u32,
        toplevel: Toplevel,
        app_id: &str,
    ) -> u32 {
        let [_, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), _] =
            host
        else {
            panic!("the host got no Aura surface: {host:?}");
        };
        let application_id = Some(format!("org.chromium.guest_os.termina.wayland.{app_id}"));
        assert_eq!(
            host,
            [
                namespaced(toplevel.toplevel, app_id),
                (
                    shell,
                    zaura_shell::Request::GetAuraSurface {
                        id: *id,
                        surface: toplevel.surface.host
                    }
                    .into()
                ),
                (
                    *id,
                    zaura_surface::Request::SetApplicationId { application_id }.into()
                ),
            ]
        );
        *id
    }

    #[tokio::test]
    async fn set_app_id_creates_zaura_surface_and_sets_app_id() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let host = set_app_id(&mut s, toplevel.toplevel, "my_app").await;
        first_app_id(&host, shell, toplevel, "my_app");
    }

    #[tokio::test]
    async fn set_app_id_reuses_existing_zaura_surface() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        // An Aura surface the first app ID created.
        let host = set_app_id(&mut s, toplevel.toplevel, "first").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "first");

        let application_id = Some("org.chromium.guest_os.termina.wayland.reused".into());
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, "reused").await,
            [
                namespaced(toplevel.toplevel, "reused"),
                (
                    aura_surface,
                    zaura_surface::Request::SetApplicationId { application_id }.into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn set_app_id_keeps_the_guest_os_namespace_with_placement_on() {
        let placement = crate::placement_config::PlacementConfig::active_for_test(&[]);
        let ctx = Context::with_placement_for_test(placement);
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let app_id = "com.example.Terminal";
        let host = set_app_id(&mut s, toplevel.toplevel, app_id).await;
        first_app_id(&host, shell, toplevel, app_id);
    }

    #[tokio::test]
    async fn set_app_id_noop_when_no_zaura_shell() {
        let mut s = Session::new();
        let toplevel = s.toplevel().await.toplevel;
        assert_eq!(
            set_app_id(&mut s, toplevel, "no_shell").await,
            [namespaced(toplevel, "no_shell")]
        );
    }

    /// The longest app ID a guest can send, which namespaced exceeds the
    /// Wayland message limit, reaches neither the role nor an Aura surface.
    #[tokio::test]
    async fn oversized_namespaced_app_id_is_dropped_before_wire_encoding() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, _, toplevel) = shelled(ctx).await;
        let oversized = "x".repeat(4_083);
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, &oversized).await,
            [],
            "an app ID exceeding the Wayland message limit must not be sent"
        );
    }

    /// Destroying a toplevel ends its role, which the session checks after
    /// each batch (`Context::assert_no_orphans`): the role must not outlive
    /// its toplevel.
    #[tokio::test]
    async fn xdg_toplevel_destroy_cleans_up_map() {
        use crate::protocols::xdg_shell::xdg_surface;
        let mut s = Session::new();
        let Toplevel {
            xdg_surface: role,
            toplevel,
            ..
        } = s.toplevel().await;
        s.guest([xdg_toplevel::req::destroy(toplevel.id)]).await;
        let destroy = xdg_toplevel::Request::Destroy {};
        assert_eq!(s.host_received(), [(toplevel.host, destroy.into())]);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, toplevel.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: toplevel.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        // The xdg_surface, which has no role left, may end now.
        s.guest([xdg_surface::req::destroy(role.id)]).await;
        let destroy = xdg_surface::Request::Destroy {};
        assert_eq!(s.host_received(), [(role.host, destroy.into())]);
    }

    #[tokio::test]
    async fn wl_surface_destroy_releases_zaura_surface() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let host = set_app_id(&mut s, toplevel.toplevel, "app").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "app");

        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [
                (aura_surface, zaura_surface::Request::Release {}.into()),
                surface_destroy(surface),
            ]
        );
    }

    /// Destroying a surface ends the xdg role on it, so its toplevel's
    /// later app ID reaches no Aura surface of the destroyed surface.
    #[tokio::test]
    async fn wl_surface_destroy_removes_stale_xdg_surface_links() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, _, toplevel) = shelled(ctx).await;
        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(s.host_received(), [surface_destroy(surface)]);
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, "app").await,
            [namespaced(toplevel.toplevel, "app")]
        );
    }

    /// An Aura shell older than `zaura_surface.release` (v38) has no
    /// destructor for an Aura surface: destroying its surface sends none,
    /// and the Aura surface's host ID stays reserved for the connection.
    #[tokio::test]
    async fn old_aura_surface_stays_reserved_without_release_request() {
        let mut s = Session::new();
        s.registry().await;
        s.advertise(zaura_shell::NAME, 37).await;
        let host = s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] = &host[..] else {
            panic!("the proxy did not bind zaura_shell: {host:?}");
        };
        let shell = id.2;
        // The gtk_shell1 the guest is advertised.
        assert_eq!(s.guest_received().len(), 1);
        let toplevel = s.toplevel().await;
        let host = set_app_id(&mut s, toplevel.toplevel, "legacy").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "legacy");

        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [surface_destroy(surface)],
            "old aura-shell versions must not receive release"
        );
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: surface.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        // The proxy reuses freed host IDs first: the surface's, and the
        // Aura surface's if it were free.
        for _ in 0..2 {
            assert_ne!(s.surface().await.host, aura_surface);
        }
    }

    #[test]
    fn namespaced_app_id_wire_size_boundary_is_checked() {
        // Exercise the exact 4096-byte message boundary independently of the
        // namespace prefix used by the production path.
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_083)).is_ok());
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_084)).is_err());
    }

    /// A commit without an attach keeps the surface's buffer, whose size
    /// full damage covers; an attach of no buffer clears it.
    #[tokio::test]
    async fn commit_without_attach_keeps_the_current_buffer() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(8, 6).await;
        // A rotated buffer damages its whole surface, which shows the size
        // of the buffer the surface shows.
        let transform = wl_surface::req::set_buffer_transform(surface.id, 1);
        sc.forwarded(surface, transform).await;
        assert_eq!(sc.show(surface, buffer).await, [(0, 0, 6, 8)]);

        let damage = || wl_surface::req::damage_buffer(surface.id, 1, 1, 1, 1);
        sc.pending([damage()]).await;
        assert_eq!(sc.commit(surface).await, [(0, 0, 6, 8)]);

        // An explicit attach(NULL) is different from omitting attach and must
        // clear the current buffer: a surface without one maps as 1x1.
        sc.attach(surface, None).await;
        sc.pending([damage()]).await;
        assert_eq!(sc.commit(surface).await, [(0, 0, 1, 1)]);
    }

    /// A buffer of a pool whose host `wl_shm` was removed has no host
    /// object and no content: attaching it detaches the surface.
    #[tokio::test]
    async fn attaching_an_inert_buffer_detaches_the_surface() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let registry = s.registry().await;
        let name = s.advertise("wl_shm", wl_shm::VERSION).await;
        let advertised = s.guest_received();
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            advertised[..]
        else {
            panic!("the guest was not advertised wl_shm: {advertised:?}");
        };
        let shm = s.new_id();
        s.guest([wl_registry::req::bind(registry.id, name, "wl_shm", version, shm).unwrap()])
            .await;
        // The proxy binds the host's wl_shm for itself.
        s.host_received();
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4])).await;
        let live = s.shm_buffer(pool, 1, 1).await;
        s.guest([
            wl_surface::req::attach(surface.id, Some(live.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        s.host_received();
        s.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        s.guest_received();
        s.host_received();

        let pool = s.new_id();
        let file = shm_pool_file(&[0; 4]);
        s.guest([wl_shm::req::create_pool(shm, pool, file.into(), 4)])
            .await;
        let inert = s.new_id();
        s.guest([
            wl_shm_pool::req::create_buffer(pool, inert, 0, 1, 1, 4, wl_shm::FORMAT_ARGB8888),
            wl_surface::req::attach(surface.id, Some(inert), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        let detach = wl_surface::Request::Attach {
            buffer: 0,
            x: 0,
            y: 0,
        };
        let mut host = s.host_received();
        assert_eq!(host.remove(0), (surface.host, detach.into()));
        assert_eq!(committed(surface, host), []);
        assert_eq!(s.guest_received(), []);
    }

    /// Only committing an explicit attach begins a compositor use: once the
    /// host released the buffer, a damage-only commit leaves it released,
    /// so the guest's destroy of it reaches the host at once.
    #[tokio::test]
    async fn damage_only_commit_preserves_a_released_use() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let buffer = sc.buffer(kind).await;
            sc.show(surface, buffer).await;
            sc.release(buffer).await;

            sc.pending([wl_surface::req::damage_buffer(surface.id, 0, 0, 1, 1)])
                .await;
            sc.commit(surface).await;
            assert_eq!(
                sc.destroy(buffer).await,
                [buffer_destroy(buffer)],
                "not resubmitted ({kind:?})"
            );
        }
    }

    #[tokio::test]
    async fn native_buffer_damage_uses_recorded_dimensions() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(100, 50).await;
        // A rotated buffer damages its whole surface, whose extent shows
        // the dimensions the damage is mapped with.
        let transform = wl_surface::req::set_buffer_transform(surface.id, 1);
        sc.forwarded(surface, transform).await;
        sc.show(surface, buffer).await;
        sc.pending([wl_surface::req::damage_buffer(surface.id, 10, 10, 20, 20)])
            .await;
        // The whole rotated 100x50 buffer, not a 1x1 fallback.
        assert_eq!(sc.commit(surface).await, [(0, 0, 50, 100)]);
    }

    #[tokio::test]
    async fn destroyed_buffer_damage_uses_committed_content_dimensions() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(100, 50).await;
        let transform = wl_surface::req::set_buffer_transform(surface.id, 1);
        sc.forwarded(surface, transform).await;
        sc.show(surface, buffer).await;
        sc.release(buffer).await;
        assert_eq!(sc.destroy(buffer).await, [buffer_destroy(buffer)]);

        sc.pending([wl_surface::req::damage_buffer(surface.id, 10, 10, 20, 20)])
            .await;
        // The whole rotated 100x50 content snapshot, not a 1x1 fallback.
        assert_eq!(sc.commit(surface).await, [(0, 0, 50, 100)]);
    }

    #[tokio::test]
    async fn release_owed_after_surface_destroy_is_expected() {
        // A GTK client's exit: the surface showing a submitted buffer is
        // destroyed first, and Exo then releases the buffer it no longer
        // shows while the guest still owns it.
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(1, 1).await;
        sc.show(surface, buffer).await;
        assert_eq!(
            sc.destroy_surface(surface).await,
            [surface_destroy(surface)]
        );

        // The guest still gets its wl_buffer.release.
        sc.release(buffer).await;
        // Reads state no peer can observe: the owed release is recorded
        // rather than logged as unexpected, so a second one would be.
        assert_eq!(
            sc.s.ctx().host_buffer_use(buffer.guest_id()),
            Some(RenderBufferUse::Released)
        );
        assert_eq!(sc.destroy(buffer).await, [buffer_destroy(buffer)]);
    }

    #[tokio::test]
    async fn attaching_an_idle_buffer_without_commit_keeps_its_release_edge() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let buffer = sc.buffer(kind).await;
            sc.show(surface, buffer).await;
            sc.release(buffer).await;
            sc.attach(surface, Some(buffer)).await;

            // No commit follows the attach yet. Keep the host object alive
            // until the pending state is resolved; otherwise a later commit
            // could race the destructor.
            assert_eq!(sc.destroy(buffer).await, [], "{kind:?}");

            // Replacing the pending attach proves that it will never commit,
            // so the host destructor can now be ordered safely.
            sc.s.guest([wl_surface::req::attach(surface.id, None, 0, 0)])
                .await;
            let detach = wl_surface::Request::Attach {
                buffer: 0,
                x: 0,
                y: 0,
            };
            assert_eq!(
                sc.s.host_received(),
                [(surface.host, detach.into()), buffer_destroy(buffer)],
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_delayed_release_before_a_pending_commit_keeps_the_host_buffer_alive() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let buffer = sc.buffer(kind).await;
            sc.show(surface, buffer).await;
            sc.attach(surface, Some(buffer)).await;
            assert_eq!(sc.destroy(buffer).await, [], "{kind:?}");

            // This release belongs to the old committed interval. It must not
            // destroy the host object while the pending attach can still
            // commit.
            assert_eq!(sc.release_destroyed(buffer).await, [], "{kind:?}");
            sc.commit(surface).await;

            // A release for the newly committed interval is the one that
            // permits the host destructor.
            assert_eq!(
                sc.release_destroyed(buffer).await,
                [buffer_destroy(buffer)],
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn replacing_a_pending_destroyed_buffer_does_not_wait_for_release() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let old = sc.buffer(kind).await;
            let new = sc.buffer(kind).await;
            sc.attach(surface, Some(old)).await;
            assert_eq!(
                sc.destroy(old).await,
                [],
                "a pending attach keeps the {kind:?} buffer alive until replaced"
            );

            // The old pending attach is superseded before any commit reaches
            // the compositor, so no wl_buffer.release can arrive for it.
            sc.s.guest([wl_surface::req::attach(surface.id, Some(new.id), 0, 0)])
                .await;
            let attach = wl_surface::Request::Attach {
                buffer: new.host,
                x: 0,
                y: 0,
            };
            assert_eq!(
                sc.s.host_received(),
                [(surface.host, attach.into()), buffer_destroy(old)],
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn replacing_detached_submitted_buffer_waits_for_release() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.shm(1, 1).await.buffer;
        let next = sc.shm(1, 1).await.buffer;
        // The buffer was committed by an earlier surface state and destroyed
        // by the guest, but its host use interval is still in flight. The
        // surface no longer shows it: a later attach must not mistake that
        // state for an uncommitted pending attach.
        sc.show(surface, buffer).await;
        sc.attach(surface, None).await;
        sc.commit(surface).await;
        assert_eq!(sc.destroy(buffer).await, []);

        sc.attach(surface, Some(next)).await;
        assert_eq!(
            sc.release_destroyed(buffer).await,
            [buffer_destroy(buffer)],
            "a submitted buffer must wait for wl_buffer.release after detach"
        );
    }

    #[tokio::test]
    async fn terminal_buffer_cannot_partially_commit_a_new_attach() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.native(8, 6).await;
        sc.attach(surface, Some(buffer)).await;
        // Injects a fault no peer can cause: the attached buffer's host
        // destroy is queued, as if the guest had destroyed it.
        let host = buffer.host_id();
        assert!(sc
            .s
            .ctx()
            .complete_queued_buffer_destroy(buffer.guest_id(), host));

        sc.s.guest_fatal([wl_surface::req::commit(surface.id)])
            .await;
        assert_eq!(
            sc.s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(sc.s.host_received(), []);
    }

    #[tokio::test]
    async fn wl_surface_destroy_retires_a_submitted_buffer_after_the_surface() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let surface = sc.surface;
            let buffer = sc.buffer(kind).await;
            sc.show(surface, buffer).await;
            assert_eq!(sc.destroy(buffer).await, [], "{kind:?}");

            assert_eq!(
                sc.destroy_surface(surface).await,
                [surface_destroy(surface), buffer_destroy(buffer)],
                "the host {kind:?} buffer must be destroyed after the surface"
            );
        }
    }

    #[tokio::test]
    async fn shared_buffer_waits_for_its_last_surface_destroy() {
        let mut sc = Screen::new().await;
        let first = sc.surface;
        let second = sc.s.surface().await;
        let buffer = sc.native(1, 1).await;
        for surface in [first, second] {
            sc.show(surface, buffer).await;
        }
        assert_eq!(sc.destroy(buffer).await, []);

        assert_eq!(
            sc.destroy_surface(first).await,
            [surface_destroy(first)],
            "one surface cannot retire a buffer still owned by another"
        );
        assert_eq!(
            sc.destroy_surface(second).await,
            [surface_destroy(second), buffer_destroy(buffer)],
            "the buffer destructor must follow both owning surface destructors"
        );
    }

    #[tokio::test]
    async fn pending_attach_preserves_backing_but_not_destroyed_surface_use() {
        let mut sc = Screen::new().await;
        let first = sc.surface;
        let second = sc.s.surface().await;
        let buffer = sc.native(1, 1).await;
        sc.show(first, buffer).await;
        sc.attach(second, Some(buffer)).await;
        assert_eq!(sc.destroy(buffer).await, []);

        // Destroying the only committed surface closes its compositor-use
        // interval, but the unresolved pending attach must preserve the
        // buffer backing.
        assert_eq!(sc.destroy_surface(first).await, [surface_destroy(first)]);
        assert_eq!(
            sc.destroy_surface(second).await,
            [surface_destroy(second), buffer_destroy(buffer)],
            "the buffer destructor must follow both surface destructors without waiting for release"
        );
    }

    #[tokio::test]
    async fn destroying_surface_then_live_buffer_does_not_wait_for_release() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let buffer = sc.shm(1, 1).await.buffer;
        sc.show(surface, buffer).await;
        // The destroyed surface was the only compositor-use reference.
        assert_eq!(
            sc.destroy_surface(surface).await,
            [surface_destroy(surface)]
        );

        // The guest is allowed to keep the wl_buffer alive after destroying
        // its surface. Once it eventually destroys the buffer, host teardown
        // must follow the surface destructor instead of waiting forever for
        // a release that may never arrive.
        assert_eq!(sc.destroy(buffer).await, [buffer_destroy(buffer)]);
    }

    #[tokio::test]
    async fn destroying_unrelated_surface_keeps_detached_submitted_buffer() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let unrelated = sc.s.surface().await;
        let buffer = sc.shm(1, 1).await.buffer;
        // A committed buffer whose surface was detached before the release
        // edge arrived. It is not owned by the unrelated surface.
        sc.show(surface, buffer).await;
        sc.attach(surface, None).await;
        sc.commit(surface).await;
        assert_eq!(sc.destroy(buffer).await, []);

        assert_eq!(
            sc.destroy_surface(unrelated).await,
            [surface_destroy(unrelated)],
            "an unrelated surface destroy must not preempt wl_buffer.release"
        );
        assert_eq!(sc.release_destroyed(buffer).await, [buffer_destroy(buffer)]);
    }

    #[tokio::test]
    async fn a_detached_use_survives_destroy_of_the_new_current_surface() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let (first, second) = (sc.surface, sc.s.surface().await);
            let buffer = sc.buffer(kind).await;

            sc.show(first, buffer).await;
            // Replacing B on S0 begins an unattributed outstanding use: S0 no
            // longer names B, but only wl_buffer.release can prove the host
            // has stopped sampling that detached attachment.
            sc.attach(first, None).await;
            sc.commit(first).await;
            // A later current use must preserve, not overwrite, the detached
            // use from S0.
            sc.show(second, buffer).await;
            assert_eq!(sc.destroy(buffer).await, [], "{kind:?}");

            assert_eq!(
                sc.destroy_surface(second).await,
                [surface_destroy(second)],
                "destroying S1 proves only S1's current use, not S0's detached \
                 use: it must keep the {kind:?} backing alive until host release"
            );
            assert_eq!(
                sc.release_destroyed(buffer).await,
                [buffer_destroy(buffer)],
                "the global host release closes every outstanding {kind:?} use"
            );
        }
    }

    #[tokio::test]
    async fn destroying_a_pending_only_surface_keeps_awaiting_release() {
        for kind in BUFFER_KINDS {
            let mut sc = Screen::new().await;
            let (first, second) = (sc.surface, sc.s.surface().await);
            let buffer = sc.buffer(kind).await;

            // The release belongs to an earlier committed use. Merely
            // attaching the same buffer to another surface does not transfer
            // ownership of that outstanding release edge until the new
            // surface commits.
            sc.show(first, buffer).await;
            sc.attach(first, None).await;
            sc.commit(first).await;
            sc.attach(second, Some(buffer)).await;
            assert_eq!(sc.destroy(buffer).await, [], "{kind:?}");

            assert_eq!(
                sc.destroy_surface(second).await,
                [surface_destroy(second)],
                "destroying a pending-only surface must not consume another \
                 use's release edge: the host {kind:?} buffer must remain \
                 alive until its real release"
            );
            assert_eq!(
                sc.release_destroyed(buffer).await,
                [buffer_destroy(buffer)],
                "the real release must retire the {kind:?} buffer"
            );
        }
    }

    #[tokio::test]
    async fn commit_from_a_truncated_pool_is_a_protocol_error_not_a_crash() {
        let mut s = Session::new();
        let surface = s.surface().await;
        let shm = s.bind("wl_shm", wl_shm::VERSION).await;
        let client_file = shm_pool_file(&[0x11; 4]);
        let pool = s.shm_pool(shm, client_file.try_clone().unwrap()).await;
        let buffer = s.shm_buffer(pool, 1, 1).await;

        // The client shrinks its pool file behind the proxy's back. Reading
        // the pixels through a mapping of it raised SIGBUS and killed the
        // proxy with every guest GUI app.
        client_file.set_len(0).unwrap();
        s.guest_fatal([
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (buffer.id, wl_shm::ERROR_INVALID_FD)),
            "the error names the buffer whose pool failed, as libwayland does"
        );
        assert_eq!(
            s.host_received(),
            [(
                surface.host,
                wl_surface::Request::Attach {
                    buffer: buffer.host,
                    x: 0,
                    y: 0
                }
                .into()
            )],
            "only the attach before the failed commit reaches the host \
             (a_rejected_request_sends_nothing_of_its_own pins the rest)"
        );
    }

    /// A surface has no state until a request sets some, so its first
    /// commit may have none; it commits the defaults, as a fresh surface
    /// has them, and is forwarded.
    #[tokio::test]
    async fn a_first_commit_without_surface_state_is_forwarded() {
        let mut s = Session::new();
        let surface = s.surface().await;
        s.guest([wl_surface::req::commit(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(surface.host, wl_surface::Request::Commit {}.into())]
        );
        assert_eq!(s.guest_received(), []);
    }

    /// Surface damage of more rectangles than the proxy keeps damages the
    /// whole surface, so a damage-only commit copies the whole buffer, not
    /// just the buffer damage, which here is none.
    #[tokio::test]
    async fn overflowing_surface_damage_copies_the_whole_buffer() {
        let mut sc = Screen::new().await;
        let surface = sc.surface;
        let a = sc.shm(2, 1).await;
        sc.show(surface, a.buffer).await;

        // The client redraws both pixels, and damages only the first, in
        // more rectangles than the proxy keeps (256).
        a.draw(0x22);
        let damage = (0..=256).map(|_| wl_surface::req::damage(surface.id, 0, 0, 1, 1));
        sc.pending(damage).await;
        assert_eq!(sc.commit(surface).await, [(0, 0, 2, 1)]);
        assert_eq!(a.copied(0x22), [(0, 0), (1, 0)]);
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
    fn damage_buffer_mapping_does_not_include_surface_offset() {
        let mut surface = SurfaceState::default();
        surface.pending.attach_offset = Some((80, 90));
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

    /// A keyboard of its own seat, which the host has focused on a surface,
    /// and the extended keyboard the proxy created for it at that enter.
    #[derive(Clone, Copy)]
    struct Focused {
        seat: u32,
        keyboard: Object,
        ext: u32,
    }

    /// The host advertises the keyboard extension, which the proxy binds
    /// for itself.
    async fn keyboard_extension(s: &mut Session) {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
        let version = zcr_keyboard_extension_v1::VERSION;
        s.advertise(zcr_keyboard_extension_v1::NAME, version).await;
        let bind = s.host_received();
        assert!(
            matches!(
                &bind[..],
                [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { .. }))]
            ),
            "{bind:?}"
        );
    }

    /// A keyboard of a new seat, which the host gives its first focus, on
    /// `surface`; the guest gets the enter.
    async fn focused_keyboard(s: &mut Session, surface: Object) -> Focused {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::Request;
        let (seat, keyboard) = s.keyboard().await;
        s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        let host = s.host_received();
        let [(_, AnyRequest::ZcrKeyboardExtensionV1(Request::GetExtendedKeyboard { id, .. }))] =
            host[..]
        else {
            panic!("the keyboard got no extended keyboard: {host:?}");
        };
        let enter = wl_keyboard::Event::Enter {
            serial: 1,
            surface: surface.id,
            keys: Vec::new(),
        };
        let guest = s.guest_received();
        assert_eq!(guest[..1], [(keyboard.id, enter.into())]);
        assert_eq!(guest.len(), 2, "the enter and its modifiers");
        Focused {
            seat,
            keyboard,
            ext: id,
        }
    }

    /// `k`'s extended keyboard peeks a press of `key`, which reaches no
    /// peer.
    async fn peek(s: &mut Session, k: Focused, serial: u32, key: u32) {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        s.host([zcr_extended_keyboard_v1::evt::peek_key(
            k.ext, serial, serial, key, pressed,
        )])
        .await;
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.host_received(), []);
    }

    /// `k`'s host sends key event `serial` of `key` in `state`; returns
    /// what the guest gets. The proxy acknowledges it.
    async fn key(
        s: &mut Session,
        k: Focused,
        serial: u32,
        key: u32,
        state: u32,
    ) -> Vec<(u32, AnyEvent)> {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::Request;
        s.host([wl_keyboard::evt::key(
            k.keyboard.host,
            serial,
            serial,
            key,
            state,
        )])
        .await;
        let host = s.host_received();
        assert!(
            matches!(
                host[..],
                [(ext, AnyRequest::ZcrExtendedKeyboardV1(Request::AckKey { serial: acked, .. }))]
                    if ext == k.ext && acked == serial
            ),
            "the key was not acknowledged: {host:?}"
        );
        s.guest_received()
    }

    /// The key event `k`'s guest keyboard gets for [`key`].
    fn key_event(k: Focused, serial: u32, key: u32, state: u32) -> (u32, AnyEvent) {
        let event = wl_keyboard::Event::Key {
            serial,
            time: serial,
            key,
            state,
        };
        (k.keyboard.id, event.into())
    }

    /// Destroying a surface retires the focus of the keyboards on it and
    /// their input state, so the host's later keys of the guest's held key
    /// reach the guest no more, while it keeps the keyboard's keymap, and
    /// the focus and input state of another seat on another surface.
    #[tokio::test]
    async fn wl_surface_destroy_removes_keyboard_focus_links() {
        const KEY_A: u32 = 30;
        const KEY_S: u32 = 31;
        const SPACE: u32 = 57;
        let (pressed, released) = (
            wl_keyboard::KEY_STATE_PRESSED,
            wl_keyboard::KEY_STATE_RELEASED,
        );
        let mut s = Session::new();
        keyboard_extension(&mut s).await;
        let destroyed = s.surface().await;
        let other = s.surface().await;
        // A keyboard of one seat on the surface, and one of another seat on
        // another.
        let first = focused_keyboard(&mut s, destroyed).await;
        let second = focused_keyboard(&mut s, other).await;
        // The first keyboard holds A, whose press the guest got, and the
        // second has a peek watermark.
        peek(&mut s, first, 2, KEY_A).await;
        assert_eq!(
            key(&mut s, first, 2, KEY_A, pressed).await,
            [key_event(first, 2, KEY_A, pressed)]
        );
        peek(&mut s, second, 3, SPACE).await;

        s.guest([wl_surface::req::destroy(destroyed.id)]).await;
        assert_eq!(s.host_received(), [surface_destroy(destroyed)]);
        assert_eq!(s.guest_received(), []);

        assert_eq!(
            key(&mut s, first, 4, KEY_A, released).await,
            [],
            "destroying a surface must retire all per-keyboard input state"
        );
        // Reads state no peer can observe: the keyboard's focus, as the
        // proxy forwards a key of a keyboard without one (the guest drops
        // it); the keymap, which the host sends once; and the peek
        // watermarks, which reach no peer until a confirmation.
        let keyboard = first.keyboard.host_id();
        assert!(
            s.ctx().keys.focus().focus_for_keyboard(keyboard).is_none(),
            "destroying a surface must retire per-keyboard focus"
        );
        let keys = &s.ctx().keys;
        assert!(
            keys.keyboards().keymap_for_test(keyboard).is_some(),
            "surface focus teardown must preserve keymap-derived capabilities"
        );
        assert!(
            keys.latest_peek_sequence(GuestId::for_test(first.seat), Some(destroyed.guest_id()))
                .is_none(),
            "destroying a surface must retire its peek watermark"
        );
        assert!(
            keys.latest_peek_sequence(GuestId::for_test(second.seat), Some(other.guest_id()))
                .is_some(),
            "another live seat's watermark must remain intact"
        );
        assert_eq!(
            key(&mut s, second, 6, KEY_S, pressed).await,
            [key_event(second, 6, KEY_S, pressed)],
            "focus for another live surface must remain intact"
        );
    }

    /// Destroying a surface moves every text input on it off it, with a
    /// `leave` while the surface is still valid and no commit of a shown
    /// preedit, and deactivates the active one, on every seat whose focus
    /// was on it; a seat on another surface keeps its focus, peek
    /// watermark and text input. The host's late keyboard leaves of the
    /// destroyed surface change nothing.
    #[tokio::test]
    async fn wl_surface_destroy_transitions_all_focused_seats_and_preserves_others() {
        use crate::protocols::wayland::wl_keyboard;
        let mut s = Session::new();
        keyboard_extension(&mut s).await;
        let destroyed = s.surface().await;
        let live = s.surface().await;
        let mut seats = Vec::new();
        for (surface, serial) in [(destroyed, 1), (destroyed, 2), (live, 3)] {
            let k = focused_keyboard(&mut s, surface).await;
            // A peek of each keyboard on its focus.
            peek(&mut s, k, serial, 30).await;
            let ti = s.text_input(k.seat).await;
            let enter = zwp_text_input_v3::Event::Enter {
                surface: surface.id,
            };
            assert_eq!(s.guest_received(), [(ti.id, enter.into())]);
            seats.push((k, ti, surface));
        }
        let [(k1, ti1, _), (k2, ti2, _), (k3, ti3, _)] = seats[..] else {
            unreachable!()
        };
        // Only the first is enabled, as one text input on a connection can
        // be, and composes; the others have state pending.
        s.enable(ti1, 1).await;
        let preedit = zwp_text_input_v1::evt::preedit_string(ti1.host, 0, "한", "");
        s.host([preedit.unwrap()]).await;
        assert_eq!(s.guest_received().len(), 2, "the preedit and its done");
        for ti in [ti2, ti3] {
            let text = zwp_text_input_v3::req::set_surrounding_text(ti.id, "한", 3, 3);
            s.guest([text.unwrap()]).await;
        }
        assert_eq!(s.host_received(), []);
        // Reads state no peer can observe: the peek watermarks.
        for (k, surface) in [(k1, destroyed), (k2, destroyed), (k3, live)] {
            let keys = &s.ctx().keys;
            assert!(keys
                .latest_peek_sequence(GuestId::for_test(k.seat), Some(surface.guest_id()))
                .is_some());
        }

        s.guest([wl_surface::req::destroy(destroyed.id)]).await;
        let leave = || zwp_text_input_v3::Event::Leave {
            surface: destroyed.id,
        };
        assert_eq!(
            s.guest_received(),
            [(ti1.id, leave().into()), (ti2.id, leave().into())]
        );
        let host = s.host_received();
        let [.., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("the host got no drain barrier: {host:?}");
        };
        let deactivate = zwp_text_input_v1::Request::Deactivate {
            seat: s
                .ctx()
                .shadow_table
                .get_host_id(GuestId::for_test(k1.seat))
                .unwrap()
                .wire(),
        };
        assert_eq!(
            host,
            [
                surface_destroy(destroyed),
                (ti1.host, deactivate.into()),
                (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into()),
            ]
        );
        // Reads state no peer can observe: the peek watermarks.
        let keys = &s.ctx().keys;
        assert!(keys
            .latest_peek_sequence(GuestId::for_test(k1.seat), Some(destroyed.guest_id()))
            .is_none());
        assert!(keys
            .latest_peek_sequence(GuestId::for_test(k2.seat), Some(destroyed.guest_id()))
            .is_none());
        assert!(keys
            .latest_peek_sequence(GuestId::for_test(k3.seat), Some(live.guest_id()))
            .is_some());

        for k in [k1, k2] {
            let leave = wl_keyboard::evt::leave(k.keyboard.host, 10, destroyed.host);
            s.host([leave]).await;
            assert_eq!(s.guest_received(), []);
            assert_eq!(s.host_received(), []);
        }
        // The third seat keeps its focus on the live surface.
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        assert_eq!(
            key(&mut s, k3, 11, 31, pressed).await,
            [key_event(k3, 11, 31, pressed)]
        );
    }

    #[tokio::test]
    async fn wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave() {
        use crate::protocols::wayland::{wl_display, wl_keyboard, wl_seat};
        use crate::wire::WL_DISPLAY_ID;
        let mut s = Session::new();
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let keyboard = s.keyboard_of(seat.id).await;
        let surface = s.surface().await;
        s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        assert_eq!(s.guest_received().len(), 2, "the enter and its modifiers");
        let text_input = s.text_input(seat.id).await;
        let ti = text_input.id;
        let enter = zwp_text_input_v3::Event::Enter {
            surface: surface.id,
        };
        assert_eq!(s.guest_received(), [(ti, enter.into())]);
        s.enable(text_input, 1).await;
        let done = || (ti, zwp_text_input_v3::Event::Done { serial: 1 }.into());
        let preedit = zwp_text_input_v1::evt::preedit_string(text_input.host, 0, "한", "");
        s.host([preedit.unwrap()]).await;
        let shown = zwp_text_input_v3::Event::PreeditString {
            text: Some("한".into()),
            cursor_begin: 3,
            cursor_end: 3,
        };
        assert_eq!(s.guest_received(), [(ti, shown.into()), done()]);

        s.guest([wl_surface::req::destroy(surface.id)]).await;
        let leave = zwp_text_input_v3::Event::Leave {
            surface: surface.id,
        };
        assert_eq!(s.guest_received(), [(ti, leave.into())]);
        let host = s.host_received();
        let [.., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("the host got no drain barrier: {host:?}");
        };
        let deactivate = zwp_text_input_v1::Request::Deactivate { seat: seat.host };
        assert_eq!(
            host,
            [
                (surface.host, wl_surface::Request::Destroy {}.into()),
                (text_input.host, deactivate.into()),
                (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into()),
            ]
        );

        // The host may report the old keyboard leave after surface teardown.
        // It must not emit a second v3 leave for the destroyed surface.
        s.host([wl_keyboard::evt::leave(keyboard.host, 2, surface.host)])
            .await;
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.host_received(), []);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
            .await;
        let delete_id = wl_display::Event::DeleteId { id: surface.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
    }

    #[test]
    fn wl_surface_destroy_repairs_only_text_inputs_on_the_stale_surface() {
        let (g, h) = (GuestId::for_test, HostId::for_test);
        let mut ctx = toplevel_ctx();
        let destroyed_surface = SURFACE.guest_id();
        let replacement_surface = g(101);
        let replacement_host_surface = h(201);
        let seat_with_replacement = g(1);
        let seat_without_owner = g(3);
        let stale_with_replacement = g(700);
        let healthy_on_replacement = g(710);
        let stale_without_owner = g(720);

        for (guest_seat, host_seat) in [(seat_with_replacement, h(2)), (seat_without_owner, h(4))] {
            ctx.shadow_table
                .insert_paired_for_test(guest_seat, host_seat, Iface::WlSeat);
        }
        ctx.shadow_table.insert_paired_for_test(
            replacement_surface,
            replacement_host_surface,
            Iface::WlSurface,
        );
        crate::handler::keyboard::for_test::focus(
            &mut ctx,
            900,
            seat_with_replacement,
            replacement_surface,
            replacement_host_surface,
        );
        for (guest_text_input, host_object, state) in [
            (
                stale_with_replacement,
                h(701),
                composing(h(702), seat_with_replacement, destroyed_surface),
            ),
            // Only the first is enabled: one text input on a connection can
            // be.
            (
                healthy_on_replacement,
                h(711),
                disabled(h(712), seat_with_replacement, replacement_surface),
            ),
            (
                stale_without_owner,
                h(721),
                disabled(h(722), seat_without_owner, destroyed_surface),
            ),
        ] {
            ctx.shadow_table.insert_paired_for_test(
                guest_text_input,
                host_object,
                Iface::ZwpTextInputV3,
            );
            text_input::insert(&mut ctx, guest_text_input, state);
        }
        // A peek of the keyboard on the replacement surface.
        crate::handler::keyboard::for_test::created(&mut ctx, 800, 900, 1);
        crate::handler::keyboard::for_test::extended(&mut ctx, 900, 950);
        crate::handler::keyboard::for_test::peek(&mut ctx, 950, 1, 30, 1);
        assert!(
            ctx.keys
                .focus()
                .surface_for_seat(seat_without_owner)
                .is_none(),
            "one stale projection must have no remaining keyboard owner"
        );

        let state = text_input::view(&ctx, healthy_on_replacement);
        assert_eq!(state.focus, Some(replacement_surface));
        assert!(state.pending);
        let healthy = text_input::snapshot(&ctx, healthy_on_replacement);
        let mut destroy =
            crate::wire::WireMessage::new(SURFACE.guest, wl_surface::REQ_DESTROY, &[]);
        let forward = crate::test_support::dispatch_request(&mut ctx, &mut destroy);
        assert_eq!(crate::test_support::raw_dispatch(forward), Ok(None));

        let repaired = text_input::view(&ctx, stale_with_replacement);
        assert_eq!(repaired.focus, Some(replacement_surface));
        assert!(!repaired.enabled);
        assert_ne!(repaired.host, Host::Active);
        assert_eq!(repaired.preedit, None);

        let repaired_without_owner = text_input::view(&ctx, stale_without_owner);
        assert_eq!(repaired_without_owner.focus, None);
        assert!(!repaired_without_owner.enabled);
        assert_ne!(repaired_without_owner.host, Host::Active);

        assert_eq!(text_input::snapshot(&ctx, healthy_on_replacement), healthy);

        let to_guest = queued_headers(ctx.guest_queue_for_test());
        assert_eq!(to_guest.len(), 3);
        assert!(to_guest
            .iter()
            .all(|&(sender, _)| sender != healthy_on_replacement.wire()));
        let to_host = queued_headers(ctx.host_queue_for_test());
        assert!(to_host.contains(&(702, zwp_text_input_v1::REQ_DEACTIVATE)));
        assert!(to_host
            .iter()
            .all(|(sender, _)| ![712, 722].contains(sender)));
        assert!(ctx
            .keys
            .latest_peek_sequence(seat_with_replacement, Some(replacement_surface))
            .is_some());
    }
}
