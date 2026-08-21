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
use crate::protocols::aura_shell::zaura_shell::{
    REQ_GET_AURA_SURFACE, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
};
use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE;
use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
use crate::protocols::aura_shell::zaura_toplevel::REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL;
use crate::protocols::wayland::wl_compositor::WlCompositorHandler;
use crate::protocols::wayland::wl_display::REQ_SYNC;
use crate::protocols::wayland::wl_region::WlRegionHandler;
use crate::protocols::wayland::wl_subcompositor::WlSubcompositorHandler;
use crate::protocols::wayland::wl_subsurface::WlSubsurfaceHandler;
use crate::protocols::wayland::wl_surface::{
    WlSurfaceHandler, REQ_COMMIT, REQ_DAMAGE, REQ_DESTROY,
};
use crate::protocols::xdg_shell::xdg_toplevel::{
    REQ_DESTROY as REQ_DESTROY_XDG_TOPLEVEL, REQ_SET_APP_ID,
};
use crate::state::{
    Context, DamageRect, SurfaceAttachment, SurfaceCommit, SurfaceState, ViewportState,
};
use crate::wire::Action;
use log::trace;
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

fn wait_for_native_buffer(ctx: &mut Context, guest_buffer_id: u32) -> std::io::Result<()> {
    if ctx.native_buffer_uses_implicit_sync(guest_buffer_id) {
        return Ok(());
    }

    let needs_implicit_sync = {
        let Some(sync_fds) = ctx.native_buffer_sync_fds(guest_buffer_id) else {
            return Ok(());
        };
        let mut needs_implicit_sync = false;
        for sync_fd in sync_fds {
            match crate::allocator::wait_for_dmabuf(sync_fd.as_raw_fd(), ctx.allocator.as_ref())? {
                crate::allocator::DmabufWaitResult::Synchronized => {}
                crate::allocator::DmabufWaitResult::ImplicitSyncFallback => {
                    needs_implicit_sync = true;
                    break;
                }
            }
        }
        needs_implicit_sync
    };

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
    Ok(())
}

impl WlCompositorHandler for CompositorHandler {}

impl crate::protocols::wayland::wl_output::WlOutputHandler for CompositorHandler {
    fn on_mode(
        &mut self,
        ctx: &mut Context,
        flags: u32,
        width: i32,
        height: i32,
        _refresh: i32,
    ) -> Action {
        ctx.window_placement
            .update_output_mode(ctx.last_sender_id, flags & 1 != 0, width, height);
        Action::Forward
    }

    fn on_scale(&mut self, ctx: &mut Context, factor: i32) -> Action {
        ctx.window_placement
            .update_output_scale(ctx.last_sender_id, factor);
        Action::Forward
    }

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let guest_output_id = crate::state::GuestId::from_request_sender(ctx);
        let Some(host_output_id) = ctx.shadow_table.host_id_of(guest_output_id) else {
            log::debug!(
                "wl_output.release for unmapped guest output {}",
                guest_output_id.0
            );
            return Action::Forward;
        };
        if !ctx.window_placement.take_output(host_output_id.0) {
            log::debug!(
                "wl_output.release for untracked host output {} (guest {})",
                host_output_id.0,
                guest_output_id.0
            );
        }
        Action::Forward
    }
}
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

pub(crate) fn wayland_string_fits_message(value: &str) -> bool {
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

/// Create or reuse the host Aura object associated with a guest wl_surface.
///
/// XDG and GTK metadata both target the same host surface. Exo rejects a
/// second `zaura_surface` for one `wl_surface`, so all metadata paths must
/// share this mapping.
pub(crate) fn ensure_host_zaura_surface(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    let wl_surface_host_id = ctx.shadow_table.get_host_id(wl_surface_guest_id)?;
    if let Some(zaura_surface_host_id) = ctx
        .window_placement
        .aura_surface_for_wl_surface(wl_surface_host_id)
    {
        return Some(zaura_surface_host_id);
    }

    let zaura_shell_host_id = ctx.window_placement.aura_shell_id()?;
    let zaura_surface_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        zaura_surface_host_id,
        "zaura_surface".to_string(),
        ctx.window_placement.aura_shell_version(),
    );

    let mut builder = crate::wire::MessageBuilder::new();
    builder.write_u32(zaura_surface_host_id);
    builder.write_u32(wl_surface_host_id);
    let Ok(message) = builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_SURFACE) else {
        log::warn!(
            "Unable to encode Aura surface request for wl_surface {}",
            wl_surface_guest_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_surface_host_id);
        return None;
    };

    if !ctx
        .window_placement
        .remember_aura_surface(wl_surface_host_id, zaura_surface_host_id)
    {
        log::error!(
            "Refusing to replace Aura surface mapping for host wl_surface {}",
            wl_surface_host_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_surface_host_id);
        return None;
    }
    ctx.client_to_host_queue.push((message, Vec::new()));
    Some(zaura_surface_host_id)
}

/// Queue a nullable Aura application ID update for a live host surface.
///
/// The request is valid only on `zaura_surface` version 5 or newer. The
/// helper is shared by XDG and GTK metadata handling so both paths apply the
/// same version and message-size checks.
pub(crate) fn queue_zaura_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    application_id: &str,
) -> bool {
    let version = ctx
        .shadow_table
        .host_object_version(zaura_surface_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version < 5 || !wayland_string_fits_message(application_id) {
        return false;
    }

    let mut builder = crate::wire::MessageBuilder::new();
    builder.write_nullable_string(Some(application_id));
    match builder.try_build_message(zaura_surface_id, REQ_SET_APPLICATION_ID) {
        Ok(message) => {
            ctx.client_to_host_queue.push((message, Vec::new()));
            true
        }
        Err(error) => {
            log::warn!(
                "Unable to encode Aura application ID for zaura_surface {}: {}",
                zaura_surface_id,
                error
            );
            false
        }
    }
}

/// Queue the Aura identity selected by the placement policy.
///
/// `persistent-native-shell` intentionally queues two updates. ChromeOS
/// applies ARC authorization properties on the first update, while the second
/// update restores the native shell application ID used by Crostini shelf
/// matching. The host currently does not clear ARC properties on that second
/// update; this is why the mode is experimental and must not become the
/// default without runtime verification.
pub(crate) fn queue_policy_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    native_application_id: &str,
    arc_application_id: Option<&str>,
) -> bool {
    if !ctx.window_placement.uses_arc_policy() {
        return queue_zaura_application_id(ctx, zaura_surface_id, native_application_id);
    }

    let Some(arc_application_id) = arc_application_id else {
        log::warn!(
            "ARC policy has no allocated task ID for zaura_surface {}",
            zaura_surface_id
        );
        return false;
    };

    match ctx.window_placement.arc_id_lifetime() {
        crate::state::WindowArcIdLifetime::Persistent => {
            queue_zaura_application_id(ctx, zaura_surface_id, arc_application_id)
        }
        crate::state::WindowArcIdLifetime::Transient => {
            queue_zaura_application_id(ctx, zaura_surface_id, native_application_id)
        }
        crate::state::WindowArcIdLifetime::PersistentNativeShell => {
            queue_zaura_application_id(ctx, zaura_surface_id, arc_application_id)
                && queue_zaura_application_id(ctx, zaura_surface_id, native_application_id)
        }
    }
}

/// Create or reuse the internal Aura toplevel associated with a guest
/// `xdg_toplevel`. The object is needed for screen-coordinate bounds requests.
pub(crate) fn ensure_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) -> Option<u32> {
    if let Some(existing_id) = ctx
        .window_placement
        .aura_toplevel_for_xdg_toplevel(xdg_toplevel_guest_id)
    {
        return Some(existing_id);
    }

    let xdg_toplevel_host_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id)?;
    let zaura_shell_host_id = ctx.window_placement.aura_shell_id()?;
    if ctx.window_placement.aura_shell_version() < 29 {
        return None;
    }

    let zaura_toplevel_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        zaura_toplevel_host_id,
        "zaura_toplevel".to_string(),
        ctx.window_placement.aura_shell_version(),
    );

    let mut builder = crate::wire::MessageBuilder::new();
    builder.write_u32(zaura_toplevel_host_id);
    builder.write_u32(xdg_toplevel_host_id);
    let Ok(get_message) =
        builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL)
    else {
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    };
    let Ok(coordinate_message) = crate::wire::MessageBuilder::new().try_build_message(
        zaura_toplevel_host_id,
        crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES,
    ) else {
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    };
    if !ctx
        .window_placement
        .remember_aura_toplevel(xdg_toplevel_guest_id, zaura_toplevel_host_id)
    {
        log::error!(
            "Refusing to replace existing Aura toplevel mapping for xdg_toplevel {}",
            xdg_toplevel_guest_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    }
    ctx.client_to_host_queue.push((get_message, Vec::new()));
    ctx.client_to_host_queue
        .push((coordinate_message, Vec::new()));
    Some(zaura_toplevel_host_id)
}

