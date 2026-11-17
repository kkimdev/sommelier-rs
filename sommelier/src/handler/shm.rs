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
use crate::protocols;
use crate::protocols::linux_dmabuf_v1::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use crate::protocols::wayland::{wl_shm, wl_shm_pool};
use crate::state::{BufferState, Context, DamageRect, DamageRegion, HostId, ShmPool, StalePixels};
use crate::wire::Action;
use log::{debug, error, warn};
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
use std::ptr;
use std::sync::Arc;

pub struct ShmHandler;

/// Formats advertised by ChromiumOS Sommelier's wl_shm shim. Every format
/// after ARGB/XRGB has the same FourCC value in wl_shm and DRM, so the
/// linux-dmabuf shim uses them as DRM formats too.
pub(crate) const SUPPORTED_SHM_FORMATS: [u32; 6] = [
    wl_shm::FORMAT_ARGB8888,
    wl_shm::FORMAT_XRGB8888,
    wl_shm::FORMAT_NV12,
    wl_shm::FORMAT_RGB565,
    wl_shm::FORMAT_ABGR8888,
    wl_shm::FORMAT_XBGR8888,
];

const MANDATORY_SHM_FORMATS: [u32; 2] = [wl_shm::FORMAT_ARGB8888, wl_shm::FORMAT_XRGB8888];

fn valid_pool_size(size: i32) -> Option<usize> {
    (size > 0)
        .then_some(size as usize)
        .filter(|&size| size <= isize::MAX as usize)
}

/// `wl_shm_pool.resize` may only grow the pool; the same size is a no-op.
fn valid_pool_resize(current_size: usize, new_size: i32) -> Option<usize> {
    let new_size = valid_pool_size(new_size)?;
    (new_size >= current_size).then_some(new_size)
}

fn supported_shm_format(format: u32) -> bool {
    SUPPORTED_SHM_FORMATS.contains(&format)
}

fn shm_format_from_drm(format: u32) -> Option<u32> {
    match format {
        // DRM_FORMAT_ARGB8888/XRGB8888 use FourCC values while wl_shm uses
        // the enum values 0/1 for the same layouts.
        0x3432_5241 => Some(wl_shm::FORMAT_ARGB8888),
        0x3432_5258 => Some(wl_shm::FORMAT_XRGB8888),
        wl_shm::FORMAT_NV12 => Some(wl_shm::FORMAT_NV12),
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
        && (format != wl_shm::FORMAT_NV12
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
            ctx.host_to_client_queue
                .push(wl_shm::evt::format(guest_id, format));
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
        ctx.host_to_client_queue
            .push(wl_shm::evt::format(guest_id, format));
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
        && (format != wl_shm::FORMAT_NV12 || (width.is_multiple_of(2) && stride.is_multiple_of(2)))
}

fn format_bytes_per_pixel(format: u32) -> Option<usize> {
    match format {
        wl_shm::FORMAT_NV12 => Some(1),
        wl_shm::FORMAT_RGB565 => Some(2),
        wl_shm::FORMAT_ARGB8888
        | wl_shm::FORMAT_XRGB8888
        | wl_shm::FORMAT_ABGR8888
        | wl_shm::FORMAT_XBGR8888 => Some(4),
        _ => None,
    }
}

/// Whether `file` is a regular file of at least `size` bytes.
///
/// The bridge reads pools with `pread`, which needs a regular file: a memfd,
/// or a tmpfs or `shm_open` file. Checking the size here reports a short pool
/// when it is created or resized; a later truncation fails the read itself.
fn regular_file_has_size(file: &File, size: usize) -> bool {
    file.metadata().is_ok_and(|metadata| {
        metadata.file_type().is_file()
            && u64::try_from(size).is_ok_and(|size| metadata.len() >= size)
    })
}

/// Whether two descriptors refer to the same dma-buf object, by inode.
///
/// Each dma-buf has its own inode only from Linux 5.3; on older kernels all
/// dma-bufs share one anonymous inode, and this reports any two of them as
/// the same object. A `true` is therefore not proof on every kernel.
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
) -> Option<i32> {
    if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
        return None;
    }
    if format == wl_shm::FORMAT_NV12 && (width % 2 != 0 || height % 2 != 0) {
        return None;
    }
    let offset = offset as usize;
    let stride = stride as usize;
    let height = height as usize;
    let bytes = if format == wl_shm::FORMAT_NV12 {
        let chroma_rows = height.checked_div(2)?;
        stride
            .checked_mul(height)?
            .checked_add(stride.checked_mul(chroma_rows)?)?
    } else {
        stride.checked_mul(height)?
    };
    let end = offset.checked_add(bytes)?;
    if end > pool_size {
        return None;
    }
    // A copied buffer becomes a host wl_shm pool, whose size is an i32.
    i32::try_from(bytes).ok()
}

/// Validated dma-buf layouts. A submodule so that nothing in this file can
/// build a [`DmabufLayout`] except [`validate_dmabuf_layout`].
mod dmabuf_layout {
    use crate::protocols::wayland::wl_shm;

    /// Where one plane lies in a dma-buf, as `zwp_linux_buffer_params_v1.add`
    /// sends it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct DmabufPlane {
        pub(super) offset: u32,
        pub(super) stride: u32,
    }

    /// A dma-buf layout that [`validate_dmabuf_layout`], its only constructor,
    /// accepted. The private fields keep its invariants: plane 1 exists only
    /// for NV12 and begins after all of plane 0, and `span` covers every plane.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct DmabufLayout {
        plane0: DmabufPlane,
        plane1: Option<DmabufPlane>,
        span: usize,
    }

    impl DmabufLayout {
        pub(super) fn plane0(&self) -> DmabufPlane {
            self.plane0
        }

        /// The NV12 chroma plane, in the same dma-buf after all of plane 0.
        pub(super) fn plane1(&self) -> Option<DmabufPlane> {
            self.plane1
        }

        /// One past the last byte of the planes.
        pub(super) fn span(&self) -> usize {
            self.span
        }

        /// Plane 1's offset from plane 0, or 0 for a single-plane format.
        pub(super) fn plane1_relative_offset(&self) -> usize {
            self.plane1
                .map_or(0, |plane1| (plane1.offset - self.plane0.offset) as usize)
        }

        /// Plane 1's stride, or 0 for a single-plane format.
        pub(super) fn plane1_stride(&self) -> usize {
            self.plane1.map_or(0, |plane1| plane1.stride as usize)
        }
    }

    /// Validate the metadata returned by a host dma-buf allocator and calculate
    /// the byte span that must be mapped in the guest.
    ///
    /// The host may pad rows and place plane 1 after an implementation-defined
    /// gap.  Deriving the span from the returned offsets/strides keeps all mmap
    /// and copy bounds checks consistent with the metadata sent to the compositor.
    pub(super) fn validate_dmabuf_layout(
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

        let (plane0_bpp, plane_count) = if format == wl_shm::FORMAT_NV12 {
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
        let plane0 = DmabufPlane {
            offset: offsets[0],
            stride: strides[0],
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
                plane0,
                plane1: None,
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
            plane0,
            plane1: Some(DmabufPlane {
                offset: offsets[1],
                stride: strides[1],
            }),
            span: plane0_end.max(plane1_end),
        })
    }
}
use dmabuf_layout::{validate_dmabuf_layout, DmabufLayout};

/// The host storage chosen for one guest SHM buffer.
enum ShmBacking {
    /// Shared memory (VirtWL, or a memfd for a local compositor) laid out
    /// exactly like the guest buffer (offset 0, the guest stride and size).
    /// The host imports it as a `wl_shm` pool.
    Shared(OwnedFd),
    /// A dma-buf the host imports through the internal `zwp_linux_dmabuf_v1`.
    Dmabuf {
        host_dmabuf_id: u32,
        allocation: DmabufAllocation,
    },
}

/// A host dma-buf allocation for a guest SHM buffer.
///
/// Both planes of an NV12 buffer are expected in the one dma-buf `fd`, which
/// the copy path maps for both: VirtWL allocates a single object, and
/// [`allocate_gbm_buffer`] rejects plane objects that
/// [`same_dma_buf_object`] can tell apart.
struct DmabufAllocation {
    bo: Option<gbm::BufferObject<()>>,
    fd: OwnedFd,
    /// GBM's own descriptor for NV12 plane 1, which plane 1's `add` sends.
    /// [`same_dma_buf_object`] cannot prove it is `fd`'s object on every
    /// kernel, so the host gets exactly what GBM returned. `None` for VirtWL,
    /// whose single descriptor plane 1 duplicates.
    plane1_fd: Option<OwnedFd>,
    layout: DmabufLayout,
    modifier: u64,
    dmabuf_sync: bool,
}

