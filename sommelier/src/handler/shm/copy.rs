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
//! `pread` and `preadv`, into the buffer's host storage.

use super::damage::{clipped_damage, coalesce_damage_rects};
use super::formats::format_bytes_per_pixel;
use crate::protocols::wayland::wl_shm;
use crate::state::{BufferRect, BufferSpace, BufferState, DamageRegion, HostMapping, Rows};
use std::fs::File;
use std::io;

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
    destination: &HostMapping,
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
        // The read fails, instead of raising SIGBUS like a mapping would,
        // if the client truncated the pool file. EFAULT is the destination
        // mapping's fault, not the client's; it is also how the mapping
        // refuses a range outside it, which valid_plane_copy already
        // excluded. (A FUSE-backed pool could also return EFAULT; it is
        // then treated as a host storage failure, which only drops the
        // commit.) Only the span's own bytes reach the destination, however
        // few reads copy them.
        let rows = Rows {
            src_offset: plane.src_offset as u64,
            src_stride: plane.src_stride,
            dst_start: plane.dst_offset,
            dst_stride: plane.dst_stride,
            row_bytes: plane.row_bytes,
            rows: plane.rows,
        };
        destination
            .pread_rows_into(rows, source)
            .map_err(|error| match error.raw_os_error() {
                Some(libc::EFAULT) => ShmCopyError::Destination,
                _ => ShmCopyError::Source(error),
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{coalesce_damage_rects, copy_shm_damage, ShmCopyError, ShmCopyLayout};
    use crate::protocols::wayland::wl_shm;
    use crate::state::{BufferRect, DamageRegion, HostMapping};
    use crate::test_support::{host_mapping, shm_pool_file};
    use std::os::fd::AsFd;

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
        let destination = host_mapping(&[0; 24]);

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &destination,
            NV12_4X4,
            &DamageRegion::Full,
        )
        .is_ok());
        let copied = destination.to_vec();
        assert_eq!(&copied[..16], &[0x11; 16]);
        assert_eq!(&copied[16..], &[0x22; 8]);

        // The Y plane fits a 23-byte destination, but the UV plane does not.
        let destination = host_mapping(&[0; 23]);
        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &destination,
            NV12_4X4,
            &DamageRegion::Full,
        )
        .is_err());
        assert_eq!(
            destination.to_vec(),
            [0; 23],
            "an invalid UV span must not leave the Y plane partially copied"
        );
    }

    /// A buffer whose last row ends past its destination is refused
    /// before anything is copied, whatever the damage: the copy never
    /// writes outside the destination, nor leaves it partly updated.
    #[test]
    fn copy_rejects_a_buffer_that_overruns_its_destination() {
        // Three rows of four ARGB8888 pixels, 16 bytes apart, need 48 bytes.
        let source = shm_pool_file(&[0x11; 48]);
        let layout = ShmCopyLayout {
            width: 4,
            height: 3,
            src_stride: 16,
            dst_stride: 16,
            ..ARGB8888_1X1
        };
        let last_row = DamageRegion::from(vec![BufferRect::new(0, 2, 4, 1)]);
        // Two spans that stay apart, in this order: the first pixel of
        // row 0, which fits, and the last pixel of row 2, which does not.
        let spans = vec![BufferRect::new(0, 0, 1, 1), BufferRect::new(3, 2, 1, 1)];
        assert_eq!(coalesce_damage_rects(&spans), spans, "kept apart, in order");
        let fitting_span_then_overrun = DamageRegion::from(spans);
        for damage in [&DamageRegion::Full, &last_row, &fitting_span_then_overrun] {
            let destination = host_mapping(&[0; 47]);
            assert!(matches!(
                copy_shm_damage(&source, &destination, layout, damage),
                Err(ShmCopyError::Destination)
            ));
            assert_eq!(destination.to_vec(), [0; 47], "nothing is copied");
        }
        let destination = host_mapping(&[0; 48]);
        copy_shm_damage(&source, &destination, layout, &DamageRegion::Full).unwrap();
        assert_eq!(destination.to_vec(), [0x11; 48]);
    }

    #[test]
    fn copy_from_a_truncated_pool_file_fails_instead_of_faulting() {
        // A client may shrink its pool file after creating the pool. Reading
        // a mapping past the new end raised SIGBUS and killed the proxy.
        let source = shm_pool_file(&[0x11; 8]);
        source.set_len(4).unwrap();
        let layout = ShmCopyLayout {
            height: 2,
            ..ARGB8888_1X1
        };
        assert!(matches!(
            copy_shm_damage(&source, &host_mapping(&[0; 8]), layout, &DamageRegion::Full),
            Err(ShmCopyError::Source(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn copy_into_an_unwritable_destination_blames_the_host_storage() {
        // The kernel reports a fault in the destination as EFAULT, which must
        // not become the client's invalid_fd.
        let source = shm_pool_file(&[0x11; 4]);
        let destination = HostMapping::map_read_only(shm_pool_file(&[0; 4]).as_fd(), 4).unwrap();
        assert!(matches!(
            copy_shm_damage(&source, &destination, ARGB8888_1X1, &DamageRegion::Full),
            Err(ShmCopyError::Destination)
        ));
    }

    #[test]
    fn copies_nv12_plane_one_at_allocator_returned_offset() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..].fill(0x22);
        let destination = host_mapping(&[0; 32]);

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &destination,
            ShmCopyLayout {
                dst_plane1_offset: 20,
                dst_plane1_stride: 4,
                ..NV12_4X4
            },
            &DamageRegion::Full,
        )
        .is_ok());
        let copied = destination.to_vec();
        assert_eq!(&copied[..16], &[0x11; 16]);
        assert_eq!(&copied[16..20], &[0; 4]);
        assert_eq!(&copied[20..28], &[0x22; 8]);
        assert_eq!(&copied[28..], &[0; 4]);
    }

    #[test]
    fn copies_partial_nv12_damage_using_plane_one_stride() {
        let mut source = [0u8; 24];
        source[..16].fill(0x11);
        source[16..20].fill(0x21);
        source[20..24].fill(0x22);
        let destination = host_mapping(&[0xcc; 36]);

        assert!(copy_shm_damage(
            &shm_pool_file(&source),
            &destination,
            ShmCopyLayout {
                dst_plane1_offset: 20,
                dst_plane1_stride: 8,
                ..NV12_4X4
            },
            &DamageRegion::from(vec![BufferRect::new(0, 2, 4, 2)]),
        )
        .is_ok());
        let destination = destination.to_vec();

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