pub(crate) fn release_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    let Some(zaura_toplevel_host_id) = ctx
        .window_placement
        .take_aura_toplevel(xdg_toplevel_guest_id)
    else {
        return;
    };
    let version = ctx
        .shadow_table
        .host_object_version(zaura_toplevel_host_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version >= 38 {
        let message = crate::wire::MessageBuilder::new()
            .build_message(zaura_toplevel_host_id, REQ_RELEASE_AURA_TOPLEVEL);
        ctx.client_to_host_queue.push((message, Vec::new()));
        // Keep the host ID reserved until the compositor acknowledges the
        // destructor with wl_display.delete_id. Removing it immediately
        // would allow a recycled ID to receive the late acknowledgement.
        ctx.shadow_table
            .mark_pending_destroy_host(zaura_toplevel_host_id);
    } else {
        // Older aura-shell versions do not expose a destructor. Retire the
        // dispatch metadata but keep the numeric ID reserved for the rest of
        // the connection so stale host events cannot target a recycled
        // object.
        ctx.shadow_table
            .retire_host_interface(zaura_toplevel_host_id);
    }
}

/// Queue a host `wl_display.sync` immediately after a window-placement
/// request.
///
/// Exo can emit a configure for the old bounds before it has processed the
/// placement request. The callback is an ordered host-stream barrier: once
/// `done` arrives, all events generated by requests before the sync have
/// already been delivered. Keep the callback host ID reserved through its
/// subsequent `wl_display.delete_id`, just like the other internal barriers.
pub(crate) fn queue_window_placement_barrier(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
) -> bool {
    let callback_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        callback_host_id,
        "wl_callback".to_string(),
        1,
    );

    let mut builder = crate::wire::MessageBuilder::new();
    builder.write_u32(callback_host_id);
    let Ok(message) = builder.try_build_message(1, REQ_SYNC) else {
        log::warn!(
            "Unable to encode window-placement barrier for zaura_toplevel {}",
            zaura_toplevel_host_id
        );
        ctx.shadow_table.remove_host_interface(callback_host_id);
        return false;
    };

    // A new shortcut supersedes the previous barrier for this toplevel. The
    // old callback remains tracked until its terminal event so its host ID
    // cannot be recycled while Exo still has the object alive.
    if !ctx
        .window_placement
        .register_barrier(callback_host_id, zaura_toplevel_host_id)
    {
        log::error!(
            "Refusing to replace existing window-placement barrier callback {}",
            callback_host_id
        );
        ctx.shadow_table.remove_host_interface(callback_host_id);
        return false;
    }
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
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
    surface_mapping_requires_full_copy: bool,
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
    if needs_full_copy || (surface_mapping_requires_full_copy && !surface_damage.is_empty()) {
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

fn build_surface_commit_messages(
    ctx: &Context,
    surface_id: u32,
    surface_damage: &[DamageRect],
    buffer_damage: &[DamageRect],
    surface_state: &SurfaceState,
    force_full_damage: bool,
) -> Option<Vec<(Vec<u8>, Vec<RawFd>)>> {
    let host_surface_id = ctx.shadow_table.get_host_id(surface_id)?;
    let (buffer_width, buffer_height) = surface_state
        .current_buffer_id()
        .and_then(|buffer_id| ctx.buffer_dimensions(buffer_id))
        .or(surface_state.current_buffer_dimensions())
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

    // The host compositor treats damage as a unioned region. Coalescing here
    // keeps a GTK frame with many overlapping requests from becoming a long
    // sequence of redundant Wayland messages as well as redundant copies.
    let mut messages = Vec::new();
    for rect in crate::handler::shm::coalesce_damage_rects(&mapped) {
        if rect.width <= 0 || rect.height <= 0 {
            continue;
        }
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_i32(rect.x);
        builder.write_i32(rect.y);
        builder.write_i32(rect.width);
        builder.write_i32(rect.height);
        let message = builder
            .try_build_message(host_surface_id, REQ_DAMAGE)
            .ok()?;
        messages.push((message, Vec::new()));
    }

    let builder = crate::wire::MessageBuilder::new();
    let commit = builder
        .try_build_message(host_surface_id, REQ_COMMIT)
        .ok()?;
    messages.push((commit, Vec::new()));
    Some(messages)
}

/// Copy one locally bridged SHM buffer before its prepared surface commit.
///
/// Native linux-dmabuf buffers have no local copy state and are immediately
/// ready. Every SHM failure keeps `needs_full_copy` armed so retrying the
/// transaction cannot lose the damaged pixels that were consumed while the
/// commit was prepared.
fn copy_surface_buffer(ctx: &mut Context, buffer_id: u32, commit: &SurfaceCommit) -> bool {
    let Some(host_id) = ctx.render_buffer_host_id(buffer_id) else {
        return true;
    };
    let Some(resources) = ctx.local_buffer_copy_resources(host_id) else {
        return true;
    };
    let allocator = resources.allocator;
    let virtwayland_channel = resources.channel;
    let buffer = resources.buffer;

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
                    virtwayland_channel,
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
                        needs_full_copy || commit.has_full_damage(),
                        src_ptr,
                        inner.size,
                        buffer.dest_ptr,
                        buffer.dest_size,
                        buffer.bo_stride as usize,
                        buffer.dmabuf_plane1_offset,
                        buffer.dmabuf_plane1_stride,
                        commit.surface_damage.rects(),
                        commit.buffer_damage.rects(),
                        commit.uses_full_mapping(),
                    )
                } else if let (Some(allocator), Some(bo)) = (allocator, buffer.bo.as_mut()) {
                    match (u32::try_from(buffer_width), u32::try_from(buffer_height)) {
                        (Ok(width), Ok(height)) => {
                            match bo.map_mut(&allocator.device, 0, 0, width, height, |mapped| {
                                copy_damage_to_ptr(
                                    format,
                                    offset,
                                    buffer_width,
                                    buffer_height,
                                    source_stride,
                                    needs_full_copy || commit.has_full_damage(),
                                    src_ptr,
                                    inner.size,
                                    mapped.buffer_mut().as_mut_ptr(),
                                    mapped.buffer().len(),
                                    mapped.stride() as usize,
                                    buffer.dmabuf_plane1_offset,
                                    buffer.dmabuf_plane1_stride,
                                    commit.surface_damage.rects(),
                                    commit.buffer_damage.rects(),
                                    commit.uses_full_mapping(),
                                )
                            }) {
                                Ok(Ok(copy_ok)) => copy_ok,
                                Ok(Err(error)) => {
                                    log::warn!("GBM BO map failed: {}", error);
                                    false
                                }
                                Err(_) => {
                                    log::warn!("GBM BO belongs to a different allocator device");
                                    false
                                }
                            }
                        }
                        (Err(_), _) => {
                            log::warn!("SHM buffer width cannot be represented as u32");
                            false
                        }
                        (_, Err(_)) => {
                            log::warn!("SHM buffer height cannot be represented as u32");
                            false
                        }
                    }
                } else {
                    // No destination mapping is available. This can happen
                    // only for a malformed or unsupported allocation path.
                    false
                };
            }
            if let Some(guard) = sync_guard.take() {
                if !guard.end() {
                    copy_ok = false;
                }
            }
            if copy_ok {
                buffer.needs_full_copy = false;
                // A 4K frame can reach this path every refresh. Keep the
                // per-frame diagnostic at trace level.
                trace!(
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
            } else {
                log::warn!("Deferring SHM commit with an invalid or unmappable destination");
            }
        } else {
            log::warn!("SHM pool has no valid client mapping");
        }
    } else {
        log::warn!("Unable to read SHM pool while committing buffer");
    }

    if !copy_ok {
        buffer.needs_full_copy = true;
    }
    copy_ok
}