fn map_dmabuf(fd: RawFd, layout: DmabufLayout) -> Option<(*mut u8, usize)> {
    let offset0 = layout.plane0().offset as usize;
    let span = layout.span();
    if fd < 0 || span <= offset0 {
        return None;
    }
    // `mmap` takes a signed length and an `off_t` offset. Avoid lossy casts
    // even on a wider host where a malicious Wayland metadata value can fit
    // in usize but not in either syscall argument.
    if span > isize::MAX as usize
        || span - offset0 > isize::MAX as usize
        || libc::off_t::try_from(offset0).is_err()
    {
        return None;
    }
    // mmap(2) requires a page-aligned file offset. VirtWL/minigbm normally
    // returns offset 0 for linear allocations; reject an incompatible layout
    // rather than mapping the wrong bytes.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = usize::try_from(page_size).ok()?;
    if page_size == 0 || !offset0.is_multiple_of(page_size) {
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
        let required_size = u64::try_from(span).ok()?;
        if file_size < required_size {
            return None;
        }
    }
    let map_size = span - offset0;
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            map_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            libc::off_t::try_from(offset0).ok()?,
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
) -> io::Result<DmabufAllocation> {
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
    // Probe the exact mapping that the copy path will use while the allocation
    // is still owned by this function.  A VirtWL driver may expose the ioctl
    // but return metadata that cannot be mmaped (for example a non-page-aligned
    // plane-0 offset); treat that as an allocation failure so the caller can
    // fall back to ordinary VirtWL shared memory.
    if let Some((mapped_ptr, mapped_size)) = map_dmabuf(fd.as_raw_fd(), layout) {
        unsafe {
            libc::munmap(mapped_ptr as *mut libc::c_void, mapped_size);
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VirtWL dma-buf cannot be mapped",
        ));
    }
    Ok(DmabufAllocation {
        bo: None,
        fd,
        plane1_fd: None,
        layout,
        modifier: 0,
        dmabuf_sync: true,
    })
}

