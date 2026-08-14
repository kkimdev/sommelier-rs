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
use crate::protocols;
use crate::state::{BufferState, Context, DamageRect, PoolInner, PoolState};
use crate::wire::{Action, MessageBuilder};
use log::{debug, error, warn};
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
use std::ptr;
use std::sync::{Arc, RwLock};

pub struct ShmHandler;

fn queue_message(
    queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>,
    sender_id: u32,
    opcode: u16,
    builder: MessageBuilder,
    fds: Vec<RawFd>,
) -> bool {
    match builder.try_build_message(sender_id, opcode) {
        Ok(message) => {
            queue.push((message, fds));
            true
        }
        Err(error) => {
            warn!(
                "Dropping oversized SHM message sender={} opcode={}: {}",
                sender_id, opcode, error
            );
            for fd in fds {
                if fd >= 0 {
                    let _ = nix::unistd::close(fd);
                }
            }
            false
        }
    }
}

/// FourCC value used by ChromiumOS' wl_shm extension for semi-planar NV12.
///
/// wl_shm's original enum uses the same FourCC values as DRM for formats
/// added after ARGB/XRGB. Keeping the value in one place prevents the SHM and
/// linux-dmabuf shims from drifting apart.
pub(crate) const WL_SHM_FORMAT_NV12: u32 = 0x3231_564e;

/// Formats advertised by ChromiumOS Sommelier's wl_shm shim. The first two
/// values are the Wayland ARGB/XRGB enum values; the remaining FourCC values
/// are also the wl_shm values for NV12/RGB565/ABGR/XBGR.
pub(crate) const SUPPORTED_SHM_FORMATS: [u32; 6] = [
    0, // WL_SHM_FORMAT_ARGB8888
    1, // WL_SHM_FORMAT_XRGB8888
    WL_SHM_FORMAT_NV12,
    0x3631_4752, // WL_SHM_FORMAT_RGB565
    0x3432_4241, // WL_SHM_FORMAT_ABGR8888
    0x3432_4258, // WL_SHM_FORMAT_XBGR8888
];

const MANDATORY_SHM_FORMATS: [u32; 2] = [
    0, // WL_SHM_FORMAT_ARGB8888
    1, // WL_SHM_FORMAT_XRGB8888
];

fn valid_pool_size(size: i32) -> Option<usize> {
    (size > 0)
        .then_some(size as usize)
        .filter(|&size| size <= isize::MAX as usize)
}

fn valid_pool_resize(current_size: usize, new_size: i32) -> Option<usize> {
    let new_size = valid_pool_size(new_size)?;
    (new_size > current_size).then_some(new_size)
}

fn supported_shm_format(format: u32) -> bool {
    SUPPORTED_SHM_FORMATS.contains(&format)
}

fn shm_format_from_drm(format: u32) -> Option<u32> {
    match format {
        // DRM_FORMAT_ARGB8888/XRGB8888 use FourCC values while wl_shm uses
        // the enum values 0/1 for the same layouts.
        0x3432_5241 => Some(0),
        0x3432_5258 => Some(1),
        WL_SHM_FORMAT_NV12 => Some(WL_SHM_FORMAT_NV12),
        format if supported_shm_format(format) => Some(format),
        _ => None,
    }
}

fn guest_shm_format_available(ctx: &Context, format: u32) -> bool {
    // The Rust bridge allocates a contiguous VirtWL buffer and copies both
    // NV12 planes into it. A GBM BO may use implementation-defined plane
    // offsets, while wl_shm exposes only one base offset and one stride, so
    // advertising NV12 without VirtWL would produce a buffer whose UV plane
    // cannot be described safely. Keep the capability conditional until a
    // direct multi-plane GBM import path exists.
    (MANDATORY_SHM_FORMATS.contains(&format) || ctx.host_shm_formats.contains(&format))
        && (format != WL_SHM_FORMAT_NV12 || ctx.virtwayland_channel.is_some())
}

/// Record a format advertised by the internal host wl_shm object and enqueue
/// it for synthetic guest wl_shm objects that were already bound.
pub(crate) fn record_host_shm_format(ctx: &mut Context, format: u32) {
    if !supported_shm_format(format) || !ctx.host_shm_formats.insert(format) {
        return;
    }

    let guest_ids: Vec<u32> = ctx.shm_guest_formats.keys().copied().collect();
    for guest_id in guest_ids {
        if ctx.stale_shm_guest_objects.contains(&guest_id) {
            continue;
        }
        if !guest_shm_format_available(ctx, format) {
            continue;
        }
        let should_send = ctx
            .shm_guest_formats
            .get_mut(&guest_id)
            .is_some_and(|advertised| advertised.insert(format));
        if should_send {
            let mut builder = MessageBuilder::new();
            builder.write_u32(format);
            let msg = builder.build_message(guest_id, protocols::wayland::wl_shm::EVT_FORMAT);
            ctx.host_to_client_queue.push((msg, Vec::new()));
        }
    }
}

/// Record a format reported by the internal host dmabuf object. ChromiumOS
/// uses those events as the capability source when the virtwl channel exposes
/// dmabuf-backed SHM buffers.
pub(crate) fn record_host_shm_drm_format(ctx: &mut Context, format: u32) {
    if let Some(shm_format) = shm_format_from_drm(format) {
        record_host_shm_format(ctx, shm_format);
    }
}

/// Register a synthetic guest wl_shm object and send every capability known so
/// far. ARGB/XRGB are mandatory wl_shm formats; optional formats are sent only
/// after the host reports support.
pub(crate) fn register_guest_shm(ctx: &mut Context, guest_id: u32) {
    let formats: Vec<u32> = SUPPORTED_SHM_FORMATS
        .iter()
        .copied()
        .filter(|format| guest_shm_format_available(ctx, *format))
        .collect();
    let advertised: std::collections::HashSet<u32> = formats.iter().copied().collect();
    ctx.shm_guest_formats.insert(guest_id, advertised);

    for format in formats {
        let mut builder = MessageBuilder::new();
        builder.write_u32(format);
        let msg = builder.build_message(guest_id, protocols::wayland::wl_shm::EVT_FORMAT);
        ctx.host_to_client_queue.push((msg, Vec::new()));
    }
}

fn valid_shm_stride(format: u32, width: i32, stride: i32) -> bool {
    let Some(width) = usize::try_from(width).ok() else {
        return false;
    };
    let Some(stride) = usize::try_from(stride).ok() else {
        return false;
    };
    let Some(bytes_per_pixel) = format_bytes_per_pixel(format) else {
        return false;
    };
    let minimum = width
        .checked_mul(bytes_per_pixel)
        .is_some_and(|minimum| stride >= minimum);
    minimum
        && (format != WL_SHM_FORMAT_NV12 || (width.is_multiple_of(2) && stride.is_multiple_of(2)))
}