impl WlSurfaceHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let wl_surface_guest_id = ctx.last_sender_id;
        let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(wl_surface_guest_id) else {
            log::error!(
                "Unable to destroy wl_surface {} without its host generation",
                wl_surface_guest_id
            );
            ctx.fatal_protocol_error = true;
            return Action::Drop;
        };
        let builder = crate::wire::MessageBuilder::new();
        let Ok(surface_destroy) = builder.try_build_message(wl_surface_host_id, REQ_DESTROY) else {
            log::error!(
                "Unable to encode wl_surface.destroy for host surface {}",
                wl_surface_host_id
            );
            ctx.fatal_protocol_error = true;
            return Action::Drop;
        };
        // Keep the references owned by this surface before removing its
        // state. A submitted buffer may have been detached from another
        // surface and still be waiting for wl_buffer.release; only buffers
        // that this host surface actually held may be retired merely because
        // its destructor is ordered ahead of the buffer destructor.
        let destroyed_surface_buffer = ctx
            .surfaces
            .get(&wl_surface_guest_id)
            .and_then(|surface| surface.current_buffer_id());
        // Clean up any host-side zaura_surface we created for this wl_surface.
        if let Some(zaura_surface_host_id) = ctx
            .window_placement
            .take_aura_surface_for_wl_surface(wl_surface_guest_id, wl_surface_host_id)
        {
            let zaura_surface_version = ctx
                .shadow_table
                .host_object_version(zaura_surface_host_id)
                .unwrap_or(ctx.window_placement.aura_shell_version());
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
        ctx.window_placement
            .take_gtk_surfaces_for_wl_surface(wl_surface_guest_id);
        // The generated dispatcher cannot express the ordering required by
        // the aura-shell integration: zaura_surface.release must precede the
        // paired wl_surface destructor.
        ctx.client_to_host_queue.push((surface_destroy, Vec::new()));
        ctx.surfaces.remove(&ctx.last_sender_id);
        crate::handler::shm::clear_buffer_uses_after_surface_destroy(ctx, destroyed_surface_buffer);
        // Pending-only objects do not receive a host release event. Retire
        // every buffer made eligible by removing this surface in one pass.
        crate::handler::shm::retire_eligible_buffers(ctx);
        ctx.viewport_to_wl_surface
            .retain(|_, surface_id| *surface_id != wl_surface_guest_id);
        ctx.key_generations
            .clear_peek_watermarks_for_surface(wl_surface_guest_id);
        // Surface destruction can race the host's wl_keyboard.leave event.
        // Transition the authoritative registry first, then emit the
        // mandatory v3 leave while the guest surface ID is still valid. A
        // later host leave matches no live generation and is idempotent.
        let focus_update = ctx.keyboard_focus.destroy_surface(wl_surface_guest_id);
        crate::handler::text_input::apply_keyboard_focus_changes(ctx, &focus_update.seat_changes);
        crate::handler::text_input::repair_destroyed_surface_focus(ctx, wl_surface_guest_id);
        for keyboard_id in focus_update.retired_keyboards {
            ctx.key_generations.clear_keyboard(keyboard_id);
        }
        // XDG objects are separate guest objects, but their role links are
        // owned by the placement state because app-ID and focus lookup use the
        // same chain. Remove the links atomically before releasing Aura
        // children so a later client ID reuse cannot route to this surface.
        let orphaned_toplevels = ctx
            .window_placement
            .take_xdg_links_for_wl_surface(wl_surface_guest_id);
        for toplevel_id in orphaned_toplevels {
            release_zaura_toplevel(ctx, toplevel_id);
        }
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
        surface_state.pending_attachment = if buffer == 0 {
            SurfaceAttachment::Detach
        } else {
            SurfaceAttachment::Attach(buffer)
        };
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
        crate::handler::shm::retire_eligible_buffers(ctx);
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

        let mut default_surface = SurfaceState::default();
        let commit = ctx.surfaces.get_mut(&surface_id).map_or_else(
            || default_surface.prepare_commit(),
            SurfaceState::prepare_commit,
        );
        let attached_buffer_id = commit.attached_buffer_id();
        let attached_full_mapping = attached_buffer_id.is_some() && commit.uses_full_mapping();
        // A native dma-buf acquire fence belongs to the new contents submitted
        // by an attach.  Re-running the dma-buf wait for a damage-only commit
        // blocks this single-threaded proxy even though the host compositor
        // is already using the same buffer.  ChromiumOS Sommelier waits at
        // attach time as well; keep explicit attach(NULL) and damage-only
        // commits non-blocking.
        let wait_for_native_sync = attached_buffer_id.is_some();

        // The viewporter protocol permits fractional source rectangles only
        // when a destination size is also set. If the source is fractional
        // and the destination is unset, the compositor must raise
        // wp_viewport.bad_size when this double-buffered state is applied.
        // Report it against the viewport object, if it is still present, and
        // do not forward a commit that would otherwise apply invalid state.
        if commit.has_invalid_fractional_viewport() {
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
            if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                commit.rollback(surface);
            }
            return Action::Drop;
        }

        let surface_damage = commit.surface_damage.rects();
        let buffer_damage = commit.buffer_damage.rects();
        // Keep a snapshot of the committed surface state for damage
        // translation. The buffer copy below can clear `needs_full_copy`, so
        // the pre-copy value is retained separately for the host damage
        // request.
        let needs_full_damage = attached_buffer_id
            .and_then(|buffer_id| ctx.local_buffer(buffer_id))
            .map(|buffer| buffer.needs_full_copy)
            .unwrap_or(false);
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
            &commit.state,
            force_full_damage,
        ) else {
            log::error!(
                "Unable to prepare an atomic host wl_surface.commit for {}",
                surface_id
            );
            ctx.fatal_protocol_error = true;
            if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                commit.rollback(surface);
            }
            crate::handler::shm::retire_eligible_buffers(ctx);
            return Action::Drop;
        };

        // A SHM-backed buffer is copied into host storage before the host
        // commit is queued. Forwarding the commit first would let the host
        // compositor sample stale or uninitialised pixels if dma-buf sync,
        // mmap, or GBM mapping fails. A failed copy leaves `needs_full_copy`
        // set so a later commit retries the complete image.
        let commit_ready =
            attached_buffer_id.is_none_or(|buffer_id| copy_surface_buffer(ctx, buffer_id, &commit));

        if !commit_ready {
            log::warn!(
                "Skipping host wl_surface.commit for {} because its SHM copy failed",
                surface_id
            );
            if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                commit.rollback(surface);
            }
            crate::handler::shm::retire_eligible_buffers(ctx);
            return Action::Drop;
        }

        if wait_for_native_sync {
            if let Some(buffer_id) = attached_buffer_id {
                // Native linux-dmabuf buffers bypass the local SHM copy path.
                // Wait for guest GPU writes before the host compositor samples
                // the buffer, matching ChromiumOS Sommelier's sync_point path.
                if let Err(error) = wait_for_native_buffer(ctx, buffer_id) {
                    log::error!(
                        "Native dma-buf synchronization failed for surface {}: {}",
                        surface_id,
                        error
                    );
                    // wl_surface.commit has no recoverable failure response.
                    // Disconnect rather than silently rolling back a request
                    // the guest reasonably believes was accepted.
                    ctx.fatal_protocol_error = true;
                    if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                        commit.rollback(surface);
                    }
                    crate::handler::shm::retire_eligible_buffers(ctx);
                    return Action::Drop;
                }
            }
        }

        if let Some((previous_buffer, next_buffer)) = commit.attachment_transition() {
            // The pending replacement has now passed every copy, fence, and
            // wire-encoding check. Apply its content snapshot and both buffer
            // lifecycle edges before appending any host message. The registry
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
                log::error!(
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
                ctx.fatal_protocol_error = true;
                if let Some(surface) = ctx.surfaces.get_mut(&surface_id) {
                    commit.rollback(surface);
                }
                crate::handler::shm::retire_eligible_buffers(ctx);
                return Action::Drop;
            }
        }

        // Damage and commit were encoded as one batch before local state was
        // finalized. Preserve their ordering while making a partial host
        // transaction impossible.
        ctx.client_to_host_queue.append(&mut host_messages);
        crate::handler::shm::retire_eligible_buffers(ctx);
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
        if !ctx.window_placement.remember_xdg_surface(id, surface) {
            log::warn!(
                "Refusing to replace xdg_surface {} association with wl_surface {}",
                id,
                surface
            );
            return Action::Drop;
        }
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_surface_id = ctx.last_sender_id;
        if let Some(wl_surface_id) = ctx.window_placement.take_xdg_surface(xdg_surface_id) {
            // A malformed client can destroy xdg_surface before its
            // xdg_toplevel. Do not leave a stale toplevel→surface association
            // that could apply a later app_id to an unrelated surface after ID
            // reuse.
            let orphaned_toplevels = ctx
                .window_placement
                .take_xdg_links_for_wl_surface(wl_surface_id);
            for toplevel_id in orphaned_toplevels {
                release_zaura_toplevel(ctx, toplevel_id);
            }
        }
        Action::Forward
    }

    fn on_get_toplevel(&mut self, ctx: &mut Context, id: u32) -> Action {
        let xdg_surface_id = ctx.last_sender_id;
        if let Some(wl_surface_id) = ctx
            .window_placement
            .wl_surface_for_xdg_surface(xdg_surface_id)
        {
            if !ctx
                .window_placement
                .remember_xdg_toplevel(id, wl_surface_id)
            {
                log::warn!(
                    "Refusing to replace xdg_toplevel {} association with wl_surface {}",
                    id,
                    wl_surface_id
                );
                return Action::Drop;
            }
            if ctx.window_placement.uses_arc_policy() {
                // Allocate the identity at role creation so both the XDG app
                // ID and a later GTK D-Bus metadata update can reuse it.
                let _ = ctx
                    .window_placement
                    .arc_policy_application_id(wl_surface_id);
            }
            // The generated dispatcher installs the guest→host mapping after
            // this callback, so the proxy retries Aura-child creation after
            // dispatch for the normal path.
            if ctx.window_placement.handles_shortcuts() {
                let _ = ensure_zaura_toplevel(ctx, id);
            }
        }
        Action::Forward
    }
}