fn allocate_gbm_buffer(
    allocator: &Allocator,
    width: i32,
    height: i32,
    format: u32,
) -> io::Result<DmabufAllocation> {
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
    let (plane1_fd, stride1, offset1) = if drm_format == wl_shm::FORMAT_NV12 {
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
        // The copy path maps `fd` for both planes, so they must be the same
        // dma-buf object.
        let plane1_fd = bo.fd_for_plane(1).map_err(io::Error::other)?;
        if !same_dma_buf_object(fd.as_raw_fd(), plane1_fd.as_raw_fd()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "GBM NV12 planes use separate dma-buf objects",
            ));
        }
        (Some(plane1_fd), stride1, offset1)
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
    if drm_format == wl_shm::FORMAT_NV12 && modifier != 0 {
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
    let layout = validate_dmabuf_layout(
        drm_format,
        width,
        height,
        [stride0, stride1, 0],
        [offset0, offset1, 0],
    )
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid GBM buffer layout"))?;
    Ok(DmabufAllocation {
        bo: Some(bo),
        fd,
        plane1_fd,
        layout,
        modifier,
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
/// Keeping the source file and destination pointer separate from this value
/// makes the copy entry point difficult to call with mismatched dimensions
/// while avoiding a long list of loosely-related scalar arguments.
#[derive(Clone, Copy)]
pub(crate) struct ShmCopyLayout {
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

impl ShmCopyLayout {
    /// The layout that copies `buffer` into its own host mapping.
    pub(crate) fn for_buffer(buffer: &BufferState) -> Option<Self> {
        Some(Self {
            dest_size: buffer.dest_size,
            format: buffer.format,
            offset: usize::try_from(buffer.offset).ok()?,
            width: usize::try_from(buffer.width).ok()?,
            src_stride: buffer.stride as usize,
            dst_stride: buffer.bo_stride as usize,
            dst_plane1_offset: buffer.dmabuf_plane1_offset,
            dst_plane1_stride: buffer.dmabuf_plane1_stride,
            height: usize::try_from(buffer.height).ok()?,
        })
    }
}

/// Why copying a SHM buffer to its host storage failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ShmCopyError {
    /// The host storage cannot take the copy: the buffer metadata does not
    /// fit it, or it could not be mapped or synchronized. A later commit
    /// retries the copy.
    #[error("the host storage cannot take the SHM copy")]
    Destination,
    /// The guest's pool file could not supply the pixels, for example
    /// because the client truncated it after creating the pool.
    #[error("cannot read the guest SHM pool: {0}")]
    Source(#[from] io::Error),
}

/// Check one plane span against the destination mapping, and that its last
/// source byte is addressable.
fn valid_plane_copy(dest_size: usize, plane: PlaneCopy) -> bool {
    if plane.src_stride == 0
        || plane.dst_stride == 0
        || plane.rows == 0
        || plane.row_bytes == 0
        || plane.row_bytes > plane.src_stride
        || plane.row_bytes > plane.dst_stride
    {
        return false;
    }
    let Some(last_row) = plane.rows.checked_sub(1) else {
        return false;
    };
    let src_end = last_row
        .checked_mul(plane.src_stride)
        .and_then(|value| value.checked_add(plane.src_offset))
        .and_then(|value| value.checked_add(plane.row_bytes));
    let dst_end = last_row
        .checked_mul(plane.dst_stride)
        .and_then(|value| value.checked_add(plane.dst_offset))
        .and_then(|value| value.checked_add(plane.row_bytes));
    src_end.is_some() && dst_end.is_some_and(|dst_end| dst_end <= dest_size)
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

/// Whether copying `damage` would write any pixel of `buffer`'s host copy:
/// damage that clips to nothing needs no copy, and no dma-buf sync either.
pub(crate) fn damage_reaches_buffer(damage: &DamageRegion, buffer: &BufferState) -> bool {
    let (Ok(width), Ok(height)) = (
        usize::try_from(buffer.width),
        usize::try_from(buffer.height),
    ) else {
        // Malformed dimensions: let the copy report them.
        return true;
    };
    match damage {
        DamageRegion::Empty => false,
        DamageRegion::Full => width > 0 && height > 0,
        DamageRegion::Rects(rects) => rects
            .iter()
            .any(|&rect| clipped_damage(rect, width, height).is_some()),
    }
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
    source: &File,
    dst_ptr: *mut u8,
    layout: ShmCopyLayout,
    damage: &DamageRegion,
) -> Result<(), ShmCopyError> {
    let ShmCopyLayout {
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
    if dst_ptr.is_null()
        || dst_ptr as *mut libc::c_void == libc::MAP_FAILED
        || width == 0
        || height == 0
        || format_bytes_per_pixel(format).is_none()
    {
        return Err(ShmCopyError::Destination);
    }

    let bytes_per_pixel = format_bytes_per_pixel(format).unwrap_or(0);
    let plane_count = if format == wl_shm::FORMAT_NV12 { 2 } else { 1 };
    let merged_damage = match damage {
        DamageRegion::Full => vec![DamageRect::new(
            0,
            0,
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
        )],
        DamageRegion::Empty | DamageRegion::Rects(_) => coalesce_damage_rects(damage.rects()),
    };
    let mut spans = Vec::new();
    for rect in &merged_damage {
        let Some((mut x0, mut y0, mut x1, mut y1)) = clipped_damage(*rect, width, height) else {
            continue;
        };

        for plane in 0..plane_count {
            let (plane_bpp, plane_src_offset, plane_dst_offset) =
                if format == wl_shm::FORMAT_NV12 && plane == 1 {
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
                            None => return Err(ShmCopyError::Destination),
                        }) {
                            Some(value) => value,
                            None => return Err(ShmCopyError::Destination),
                        },
                        if dst_plane1_stride == 0 {
                            match height.checked_mul(dst_stride) {
                                Some(value) => value,
                                None => return Err(ShmCopyError::Destination),
                            }
                        } else {
                            dst_plane1_offset
                        },
                    )
                } else {
                    (bytes_per_pixel, offset, 0usize)
                };
            let plane_dst_stride = if format == wl_shm::FORMAT_NV12 && plane == 1 {
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
                return Err(ShmCopyError::Destination);
            };
            let Some(y_offset) = y0.checked_mul(src_stride) else {
                return Err(ShmCopyError::Destination);
            };
            let Some(x_offset) = x0.checked_mul(plane_bpp) else {
                return Err(ShmCopyError::Destination);
            };
            let Some(src_offset) = plane_src_offset
                .checked_add(y_offset)
                .and_then(|value| value.checked_add(x_offset))
            else {
                return Err(ShmCopyError::Destination);
            };
            let Some(y_offset) = y0.checked_mul(plane_dst_stride) else {
                return Err(ShmCopyError::Destination);
            };
            let Some(dst_offset) = plane_dst_offset
                .checked_add(y_offset)
                .and_then(|value| value.checked_add(x_offset))
            else {
                return Err(ShmCopyError::Destination);
            };
            let plane = PlaneCopy {
                src_offset,
                dst_offset,
                src_stride,
                dst_stride: plane_dst_stride,
                rows,
                row_bytes,
            };
            if !valid_plane_copy(dest_size, plane) {
                return Err(ShmCopyError::Destination);
            }
            spans.push(plane);
        }
    }

    // Every span was validated above, before the first read, so bad
    // metadata leaves the host storage untouched. A read that fails part
    // way does leave it partly updated; the caller then stales the whole
    // host copy, or ends the session. An empty list is a valid no-op.
    for plane in spans {
        // A full-width span with matching strides is one contiguous range in
        // both the pool file and the destination, which one read copies.
        // That is the common case, and avoids a syscall per row of a 4K
        // buffer.
        let (chunk_bytes, chunks) =
            if plane.row_bytes == plane.src_stride && plane.row_bytes == plane.dst_stride {
                (plane.row_bytes * plane.rows, 1)
            } else {
                (plane.row_bytes, plane.rows)
            };
        for chunk in 0..chunks {
            let src_offset = plane.src_offset + chunk * plane.src_stride;
            let dst_offset = plane.dst_offset + chunk * plane.dst_stride;
            // Safety: valid_plane_copy checked that every row of the span,
            // and so this chunk, lies inside the destination mapping, and
            // nothing else references that memory during the copy.
            let destination =
                unsafe { std::slice::from_raw_parts_mut(dst_ptr.add(dst_offset), chunk_bytes) };
            // The read fails, instead of raising SIGBUS like a mapping
            // would, if the client truncated the pool file. EFAULT is the
            // destination mapping's fault, not the client's. (A FUSE-backed
            // pool could also return EFAULT; it is then treated as a host
            // storage failure, which only drops the commit.)
            source
                .read_exact_at(destination, src_offset as u64)
                .map_err(|error| match error.raw_os_error() {
                    Some(libc::EFAULT) => ShmCopyError::Destination,
                    _ => ShmCopyError::Source(error),
                })?;
        }
    }
    Ok(())
}

/// Report a resource failure while creating a `wl_buffer`.
///
/// `create_buffer` carries a client supplied `new_id`; silently dropping the
/// request leaves the client believing that the ID was created while the
/// proxy has no object to service it.  Wayland errors terminate the connection,
/// which is the only safe outcome once allocation has failed.
fn queue_buffer_creation_error(ctx: &mut Context, object_id: u32, message: &'static str) {
    ctx.reject(object_id, wl_shm::ERROR_INVALID_FD, message);
}

/// Undo the guest mapping after a host `wl_buffer` has already been queued.
///
/// The host stream is ordered, so queueing `wl_buffer.destroy` after the
/// create requests retires the host object even though no local
/// `BufferState` was registered.  Keep the host numeric ID reserved until its
/// `delete_id` acknowledgement; otherwise a subsequent allocation could route
/// a late release to an unrelated object.
fn rollback_registered_buffer(ctx: &mut Context, guest_id: u32, host_id: u32) {
    if ctx.shadow_table.get_host_id(guest_id) != Some(host_id) {
        return;
    }
    queue_host_buffer_destroy(ctx, host_id);
    ctx.shadow_table.guest_ended_host_destroyed(guest_id);
}

fn queue_host_params_destroy(ctx: &mut Context, params_id: u32) {
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::destroy(params_id));
    // Keep the numeric ID reserved until the host's wl_display.delete_id
    // acknowledgement arrives. The destroy request is ordered after all
    // already-queued params requests.
    ctx.retire_destroyed_host_object(params_id);
}

/// Queue the host `wl_buffer.destroy`; the caller retires the ID mapping.
pub(crate) fn queue_host_buffer_destroy(ctx: &mut Context, host_id: u32) {
    ctx.client_to_host_queue
        .push(protocols::wayland::wl_buffer::req::destroy(host_id));
}

fn clear_surface_buffer_references(ctx: &mut Context, guest_id: u32) {
    for surface in ctx.surfaces.values_mut() {
        surface.clear_buffer_references(guest_id);
    }
}

fn record_host_buffer_release(ctx: &mut Context, guest_id: u32) {
    if ctx.render_buffer_host_id(guest_id).is_some() && !ctx.mark_buffer_released(guest_id) {
        log::warn!(
            "Ignoring wl_buffer.release that no render use owes for guest {}",
            guest_id
        );
    }
}

fn retire_destroyed_buffer_mapping(ctx: &mut Context, guest_id: u32, host_id: HostId) {
    if !ctx.complete_queued_buffer_destroy(guest_id, host_id) {
        ctx.fail(format_args!(
            "Render buffer generation diverged while destroying guest {} host {}",
            guest_id, host_id.0
        ));
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
            current.extend(surface.current_buffer_id());
            pending.extend(surface.pending.attachment.buffer_id());
        }
        Self { current, pending }
    }

    fn contains(&self, guest_id: u32) -> bool {
        self.current.contains(&guest_id) || self.pending.contains(&guest_id)
    }

    fn has_current(&self, guest_id: u32) -> bool {
        self.current.contains(&guest_id)
    }

    fn has_pending(&self, guest_id: u32) -> bool {
        self.pending.contains(&guest_id)
    }
}

fn can_retire_deferred_buffer(
    ctx: &Context,
    references: &SurfaceBufferReferences,
    guest_id: u32,
) -> bool {
    (!references.contains(guest_id)
        || (ctx.buffer_is_released(guest_id) && !references.has_pending(guest_id)))
        && !ctx.buffer_is_submitted(guest_id)
}

/// Retire every eligible guest-destroyed local-copy or native buffer.
///
/// SHM and native buffers differ only in whether local copy backing must be
/// retained. Surface references, submitted/released phases, and ordered
/// surface-destroy proofs have identical lifetime meaning and therefore must
/// not be evaluated by separate condition trees.
pub(crate) fn retire_eligible_buffers(ctx: &mut Context) {
    let references = SurfaceBufferReferences::collect(ctx);
    let candidates: Vec<(u32, HostId)> = ctx
        .render_buffer_lifecycles()
        .filter_map(|(host_id, lifecycle)| {
            if !lifecycle.is_guest_destroyed() {
                return None;
            }
            let guest_id = ctx.shadow_table.get_guest_id(host_id.0)?;
            can_retire_deferred_buffer(ctx, &references, guest_id).then_some((guest_id, host_id))
        })
        .collect();

    for (guest_id, host_id) in candidates {
        queue_host_buffer_destroy(ctx, host_id.0);
        // Keep the guest↔host mapping and empty registry generation reserved
        // until wl_display.delete_id acknowledges the host destructor.
        // Server-created generations are removed immediately because neither
        // side emits delete_id.
        retire_destroyed_buffer_mapping(ctx, guest_id, host_id);
    }
}

/// Apply the ordering proof provided by one successfully queued surface
/// destructor to the buffer that surface held.
///
/// The authoritative current-surface edges live in `Context::surfaces`. The
/// registry stores only whether an earlier detached use still lacks a host
/// release, so destroying a different current surface cannot erase that
/// unresolved use.
pub(crate) fn clear_buffer_uses_after_surface_destroy(
    ctx: &mut Context,
    destroyed_surface_buffer: Option<u32>,
) {
    let Some(guest_id) = destroyed_surface_buffer else {
        return;
    };
    let references = SurfaceBufferReferences::collect(ctx);
    if ctx.host_buffer_use(guest_id).is_some()
        && !ctx.finish_surface_destroy_use(guest_id, references.has_current(guest_id))
    {
        ctx.fail(format_args!(
            "Render buffer {guest_id} could not finish its destroyed-surface use"
        ));
    }
}

impl protocols::wayland::wl_shm::WlShmHandler for ShmHandler {
    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if ctx.host_shm_id == Some(ctx.last_sender_id) {
            record_host_shm_wl_format(ctx, format);
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_create_pool(
        &mut self,
        ctx: &mut Context,
        id: u32,
        fd: BorrowedFd<'_>,
        size: i32,
    ) -> Action {
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
            return ctx.reject(
                ctx.last_sender_id,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm pool size",
            );
        };
        // The request closes `fd` once handled, so the pool owns a
        // duplicate, close-on-exec like every descriptor the proxy keeps.
        let file = match fd.try_clone_to_owned() {
            Ok(duplicate) => File::from(duplicate),
            Err(error) => {
                error!("Failed to dup FD for SHM pool: {}", error);
                return ctx.reject(
                    ctx.last_sender_id,
                    wl_shm::ERROR_INVALID_FD,
                    "invalid wl_shm pool fd",
                );
            }
        };
        if !regular_file_has_size(&file, pool_size) {
            warn!(
                "Rejecting SHM pool fd {} that is not a regular file of at least {} bytes",
                fd.as_raw_fd(),
                pool_size
            );
            return ctx.reject(
                ctx.last_sender_id,
                wl_shm::ERROR_INVALID_FD,
                "invalid wl_shm pool fd",
            );
        }
        let pool = ShmPool {
            file: Arc::new(file),
            size: pool_size,
        };

        ctx.pools.insert(pool_id, pool);
        ctx.shadow_table.create_local(pool_id, "wl_shm_pool", 1);

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

/// A `wl_shm_pool.create_buffer` request that passed protocol validation.
struct ShmBufferRequest {
    id: u32,
    pool_id: u32,
    pool_file: Arc<File>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
    /// The buffer's size in bytes, as [`valid_buffer_layout`] returned it.
    size: i32,
}

/// Choose and allocate the host storage for a guest SHM buffer.
///
/// # Returns
///
/// `None` after queueing the creation error when no storage is available.
fn select_shm_backing(ctx: &mut Context, request: &ShmBufferRequest) -> Option<ShmBacking> {
    let ShmBufferRequest {
        pool_id,
        width,
        height,
        format,
        size: buffer_size,
        ..
    } = *request;
    debug!("Allocator present={}", ctx.allocator.is_some());
    debug!(
        "VirtWayland channel present={}",
        ctx.virtwayland_channel.is_some()
    );

    // `--gpu-accel` changes only the output allocation.  Guest wl_shm is
    // still accepted for applications that do not expose linux-dmabuf;
    // those buffers use the VirtWL shared-memory allocation below.
    let host_dmabuf_id = ctx.host_dmabuf_id.filter(|_| ctx.gpu_accel);
    let virtwl_dmabuf = host_dmabuf_id.and_then(|host_dmabuf_id| {
        match allocate_virtwl_dmabuf(ctx, width, height, format) {
            Ok(allocation) => {
                debug!(
                    "Using VirtWL linux-dmabuf allocation: {}x{} format={:#010x} stride={} modifier={:#x}",
                    width,
                    height,
                    format,
                    allocation.layout.plane0().stride,
                    allocation.modifier
                );
                Some(ShmBacking::Dmabuf {
                    host_dmabuf_id,
                    allocation,
                })
            }
            Err(error) => {
                debug!("VirtWL dma-buf allocation unavailable: {}", error);
                None
            }
        }
    });

    let backing = if let Some(backing) = virtwl_dmabuf {
        backing
    } else if let Some(channel) = &ctx.virtwayland_channel {
        debug!(
            "Allocating VirtWayland shared-memory buffer: size={}",
            buffer_size
        );
        // `valid_buffer_layout` returns a positive size.
        let (fd, _alloc_size) = match channel.allocate(buffer_size.unsigned_abs()) {
            Ok(result) => result,
            Err(error) => {
                error!(
                    "Failed to allocate VirtWayland shared-memory buffer: {}",
                    error
                );
                queue_buffer_creation_error(
                    ctx,
                    pool_id,
                    "unable to allocate wl_shm buffer backing",
                );
                return None;
            }
        };
        debug!(
            "Using VirtWL shared-memory allocation: {}x{} format={:#010x} size={}",
            width, height, format, buffer_size
        );
        ShmBacking::Shared(fd)
    } else if let (Some(allocator), Some(host_dmabuf_id)) = (&ctx.allocator, host_dmabuf_id) {
        // GBM output is valid only for the direct linux-dmabuf path: a PRIME
        // fd sent as a wl_shm pool would make the host compositor mmap an
        // object with the wrong protocol contract.
        match allocate_gbm_buffer(allocator, width, height, format) {
            Ok(allocation) => {
                debug!(
                    "Using GBM linux-dmabuf allocation: {}x{} format={:#010x} stride={} modifier={:#x}",
                    width,
                    height,
                    format,
                    allocation.layout.plane0().stride,
                    allocation.modifier
                );
                ShmBacking::Dmabuf {
                    host_dmabuf_id,
                    allocation,
                }
            }
            Err(error) => {
                error!("Failed to allocate GBM dma-buf: {}", error);
                queue_buffer_creation_error(
                    ctx,
                    pool_id,
                    "unable to allocate wl_shm buffer backing",
                );
                return None;
            }
        }
    } else {
        // Without VirtWL the host is a local compositor (`--local-compositor`),
        // which maps ordinary shared memory as a wl_shm pool.
        match local_shared_memory(buffer_size.unsigned_abs()) {
            Ok(fd) => {
                debug!(
                    "Using local shared-memory allocation: {}x{} format={:#010x} size={}",
                    width, height, format, buffer_size
                );
                ShmBacking::Shared(fd)
            }
            Err(error) => {
                error!("Failed to allocate local shared memory: {}", error);
                queue_buffer_creation_error(
                    ctx,
                    pool_id,
                    "unable to allocate wl_shm buffer backing",
                );
                return None;
            }
        }
    };
    Some(backing)
}

/// A memfd of `size` bytes, the host storage for a guest SHM buffer when the
/// host is a local compositor that maps it directly.
fn local_shared_memory(size: u32) -> io::Result<OwnedFd> {
    let fd = nix::sys::memfd::memfd_create(
        c"sommelier-shm-buffer",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )?;
    nix::unistd::ftruncate(&fd, size.into())?;
    Ok(fd)
}

/// Create a guest SHM buffer backed by shared memory ([`ShmBacking::Shared`]),
/// which the host imports as a temporary `wl_shm` pool.
fn create_shared_buffer(ctx: &mut Context, request: ShmBufferRequest, shared_fd: OwnedFd) {
    let ShmBufferRequest {
        pool_id,
        ref pool_file,
        offset,
        width,
        height,
        stride,
        format,
        size: buffer_size,
        ..
    } = request;
    // Check the capability before mapping the destination so a
    // disappearing wl_shm global cannot leak a raw mmap that has
    // no BufferState owner.
    let Some(host_wl_shm_id) = ctx.host_shm_id else {
        error!("wl_shm not available on host");
        queue_buffer_creation_error(ctx, pool_id, "host wl_shm is unavailable");
        return;
    };
    let fd_to_send = match shared_fd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            error!("Failed to dup FD: {}", e);
            queue_buffer_creation_error(ctx, pool_id, "unable to duplicate wl_shm buffer backing");
            return;
        }
    };
    debug!(
        "Dup for send={} size={}",
        fd_to_send.as_raw_fd(),
        buffer_size
    );

    // Map the buffer for SHM synchronization. Both VirtWL allocations
    // and memfds are ordinary shared-memory fds and can be mapped directly.
    let dest_size = buffer_size.unsigned_abs() as usize;
    let dest_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            dest_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            shared_fd.as_raw_fd(),
            0,
        ) as *mut u8
    };
    if dest_ptr as *mut libc::c_void == libc::MAP_FAILED {
        error!(
            "Failed to mmap shared SHM buffer: {:?}",
            std::io::Error::last_os_error()
        );
        queue_buffer_creation_error(ctx, pool_id, "unable to map wl_shm buffer backing");
        return;
    }

    // The temporary host wl_shm_pool remains live until the queued
    // destroy request is processed by the compositor, although it has no
    // guest-side object or event callbacks. A new object takes its
    // parent's version.
    let pool_version = ctx
        .shadow_table
        .host_object_version_or_unknown(host_wl_shm_id);
    let host_pool_id = ctx
        .shadow_table
        .create_host_only("wl_shm_pool", pool_version);

    let host_buffer_id = ctx
        .shadow_table
        .create_paired(request.id, "wl_buffer", 1, 1);
    ctx.client_to_host_queue.extend([
        wl_shm::req::create_pool(host_wl_shm_id, host_pool_id, fd_to_send, buffer_size),
        // The buffer starts the pool, and keeps its SHM (not DRM) format.
        wl_shm_pool::req::create_buffer(
            host_pool_id,
            host_buffer_id,
            0,
            width,
            height,
            stride,
            format,
        ),
        wl_shm_pool::req::destroy(host_pool_id),
    ]);

    // The three requests are emitted in one ordered stream:
    // create_pool, create_buffer, destroy. Once queued, a later
    // allocation may safely reuse this temporary ID because the
    // host will process the destroy before the later request.
    // Keeping the registration forever would leak one host ID per
    // SHM buffer.
    ctx.retire_destroyed_host_object(host_pool_id);

    let (plane1_offset, plane1_stride) = if format == wl_shm::FORMAT_NV12 {
        ((height as usize) * (stride as usize), stride as usize)
    } else {
        (0, 0)
    };
    let state = BufferState {
        pool_file: Arc::clone(pool_file),
        offset,
        width,
        height,
        stride: stride as u32,
        format,
        bo: None,
        dmabuf_fd: Some(shared_fd),
        bo_stride: stride as u32,
        dmabuf_plane1_offset: plane1_offset,
        dmabuf_plane1_stride: plane1_stride,
        dmabuf_sync: false,
        dest_ptr,
        dest_size,
        stale: StalePixels::all(),
    };
    register_shm_buffer(
        ctx,
        &request,
        host_buffer_id,
        state,
        "unable to register wl_shm buffer",
    );
}