fn format_bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        WL_SHM_FORMAT_NV12 => Some(1),
        0x3631_4752 => Some(2), // WL_SHM_FORMAT_RGB565
        0 | 1 | 0x3432_4241 | 0x3432_4258 => Some(4),
        _ => None,
    }
}

/// Return the number of bytes occupied by the linear representation used by
/// the SHM bridge. This matches ChromiumOS' `sl_shm_format_size`: NV12 has a
/// full-height Y plane followed by a half-height UV plane, while all other
/// formats are single-plane.
pub(crate) fn required_buffer_size(
    width: i32,
    height: i32,
    stride: usize,
    format: u32,
) -> Option<usize> {
    if width <= 0 || height <= 0 || stride == 0 {
        return None;
    }
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    let bytes_per_pixel = format_bytes_per_pixel(format)?;
    if stride < width.checked_mul(bytes_per_pixel)? {
        return None;
    }
    if format == WL_SHM_FORMAT_NV12 {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) || !stride.is_multiple_of(2) {
            return None;
        }
        stride
            .checked_mul(height)?
            .checked_add(stride.checked_mul(height / 2)?)
    } else {
        stride.checked_mul(height)
    }
}

fn backing_fd_has_size(fd: RawFd, size: usize) -> bool {
    if fd < 0 {
        return false;
    }
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }

    let mode = stat.st_mode as libc::mode_t;
    if mode & libc::S_IFMT == libc::S_IFREG {
        stat.st_size >= i64::try_from(size).unwrap_or(i64::MAX)
    } else {
        // Non-regular shared-memory providers (for example a virtwl fd) do
        // not expose a meaningful st_size. mmap remains the final check.
        true
    }
}

fn valid_buffer_layout(
    pool_size: usize,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) -> Option<usize> {
    if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
        return None;
    }
    if format == WL_SHM_FORMAT_NV12 && (width % 2 != 0 || height % 2 != 0) {
        return None;
    }
    let offset = offset as usize;
    let stride = stride as usize;
    let height = height as usize;
    let bytes = if format == WL_SHM_FORMAT_NV12 {
        let chroma_rows = height.checked_div(2)?;
        stride
            .checked_mul(height)?
            .checked_add(stride.checked_mul(chroma_rows)?)?
    } else {
        stride.checked_mul(height)?
    };
    let end = offset.checked_add(bytes)?;
    if end > pool_size || bytes > i32::MAX as usize {
        return None;
    }
    Some(bytes)
}

fn virtwl_allocation_size(size: usize) -> Option<u32> {
    u32::try_from(size).ok()
}

#[derive(Clone, Copy)]
struct PlaneCopy {
    src_offset: usize,
    dst_offset: usize,
    src_stride: usize,
    dst_stride: usize,
    rows: usize,
    row_bytes: usize,
}

/// Bounds and layout metadata for copying one guest SHM buffer.
///
/// Keeping the raw pointers separate from this value makes the copy entry
/// point difficult to call with mismatched dimensions while avoiding a
/// long list of loosely-related scalar arguments.
#[derive(Clone, Copy)]
pub(crate) struct ShmCopyLayout {
    pub(crate) pool_size: usize,
    pub(crate) dest_size: usize,
    pub(crate) format: u32,
    pub(crate) offset: usize,
    pub(crate) width: usize,
    pub(crate) src_stride: usize,
    pub(crate) dst_stride: usize,
    pub(crate) height: usize,
}

fn valid_plane_copy(pool_size: usize, dest_size: usize, plane: PlaneCopy) -> Option<usize> {
    if plane.src_stride == 0
        || plane.dst_stride == 0
        || plane.rows == 0
        || plane.row_bytes == 0
        || plane.row_bytes > plane.src_stride
        || plane.row_bytes > plane.dst_stride
    {
        return None;
    }
    let last_row = plane.rows.checked_sub(1)?;
    let src_end = plane
        .src_offset
        .checked_add(last_row.checked_mul(plane.src_stride)?)?
        .checked_add(plane.row_bytes)?;
    let dst_end = plane
        .dst_offset
        .checked_add(last_row.checked_mul(plane.dst_stride)?)?
        .checked_add(plane.row_bytes)?;
    (src_end <= pool_size && dst_end <= dest_size).then_some(plane.row_bytes)
}

fn clipped_damage(
    rect: DamageRect,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let x0 = i64::from(rect.x).max(0);
    let y0 = i64::from(rect.y).max(0);
    let x1 = i64::from(rect.x)
        .checked_add(i64::from(rect.width))?
        .min(i64::try_from(width).ok()?);
    let y1 = i64::from(rect.y)
        .checked_add(i64::from(rect.height))?
        .min(i64::try_from(height).ok()?);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some((
        usize::try_from(x0).ok()?,
        usize::try_from(y0).ok()?,
        usize::try_from(x1).ok()?,
        usize::try_from(y1).ok()?,
    ))
}

