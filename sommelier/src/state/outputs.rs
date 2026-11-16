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

//! Host output metadata and the `wl_output.transform` rules.
//!
//! [`OutputState`] is one output's geometry as Exo reports it, and
//! [`OutputState::work_area`] turns it into logical screen coordinates for
//! compositor-owned window placement. [`is_valid_transform`] and
//! [`transformed_extent`] are the only statements of the `wl_output.transform`
//! enum rules; surfaces' buffer transforms use them too.

/// `wl_output.transform.normal`, the lowest enum value.
const TRANSFORM_NORMAL: i32 = 0;
/// `wl_output.transform.flipped_270`, the highest enum value.
const TRANSFORM_FLIPPED_270: i32 = 7;

/// Whether `transform` is a `wl_output.transform` enum value.
pub(crate) fn is_valid_transform(transform: i32) -> bool {
    (TRANSFORM_NORMAL..=TRANSFORM_FLIPPED_270).contains(&transform)
}

/// Width and height of a `width` x `height` extent after `transform`.
///
/// The odd enum values are the 90 and 270 degree rotations, flipped or not,
/// and swap the axes; the even values keep them.
pub(crate) fn transformed_extent(transform: i32, width: i32, height: i32) -> (i32, i32) {
    if transform & 1 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

/// Host output geometry used for compositor-owned window layout requests.
///
/// `wl_output.mode` reports pixel dimensions while Aura window bounds use
/// logical screen coordinates. Aura's device and current output scales
/// convert the former into the latter; `wl_output.scale` rounds fractional
/// scales upward and cannot describe those coordinates. Output insets remove
/// shelf/non-work-area margins when they are known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputState {
    /// Logical screen-space origin from `wl_output.geometry`.
    pub origin_x: i32,
    pub origin_y: i32,
    pub mode_width: i32,
    pub mode_height: i32,
    /// Current `zaura_output.scale`, in thousandths. Preferred-only scale
    /// entries do not affect the current output geometry.
    pub aura_current_scale: Option<u32>,
    /// `zaura_output.device_scale_factor`, in thousandths. Both Aura factors
    /// must arrive before this output can authorize placement.
    pub aura_device_scale_factor: Option<u32>,
    /// Panel transform from `wl_output.geometry`, the rotation that maps the
    /// native-orientation `wl_output.mode` onto the logical screen.
    ///
    /// Exo sends `display.panel_rotation()` here: the active rotation plus
    /// the panel's mounting orientation. `zaura_output.logical_transform` is
    /// the active rotation alone, so it would leave a portrait-native panel
    /// in the wrong orientation; it is not used.
    pub transform: i32,
    pub insets_top: i32,
    pub insets_left: i32,
    pub insets_bottom: i32,
    pub insets_right: i32,
}

impl OutputState {
    /// The output's work area, `(x, y, width, height)` in logical screen
    /// coordinates.
    ///
    /// The native pixel mode is rotated by the panel transform, divided by
    /// both Aura scale factors (floored), and shrunk by the logical insets.
    ///
    /// # Returns
    ///
    /// `None` until both Aura factors are known, and for an empty or
    /// overflowing area.
    pub fn work_area(self) -> Option<(i32, i32, i32, i32)> {
        let scale = u64::from(self.aura_current_scale?)
            .checked_mul(u64::from(self.aura_device_scale_factor?))?;
        let (mode_width, mode_height) =
            transformed_extent(self.transform, self.mode_width, self.mode_height);
        // Legacy Aura factors are quantized to thousandths, so retain their
        // precision until the final division and conservatively floor DP
        // extents. Checked conversion rejects malformed or overflowing modes.
        let logical_extent = |pixels: i32| {
            let extent = u64::try_from(pixels)
                .ok()?
                .checked_mul(1_000_000)?
                .checked_div(scale)?;
            i32::try_from(extent).ok()
        };
        let width = logical_extent(mode_width)?;
        let height = logical_extent(mode_height)?;
        let x = self.origin_x.checked_add(self.insets_left)?;
        let y = self.origin_y.checked_add(self.insets_top)?;
        let width = width
            .checked_sub(self.insets_left)?
            .checked_sub(self.insets_right)?;
        let height = height
            .checked_sub(self.insets_top)?
            .checked_sub(self.insets_bottom)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((x, y, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_rules_follow_the_wl_output_enum() {
        for transform in 0..=7 {
            assert!(is_valid_transform(transform));
            let swapped = matches!(transform, 1 | 3 | 5 | 7);
            assert_eq!(
                transformed_extent(transform, 4, 3),
                if swapped { (3, 4) } else { (4, 3) },
                "transform {transform}"
            );
        }
        assert!(!is_valid_transform(-1));
        assert!(!is_valid_transform(8));
    }

    #[test]
    fn output_work_area_converts_scale_and_insets() {
        let output = OutputState {
            origin_x: 1920,
            origin_y: 16,
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(2000),
            insets_top: 24,
            insets_left: 8,
            insets_bottom: 48,
            insets_right: 16,
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some((1928, 40, 1896, 1008)));
    }

    #[test]
    fn output_work_area_combines_aura_scales_before_rounding() {
        let output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1250),
            aura_device_scale_factor: Some(2000),
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some((0, 0, 1536, 864)));
    }

    #[test]
    fn fractional_output_work_area_rotates_before_applying_dp_insets() {
        let output = OutputState {
            origin_x: 1920,
            origin_y: 16,
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1250),
            transform: 1,
            insets_top: 24,
            insets_left: 8,
            insets_bottom: 48,
            insets_right: 16,
        };
        assert_eq!(output.work_area(), Some((1928, 40, 1704, 3000)));
    }

    #[test]
    fn output_work_area_requires_valid_aura_factors() {
        let mut output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            ..Default::default()
        };
        for (current, device) in [
            (None, None),
            (Some(1000), None),
            (None, Some(1250)),
            (Some(0), Some(1250)),
            (Some(1000), Some(0)),
            (Some(u32::MAX), Some(u32::MAX)),
        ] {
            output.aura_current_scale = current;
            output.aura_device_scale_factor = device;
            assert_eq!(output.work_area(), None, "factors: {current:?}, {device:?}");
        }
        output.mode_width = i32::MAX;
        output.aura_current_scale = Some(1);
        output.aura_device_scale_factor = Some(1);
        assert_eq!(output.work_area(), None, "DP dimensions must fit i32");
    }

    #[test]
    fn output_work_area_rejects_invalid_dimensions() {
        let output = OutputState {
            mode_width: 100,
            mode_height: 100,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(2000),
            insets_top: 60,
            ..Default::default()
        };
        assert_eq!(output.work_area(), None);
    }

    #[test]
    fn output_work_area_swaps_rotated_dimensions() {
        let output = OutputState {
            mode_width: 1920,
            mode_height: 1080,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            transform: 1,
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some((0, 0, 1080, 1920)));
    }
}
