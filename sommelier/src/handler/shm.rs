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

use crate::allocator::Allocator;
use crate::handler::display::queue_protocol_error;
use crate::protocols;
use crate::state::{BufferState, Context, DamageRect, PoolInner, PoolState};
use crate::wire::{Action, MessageBuilder};
use log::{debug, error, warn};
use std::collections::HashSet;
use std::io;
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
    // NV12 is advertised only when the selected output path can describe both
    // planes. VirtWL uses one contiguous allocation; the GBM direct path
    // validates plane metadata and rejects separate dma-buf objects before
    // exposing a buffer to the host compositor.
    (MANDATORY_SHM_FORMATS.contains(&format) || ctx.host_shm_formats.contains(&format))
        && (format != WL_SHM_FORMAT_NV12
            || ctx.virtwayland_channel.is_some()
            || (ctx.gpu_accel && ctx.host_dmabuf_id.is_some() && ctx.allocator.is_some()))
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

pub(crate) fn record_host_shm_wl_format(ctx: &mut Context, format: u32) {
    if supported_shm_format(format) {
        ctx.host_wl_shm_formats.insert(format);
    }
    record_host_shm_format(ctx, format);
}

/// Record a format reported by the internal host dmabuf object. ChromiumOS
/// uses those events as the capability source when the virtwl channel exposes
/// dmabuf-backed SHM buffers.
pub(crate) fn record_host_shm_drm_format(ctx: &mut Context, format: u32) {
    if let Some(shm_format) = shm_format_from_drm(format) {
        ctx.host_dmabuf_shm_formats.insert(shm_format);
        record_host_shm_format(ctx, shm_format);
    }
}

pub(crate) fn clear_host_shm_wl_formats(ctx: &mut Context) {
    let removed = std::mem::take(&mut ctx.host_wl_shm_formats);
    for format in removed {
        if !ctx.host_dmabuf_shm_formats.contains(&format) {
            ctx.host_shm_formats.remove(&format);
        }
    }
}

pub(crate) fn clear_host_shm_dmabuf_formats(ctx: &mut Context) {
    let removed = std::mem::take(&mut ctx.host_dmabuf_shm_formats);
    for format in removed {
        if !ctx.host_wl_shm_formats.contains(&format) {
            ctx.host_shm_formats.remove(&format);
        }
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

fn same_dma_buf_object(first_fd: RawFd, second_fd: RawFd) -> bool {
    if first_fd < 0 || second_fd < 0 {
        return false;
    }
    let mut first = unsafe { std::mem::zeroed::<libc::stat>() };
    let mut second = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(first_fd, &mut first) } != 0
        || unsafe { libc::fstat(second_fd, &mut second) } != 0
    {
        return false;
    }
    first.st_dev == second.st_dev && first.st_ino == second.st_ino
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DmabufLayout {
    stride0: usize,
    stride1: usize,
    offset0: usize,
    offset1: usize,
    span: usize,
}

struct HostBufferAllocation {
    bo: Option<gbm::BufferObject<()>>,
    fd: std::os::fd::OwnedFd,
    /// An additional descriptor for plane 1 when the allocator returns
    /// separate dma-buf objects (GBM may do this for multi-planar formats).
    /// VirtWL's NV12 allocation is a single object, so this remains `None`
    /// and the plane-0 descriptor is duplicated for both protocol planes.
    plane1_fd: Option<std::os::fd::OwnedFd>,
    stride0: u32,
    modifier: u64,
    offset0: u32,
    total_size: u64,
    plane1_offset: usize,
    plane1_stride: usize,
    direct_dmabuf: bool,
    dmabuf_sync: bool,
}

/// Validate the metadata returned by a host dma-buf allocator and calculate
/// the byte span that must be mapped in the guest.
///
/// The host may pad rows and place plane 1 after an implementation-defined
/// gap.  Deriving the span from the returned offsets/strides keeps all mmap
/// and copy bounds checks consistent with the metadata sent to the compositor.
fn validate_dmabuf_layout(
    format: u32,
    width: i32,
    height: i32,
    strides: [u32; 3],
    offsets: [u32; 3],
) -> Option<DmabufLayout> {
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    if width == 0 || height == 0 || strides[0] == 0 {
        return None;
    }

    let (plane0_bpp, plane_count) = if format == WL_SHM_FORMAT_NV12 {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return None;
        }
        (1usize, 2usize)
    } else if format == 0x3631_4752 {
        (2usize, 1usize)
    } else if matches!(
        format,
        0x3432_5258 | 0x3432_5241 | 0x3432_4258 | 0x3432_4241
    ) {
        (4usize, 1usize)
    } else {
        return None;
    };
    let stride0 = strides[0] as usize;
    let offset0 = offsets[0] as usize;
    let minimum0 = width.checked_mul(plane0_bpp)?;
    if stride0 < minimum0 {
        return None;
    }
    let plane0_end = offset0.checked_add(stride0.checked_mul(height)?)?;

    if plane_count == 1 {
        return Some(DmabufLayout {
            stride0,
            stride1: 0,
            offset0,
            offset1: 0,
            span: plane0_end,
        });
    }

    let stride1 = strides[1] as usize;
    let offset1 = offsets[1] as usize;
    let minimum1 = width;
    // VirtWL and the GBM path use one contiguous dma-buf for NV12. Plane 1
    // must begin after the complete Y plane; merely checking against
    // `offset0` would allow overlapping planes and make the copy layout
    // ambiguous.
    if stride1 < minimum1 || !stride1.is_multiple_of(2) || offset1 < plane0_end {
        return None;
    }
    let plane1_end = offset1.checked_add(stride1.checked_mul(height / 2)?)?;
    Some(DmabufLayout {
        stride0,
        stride1,
        offset0,
        offset1,
        span: plane0_end.max(plane1_end),
    })
}

fn map_dmabuf(fd: RawFd, layout: DmabufLayout) -> Option<(*mut u8, usize)> {
    if fd < 0 || layout.span <= layout.offset0 {
        return None;
    }
    // `mmap` takes a signed length and an `off_t` offset. Avoid lossy casts
    // even on a wider host where a malicious Wayland metadata value can fit
    // in usize but not in either syscall argument.
    if layout.span > isize::MAX as usize
        || layout.span - layout.offset0 > isize::MAX as usize
        || libc::off_t::try_from(layout.offset0).is_err()
    {
        return None;
    }
    // mmap(2) requires a page-aligned file offset. VirtWL/minigbm normally
    // returns offset 0 for linear allocations; reject an incompatible layout
    // rather than mapping the wrong bytes.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = usize::try_from(page_size).ok()?;
    if page_size == 0 || !layout.offset0.is_multiple_of(page_size) {
        return None;
    }
    // A successful mmap does not prove that a regular file is large enough:
    // touching a page beyond EOF raises SIGBUS. DMA-BUF providers generally
    // expose anon-inode descriptors without a useful st_size, so only enforce
    // the check for regular files (including memfd objects).
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return None;
    }
    let file_type = (stat.st_mode as libc::mode_t) & libc::S_IFMT;
    if stat.st_size < 0 {
        return None;
    }
    // DMA-BUF anon-inode implementations differ in their mode bits, but
    // several expose a positive st_size that is authoritative. Check every
    // descriptor with a usable size; only a zero-sized non-regular provider
    // has to rely on mmap as the final validation.
    if file_type == libc::S_IFREG || stat.st_size > 0 {
        let file_size = u64::try_from(stat.st_size).ok()?;
        let required_size = u64::try_from(layout.span).ok()?;
        if file_size < required_size {
            return None;
        }
    }
    let map_size = layout.span - layout.offset0;
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            map_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            libc::off_t::try_from(layout.offset0).ok()?,
        )
    };
    if ptr == libc::MAP_FAILED {
        None
    } else {
        Some((ptr as *mut u8, map_size))
    }
}