/// Copy only the damaged rectangles of a SHM image into the host-side
/// linear buffer.
///
/// ChromiumOS treats NV12 as two planes sharing one FD: the UV plane starts
/// immediately after the full-resolution Y plane and has half as many rows.
/// Validate every plane and every rectangle before touching either destination
/// plane so malformed client metadata cannot leave a partially updated buffer.
pub(crate) fn copy_shm_damage(
    src_ptr: *const u8,
    dst_ptr: *mut u8,
    layout: ShmCopyLayout,
    damage: &[DamageRect],
) -> bool {
    let ShmCopyLayout {
        pool_size,
        dest_size,
        format,
        offset,
        width,
        src_stride,
        dst_stride,
        height,
    } = layout;
    if width == 0 || height == 0 || format_bytes_per_pixel(format).is_none() {
        return false;
    }

    let bytes_per_pixel = format_bytes_per_pixel(format).unwrap_or(0);
    let plane_count = if format == WL_SHM_FORMAT_NV12 { 2 } else { 1 };
    let mut spans = Vec::new();
    for rect in damage {
        let Some((mut x0, mut y0, mut x1, mut y1)) = clipped_damage(*rect, width, height) else {
            continue;
        };

        for plane in 0..plane_count {
            let (plane_bpp, plane_src_offset, plane_dst_offset) =
                if format == WL_SHM_FORMAT_NV12 && plane == 1 {
                    // Chroma samples cover two horizontal pixels and two
                    // vertical pixels. Expand a partial damage rectangle to
                    // complete samples before copying.
                    x0 &= !1;
                    x1 = x1.saturating_add(1).min(width) & !1;
                    if x1 <= x0 {
                        x1 = (x0 + 2).min(width);
                    }
                    y0 /= 2;
                    y1 = y1.saturating_add(1) / 2;
                    (
                        1usize,
                        match offset.checked_add(match height.checked_mul(src_stride) {
                            Some(value) => value,
                            None => return false,
                        }) {
                            Some(value) => value,
                            None => return false,
                        },
                        match height.checked_mul(dst_stride) {
                            Some(value) => value,
                            None => return false,
                        },
                    )
                } else {
                    (bytes_per_pixel, offset, 0usize)
                };
            let rows = y1.saturating_sub(y0);
            let Some(row_bytes) = x1
                .checked_sub(x0)
                .and_then(|width| width.checked_mul(plane_bpp))
            else {
                return false;
            };
            let Some(y_offset) = y0.checked_mul(src_stride) else {
                return false;
            };
            let Some(x_offset) = x0.checked_mul(plane_bpp) else {
                return false;
            };
            let Some(src_offset) = plane_src_offset
                .checked_add(y_offset)
                .and_then(|value| value.checked_add(x_offset))
            else {
                return false;
            };
            let Some(y_offset) = y0.checked_mul(dst_stride) else {
                return false;
            };
            let Some(dst_offset) = plane_dst_offset
                .checked_add(y_offset)
                .and_then(|value| value.checked_add(x_offset))
            else {
                return false;
            };
            let plane = PlaneCopy {
                src_offset,
                dst_offset,
                src_stride,
                dst_stride,
                rows,
                row_bytes,
            };
            if valid_plane_copy(pool_size, dest_size, plane).is_none() {
                return false;
            }
            spans.push(plane);
        }
    }

    // An empty damage list is a valid no-op. This is important for a
    // damage-only commit that carries no actual damage.
    if spans.is_empty() {
        return true;
    }

    // Safety: every plane's final source and destination byte was checked
    // against the corresponding mapping length above. The mappings are
    // disjoint buffers allocated for this wl_buffer, so non-overlapping copy
    // is valid.
    unsafe {
        for plane in spans {
            for row in 0..plane.rows {
                let Some(row_src_offset) = row.checked_mul(plane.src_stride) else {
                    return false;
                };
                let Some(row_dst_offset) = row.checked_mul(plane.dst_stride) else {
                    return false;
                };
                let Some(src_row) = plane.src_offset.checked_add(row_src_offset) else {
                    return false;
                };
                let Some(dst_row) = plane.dst_offset.checked_add(row_dst_offset) else {
                    return false;
                };
                ptr::copy_nonoverlapping(
                    src_ptr.add(src_row),
                    dst_ptr.add(dst_row),
                    plane.row_bytes,
                );
            }
        }
    }
    true
}

/// Copy a complete SHM image. This remains a separate helper because a newly
/// allocated host buffer has no valid pixels until its first commit.
#[cfg(test)]
pub(crate) fn copy_shm_planes(src_ptr: *const u8, dst_ptr: *mut u8, layout: ShmCopyLayout) -> bool {
    let rect = DamageRect::new(
        0,
        0,
        i32::try_from(layout.width).unwrap_or(i32::MAX),
        i32::try_from(layout.height).unwrap_or(i32::MAX),
    );
    copy_shm_damage(src_ptr, dst_ptr, layout, std::slice::from_ref(&rect))
}

fn release_temporary_host_pool(ctx: &mut Context, host_pool_id: u32) {
    // The pool is an internal host-only object. It is reserved while the
    // create_pool/create_buffer/destroy sequence is queued, but no guest
    // request can ever remove it if queue construction fails midway.
    ctx.shadow_table.remove_host_interface(host_pool_id);
}

fn queue_host_buffer_destroy(ctx: &mut Context, host_id: u32) {
    let builder = MessageBuilder::new();
    queue_message(
        &mut ctx.client_to_host_queue,
        host_id,
        0,
        builder,
        Vec::new(),
    );
}

fn clear_surface_buffer_references(ctx: &mut Context, guest_id: u32) {
    for surface in ctx.surfaces.values_mut() {
        if surface.current_buffer_id == Some(guest_id) {
            surface.current_buffer_id = None;
        }
        if surface.pending_buffer_id == Some(Some(guest_id)) {
            surface.pending_buffer_id = Some(None);
        }
    }
}

fn surface_references_buffer(ctx: &Context, guest_id: u32) -> bool {
    ctx.surfaces.values().any(|surface| {
        surface.current_buffer_id == Some(guest_id)
            || surface.pending_buffer_id == Some(Some(guest_id))
    })
}

/// Drop deferred SHM buffers once the host has released them and no surface
/// still refers to their guest ID.
pub(crate) fn collect_retired_buffers(ctx: &mut Context) {
    let referenced: std::collections::HashSet<u32> = ctx
        .surfaces
        .values()
        .flat_map(|surface| {
            surface
                .current_buffer_id
                .into_iter()
                .chain(surface.pending_buffer_id.into_iter().flatten())
        })
        .collect();
    let releasable: Vec<u32> = ctx
        .retired_buffers
        .iter()
        .filter_map(|(&guest_id, buffer)| {
            (buffer.host_released && !referenced.contains(&guest_id)).then_some(guest_id)
        })
        .collect();
    for guest_id in releasable {
        ctx.retired_buffers.remove(&guest_id);
        if !ctx.shadow_table.is_pending_destroy_guest(guest_id) {
            ctx.shadow_table.remove_id(guest_id);
        }
        ctx.submitted_buffers.remove(&guest_id);
    }
}

impl protocols::wayland::wl_shm::WlShmHandler for ShmHandler {
    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if ctx.host_shm_id == Some(ctx.last_sender_id) {
            record_host_shm_format(ctx, format);
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_create_pool(&mut self, ctx: &mut Context, id: u32, fd: RawFd, size: i32) -> Action {
        if ctx.stale_shm_guest_objects.contains(&ctx.last_sender_id) {
            debug!(
                "Ignoring wl_shm.create_pool from removed host global object {}",
                ctx.last_sender_id
            );
            return Action::Drop;
        }
        let pool_id = id;

        let Some(pool_size) = valid_pool_size(size) else {
            warn!("Rejecting invalid SHM pool size {}", size);
            queue_protocol_error(ctx, ctx.last_sender_id, 1, "invalid wl_shm pool size");
            return Action::Drop;
        };
        if !backing_fd_has_size(fd, pool_size) {
            warn!(
                "Rejecting SHM pool fd {} that is shorter than declared size {}",
                fd, pool_size
            );
            queue_protocol_error(ctx, ctx.last_sender_id, 2, "invalid wl_shm pool fd");
            return Action::Drop;
        }
        debug!("Creating pool, fd={}", fd);

        // Duplicate the FD for PoolState because proxy.rs closes the original 'fd'
        // after this handler returns.
        let pool_fd = unsafe { libc::dup(fd) };
        debug!("Dup result={}", pool_fd);

        if pool_fd < 0 {
            error!("Failed to dup FD for SHM pool");
            queue_protocol_error(ctx, ctx.last_sender_id, 2, "invalid wl_shm pool fd");
            return Action::Drop;
        }

        // Mmap the file
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                pool_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                pool_fd,
                0,
            )
        };
        debug!("Mmap result={:?}", ptr);