impl crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler for CompositorHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_toplevel_id = ctx.last_sender_id;
        let Some(host_xdg_toplevel_id) = ctx.shadow_table.get_host_id(xdg_toplevel_id) else {
            log::error!(
                "Unable to destroy xdg_toplevel {} without its host generation",
                xdg_toplevel_id
            );
            ctx.fatal_protocol_error = true;
            return Action::Drop;
        };
        let Ok(destroy_message) = crate::wire::MessageBuilder::new()
            .try_build_message(host_xdg_toplevel_id, REQ_DESTROY_XDG_TOPLEVEL)
        else {
            log::error!(
                "Unable to encode xdg_toplevel.destroy for host object {}",
                host_xdg_toplevel_id
            );
            ctx.fatal_protocol_error = true;
            return Action::Drop;
        };
        // The Aura child is valid only while the xdg role exists. Queue the
        // role destructor first, then release the Aura child, and retain the
        // guest ID until the host acknowledges the destructor.
        ctx.client_to_host_queue.push((destroy_message, Vec::new()));
        ctx.window_placement.take_xdg_toplevel(xdg_toplevel_id);
        release_zaura_toplevel(ctx, xdg_toplevel_id);
        ctx.shadow_table.mark_pending_destroy(xdg_toplevel_id);
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
        let wl_surface_guest_id = ctx
            .window_placement
            .wl_surface_for_xdg_toplevel(xdg_toplevel_id);
        // Keep the XDG role in Sommelier's normal Guest OS namespace for shelf
        // and restore matching. In ARC mode the Aura surface uses the
        // task-form compatibility ID continuously: changing this metadata
        // around a bounds request makes Exo emit a leave/enter focus cycle,
        // which resets ChromeOS IME state after the first shortcut.
        let xdg_app_id = ctx.window_placement.native_wayland_app_id(app_id);
        if !wayland_string_fits_message(&xdg_app_id) {
            log::warn!(
                "Dropping oversized XDG application ID for xdg_toplevel {} ({} bytes)",
                xdg_toplevel_id,
                xdg_app_id.len()
            );
            return Action::Drop;
        }

        let arc_app_id = if ctx.window_placement.uses_arc_policy() {
            let Some(wl_surface_guest_id) = wl_surface_guest_id else {
                log::warn!(
                    "Cannot allocate ARC policy ID for xdg_toplevel {} without its wl_surface",
                    xdg_toplevel_id
                );
                return Action::Drop;
            };
            let Some(application_id) = ctx
                .window_placement
                .arc_policy_application_id(wl_surface_guest_id)
            else {
                log::warn!(
                    "ARC placement mode changed before app ID allocation for xdg_toplevel {}",
                    xdg_toplevel_id
                );
                return Action::Drop;
            };
            Some(application_id)
        } else {
            None
        };
        if let Some(arc_app_id) = arc_app_id.as_deref() {
            if !wayland_string_fits_message(arc_app_id) {
                log::warn!(
                    "Dropping oversized Aura application ID for xdg_toplevel {} ({} bytes)",
                    xdg_toplevel_id,
                    arc_app_id.len()
                );
                return Action::Drop;
            }
        }
        let aura_app_id = match ctx.window_placement.arc_id_lifetime() {
            crate::state::WindowArcIdLifetime::Persistent
            | crate::state::WindowArcIdLifetime::PersistentNativeShell
                if ctx.window_placement.uses_arc_policy() =>
            {
                arc_app_id.as_deref().unwrap_or(&xdg_app_id)
            }
            _ => &xdg_app_id,
        };
        if !wayland_string_fits_message(aura_app_id) {
            log::warn!(
                "Dropping oversized Aura application ID for xdg_toplevel {} ({} bytes)",
                xdg_toplevel_id,
                aura_app_id.len()
            );
            return Action::Drop;
        }

        // The host xdg_toplevel carries the app ID used by ordinary Exo
        // shelf/application matching. Keep this request in the normal Guest OS
        // namespace even when the Aura surface uses the ARC bounds policy.
        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_string(&xdg_app_id);
        let Ok(message) = builder.try_build_message(xdg_toplevel_host_id, REQ_SET_APP_ID) else {
            log::warn!(
                "Dropping oversized XDG application ID for xdg_toplevel {}",
                xdg_toplevel_id
            );
            return Action::Drop;
        };
        ctx.client_to_host_queue.push((message, Vec::new()));

        // Resolve xdg_toplevel → wl_surface (guest) → wl_surface (host).
        if let Some(wl_surface_guest_id) = wl_surface_guest_id {
            ctx.window_placement
                .remember_native_application_id(wl_surface_guest_id, xdg_app_id.clone());
            if let Some(zaura_surface_host_id) = ensure_host_zaura_surface(ctx, wl_surface_guest_id)
            {
                let zaura_surface_version = ctx
                    .shadow_table
                    .host_object_version(zaura_surface_host_id)
                    .unwrap_or(ctx.window_placement.aura_shell_version());
                if zaura_surface_version < 5 {
                    return Action::Drop;
                }
                if !queue_policy_application_id(
                    ctx,
                    zaura_surface_host_id,
                    &xdg_app_id,
                    arc_app_id.as_deref(),
                ) {
                    return Action::Drop;
                }
                log::debug!(
                    "Set application ID to {} (XDG: {}, Aura: {}) on zaura_surface (host_id={})",
                    app_id,
                    xdg_app_id,
                    aura_app_id,
                    zaura_surface_host_id
                );
            }
        }
        Action::Drop
    }
}

