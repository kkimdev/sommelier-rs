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

//! Copying the damaged pixels of a guest SHM buffer from its pool, with
//! `pread`, into the buffer's host storage.

use super::damage::{clipped_damage, coalesce_damage_rects};
use super::formats::format_bytes_per_pixel;
use crate::protocols::wayland::wl_shm;
use crate::state::{BufferRect, BufferSpace, BufferState, DamageRegion};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

#[derive(Clone, Copy)]
struct PlaneCopy {
    src_offset: usize,
    dst_offset: usize,
    src_stride: usize,
    dst_stride: usize,
    rows: usize,
    row_bytes: usize,
}

/// Layout metadata for copying one guest SHM buffer.
///
/// Keeping the source file and the destination separate from this value
/// makes the copy entry point difficult to call with mismatched dimensions
/// while avoiding a long list of loosely-related scalar arguments. The
/// destination's length bounds the copy.
#[derive(Clone, Copy)]
pub(crate) struct ShmCopyLayout {
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
            format: buffer.format,
            offset: usize::try_from(buffer.offset).ok()?,
            width: usize::try_from(buffer.width).ok()?,
            src_stride: buffer.stride as usize,
            dst_stride: buffer.dest_stride as usize,
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

/// Copy only the damaged rectangles of a SHM image from `source` into
/// `destination`, the host-side linear buffer.
///
/// ChromiumOS treats NV12 as two planes sharing one FD: the UV plane starts
/// immediately after the full-resolution Y plane and has half as many rows.
/// Validate every plane and every rectangle against `destination` before
/// touching either destination plane so malformed client metadata cannot
/// leave a partially updated buffer.
pub(crate) fn copy_shm_damage(
    source: &File,
    destination: &mut [u8],
    layout: ShmCopyLayout,
    damage: &DamageRegion<BufferSpace>,
) -> Result<(), ShmCopyError> {
    let dest_size = destination.len();
    let ShmCopyLayout {
        format,
        offset,
        width,
        src_stride,
        dst_stride,
        dst_plane1_offset,
        dst_plane1_stride,
        height,
    } = layout;
    if width == 0 || height == 0 || format_bytes_per_pixel(format).is_none() {
        return Err(ShmCopyError::Destination);
    }

    let bytes_per_pixel = format_bytes_per_pixel(format).unwrap_or(0);
    let plane_count = if format == wl_shm::FORMAT_NV12 { 2 } else { 1 };
    let merged_damage = match damage.rects() {
        None => vec![BufferRect::new(
            0,
            0,
            i32::try_from(width).unwrap_or(i32::MAX),
            i32::try_from(height).unwrap_or(i32::MAX),
        )],
        Some(rects) => coalesce_damage_rects(rects),
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
            // valid_plane_copy checked that every row of the span, and so
            // this chunk, lies inside the destination.
            let target = &mut destination[dst_offset..dst_offset + chunk_bytes];
            // The read fails, instead of raising SIGBUS like a mapping
            // would, if the client truncated the pool file. EFAULT is the
            // destination mapping's fault, not the client's. (A FUSE-backed
            // pool could also return EFAULT; it is then treated as a host
            // storage failure, which only drops the commit.)
            source
                .read_exact_at(target, src_offset as u64)
                .map_err(|error| match error.raw_os_error() {
                    Some(libc::EFAULT) => ShmCopyError::Destination,
                    _ => ShmCopyError::Source(error),
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{copy_shm_damage, ShmCopyError, ShmCopyLayout};
    use crate::protocols::wayland::wl_shm;
    use crate::state::{BufferRect, DamageRegion};
    use crate::test_support::shm_pool_file;

    /// A 4x4 NV12 frame, which fills a packed destination of 24 bytes.
    const NV12_4X4: ShmCopyLayout = ShmCopyLayout {
        format: wl_shm::FORMAT_NV12,
        offset: 0,
        width: 4,
        src_stride: 4,
        dst_stride: 4,
        dst_plane1_offset: 0,
        dst_plane1_stride: 0,
        height: 4,
    };

    /// One ARGB8888 pixel, which fills a destination of 4 bytes.
    const ARGB8888_1X1: ShmCopyLayout = ShmCopyLayout {
        format: 0,
        width: 1,
        height: 1,
        ..NV12_4X4
    };

    #[test]
    fn copies_both_nv12_planes_only_after_validating_all_spans() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let mut destination = [0u8; 24];

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &mut destination,
            NV12_4X4,
            &DamageRegion::Full,
        )
        .is_ok());
        assert_eq!(&destination[..16], &[0x11; 16]);
        assert_eq!(&destination[16..], &[0x22; 8]);

        // The Y plane fits a 23-byte destination, but the UV plane does not.
        destination.fill(0);
        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &mut destination[..23],
            NV12_4X4,
            &DamageRegion::Full,
        )
        .is_err());
        assert!(
            destination.iter().all(|byte| *byte == 0),
            "an invalid UV span must not leave the Y plane partially copied"
        );
    }

    /// A buffer whose last row ends past its destination is refused
    /// before anything is copied, whatever the damage: the copy never
    /// writes outside the destination, nor leaves it partly updated.
    #[test]
    fn copy_rejects_a_buffer_that_overruns_its_destination() {
        // Two rows of two ARGB8888 pixels, 8 bytes apart, need 16 bytes.
        let source = shm_pool_file(&[0x11; 16]);
        let layout = ShmCopyLayout {
            width: 2,
            height: 2,
            src_stride: 8,
            dst_stride: 8,
            ..ARGB8888_1X1
        };
        let mut destination = [0u8; 16];
        let last_row = DamageRegion::from(vec![BufferRect::new(0, 1, 2, 1)]);
        for damage in [&DamageRegion::Full, &last_row] {
            destination.fill(0);
            assert!(matches!(
                copy_shm_damage(&source, &mut destination[..15], layout, damage),
                Err(ShmCopyError::Destination)
            ));
            assert_eq!(destination, [0; 16], "nothing is copied");
        }
        copy_shm_damage(&source, &mut destination, layout, &DamageRegion::Full).unwrap();
        assert_eq!(destination, [0x11; 16]);
    }

    #[test]
    fn copy_from_a_truncated_pool_file_fails_instead_of_faulting() {
        // A client may shrink its pool file after creating the pool. Reading
        // a mapping past the new end raised SIGBUS and killed the proxy.
        let source = shm_pool_file(&[0x11; 8]);
        source.set_len(4).unwrap();
        let mut destination = [0u8; 8];
        let layout = ShmCopyLayout {
            height: 2,
            ..ARGB8888_1X1
        };
        assert!(matches!(
            copy_shm_damage(&source, &mut destination, layout, &DamageRegion::Full),
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
        // SAFETY: the mapping is 4096 bytes long, lives until the munmap
        // below, and nothing else references it. It is read-only, so the
        // kernel's write into it fails with EFAULT.
        let slice = unsafe { std::slice::from_raw_parts_mut(destination.cast::<u8>(), 4) };
        let result = copy_shm_damage(&source, slice, ARGB8888_1X1, &DamageRegion::Full);
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
            &mut destination,
            ShmCopyLayout {
                dst_plane1_offset: 20,
                dst_plane1_stride: 4,
                ..NV12_4X4
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
            &mut destination,
            ShmCopyLayout {
                dst_plane1_offset: 20,
                dst_plane1_stride: 8,
                ..NV12_4X4
            },
            &DamageRegion::from(vec![BufferRect::new(0, 2, 4, 2)]),
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
}