        if ptr == libc::MAP_FAILED {
            error!("Failed to mmap SHM pool");
            unsafe {
                libc::close(pool_fd);
            }
            queue_protocol_error(ctx, ctx.last_sender_id, 2, "invalid wl_shm pool fd");
            return Action::Drop;
        }

        let pool = Arc::new(PoolState {
            client_fd: pool_fd, // Takes ownership of new FD
            inner: RwLock::new(PoolInner {
                client_ptr: ptr,
                size: pool_size,
            }),
        });

        ctx.pools.insert(pool_id, pool);
        ctx.shadow_table
            .track_interface_with_version(pool_id, "wl_shm_pool".to_string(), 1);

        Action::Drop
    }
}

impl protocols::wayland::wl_shm_pool::WlShmPoolHandler for ShmHandler {
    fn on_create_buffer(
        &mut self,
        ctx: &mut Context,
        id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Action {
        debug!("Creating buffer id={}", id);
        let pool_id = ctx.last_sender_id;
        if ctx.stale_shm_pools.contains(&pool_id) {
            debug!(
                "Ignoring wl_shm_pool.create_buffer from removed host global object {}",
                pool_id
            );
            return Action::Drop;
        }
        let pool = match ctx.pools.get(&pool_id) {
            Some(p) => p.clone(),
            None => {
                warn!("Unknown pool ID: {}", pool_id);
                queue_protocol_error(ctx, pool_id, 2, "invalid wl_shm pool");
                return Action::Drop;
            }
        };
        if !supported_shm_format(format) || !guest_shm_format_available(ctx, format) {
            warn!("Rejecting unsupported SHM buffer format {:#010x}", format);
            queue_protocol_error(ctx, pool_id, 0, "invalid wl_shm buffer format");
            return Action::Drop;
        }
        if !valid_shm_stride(format, width, stride) {
            warn!(
                "Rejecting SHM buffer stride {} for format {:#010x} and width {}",
                stride, format, width
            );
            queue_protocol_error(ctx, pool_id, 1, "invalid wl_shm buffer stride");
            return Action::Drop;
        }

        let pool_size = match pool.inner.read() {
            Ok(inner) => inner.size,
            Err(_) => {
                warn!("Cannot inspect SHM pool {}", pool_id);
                return Action::Drop;
            }
        };
        let Some(buffer_size) =
            valid_buffer_layout(pool_size, offset, width, height, stride, format)
        else {
            warn!(
                "Rejecting invalid SHM buffer: pool={}, offset={}, width={}, height={}, stride={}, pool_size={}",
                pool_id, offset, width, height, stride, pool_size
            );
            queue_protocol_error(ctx, pool_id, 1, "invalid wl_shm buffer layout");
            return Action::Drop;
        };

        debug!("Allocator present={}", ctx.allocator.is_some());
        debug!(
            "VirtWayland channel present={}",
            ctx.virtwayland_channel.is_some()
        );

        // Allocate buffer (GBM or VirtWayland)
        let alloc_res = if let Some(channel) = &ctx.virtwayland_channel {
            debug!("Allocating VirtWayland buffer: size={}", buffer_size);
            let Some(allocation_size) = virtwl_allocation_size(buffer_size) else {
                error!(
                    "Rejecting SHM buffer size {}: virtwl allocation size exceeds u32",
                    buffer_size
                );
                return Action::Drop;
            };
            match channel.allocate(allocation_size) {
                Ok((fd, _alloc_size)) => {
                    // virtwl allocation is a simple SHM-like buffer.
                    // No modifier, offset 0.
                    Some((None, stride as u32, fd, 0, 0, buffer_size as u64))
                }
                Err(e) => {
                    error!("Failed to allocate VirtWayland buffer: {}", e);
                    return Action::Drop;
                }
            }
        } else {
            None
        };

        let (bo, bo_stride, dmabuf_fd_owned, _modifier, blob_offset, total_size) =
            if let Some(res) = alloc_res {
                res
            } else if let Some(allocator) = &mut ctx.allocator {
                // Fallback to GBM allocator
                match allocator.allocate(
                    width as u32,
                    height as u32,
                    Self::wl_shm_format_to_drm_format(format),
                ) {
                    Ok(bo) => {
                        let bo_stride = bo.stride().unwrap_or(0);
                        if bo_stride == 0 || bo_stride > i32::MAX as u32 {
                            error!("GBM returned an invalid stride {}", bo_stride);
                            return Action::Drop;
                        }
                        if !valid_shm_stride(format, width, bo_stride as i32) {
                            error!(
                                "GBM returned stride {} that cannot cover {}x{} format {:#010x}",
                                bo_stride, width, height, format
                            );
                            return Action::Drop;
                        }
                        debug!(
                            "Allocated GBM BO: width={}, height={}, stride={}, format={}",
                            width, height, bo_stride, format
                        );

                        let fd = match bo.fd() {
                            Ok(f) => f,
                            Err(e) => {
                                error!("Failed to get FD from BO: {}", e);
                                return Action::Drop;
                            }
                        };

                        // For GBM with LINEAR flag, modifier is likely 0 (LINEAR)
                        // We can try to get it from BO if needed, but for now we default to 0
                        // as that was the behavior and we want to be safe.
                        // If we want to be correct:
                        let modifier: u64 = match bo.modifier() {
                            Ok(m) => m.into(),
                            Err(_) => 0,
                        };

                        debug!("GBM BO modifier: {}", modifier);

                        // FIX: Add offset and calculate size to match the 6-element tuple
                        let offset = 0;
                        let Some(total_size) =
                            required_buffer_size(width, height, bo_stride as usize, format)
                                .and_then(|size| u64::try_from(size).ok())
                        else {
                            error!("GBM buffer size/layout is invalid");
                            return Action::Drop;
                        };

                        (Some(bo), bo_stride, fd, modifier, offset, total_size)
                    }
                    Err(e) => {
                        error!("Failed to allocate GBM BO: {}", e);
                        return Action::Drop;
                    }
                }
            } else {
                error!("No allocator (VirtGpu or GBM) available");
                return Action::Drop;
            };

        // Create WL_SHM buffer on host
        if let Some(host_wl_shm_id) = ctx.host_shm_id {
            // Map the buffer for SHM synchronization
            if total_size == 0 || total_size > i32::MAX as u64 {
                error!("SHM destination size is invalid: {}", total_size);
                return Action::Drop;
            }
            let dest_size = total_size as usize;
            // VirtWL allocations are ordinary shared-memory fds and can be
            // mapped directly. GBM PRIME fds must not be treated as generic
            // mmap-able memory: tiled/modifier-backed BOs require the GBM
            // map/unmap path to perform the required cache synchronization.
            let dest_ptr = if bo.is_none() {
                let ptr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        dest_size,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        dmabuf_fd_owned.as_raw_fd(),
                        blob_offset as i64,
                    ) as *mut u8
                };
                if ptr as *mut libc::c_void == libc::MAP_FAILED {
                    error!(
                        "Failed to mmap VirtWL SHM buffer: {:?}",
                        std::io::Error::last_os_error()
                    );
                    return Action::Drop;
                }
                ptr
            } else {
                std::ptr::null_mut()
            };

            let host_pool_id = ctx.shadow_table.allocate_host_id();
            // The temporary host wl_shm_pool remains live until the queued
            // destroy request is processed by the compositor. Reserve its ID
            // even though it has no guest-side object or event callbacks.
            ctx.shadow_table.track_host_interface_with_version(
                host_pool_id,
                "wl_shm_pool".to_string(),
                1,
            );

            // wl_shm.create_pool(new_id, fd, size)
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_pool_id);
            builder.write_i32(total_size as i32);

