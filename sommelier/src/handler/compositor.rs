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

use crate::handler::display::queue_protocol_error;
use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE;
use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE;
use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
use crate::protocols::wayland::wl_compositor::WlCompositorHandler;
use crate::protocols::wayland::wl_region::WlRegionHandler;
use crate::protocols::wayland::wl_subcompositor::WlSubcompositorHandler;
use crate::protocols::wayland::wl_subsurface::WlSubsurfaceHandler;
use crate::protocols::wayland::wl_surface::{
    WlSurfaceHandler, REQ_COMMIT, REQ_DAMAGE, REQ_DESTROY,
};
use crate::protocols::xdg_shell::xdg_toplevel::REQ_SET_APP_ID;
use crate::state::{Context, DamageRect, SurfaceState, ViewportState};
use crate::wire::Action;
use log::debug;
use std::collections::HashSet;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

pub struct CompositorHandler;

/// RAII guard for the VirtWL dma-buf CPU-write interval.
///
/// A copy path can fail while mapping a BO or while validating a damage
/// rectangle. Keeping the matching END ioctl in `Drop` prevents a panic or a
/// future early-return path from leaving the host dma-buf permanently locked.
struct DmabufWriteSync<'a> {
    channel: &'a crate::virtwl_channel::VirtWaylandChannel,
    fd: RawFd,
    ended: bool,
}

