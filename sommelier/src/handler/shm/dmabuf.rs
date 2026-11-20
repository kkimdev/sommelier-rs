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

//! VirtWL dma-bufs as the host storage of guest SHM buffers: allocating
//! one, and mapping it for the copy.

use super::dmabuf_layout::{validate_dmabuf_layout, DmabufLayout};
use super::formats::wl_shm_format_to_drm_format;
use crate::state::{Context, HostMapping};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

/// A VirtWL dma-buf allocation for a guest SHM buffer.
///
/// VirtWL allocates a single object, so both planes of an NV12 buffer are in
/// the one dma-buf `fd`, which `host_copy` maps for both.
pub(super) struct DmabufAllocation {
    pub(super) fd: OwnedFd,
    /// The mapping of `fd` that the copy writes, made once, when the
    /// allocation proves it can be mapped.
    pub(super) host_copy: HostMapping,
    pub(super) layout: DmabufLayout,
    pub(super) modifier: u64,
    pub(super) dmabuf_sync: bool,
}

pub(super) fn map_dmabuf(fd: BorrowedFd<'_>, layout: DmabufLayout) -> Option<HostMapping> {
    let offset0 = layout.plane0().offset as usize;
    let span = layout.span();
    if span <= offset0 {
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
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) } != 0 {
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
    HostMapping::map_shared(fd, span - offset0, u64::try_from(offset0).ok()?).ok()
}

pub(super) fn allocate_virtwl_dmabuf(
    ctx: &Context,
    width: i32,
    height: i32,
    format: u32,
) -> io::Result<DmabufAllocation> {
    let Some(channel) = ctx
        .virtwayland_channel()
        .filter(|channel| channel.supports_dmabuf())
    else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "VirtWL dma-buf allocation is unavailable",
        ));
    };
    let drm_format = wl_shm_format_to_drm_format(format);
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
    // Map it for the copy path while the allocation is still owned by this
    // function. A VirtWL driver may expose the ioctl but return metadata
    // that cannot be mmaped (for example a non-page-aligned plane-0
    // offset); treat that as an allocation failure so the caller can fall
    // back to ordinary VirtWL shared memory.
    let Some(host_copy) = map_dmabuf(fd.as_fd(), layout) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VirtWL dma-buf cannot be mapped",
        ));
    };
    Ok(DmabufAllocation {
        fd,
        host_copy,
        layout,
        modifier: 0,
        dmabuf_sync: true,
    })
}

#[cfg(test)]
mod tests {
    use super::{map_dmabuf, validate_dmabuf_layout};
    use crate::test_support::shm_pool_file;
    use std::os::fd::AsFd;

    #[test]
    fn rejects_unaligned_dmabuf_mappings() {
        // A layout's span cannot exceed isize::MAX on a 64-bit host (u32
        // offsets and strides, i32 rows), so only the alignment rule is
        // reachable here.
        let layout = validate_dmabuf_layout(0x3432_5258, 1, 1, [4, 0, 0], [1, 0, 0])
            .expect("a valid but unaligned single-plane layout");
        assert!(map_dmabuf(shm_pool_file(&[0; 4096]).as_fd(), layout).is_none());
    }

    #[test]
    fn rejects_regular_dmabuf_backing_shorter_than_metadata() {
        let fd = shm_pool_file(&[0; 4096]);
        assert!(
            map_dmabuf(
                fd.as_fd(),
                validate_dmabuf_layout(0x3432_5258, 16, 128, [64, 0, 0], [0, 0, 0])
                    .expect("an 8192-byte single-plane layout")
            )
            .is_none(),
            "mmap preflight must reject a regular backing that would SIGBUS"
        );
    }
}
