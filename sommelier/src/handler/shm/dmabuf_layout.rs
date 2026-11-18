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

//! Validated dma-buf layouts. A module of its own so that nothing else in
//! `shm` can build a [`DmabufLayout`] except [`validate_dmabuf_layout`].

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
    // VirtWL uses one contiguous dma-buf for NV12. Plane 1
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

#[cfg(test)]
mod tests {
    use super::{validate_dmabuf_layout, DmabufPlane};
    use crate::protocols::wayland::wl_shm;

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
}