/// Create a guest SHM buffer backed by a dma-buf, which the host imports
/// through its `zwp_linux_dmabuf_v1` factory.
fn create_dmabuf_buffer(
    ctx: &mut Context,
    request: ShmBufferRequest,
    host_dmabuf_id: u32,
    allocation: DmabufAllocation,
) {
    let ShmBufferRequest {
        pool_id,
        ref pool_file,
        offset,
        width,
        height,
        stride,
        format,
        ..
    } = request;
    let DmabufAllocation {
        bo,
        fd: dmabuf_fd_owned,
        plane1_fd,
        layout,
        modifier,
        dmabuf_sync,
    } = allocation;

    // Duplicate the descriptors the ADDs hand to the host before mapping
    // or queueing anything, so a failure has nothing to undo.
    let fd_to_send = match dmabuf_fd_owned.try_clone() {
        Ok(fd) => fd,
        Err(error) => {
            error!("Failed to duplicate VirtWL dma-buf fd: {}", error);
            queue_buffer_creation_error(ctx, pool_id, "unable to duplicate wl_shm dma-buf backing");
            return;
        }
    };
    let plane1_add = if let Some(plane1) = layout.plane1() {
        match plane1_fd.as_ref().unwrap_or(&dmabuf_fd_owned).try_clone() {
            Ok(fd) => Some((plane1, fd)),
            Err(error) => {
                error!("Failed to duplicate NV12 dma-buf fd: {}", error);
                queue_buffer_creation_error(
                    ctx,
                    pool_id,
                    "unable to duplicate wl_shm dma-buf plane",
                );
                return;
            }
        }
    } else {
        None
    };

    let (dest_ptr, dest_size) = if bo.is_none() {
        let Some((ptr, size)) = map_dmabuf(dmabuf_fd_owned.as_raw_fd(), layout) else {
            error!(
                "dma-buf cannot be safely mmaped (offset={}, size={})",
                layout.plane0().offset,
                layout.span()
            );
            queue_buffer_creation_error(ctx, pool_id, "unable to map wl_shm dma-buf backing");
            return;
        };
        (ptr, size)
    } else if format == wl_shm::FORMAT_NV12 {
        // GBM's map API does not guarantee that a multi-plane BO's UV
        // plane is included in the mapped slice. A direct dma-buf
        // mapping is therefore mandatory for NV12: falling back to
        // `gbm.map_mut` would expose only the Y plane and make every
        // commit defer forever without ever updating UV.
        let Some(mapped) = map_dmabuf(dmabuf_fd_owned.as_raw_fd(), layout) else {
            error!("GBM NV12 dma-buf cannot be safely mmaped");
            queue_buffer_creation_error(ctx, pool_id, "unable to map wl_shm dma-buf backing");
            return;
        };
        mapped
    } else {
        (std::ptr::null_mut(), 0)
    };

    // A new object takes its parent's version.
    let params_version = ctx
        .shadow_table
        .host_object_version_or_unknown(host_dmabuf_id);
    let params_id = ctx
        .shadow_table
        .create_host_only("zwp_linux_buffer_params_v1", params_version);
    ctx.client_to_host_queue
        .push(zwp_linux_dmabuf_v1::req::create_params(
            host_dmabuf_id,
            params_id,
        ));
    let (modifier_hi, modifier_lo) = ((modifier >> 32) as u32, modifier as u32);
    let plane0 = layout.plane0();
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::add(
            params_id,
            fd_to_send,
            0,
            plane0.offset,
            plane0.stride,
            modifier_hi,
            modifier_lo,
        ));
    if let Some((plane1, plane1_fd)) = plane1_add {
        ctx.client_to_host_queue
            .push(zwp_linux_buffer_params_v1::req::add(
                params_id,
                plane1_fd,
                1,
                plane1.offset,
                plane1.stride,
                modifier_hi,
                modifier_lo,
            ));
    }

    let host_buffer_id = ctx
        .shadow_table
        .create_paired(request.id, "wl_buffer", 1, 1);
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::create_immed(
            params_id,
            host_buffer_id,
            width,
            height,
            ShmHandler::wl_shm_format_to_drm_format(format),
            0,
        ));
    // create_immed is processed before this destructor in the ordered
    // host stream. Reserve the params ID until wl_display.delete_id
    // acknowledges its destruction.
    queue_host_params_destroy(ctx, params_id);

    let state = BufferState {
        pool_file: Arc::clone(pool_file),
        offset,
        width,
        height,
        stride: stride as u32,
        format,
        bo,
        dmabuf_fd: Some(dmabuf_fd_owned),
        bo_stride: layout.plane0().stride,
        dmabuf_plane1_offset: layout.plane1_relative_offset(),
        dmabuf_plane1_stride: layout.plane1_stride(),
        dmabuf_sync,
        dest_ptr,
        dest_size,
        stale: StalePixels::all(),
    };
    register_shm_buffer(
        ctx,
        &request,
        host_buffer_id,
        state,
        "unable to register wl_shm dma-buf buffer",
    );
}