fn allocate_virtwl_dmabuf(
    ctx: &Context,
    width: i32,
    height: i32,
    format: u32,
) -> io::Result<HostBufferAllocation> {
    let Some(channel) = ctx
        .virtwayland_channel
        .as_ref()
        .filter(|channel| channel.supports_dmabuf())
    else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VirtWL dma-buf allocation is unavailable",
        ));
    };
    let drm_format = ShmHandler::wl_shm_format_to_drm_format(format);
    let (fd, metadata) = channel.allocate_dmabuf(
        u32::try_from(width)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dma-buf width"))?,
        u32::try_from(height)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid dma-buf height"))?,
        drm_format,
    )?;
    let layout = validate_dmabuf_layout(
        drm_format,
        width,
        height,
        metadata.strides,
        metadata.offsets,
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid VirtWL dma-buf layout"))?;
    let (plane1_offset, plane1_stride) = if drm_format == WL_SHM_FORMAT_NV12 {
        (layout.offset1 - layout.offset0, layout.stride1)
    } else {
        (0, 0)
    };
    // Probe the exact mapping that the copy path will use while the allocation
    // is still owned by this function.  A VirtWL driver may expose the ioctl
    // but return metadata that cannot be mmaped (for example a non-page-aligned
    // plane-0 offset); treat that as an allocation failure so the caller can
    // fall back to ordinary VirtWL shared memory.
    if let Some((mapped_ptr, mapped_size)) = map_dmabuf(
        fd.as_raw_fd(),
        DmabufLayout {
            stride0: layout.stride0,
            stride1: layout.stride1,
            offset0: layout.offset0,
            offset1: layout.offset1,
            span: layout.span,
        },
    ) {
        unsafe {
            libc::munmap(mapped_ptr as *mut libc::c_void, mapped_size);
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VirtWL dma-buf cannot be mapped",
        ));
    }
    let total_size = u64::try_from(layout.span).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "VirtWL dma-buf size overflows u64",
        )
    })?;
    Ok(HostBufferAllocation {
        bo: None,
        fd,
        plane1_fd: None,
        stride0: u32::try_from(layout.stride0).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "VirtWL dma-buf stride overflows u32",
            )
        })?,
        modifier: 0,
        offset0: u32::try_from(layout.offset0).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "VirtWL dma-buf offset overflows u32",
            )
        })?,
        total_size,
        plane1_offset,
        plane1_stride,
        direct_dmabuf: true,
        dmabuf_sync: true,
    })
}