impl<'a> DmabufWriteSync<'a> {
    fn begin(
        channel: Option<&'a Arc<crate::virtwl_channel::VirtWaylandChannel>>,
        fd: Option<RawFd>,
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

impl WlCompositorHandler for CompositorHandler {}

// ChromiumOS reserves headroom around the i32 damage range before applying
// compositor scaling. This keeps the host compositor's x + width arithmetic
// from overflowing when clients submit extreme but representable damage.
const DAMAGE_MIN: i64 = i32::MIN as i64 / 10;
const DAMAGE_MAX: i64 = i32::MAX as i64 / 10;

fn clamp_damage_edge(value: i64) -> i64 {
    value.clamp(DAMAGE_MIN, DAMAGE_MAX)
}

fn transformed_extent(transform: i32, width: i32, height: i32) -> (i32, i32) {
    if matches!(transform, 1 | 3 | 5 | 7) {
        (height, width)
    } else {
        (width, height)
    }
}

fn logical_transformed_extent(
    transform: i32,
    width: i32,
    height: i32,
    buffer_scale: i32,
) -> (i32, i32) {
    let (width, height) = transformed_extent(transform, width, height);
    let scale = f64::from(buffer_scale.max(1));
    (
        (f64::from(width) / scale).ceil().max(1.0) as i32,
        (f64::from(height) / scale).ceil().max(1.0) as i32,
    )
}

fn map_surface_damage(rect: DamageRect) -> DamageRect {
    if rect.width <= 0 || rect.height <= 0 {
        return DamageRect::new(0, 0, 0, 0);
    }
    let left = clamp_damage_edge(i64::from(rect.x).saturating_sub(1));
    let top = clamp_damage_edge(i64::from(rect.y).saturating_sub(1));
    let right = clamp_damage_edge(
        i64::from(rect.x)
            .saturating_add(i64::from(rect.width))
            .saturating_add(1),
    );
    let bottom = clamp_damage_edge(
        i64::from(rect.y)
            .saturating_add(i64::from(rect.height))
            .saturating_add(1),
    );
    DamageRect::new(
        left as i32,
        top as i32,
        (right - left).max(1) as i32,
        (bottom - top).max(1) as i32,
    )
}

fn native_wayland_app_id(vm_identifier: &str, app_id: &str) -> String {
    format!("org.chromium.guest_os.{}.wayland.{}", vm_identifier, app_id)
}

fn wayland_string_fits_message(value: &str) -> bool {
    // A string is encoded as a u32 length (including NUL), the bytes, and
    // 32-bit padding. Keep the complete message within Wayland's 16-bit
    // length field before handing it to MessageBuilder.
    let Some(length_with_nul) = value.len().checked_add(1) else {
        return false;
    };
    let Some(padded_length) = length_with_nul.checked_add(3).map(|len| len & !3) else {
        return false;
    };
    8usize
        .checked_add(4)
        .and_then(|header_and_length| header_and_length.checked_add(padded_length))
        .is_some_and(|total| total <= 0xffff)
}

#[allow(clippy::too_many_arguments)]
fn copy_damage_to_ptr(
    format: u32,
    offset: i32,
    width: i32,
    height: i32,
    src_stride: u32,
    needs_full_copy: bool,
    src_ptr: *const u8,
    pool_size: usize,
    dst_ptr: *mut u8,
    dest_size: usize,
    dst_stride: usize,
    dst_plane1_offset: usize,
    dst_plane1_stride: usize,
    surface_damage: &[DamageRect],
    buffer_damage: &[DamageRect],
    full_mapping: bool,
) -> bool {
    if src_ptr.is_null() || dst_ptr.is_null() {
        return false;
    }
    let Some(offset) = usize::try_from(offset).ok() else {
        return false;
    };
    let Some(height) = usize::try_from(height).ok() else {
        return false;
    };
    let Some(width) = usize::try_from(width).ok() else {
        return false;
    };
    let layout = crate::handler::shm::ShmCopyLayout {
        pool_size,
        dest_size,
        format,
        offset,
        width,
        src_stride: src_stride as usize,
        dst_stride,
        dst_plane1_offset,
        dst_plane1_stride,
        height,
    };
    let full_damage = [DamageRect::new(0, 0, width as i32, height as i32)];
    if needs_full_copy || full_mapping {
        crate::handler::shm::copy_shm_damage(src_ptr, dst_ptr, layout, &full_damage)
    } else {
        let mut merged = Vec::with_capacity(surface_damage.len() + buffer_damage.len());
        merged.extend_from_slice(surface_damage);
        merged.extend_from_slice(buffer_damage);
        // Keep the temporary vector alive through the copy call.
        crate::handler::shm::copy_shm_damage(src_ptr, dst_ptr, layout, &merged)
    }
}

fn saturating_i32(value: f64) -> i32 {
    if value.is_nan() {
        return 0;
    }
    if value <= f64::from(i32::MIN) {
        i32::MIN
    } else if value >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        value as i32
    }
}

/// Convert one buffer-local damage rectangle into surface-local coordinates.
///
/// This is deliberately conservative. The viewport protocol describes source
/// coordinates in post-buffer-scale surface units, while `damage_buffer`
/// describes pixels. Mapping the two with floating point and then enclosing
/// the result avoids under-damaging a pixel at either edge. Buffer transforms
/// and malformed viewport state fall back to the complete surface rectangle;
/// dropping damage is never safe.
fn map_buffer_damage(
    rect: DamageRect,
    surface: &SurfaceState,
    buffer_width: i32,
    buffer_height: i32,
) -> DamageRect {
    if rect.width <= 0 || rect.height <= 0 {
        return DamageRect::new(0, 0, 0, 0);
    }
    // wl_surface transforms are forwarded to the host, but this small SHM
    // bridge does not rotate its linear copy. Damage the complete buffer until
    // a rotated copy path is available.
    if surface.current_buffer_transform != 0 {
        if let Some(viewport) = surface.viewport.filter(|viewport| !viewport.is_identity()) {
            if let Some((destination_width, destination_height)) = viewport.destination {
                return DamageRect::new(0, 0, destination_width, destination_height);
            }
            if let Some((_, _, source_width, source_height)) = viewport.source {
                return DamageRect::new(
                    0,
                    0,
                    (source_width / 256).max(1),
                    (source_height / 256).max(1),
                );
            }
        }
        let (width, height) = logical_transformed_extent(
            surface.current_buffer_transform,
            buffer_width.max(1),
            buffer_height.max(1),
            surface.current_buffer_scale,
        );
        return DamageRect::new(0, 0, width, height);
    }

    let viewport = surface.viewport.filter(|viewport| !viewport.is_identity());
    let contents_width = f64::from(buffer_width.max(1));
    let contents_height = f64::from(buffer_height.max(1));
    let mut scale_x = f64::from(surface.current_buffer_scale.max(1));
    let mut scale_y = scale_x;
    let mut offset_x = 0.0;
    let mut offset_y = 0.0;

    // Match ChromiumOS' compute_buffer_scale_and_offset(). Source offsets are
    // in the post-buffer-scale surface coordinate space.
    if let Some(viewport) = viewport {
        if let Some((x, y, _, _)) = viewport.source {
            // ChromiumOS uses wl_fixed_to_int() for the source offset before
            // converting buffer damage, so retain its truncation semantics for
            // fractional fixed-point offsets.
            offset_x = f64::from(x / 256);
            offset_y = f64::from(y / 256);
        }
        if let Some((destination_width, destination_height)) = viewport.destination {
            if destination_width <= 0 || destination_height <= 0 {
                return DamageRect::new(0, 0, buffer_width.max(1), buffer_height.max(1));
            }
            scale_x *= contents_width / f64::from(destination_width);
            scale_y *= contents_height / f64::from(destination_height);
            if let Some((_, _, source_width, source_height)) = viewport.source {
                let source_width = f64::from(source_width) / 256.0;
                let source_height = f64::from(source_height) / 256.0;
                if source_width <= 0.0 || source_height <= 0.0 {
                    return DamageRect::new(0, 0, buffer_width.max(1), buffer_height.max(1));
                }
                scale_x *= source_width / contents_width;
                scale_y *= source_height / contents_height;
            }
        }
    }

    // ChromiumOS expands both edges by one buffer pixel before applying the
    // scale/viewport transform. This prevents filtering at a fractional
    // boundary from reading an undamaged source pixel.
    let x_start = clamp_damage_edge(i64::from(rect.x).saturating_sub(1));
    let x_end = clamp_damage_edge(
        i64::from(rect.x)
            .saturating_add(i64::from(rect.width))
            .saturating_add(1),
    );
    let y_start = clamp_damage_edge(i64::from(rect.y).saturating_sub(1));
    let y_end = clamp_damage_edge(
        i64::from(rect.y)
            .saturating_add(i64::from(rect.height))
            .saturating_add(1),
    );

    let x0 = (x_start as f64 - offset_x) / scale_x;
    let x1 = (x_end as f64 - offset_x) / scale_x;
    let y0 = (y_start as f64 - offset_y) / scale_y;
    let y1 = (y_end as f64 - offset_y) / scale_y;

    // Attach/offset positions the new buffer relative to the previous
    // surface contents. It does not change the buffer-coordinate damage
    // mapping; ChromiumOS' implementation likewise omits that offset here.
    let left = x0.min(x1).trunc().max(DAMAGE_MIN as f64);
    let top = y0.min(y1).trunc().max(DAMAGE_MIN as f64);
    let right = x0.max(x1).ceil().min(DAMAGE_MAX as f64);
    let bottom = y0.max(y1).ceil().min(DAMAGE_MAX as f64);
    DamageRect::new(
        saturating_i32(left),
        saturating_i32(top),
        saturating_i32((right - left).max(1.0)),
        saturating_i32((bottom - top).max(1.0)),
    )
}

fn full_surface_damage(
    surface: &SurfaceState,
    buffer_width: i32,
    buffer_height: i32,
) -> DamageRect {
    let width = buffer_width.max(1);
    let height = buffer_height.max(1);
    if let Some(viewport) = surface.viewport.filter(|viewport| !viewport.is_identity()) {
        if let Some((destination_width, destination_height)) = viewport.destination {
            return DamageRect::new(0, 0, destination_width, destination_height);
        }
        if let Some((_, _, source_width, source_height)) = viewport.source {
            return DamageRect::new(
                0,
                0,
                (source_width / 256).max(1),
                (source_height / 256).max(1),
            );
        }
    }

    if surface.current_buffer_transform != 0 {
        let (width, height) = logical_transformed_extent(
            surface.current_buffer_transform,
            width,
            height,
            surface.current_buffer_scale,
        );
        return DamageRect::new(0, 0, width, height);
    }

    DamageRect::new(
        0,
        0,
        ((f64::from(width) / f64::from(surface.current_buffer_scale.max(1))).ceil())
            .min(f64::from(i32::MAX)) as i32,
        ((f64::from(height) / f64::from(surface.current_buffer_scale.max(1))).ceil())
            .min(f64::from(i32::MAX)) as i32,
    )
}

fn queue_surface_damage(
    ctx: &mut Context,
    surface_id: u32,
    surface_damage: &[DamageRect],
    buffer_damage: &[DamageRect],
    surface_state: &SurfaceState,
    force_full_damage: bool,
) {
    let Some(host_surface_id) = ctx.shadow_table.get_host_id(surface_id) else {
        return;
    };
    let (buffer_width, buffer_height) = surface_state
        .current_buffer_id
        .and_then(|buffer_id| {
            ctx.buffers
                .get(&buffer_id)
                .or_else(|| ctx.retired_buffers.get(&buffer_id))
                .map(|buffer| (buffer.width, buffer.height))
                .or_else(|| {
                    ctx.native_buffer_sizes
                        .get(&buffer_id)
                        .copied()
                        .or_else(|| {
                            ctx.shadow_table
                                .get_host_id(buffer_id)
                                .and_then(|host_id| ctx.native_buffer_sizes.get(&host_id).copied())
                        })
                })
        })
        .unwrap_or((1, 1));

    let full_damage = full_surface_damage(surface_state, buffer_width, buffer_height);
    let mut mapped = Vec::with_capacity(surface_damage.len() + buffer_damage.len() + 1);
    if force_full_damage {
        mapped.push(full_damage);
    }
    mapped.extend(surface_damage.iter().copied().map(map_surface_damage));
    mapped.extend(
        buffer_damage
            .iter()
            .map(|rect| map_buffer_damage(*rect, surface_state, buffer_width, buffer_height)),
    );

    for rect in mapped {
        if rect.width <= 0 || rect.height <= 0 {
            continue;
        }
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_i32(rect.x);
        builder.write_i32(rect.y);
        builder.write_i32(rect.width);
        builder.write_i32(rect.height);
        let Ok(message) = builder.try_build_message(host_surface_id, REQ_DAMAGE) else {
            log::warn!("Dropping an oversized wl_surface.damage request");
            continue;
        };
        ctx.client_to_host_queue.push((message, Vec::new()));
    }
}

fn queue_surface_commit(ctx: &mut Context, surface_id: u32) {
    let Some(host_surface_id) = ctx.shadow_table.get_host_id(surface_id) else {
        return;
    };
    let builder = crate::wire::MessageBuilder::new();
    let Ok(message) = builder.try_build_message(host_surface_id, REQ_COMMIT) else {
        log::warn!(
            "Unable to encode wl_surface.commit for host surface {}",
            host_surface_id
        );
        return;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
}

#[derive(Clone, Copy)]
struct SurfaceCommitRollback {
    pending_buffer_id: Option<Option<u32>>,
    previous_buffer_id: Option<u32>,
    previous_buffer_scale: i32,
    previous_buffer_transform: i32,
    previous_offset: (i32, i32),
    previous_viewport: Option<ViewportState>,
    pending_buffer_scale: Option<i32>,
    pending_buffer_transform: Option<i32>,
    pending_offset: Option<(i32, i32)>,
    pending_attach_offset: Option<(i32, i32)>,
    pending_viewport: Option<Option<ViewportState>>,
}

fn restore_failed_surface_commit(
    ctx: &mut Context,
    surface_id: u32,
    rollback: SurfaceCommitRollback,
    surface_damage: Vec<DamageRect>,
    buffer_damage: Vec<DamageRect>,
) {
    let Some(surface_state) = ctx.surfaces.get_mut(&surface_id) else {
        return;
    };
    if rollback.pending_buffer_id.is_some() {
        surface_state.current_buffer_id = rollback.previous_buffer_id;
        surface_state.pending_buffer_id = rollback.pending_buffer_id;
    }
    surface_state.current_buffer_scale = rollback.previous_buffer_scale;
    surface_state.current_buffer_transform = rollback.previous_buffer_transform;
    surface_state.current_offset = rollback.previous_offset;
    surface_state.viewport = rollback.previous_viewport;
    surface_state.pending_buffer_scale = rollback.pending_buffer_scale;
    surface_state.pending_buffer_transform = rollback.pending_buffer_transform;
    surface_state.pending_offset = rollback.pending_offset;
    surface_state.pending_attach_offset = rollback.pending_attach_offset;
    surface_state.pending_viewport = rollback.pending_viewport;
    surface_state.pending_surface_damage = surface_damage;
    surface_state.pending_buffer_damage = buffer_damage;
}

impl WlSurfaceHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let wl_surface_guest_id = ctx.last_sender_id;
        let host_surface_destroy_queued =
            ctx.shadow_table.get_host_id(wl_surface_guest_id).is_some();
        // Keep the references owned by this surface before removing its
        // state. A submitted buffer may have been detached from another
        // surface and still be waiting for wl_buffer.release; only buffers
        // that this host surface actually held may be retired merely because
        // its destructor is ordered ahead of the buffer destructor.
        let destroyed_surface_buffers: HashSet<u32> = ctx
            .surfaces
            .get(&wl_surface_guest_id)
            .into_iter()
            .flat_map(|surface| {
                surface
                    .current_buffer_id
                    .into_iter()
                    .chain(surface.pending_buffer_id.into_iter().flatten())
            })
            .collect();
        // Clean up any host-side zaura_surface we created for this wl_surface.
        if let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(wl_surface_guest_id) {
            if let Some(zaura_surface_host_id) =
                ctx.wl_surface_to_zaura_surface.remove(&wl_surface_host_id)
            {
                let zaura_surface_version = ctx
                    .shadow_table
                    .host_object_version(zaura_surface_host_id)
                    .unwrap_or(ctx.host_zaura_shell_version);
                if zaura_surface_version >= 38 {
                    let builder = crate::wire::MessageBuilder::new();
                    let msg = builder.build_message(zaura_surface_host_id, REQ_RELEASE);
                    ctx.client_to_host_queue.push((msg, Vec::new()));
                    ctx.shadow_table
                        .mark_pending_destroy_host(zaura_surface_host_id);
                } else {
                    // zaura_surface.release was introduced in v38. On older
                    // hosts the object has no destructor request. Retire its
                    // dispatch metadata immediately so stale events cannot
                    // reach a destroyed guest surface, while retaining the
                    // numeric ID reservation until connection teardown.
                    ctx.shadow_table
                        .retire_host_interface(zaura_surface_host_id);
                }
            }

            // The generated dispatcher cannot express the ordering required
            // by the aura-shell integration: a queued zaura_surface.release
            // must reach the host before the paired wl_surface is destroyed.
            // Queue the host destructor explicitly and consume the guest
            // request locally.
            let builder = crate::wire::MessageBuilder::new();
            let Ok(message) = builder.try_build_message(wl_surface_host_id, REQ_DESTROY) else {
                log::warn!(
                    "Unable to encode wl_surface.destroy for host surface {}",
                    wl_surface_host_id
                );
                return Action::Drop;
            };
            ctx.client_to_host_queue.push((message, Vec::new()));
        }
        ctx.surfaces.remove(&ctx.last_sender_id);
        crate::handler::shm::clear_submitted_buffers_after_surface_destroy(
            ctx,
            &destroyed_surface_buffers,
        );
        // Pending-only objects do not receive a host release event. This
        // collector is safe even without a host surface mapping because it
        // only handles buffers that were never submitted.
        crate::handler::shm::collect_uncommitted_retired_buffers(ctx);
        crate::handler::shm::collect_deferred_native_buffers(ctx);
        if host_surface_destroy_queued {
            // The host surface destructor was queued above. Any native
            // dma-buf that was deferred solely for this surface can now be
            // retired in the same ordered host stream, even if the host does
            // not emit a separate wl_buffer.release on surface teardown.
            crate::handler::shm::collect_deferred_native_buffers_after_surface_destroy(
                ctx,
                &destroyed_surface_buffers,
            );
            crate::handler::shm::collect_retired_buffers_after_surface_destroy(
                ctx,
                &destroyed_surface_buffers,
            );
        }
        crate::handler::shm::collect_retired_buffers(ctx);
        ctx.viewport_to_wl_surface
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        // A client may destroy the focused surface before the compositor's
        // delayed wl_keyboard.leave reaches the proxy. Retire the local
        // text-input focus immediately; otherwise a subsequent commit can
        // reactivate the host IME against a dead surface.
        ctx.active_surface_for_seat
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        let mut text_inputs_to_update = Vec::new();
        for (guest_text_input_id, state) in ctx.text_inputs.iter_mut() {
            if state.active_surface == Some(wl_surface_guest_id) {
                state.active_surface = None;
                crate::handler::text_input::invalidate_for_keyboard_focus(state);
                text_inputs_to_update.push(*guest_text_input_id);
            }
        }
        // Surface destruction can race the host's wl_keyboard.leave event.
        // Emit the mandatory v3 leave now, while the guest surface ID is
        // still valid. The later host leave is deliberately treated as stale
        // and must not emit a duplicate event.
        for guest_text_input_id in &text_inputs_to_update {
            let mut builder = crate::wire::MessageBuilder::new();
            builder.write_u32(wl_surface_guest_id);
            let message = builder.build_message(*guest_text_input_id, 1);
            ctx.host_to_client_queue.push((message, Vec::new()));
        }
        for guest_text_input_id in text_inputs_to_update {
            crate::handler::text_input::update_host_activation(ctx, guest_text_input_id);
        }
        // Do not retain a dead surface as the current focus of an individual
        // keyboard. A later delayed leave is still allowed to clear the
        // keyboard's physical state; if the keyboard entered a new surface in
        // the meantime, on_enter will have installed that newer surface and
        // the leave path will preserve it.
        // The delayed host wl_keyboard.leave may be rejected as stale once
        // the surface enters pending-destroy state. Retire all proxy-owned
        // physical-key/IME bookkeeping now so a later keyboard or text-input
        // event cannot synthesize input against this dead surface.
        let destroyed_keyboard_ids: Vec<_> = ctx
            .keyboard_active_surfaces
            .iter()
            .filter_map(|(keyboard_id, surface_id)| {
                (*surface_id == wl_surface_guest_id).then_some(*keyboard_id)
            })
            .collect();
        ctx.keyboard_active_surfaces
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        for keyboard_id in destroyed_keyboard_ids {
            ctx.keyboard_pressed_keys.remove(&keyboard_id);
            ctx.keyboard_backspace_repeat_cancelled.remove(&keyboard_id);
            ctx.keyboard_event_times.remove(&keyboard_id);
            ctx.keyboard_ime_suppressed_keys.remove(&keyboard_id);
            ctx.keyboard_forwarded_keys.remove(&keyboard_id);
            ctx.keyboard_keysym_forwarded_keys.remove(&keyboard_id);
        }
        // xdg objects are separate guest objects, but both maps resolve back
        // to this wl_surface. Remove stale links now so a later client ID
        // reuse cannot associate a new toplevel with the destroyed surface.
        ctx.xdg_surface_to_wl_surface
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        ctx.xdg_toplevel_to_wl_surface
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        // The host destructor is queued above. Retain the numeric mapping
        // until the host acknowledges it with wl_display.delete_id so the
        // guest can safely reuse the ID only after that acknowledgement.
        ctx.shadow_table.mark_pending_destroy(wl_surface_guest_id);
        Action::Drop
    }