/// Register the storage of the guest `wl_buffer` paired with host buffer
/// `host_buffer_id`. A duplicate host generation rolls the buffer back and
/// reports `failure`.
fn register_shm_buffer(
    ctx: &mut Context,
    request: &ShmBufferRequest,
    host_buffer_id: u32,
    state: BufferState,
    failure: &'static str,
) {
    if !ctx.register_local_buffer(request.id, host_buffer_id, state) {
        error!("Duplicate render buffer host ID {}", host_buffer_id);
        rollback_registered_buffer(ctx, request.id, host_buffer_id);
        queue_buffer_creation_error(ctx, request.pool_id, failure);
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
        let Some(pool) = ctx.pools.get(&pool_id) else {
            warn!("Unknown pool ID: {}", pool_id);
            return ctx.reject(pool_id, wl_shm::ERROR_INVALID_FD, "invalid wl_shm pool");
        };
        let pool_file = Arc::clone(&pool.file);
        let pool_size = pool.size;
        if !supported_shm_format(format) || !guest_shm_format_available(ctx, format) {
            warn!("Rejecting unsupported SHM buffer format {:#010x}", format);
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_FORMAT,
                "invalid wl_shm buffer format",
            );
        }
        if !valid_shm_stride(format, width, stride) {
            warn!(
                "Rejecting SHM buffer stride {} for format {:#010x} and width {}",
                stride, format, width
            );
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm buffer stride",
            );
        }

        let Some(buffer_size) =
            valid_buffer_layout(pool_size, offset, width, height, stride, format)
        else {
            warn!(
                "Rejecting invalid SHM buffer: pool={}, offset={}, width={}, height={}, stride={}, pool_size={}",
                pool_id, offset, width, height, stride, pool_size
            );
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm buffer layout",
            );
        };

        let request = ShmBufferRequest {
            id,
            pool_id,
            pool_file,
            offset,
            width,
            height,
            stride,
            format,
            size: buffer_size,
        };
        match select_shm_backing(ctx, &request) {
            Some(ShmBacking::Shared(shared_fd)) => create_shared_buffer(ctx, request, shared_fd),
            Some(ShmBacking::Dmabuf {
                host_dmabuf_id,
                allocation,
            }) => create_dmabuf_buffer(ctx, request, host_dmabuf_id, allocation),
            None => {}
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
        let Some(pool) = ctx.pools.get_mut(&pool_id) else {
            warn!("Pool not found for resize: {}", pool_id);
            return Action::Drop;
        };
        let error = match valid_pool_resize(pool.size, size) {
            // A client that needs a smaller pool must create a new one;
            // shrinking could invalidate live wl_buffers that still
            // reference the old tail. libwayland reports it as invalid_fd.
            None => {
                warn!(
                    "Rejecting shrinking SHM pool resize from {} to {}",
                    pool.size, size
                );
                Some((wl_shm::ERROR_INVALID_FD, "shrinking pool invalid"))
            }
            Some(new_size) if !regular_file_has_size(&pool.file, new_size) => {
                warn!(
                    "Rejecting SHM pool resize to {}: the pool file is too short",
                    new_size
                );
                Some((wl_shm::ERROR_INVALID_FD, "invalid wl_shm pool fd"))
            }
            Some(new_size) => {
                debug!(
                    "Resizing pool id={} from {} to {}",
                    pool_id, pool.size, new_size
                );
                pool.size = new_size;
                None
            }
        };
        match error {
            Some((code, message)) => ctx.reject(pool_id, code, message),
            None => Action::Drop,
        }
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

        // Local-copy and native buffers have identical Wayland lifetime
        // semantics. Keep either backing alive while a committed use interval
        // or unresolved pending attach can still produce host traffic.
        if ctx.render_buffer_host_id(guest_id).is_some()
            && still_referenced
            && (!ctx.buffer_is_released(guest_id) || uncommitted_attach)
        {
            if !ctx.mark_buffer_guest_destroyed(guest_id) {
                return ctx.fail(format_args!(
                    "Render buffer {guest_id} could not enter guest-destroyed state"
                ));
            }
            ctx.shadow_table.guest_destroyed_in_use(guest_id);
            return Action::Drop;
        }

        if let Some(host_id) = host_id {
            queue_host_buffer_destroy(ctx, host_id);
            retire_destroyed_buffer_mapping(ctx, guest_id, HostId(host_id));
        }
        clear_surface_buffer_references(ctx, guest_id);
        Action::Drop
    }

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let pending_attach = SurfaceBufferReferences::collect(ctx).has_pending(guest_id);

        if ctx.host_buffer_is_guest_destroyed(host_id) {
            if pending_attach {
                // This release completes the previous use interval. Keep the
                // deferred object alive because the pending attach may still
                // begin another interval.
                record_host_buffer_release(ctx, guest_id);
                return Action::Drop;
            }
            record_host_buffer_release(ctx, guest_id);
            queue_host_buffer_destroy(ctx, host_id);
            retire_destroyed_buffer_mapping(ctx, guest_id, HostId(host_id));
            clear_surface_buffer_references(ctx, guest_id);
            return Action::Drop;
        }

        // Retain the release phase for both local-copy and native buffers so a later
        // guest destroy can retire the host proxy immediately even when a
        // surface still stores the buffer as its current content.
        record_host_buffer_release(ctx, guest_id);
        // A release is the compositor's lifetime signal. Once it arrives, the
        // backing storage may be reused or destroyed, even if the surface
        // still has the buffer as its current content. Keeping this marker in
        // a submitted phase would make a later guest destroy incorrectly
        // retain an already-idle buffer.
        Action::Forward
    }
}