fn allocate_gbm_buffer(
    allocator: &Allocator,
    width: i32,
    height: i32,
    format: u32,
    direct_dmabuf: bool,
) -> io::Result<HostBufferAllocation> {
    let bo = allocator
        .allocate(
            u32::try_from(width)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid buffer width"))?,
            u32::try_from(height).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid buffer height")
            })?,
            ShmHandler::wl_shm_format_to_drm_format(format),
        )
        .map_err(io::Error::other)?;
    let drm_format = ShmHandler::wl_shm_format_to_drm_format(format);
    let stride0 = bo.stride_for_plane(0).map_err(io::Error::other)?;
    if stride0 == 0 || stride0 > i32::MAX as u32 || !valid_shm_stride(format, width, stride0 as i32)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "GBM returned an invalid SHM stride",
        ));
    }
    let fd = bo.fd().map_err(io::Error::other)?;
    let plane_count = bo.plane_count().map_err(io::Error::other)?;
    let offset0 = bo.offset(0).map_err(io::Error::other)?;
    let (plane1_fd, plane1_offset, plane1_stride) = if drm_format == WL_SHM_FORMAT_NV12 {
        if plane_count != 2 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("GBM returned {plane_count} planes for NV12, expected 2"),
            ));
        }
        let stride1 = bo.stride_for_plane(1).map_err(io::Error::other)?;
        let offset1 = bo.offset(1).map_err(io::Error::other)?;
        if stride1 < u32::try_from(width).unwrap_or(u32::MAX)
            || !stride1.is_multiple_of(2)
            || offset1 < offset0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "GBM returned invalid NV12 plane-1 metadata",
            ));
        }
        let plane1_fd = bo.fd_for_plane(1).map_err(io::Error::other)?;
        if !same_dma_buf_object(fd.as_raw_fd(), plane1_fd.as_raw_fd()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "GBM NV12 planes use separate dma-buf objects",
            ));
        }
        (Some(plane1_fd), offset1 - offset0, stride1)
    } else {
        if plane_count != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("GBM returned {plane_count} planes for single-plane format"),
            ));
        }
        (None, 0, 0)
    };
    // The modifier is part of the host import contract. Treat an allocator
    // query failure as an allocation failure instead of silently labelling an
    // unknown/tiled BO as LINEAR (modifier 0).
    let modifier = bo.modifier().map(u64::from).map_err(io::Error::other)?;
    if drm_format == WL_SHM_FORMAT_NV12 && modifier != 0 {
        // The NV12 copy path maps the PRIME descriptor directly because GBM's
        // single-plane map API does not promise that the UV plane is present.
        // A tiled/compressed modifier would make those linear byte offsets
        // incorrect even if mmap itself succeeds. Fall back to ordinary SHM
        // allocation instead of presenting corrupted frames.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("GBM returned non-linear NV12 modifier {modifier:#x}"),
        ));
    }
    let offset1 = offset0.checked_add(plane1_offset).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "GBM plane-1 offset arithmetic overflows",
        )
    })?;
    let layout = validate_dmabuf_layout(
        drm_format,
        width,
        height,
        [stride0, plane1_stride, 0],
        [offset0, offset1, 0],
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid GBM buffer layout"))?;
    let total_size = u64::try_from(layout.span)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid GBM buffer size"))?;
    Ok(HostBufferAllocation {
        bo: Some(bo),
        fd,
        plane1_fd,
        stride0,
        modifier,
        offset0,
        total_size,
        plane1_offset: plane1_offset as usize,
        plane1_stride: plane1_stride as usize,
        direct_dmabuf,
        dmabuf_sync: false,
    })
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
    /// Offset and stride of plane 1 in the destination mapping.  For
    /// single-plane formats these remain zero.  NV12 must use the allocator's
    /// returned values instead of assuming plane 1 follows `height * stride`.
    pub(crate) dst_plane1_offset: usize,
    pub(crate) dst_plane1_stride: usize,
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

/// Return the half-open edges of a damage rectangle without overflowing i32.
///
/// Wayland damage coordinates are signed and may extend outside the buffer.
/// Keeping the intermediate edges in i64 lets the union path remain safe
/// before the copy path clips them to the actual image dimensions.
fn damage_edges(rect: DamageRect) -> Option<(i64, i64, i64, i64)> {
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let x0 = i64::from(rect.x);
    let y0 = i64::from(rect.y);
    let x1 = x0.checked_add(i64::from(rect.width))?;
    let y1 = y0.checked_add(i64::from(rect.height))?;
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

fn damage_edges_fit_i32(edges: (i64, i64, i64, i64)) -> bool {
    let (x0, y0, x1, y1) = edges;
    x0 >= i64::from(i32::MIN)
        && y0 >= i64::from(i32::MIN)
        && x0 <= i64::from(i32::MAX)
        && y0 <= i64::from(i32::MAX)
        && x1 <= i64::from(i32::MAX)
        && y1 <= i64::from(i32::MAX)
        && x1 - x0 <= i64::from(i32::MAX)
        && y1 - y0 <= i64::from(i32::MAX)
}

fn damage_from_edges(edges: (i64, i64, i64, i64)) -> DamageRect {
    let (x0, y0, x1, y1) = edges;
    DamageRect::new(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32)
}

fn damage_rects_touch_or_overlap(
    first: (i64, i64, i64, i64),
    second: (i64, i64, i64, i64),
) -> bool {
    let (first_x0, first_y0, first_x1, first_y1) = first;
    let (second_x0, second_y0, second_x1, second_y1) = second;
    first_x0 <= second_x1 && second_x0 <= first_x1 && first_y0 <= second_y1 && second_y0 <= first_y1
}

fn damage_area(edges: (i64, i64, i64, i64)) -> u128 {
    let (x0, y0, x1, y1) = edges;
    (x1 - x0) as u128 * (y1 - y0) as u128
}

fn damage_union_is_compact(
    first: (i64, i64, i64, i64),
    second: (i64, i64, i64, i64),
    expanded: (i64, i64, i64, i64),
) -> bool {
    // A bounding rectangle is safe but can be much larger than two sparse
    // damage regions (for example, two rectangles that only touch at a
    // corner). Keep the merge when the overdraw is bounded to 50%; this still
    // collapses adjacent text/cursor spans while avoiding a sparse 4K frame
    // turning into a full-buffer copy.
    damage_area(expanded).saturating_mul(2)
        <= damage_area(first)
            .saturating_add(damage_area(second))
            .saturating_mul(3)
}

/// Union overlapping or edge-adjacent damage rectangles.
///
/// `wl_surface.damage` is a region operation, not a list of independent
/// copies.  The C++ Sommelier implementation gets this behavior from
/// pixman_region32; keeping a vector of raw requests would copy the same
/// 4K pixels repeatedly when GTK emits several overlapping rectangles for one
/// frame.  A merged bounding rectangle can include a small amount of clean
/// area, which is safe and substantially cheaper than duplicate row copies.
pub(crate) fn coalesce_damage_rects(damage: &[DamageRect]) -> Vec<DamageRect> {
    let mut merged = Vec::with_capacity(damage.len());

    for rect in damage {
        let Some(mut candidate) = damage_edges(*rect) else {
            continue;
        };

        // Merge until the expanded candidate no longer intersects another
        // rectangle.  The fixed-point loop is needed for A touching B and B
        // touching C while A and C do not touch directly.
        while let Some(index) = merged.iter().position(|existing: &DamageRect| {
            damage_edges(*existing).is_some_and(|existing_edges| {
                if !damage_rects_touch_or_overlap(candidate, existing_edges) {
                    return false;
                }
                let expanded = (
                    candidate.0.min(existing_edges.0),
                    candidate.1.min(existing_edges.1),
                    candidate.2.max(existing_edges.2),
                    candidate.3.max(existing_edges.3),
                );
                damage_edges_fit_i32(expanded)
                    && damage_union_is_compact(candidate, existing_edges, expanded)
            })
        }) {
            let existing =
                damage_edges(merged[index]).expect("merged damage rectangles are always positive");
            let expanded = (
                candidate.0.min(existing.0),
                candidate.1.min(existing.1),
                candidate.2.max(existing.2),
                candidate.3.max(existing.3),
            );
            merged.swap_remove(index);
            candidate = expanded;
        }

        // A malformed client can submit representable i32 rectangles whose
        // union exceeds the wire field. Leave those rectangles separate so
        // the existing per-message validation can reject or clamp them
        // without an i64→i32 wrap.
        if damage_edges_fit_i32(candidate) {
            merged.push(damage_from_edges(candidate));
        } else {
            merged.push(*rect);
        }
    }

    merged
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
        dst_plane1_offset,
        dst_plane1_stride,
        height,
    } = layout;
    if src_ptr.is_null()
        || src_ptr as *const libc::c_void == libc::MAP_FAILED
        || dst_ptr.is_null()
        || dst_ptr as *mut libc::c_void == libc::MAP_FAILED
        || width == 0
        || height == 0
        || format_bytes_per_pixel(format).is_none()
    {
        return false;
    }

    let bytes_per_pixel = format_bytes_per_pixel(format).unwrap_or(0);
    let plane_count = if format == WL_SHM_FORMAT_NV12 { 2 } else { 1 };
    let merged_damage = coalesce_damage_rects(damage);
    let mut spans = Vec::new();
    for rect in &merged_damage {
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
                        if dst_plane1_stride == 0 {
                            match height.checked_mul(dst_stride) {
                                Some(value) => value,
                                None => return false,
                            }
                        } else {
                            dst_plane1_offset
                        },
                    )
                } else {
                    (bytes_per_pixel, offset, 0usize)
                };
            let plane_dst_stride = if format == WL_SHM_FORMAT_NV12 && plane == 1 {
                if dst_plane1_stride == 0 {
                    dst_stride
                } else {
                    dst_plane1_stride
                }
            } else {
                dst_stride
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
            let Some(y_offset) = y0.checked_mul(plane_dst_stride) else {
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
                dst_stride: plane_dst_stride,
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
            // A full-width image with matching strides is physically
            // contiguous in both mappings.  One large copy is noticeably
            // cheaper than issuing one call per row for 4K buffers.
            if plane.row_bytes == plane.src_stride && plane.row_bytes == plane.dst_stride {
                let Some(bytes) = plane.row_bytes.checked_mul(plane.rows) else {
                    return false;
                };
                ptr::copy_nonoverlapping(
                    src_ptr.add(plane.src_offset),
                    dst_ptr.add(plane.dst_offset),
                    bytes,
                );
                continue;
            }
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

fn rollback_queued_host_messages(queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>, queue_start: usize) {
    if queue_start > queue.len() {
        return;
    }
    for (_, fds) in queue.drain(queue_start..) {
        for fd in fds {
            if fd >= 0 {
                let _ = nix::unistd::close(fd);
            }
        }
    }
}

fn unmap_destination(ptr: *mut u8, size: usize) {
    if !ptr.is_null() && ptr as *mut libc::c_void != libc::MAP_FAILED {
        unsafe {
            libc::munmap(ptr as *mut libc::c_void, size);
        }
    }
}

fn queue_host_params_destroy(ctx: &mut Context, params_id: u32) -> bool {
    let queued = queue_message(
        &mut ctx.client_to_host_queue,
        params_id,
        crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::REQ_DESTROY,
        MessageBuilder::new(),
        Vec::new(),
    );
    if queued {
        // Keep the numeric ID reserved until the host's wl_display.delete_id
        // acknowledgement arrives. The destroy request is ordered after all
        // already-queued params requests.
        ctx.shadow_table.mark_pending_destroy_host(params_id);
    } else {
        // An empty destroy request should never exceed the wire limit. If a
        // future wire implementation rejects it, do not leave a local ID
        // reservation behind after the host params object is unreachable.
        ctx.shadow_table.remove_host_interface(params_id);
        ctx.fatal_protocol_error = true;
    }
    queued
}

fn cleanup_direct_dmabuf_failure(
    ctx: &mut Context,
    params_id: u32,
    params_queued: bool,
    dest_ptr: *mut u8,
    dest_size: usize,
) {
    unmap_destination(dest_ptr, dest_size);
    if params_queued {
        let _ = queue_host_params_destroy(ctx, params_id);
    } else {
        ctx.shadow_table.remove_host_interface(params_id);
    }
}

pub(crate) fn queue_host_buffer_destroy(ctx: &mut Context, host_id: u32) -> bool {
    let builder = MessageBuilder::new();
    let queued = queue_message(
        &mut ctx.client_to_host_queue,
        host_id,
        0,
        builder,
        Vec::new(),
    );
    if !queued {
        // This is a fixed-size destructor request, so failure indicates a
        // broken wire builder rather than a recoverable client error. Tear
        // down the connection instead of pretending the host object was
        // retired and allowing its ID to be reused.
        ctx.fatal_protocol_error = true;
    }
    queued
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

/// Canonical snapshot of every current and pending surface→buffer edge.
///
/// Pending attaches are kept distinct because they have already reached the
/// host but do not begin a compositor-use interval until commit. Computing the
/// two sets together prevents SHM and native collectors from drifting into
/// subtly different definitions of "referenced".
struct SurfaceBufferReferences {
    current: HashSet<u32>,
    pending: HashSet<u32>,
}

impl SurfaceBufferReferences {
    fn collect(ctx: &Context) -> Self {
        let mut current = HashSet::new();
        let mut pending = HashSet::new();
        for surface in ctx.surfaces.values() {
            current.extend(surface.current_buffer_id);
            pending.extend(surface.pending_buffer_id.flatten());
        }
        Self { current, pending }
    }

    fn contains(&self, guest_id: u32) -> bool {
        self.current.contains(&guest_id) || self.pending.contains(&guest_id)
    }

    fn has_pending(&self, guest_id: u32) -> bool {
        self.pending.contains(&guest_id)
    }
}

fn can_retire_deferred_buffer(
    ctx: &Context,
    references: &SurfaceBufferReferences,
    guest_id: u32,
    allow_submitted: Option<&HashSet<u32>>,
) -> bool {
    (!references.contains(guest_id)
        || (ctx.buffer_is_released(guest_id) && !references.has_pending(guest_id)))
        && (!ctx.buffer_is_submitted(guest_id)
            || allow_submitted.is_some_and(|allowed| allowed.contains(&guest_id)))
}

/// Drop deferred SHM buffers once the host has released them and no pending
/// attach can make them compositor-visible again.
///
/// `current_buffer_id` is intentionally retained until the next surface
/// commit, so it can be stale after a release followed by `attach(NULL)` (or
/// a replacement attach).  A release proves that the old compositor-use
/// interval has ended; only a still-pending attach to the same buffer keeps
/// the host object alive for a possible new interval.
pub(crate) fn collect_retired_buffers(ctx: &mut Context) {
    let references = SurfaceBufferReferences::collect(ctx);
    let releasable: Vec<(u32, u32)> = ctx
        .retired_buffers
        .iter()
        .filter_map(|(&guest_id, buffer)| {
            (ctx.buffer_is_released(guest_id)
                && (!references.contains(guest_id) || !references.has_pending(guest_id)))
            .then_some((guest_id, buffer.host_buffer_id))
        })
        .collect();
    for (guest_id, host_id) in releasable {
        ctx.retired_buffers.remove(&guest_id);
        // Keep the guest↔host mapping reserved until the host acknowledges
        // this destructor with wl_display.delete_id.
        let _ = queue_host_buffer_destroy(ctx, host_id);
        ctx.shadow_table.mark_pending_destroy(guest_id);
        ctx.clear_buffer_use(guest_id);
    }
}

/// Retire guest-destroyed local-copy and native buffers with one lifecycle
/// predicate.
///
/// SHM and native buffers differ only in whether local copy backing must be
/// retained. Surface references, submitted/released phases, and ordered
/// surface-destroy proofs have identical lifetime meaning and therefore must
/// not be evaluated by separate condition trees.
fn collect_deferred_buffers_impl(ctx: &mut Context, allow_submitted: Option<&HashSet<u32>>) {
    let references = SurfaceBufferReferences::collect(ctx);
    let local_copy_buffers: Vec<(u32, u32)> = ctx
        .retired_buffers
        .iter()
        .filter_map(|(&guest_id, buffer)| {
            can_retire_deferred_buffer(ctx, &references, guest_id, allow_submitted)
                .then_some((guest_id, buffer.host_buffer_id))
        })
        .collect();
    let native_buffers: Vec<(u32, u32)> = ctx
        .deferred_host_buffers
        .iter()
        .filter_map(|(&guest_id, &host_id)| {
            can_retire_deferred_buffer(ctx, &references, guest_id, allow_submitted)
                .then_some((guest_id, host_id))
        })
        .collect();

    for (guest_id, host_id) in local_copy_buffers {
        ctx.retired_buffers.remove(&guest_id);
        let _ = queue_host_buffer_destroy(ctx, host_id);
        ctx.shadow_table.mark_pending_destroy(guest_id);
        ctx.clear_buffer_use(guest_id);
    }
    for (guest_id, host_id) in native_buffers {
        ctx.deferred_host_buffers.remove(&guest_id);
        ctx.clear_buffer_use(guest_id);
        let _ = queue_host_buffer_destroy(ctx, host_id);
        ctx.shadow_table.mark_pending_destroy(guest_id);
    }
}

/// A surface destructor is ordered before any buffer destructors that follow
/// it in the guest stream.  Once the destroyed surface is gone, a submitted
/// marker is no longer needed for a buffer that no other surface references:
/// if the guest destroys that buffer later, its host wl_buffer can be retired
/// immediately even when the compositor does not emit a separate release for
/// surface teardown.  Keep the marker while another surface still owns it.
pub(crate) fn clear_buffer_uses_after_surface_destroy(
    ctx: &mut Context,
    destroyed_surface_buffers: &HashSet<u32>,
) {
    let references = SurfaceBufferReferences::collect(ctx);
    for &guest_id in destroyed_surface_buffers {
        if !references.contains(guest_id) {
            ctx.clear_buffer_use(guest_id);
        }
    }
}

/// Retire buffers whose only reference was a pending attach that has since
/// been replaced or cleared. A submitted buffer remains deferred until its
/// release event.
pub(crate) fn collect_deferred_buffers(ctx: &mut Context) {
    collect_deferred_buffers_impl(ctx, None);
}

/// Retire local-copy and native buffers after the owning host surface
/// destructor has been queued. This is the one path that may retire a
/// submitted buffer without waiting for a separate release event.
pub(crate) fn collect_deferred_buffers_after_surface_destroy(
    ctx: &mut Context,
    destroyed_surface_buffers: &HashSet<u32>,
) {
    collect_deferred_buffers_impl(ctx, Some(destroyed_surface_buffers));
}

impl protocols::wayland::wl_shm::WlShmHandler for ShmHandler {
    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if ctx.host_shm_id == Some(ctx.last_sender_id) {
            record_host_shm_wl_format(ctx, format);
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

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        // Synthetic wl_shm is local-only. The generated dispatcher emits the
        // guest delete_id after this handler returns, but it cannot know about
        // the per-object capability cache. Clear it first so a later guest ID
        // reuse cannot receive stale format events from this dead object.
        let guest_id = ctx.last_sender_id;
        ctx.shm_guest_formats.remove(&guest_id);
        ctx.stale_shm_guest_objects.remove(&guest_id);
        Action::Forward
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

        // `--gpu-accel` changes only the output allocation.  Guest wl_shm is
        // still accepted for applications that do not expose linux-dmabuf;
        // those buffers use the VirtWL shared-memory allocation below.
        let wants_dmabuf = ctx.gpu_accel && ctx.host_dmabuf_id.is_some();
        let mut allocation = if wants_dmabuf {
            match allocate_virtwl_dmabuf(ctx, width, height, format) {
                Ok(allocation) => {
                    debug!(
                        "Using VirtWL linux-dmabuf allocation: {}x{} format={:#010x} stride={} modifier={:#x}",
                        width,
                        height,
                        format,
                        allocation.stride0,
                        allocation.modifier
                    );
                    Some(allocation)
                }
                Err(error) => {
                    debug!("VirtWL dma-buf allocation unavailable: {}", error);
                    None
                }
            }
        } else {
            None
        };

        if allocation.is_none() {
            if let Some(channel) = &ctx.virtwayland_channel {
                debug!(
                    "Allocating VirtWayland shared-memory buffer: size={}",
                    buffer_size
                );
                let Some(allocation_size) = virtwl_allocation_size(buffer_size) else {
                    error!(
                        "Rejecting SHM buffer size {}: VirtWL allocation size exceeds u32",
                        buffer_size
                    );
                    return Action::Drop;
                };
                let (fd, _alloc_size) = match channel.allocate(allocation_size) {
                    Ok(result) => result,
                    Err(error) => {
                        error!(
                            "Failed to allocate VirtWayland shared-memory buffer: {}",
                            error
                        );
                        return Action::Drop;
                    }
                };
                debug!(
                    "Using VirtWL shared-memory allocation: {}x{} format={:#010x} size={}",
                    width, height, format, buffer_size
                );
                allocation = Some(HostBufferAllocation {
                    bo: None,
                    fd,
                    plane1_fd: None,
                    stride0: stride as u32,
                    modifier: 0,
                    offset0: 0,
                    total_size: buffer_size as u64,
                    plane1_offset: if format == WL_SHM_FORMAT_NV12 {
                        (height as usize) * (stride as usize)
                    } else {
                        0
                    },
                    plane1_stride: if format == WL_SHM_FORMAT_NV12 {
                        stride as usize
                    } else {
                        0
                    },
                    direct_dmabuf: false,
                    dmabuf_sync: false,
                });
            } else if let Some(allocator) = &ctx.allocator {
                // GBM output is valid only for the direct linux-dmabuf path.
                // Sending a PRIME fd as a wl_shm pool would make the host
                // compositor mmap an object with the wrong protocol contract.
                if !wants_dmabuf {
                    error!(
                        "No VirtWL shared-memory channel is available; \
                         refusing to send a GBM PRIME fd as wl_shm"
                    );
                    return Action::Drop;
                }
                allocation = Some(
                    match allocate_gbm_buffer(allocator, width, height, format, true) {
                        Ok(allocation) => {
                            debug!(
                                "Using GBM linux-dmabuf allocation: {}x{} format={:#010x} stride={} modifier={:#x}",
                                width,
                                height,
                                format,
                                allocation.stride0,
                                allocation.modifier
                            );
                            allocation
                        }
                        Err(error) => {
                            error!("Failed to allocate GBM dma-buf: {}", error);
                            return Action::Drop;
                        }
                    },
                );
            } else {
                error!("No VirtWL or GBM allocator is available");
                return Action::Drop;
            }
        }

        let mut allocation = allocation.expect("SHM allocation must be present");
        let bo = allocation.bo.take();
        let bo_stride = allocation.stride0;
        let dmabuf_fd_owned = allocation.fd;
        let plane1_fd_owned = allocation.plane1_fd;
        let modifier = allocation.modifier;
        let blob_offset = allocation.offset0;
        let total_size = allocation.total_size;
        let plane1_offset = allocation.plane1_offset;
        let plane1_stride = allocation.plane1_stride;
        let direct_dmabuf = allocation.direct_dmabuf;
        let dmabuf_sync = allocation.dmabuf_sync;

        if direct_dmabuf {
            let Some(host_dmabuf_id) = ctx.host_dmabuf_id else {
                error!("GPU allocation succeeded without an internal dmabuf object");
                return Action::Drop;
            };
            let host_plane1_offset = match (blob_offset as usize).checked_add(plane1_offset) {
                Some(offset) => offset,
                None => {
                    error!("VirtWL dma-buf plane-1 offset overflows usize");
                    return Action::Drop;
                }
            };
            let host_plane1_offset_u32 = match u32::try_from(host_plane1_offset) {
                Ok(offset) => offset,
                Err(_) => {
                    error!("VirtWL dma-buf plane-1 offset overflows u32");
                    return Action::Drop;
                }
            };
            let plane1_stride_u32 = match u32::try_from(plane1_stride) {
                Ok(stride) => stride,
                Err(_) => {
                    error!("VirtWL dma-buf plane-1 stride overflows u32");
                    return Action::Drop;
                }
            };
            if format == WL_SHM_FORMAT_NV12
                && (plane1_stride == 0 || host_plane1_offset < blob_offset as usize)
            {
                error!("VirtWL dma-buf returned invalid NV12 plane-1 metadata");
                return Action::Drop;
            }
            let layout = DmabufLayout {
                stride0: bo_stride as usize,
                stride1: plane1_stride,
                offset0: blob_offset as usize,
                offset1: host_plane1_offset,
                span: usize::try_from(total_size).unwrap_or(0),
            };
            let (dest_ptr, dest_size) = if bo.is_none() {
                let Some((ptr, size)) = map_dmabuf(dmabuf_fd_owned.as_raw_fd(), layout) else {
                    error!(
                        "dma-buf cannot be safely mmaped (offset={}, size={})",
                        blob_offset, total_size
                    );
                    return Action::Drop;
                };
                (ptr, size)
            } else if format == WL_SHM_FORMAT_NV12 {
                // GBM's map API does not guarantee that a multi-plane BO's UV
                // plane is included in the mapped slice. A direct dma-buf
                // mapping is therefore mandatory for NV12: falling back to
                // `gbm.map_mut` would expose only the Y plane and make every
                // commit defer forever without ever updating UV.
                let Some(mapped) = map_dmabuf(dmabuf_fd_owned.as_raw_fd(), layout) else {
                    error!("GBM NV12 dma-buf cannot be safely mmaped");
                    return Action::Drop;
                };
                mapped
            } else {
                (std::ptr::null_mut(), 0)
            };

            let params_id = ctx.shadow_table.allocate_host_id();
            let mut params_queued = false;
            ctx.shadow_table.track_host_interface_with_version(
                params_id,
                "zwp_linux_buffer_params_v1".to_string(),
                2,
            );
            let mut builder = MessageBuilder::new();
            builder.write_u32(params_id);
            if !queue_message(
                &mut ctx.client_to_host_queue,
                host_dmabuf_id,
                crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1::REQ_CREATE_PARAMS,
                builder,
                Vec::new(),
            ) {
                cleanup_direct_dmabuf_failure(ctx, params_id, params_queued, dest_ptr, dest_size);
                return Action::Drop;
            }
            params_queued = true;

            let fd_to_send = match dmabuf_fd_owned.try_clone() {
                Ok(fd) => fd.into_raw_fd(),
                Err(error) => {
                    error!("Failed to duplicate VirtWL dma-buf fd: {}", error);
                    cleanup_direct_dmabuf_failure(
                        ctx,
                        params_id,
                        params_queued,
                        dest_ptr,
                        dest_size,
                    );
                    return Action::Drop;
                }
            };
            let mut add = MessageBuilder::new();
            add.write_u32(0); // plane index
            add.write_u32(blob_offset);
            add.write_u32(bo_stride);
            add.write_u32((modifier >> 32) as u32);
            add.write_u32(modifier as u32);
            if !queue_message(
                &mut ctx.client_to_host_queue,
                params_id,
                crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::REQ_ADD,
                add,
                vec![fd_to_send],
            ) {
                cleanup_direct_dmabuf_failure(ctx, params_id, params_queued, dest_ptr, dest_size);
                return Action::Drop;
            }
            if format == WL_SHM_FORMAT_NV12 {
                let mut add_plane1 = MessageBuilder::new();
                add_plane1.write_u32(1);
                add_plane1.write_u32(host_plane1_offset_u32);
                add_plane1.write_u32(plane1_stride_u32);
                add_plane1.write_u32((modifier >> 32) as u32);
                add_plane1.write_u32(modifier as u32);
                let plane1_fd = match plane1_fd_owned
                    .as_ref()
                    .unwrap_or(&dmabuf_fd_owned)
                    .try_clone()
                {
                    Ok(fd) => fd.into_raw_fd(),
                    Err(error) => {
                        error!("Failed to duplicate NV12 dma-buf fd: {}", error);
                        cleanup_direct_dmabuf_failure(
                            ctx,
                            params_id,
                            params_queued,
                            dest_ptr,
                            dest_size,
                        );
                        return Action::Drop;
                    }
                };
                if !queue_message(
                    &mut ctx.client_to_host_queue,
                    params_id,
                    crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::REQ_ADD,
                    add_plane1,
                    vec![plane1_fd],
                ) {
                    cleanup_direct_dmabuf_failure(
                        ctx,
                        params_id,
                        params_queued,
                        dest_ptr,
                        dest_size,
                    );
                    return Action::Drop;
                }
            }

            let host_buffer_id = ctx.shadow_table.allocate_host_id();
            let mut create = MessageBuilder::new();
            create.write_u32(host_buffer_id);
            create.write_i32(width);
            create.write_i32(height);
            create.write_u32(Self::wl_shm_format_to_drm_format(format));
            create.write_u32(0);
            if !queue_message(
                &mut ctx.client_to_host_queue,
                params_id,
                crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::REQ_CREATE_IMMED,
                create,
                Vec::new(),
            ) {
                cleanup_direct_dmabuf_failure(ctx, params_id, params_queued, dest_ptr, dest_size);
                return Action::Drop;
            }
            // create_immed is processed before this destructor in the ordered
            // host stream. Reserve the params ID until wl_display.delete_id
            // acknowledges its destruction.
            let _ = queue_host_params_destroy(ctx, params_id);

            ctx.shadow_table.map_id(id, host_buffer_id);
            ctx.shadow_table
                .track_interface_with_version(id, "wl_buffer".to_string(), 1);
            ctx.shadow_table.set_host_version(host_buffer_id, 1);
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
                    bo_stride,
                    dmabuf_plane1_offset: plane1_offset,
                    dmabuf_plane1_stride: plane1_stride,
                    dmabuf_sync,
                    dest_ptr,
                    dest_size,
                    needs_full_copy: true,
                },
            );
            return Action::Drop;
        }

        // Create WL_SHM buffer on host. Check the capability before mapping
        // the destination so a disappearing wl_shm global cannot leak a raw
        // mmap that has no BufferState owner.
        let Some(host_wl_shm_id) = ctx.host_shm_id else {
            error!("wl_shm not available on host");
            return Action::Drop;
        };
        {
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

            let queue_start = ctx.client_to_host_queue.len();
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
                    unmap_destination(dest_ptr, dest_size);
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
                unmap_destination(dest_ptr, dest_size);
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            }

            // wl_shm_pool.create_buffer(new_id, offset, width, height, stride, format)
            let host_buffer_id = ctx.shadow_table.allocate_host_id();
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_buffer_id);
            let Ok(blob_offset_i32) = i32::try_from(blob_offset) else {
                error!("SHM destination offset exceeds the Wayland i32 range");
                rollback_queued_host_messages(&mut ctx.client_to_host_queue, queue_start);
                unmap_destination(dest_ptr, dest_size);
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            };
            builder.write_i32(blob_offset_i32); // offset
            builder.write_i32(width);
            builder.write_i32(height);
            let Ok(bo_stride_i32) = i32::try_from(bo_stride) else {
                error!("SHM destination stride exceeds the Wayland i32 range");
                rollback_queued_host_messages(&mut ctx.client_to_host_queue, queue_start);
                unmap_destination(dest_ptr, dest_size);
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            };
            builder.write_i32(bo_stride_i32); // stride
            builder.write_u32(format); // format (SHM format, not DRM format)

            if !queue_message(
                &mut ctx.client_to_host_queue,
                host_pool_id,
                0,
                builder,
                Vec::new(),
            ) {
                rollback_queued_host_messages(&mut ctx.client_to_host_queue, queue_start);
                unmap_destination(dest_ptr, dest_size);
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            }

            // wl_shm_pool.destroy()
            let builder = MessageBuilder::new();
            if !queue_message(
                &mut ctx.client_to_host_queue,
                host_pool_id,
                1,
                builder,
                Vec::new(),
            ) {
                rollback_queued_host_messages(&mut ctx.client_to_host_queue, queue_start);
                unmap_destination(dest_ptr, dest_size);
                release_temporary_host_pool(ctx, host_pool_id);
                return Action::Drop;
            }

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
                    dmabuf_plane1_offset: plane1_offset,
                    dmabuf_plane1_stride: plane1_stride,
                    dmabuf_sync: false,
                    dest_ptr,
                    dest_size,
                    needs_full_copy: true,
                },
            );
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
        // wl_surface.attach is forwarded immediately, while the submitted
        // phase starts only when the later wl_surface.commit is handled. A
        // client is allowed to destroy the wl_buffer in that interval; the
        // host surface still owns the attached host buffer and needs its local
        // SHM backing through the pending commit.
        let references = SurfaceBufferReferences::collect(ctx);
        let uncommitted_attach = references.has_pending(guest_id);
        let still_referenced = ctx.buffer_is_submitted(guest_id) || references.contains(guest_id);

        // A guest may destroy a wl_buffer while a surface still has it
        // attached. Keep the backing mmap and the guest↔host mapping alive
        // until the host compositor sends wl_buffer.release; otherwise a
        // damage-only commit can dereference unmapped memory and the host
        // release event can be routed to a newly reused object ID.
        if let Some(mut buffer) = ctx.buffers.remove(&guest_id) {
            if still_referenced && (!ctx.buffer_is_released(guest_id) || uncommitted_attach) {
                // Keep the host wl_buffer alive until its release event. A
                // destroy request would remove the host resource before it
                // can report release, leaving no compositor-use lifetime
                // signal for the deferred mmap. The guest object and host
                // object therefore have deliberately different destruction
                // points.  If the previous interval was already released,
                // retain that edge while a pending attach is unresolved;
                // `on_commit` clears it when a new interval actually starts,
                // while replacing the attach lets the collector destroy the
                // host object immediately.
                buffer.guest_buffer_id = guest_id;
                ctx.retired_buffers.insert(guest_id, buffer);
                ctx.shadow_table.retire_guest_object(guest_id);
            } else {
                if let Some(host_id) = host_id {
                    let _ = queue_host_buffer_destroy(ctx, host_id);
                }
                clear_surface_buffer_references(ctx, guest_id);
                ctx.shadow_table.mark_pending_destroy(guest_id);
                ctx.clear_buffer_use(guest_id);
            }
        } else {
            // Native linux-dmabuf buffers have no local mapping to retire, but
            // the host compositor still owns the attached resource until it
            // sends wl_buffer.release. Keep the mapping alive across a guest
            // destroy just like the SHM path.
            // A release may have arrived before the guest destroys the
            // wl_buffer. In that case the host has already completed its use
            // interval and no deferred lifetime signal remains to wait for.
            let host_already_released = ctx.buffer_is_released(guest_id);
            if still_referenced && (!host_already_released || uncommitted_attach) {
                if let Some(host_id) = host_id {
                    ctx.deferred_host_buffers.insert(guest_id, host_id);
                    // Keep a release edge that predates the unresolved
                    // pending attach.  A later commit clears it and starts a
                    // fresh use interval; replacing the attach allows the
                    // collector to retire the already-idle host buffer.
                    ctx.shadow_table.retire_guest_object(guest_id);
                    return Action::Drop;
                }
            } else if let Some(host_id) = host_id {
                ctx.clear_buffer_use(guest_id);
                let _ = queue_host_buffer_destroy(ctx, host_id);
            } else {
                ctx.clear_buffer_use(guest_id);
            }
            clear_surface_buffer_references(ctx, guest_id);
            ctx.shadow_table.mark_pending_destroy(guest_id);
            ctx.clear_buffer_use(guest_id);
        }
        Action::Drop
    }

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let pending_attach = SurfaceBufferReferences::collect(ctx).has_pending(guest_id);

        if let Some(deferred_host_id) = ctx.deferred_host_buffers.remove(&guest_id) {
            if pending_attach {
                // This release completes the previous compositor-use
                // interval. Keep the deferred host object alive because the
                // pending attach may still be committed; the collector uses
                // this marker to retire it if the attach is replaced first.
                ctx.mark_buffer_released(guest_id);
                ctx.deferred_host_buffers.insert(guest_id, deferred_host_id);
                return Action::Drop;
            }
            // The guest object was destroyed before the host compositor
            // released a native dma-buf. Now that the release is the lifetime
            // signal, destroy the host proxy and retain its mapping until the
            // corresponding wl_display.delete_id arrives.
            ctx.clear_buffer_use(guest_id);
            let _ = queue_host_buffer_destroy(ctx, deferred_host_id);
            ctx.shadow_table.mark_pending_destroy(guest_id);
            clear_surface_buffer_references(ctx, guest_id);
            return Action::Drop;
        }

        if ctx.retired_buffers.contains_key(&guest_id) {
            if pending_attach {
                // The release belongs to the previous compositor-use
                // interval. Keep the retired state and host object alive for
                // the pending attach; on commit the state is reopened for a
                // new interval, while replacing the attach lets the
                // collectors queue the destructor.
                ctx.mark_buffer_released(guest_id);
                return Action::Drop;
            }
            // The guest object is already gone, so there is no valid object on
            // which to deliver the release event. It is nevertheless the
            // release that makes it safe to drop the deferred backing storage.
            ctx.mark_buffer_released(guest_id);
            // The host object remained alive specifically so this release
            // could arrive. It is now safe to destroy the host proxy and drop
            // the local backing.
            let _ = queue_host_buffer_destroy(ctx, host_id);
            ctx.shadow_table.mark_pending_destroy(guest_id);
            ctx.clear_buffer_use(guest_id);
            clear_surface_buffer_references(ctx, guest_id);
            // The backing state can be dropped now that the compositor sent
            // release, but the host object still owes wl_display.delete_id
            // for the queued destroy request. Keep its numeric mapping until
            // that acknowledgement instead of letting collect_retired_buffers
            // remove it immediately.
            ctx.retired_buffers.remove(&guest_id);
            return Action::Drop;
        }

        // Retain the release edge for both SHM and native buffers so a later
        // guest destroy can retire the host proxy immediately even when a
        // surface still stores the buffer as its current content.
        ctx.mark_buffer_released(guest_id);
        // A release is the compositor's lifetime signal. Once it arrives, the
        // backing storage may be reused or destroyed, even if the surface
        // still has the buffer as its current content. Keeping this marker in
        // a submitted phase would make a later guest destroy incorrectly
        // retain an already-idle buffer.
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
    use super::map_dmabuf;
    use super::ShmHandler;
    use super::{
        backing_fd_has_size, clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats,
        coalesce_damage_rects, collect_deferred_buffers, collect_retired_buffers, copy_shm_damage,
        copy_shm_planes, guest_shm_format_available, record_host_shm_drm_format,
        record_host_shm_format, record_host_shm_wl_format, register_guest_shm,
        release_temporary_host_pool, same_dma_buf_object, valid_buffer_layout, valid_pool_resize,
        valid_pool_size, valid_shm_stride, validate_dmabuf_layout, virtwl_allocation_size,
        DmabufLayout, WL_SHM_FORMAT_NV12,
    };
    use crate::handler::registry::RegistryHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_shm::WlShmHandler;
    use crate::state::DamageRect;
    use crate::state::{BufferState, Context, PoolInner, PoolState};
    use crate::wire::{Action, WireMessage};
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
    fn coalesces_overlapping_and_compact_adjacent_damage() {
        let damage = [
            DamageRect::new(0, 0, 8, 4),
            DamageRect::new(6, 2, 8, 4),
            DamageRect::new(14, 2, 2, 4),
        ];

        assert_eq!(
            coalesce_damage_rects(&damage),
            vec![DamageRect::new(0, 0, 16, 6)]
        );
    }

    #[test]
    fn keeps_sparse_corner_damage_separate() {
        let damage = [DamageRect::new(0, 0, 2, 2), DamageRect::new(2, 2, 2, 2)];

        assert_eq!(coalesce_damage_rects(&damage), damage);
    }

    #[test]
    fn coalescing_discards_empty_damage_without_overflowing_edges() {
        let damage = [
            DamageRect::new(0, 0, 0, 8),
            DamageRect::new(i32::MAX, i32::MAX, 1, 1),
            DamageRect::new(4, 4, 2, 2),
        ];

        assert_eq!(
            coalesce_damage_rects(&damage),
            vec![
                DamageRect::new(i32::MAX, i32::MAX, 1, 1),
                DamageRect::new(4, 4, 2, 2)
            ]
        );
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
                dst_plane1_offset: 0,
                dst_plane1_stride: 0,
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
                dst_plane1_offset: 0,
                dst_plane1_stride: 0,
                height: 4,
            },
        ));
        assert!(
            destination.iter().all(|byte| *byte == 0),
            "an invalid UV span must not leave the Y plane partially copied"
        );
    }

    #[test]
    fn copy_rejects_null_or_failed_mappings_before_pointer_arithmetic() {
        let mut destination = [0u8; 4];
        let layout = super::ShmCopyLayout {
            pool_size: 4,
            dest_size: destination.len(),
            format: 0,
            offset: 0,
            width: 1,
            src_stride: 4,
            dst_stride: 4,
            dst_plane1_offset: 0,
            dst_plane1_stride: 0,
            height: 1,
        };
        let damage = [DamageRect::new(0, 0, 1, 1)];
        assert!(!copy_shm_damage(
            std::ptr::null(),
            destination.as_mut_ptr(),
            layout,
            &damage
        ));
        assert!(!copy_shm_damage(
            [0u8; 4].as_ptr(),
            std::ptr::null_mut(),
            layout,
            &damage
        ));
        assert!(!copy_shm_damage(
            libc::MAP_FAILED.cast(),
            destination.as_mut_ptr(),
            layout,
            &damage
        ));
    }

    #[test]
    fn copies_nv12_plane_one_at_allocator_returned_offset() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let mut destination = [0u8; 32];

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
                dst_plane1_offset: 20,
                dst_plane1_stride: 4,
                height: 4,
            },
        ));
        assert_eq!(&destination[..16], &[0x11; 16]);
        assert_eq!(&destination[16..20], &[0; 4]);
        assert_eq!(&destination[20..28], &[0x22; 8]);
        assert_eq!(&destination[28..], &[0; 4]);
    }

    #[test]
    fn copies_partial_nv12_damage_using_plane_one_stride() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..20].fill(0x21);
        source[20..24].fill(0x22);
        let mut destination = [0xcc; 36];

        assert!(copy_shm_damage(
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
                dst_plane1_offset: 20,
                dst_plane1_stride: 8,
                height: 4,
            },
            &[DamageRect::new(0, 2, 4, 2)],
        ));

        // Only the damaged bottom half is changed.  The UV row must use its
        // own eight-byte destination stride, not the four-byte Y stride.
        assert_eq!(&destination[..8], &[0xcc; 8]);
        assert_eq!(&destination[8..16], &[0x11; 8]);
        assert_eq!(&destination[16..20], &[0xcc; 4]);
        assert_eq!(&destination[20..28], &[0xcc; 8]);
        assert_eq!(&destination[28..32], &[0x22; 4]);
        assert_eq!(&destination[32..36], &[0xcc; 4]);
    }

    #[test]
    fn validates_dmabuf_metadata_without_assuming_plane_one_for_single_plane() {
        let layout = validate_dmabuf_layout(0x3432_5258, 64, 32, [512, 0, 0], [4096, 0, 0])
            .expect("single-plane metadata with a non-zero plane-0 offset");
        assert_eq!(layout.offset0, 4096);
        assert_eq!(layout.offset1, 0);
        assert_eq!(layout.stride1, 0);
        assert_eq!(layout.span, 4096 + 512 * 32);

        assert!(validate_dmabuf_layout(
            WL_SHM_FORMAT_NV12,
            64,
            32,
            [64, 64, 0],
            [0, 64 * 32 + 4096, 0],
        )
        .is_some());
        assert!(
            validate_dmabuf_layout(WL_SHM_FORMAT_NV12, 64, 32, [64, 64, 0], [4096, 0, 0],)
                .is_none()
        );
        assert!(
            validate_dmabuf_layout(
                WL_SHM_FORMAT_NV12,
                64,
                32,
                [64, 64, 0],
                [0, 64 * 16 - 64, 0],
            )
            .is_none(),
            "NV12 plane metadata must not overlap the Y plane"
        );
    }

    #[test]
    fn rejects_unrepresentable_or_unaligned_dmabuf_mappings() {
        let too_large = usize::try_from(isize::MAX).unwrap().saturating_add(1);
        assert!(map_dmabuf(
            0,
            DmabufLayout {
                stride0: 4,
                stride1: 0,
                offset0: 0,
                offset1: 0,
                span: too_large,
            }
        )
        .is_none());
        assert!(map_dmabuf(
            0,
            DmabufLayout {
                stride0: 4,
                stride1: 0,
                offset0: 1,
                offset1: 0,
                span: 4097,
            }
        )
        .is_none());
    }

    #[test]
    fn rejects_regular_dmabuf_backing_shorter_than_metadata() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("sommelier-short-dmabuf").unwrap().as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");

        assert!(
            map_dmabuf(
                fd.as_raw_fd(),
                DmabufLayout {
                    stride0: 64,
                    stride1: 0,
                    offset0: 0,
                    offset1: 0,
                    span: 8192,
                }
            )
            .is_none(),
            "mmap preflight must reject a regular backing that would SIGBUS"
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
    fn nv12_is_not_advertised_without_a_real_output_allocator() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.host_dmabuf_id = Some(7);
        ctx.allocator = None;
        ctx.host_shm_formats.insert(WL_SHM_FORMAT_NV12);
        assert!(
            !guest_shm_format_available(&ctx, WL_SHM_FORMAT_NV12),
            "advertising NV12 without VirtWL or GBM would make create_buffer fail"
        );
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
    fn synthetic_shm_release_clears_cache_before_guest_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        let optional_format = 0x3631_4752;
        ctx.host_shm_formats.insert(optional_format);
        ctx.shadow_table
            .track_interface_with_version(guest_shm, "wl_shm".to_string(), 2);
        register_guest_shm(&mut ctx, guest_shm);
        ctx.host_to_client_queue.clear();
        ctx.stale_shm_guest_objects.insert(guest_shm);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        let mut release = WireMessage::new(
            guest_shm,
            crate::protocols::wayland::wl_shm::REQ_RELEASE,
            &[],
            &[],
        );
        assert_eq!(
            crate::protocols::wayland::wl_shm::dispatch_request(
                &mut release,
                &mut handler,
                &mut ctx
            ),
            Ok(None)
        );
        assert!(!ctx.shm_guest_formats.contains_key(&guest_shm));
        assert!(!ctx.stale_shm_guest_objects.contains(&guest_shm));
        assert!(ctx.shadow_table.get_interface(guest_shm).is_none());
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "local wl_shm.release must emit one guest delete_id"
        );

        // Reusing the same guest ID must behave like a fresh synthetic object.
        ctx.shadow_table
            .track_interface_with_version(guest_shm, "wl_shm".to_string(), 2);
        register_guest_shm(&mut ctx, guest_shm);
        ctx.host_to_client_queue.clear();
        record_host_shm_format(&mut ctx, optional_format);
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a capability already sent by the replacement object must not be duplicated"
        );
    }

    #[test]
    fn source_separated_host_formats_survive_single_source_removal() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        let format = 0x3631_4752;
        register_guest_shm(&mut ctx, guest_shm);
        ctx.host_to_client_queue.clear();

        record_host_shm_wl_format(&mut ctx, format);
        record_host_shm_drm_format(&mut ctx, format);
        assert!(ctx.host_wl_shm_formats.contains(&format));
        assert!(ctx.host_dmabuf_shm_formats.contains(&format));
        assert!(ctx.host_shm_formats.contains(&format));
        assert_eq!(ctx.host_to_client_queue.len(), 1);

        ctx.host_to_client_queue.clear();
        clear_host_shm_dmabuf_formats(&mut ctx);
        assert!(
            ctx.host_shm_formats.contains(&format),
            "wl_shm remains a valid capability source"
        );
        assert!(ctx.host_to_client_queue.is_empty());

        clear_host_shm_wl_formats(&mut ctx);
        assert!(!ctx.host_shm_formats.contains(&format));

        record_host_shm_wl_format(&mut ctx, format);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            0,
            "an existing object must not receive a duplicate format event"
        );
        register_guest_shm(&mut ctx, guest_shm + 1);
        assert!(
            ctx.host_to_client_queue
                .iter()
                .any(
                    |(message, _)| u32::from_ne_bytes(message[8..12].try_into().unwrap()) == format
                ),
            "a replacement synthetic object must receive the re-advertised format"
        );
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
    fn identifies_dup_fds_for_one_dma_buf_object() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("sommelier-dmabuf-identity")
                .unwrap()
                .as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        let duplicate = fd.try_clone().expect("duplicate fd");
        assert!(same_dma_buf_object(fd.as_raw_fd(), duplicate.as_raw_fd()));

        let (pipe_read, pipe_write) = nix::unistd::pipe().expect("pipe");
        assert!(!same_dma_buf_object(fd.as_raw_fd(), pipe_read.as_raw_fd()));
        drop(pipe_write);
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
        ctx.mark_buffer_submitted(guest_buffer);
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
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
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
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
            },
        );
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(!ctx.buffer_is_submitted(guest_buffer));
        assert!(ctx.buffer_is_released(guest_buffer));

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
    fn destroying_submitted_non_shm_buffer_defers_until_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(
            ctx.deferred_host_buffers.contains_key(&guest_buffer),
            "a submitted native buffer must stay alive until host release"
        );
        assert!(ctx.buffer_is_submitted(guest_buffer));
        assert_eq!(ctx.client_to_host_queue.len(), 0);

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(!ctx.deferred_host_buffers.contains_key(&guest_buffer));
        assert!(!ctx.buffer_is_submitted(guest_buffer));
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer)
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
    }

    #[test]
    fn native_release_before_guest_destroy_is_recorded() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(ctx.buffer_is_released(guest_buffer));
        assert!(!ctx.buffer_is_submitted(guest_buffer));

        // The surface can still retain the released buffer as its current
        // content. Destroying the guest object must use the recorded release
        // edge instead of waiting for a second event that cannot arrive.
        ctx.surfaces.entry(100).or_default().current_buffer_id = Some(guest_buffer);
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(ctx.deferred_host_buffers.is_empty());
        assert!(ctx.host_buffer_use(guest_buffer).is_none());
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
    }

    #[test]
    fn local_copy_and_native_buffers_share_one_retirement_oracle() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let local_buffer = 20;
        let local_host = 30;
        let native_buffer = 21;
        let native_host = 31;

        for (guest_id, host_id) in [(local_buffer, local_host), (native_buffer, native_host)] {
            ctx.shadow_table.map_id(guest_id, host_id);
            ctx.shadow_table
                .track_interface_with_version(guest_id, "wl_buffer".to_string(), 1);
            ctx.shadow_table.set_host_version(host_id, 1);
            ctx.shadow_table.retire_guest_object(guest_id);
            ctx.mark_buffer_released(guest_id);
        }
        let pool = Arc::new(PoolState {
            client_fd: -1,
            inner: RwLock::new(PoolInner {
                client_ptr: std::ptr::null_mut(),
                size: 0,
            }),
        });
        ctx.retired_buffers.insert(
            local_buffer,
            BufferState {
                guest_buffer_id: local_buffer,
                pool,
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                host_buffer_id: local_host,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
            },
        );
        ctx.deferred_host_buffers.insert(native_buffer, native_host);
        ctx.surfaces.entry(40).or_default().pending_buffer_id = Some(Some(local_buffer));
        ctx.surfaces.entry(41).or_default().pending_buffer_id = Some(Some(native_buffer));

        collect_deferred_buffers(&mut ctx);
        assert!(ctx.retired_buffers.contains_key(&local_buffer));
        assert!(ctx.deferred_host_buffers.contains_key(&native_buffer));

        ctx.surfaces.get_mut(&40).unwrap().pending_buffer_id = Some(None);
        ctx.surfaces.get_mut(&41).unwrap().pending_buffer_id = Some(None);
        collect_deferred_buffers(&mut ctx);

        assert!(ctx.retired_buffers.is_empty());
        assert!(ctx.deferred_host_buffers.is_empty());
        assert!(ctx.host_buffer_use(local_buffer).is_none());
        assert!(ctx.host_buffer_use(native_buffer).is_none());
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(|(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap()))
                .collect::<Vec<_>>(),
            vec![local_host, native_host],
            "local-copy and native buffers must make the same lifecycle decision"
        );
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
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                needs_full_copy: false,
            },
        );
        ctx.shadow_table.map_id(guest_buffer, host_buffer);
        ctx.shadow_table
            .track_interface_with_version(guest_buffer, "wl_buffer".to_string(), 1);
        ctx.shadow_table.set_host_version(host_buffer, 1);

        collect_retired_buffers(&mut ctx);
        assert!(ctx.retired_buffers.contains_key(&guest_buffer));

        ctx.mark_buffer_released(guest_buffer);
        collect_retired_buffers(&mut ctx);
        assert!(!ctx.retired_buffers.contains_key(&guest_buffer));
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer),
            "the host mapping remains reserved until wl_display.delete_id"
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .any(
                    |(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap())
                        == host_buffer
                ),
            "retiring a released buffer must queue its host destructor"
        );
    }
}