    fn on_attach(&mut self, ctx: &mut Context, buffer: u32, x: i32, y: i32) -> Action {
        let surface_id = ctx.last_sender_id;
        let object_version = ctx.shadow_table.guest_object_version(surface_id);
        if object_version.is_some_and(|version| version != u32::MAX && version >= 5)
            && (x != 0 || y != 0)
        {
            // wl_surface v5 made non-zero attach offsets a protocol error;
            // clients must use wl_surface.offset instead. Do not let an
            // invalid request reach the host compositor.
            log::warn!(
                "Rejecting non-zero wl_surface.attach offset ({}, {}) for v5+ surface {}",
                x,
                y,
                surface_id
            );
            queue_protocol_error(
                ctx,
                surface_id,
                3,
                "wl_surface.attach offset is non-zero for version 5+",
            );
            return Action::Drop;
        }
        let surface_state = ctx.surfaces.entry(surface_id).or_default();
        surface_state.pending_buffer_id = Some(if buffer == 0 { None } else { Some(buffer) });
        surface_state.pending_attach_offset = None;
        // Since wl_surface version 5, attach's x/y arguments are ignored
        // (and non-zero values were rejected above). Keep them out of the
        // legacy pending-offset slot so a legal attach(buffer, 0, 0) cannot
        // reset a previously committed wl_surface.offset.
        let legacy_attach_offset =
            object_version.is_none_or(|version| version < 5 || version == u32::MAX);
        if buffer != 0 && legacy_attach_offset {
            surface_state.pending_attach_offset = Some((x, y));
        }
        // A pending native buffer can be destroyed before the commit that
        // attached it. Replacing that pending attach (including attach(NULL))
        // means the compositor will never emit wl_buffer.release for the
        // superseded object, so retire its host proxy after the new attach
        // request in the ordered queue.
        crate::handler::shm::collect_uncommitted_retired_buffers(ctx);
        crate::handler::shm::collect_deferred_native_buffers(ctx);
        Action::Forward
    }

    fn on_damage(&mut self, ctx: &mut Context, x: i32, y: i32, width: i32, height: i32) -> Action {
        ctx.surfaces
            .entry(ctx.last_sender_id)
            .or_default()
            .pending_surface_damage
            .push(DamageRect::new(x, y, width, height));
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
        ctx.surfaces
            .entry(ctx.last_sender_id)
            .or_default()
            .pending_buffer_damage
            .push(DamageRect::new(x, y, width, height));
        // ChromiumOS Sommelier implements damage_buffer by translating it to
        // wl_surface.damage because the host compositor may only expose v3.
        Action::Drop
    }