            let fd_to_send = match dmabuf_fd_owned.try_clone() {
                Ok(f) => f.into_raw_fd(),
                Err(e) => {
                    error!("Failed to dup FD: {}", e);
                    if !dest_ptr.is_null() {
                        unsafe {
                            libc::munmap(dest_ptr as *mut libc::c_void, dest_size);
                        }
                    }
                    release_temporary_host_pool(ctx, host_pool_id);
                    return Action::Drop;
                }
            };
            debug!("Dup for send={} size={}", fd_to_send, total_size);

            if !queue_message(
                &mut ctx.client_to_host_queue,
                host_wl_shm_id,
                0,
                builder,
                vec![fd_to_send],
            ) {
                if !dest_ptr.is_null() {
                    unsafe {
                        libc::munmap(dest_ptr as *mut libc::c_void, dest_size);
                    }
                }
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            }

            // wl_shm_pool.create_buffer(new_id, offset, width, height, stride, format)
            let host_buffer_id = ctx.shadow_table.allocate_host_id();
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_buffer_id);
            builder.write_i32(blob_offset); // offset
            builder.write_i32(width);
            builder.write_i32(height);
            builder.write_i32(bo_stride as i32); // stride
            builder.write_u32(format); // format (SHM format, not DRM format)

            queue_message(
                &mut ctx.client_to_host_queue,
                host_pool_id,
                0,
                builder,
                Vec::new(),
            );

            // wl_shm_pool.destroy()
            let builder = MessageBuilder::new();
            queue_message(
                &mut ctx.client_to_host_queue,
                host_pool_id,
                1,
                builder,
                Vec::new(),
            );

            // The three requests are emitted in one ordered stream:
            // create_pool, create_buffer, destroy. Once queued, a later
            // allocation may safely reuse this temporary ID because the host
            // will process the destroy before the later request. Keeping the
            // registration forever would leak one host ID per SHM buffer.
            ctx.shadow_table.mark_pending_destroy_host(host_pool_id);

            // Store mapping
            ctx.shadow_table.map_id(id, host_buffer_id);
            ctx.shadow_table
                .track_interface_with_version(id, "wl_buffer".to_string(), 1);
            ctx.shadow_table.set_host_version(host_buffer_id, 1);

            // Save buffer state
            ctx.buffers.insert(
                id,
                BufferState {
                    guest_buffer_id: id,
                    pool: pool.clone(),
                    offset,
                    width,
                    height,
                    stride: stride as u32,
                    format,
                    host_buffer_id,
                    bo,
                    dmabuf_fd: Some(dmabuf_fd_owned),
                    bo_stride, // Store bo_stride
                    dest_ptr,
                    dest_size,
                    needs_full_copy: true,
                    host_released: false,
                },
            );
        } else {
            error!("wl_shm not available on host");
            return Action::Drop;
        }

        Action::Drop
    }

    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let pool_id = ctx.last_sender_id;
        ctx.pools.remove(&pool_id);
        ctx.stale_shm_pools.remove(&pool_id);
        // The synthetic guest object's delete_id is queued by the generated
        // local-only destructor path after this handler returns.
        Action::Drop
    }

    fn on_resize(&mut self, ctx: &mut Context, size: i32) -> Action {
        let pool_id = ctx.last_sender_id;
        if ctx.stale_shm_pools.contains(&pool_id) {
            debug!(
                "Ignoring wl_shm_pool.resize from removed host global object {}",
                pool_id
            );
            return Action::Drop;
        }
        debug!("Resizing pool id={}", pool_id);
        if let Some(pool) = ctx.pools.get(&pool_id).cloned() {
            if let Ok(mut inner) = pool.inner.write() {
                let Some(new_size) = valid_pool_resize(inner.size, size) else {
                    // wl_shm_pool.resize is an expansion-only operation. A
                    // client that needs a smaller pool must create a new pool;
                    // shrinking an existing mapping could invalidate live
                    // wl_buffers that still reference the old tail.
                    warn!(
                        "Rejecting non-growing SHM pool resize from {} to {}",
                        inner.size, size
                    );
                    queue_protocol_error(ctx, pool_id, 1, "invalid wl_shm pool resize");
                    return Action::Drop;
                };
                if !backing_fd_has_size(pool.client_fd, new_size) {
                    warn!(
                        "Rejecting SHM pool resize to {}: backing fd {} is too short",
                        new_size, pool.client_fd
                    );
                    queue_protocol_error(ctx, pool_id, 2, "invalid wl_shm pool fd");
                    return Action::Drop;
                }
                debug!(
                    "Resizing pool id={} from {} to {}",
                    pool_id, inner.size, size
                );
                // mremap with MREMAP_MAYMOVE
                let new_ptr = unsafe {
                    libc::mremap(inner.client_ptr, inner.size, new_size, libc::MREMAP_MAYMOVE)
                };

                if new_ptr == libc::MAP_FAILED {
                    error!(
                        "Failed to mremap pool: {:?}",
                        std::io::Error::last_os_error()
                    );
                    // A failed resize must be fatal.  Otherwise the client
                    // continues with the old mapping while believing that
                    // the pool has grown, and a later buffer request can
                    // address memory outside the mapped range.
                    queue_protocol_error(ctx, pool_id, 2, "invalid wl_shm pool fd");
                } else {
                    inner.client_ptr = new_ptr;
                    inner.size = new_size;
                    debug!("Pool resized successfully to {:?}", new_ptr);
                }
            } else {
                error!("Failed to acquire write lock on pool");
            }
        } else {
            warn!("Pool not found for resize: {}", pool_id);
        }
        Action::Drop
    }
}