impl crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler for CompositorHandler {
    fn on_configure(
        &mut self,
        ctx: &mut Context,
        _x: i32,
        _y: i32,
        width: i32,
        height: i32,
        states: &[u8],
    ) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_xdg_toplevel_id) =
            ctx.window_placement.xdg_toplevel_for_aura_toplevel(host_id)
        else {
            return Action::Drop;
        };

        // The Aura screen-coordinate event uses the same client/contents
        // origin that `zaura_surface.set_parent` uses as its parent-space
        // origin. Ignore delayed intermediate positions while a self-parent
        // request is converging; the placement path already predicts its
        // requested target for rapid follow-up shortcuts.
        if !ctx.window_placement.record_origin(host_id, (_x, _y)) {
            log::debug!(
                "ignoring intermediate Aura origin for self-parent target: \
                 host={} origin=({}, {})",
                host_id,
                _x,
                _y
            );
        }

        let barrier_pending = ctx.window_placement.has_pending_barrier(host_id);
        log::debug!(
            "zaura_toplevel.configure host={} guest={} bounds={}x{} origin=({}, {}) \
             barrier_pending={}",
            host_id,
            guest_xdg_toplevel_id,
            width,
            height,
            _x,
            _y,
            barrier_pending
        );

        let mut builder = crate::wire::MessageBuilder::new();
        builder.write_i32(width);
        builder.write_i32(height);
        builder.write_array(states);
        let Ok(message) = builder.try_build_message(
            guest_xdg_toplevel_id,
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE,
        ) else {
            return Action::Drop;
        };
        ctx.host_to_client_queue.push((message, Vec::new()));
        Action::Drop
    }

    fn on_origin_change(&mut self, ctx: &mut Context, x: i32, y: i32) -> Action {
        let host_id = ctx.last_sender_id;
        if ctx
            .window_placement
            .xdg_toplevel_for_aura_toplevel(host_id)
            .is_none()
        {
            log::debug!(
                "dropping origin_change for unowned zaura_toplevel host={}",
                host_id
            );
            return Action::Drop;
        }
        if !ctx.window_placement.record_origin(host_id, (x, y)) {
            log::debug!(
                "ignoring intermediate Aura origin for self-parent target: \
                 host={} origin=({}, {})",
                host_id,
                x,
                y
            );
        }
        let barrier_pending = ctx.window_placement.has_pending_barrier(host_id);
        log::debug!(
            "zaura_toplevel.origin_change host={} origin=({}, {}) barrier_pending={}",
            host_id,
            x,
            y,
            barrier_pending
        );

        // xdg_toplevel has no origin-change event. A subsequent Aura
        // configure carries the authoritative size/state. The sync callback
        // only orders host processing; origin prediction above decides
        // whether an intermediate position should be recorded.
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::display::DisplayHandler;
    use crate::protocols::aura_shell::zaura_shell::{
        REQ_GET_AURA_SURFACE, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
    };
    use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE;
    use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
    use crate::protocols::viewporter::wp_viewport::WpViewportHandler;
    use crate::protocols::viewporter::wp_viewporter::WpViewporterHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::wl_output::WlOutputHandler;
    use crate::protocols::wayland::wl_surface::WlSurfaceHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::REQ_SET_APP_ID;
    use crate::state::{
        BufferState, Context, PoolInner, PoolState, RenderBufferLifecycle, RenderBufferUse,
        ARC_TASK_APPLICATION_ID_PREFIX, ARC_TASK_ID_POOL_END, ARC_TASK_ID_POOL_START,
    };
    use std::os::fd::{FromRawFd, OwnedFd};
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
        ctx.window_placement
            .set_aura_shell_binding_for_test(zaura_shell_host, 38);
        assert!(ctx
            .window_placement
            .remember_xdg_surface(xdg_surface_id, wl_surface_guest));
        assert!(ctx
            .window_placement
            .remember_xdg_toplevel(xdg_toplevel_id, wl_surface_guest));

        (ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host)
    }

    #[test]
    fn wl_output_release_retires_placement_geometry() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let output_guest_id = 10;
        let output_host_id = 50;
        ctx.shadow_table.map_id(output_guest_id, output_host_id);
        assert!(ctx.window_placement.remember_output(output_host_id));
        ctx.window_placement
            .update_output_mode(output_host_id, true, 3840, 2160);
        ctx.window_placement.update_output_scale(output_host_id, 1);
        ctx.last_sender_id = output_guest_id;

        assert_eq!(
            WlOutputHandler::on_release(&mut CompositorHandler, &mut ctx),
            Action::Forward
        );
        assert_eq!(ctx.window_placement.primary_output(), None);

        // A delayed event after release must not recreate the retired output.
        ctx.window_placement
            .update_output_mode(output_host_id, true, 1920, 1080);
        assert_eq!(ctx.window_placement.primary_output(), None);
    }

    fn active_text_input_state(
        host_v1_id: u32,
        guest_seat: u32,
        active_surface: u32,
    ) -> crate::state::TextInputState {
        crate::state::TextInputState {
            host_v1_id,
            host_ext_id: None,
            guest_seat,
            active_surface: Some(active_surface),
            pending_enabled: true,
            committed_enabled: true,
            enabled_dirty: true,
            pending_surrounding_text: Some(("한".to_string(), 3, 3)),
            committed_surrounding_text: Some(("한".to_string(), 3, 3)),
            surrounding_text_dirty: true,
            content_hint: 1,
            content_purpose: 1,
            committed_content_type: Some((1, 1)),
            content_type_dirty: true,
            cursor_rect: Some((1, 2, 3, 4)),
            cursor_rect_dirty: true,
            text_change_cause: 1,
            current_preedit: "한".to_string(),
            guest_commit_serial: 1,
            pending_preedit_cursor: Some(1),
            pending_preedit_selection: Some((0, 1)),
            pending_deletes: vec![(1, 1)],
            pending_cursor_position: Some((1, 1)),
            host_activation: crate::state::HostActivationState::Active,
        }
    }

    fn mapped_test_buffer(
        _guest_buffer_id: u32,
        width: i32,
        height: i32,
        stride: u32,
        needs_full_copy: bool,
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
            pool,
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
            needs_full_copy,
        }
    }

    fn register_test_local(ctx: &mut Context, guest_id: u32, backing: BufferState) {
        let host_id = ctx.shadow_table.get_host_id(guest_id).unwrap_or_else(|| {
            let host_id = guest_id + 1;
            ctx.shadow_table.map_id(guest_id, host_id);
            ctx.shadow_table
                .track_interface_with_version(guest_id, "wl_buffer".to_string(), 1);
            ctx.shadow_table.set_host_version(host_id, 1);
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
            ctx.shadow_table.map_id(guest_id, host_id);
            ctx.shadow_table
                .track_interface_with_version(guest_id, "wl_buffer".to_string(), 1);
            ctx.shadow_table.set_host_version(host_id, 1);
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
        ctx.surfaces
            .entry(surface_id)
            .or_default()
            .set_current_buffer_for_test(buffer_id, dimensions);
    }

    #[test]
    fn unsupported_native_sync_uses_cached_implicit_fallback_and_commits() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();
        let surface = 100;
        let buffer = 50;
        ctx.allocator = None;
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
        assert!(!ctx.fatal_protocol_error);
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
        assert!(!ctx.fatal_protocol_error);
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
        ctx.allocator = None;
        let sync_fd = invalid_ioctl_fd();
        let error = crate::allocator::wait_for_dmabuf(sync_fd.as_raw_fd(), None).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        register_test_native_with_sync_fds(&mut ctx, buffer, (1, 1), vec![sync_fd]);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = surface;
        assert_eq!(handler.on_attach(&mut ctx, buffer, 0, 0), Action::Forward);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.fatal_protocol_error);
        assert!(!ctx.native_buffer_uses_implicit_sync(buffer));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| msg_opcode(message) != REQ_COMMIT),
            "a commit whose synchronization failed must not reach the host"
        );
        assert_eq!(
            ctx.surfaces.get(&surface).unwrap().pending_attachment,
            SurfaceAttachment::Attach(buffer),
            "fatal teardown still rolls back the prepared surface transaction"
        );
    }

    #[test]
    fn mapped_buffer_damage_copies_only_buffer_pixel_rectangles() {
        const WIDTH: usize = 4;
        const HEIGHT: usize = 4;
        const STRIDE: usize = WIDTH * 4;
        let source: Vec<u8> = (0..STRIDE * HEIGHT).map(|value| value as u8).collect();
        let mut destination = vec![0xee; STRIDE * HEIGHT];
        let damage = [DamageRect::new(1, 1, 1, 2)];

        assert!(copy_damage_to_ptr(
            0,
            0,
            WIDTH as i32,
            HEIGHT as i32,
            STRIDE as u32,
            false,
            source.as_ptr(),
            source.len(),
            destination.as_mut_ptr(),
            destination.len(),
            STRIDE,
            0,
            0,
            &[],
            &damage,
            true,
        ));

        for index in 0..destination.len() {
            let row = index / STRIDE;
            let column = index % STRIDE;
            let in_damage = (row == 1 || row == 2) && (4..8).contains(&column);
            assert_eq!(
                destination[index],
                if in_damage { source[index] } else { 0xee },
                "unexpected copy at byte {index}"
            );
        }
    }

    #[test]
    fn mapped_surface_damage_uses_conservative_full_copy() {
        const WIDTH: usize = 4;
        const HEIGHT: usize = 4;
        const STRIDE: usize = WIDTH * 4;
        let source: Vec<u8> = (0..STRIDE * HEIGHT).map(|value| value as u8).collect();
        let mut destination = vec![0xee; STRIDE * HEIGHT];

        assert!(copy_damage_to_ptr(
            0,
            0,
            WIDTH as i32,
            HEIGHT as i32,
            STRIDE as u32,
            false,
            source.as_ptr(),
            source.len(),
            destination.as_mut_ptr(),
            destination.len(),
            STRIDE,
            0,
            0,
            &[DamageRect::new(1, 1, 1, 1)],
            &[],
            true,
        ));
        assert_eq!(destination, source);
    }

    #[test]
    fn initialized_buffer_without_damage_performs_no_copy() {
        const WIDTH: usize = 4;
        const HEIGHT: usize = 4;
        const STRIDE: usize = WIDTH * 4;
        let source = [0x11; STRIDE * HEIGHT];
        let mut destination = vec![0xee; STRIDE * HEIGHT];

        assert!(copy_damage_to_ptr(
            0,
            0,
            WIDTH as i32,
            HEIGHT as i32,
            STRIDE as u32,
            false,
            source.as_ptr(),
            source.len(),
            destination.as_mut_ptr(),
            destination.len(),
            STRIDE,
            0,
            0,
            &[],
            &[],
            true,
        ));
        assert_eq!(destination, vec![0xee; STRIDE * HEIGHT]);
    }

    #[test]
    fn collapsed_damage_copies_the_complete_buffer() {
        const WIDTH: usize = 4;
        const HEIGHT: usize = 4;
        const STRIDE: usize = WIDTH * 4;
        let source: Vec<u8> = (0..STRIDE * HEIGHT).map(|value| value as u8).collect();
        let mut destination = vec![0xee; STRIDE * HEIGHT];

        assert!(copy_damage_to_ptr(
            0,
            0,
            WIDTH as i32,
            HEIGHT as i32,
            STRIDE as u32,
            false,
            source.as_ptr(),
            source.len(),
            destination.as_mut_ptr(),
            destination.len(),
            STRIDE,
            0,
            0,
            &[],
            &[DamageRect::new(0, 0, i32::MAX, i32::MAX)],
            false,
        ));
        assert_eq!(destination, source);
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
        ctx.shadow_table.retire_guest_object(guest_id);
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
        assert!(ctx
            .window_placement
            .remember_aura_surface(wl_surface_host, zaura_surface_host));

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
    fn set_app_id_keeps_arc_task_identity_on_aura_surface() {
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_app_id(&mut ctx, &"com.example.Terminal".to_string()),
            Action::Drop
        );

        // The steady-state sequence is:
        //   xdg_toplevel.set_app_id(native guest ID)
        //   zaura_shell.get_aura_surface(...)
        //   zaura_surface.set_application_id(ARC task ID)
        //
        // The task-form ID remains stable so placement does not cause an
        // additional Aura app-ID transition and IME focus reset.
        // The rewritten handler returns Drop because it queues the translated
        // XDG request itself; assert the complete queue rather than merely
        // searching for the final ARC string.
        assert_eq!(ctx.client_to_host_queue.len(), 3);
        let xdg_message = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(xdg_message), xdg_toplevel_id + 100);
        assert_eq!(msg_opcode(xdg_message), REQ_SET_APP_ID);
        let xdg_payload = &xdg_message[8..];
        let xdg_len = u32::from_ne_bytes(xdg_payload[0..4].try_into().unwrap()) as usize;
        let xdg_app_id =
            std::str::from_utf8(&xdg_payload[4..4 + xdg_len - 1]).expect("valid guest app ID");
        assert_eq!(
            xdg_app_id,
            ctx.window_placement
                .native_wayland_app_id("com.example.Terminal")
        );

        let get_surface = &ctx.client_to_host_queue[1].0;
        assert_eq!(msg_sender(get_surface), zaura_shell_host);
        assert_eq!(msg_opcode(get_surface), REQ_GET_AURA_SURFACE);

        let set_aura_id = &ctx.client_to_host_queue[2].0;
        assert_eq!(msg_opcode(set_aura_id), REQ_SET_APPLICATION_ID);
        let aura_surface_host = msg_sender(set_aura_id);
        assert_eq!(
            Some(aura_surface_host),
            ctx.window_placement
                .aura_surface_for_wl_surface(wl_surface_host)
        );
        let payload = &set_aura_id[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let app_id =
            std::str::from_utf8(&payload[4..4 + str_len - 1]).expect("valid ARC application ID");
        let arc_id = ctx
            .window_placement
            .arc_policy_application_id(100)
            .expect("ARC task ID allocated for placement");
        assert_eq!(app_id, arc_id);
        let task_id = arc_id
            .strip_prefix(ARC_TASK_APPLICATION_ID_PREFIX)
            .expect("ARC task-form application ID");
        let task_id = task_id.parse::<u32>().expect("numeric ARC task ID");
        assert!((ARC_TASK_ID_POOL_START..=ARC_TASK_ID_POOL_END).contains(&task_id));
    }

    #[test]
    fn transient_arc_mode_keeps_native_aura_identity_until_placement() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.window_placement.set_mode_for_test(
            crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            )
            .with_arc_id_lifetime(crate::state::WindowArcIdLifetime::Transient),
        );
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_app_id(&mut ctx, &"com.example.Terminal".to_string()),
            Action::Drop
        );
        let message = ctx
            .client_to_host_queue
            .last()
            .expect("transient mode still sets an Aura application ID");
        let payload = &message.0[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let aura_app_id =
            std::str::from_utf8(&payload[4..4 + str_len - 1]).expect("valid native application ID");
        assert_eq!(
            aura_app_id,
            ctx.window_placement
                .native_wayland_app_id("com.example.Terminal")
        );
    }

    #[test]
    fn persistent_native_shell_mode_queues_arc_then_native_identity() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.window_placement.set_mode_for_test(
            crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            )
            .with_arc_id_lifetime(crate::state::WindowArcIdLifetime::PersistentNativeShell),
        );
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_app_id(&mut ctx, &"com.example.Terminal".to_string()),
            Action::Drop
        );
        let aura_messages = ctx
            .client_to_host_queue
            .iter()
            .filter(|(message, _)| msg_opcode(message) == REQ_SET_APPLICATION_ID)
            .collect::<Vec<_>>();
        assert_eq!(
            aura_messages.len(),
            2,
            "ARC authorization must be followed by native shell restoration"
        );
        let arc_payload = &aura_messages[0].0[8..];
        let arc_len = u32::from_ne_bytes(arc_payload[0..4].try_into().unwrap()) as usize;
        let arc_app_id =
            std::str::from_utf8(&arc_payload[4..4 + arc_len - 1]).expect("valid ARC ID");
        assert!(arc_app_id.starts_with(ARC_TASK_APPLICATION_ID_PREFIX));
        let native_payload = &aura_messages[1].0[8..];
        let native_len = u32::from_ne_bytes(native_payload[0..4].try_into().unwrap()) as usize;
        let native_app_id =
            std::str::from_utf8(&native_payload[4..4 + native_len - 1]).expect("valid native ID");
        assert_eq!(
            native_app_id,
            ctx.window_placement
                .native_wayland_app_id("com.example.Terminal")
        );
    }

    #[test]
    fn arc_policy_keeps_xdg_app_id_in_guest_namespace() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            handler.on_set_app_id(&mut ctx, &"com.example.Terminal".to_string()),
            Action::Drop
        );

        let xdg_message = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| msg_opcode(message) == REQ_SET_APP_ID)
            .expect("host XDG app ID request");
        let xdg_payload = &xdg_message.0[8..];
        let xdg_len = u32::from_ne_bytes(xdg_payload[0..4].try_into().unwrap()) as usize;
        let xdg_app_id =
            std::str::from_utf8(&xdg_payload[4..4 + xdg_len - 1]).expect("valid XDG app ID");
        assert_eq!(
            xdg_app_id,
            ctx.window_placement
                .native_wayland_app_id("com.example.Terminal")
        );
        assert!(
            !xdg_app_id.starts_with("org.chromium.arc."),
            "ARC policy must not relabel the host XDG role"
        );
    }

    #[test]
    fn set_app_id_noop_when_no_zaura_shell() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.window_placement.clear_aura_shell_for_test();
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"no_shell".to_string());
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
    }

    #[test]
    fn set_app_id_noop_when_version_below_5() {
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.window_placement
            .set_aura_shell_binding_for_test(zaura_shell_host, 4);
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        let action = handler.on_set_app_id(&mut ctx, &"old_host".to_string());
        assert_eq!(action, Action::Drop);

        // set_application_id is skipped when the host aura-shell version is
        // too old, but both the namespaced xdg request and Aura binding are
        // still queued.
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue[0].0), REQ_SET_APP_ID);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            REQ_GET_AURA_SURFACE
        );

        // zaura_surface should still be tracked for cleanup
        assert!(ctx
            .window_placement
            .aura_surface_for_wl_surface(wl_surface_host)
            .is_some());
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
            mapped_test_buffer(buffer_id, 1, 1, 4, false),
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
            mapped_test_buffer(buffer_id, 1, 1, 4, false),
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
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
    fn attaching_idle_shm_buffer_without_commit_keeps_release_edge() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        let host_buffer_id = 43;
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, false),
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 1, 1, 4, true),
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
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
        ctx.shadow_table.map_id(old_buffer, old_host_buffer);
        ctx.shadow_table
            .track_interface_with_version(old_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(old_host_buffer, 1);
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
        ctx.shadow_table.map_id(old_buffer, old_host_buffer);
        ctx.shadow_table
            .track_interface_with_version(old_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(old_host_buffer, 1);
        register_test_local(
            &mut ctx,
            old_buffer,
            mapped_test_buffer(old_buffer, 1, 1, 4, false),
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
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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
    fn damage_only_commit_does_not_recopy_mutated_released_shm_buffer() {
        const BUFFER_BYTES: usize = 4;

        // Use real anonymous mappings so the test detects any accidental copy
        // after the explicit attachment has already been committed.
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
        register_test_local(
            &mut ctx,
            buffer_id,
            BufferState {
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: BUFFER_BYTES as u32,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: BUFFER_BYTES as u32,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: destination_ptr,
                dest_size: BUFFER_BYTES,
                needs_full_copy: true,
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

        // No attach is submitted here. The surface keeps its existing content,
        // and the mutated wl_buffer storage must not be sampled again.
        assert_eq!(handler.on_damage(&mut ctx, 0, 0, 1, 1), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(destination_ptr, BUFFER_BYTES),
                &[0x11; BUFFER_BYTES]
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
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table
            .track_interface_with_version(buffer_id, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer_id, 1);
        register_test_local(
            &mut ctx,
            buffer_id,
            BufferState {
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: BUFFER_BYTES as u32,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: BUFFER_BYTES as u32,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: destination_ptr,
                dest_size: BUFFER_BYTES,
                needs_full_copy: true,
            },
        );
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
                .map(|surface| surface.pending_attachment),
            Some(SurfaceAttachment::Attach(buffer_id))
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
                .map(|surface| surface.pending_attachment)
                .filter(|attachment| *attachment != SurfaceAttachment::Unchanged),
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
    fn v5_zero_attach_does_not_create_an_attach_offset() {
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
                .and_then(|surface| surface.pending_offset),
            None,
            "commit must consume the one-shot offset"
        );

        // Version 5+ ignores attach's zero coordinates, so the new attachment
        // must not acquire a synthetic legacy offset.
        assert_eq!(handler.on_attach(&mut ctx, 42, 0, 0), Action::Forward);
        assert_eq!(
            ctx.surfaces
                .get(&surface_id)
                .and_then(|surface| surface.pending_attach_offset),
            None
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
        ctx.shadow_table
            .track_interface_with_version(surface_id, "wl_surface".to_string(), 4);
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;

        assert_eq!(handler.on_attach(&mut ctx, 42, 3, 4), Action::Forward);
        ctx.surfaces
            .get_mut(&surface_id)
            .expect("surface state")
            .pending_offset = Some((8, 9));
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
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, true),
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
        let mut buffer = mapped_test_buffer(buffer_id, 8, 6, 32, true);
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
                .is_some_and(|buffer| buffer.needs_full_copy),
            "a failed copy must force a complete retry on the next commit"
        );
    }

    #[test]
    fn missing_host_surface_rolls_back_the_complete_commit() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        ctx.last_sender_id = surface_id;
        let mut handler = CompositorHandler;
        assert_eq!(handler.on_damage(&mut ctx, 1, 2, 3, 4), Action::Drop);
        assert_eq!(handler.on_offset(&mut ctx, 5, 6), Action::Forward);
        let pending = ctx.surfaces.get(&surface_id).unwrap().clone();
        ctx.shadow_table.remove_id(surface_id);

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.fatal_protocol_error);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(ctx.surfaces.get(&surface_id), Some(&pending));
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
        let pending = ctx.surfaces.get(&surface_id).unwrap().clone();

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.fatal_protocol_error);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(ctx.surfaces.get(&surface_id), Some(&pending));
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
        assert!(surface.pending_surface_damage.is_empty());
        assert!(surface.pending_buffer_damage.is_empty());

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
        let mut surface = SurfaceState::default();
        surface.current_buffer_scale = 2;

        let mapped = map_buffer_damage(DamageRect::new(20, 10, 4, 6), &surface, 100, 100);

        assert_eq!(mapped, DamageRect::new(9, 4, 4, 5));
    }

    #[test]
    fn full_mapping_without_explicit_damage_still_damages_the_host_surface() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let surface_id = 100;
        let buffer_id = 42;
        register_test_local(
            &mut ctx,
            buffer_id,
            mapped_test_buffer(buffer_id, 8, 6, 32, false),
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
        let mut surface = SurfaceState::default();
        surface.pending_offset = Some((80, 90));
        let commit = surface.prepare_commit();
        assert_eq!(commit.buffer_offset, (80, 90));

        let mapped = map_buffer_damage(DamageRect::new(4, 5, 6, 7), &commit.state, 100, 100);

        assert_eq!(
            mapped,
            DamageRect::new(3, 4, 8, 9),
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
            mapped_test_buffer(buffer_id, 8, 6, 32, false),
        );
        let mut surface = SurfaceState::default();
        surface.pending_offset = Some((3, 0));
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
            mapped_test_buffer(buffer_id, 8, 6, 32, false),
        );
        let mut surface = SurfaceState::default();
        surface.pending_offset = Some((3, 0));
        ctx.surfaces.insert(surface_id, surface);
        set_test_surface_content(&mut ctx, surface_id, Some(buffer_id), Some((8, 6)));
        ctx.last_sender_id = surface_id;

        let mut handler = CompositorHandler;
        assert_eq!(handler.on_damage_buffer(&mut ctx, 1, 1, 1, 1), Action::Drop);
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
        let mut surface = SurfaceState::default();
        surface.current_buffer_transform = 1;

        assert_eq!(
            full_surface_damage(&surface, 8, 6),
            DamageRect::new(0, 0, 6, 8),
            "90-degree transforms expose a height-by-width surface extent"
        );
    }

    #[test]
    fn rotated_scaled_buffer_full_damage_uses_logical_extent() {
        let mut surface = SurfaceState::default();
        surface.current_buffer_scale = 2;
        surface.current_buffer_transform = 1;

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
        register_test_local(
            &mut ctx,
            42,
            mapped_test_buffer(42, 2048, 2048, 8192, false),
        );
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

        assert_eq!(
            ctx.window_placement
                .wl_surface_for_xdg_toplevel(xdg_toplevel_id),
            Some(100)
        );

        ctx.last_sender_id = xdg_toplevel_id;
        let action = XdgToplevelHandler::on_destroy(&mut handler, &mut ctx);
        assert_eq!(action, Action::Drop);

        assert_eq!(
            ctx.window_placement
                .wl_surface_for_xdg_toplevel(xdg_toplevel_id),
            None
        );
    }

    #[test]
    fn ensure_zaura_toplevel_requests_screen_coordinates() {
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id),
            Some(aura_id)
        );
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_sender(&ctx.client_to_host_queue[0].0), zaura_shell_host);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[0].0),
            REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue[1].0), aura_id);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES
        );
    }

    #[test]
    fn xdg_toplevel_destroy_reserves_aura_id_until_host_delete_id() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = xdg_toplevel_id;

        assert_eq!(
            XdgToplevelHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id),
            None
        );
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[0].0),
            REQ_DESTROY_XDG_TOPLEVEL
        );
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            REQ_RELEASE_AURA_TOPLEVEL
        );
        assert!(ctx.shadow_table.is_pending_destroy_host_only(aura_id));
        assert!(!ctx.shadow_table.is_host_id_available(aura_id));

        ctx.last_sender_id = 1;
        assert_eq!(DisplayHandler.on_delete_id(&mut ctx, aura_id), Action::Drop);
        assert!(!ctx.shadow_table.is_pending_destroy_host_only(aura_id));
        assert!(ctx.shadow_table.is_host_id_available(aura_id));
    }

    #[test]
    fn window_placement_barrier_is_queued_after_placement_request() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();

        assert!(queue_window_placement_barrier(&mut ctx, aura_id));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(message), 1);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::wayland::wl_display::REQ_SYNC
        );
        let callback_id = u32::from_ne_bytes(message[8..12].try_into().unwrap());
        assert_eq!(
            ctx.window_placement.barrier_for_callback(callback_id),
            Some(aura_id)
        );
        assert_eq!(
            ctx.window_placement.active_barrier_for_toplevel(aura_id),
            Some(callback_id)
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(callback_id),
            Some(&"wl_callback".to_string())
        );
    }

    #[test]
    fn aura_configure_is_translated_to_guest_xdg_configure() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        assert!(ctx
            .window_placement
            .remember_aura_toplevel(xdg_toplevel_id, 77));
        ctx.shadow_table
            .track_host_interface_with_version(77, "zaura_toplevel".to_string(), 38);
        ctx.last_sender_id = 77;

        let states = [1u8, 2, 3, 4];
        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                10,
                20,
                1920,
                1080,
                &states,
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.window_placement.origin(77), Some((10, 20)));
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(msg_sender(message), xdg_toplevel_id);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
        let mut expected = Vec::new();
        expected.extend_from_slice(&1920i32.to_ne_bytes());
        expected.extend_from_slice(&1080i32.to_ne_bytes());
        expected.extend_from_slice(&4u32.to_ne_bytes());
        expected.extend_from_slice(&states);
        assert_eq!(&message[8..], expected.as_slice());
    }

    #[test]
    fn stale_self_parent_origin_does_not_rebase_following_shortcut() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        assert!(ctx
            .window_placement
            .remember_aura_toplevel(xdg_toplevel_id, 77));
        ctx.shadow_table
            .track_host_interface_with_version(77, "zaura_toplevel".to_string(), 38);
        assert!(ctx.window_placement.record_origin(77, (100, 200)));
        assert!(ctx.window_placement.predict_origin(77, (0, 0)));
        ctx.last_sender_id = 77;

        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                80,
                160,
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(
            ctx.window_placement.origin(77),
            Some((0, 0)),
            "an intermediate animation origin must not replace the predicted target"
        );
        assert_eq!(
            ctx.window_placement.pending_origin(77),
            Some((0, 0)),
            "the target remains pending until the matching origin arrives"
        );

        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.window_placement.origin(77), Some((0, 0)));
        assert_eq!(ctx.window_placement.pending_origin(77), None);
    }

    #[test]
    fn stale_origin_change_cannot_recreate_released_toplevel_state() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        assert!(ctx.window_placement.record_origin(aura_id, (10, 20)));
        release_zaura_toplevel(&mut ctx, xdg_toplevel_id);
        ctx.last_sender_id = aura_id;

        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                100,
                200,
            );

        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.window_placement.origin(aura_id), None);
        assert_eq!(
            ctx.window_placement.xdg_toplevel_for_aura_toplevel(aura_id),
            None
        );
    }

    #[test]
    fn aura_origin_change_is_consumed_during_placement_barrier() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        assert!(queue_window_placement_barrier(&mut ctx, aura_id));
        ctx.last_sender_id = aura_id;

        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                123,
                456,
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.window_placement.origin(aura_id), Some((123, 456)));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn wl_surface_destroy_releases_zaura_surface() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) = setup_ctx();

        ctx.last_sender_id = xdg_toplevel_id;
        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"app".to_string());

        let zaura_surface_host = ctx
            .window_placement
            .aura_surface_for_wl_surface(wl_surface_host)
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

        assert_eq!(
            ctx.window_placement
                .aura_surface_for_wl_surface(wl_surface_host),
            None
        );
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

        ctx.shadow_table.map_id(second_surface, second_surface_host);
        ctx.shadow_table
            .track_interface_with_version(second_surface, "wl_surface".to_string(), 5);
        ctx.shadow_table.set_host_version(second_surface_host, 5);
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
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

        ctx.shadow_table.map_id(second_surface, second_surface_host);
        ctx.shadow_table
            .track_interface_with_version(second_surface, "wl_surface".to_string(), 5);
        ctx.shadow_table.set_host_version(second_surface_host, 5);
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        register_test_native(&mut ctx, guest_buffer, (1, 1));
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        assert!(ctx.mark_buffer_submitted(guest_buffer));
        set_test_surface_content(&mut ctx, first_surface, Some(guest_buffer), None);
        ctx.surfaces
            .entry(second_surface)
            .or_default()
            .pending_attachment = SurfaceAttachment::Attach(guest_buffer);

        let mut handler = CompositorHandler;
        ctx.last_sender_id = first_surface;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            buffer_lifecycle(&ctx, guest_buffer),
            Some(RenderBufferLifecycle::GuestDestroyed(
                RenderBufferUse::NeverSubmitted
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        register_test_local(
            &mut ctx,
            guest_buffer,
            mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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

        ctx.shadow_table.map_id(second_surface, second_surface_host);
        ctx.shadow_table
            .track_interface_with_version(second_surface, "wl_surface".to_string(), 5);
        ctx.shadow_table.set_host_version(second_surface_host, 5);
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        if native {
            register_test_native(&mut ctx, guest_buffer, (1, 1));
        } else {
            register_test_local(
                &mut ctx,
                guest_buffer,
                mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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

        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        if native {
            register_test_native(&mut ctx, guest_buffer, (1, 1));
        } else {
            register_test_local(
                &mut ctx,
                guest_buffer,
                mapped_test_buffer(guest_buffer, 1, 1, 4, false),
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
        ctx.last_sender_id = wl_surface_guest_id;

        let mut handler = CompositorHandler;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.window_placement.wl_surface_for_xdg_surface(500), None);
        assert_eq!(ctx.window_placement.wl_surface_for_xdg_toplevel(400), None);
        // The setup toplevel is also removed because it points to the same
        // surface; no stale association should survive destruction.
        assert_eq!(
            ctx.window_placement
                .wl_surface_for_xdg_toplevel(xdg_toplevel_id),
            None
        );
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

        ctx.shadow_table.map_id(1, 2);
        ctx.shadow_table.track_interface(1, "wl_seat".to_string());
        ctx.shadow_table.map_id(text_input, 701);
        ctx.shadow_table
            .track_interface(text_input, "zwp_text_input_v3".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard, 1);
        ctx.keyboard_focus.set_for_test(
            crate::state::HostId(host_keyboard),
            1,
            surface,
            _wl_surface_host,
        );
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
                committed_content_type: None,
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
                host_activation: crate::state::HostActivationState::Active,
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

        ctx.shadow_table.map_id(live_surface, live_host_surface);
        for (guest_seat, host_seat) in seats {
            ctx.shadow_table.map_id(guest_seat, host_seat);
            ctx.shadow_table
                .track_interface(guest_seat, "wl_seat".to_string());
        }
        for ((guest_keyboard, host_keyboard), (guest_seat, _)) in keyboards.into_iter().zip(seats) {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.shadow_table
                .track_interface(guest_keyboard, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        }
        for (((guest_text_input, host_object, host_v1), (guest_seat, _)), active_surface) in
            text_inputs.into_iter().zip(seats).zip([
                destroyed_surface,
                destroyed_surface,
                live_surface,
            ])
        {
            ctx.shadow_table.map_id(guest_text_input, host_object);
            ctx.shadow_table
                .track_interface(guest_text_input, "zwp_text_input_v3".to_string());
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
            assert!(!state.committed_enabled);
            assert!(!state.host_is_active());
        }
        let unaffected = &ctx.text_inputs[&720];
        assert_eq!(unaffected.active_surface, Some(live_surface));
        assert!(unaffected.committed_enabled);
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
            ctx.shadow_table.map_id(guest_seat, host_seat);
            ctx.shadow_table
                .track_interface(guest_seat, "wl_seat".to_string());
        }
        ctx.shadow_table
            .map_id(replacement_surface, replacement_host_surface);
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
            ctx.shadow_table.map_id(guest_text_input, host_object);
            ctx.shadow_table
                .track_interface(guest_text_input, "zwp_text_input_v3".to_string());
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
        assert!(!repaired.committed_enabled);
        assert!(!repaired.host_is_active());
        assert!(repaired.current_preedit.is_empty());

        let repaired_without_owner = &ctx.text_inputs[&stale_without_owner];
        assert_eq!(repaired_without_owner.active_surface, None);
        assert!(!repaired_without_owner.committed_enabled);
        assert!(!repaired_without_owner.host_is_active());

        let healthy = &ctx.text_inputs[&healthy_on_replacement];
        assert_eq!(healthy.active_surface, Some(replacement_surface));
        assert!(healthy.committed_enabled);
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
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host) = setup_ctx();
        ctx.window_placement
            .set_aura_shell_binding_for_test(zaura_shell_host, 37);
        ctx.last_sender_id = xdg_toplevel_id;

        let mut handler = CompositorHandler;
        handler.on_set_app_id(&mut ctx, &"legacy".to_string());
        let zaura_surface_host = ctx
            .window_placement
            .aura_surface_for_wl_surface(wl_surface_host)
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