    fn on_set_buffer_scale(&mut self, ctx: &mut Context, scale: i32) -> Action {
        if scale <= 0 {
            log::warn!("Rejecting invalid wl_surface buffer scale {}", scale);
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                0,
                "invalid wl_surface buffer scale",
            );
            return Action::Drop;
        }
        ctx.surfaces
            .entry(ctx.last_sender_id)
            .or_default()
            .pending_buffer_scale = Some(scale);
        Action::Forward
    }

    fn on_set_buffer_transform(&mut self, ctx: &mut Context, transform: i32) -> Action {
        if !(0..=7).contains(&transform) {
            log::warn!(
                "Rejecting invalid wl_surface buffer transform {}",
                transform
            );
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                1,
                "invalid wl_surface buffer transform",
            );
            return Action::Drop;
        }
        ctx.surfaces
            .entry(ctx.last_sender_id)
            .or_default()
            .pending_buffer_transform = Some(transform);
        Action::Forward
    }

    fn on_offset(&mut self, _ctx: &mut Context, _x: i32, _y: i32) -> Action {
        let ctx = _ctx;
        ctx.surfaces
            .entry(ctx.last_sender_id)
            .or_default()
            .pending_offset = Some((_x, _y));
        Action::Forward
    }

    fn on_commit(&mut self, ctx: &mut Context) -> Action {
        let surface_id = ctx.last_sender_id;

        let (rollback, surface_damage, buffer_damage, full_mapping) =
            ctx.surfaces.get_mut(&surface_id).map_or(
                (
                    SurfaceCommitRollback {
                        pending_buffer_id: None,
                        previous_buffer_id: None,
                        previous_buffer_scale: 1,
                        previous_buffer_transform: 0,
                        previous_offset: (0, 0),
                        previous_viewport: None,
                        pending_buffer_scale: None,
                        pending_buffer_transform: None,
                        pending_offset: None,
                        pending_attach_offset: None,
                        pending_viewport: None,
                    },
                    Vec::new(),
                    Vec::new(),
                    false,
                ),
                |surface_state| {
                    let pending_buffer_id = surface_state.pending_buffer_id.take();
                    let previous_buffer_id = surface_state.current_buffer_id;
                    let previous_buffer_scale = surface_state.current_buffer_scale;
                    let previous_buffer_transform = surface_state.current_buffer_transform;
                    let previous_offset = surface_state.current_offset;
                    let previous_viewport = surface_state.viewport;
                    if let Some(buffer_id) = pending_buffer_id {
                        surface_state.current_buffer_id = buffer_id;
                    }
                    let pending_buffer_scale = surface_state.pending_buffer_scale.take();
                    if let Some(scale) = pending_buffer_scale {
                        surface_state.current_buffer_scale = scale;
                    }
                    let pending_buffer_transform = surface_state.pending_buffer_transform.take();
                    if let Some(transform) = pending_buffer_transform {
                        surface_state.current_buffer_transform = transform;
                    }
                    let pending_offset = surface_state.pending_offset.take();
                    let pending_attach_offset = surface_state.pending_attach_offset.take();
                    if let Some(offset) = pending_offset {
                        surface_state.current_offset = offset;
                    } else if let Some(offset) = pending_attach_offset {
                        surface_state.current_offset = offset;
                    }
                    let pending_viewport = surface_state.pending_viewport.take();
                    if let Some(viewport) = pending_viewport {
                        surface_state.viewport = viewport;
                    }
                    let full_mapping = surface_state.current_buffer_scale != 1
                        || surface_state.current_buffer_transform != 0
                        || surface_state.current_offset != (0, 0)
                        || surface_state
                            .viewport
                            .is_some_and(|viewport| !viewport.is_identity());
                    (
                        SurfaceCommitRollback {
                            pending_buffer_id,
                            previous_buffer_id,
                            previous_buffer_scale,
                            previous_buffer_transform,
                            previous_offset,
                            previous_viewport,
                            pending_buffer_scale,
                            pending_buffer_transform,
                            pending_offset,
                            pending_attach_offset,
                            pending_viewport,
                        },
                        std::mem::take(&mut surface_state.pending_surface_damage),
                        std::mem::take(&mut surface_state.pending_buffer_damage),
                        full_mapping,
                    )
                },
            );
        // A commit with no new attach reuses the currently committed buffer
        // (for damage-only commits). An explicit attach(NULL), represented by
        // `Some(None)`, is the one case that deliberately commits no buffer.
        let pending_buffer_state = rollback.pending_buffer_id;
        let previous_buffer_id = rollback.previous_buffer_id;
        let commit_buffer_id = pending_buffer_state.flatten().or_else(|| {
            pending_buffer_state
                .is_none()
                .then_some(previous_buffer_id)
                .flatten()
        });

        // The viewporter protocol permits fractional source rectangles only
        // when a destination size is also set. If the source is fractional
        // and the destination is unset, the compositor must raise
        // wp_viewport.bad_size when this double-buffered state is applied.
        // Report it against the viewport object, if it is still present, and
        // do not forward a commit that would otherwise apply invalid state.
        if let Some(surface_state) = ctx.surfaces.get(&surface_id) {
            if let Some((_, _, source_width, source_height)) =
                surface_state.viewport.and_then(|viewport| viewport.source)
            {
                if surface_state
                    .viewport
                    .is_some_and(|viewport| viewport.destination.is_none())
                    && (source_width % 256 != 0 || source_height % 256 != 0)
                {
                    if let Some((&viewport_id, _)) = ctx
                        .viewport_to_wl_surface
                        .iter()
                        .find(|(_, &owner)| owner == surface_id)
                    {
                        queue_protocol_error(
                            ctx,
                            viewport_id,
                            1,
                            "fractional wp_viewport source requires a destination",
                        );
                    }
                    restore_failed_surface_commit(
                        ctx,
                        surface_id,
                        rollback,
                        surface_damage,
                        buffer_damage,
                    );
                    return Action::Drop;
                }
            }
        }

        // Keep a snapshot of the committed surface state for damage
        // translation. The buffer copy below can clear `needs_full_copy`, so
        // the pre-copy value is retained separately for the host damage
        // request.
        let surface_snapshot = ctx.surfaces.get(&surface_id).cloned();
        let needs_full_damage = commit_buffer_id
            .or_else(|| {
                surface_snapshot
                    .as_ref()
                    .and_then(|surface| surface.current_buffer_id)
            })
            .and_then(|buffer_id| {
                ctx.buffers
                    .get(&buffer_id)
                    .or_else(|| ctx.retired_buffers.get(&buffer_id))
                    .map(|buffer| buffer.needs_full_copy)
            })
            .unwrap_or(false);

        // A SHM-backed buffer is copied into host storage before the host
        // commit is queued. Forwarding the commit first would let the host
        // compositor sample stale or uninitialised pixels if dma-buf sync,
        // mmap, or GBM mapping fails. A failed copy leaves `needs_full_copy`
        // set so a later commit retries the complete image.
        let mut commit_ready = true;
        if let Some(buffer_id) = commit_buffer_id {
            let allocator = ctx.allocator.as_ref();
            let buffer = if ctx.buffers.contains_key(&buffer_id) {
                ctx.buffers.get_mut(&buffer_id)
            } else {
                ctx.retired_buffers.get_mut(&buffer_id)
            };
            if let Some(buffer) = buffer {
                let mut copy_ok = false;
                let pool = &buffer.pool;
                if let Ok(inner) = pool.inner.read() {
                    if !inner.client_ptr.is_null() && inner.client_ptr != libc::MAP_FAILED {
                        let src_ptr = inner.client_ptr as *const u8;
                        let format = buffer.format;
                        let offset = buffer.offset;
                        let buffer_width = buffer.width;
                        let buffer_height = buffer.height;
                        let source_stride = buffer.stride;
                        let needs_full_copy = buffer.needs_full_copy;
                        let mut sync_guard = if buffer.dmabuf_sync {
                            let guard = DmabufWriteSync::begin(
                                ctx.virtwayland_channel.as_ref(),
                                buffer.dmabuf_fd.as_ref().map(AsRawFd::as_raw_fd),
                            );
                            if guard.is_none() {
                                log::warn!("VirtWL dma-buf buffer has no channel or backing fd");
                            }
                            guard
                        } else {
                            None
                        };
                        let sync_started = !buffer.dmabuf_sync || sync_guard.is_some();
                        if sync_started {
                            copy_ok = if !buffer.dest_ptr.is_null() {
                                copy_damage_to_ptr(
                                    format,
                                    offset,
                                    buffer_width,
                                    buffer_height,
                                    source_stride,
                                    needs_full_copy,
                                    src_ptr,
                                    inner.size,
                                    buffer.dest_ptr,
                                    buffer.dest_size,
                                    buffer.bo_stride as usize,
                                    buffer.dmabuf_plane1_offset,
                                    buffer.dmabuf_plane1_stride,
                                    &surface_damage,
                                    &buffer_damage,
                                    full_mapping,
                                )
                            } else if let (Some(allocator), Some(bo)) =
                                (allocator, buffer.bo.as_mut())
                            {
                                match (u32::try_from(buffer_width), u32::try_from(buffer_height)) {
                                    (Ok(width), Ok(height)) => {
                                        match bo.map_mut(
                                            &allocator.device,
                                            0,
                                            0,
                                            width,
                                            height,
                                            |mapped| {
                                                copy_damage_to_ptr(
                                                    format,
                                                    offset,
                                                    buffer_width,
                                                    buffer_height,
                                                    source_stride,
                                                    needs_full_copy,
                                                    src_ptr,
                                                    inner.size,
                                                    mapped.buffer_mut().as_mut_ptr(),
                                                    mapped.buffer().len(),
                                                    mapped.stride() as usize,
                                                    buffer.dmabuf_plane1_offset,
                                                    buffer.dmabuf_plane1_stride,
                                                    &surface_damage,
                                                    &buffer_damage,
                                                    full_mapping,
                                                )
                                            },
                                        ) {
                                            Ok(Ok(copy_ok)) => copy_ok,
                                            Ok(Err(error)) => {
                                                log::warn!("GBM BO map failed: {}", error);
                                                false
                                            }
                                            Err(_) => {
                                                log::warn!(
                                                    "GBM BO belongs to a different allocator device"
                                                );
                                                false
                                            }
                                        }
                                    }
                                    (Err(_), _) => {
                                        log::warn!("SHM buffer width cannot be represented as u32");
                                        false
                                    }
                                    (_, Err(_)) => {
                                        log::warn!(
                                            "SHM buffer height cannot be represented as u32"
                                        );
                                        false
                                    }
                                }
                            } else {
                                // No destination mapping is available. This can
                                // happen only for a malformed or unsupported
                                // allocation path; leave the host buffer untouched.
                                false
                            };
                        }
                        if let Some(guard) = sync_guard.take() {
                            if !guard.end() {
                                copy_ok = false;
                            }
                        }
                        if !copy_ok {
                            log::warn!(
                                "Deferring SHM commit with an invalid or unmappable destination"
                            );
                        } else {
                            buffer.needs_full_copy = false;
                            debug!(
                                "Copied damaged SHM buffer: dst_stride={}, src_stride={}, \
                                 width={}, height={}, format={:#010x}, buffer.offset={}, \
                                 pool_size={}, dest_size={}",
                                buffer.bo_stride,
                                buffer.stride,
                                buffer.width,
                                buffer.height,
                                buffer.format,
                                buffer.offset,
                                inner.size,
                                buffer.dest_size
                            );
                        }
                    } else {
                        log::warn!("SHM pool has no valid client mapping");
                    }
                } else {
                    log::warn!("Unable to read SHM pool while committing buffer");
                }
                if !copy_ok {
                    // Preserve a complete retry even when this was a
                    // damage-only commit. Otherwise a transient sync/map
                    // failure would discard the only dirty region.
                    buffer.needs_full_copy = true;
                    commit_ready = false;
                }
            } else {
                // Buffers created through the native linux-dmabuf path do
                // not have a local SHM copy state. Their attach is already
                // forwarded and the host commit can proceed normally.
            }
        }

        if commit_ready {
            // Damage requests are double-buffered. Emit the translated host
            // requests immediately before the commit so the host sees exactly
            // the same pending damage set as the guest compositor.
            if let Some(surface_state) = surface_snapshot {
                // Offset-aware damage translation is not implemented yet. A
                // partial surface-local damage rectangle would describe the
                // wrong host region after the buffer is repositioned, while
                // the local copy path conservatively updates the complete
                // buffer. Keep the host damage equally conservative until an
                // offset map exists.
                let force_full_damage = needs_full_damage
                    || surface_state.current_offset != (0, 0)
                    || (full_mapping && surface_damage.is_empty() && buffer_damage.is_empty());
                queue_surface_damage(
                    ctx,
                    surface_id,
                    &surface_damage,
                    &buffer_damage,
                    &surface_state,
                    force_full_damage,
                );
            }
            // The generated dispatcher appends context-queued messages after
            // the forwarded packet. Queue the commit itself so translated
            // damage is guaranteed to reach the host before the commit that
            // consumes it.
            queue_surface_commit(ctx, surface_id);

            if let Some(buffer_id) = commit_buffer_id {
                // The pending attach has now been consumed by a successful
                // commit, so a fresh compositor-use interval begins. Clear
                // the previous release edge only here; doing it in attach
                // would make an attach-without-commit leak when the guest
                // destroys the otherwise-idle buffer.
                ctx.released_host_buffers.remove(&buffer_id);
                ctx.submitted_buffers.insert(buffer_id);
                if let Some(buffer) = ctx.buffers.get_mut(&buffer_id) {
                    // A commit without a new attach still submits the current
                    // buffer. A prior wl_buffer.release only completed the
                    // previous compositor-use interval; this commit starts a
                    // new one and must make a later guest destroy defer
                    // cleanup again.
                    buffer.host_released = false;
                } else if let Some(buffer) = ctx.retired_buffers.get_mut(&buffer_id) {
                    buffer.host_released = false;
                }
            }
        } else {
            log::warn!(
                "Skipping host wl_surface.commit for {} because its SHM copy failed",
                surface_id
            );
        }
        if !commit_ready {
            restore_failed_surface_commit(ctx, surface_id, rollback, surface_damage, buffer_damage);
        }
        crate::handler::shm::collect_retired_buffers(ctx);
        Action::Drop
    }
}