impl protocols::wayland::wl_buffer::WlBufferHandler for ShmHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        let host_id = ctx.shadow_table.get_host_id(guest_id);
        // wl_surface.attach is forwarded immediately, while submitted_buffers
        // is marked only when the later wl_surface.commit is handled. A
        // client is allowed to destroy the wl_buffer in that interval; the
        // host surface still owns the attached host buffer and needs its local
        // SHM backing through the pending commit.
        let still_referenced =
            ctx.submitted_buffers.contains(&guest_id) || surface_references_buffer(ctx, guest_id);

        // A guest may destroy a wl_buffer while a surface still has it
        // attached. Keep the backing mmap and the guest↔host mapping alive
        // until the host compositor sends wl_buffer.release; otherwise a
        // damage-only commit can dereference unmapped memory and the host
        // release event can be routed to a newly reused object ID.
        if let Some(mut buffer) = ctx.buffers.remove(&guest_id) {
            if still_referenced && !buffer.host_released {
                // Keep the host wl_buffer alive until its release event. A
                // destroy request would remove the host resource before it
                // can report release, leaving no compositor-use lifetime
                // signal for the deferred mmap. The guest object and host
                // object therefore have deliberately different destruction
                // points.
                buffer.guest_buffer_id = guest_id;
                ctx.retired_buffers.insert(guest_id, buffer);
                ctx.shadow_table.retire_guest_object(guest_id);
            } else {
                if let Some(host_id) = host_id {
                    queue_host_buffer_destroy(ctx, host_id);
                }
                clear_surface_buffer_references(ctx, guest_id);
                ctx.shadow_table.mark_pending_destroy(guest_id);
                ctx.submitted_buffers.remove(&guest_id);
            }
        } else {
            // Non-SHM wl_buffers have no local backing storage, so their
            // host object can be destroyed and their mapping removed now.
            if let Some(host_id) = host_id {
                queue_host_buffer_destroy(ctx, host_id);
            }
            clear_surface_buffer_references(ctx, guest_id);
            ctx.shadow_table.mark_pending_destroy(guest_id);
            ctx.submitted_buffers.remove(&guest_id);
        }
        Action::Drop
    }

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };

        if ctx.retired_buffers.contains_key(&guest_id) {
            // The guest object is already gone, so there is no valid object on
            // which to deliver the release event. It is nevertheless the
            // release that makes it safe to drop the deferred backing storage.
            if let Some(buffer) = ctx.retired_buffers.get_mut(&guest_id) {
                buffer.host_released = true;
            }
            // The host object remained alive specifically so this release
            // could arrive. It is now safe to destroy the host proxy and drop
            // the local backing.
            queue_host_buffer_destroy(ctx, host_id);
            ctx.shadow_table.mark_pending_destroy(guest_id);
            ctx.submitted_buffers.remove(&guest_id);
            clear_surface_buffer_references(ctx, guest_id);
            // The backing state can be dropped now that the compositor sent
            // release, but the host object still owes wl_display.delete_id
            // for the queued destroy request. Keep its numeric mapping until
            // that acknowledgement instead of letting collect_retired_buffers
            // remove it immediately.
            ctx.retired_buffers.remove(&guest_id);
            return Action::Drop;
        }

        if let Some(buffer) = ctx.buffers.get_mut(&guest_id) {
            buffer.host_released = true;
        }
        // A release is the compositor's lifetime signal. Once it arrives, the
        // backing storage may be reused or destroyed, even if the surface
        // still has the buffer as its current content. Keeping this marker in
        // `submitted_buffers` would make a later guest destroy incorrectly
        // retire an already-idle buffer.
        ctx.submitted_buffers.remove(&guest_id);
        collect_retired_buffers(ctx);
        Action::Forward
    }
}