impl ShmHandler {
    fn wl_shm_format_to_drm_format(format: u32) -> u32 {
        match format {
            wl_shm::FORMAT_ARGB8888 => 0x34325241, // DRM_FORMAT_ARGB8888
            wl_shm::FORMAT_XRGB8888 => 0x34325258, // DRM_FORMAT_XRGB8888
            _ => format,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dmabuf_layout::DmabufPlane;
    use super::map_dmabuf;
    use super::ShmHandler;
    use super::{
        clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, coalesce_damage_rects,
        copy_shm_damage, guest_shm_format_available, record_host_shm_drm_format,
        record_host_shm_format, record_host_shm_wl_format, register_guest_shm,
        regular_file_has_size, retire_eligible_buffers, rollback_registered_buffer,
        same_dma_buf_object, valid_buffer_layout, valid_pool_resize, valid_pool_size,
        valid_shm_stride, validate_dmabuf_layout, ShmCopyError,
    };
    use crate::handler::registry::RegistryHandler;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_shm;
    use crate::protocols::wayland::wl_shm::WlShmHandler;
    use crate::state::{BufferState, Context, HostId, RenderBufferLifecycle, ShmPool, StalePixels};
    use crate::state::{DamageRect, DamageRegion};
    use crate::test_support::{
        msg_sender, protocol_error_code, queued_headers, raw_dispatch, shm_pool_file,
    };
    use crate::wire::{Action, WireMessage};
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::sync::Arc;

    fn register_test_local(ctx: &mut Context, guest_id: u32, host_id: u32, backing: BufferState) {
        assert!(ctx.register_local_buffer(guest_id, host_id, backing));
    }

    fn register_test_native(ctx: &mut Context, host_id: u32) {
        assert!(ctx.register_native_buffer(host_id, (1, 1), Vec::new()));
    }

    fn register_test_server_native(ctx: &mut Context, host_id: u32) -> u32 {
        let guest_id = ctx.shadow_table.adopt_host_created(host_id, "wl_buffer", 1);
        register_test_native(ctx, host_id);
        guest_id
    }

    fn mark_test_buffer_guest_destroyed(ctx: &mut Context, guest_id: u32) {
        assert!(ctx.mark_buffer_guest_destroyed(guest_id));
        ctx.shadow_table.guest_destroyed_in_use(guest_id);
    }

    fn buffer_lifecycle(ctx: &Context, host_id: u32) -> Option<RenderBufferLifecycle> {
        ctx.render_buffer_lifecycle_for_host(HostId(host_id))
    }

    fn buffer_is_guest_destroyed(ctx: &Context, host_id: u32) -> bool {
        buffer_lifecycle(ctx, host_id).is_some_and(|lifecycle| lifecycle.is_guest_destroyed())
    }

    fn buffer_host_destroy_is_queued(ctx: &Context, host_id: u32) -> bool {
        buffer_lifecycle(ctx, host_id) == Some(RenderBufferLifecycle::HostDestroyQueued)
    }

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
        assert_eq!(
            handler.on_create_pool(&mut ctx, 20, shm_pool_file(&[]).as_fd(), 0),
            Action::Drop
        );
        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            1,
            "invalid pool size must use wl_shm.invalid_stride"
        );

        ctx.host_to_client_queue.clear();
        ctx.clear_protocol_error_for_test();
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
            handler.on_create_pool(&mut ctx, 20, fd.as_fd(), 4096),
            Action::Drop
        );
        ctx.host_to_client_queue.clear();
        ctx.clear_protocol_error_for_test();
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
        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            0,
            "unsupported format must use wl_shm.invalid_format"
        );
    }

    #[test]
    fn pool_resize_past_the_file_end_queues_invalid_fd_error() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let pool_id = 20;
        ctx.pools.insert(
            pool_id,
            ShmPool {
                file: Arc::new(shm_pool_file(&[0; 4096])),
                size: 4096,
            },
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
        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            wl_shm::ERROR_INVALID_FD,
            "a resize past the file's end must use wl_shm.invalid_fd"
        );
        assert_eq!(ctx.pools[&pool_id].size, 4096);
    }

    #[test]
    fn shm_pool_resize_only_allows_growth() {
        assert_eq!(valid_pool_resize(4096, 8192), Some(8192));
        assert_eq!(valid_pool_resize(4096, 4096), Some(4096));
        assert_eq!(valid_pool_resize(4096, 2048), None);
        assert_eq!(valid_pool_resize(4096, 0), None);
        assert_eq!(valid_pool_resize(4096, -1), None);
    }

    #[test]
    fn pool_resize_accepts_the_same_size_and_rejects_shrinking_as_invalid_fd() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let pool_id = 20;
        ctx.pools.insert(
            pool_id,
            ShmPool {
                file: Arc::new(shm_pool_file(&[0; 4096])),
                size: 4096,
            },
        );
        ctx.last_sender_id = pool_id;
        let mut handler = ShmHandler;
        let mut resize = |ctx: &mut Context, size| {
            <ShmHandler as crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler>::on_resize(
                &mut handler,
                ctx,
                size,
            )
        };

        // libwayland accepts a resize to the current size.
        assert_eq!(resize(&mut ctx, 4096), Action::Drop);
        assert!(!ctx.is_fatal());
        assert!(ctx.host_to_client_queue.is_empty());
        assert_eq!(ctx.pools[&pool_id].size, 4096);

        assert_eq!(resize(&mut ctx, 2048), Action::Drop);
        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            wl_shm::ERROR_INVALID_FD,
            "shrinking a pool is wl_shm.invalid_fd, as in libwayland"
        );
        assert_eq!(ctx.pools[&pool_id].size, 4096);
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
        assert!(valid_shm_stride(wl_shm::FORMAT_NV12, 10, 10));
        assert!(!valid_shm_stride(wl_shm::FORMAT_NV12, 11, 11));
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
            valid_buffer_layout(24, 0, 4, 4, 4, wl_shm::FORMAT_NV12),
            Some(24)
        );
        assert_eq!(
            valid_buffer_layout(23, 0, 4, 4, 4, wl_shm::FORMAT_NV12),
            None
        );
        assert_eq!(
            valid_buffer_layout(24, 0, 4, 3, 4, wl_shm::FORMAT_NV12),
            None,
        );
    }

    #[test]
    fn copies_both_nv12_planes_only_after_validating_all_spans() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let mut destination = [0u8; 24];

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                dest_size: destination.len(),
                format: wl_shm::FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                dst_plane1_offset: 0,
                dst_plane1_stride: 0,
                height: 4,
            },
            &DamageRegion::Full,
        )
        .is_ok());
        assert_eq!(&destination[..16], &[0x11; 16]);
        assert_eq!(&destination[16..], &[0x22; 8]);

        // The Y plane fits a 23-byte destination, but the UV plane does not.
        destination.fill(0);
        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                dest_size: 23,
                format: wl_shm::FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                dst_plane1_offset: 0,
                dst_plane1_stride: 0,
                height: 4,
            },
            &DamageRegion::Full,
        )
        .is_err());
        assert!(
            destination.iter().all(|byte| *byte == 0),
            "an invalid UV span must not leave the Y plane partially copied"
        );
    }

    #[test]
    fn copy_rejects_null_or_failed_destinations_before_pointer_arithmetic() {
        let source = shm_pool_file(&[0u8; 4]);
        let layout = super::ShmCopyLayout {
            dest_size: 4,
            format: 0,
            offset: 0,
            width: 1,
            src_stride: 4,
            dst_stride: 4,
            dst_plane1_offset: 0,
            dst_plane1_stride: 0,
            height: 1,
        };
        let damage = DamageRegion::from(vec![DamageRect::new(0, 0, 1, 1)]);
        for destination in [std::ptr::null_mut(), libc::MAP_FAILED.cast()] {
            assert!(matches!(
                copy_shm_damage(&source, destination, layout, &damage),
                Err(ShmCopyError::Destination)
            ));
        }
    }

    #[test]
    fn copy_from_a_truncated_pool_file_fails_instead_of_faulting() {
        // A client may shrink its pool file after creating the pool. Reading
        // a mapping past the new end raised SIGBUS and killed the proxy.
        let source = shm_pool_file(&[0x11; 8]);
        source.set_len(4).unwrap();
        let mut destination = [0u8; 8];
        let layout = super::ShmCopyLayout {
            dest_size: destination.len(),
            format: 0,
            offset: 0,
            width: 1,
            src_stride: 4,
            dst_stride: 4,
            dst_plane1_offset: 0,
            dst_plane1_stride: 0,
            height: 2,
        };
        assert!(matches!(
            copy_shm_damage(&source, destination.as_mut_ptr(), layout, &DamageRegion::Full),
            Err(ShmCopyError::Source(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn copy_into_an_unwritable_destination_blames_the_host_storage() {
        // The kernel reports a fault in the destination as EFAULT, which must
        // not become the client's invalid_fd.
        let source = shm_pool_file(&[0x11; 4]);
        let destination = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(destination, libc::MAP_FAILED);
        let layout = super::ShmCopyLayout {
            dest_size: 4,
            format: 0,
            offset: 0,
            width: 1,
            src_stride: 4,
            dst_stride: 4,
            dst_plane1_offset: 0,
            dst_plane1_stride: 0,
            height: 1,
        };
        let result = copy_shm_damage(&source, destination.cast(), layout, &DamageRegion::Full);
        unsafe { libc::munmap(destination, 4096) };
        assert!(matches!(result, Err(ShmCopyError::Destination)));
    }

    #[test]
    fn copies_nv12_plane_one_at_allocator_returned_offset() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let mut destination = [0u8; 32];

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                dest_size: destination.len(),
                format: wl_shm::FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                dst_plane1_offset: 20,
                dst_plane1_stride: 4,
                height: 4,
            },
            &DamageRegion::Full,
        )
        .is_ok());
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
            &shm_pool_file(&source),
            destination.as_mut_ptr(),
            super::ShmCopyLayout {
                dest_size: destination.len(),
                format: wl_shm::FORMAT_NV12,
                offset: 0,
                width: 4,
                src_stride: 4,
                dst_stride: 4,
                dst_plane1_offset: 20,
                dst_plane1_stride: 8,
                height: 4,
            },
            &DamageRegion::from(vec![DamageRect::new(0, 2, 4, 2)]),
        )
        .is_ok());

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
        assert_eq!(
            layout.plane0(),
            DmabufPlane {
                offset: 4096,
                stride: 512
            }
        );
        assert_eq!(layout.plane1(), None);
        assert_eq!(layout.plane1_relative_offset(), 0);
        assert_eq!(layout.plane1_stride(), 0);
        assert_eq!(layout.span(), 4096 + 512 * 32);

        let nv12 = validate_dmabuf_layout(
            wl_shm::FORMAT_NV12,
            64,
            32,
            [64, 64, 0],
            [0, 64 * 32 + 4096, 0],
        )
        .expect("NV12 metadata with a gap before plane 1");
        assert_eq!(
            nv12.plane1(),
            Some(DmabufPlane {
                offset: 64 * 32 + 4096,
                stride: 64
            })
        );
        assert_eq!(nv12.plane1_relative_offset(), 64 * 32 + 4096);
        assert!(
            validate_dmabuf_layout(wl_shm::FORMAT_NV12, 64, 32, [64, 64, 0], [4096, 0, 0],)
                .is_none()
        );
        assert!(
            validate_dmabuf_layout(
                wl_shm::FORMAT_NV12,
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
    fn rejects_unaligned_dmabuf_mappings() {
        // A layout's span cannot exceed isize::MAX on a 64-bit host (u32
        // offsets and strides, i32 rows), so only the alignment rule is
        // reachable here.
        let layout = validate_dmabuf_layout(0x3432_5258, 1, 1, [4, 0, 0], [1, 0, 0])
            .expect("a valid but unaligned single-plane layout");
        assert!(map_dmabuf(0, layout).is_none());
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
                validate_dmabuf_layout(0x3432_5258, 16, 128, [64, 0, 0], [0, 0, 0])
                    .expect("an 8192-byte single-plane layout")
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
        ctx.host_shm_formats.insert(wl_shm::FORMAT_NV12);
        assert!(
            !guest_shm_format_available(&ctx, wl_shm::FORMAT_NV12),
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
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 2);
        register_guest_shm(&mut ctx, guest_shm);
        ctx.host_to_client_queue.clear();
        ctx.stale_shm_guest_objects.insert(guest_shm);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        let mut release = WireMessage::new(
            guest_shm,
            crate::protocols::wayland::wl_shm::REQ_RELEASE,
            &[],
        );
        assert_eq!(
            raw_dispatch(crate::protocols::wayland::wl_shm::dispatch_request(
                &mut release,
                &mut handler,
                &mut ctx
            )),
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
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 2);
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
    fn pool_backing_must_be_a_long_enough_regular_file() {
        let file = shm_pool_file(&[]);
        assert!(regular_file_has_size(&file, 0));
        assert!(!regular_file_has_size(&file, 4096));
        file.set_len(4096).unwrap();
        assert!(regular_file_has_size(&file, 4096));

        // `pread` cannot read a pool from a pipe or a device.
        let (pipe_read, _pipe_write) = nix::unistd::pipe().expect("pipe");
        assert!(!regular_file_has_size(&File::from(pipe_read), 0));
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

    /// Without VirtWL the host is a local compositor (`--local-compositor`),
    /// which maps a memfd as the buffer's wl_shm pool.
    #[test]
    fn without_virtwl_a_buffer_is_backed_by_a_memfd_of_its_size() {
        use crate::protocols::wayland::{wl_shm, wl_shm_pool};
        const HOST_SHM: u32 = 2;
        let (pool_id, buffer_id) = (20, 30);
        let (width, height, stride) = (8, 4, 32);

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_shm_id = Some(HOST_SHM);
        ctx.shadow_table
            .insert_host_only_for_test(HOST_SHM, "wl_shm", 1);
        ctx.pools.insert(
            pool_id,
            ShmPool {
                file: Arc::new(shm_pool_file(&[0; 4096])),
                size: 4096,
            },
        );
        ctx.shadow_table.create_local(pool_id, "wl_shm_pool", 1);
        ctx.last_sender_id = pool_id;

        assert_eq!(
            <ShmHandler as crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler>::on_create_buffer(
                &mut ShmHandler,
                &mut ctx,
                buffer_id,
                0,
                width,
                height,
                stride,
                0,
            ),
            Action::Drop
        );

        assert!(!ctx.is_fatal());
        assert!(ctx.shadow_table.get_host_id(buffer_id).is_some());
        let host_pool = msg_sender(&ctx.client_to_host_queue[1].0);
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [
                (HOST_SHM, wl_shm::REQ_CREATE_POOL),
                (host_pool, wl_shm_pool::REQ_CREATE_BUFFER),
                (host_pool, wl_shm_pool::REQ_DESTROY),
            ]
        );
        let pool_fd = ctx.client_to_host_queue[0].1[0].as_fd();
        let pool_size = nix::sys::stat::fstat(pool_fd)
            .expect("fstat the pool")
            .st_size;
        assert_eq!(pool_size, i64::from(stride * height));
    }

    /// Host storage that cannot be set up (here: the host has no wl_shm to
    /// import the backing into) fails the buffer, which is fatal and leaves
    /// no trace of the buffer behind.
    #[test]
    fn failed_host_storage_is_fatal_without_creating_a_ghost_buffer() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let fd = memfd_create(
            CString::new("sommelier-allocation-failure")
                .unwrap()
                .as_c_str(),
            MFdFlags::empty(),
        )
        .expect("memfd_create");
        nix::unistd::ftruncate(&fd, 4096).expect("ftruncate");

        let mut ctx = Context::new_for_test(false, false, vec![]);
        let pool_id = 20;
        let buffer_id = 30;
        ctx.pools.insert(
            pool_id,
            ShmPool {
                file: Arc::new(File::from(fd)),
                size: 4096,
            },
        );
        ctx.shadow_table.create_local(pool_id, "wl_shm_pool", 1);
        ctx.last_sender_id = pool_id;

        let mut handler = ShmHandler;
        assert_eq!(
            <ShmHandler as crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler>::on_create_buffer(
                &mut handler,
                &mut ctx,
                buffer_id,
                0,
                1,
                1,
                4,
                0,
            ),
            Action::Drop
        );

        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            wl_shm::ERROR_INVALID_FD,
            "a buffer whose storage fails reports wl_shm.invalid_fd"
        );
        assert_eq!(ctx.shadow_table.get_host_id(buffer_id), None);
        assert_eq!(ctx.shadow_table.get_interface(buffer_id), None);
        assert!(ctx.shadow_table.is_guest_id_available(buffer_id));
        assert_eq!(ctx.render_buffer_count(), 0);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn failed_registration_retires_host_buffer_without_guest_mapping() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_id = 30;
        let host_id = 40;
        ctx.shadow_table
            .insert_paired_with_version_for_test(guest_id, host_id, "wl_buffer", 1);

        rollback_registered_buffer(&mut ctx, guest_id, host_id);

        assert_eq!(ctx.shadow_table.get_host_id(guest_id), None);
        assert_eq!(ctx.shadow_table.get_interface(guest_id), None);
        assert!(ctx.shadow_table.is_guest_id_available(guest_id));
        assert_eq!(ctx.shadow_table.get_host_interface(host_id), None);
        assert!(ctx.shadow_table.is_retired_host(host_id));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(
            crate::test_support::msg_opcode(&ctx.client_to_host_queue[0].0),
            0,
            "rollback must queue wl_buffer.destroy"
        );
    }

    #[test]
    fn removed_host_shm_rejects_pool_creation_from_existing_synthetic_object() {
        use nix::sys::memfd::{memfd_create, MFdFlags};
        use std::ffi::CString;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 1);
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
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
            handler.on_create_pool(&mut ctx, 30, fd.as_fd(), 4096),
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
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
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
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
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
            WlShmHandler::on_create_pool(&mut handler, &mut ctx, 30, fd.as_fd(), 4096),
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
            ctx.pools.get(&30).map(|pool| pool.size),
            Some(4096),
            "a stale pool must not resize"
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
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        let mut surface_state = crate::state::SurfaceState::default();
        surface_state.set_current_buffer_for_test(Some(guest_buffer), Some((1, 1)));
        ctx.surfaces.insert(surface, surface_state);
        register_test_local(
            &mut ctx,
            guest_buffer,
            host_buffer,
            BufferState {
                pool_file: Arc::new(shm_pool_file(&[])),
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                stale: StalePixels::all(),
            },
        );
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(
            buffer_is_guest_destroyed(&ctx, host_buffer),
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
        assert!(buffer_host_destroy_is_queued(&ctx, host_buffer));
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer)
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert_eq!(
            ctx.surfaces
                .get(&surface)
                .and_then(crate::state::SurfaceState::current_buffer_id),
            None
        );
        assert_eq!(
            ctx.surfaces
                .get(&surface)
                .and_then(crate::state::SurfaceState::current_buffer_dimensions),
            Some((1, 1)),
            "destroying a released wl_buffer must preserve committed content metadata"
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
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            host_buffer,
            BufferState {
                pool_file: Arc::new(shm_pool_file(&[])),
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                stale: StalePixels::all(),
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
        assert!(buffer_host_destroy_is_queued(&ctx, host_buffer));
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
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(
            buffer_is_guest_destroyed(&ctx, host_buffer),
            "a submitted native buffer must stay alive until host release"
        );
        assert!(ctx.buffer_is_submitted(guest_buffer));
        assert_eq!(ctx.client_to_host_queue.len(), 0);

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert!(buffer_host_destroy_is_queued(&ctx, host_buffer));
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
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_native(&mut ctx, host_buffer);
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
        ctx.surface_mut(100)
            .set_current_buffer_for_test(Some(guest_buffer), None);
        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(buffer_host_destroy_is_queued(&ctx, host_buffer));
        assert!(ctx.host_buffer_use(guest_buffer).is_none());
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
    }

    #[test]
    fn destroying_idle_server_created_buffer_ends_without_delete_id() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());

        let reused_guest = register_test_server_native(&mut ctx, host_buffer);
        assert_eq!(
            reused_guest, guest_buffer,
            "a destroyed server-created buffer's guest ID is reused first"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(reused_guest),
            Some(host_buffer),
            "a later ordered host event may reuse the server-owned buffer ID"
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "server-created guest IDs never receive wl_display.delete_id"
        );
    }

    #[test]
    fn destroying_submitted_server_created_buffer_retires_after_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer),
            "the mapping must remain until a delayed host release can resolve"
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert!(buffer_is_guest_destroyed(&ctx, host_buffer));

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn released_server_created_buffer_retires_on_later_guest_destroy() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(ctx.buffer_is_released(guest_buffer));

        ctx.last_sender_id = guest_buffer;
        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn local_copy_and_native_buffers_share_one_retirement_oracle() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let local_buffer = 20;
        let local_host = 30;
        let native_buffer = 21;
        let native_host = 31;

        for (guest_id, host_id) in [(local_buffer, local_host), (native_buffer, native_host)] {
            ctx.shadow_table
                .insert_paired_with_version_for_test(guest_id, host_id, "wl_buffer", 1);
        }
        register_test_local(
            &mut ctx,
            local_buffer,
            local_host,
            BufferState {
                pool_file: Arc::new(shm_pool_file(&[])),
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                stale: StalePixels::all(),
            },
        );
        register_test_native(&mut ctx, native_host);
        for guest_id in [local_buffer, native_buffer] {
            mark_test_buffer_guest_destroyed(&mut ctx, guest_id);
            assert!(ctx.mark_buffer_submitted(guest_id));
            assert!(ctx.mark_buffer_released(guest_id));
        }
        ctx.surface_mut(40).pending.attachment =
            crate::state::SurfaceAttachment::Attach(local_buffer);
        ctx.surface_mut(41).pending.attachment =
            crate::state::SurfaceAttachment::Attach(native_buffer);

        retire_eligible_buffers(&mut ctx);
        assert!(buffer_is_guest_destroyed(&ctx, local_host));
        assert!(buffer_is_guest_destroyed(&ctx, native_host));

        ctx.surfaces.get_mut(&40).unwrap().pending.attachment =
            crate::state::SurfaceAttachment::Detach;
        ctx.surfaces.get_mut(&41).unwrap().pending.attachment =
            crate::state::SurfaceAttachment::Detach;
        retire_eligible_buffers(&mut ctx);

        assert!(buffer_host_destroy_is_queued(&ctx, local_host));
        assert!(buffer_host_destroy_is_queued(&ctx, native_host));
        assert!(ctx.host_buffer_use(local_buffer).is_none());
        assert!(ctx.host_buffer_use(native_buffer).is_none());
        let mut destroyed_hosts = ctx
            .client_to_host_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap()))
            .collect::<Vec<_>>();
        destroyed_hosts.sort_unstable();
        assert_eq!(
            destroyed_hosts,
            vec![local_host, native_host],
            "local-copy and native buffers must make the same lifecycle decision"
        );
    }

    #[test]
    fn retired_collection_waits_for_host_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_buffer,
            host_buffer,
            "wl_buffer",
            1,
        );
        register_test_local(
            &mut ctx,
            guest_buffer,
            host_buffer,
            BufferState {
                pool_file: Arc::new(shm_pool_file(&[])),
                offset: 0,
                width: 1,
                height: 1,
                stride: 4,
                format: 0,
                bo: None,
                dmabuf_fd: None,
                bo_stride: 4,
                dmabuf_plane1_offset: 0,
                dmabuf_plane1_stride: 0,
                dmabuf_sync: false,
                dest_ptr: std::ptr::null_mut(),
                dest_size: 0,
                stale: StalePixels::all(),
            },
        );
        ctx.mark_buffer_submitted(guest_buffer);
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);

        retire_eligible_buffers(&mut ctx);
        assert!(buffer_is_guest_destroyed(&ctx, host_buffer));

        assert!(ctx.mark_buffer_released(guest_buffer));
        retire_eligible_buffers(&mut ctx);
        assert!(buffer_host_destroy_is_queued(&ctx, host_buffer));
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