impl WlRegionHandler for CompositorHandler {}
impl WlSubcompositorHandler for CompositorHandler {}
impl WlSubsurfaceHandler for CompositorHandler {}

impl crate::protocols::viewporter::wp_viewporter::WpViewporterHandler for CompositorHandler {
    fn on_get_viewport(&mut self, ctx: &mut Context, id: u32, surface: u32) -> Action {
        if ctx
            .viewport_to_wl_surface
            .values()
            .any(|&owner| owner == surface)
        {
            log::warn!("Rejecting duplicate wp_viewport for wl_surface {}", surface);
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                0,
                "wl_surface already has a wp_viewport",
            );
            return Action::Drop;
        }
        ctx.viewport_to_wl_surface.insert(id, surface);
        ctx.surfaces.entry(surface).or_default().pending_viewport =
            Some(Some(ViewportState::new()));
        Action::Forward
    }
}

impl crate::protocols::viewporter::wp_viewport::WpViewportHandler for CompositorHandler {
    fn on_set_source(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let Some(&surface_id) = ctx.viewport_to_wl_surface.get(&ctx.last_sender_id) else {
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                3,
                "wp_viewport has no associated wl_surface",
            );
            return Action::Drop;
        };
        let unset = (x, y, width, height) == (-256, -256, -256, -256);
        if !unset && (x < 0 || y < 0 || width <= 0 || height <= 0) {
            log::warn!("Rejecting invalid wp_viewport source rectangle");
            queue_protocol_error(ctx, ctx.last_sender_id, 0, "invalid wp_viewport source");
            return Action::Drop;
        }
        let surface = ctx.surfaces.entry(surface_id).or_default();
        let mut viewport = surface
            .pending_viewport
            .and_then(|pending| pending)
            .or(surface.viewport)
            .unwrap_or_default();
        viewport.source = (!unset).then_some((x, y, width, height));
        surface.pending_viewport = Some(Some(viewport));
        Action::Forward
    }

    fn on_set_destination(&mut self, ctx: &mut Context, width: i32, height: i32) -> Action {
        let Some(&surface_id) = ctx.viewport_to_wl_surface.get(&ctx.last_sender_id) else {
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                3,
                "wp_viewport has no associated wl_surface",
            );
            return Action::Drop;
        };
        let unset = width == -1 && height == -1;
        if !unset && (width <= 0 || height <= 0) {
            log::warn!(
                "Rejecting invalid wp_viewport destination {}x{}",
                width,
                height
            );
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                0,
                "invalid wp_viewport destination",
            );
            return Action::Drop;
        }
        let surface = ctx.surfaces.entry(surface_id).or_default();
        let mut viewport = surface
            .pending_viewport
            .and_then(|pending| pending)
            .or(surface.viewport)
            .unwrap_or_default();
        viewport.destination = (!unset).then_some((width, height));
        surface.pending_viewport = Some(Some(viewport));
        Action::Forward
    }

    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let viewport_id = ctx.last_sender_id;
        if let Some(surface_id) = ctx.viewport_to_wl_surface.remove(&viewport_id) {
            if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                surface.pending_viewport = Some(None);
            }
        }
        Action::Forward
    }
}

// --- XDG Shell → wl_surface tracking for zaura_shell integration ---
//
// ChromeOS needs a zaura_surface (from zaura_shell) to set the application ID
// that the shelf uses for icon matching. But the app ID arrives via
// xdg_toplevel::set_app_id, which doesn't carry a wl_surface reference.
//
// We bridge this gap by tracking the chain:
//   xdg_wm_base::get_xdg_surface(xdg_surface, wl_surface)
//   xdg_surface::get_toplevel(xdg_toplevel)
// so that when set_app_id fires on an xdg_toplevel, we can resolve back to
// the underlying wl_surface and create/reuse a host zaura_surface on it.

impl crate::protocols::xdg_shell::xdg_wm_base::XdgWmBaseHandler for CompositorHandler {
    fn on_get_xdg_surface(&mut self, ctx: &mut Context, id: u32, surface: u32) -> Action {
        ctx.xdg_surface_to_wl_surface.insert(id, surface);
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_surface_id = ctx.last_sender_id;
        if let Some(wl_surface_id) = ctx.xdg_surface_to_wl_surface.remove(&xdg_surface_id) {
            // A malformed client can destroy xdg_surface before its
            // xdg_toplevel. Do not leave a stale toplevel→surface association
            // that could apply a later app_id to an unrelated surface after ID
            // reuse.
            ctx.xdg_toplevel_to_wl_surface
                .retain(|_, surface_id| *surface_id != wl_surface_id);
        }
        Action::Forward
    }

    fn on_get_toplevel(&mut self, ctx: &mut Context, id: u32) -> Action {
        let xdg_surface_id = ctx.last_sender_id;
        if let Some(&wl_surface_id) = ctx.xdg_surface_to_wl_surface.get(&xdg_surface_id) {
            ctx.xdg_toplevel_to_wl_surface.insert(id, wl_surface_id);
        }
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_toplevel_id = ctx.last_sender_id;
        ctx.xdg_toplevel_to_wl_surface.remove(&xdg_toplevel_id);
        Action::Forward
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
        let formatted_app_id = native_wayland_app_id(&ctx.vm_identifier, app_id);
        if !wayland_string_fits_message(&formatted_app_id) {
            log::warn!(
                "Dropping oversized application ID for xdg_toplevel {} ({} bytes)",
                xdg_toplevel_id,
                formatted_app_id.len()
            );
            return Action::Drop;
        }

        // ChromiumOS namespaces the host xdg_toplevel app ID itself, not only
        // the optional zaura_surface metadata. Forward the request manually so
        // the guest never exposes a conflicting unqualified app ID to Exo.
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_string(&formatted_app_id);
        let Ok(msg) = builder.try_build_message(xdg_toplevel_host_id, REQ_SET_APP_ID) else {
            log::warn!(
                "Dropping oversized namespaced app ID for xdg_toplevel {}",
                xdg_toplevel_id
            );
            return Action::Drop;
        };
        ctx.client_to_host_queue.push((msg, Vec::new()));

        // Resolve xdg_toplevel → wl_surface (guest) → wl_surface (host).
        if let Some(&wl_surface_guest_id) = ctx.xdg_toplevel_to_wl_surface.get(&xdg_toplevel_id) {
            if let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(wl_surface_guest_id) {
                // Lazily create a host zaura_surface for this wl_surface, or reuse
                // an existing one. This avoids overhead for surfaces that never
                // set an app ID (subsurfaces, popups, etc.).
                let zaura_surface_host_id = if let Some(&existing_zaura_id) =
                    ctx.wl_surface_to_zaura_surface.get(&wl_surface_host_id)
                {
                    existing_zaura_id
                } else if let Some(zaura_shell_host_id) = ctx.host_zaura_shell_id {
                    let zaura_surface_host_id = ctx.shadow_table.allocate_host_id();
                    ctx.shadow_table.track_host_interface_with_version(
                        zaura_surface_host_id,
                        "zaura_surface".to_string(),
                        ctx.host_zaura_shell_version,
                    );

                    let mut builder = crate::wire::MessageBuilder::new();
                    builder.write_u32(zaura_surface_host_id);
                    builder.write_u32(wl_surface_host_id);

                    let Ok(msg) =
                        builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_SURFACE)
                    else {
                        log::warn!(
                            "Dropping aura surface request for xdg_toplevel {}",
                            xdg_toplevel_id
                        );
                        return Action::Drop;
                    };
                    ctx.client_to_host_queue.push((msg, Vec::new()));

                    ctx.wl_surface_to_zaura_surface
                        .insert(wl_surface_host_id, zaura_surface_host_id);

                    zaura_surface_host_id
                } else {
                    0
                };

                let zaura_surface_version = ctx
                    .shadow_table
                    .host_object_version(zaura_surface_host_id)
                    .unwrap_or(ctx.host_zaura_shell_version);
                if zaura_surface_host_id != 0 && zaura_surface_version >= 5 {
                    let mut builder = crate::wire::MessageBuilder::new();
                    builder.write_string(&formatted_app_id);

                    let Ok(msg) =
                        builder.try_build_message(zaura_surface_host_id, REQ_SET_APPLICATION_ID)
                    else {
                        log::warn!(
                            "Dropping oversized aura application ID for xdg_toplevel {}",
                            xdg_toplevel_id
                        );
                        return Action::Drop;
                    };
                    ctx.client_to_host_queue.push((msg, Vec::new()));
                    log::debug!(
                        "Set application ID to {} (formatted: {}) on zaura_surface (host_id={})",
                        app_id,
                        formatted_app_id,
                        zaura_surface_host_id
                    );
                }
            }
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE;
    use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE;
    use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
    use crate::protocols::viewporter::wp_viewport::WpViewportHandler;
    use crate::protocols::viewporter::wp_viewporter::WpViewporterHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::wl_surface::WlSurfaceHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler;
    use crate::state::{BufferState, Context, PoolInner, PoolState};
    use std::sync::{Arc, RwLock};

    fn msg_sender(msg: &[u8]) -> u32 {
        u32::from_ne_bytes(msg[0..4].try_into().unwrap())
    }

    fn msg_opcode(msg: &[u8]) -> u16 {
        let word2 = u32::from_ne_bytes(msg[4..8].try_into().unwrap());
        (word2 & 0xffff) as u16
    }

    fn protocol_error_code(ctx: &Context) -> u32 {
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(msg_sender(message), 1);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::wayland::wl_display::EVT_ERROR
        );
        u32::from_ne_bytes(message[12..16].try_into().unwrap())
    }

    fn setup_ctx() -> (Context, u32, u32, u32) {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let wl_surface_guest = 100u32;
        let wl_surface_host = 200u32;
        let zaura_shell_host = 300u32;
        let xdg_toplevel_id = 400u32;
        let xdg_surface_id = 500u32;

        ctx.shadow_table.map_id(wl_surface_guest, wl_surface_host);
        ctx.shadow_table
            .map_id(xdg_toplevel_id, xdg_toplevel_id + 100);
        ctx.host_zaura_shell_id = Some(zaura_shell_host);
        ctx.host_zaura_shell_version = 38;
        ctx.xdg_surface_to_wl_surface
            .insert(xdg_surface_id, wl_surface_guest);
        ctx.xdg_toplevel_to_wl_surface
            .insert(xdg_toplevel_id, wl_surface_guest);

        (ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host)
    }