impl ShmHandler {
    fn wl_shm_format_to_drm_format(format: u32) -> u32 {
        match format {
            0 => 0x34325241, // ARGB8888
            1 => 0x34325258, // XRGB8888
            _ => format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ShmHandler;
    use super::{
        backing_fd_has_size, collect_retired_buffers, copy_shm_planes, guest_shm_format_available,
        record_host_shm_drm_format, record_host_shm_format, register_guest_shm,
        release_temporary_host_pool, valid_buffer_layout, valid_pool_resize, valid_pool_size,
        valid_shm_stride, virtwl_allocation_size, WL_SHM_FORMAT_NV12,
    };
    use crate::handler::registry::RegistryHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_shm::WlShmHandler;
    use crate::state::{BufferState, Context, PoolInner, PoolState};
    use crate::wire::Action;
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::sync::{Arc, RwLock};

    #[test]
    fn rejects_non_positive_pool_sizes() {
        assert_eq!(valid_pool_size(0), None);
        assert_eq!(valid_pool_size(-1), None);
        assert_eq!(valid_pool_size(4096), Some(4096));
    }

    #[test]
    fn invalid_shm_requests_queue_protocol_errors() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = ShmHandler;

        ctx.last_sender_id = 10;
        assert_eq!(handler.on_create_pool(&mut ctx, 20, -1, 0), Action::Drop);
        assert!(ctx.fatal_protocol_error);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            1,
            "invalid pool size must use wl_shm.invalid_stride"
        );

        ctx.host_to_client_queue.clear();
        ctx.fatal_protocol_error = false;
        let fd = {
            use nix::sys::memfd::{memfd_create, MFdFlags};
            use std::ffi::CString;
            let fd = memfd_create(
                CString::new("sommelier-invalid-shm-buffer")
                    .unwrap()
                    .as_c_str(),
                MFdFlags::empty(),
            )
            .expect("memfd_create");
            nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");
            fd
        };
        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_create_pool(&mut ctx, 20, fd.as_raw_fd(), 4096),
            Action::Drop
        );
        ctx.host_to_client_queue.clear();
        ctx.fatal_protocol_error = false;
        ctx.last_sender_id = 20;
        assert_eq!(
            <ShmHandler as crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler>::on_create_buffer(
                &mut handler,
                &mut ctx,
                30,
                0,
                1,
                1,
                4,
                0xffff_ffff,
            ),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            0,
            "unsupported format must use wl_shm.invalid_format"
        );
    }

    #[test]
    fn failed_pool_resize_queues_invalid_fd_error() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("sommelier-failed-resize").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        nix::unistd::ftruncate(&fd, 8192).expect("ftruncate");
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let pool_id = 20;
        ctx.pools.insert(
            pool_id,
            Arc::new(PoolState {
                client_fd: fd.into_raw_fd(),
                inner: RwLock::new(PoolInner {
                    // MAP_FAILED makes mremap fail deterministically without
                    // touching a live mapping.
                    client_ptr: libc::MAP_FAILED,
                    size: 4096,
                }),
            }),
        );
        ctx.last_sender_id = pool_id;
        let mut handler = ShmHandler;
        assert_eq!(
            <ShmHandler as crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler>::on_resize(
                &mut handler,
                &mut ctx,
                8192,
            ),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            2,
            "mremap failure must use wl_shm.invalid_fd"
        );
    }

    #[test]
    fn shm_pool_resize_only_allows_growth() {
        assert_eq!(valid_pool_resize(4096, 8192), Some(8192));
        assert_eq!(valid_pool_resize(4096, 4096), None);
        assert_eq!(valid_pool_resize(4096, 2048), None);
        assert_eq!(valid_pool_resize(4096, 0), None);
        assert_eq!(valid_pool_resize(4096, -1), None);
    }

    #[test]
    fn validates_buffer_layout_against_pool_bounds() {
        assert_eq!(valid_buffer_layout(4096, 0, 32, 32, 128, 0), Some(4096));
        assert_eq!(valid_buffer_layout(4096, -1, 32, 32, 128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 0, 0, 32, 128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 0, 32, 32, -128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 1, 32, 32, 128, 0), None);
    }

    #[test]
    fn rejects_buffer_size_overflow() {
        assert_eq!(
            valid_buffer_layout(usize::MAX, 0, i32::MAX, i32::MAX, i32::MAX, 0),
            None
        );
    }

    #[test]
    fn virtwl_allocation_size_rejects_u32_truncation() {
        assert_eq!(virtwl_allocation_size(u32::MAX as usize), Some(u32::MAX));
        assert_eq!(virtwl_allocation_size(u32::MAX as usize + 1), None);
    }

    #[test]
    fn validates_shm_format_stride() {
        assert!(valid_shm_stride(0, 10, 40));
        assert!(valid_shm_stride(0x3631_4752, 10, 20));
        assert!(valid_shm_stride(WL_SHM_FORMAT_NV12, 10, 10));
        assert!(!valid_shm_stride(WL_SHM_FORMAT_NV12, 11, 11));
        assert!(!valid_shm_stride(0, 10, 39));
        assert!(!valid_shm_stride(0xdead_beef, 10, 40));
    }

    #[test]
    fn gbm_stride_must_cover_the_guest_row() {
        // GBM may choose a backend-specific stride. It is still invalid to
        // expose a host buffer whose row is shorter than the guest format's
        // minimum row width, because the copy path would silently truncate
        // every row.
        assert!(!valid_shm_stride(0, 10, 39));
        assert!(valid_shm_stride(0, 10, 40));
        assert!(!valid_shm_stride(0x3631_4752, 10, 19));
        assert!(valid_shm_stride(0x3631_4752, 10, 20));
    }

    #[test]
    fn validates_nv12_two_plane_pool_layout() {
        assert_eq!(
            valid_buffer_layout(24, 0, 4, 4, 4, WL_SHM_FORMAT_NV12),
            Some(24)
        );
        assert_eq!(
            valid_buffer_layout(23, 0, 4, 4, 4, WL_SHM_FORMAT_NV12),
            None
        );
        assert_eq!(
            valid_buffer_layout(24, 0, 4, 3, 4, WL_SHM_FORMAT_NV12),
            None,
        );
    }

    #[test]
    fn copies_both_nv12_planes_only_after_validating_all_spans() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let mut destination = [0u8; 24];

        assert!(copy_shm_planes(
            source.as_ptr(),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                pool_size: source.len(),
                dest_size: destination.len(),
                format: WL_SHM_FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                height: 4,
            },
        ));
        assert_eq!(&destination[..16], &[0x11; 16]);
        assert_eq!(&destination[16..], &[0x22; 8]);

        destination.fill(0);
        assert!(!copy_shm_planes(
            source.as_ptr(),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                pool_size: 23,
                dest_size: destination.len(),
                format: WL_SHM_FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                height: 4,
            },
        ));
        assert!(
            destination.iter().all(|byte| *byte == 0),
            "an invalid UV span must not leave the Y plane partially copied"
        );
    }

    #[test]
    fn synthetic_shm_advertises_mandatory_formats_until_host_capabilities_arrive() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        register_guest_shm(&mut ctx, 20);

        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert!(guest_shm_format_available(&ctx, 0));
        assert!(guest_shm_format_available(&ctx, 1));
        assert!(!guest_shm_format_available(&ctx, 0x3631_4752));
    }

    #[test]
    fn host_optional_shm_format_is_sent_once_to_existing_guest_bindings() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        register_guest_shm(&mut ctx, 20);
        ctx.host_to_client_queue.clear();

        record_host_shm_format(&mut ctx, 0x3631_4752);
        record_host_shm_format(&mut ctx, 0x3631_4752);

        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert!(guest_shm_format_available(&ctx, 0x3631_4752));
        let format = u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap());
        assert_eq!(format, 0x3631_4752);
    }

    #[test]
    fn drm_fourcc_capability_maps_to_wayland_shm_format() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        record_host_shm_drm_format(&mut ctx, 0x3432_5241); // DRM ARGB8888
        record_host_shm_drm_format(&mut ctx, 0x3432_5258); // DRM XRGB8888

        assert!(ctx.host_shm_formats.contains(&0));
        assert!(ctx.host_shm_formats.contains(&1));
        assert!(!ctx.host_shm_formats.contains(&0xdead_beef));
    }

    #[test]
    fn rejects_regular_pool_backing_that_is_too_short() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("sommelier-short-shm").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        assert!(backing_fd_has_size(fd.as_raw_fd(), 0));
        assert!(!backing_fd_has_size(fd.as_raw_fd(), 4096));
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");
        assert!(backing_fd_has_size(fd.as_raw_fd(), 4096));
    }

    #[test]
    fn temporary_host_pool_cleanup_releases_reserved_id() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_pool_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table
            .track_host_interface(host_pool_id, "wl_shm_pool".to_string());

        release_temporary_host_pool(&mut ctx, host_pool_id);

        assert_eq!(
            ctx.shadow_table.get_host_interface(host_pool_id),
            None,
            "a failed temporary-pool setup must not leak its host ID reservation"
        );
    }

    #[test]
    fn removed_host_shm_rejects_pool_creation_from_existing_synthetic_object() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        ctx.shadow_table
            .track_interface_with_version(guest_shm, "wl_shm".to_string(), 1);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut registry_handler = RegistryHandler;
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &"wl_shm".to_string(), 1),
            Action::Drop
        );
        register_guest_shm(&mut ctx, guest_shm);
        // The host binding has now been retired by the real
        // wl_registry.global_remove path. The synthetic guest object itself
        // remains addressable for teardown, but its requests must no longer
        // allocate local SHM state.
        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );

        let fd = memfd_create(
            CString::new("sommelier-removed-shm").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        assert_eq!(
            handler.on_create_pool(&mut ctx, 30, fd.as_raw_fd(), 4096),
            Action::Drop
        );
        assert!(
            ctx.pools.is_empty(),
            "a removed host wl_shm binding must not retain a new local pool"
        );
        assert_eq!(
            ctx.shadow_table.get_interface(30),
            None,
            "a rejected pool must not register a synthetic child object"
        );
    }

    #[test]
    fn replacement_shm_capability_does_not_reach_stale_guest_object() {
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut registry_handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let old_guest_shm = 20;
        register_guest_shm(&mut ctx, old_guest_shm);
        ctx.host_to_client_queue.clear();

        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(
            ctx.stale_shm_guest_objects.contains(&old_guest_shm),
            "removed host SHM generation must mark existing guest objects stale"
        );
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let new_guest_shm = 21;
        register_guest_shm(&mut ctx, new_guest_shm);
        ctx.host_to_client_queue.clear();

        record_host_shm_format(&mut ctx, 0x3631_4752);

        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "replacement capability must be sent only to the replacement guest object"
        );
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()),
            new_guest_shm
        );
    }

    #[test]
    fn removed_host_shm_ignores_requests_from_existing_pool_until_destroy() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut registry_handler = RegistryHandler;
        let shm = "wl_shm".to_string();
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let guest_shm = 20;
        register_guest_shm(&mut ctx, guest_shm);

        let fd = memfd_create(
            CString::new("sommelier-stale-shm-pool").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        assert_eq!(
            WlShmHandler::on_create_pool(&mut handler, &mut ctx, 30, fd.as_raw_fd(), 4096),
            Action::Drop
        );
        assert!(ctx.pools.contains_key(&30));

        ctx.last_sender_id = 1;
        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(ctx.stale_shm_pools.contains(&30));

        ctx.last_sender_id = 30;
        assert_eq!(
            crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler::on_resize(
                &mut handler,
                &mut ctx,
                8192
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.pools
                .get(&30)
                .and_then(|pool| pool.inner.read().ok())
                .map(|inner| inner.size),
            Some(4096),
            "a stale pool must not resize its local mapping"
        );
        assert_eq!(
            crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler::on_create_buffer(
                &mut handler,
                &mut ctx,
                31,
                0,
                1,
                1,
                4,
                0
            ),
            Action::Drop
        );
        assert!(
            ctx.shadow_table.get_host_id(31).is_none(),
            "a stale pool must not allocate a host buffer through replacement SHM"
        );

        assert_eq!(
            crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler::on_destroy(
                &mut handler,
                &mut ctx
            ),
            Action::Drop
        );
        assert!(!ctx.pools.contains_key(&30));
        assert!(!ctx.stale_shm_pools.contains(&30));
    }

    #[test]
    fn destroyed_attached_shm_buffer_stays_mapped_until_host_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        let surface = 40;
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.surfaces.insert(
            surface,
            crate::state::SurfaceState {
                current_buffer_id: Some(guest_buffer),
                ..Default::default()
            },
        );
        ctx.submitted_buffers.insert(guest_buffer);
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: std::ptr::null_mut(),
                size: 0,
            }),
        });
        ctx.buffers.insert(
            guest_buffer,
            BufferState {
                guest_buffer_id: guest_buffer,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                host_buffer_id: host_buffer,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
                host_released: false,
            },
        );

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(
            ctx.retired_buffers.contains_key(&guest_buffer),
            "destroy must retain backing state while the surface still references it"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer),
            "host mapping must remain reserved until release"
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            0,
            "the host buffer must stay alive until its release event"
        );

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(!ctx.retired_buffers.contains_key(&guest_buffer));
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer)
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert_eq!(
            ctx.surfaces
                .get(&surface)
                .and_then(|state| state.current_buffer_id),
            None
        );
        assert_eq!(
            ctx.client_to_host_queue.len(),
            1,
            "release must enqueue the deferred host destroy request exactly once"
        );
    }

    #[test]
    fn release_marks_live_buffer_idle_and_allows_immediate_destroy() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: std::ptr::null_mut(),
                size: 0,
            }),
        });
        ctx.buffers.insert(
            guest_buffer,
            BufferState {
                guest_buffer_id: guest_buffer,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                host_buffer_id: host_buffer,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
                host_released: false,
            },
        );
        ctx.submitted_buffers.insert(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(!ctx.submitted_buffers.contains(&guest_buffer));
        assert!(ctx
            .buffers
            .get(&guest_buffer)
            .is_some_and(|buffer| buffer.host_released));

        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(ctx.retired_buffers.is_empty());
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer)
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
    }

    #[test]
    fn destroying_non_shm_buffer_clears_submitted_marker() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.submitted_buffers.insert(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(
            !ctx.submitted_buffers.contains(&guest_buffer),
            "destroying a non-SHM buffer must not leave stale submitted state"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer)
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
    }

    #[test]
    fn retired_collection_waits_for_host_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: std::ptr::null_mut(),
                size: 0,
            }),
        });
        ctx.retired_buffers.insert(
            guest_buffer,
            BufferState {
                guest_buffer_id: guest_buffer,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                host_buffer_id: host_buffer,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
                host_released: false,
            },
        );
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);

        collect_retired_buffers(&mut ctx);
        assert!(ctx.retired_buffers.contains_key(&guest_buffer));

        ctx.retired_buffers
            .get_mut(&guest_buffer)
            .expect("retired buffer")
            .host_released = true;
        collect_retired_buffers(&mut ctx);
        assert!(!ctx.retired_buffers.contains_key(&guest_buffer));
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
    }
}