    fn mapped_test_buffer(
        guest_buffer_id: u32,
        width: i32,
        height: i32,
        stride: u32,
        needs_full_copy: bool,
        host_released: bool,
    ) -> BufferState {
        let source_size = usize::try_from(height)
            .unwrap()
            .checked_mul(stride as usize)
            .unwrap();
        let source_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                source_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(source_ptr, libc::MAP_FAILED);
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
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: source_ptr,
                size: source_size,
            }),
        });
        BufferState {
            guest_buffer_id,
            pool,
            offset: 0,
            width,
            height,
            stride,
            format: 0,
            host_buffer_id: guest_buffer_id + 1,
            bo: None,
            dmabuf_fd: None,
            bo_stride: stride,
            dmabuf_plane1_offset: 0,
            dmabuf_plane1_stride: 0,
            dmabuf_sync: false,
            dest_ptr: destination_ptr as *mut u8,
            dest_size: source_size,
            needs_full_copy,
            host_released,
        }
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

        let zaura_surface_host = 99u32;
        ctx.wl_surface_to_zaura_surface
            .insert(wl_surface_host, zaura_surface_host);

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"reused".to_string());
        assert_eq!(action, Action::Drop);

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
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
    fn set_app_id_noop_when_no_zaura_shell() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell_id = None;
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"no_shell".to_string());
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
    }

    #[test]
    fn set_app_id_noop_when_version_below_5() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell_version = 4;
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"old_host".to_string());
        assert_eq!(action, Action::Drop);

        // The xdg_toplevel app ID is always namespaced; set_application_id is
        // skipped when the host aura-shell version is too old.
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            REQ_GET_AURA_SURFACE
        );

        // zaura_surface should still be tracked for cleanup
        assert!(ctx
            .wl_surface_to_zaura_surface
            .contains_key(&wl_surface_host));
    }

    #[test]
    fn oversized_namespaced_app_id_is_dropped_before_wire_encoding() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.last_sender_id = xdg_toplevel_id;
        let oversized = "x".repeat(65_520);

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_set_app_id(&mut ctx, &oversized), Action::Drop);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "an app ID exceeding the Wayland message limit must not be queued"
        );
    }

    #[test]
    fn namespaced_app_id_wire_size_boundary_is_checked() {
        // Exercise the exact 16-bit boundary independently of the namespace
        // prefix used by the production path.
        assert!(wayland_string_fits_message(&"x".repeat(65_519)));
        assert!(!wayland_string_fits_message(&"x".repeat(65_520)));
    }

    #[test]
    fn commit_without_attach_keeps_the_current_buffer() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.current_buffer_id),
            Some(42)
        );

        // A damage-only commit has no new attach request. The previously
        // committed buffer remains the source of the copied pixels.
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.current_buffer_id),
            Some(42)
        );

        // An explicit attach(NULL) is different from omitting attach and must
        // clear the current buffer.
        assert_eq!(handler.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.current_buffer_id),
            None
        );
    }

    #[test]
    fn damage_only_commit_restarts_use_after_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, false, true),
        );
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            !ctx.buffers
                .get(&buffer_id)
                .expect("current buffer")
                .host_released,
            "a damage-only commit must begin a new host compositor use interval"
        );
        assert!(ctx.submitted_buffers.contains(&buffer_id));
    }

    #[test]
    fn damage_only_commit_resubmits_released_current_buffer() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, false, true),
        );
        // The host has released the previous compositor-use interval. A
        // damage-only commit must begin a new interval even though no attach
        // request appears in this commit.
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            ctx.submitted_buffers.contains(&buffer_id),
            "damage-only commit must mark the current buffer as submitted again"
        );
        assert!(
            !ctx.buffers
                .get(&buffer_id)
                .expect("current buffer")
                .host_released,
            "damage-only commit must reopen the compositor-use interval"
        );
    }

    #[test]
    fn damage_only_commit_reopens_native_buffer_after_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.released_host_buffers.insert(buffer_id);
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(
            !ctx.released_host_buffers.contains(&buffer_id),
            "a commit must clear the prior release marker for a native buffer"
        );
        assert!(ctx.submitted_buffers.contains(&buffer_id));
    }

    #[test]
    fn native_buffer_damage_uses_recorded_dimensions() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.native_buffer_sizes.insert(buffer_id, (100, 50));
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
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
    fn native_release_commit_destroy_waits_for_the_new_release() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Forward
        );
        assert!(ctx.released_host_buffers.contains(&buffer_id));

        // A damage-only commit starts a new host use interval even without an
        // attach request.
        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(!ctx.released_host_buffers.contains(&buffer_id));
        assert!(ctx.submitted_buffers.contains(&buffer_id));

        // Destroying the guest object before the new host release must defer
        // the host destructor rather than using the old release marker.
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.deferred_host_buffers.get(&buffer_id),
            Some(&host_buffer_id)
        );
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer_id),
            "host buffer destroy must wait for the new release"
        );
    }

    #[test]
    fn attaching_idle_shm_buffer_without_commit_keeps_release_edge() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, false, false),
        );
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);

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
        assert!(ctx.retired_buffers.contains_key(&buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        // Replacing the pending attach proves that it will never commit, so
        // the host destructor can now be ordered safely.
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(!ctx.retired_buffers.contains_key(&buffer_id));
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);

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
        assert!(ctx.deferred_host_buffers.contains_key(&buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 0, 0, 0), Action::Forward);
        assert!(!ctx.deferred_host_buffers.contains_key(&buffer_id));
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, true, false),
        );
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert!(ctx.retired_buffers.contains_key(&buffer_id));

        // This release belongs to the old committed interval. It must not
        // destroy the host object while the pending attach can still commit.
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(ctx.retired_buffers.contains_key(&buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.retired_buffers.contains_key(&buffer_id));
        assert!(ctx.submitted_buffers.contains(&buffer_id));

        // A release for the newly committed interval is the one that permits
        // the host destructor.
        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(!ctx.retired_buffers.contains_key(&buffer_id));
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .current_buffer_id = Some(buffer_id);
        ctx.submitted_buffers.insert(buffer_id);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );

        let mut shm = crate::handler::shm::ShmHandler;
        ctx.last_sender_id = buffer_id;
        assert_eq!(shm.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.deferred_host_buffers.get(&buffer_id),
            Some(&host_buffer_id)
        );

        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(ctx.deferred_host_buffers.contains_key(&buffer_id));
        assert!(ctx.released_host_buffers.contains(&buffer_id));
        assert!(!ctx
            .client_to_host_queue
            .iter()
            .any(|(message, _)| msg_sender(message) == host_buffer_id));

        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.submitted_buffers.contains(&buffer_id));
        assert!(ctx.deferred_host_buffers.contains_key(&buffer_id));
        assert!(!ctx.released_host_buffers.contains(&buffer_id));

        ctx.last_sender_id = host_buffer_id;
        assert_eq!(
            WlBufferHandler::on_release(&mut shm, &mut ctx),
            Action::Drop
        );
        assert!(!ctx.deferred_host_buffers.contains_key(&buffer_id));
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
        ctx.shadow_table.map_id(old_buffer, old_host_buffer);
        ctx.shadow_table
            .track_interface_with_version(old_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(old_host_buffer, 1);

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
            ctx.deferred_host_buffers.contains_key(&old_buffer),
            "a pending attach keeps the native buffer alive until replaced"
        );

        // The old pending attach is superseded before any commit reaches the
        // compositor, so no wl_buffer.release event can arrive for it.
        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, new_buffer, 0, 0),
            Action::Forward
        );
        assert!(ctx.deferred_host_buffers.is_empty());
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
        ctx.shadow_table.map_id(old_buffer, old_host_buffer);
        ctx.shadow_table
            .track_interface_with_version(old_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(old_host_buffer, 1);
        ctx.buffers.insert(
            old_buffer,
            mapped_test_buffer(old_buffer, 1, 1, 4, false, false),
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
        assert!(ctx.retired_buffers.contains_key(&old_buffer));

        ctx.last_sender_id = surface_id;
        assert_eq!(
            compositor.on_attach(&mut ctx, new_buffer, 0, 0),
            Action::Forward
        );
        assert!(ctx.retired_buffers.is_empty());
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
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.retired_buffers.insert(
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false, false),
        );
        // The buffer was committed by an earlier surface state and destroyed
        // by the guest, but its host use interval is still in flight. It is
        // intentionally no longer referenced by the surface below: a later
        // attach must not mistake that state for an uncommitted pending attach.
        ctx.submitted_buffers.insert(guest_buffer);

        let mut compositor = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_attach(&mut ctx, 44, 0, 0), Action::Forward);
        assert!(ctx.retired_buffers.contains_key(&guest_buffer));
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "a submitted buffer must wait for wl_buffer.release after detach"
        );
    }

    #[test]
    fn damage_only_commit_copies_mutated_committed_shm_buffer() {
        const BUFFER_BYTES: usize = 4;

        // Use real anonymous mappings so BufferState/PoolState own the exact
        // pointer kinds that the production SHM path receives. This catches a
        // regression where damage-only commits update bookkeeping but skip
        // copying from the already committed buffer.
        let source_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BUFFER_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(source_ptr, libc::MAP_FAILED);
        let destination_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BUFFER_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(destination_ptr, libc::MAP_FAILED);

        let source_ptr = source_ptr as *mut u8;
        let destination_ptr = destination_ptr as *mut u8;
        unsafe {
            std::ptr::write_bytes(source_ptr, 0x11, BUFFER_BYTES);
            std::ptr::write_bytes(destination_ptr, 0, BUFFER_BYTES);
        }

        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: source_ptr.cast(),
                size: BUFFER_BYTES,
            }),
        });
        ctx.buffers.insert(
            buffer_id,
            BufferState {
                guest_buffer_id: buffer_id,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: BUFFER_BYTES as u32,
                format: 0,
                host_buffer_id: 43,
                bo: None,
                dmabuf_fd: None,
                bo_stride: BUFFER_BYTES as u32,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: destination_ptr,
                dest_size: BUFFER_BYTES,
                needs_full_copy: true,
                host_released: false,
            },
        );

        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(destination_ptr, BUFFER_BYTES),
                &[0x11; BUFFER_BYTES]
            );
            std::ptr::write_bytes(source_ptr, 0x22, BUFFER_BYTES);
        }

        // No attach is submitted here: the current buffer is reused and only
        // a new damage request is committed.
        assert_eq!(handler.on_damage(&mut ctx, 0, 0, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(destination_ptr, BUFFER_BYTES),
                &[0x22; BUFFER_BYTES]
            );
        }
    }

    #[test]
    fn destroyed_pending_shm_attach_survives_until_commit() {
        const BUFFER_BYTES: usize = 4;
        let source_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BUFFER_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        let destination_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                BUFFER_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(source_ptr, libc::MAP_FAILED);
        assert_ne!(destination_ptr, libc::MAP_FAILED);

        let source_ptr = source_ptr as *mut u8;
        let destination_ptr = destination_ptr as *mut u8;
        unsafe {
            std::ptr::write_bytes(source_ptr, 0x11, BUFFER_BYTES);
            std::ptr::write_bytes(destination_ptr, 0, BUFFER_BYTES);
        }

        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 5);
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: source_ptr.cast(),
                size: BUFFER_BYTES,
            }),
        });
        ctx.buffers.insert(
            buffer_id,
            BufferState {
                guest_buffer_id: buffer_id,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: BUFFER_BYTES as u32,
                format: 0,
                host_buffer_id,
                bo: None,
                dmabuf_fd: None,
                bo_stride: BUFFER_BYTES as u32,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: destination_ptr,
                dest_size: BUFFER_BYTES,
                needs_full_copy: true,
                // This buffer has completed a previous compositor-use
                // interval. A new attach must reopen that interval even when
                // the guest destroys the object before commit.
                host_released: true,
            },
        );
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);

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
        assert!(!ctx.buffers.contains_key(&buffer_id));
        assert!(ctx.retired_buffers.contains_key(&buffer_id));
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending_buffer_id),
            Some(Some(buffer_id))
        );

        unsafe {
            std::ptr::write_bytes(source_ptr, 0x22, BUFFER_BYTES);
        }
        ctx.last_sender_id = surface_id;
        assert_eq!(compositor.on_commit(&mut ctx), Action::Drop);
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(destination_ptr, BUFFER_BYTES),
                &[0x22; BUFFER_BYTES],
                "destroying the guest buffer before commit must not lose its frame"
            );
        }
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.current_buffer_id),
            Some(buffer_id)
        );
        assert!(ctx.submitted_buffers.contains(&buffer_id));
    }

    #[test]
    fn rejects_invalid_scale_and_transform_without_mutating_pending_state() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(
            handler.on_set_buffer_scale(&mut ctx, 0),
            Action::Drop,
            "wl_surface.set_buffer_scale(0) is a protocol error"
        );
        assert_eq!(
            handler.on_set_buffer_scale(&mut ctx, -2),
            Action::Drop,
            "negative buffer scales must not enter pending state"
        );
        assert_eq!(
            handler.on_set_buffer_transform(&mut ctx, 8),
            Action::Drop,
            "wl_output.transform values are limited to 0..=7"
        );
        assert!(
            ctx.surfaces.get(&surface_id).is_none_or(|surface| {
                surface.pending_buffer_scale.is_none() && surface.pending_buffer_transform.is_none()
            }),
            "invalid requests must not create or mutate pending surface state"
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.fatal_protocol_error);
    }

    #[test]
    fn v5_attach_rejects_nonzero_offsets_but_legacy_attach_tracks_them() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 5);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending_buffer_id),
            None
        );
        assert_eq!(protocol_error_code(&ctx), 3);
        assert!(ctx.fatal_protocol_error);

        ctx.host_to_client_queue.clear();
        ctx.fatal_protocol_error = false;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 4);
        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending_attach_offset),
            Some((3, 4))
        );
    }

    #[test]
    fn v5_zero_attach_does_not_reset_committed_offset() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 5);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_offset(&mut ctx, 7, 8), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.current_offset),
            Some((7, 8))
        );

        // Version 5+ ignores attach's zero coordinates. They must not
        // overwrite the offset committed by the dedicated offset request.
        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.current_offset),
            Some((7, 8)),
            "a legal v5 attach(buffer, 0, 0) must preserve the committed offset"
        );
    }

    #[test]
    fn offset_request_wins_over_legacy_attach_offset_at_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 4);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        // Simulate a v5+ offset request in the state path; the commit logic
        // must prefer it over the legacy attach coordinates.
        ctx.surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .pending_offset = Some((8, 9));
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.current_offset),
            Some((8, 9))
        );
    }

    #[test]
    fn consumed_attach_offset_is_not_reapplied_on_later_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 4);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        ctx.surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .pending_offset = Some((8, 9));
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.current_offset),
            Some((8, 9))
        );

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .map(|surface| surface.current_offset),
            Some((8, 9)),
            "the old legacy attach offset must be consumed by the first commit"
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
            ctx.client_to_host_queue.len(),
            2,
            "translated damage must be queued before the replacement commit"
        );
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(message), 200);
        assert_eq!(msg_opcode(message), REQ_DAMAGE);
        assert_eq!(
            &message[8..],
            &[
                3i32.to_ne_bytes(),
                4i32.to_ne_bytes(),
                8i32.to_ne_bytes(),
                9i32.to_ne_bytes()
            ]
            .concat()
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue[1].0), 200);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[1].0), REQ_COMMIT);
    }

    #[test]
    fn first_shm_commit_without_explicit_damage_queues_full_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, true, false),
        );

        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_attach(&mut ctx, buffer_id, 0, 0),
            Action::Forward
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        let damage = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_opcode(damage), REQ_DAMAGE);
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &damage[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 8);
        assert_eq!(wire.read_i32().unwrap(), 6);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[1].0), REQ_COMMIT);
    }

    #[test]
    fn failed_shm_copy_does_not_queue_a_host_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let mut buffer = mapped_test_buffer(buffer_id, 8, 6, 32, true, false);
        let destination_ptr = buffer.dest_ptr;
        buffer.dest_ptr = std::ptr::null_mut();
        buffer.dest_size = 0;
        unsafe {
            libc::munmap(destination_ptr as *mut libc::c_void, 32 * 6);
        }
        ctx.buffers.insert(buffer_id, buffer);

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
            ctx.buffers
                .get(&buffer_id)
                .is_some_and(|buffer| buffer.needs_full_copy),
            "a failed copy must force a complete retry on the next commit"
        );
    }

    #[test]
    fn surface_damage_is_outset_before_host_forwarding() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_damage(&mut ctx, 10, 20, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let damage = &ctx.client_to_host_queue[0].0;
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &damage[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 9);
        assert_eq!(wire.read_i32().unwrap(), 19);
        assert_eq!(wire.read_i32().unwrap(), 3);
        assert_eq!(wire.read_i32().unwrap(), 3);
    }

    #[test]
    fn damage_buffer_mapping_applies_buffer_scale_and_filtering_outset() {
        let surface = SurfaceState {
            current_buffer_scale: 2,
            ..Default::default()
        };

        let mapped = map_buffer_damage(DamageRect::new(20, 10, 4, 6), &surface, 100, 100);

        assert_eq!(mapped, DamageRect::new(9, 4, 4, 5));
    }

    #[test]
    fn full_mapping_without_explicit_damage_still_damages_the_host_surface() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, false, false),
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

        let damage = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_opcode(damage), REQ_DAMAGE);
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &damage[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 6);
        assert_eq!(wire.read_i32().unwrap(), 8);
    }

    #[test]
    fn damage_buffer_mapping_does_not_include_surface_offset() {
        let surface = SurfaceState {
            current_offset: (80, 90),
            ..Default::default()
        };

        let mapped = map_buffer_damage(DamageRect::new(4, 5, 6, 7), &surface, 100, 100);

        assert_eq!(
            mapped,
            DamageRect::new(3, 4, 8, 9),
            "wl_surface.attach/offset changes content placement, not buffer damage coordinates"
        );
    }

    #[test]
    fn nonzero_surface_offset_without_explicit_damage_forces_full_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, false, false),
        );
        ctx.surfaces.insert(
            surface_id,
            SurfaceState {
                current_buffer_id: Some(buffer_id),
                current_offset: (3, 0),
                ..Default::default()
            },
        );
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let damage = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("offset-only commit must damage the complete host surface");
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &damage.0[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 8);
        assert_eq!(wire.read_i32().unwrap(), 6);
    }

    #[test]
    fn nonzero_surface_offset_with_explicit_damage_forces_full_host_damage() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        ctx.buffers.insert(
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, false, false),
        );
        ctx.surfaces.insert(
            surface_id,
            SurfaceState {
                current_buffer_id: Some(buffer_id),
                current_offset: (3, 0),
                ..Default::default()
            },
        );
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_damage(&mut ctx, 1, 1, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let damage = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("offset plus explicit damage must include complete host damage");
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &damage.0[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 8);
        assert_eq!(wire.read_i32().unwrap(), 6);
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

        let message = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("damage request");
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &message.0[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), i32::MIN / 10);
        assert_eq!(wire.read_i32().unwrap(), -1);
        assert_eq!(wire.read_i32().unwrap(), 214_748_364);
        assert_eq!(wire.read_i32().unwrap(), 3);
    }

    #[test]
    fn rotated_non_square_buffer_full_damage_swaps_extent() {
        let surface = SurfaceState {
            current_buffer_transform: 1,
            ..Default::default()
        };

        assert_eq!(
            full_surface_damage(&surface, 8, 6),
            DamageRect::new(0, 0, 6, 8),
            "90-degree transforms expose a height-by-width surface extent"
        );
    }

    #[test]
    fn rotated_scaled_buffer_full_damage_uses_logical_extent() {
        let surface = SurfaceState {
            current_buffer_scale: 2,
            current_buffer_transform: 1,
            ..Default::default()
        };

        assert_eq!(
            full_surface_damage(&surface, 8, 6),
            DamageRect::new(0, 0, 3, 4),
            "buffer scale converts the rotated extent to surface coordinates"
        );
    }

    #[test]
    fn viewport_damage_uses_committed_crop_and_destination() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.buffers
            .insert(42, mapped_test_buffer(42, 2048, 2048, 8192, false, false));
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

        // The viewport update becomes current at this commit, and the buffer
        // damage is converted from buffer pixels into destination coordinates.
        ctx.last_sender_id = surface_id;
        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(
            handler.on_damage_buffer(&mut ctx, 256, 512, 256, 256),
            Action::Drop
        );
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        let message = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_DAMAGE)
            .expect("translated damage request");
        let mut wire = crate::wire::WireMessage::new(200, REQ_DAMAGE, &message.0[8..], &[]);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 0);
        assert_eq!(wire.read_i32().unwrap(), 51);
        assert_eq!(wire.read_i32().unwrap(), 26);
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
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(ctx
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.viewport)
            .is_some());

        ctx.last_sender_id = 700;
        assert_eq!(
            WpViewportHandler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
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
        assert!(ctx.fatal_protocol_error);

        ctx.host_to_client_queue.clear();
        ctx.fatal_protocol_error = false;
        assert_eq!(handler.on_set_destination(&mut ctx, 0, 10), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.fatal_protocol_error);

        ctx.host_to_client_queue.clear();
        ctx.fatal_protocol_error = false;
        ctx.viewport_to_wl_surface.remove(&700);
        assert_eq!(handler.on_set_destination(&mut ctx, 10, 10), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 3);
        assert!(ctx.fatal_protocol_error);
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
        ctx.fatal_protocol_error = false;
        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 701, surface_id),
            Action::Drop
        );
        assert_eq!(protocol_error_code(&ctx), 0);
        assert!(ctx.fatal_protocol_error);
    }

    #[test]
    fn fractional_source_without_destination_is_bad_size_on_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let mut handler = CompositorHandler;

        ctx.last_sender_id = surface_id;
        assert_eq!(
            handler.on_get_viewport(&mut ctx, 700, surface_id),
            Action::Forward
        );
        ctx.last_sender_id = 700;
        assert_eq!(
            handler.on_set_source(&mut ctx, 0, 0, 128, 256),
            Action::Forward
        );
        ctx.last_sender_id = surface_id;
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert_eq!(protocol_error_code(&ctx), 1);
        assert!(ctx.fatal_protocol_error);
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| msg_opcode(message) != REQ_COMMIT),
            "invalid viewport state must not forward the surface commit"
        );
    }

    #[test]
    fn xdg_toplevel_destroy_cleans_up_map() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();

        let mut handler = CompositorHandler;
        ctx.last_sender_id = xdg_toplevel_id;

        assert!(ctx
            .xdg_toplevel_to_wl_surface
            .contains_key(&xdg_toplevel_id));

        ctx.last_sender_id = xdg_toplevel_id;
        let action = XdgToplevelHandler::on_destroy(&mut handler, &mut ctx);
        assert_eq!(action, Action::Forward);

        assert!(!ctx
            .xdg_toplevel_to_wl_surface
            .contains_key(&xdg_toplevel_id));
    }

    #[test]
    fn wl_surface_destroy_releases_zaura_surface() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();

        ctx.last_sender_id = xdg_toplevel_id;
        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"app".to_string());

        let zaura_surface_host = *ctx
            .wl_surface_to_zaura_surface
            .get(&wl_surface_host)
            .unwrap();

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

        assert!(!ctx
            .wl_surface_to_zaura_surface
            .contains_key(&wl_surface_host));
    }

    #[test]
    fn wl_surface_destroy_retires_deferred_native_buffer_after_surface_destroy() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 60u32;

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.shadow_table.mark_pending_destroy(guest_buffer);
        ctx.deferred_host_buffers.insert(guest_buffer, host_buffer);
        ctx.submitted_buffers.insert(guest_buffer);
        ctx.surfaces
            .entry(wl_surface_guest)
            .or_default()
            .current_buffer_id = Some(guest_buffer);

        ctx.last_sender_id = wl_surface_guest;
        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(ctx.deferred_host_buffers.is_empty());
        assert!(!ctx.submitted_buffers.contains(&guest_buffer));
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
    fn wl_surface_destroy_retires_submitted_shm_buffer_after_surface_destroy() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let wl_surface_guest = 100u32;
        let guest_buffer = 50u32;
        let host_buffer = 51u32;

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.retired_buffers.insert(
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false, false),
        );
        ctx.submitted_buffers.insert(guest_buffer);
        ctx.surfaces
            .entry(wl_surface_guest)
            .or_default()
            .current_buffer_id = Some(guest_buffer);

        ctx.last_sender_id = wl_surface_guest;
        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(ctx.retired_buffers.is_empty());
        assert!(!ctx.submitted_buffers.contains(&guest_buffer));
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.buffers.insert(
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false, false),
        );
        ctx.submitted_buffers.insert(guest_buffer);
        ctx.surfaces
            .entry(wl_surface_guest)
            .or_default()
            .current_buffer_id = Some(guest_buffer);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = wl_surface_guest;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(
            !ctx.submitted_buffers.contains(&guest_buffer),
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
        assert!(!ctx.retired_buffers.contains_key(&guest_buffer));
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.retired_buffers.insert(
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false, false),
        );
        // This is a committed buffer whose surface was detached before the
        // release edge arrived. It is not owned by surface 100, even though
        // the submitted marker is global.
        ctx.submitted_buffers.insert(guest_buffer);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = 100;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );

        assert!(ctx.retired_buffers.contains_key(&guest_buffer));
        assert!(ctx.submitted_buffers.contains(&guest_buffer));
        assert!(
            !ctx.client_to_host_queue
                .iter()
                .any(|(message, _)| msg_sender(message) == host_buffer),
            "an unrelated surface destroy must not preempt wl_buffer.release"
        );
    }

    #[test]
    fn wl_surface_destroy_removes_stale_xdg_surface_links() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let wl_surface_guest_id = 100u32;
        ctx.xdg_surface_to_wl_surface
            .insert(601, wl_surface_guest_id);
        ctx.xdg_toplevel_to_wl_surface
            .insert(602, wl_surface_guest_id);
        ctx.last_sender_id = wl_surface_guest_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(!ctx
            .xdg_surface_to_wl_surface
            .values()
            .any(|id| *id == wl_surface_guest_id));
        assert!(!ctx
            .xdg_toplevel_to_wl_surface
            .values()
            .any(|id| *id == wl_surface_guest_id));
        // The unrelated setup mapping is also removed because it points to
        // the same surface; no stale association should survive destruction.
        assert!(!ctx
            .xdg_toplevel_to_wl_surface
            .contains_key(&xdg_toplevel_id));
    }

    #[test]
    fn wl_surface_destroy_removes_keyboard_focus_links() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let wl_surface_guest_id = 100u32;
        let host_keyboard_id = crate::state::HostId(700);
        ctx.keyboard_active_surfaces
            .insert(host_keyboard_id, wl_surface_guest_id);
        ctx.keyboard_pressed_keys
            .insert(host_keyboard_id, [14_u32].into_iter().collect());
        ctx.keyboard_backspace_repeat_cancelled
            .insert(host_keyboard_id);
        ctx.keyboard_event_times.insert(host_keyboard_id, 123);
        ctx.keyboard_ime_suppressed_keys
            .insert(host_keyboard_id, [14_u32].into_iter().collect());
        ctx.keyboard_forwarded_keys
            .insert(host_keyboard_id, [14_u32].into_iter().collect());
        ctx.keyboard_keysym_forwarded_keys
            .insert(host_keyboard_id, [14_u32].into_iter().collect());
        ctx.keyboard_active_surfaces
            .insert(crate::state::HostId(701), 999);
        ctx.last_sender_id = wl_surface_guest_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(
            !ctx.keyboard_active_surfaces
                .contains_key(&crate::state::HostId(700)),
            "destroying a surface must retire per-keyboard focus"
        );
        assert!(
            !ctx.keyboard_pressed_keys.contains_key(&host_keyboard_id)
                && !ctx
                    .keyboard_backspace_repeat_cancelled
                    .contains(&host_keyboard_id)
                && !ctx.keyboard_event_times.contains_key(&host_keyboard_id)
                && !ctx
                    .keyboard_ime_suppressed_keys
                    .contains_key(&host_keyboard_id)
                && !ctx.keyboard_forwarded_keys.contains_key(&host_keyboard_id)
                && !ctx
                    .keyboard_keysym_forwarded_keys
                    .contains_key(&host_keyboard_id),
            "destroying a surface must retire all per-keyboard input state"
        );
        assert_eq!(
            ctx.keyboard_active_surfaces.get(&crate::state::HostId(701)),
            Some(&999),
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

        ctx.shadow_table.map_id(1, 2);
        ctx.shadow_table.track_interface(1, "wl_seat".to_string());
        ctx.shadow_table.map_id(text_input, 701);
        ctx.shadow_table
            .track_interface(text_input, "zwp_text_input_v3".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard, 1);
        ctx.keyboard_active_surfaces
            .insert(crate::state::HostId(host_keyboard), surface);
        ctx.active_surface_for_seat.insert(1, surface);
        ctx.text_inputs.insert(
            text_input,
            crate::state::TextInputState {
                host_v1_id: 702,
                host_ext_id: None,
                guest_seat: 1,
                active_surface: Some(surface),
                pending_enabled: false,
                committed_enabled: false,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 0,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                empty_preedit_repeat_active: false,
                host_activated: true,
            },
        );

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
            Action::Forward
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
    }

    #[test]
    fn old_aura_surface_stays_reserved_without_release_request() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell_version = 37;
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"legacy".to_string());
        let zaura_surface_host = *ctx
            .wl_surface_to_zaura_surface
            .get(&wl_surface_host)
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
        assert_ne!(ctx.shadow_table.allocate_host_id(), zaura_surface_host);
    }
}
