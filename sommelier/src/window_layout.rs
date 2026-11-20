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

//! Work-area geometry for compositor-owned window placement.
//!
//! This module owns *where* a placement shortcut puts a window:
//!
//! * [`NormalizedRect`] - `[x, y, width, height]` as fractions of the output
//!   work area: what a placement binding places a window at, whether the
//!   binding names a preset ([`NormalizedRect::preset`], the one list of
//!   the nine named presets) or a rectangle. Every placement goes through
//!   [`NormalizedRect::to_work_area_bounds`], so there is exactly one
//!   rounding rule.
//!
//! It maps a [`NormalizedRect`] into a work area, a [`Rect`] in logical
//! screen coordinates (`crate::geometry`, which the output registry
//! returns work areas in too), and depends on nothing else in the crate.
//! The configuration parser (`placement_config`) builds the rectangles,
//! and the placement handler (`handler::window_placement`) maps one into
//! the other.
//!
//! # Rules
//!
//! The rules this module implements are specified once, in
//! `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`: the edge band in
//! "Validation", the precision a rect needs in "Precision", and the pixel
//! mapping in "From rectangle to window bounds". The constants and the unit
//! tests below are the code side of that contract.

use thiserror::Error;

use crate::geometry::Rect;

/// Distance within which an edge fraction is a work-area border (the edge
/// band; see the module rules).
const EDGE_TOLERANCE: f64 = 1e-9;

/// Sub-pixel bias added before flooring an edge to a pixel offset.
///
/// The worst-case product error for an `i32` extent is below this bias, so
/// the bias corrects representation error. It moves an edge by at most the
/// bias: only an edge less than `1e-6` px below a pixel boundary lands on
/// the next pixel.
const PIXEL_BIAS: f64 = 1e-6;

/// Reasons a rectangle is not a valid normalized work-area rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum RectError {
    /// A component is NaN or infinite.
    #[error("rect components must be finite numbers")]
    NonFinite,
    /// `x` or `y` is outside `[0, 1)`.
    #[error("rect origin must satisfy 0 <= x < 1 and 0 <= y < 1")]
    OriginOutOfRange,
    /// `width` or `height` is outside `(0, 1]`.
    #[error("rect size must satisfy 0 < width <= 1 and 0 < height <= 1")]
    SizeOutOfRange,
    /// The rectangle extends past the right or bottom work-area edge.
    #[error("rect must satisfy x + width <= 1 and y + height <= 1")]
    ExceedsWorkArea,
}

/// A rectangle expressed as fractions of the active output work area: where
/// a placement binding puts a window.
///
/// Instances are always validated: every edge is finite and inside `[0, 1]`
/// with `left < right` and `top < bottom` (before pixel rounding). The fields
/// are private so no caller can bypass that invariant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NormalizedRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl NormalizedRect {
    /// Validate and construct a normalized rectangle.
    ///
    /// # Arguments
    ///
    /// * `x`, `y` - origin as a fraction of the work-area width/height.
    /// * `width`, `height` - size as a fraction of the work-area width/height.
    ///
    /// # Returns
    ///
    /// The rectangle when every bound holds.
    ///
    /// # Errors
    ///
    /// Returns a [`RectError`] for non-finite values, an origin outside
    /// `[0, 1)`, a size outside `(0, 1]`, or a rectangle that extends past
    /// the work area by more than floating-point representation error.
    pub(crate) fn new(x: f64, y: f64, width: f64, height: f64) -> Result<Self, RectError> {
        if ![x, y, width, height].iter().all(|value| value.is_finite()) {
            return Err(RectError::NonFinite);
        }
        if !(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y) {
            return Err(RectError::OriginOutOfRange);
        }
        if !(width > 0.0 && width <= 1.0 && height > 0.0 && height <= 1.0) {
            return Err(RectError::SizeOutOfRange);
        }
        let (right, bottom) = (snap_edge(x + width), snap_edge(y + height));
        if right > 1.0 || bottom > 1.0 {
            return Err(RectError::ExceedsWorkArea);
        }
        Ok(Self {
            left: snap_edge(x),
            top: snap_edge(y),
            right,
            bottom,
        })
    }

    /// Map this rectangle to integer bounds inside a work area.
    ///
    /// # Arguments
    ///
    /// * `work_area` - the output work area in logical screen coordinates.
    ///
    /// # Returns
    ///
    /// The screen bounds using the rounding rule (see the module rules), or
    /// `None` when the work area is empty, too small to represent the
    /// rectangle without collapsing an interior edge, or the absolute
    /// coordinates overflow `i32`.
    pub(crate) fn to_work_area_bounds(self, work_area: Rect) -> Option<Rect> {
        let (left, right) = span(self.left, self.right, work_area.width)?;
        let (top, bottom) = span(self.top, self.bottom, work_area.height)?;
        Some(Rect::new(
            work_area.x.checked_add(left)?,
            work_area.y.checked_add(top)?,
            right - left,
            bottom - top,
        ))
    }

    /// Return `[x, y, width, height]` of the validated (snapped) rectangle.
    #[cfg(test)]
    pub(crate) fn components(self) -> [f64; 4] {
        [
            self.left,
            self.top,
            self.right - self.left,
            self.bottom - self.top,
        ]
    }
}

/// Snap an edge fraction within [`EDGE_TOLERANCE`] of a border onto it.
///
/// A fraction further than the tolerance outside `[0, 1]` is returned
/// unchanged, so the caller's range check rejects it.
fn snap_edge(fraction: f64) -> f64 {
    if fraction < EDGE_TOLERANCE {
        0.0
    } else if fraction > 1.0 - EDGE_TOLERANCE && fraction <= 1.0 + EDGE_TOLERANCE {
        1.0
    } else {
        fraction
    }
}

/// Round one axis of a rectangle to pixel edges inside `extent`.
///
/// Returns `None` when the extent is empty, an interior edge lands on the
/// border, or the rounded span is empty.
fn span(start: f64, end: f64, extent: i32) -> Option<(i32, i32)> {
    if extent <= 0 {
        return None;
    }
    let start = edge(start, extent)?;
    let end = edge(end, extent)?;
    (end > start).then_some((start, end))
}

/// Round an edge fraction to a pixel offset (see the module rules).
fn edge(fraction: f64, extent: i32) -> Option<i32> {
    debug_assert!(fraction.is_finite() && (0.0..=1.0).contains(&fraction));
    // `fraction * extent` is at most `i32::MAX` plus the bias, so the cast
    // cannot truncate; the clamp keeps a biased border edge on the border.
    let offset =
        ((fraction * f64::from(extent) + PIXEL_BIAS).floor() as i64).clamp(0, i64::from(extent));
    let offset = i32::try_from(offset).ok()?;
    let interior = fraction > 0.0 && fraction < 1.0;
    if interior && (offset == 0 || offset == extent) {
        return None;
    }
    Some(offset)
}

/// The named presets, by name, in the order of the "Named actions" table in
/// `sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`, which presents them.
///
/// They are exact binary fractions inside the unit square, so validation
/// could not change them; a test proves that each equals
/// [`NormalizedRect::new`] of its values.
const PRESETS: [(&str, NormalizedRect); 9] = {
    const fn preset(x: f64, y: f64, width: f64, height: f64) -> NormalizedRect {
        NormalizedRect {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        }
    }
    [
        ("top-left", preset(0.0, 0.0, 0.5, 0.5)),
        ("top", preset(0.0, 0.0, 1.0, 0.5)),
        ("top-right", preset(0.5, 0.0, 0.5, 0.5)),
        ("left", preset(0.0, 0.0, 0.5, 1.0)),
        ("fullscreen", preset(0.0, 0.0, 1.0, 1.0)),
        ("right", preset(0.5, 0.0, 0.5, 1.0)),
        ("bottom-left", preset(0.0, 0.5, 0.5, 0.5)),
        ("bottom", preset(0.0, 0.5, 1.0, 0.5)),
        ("bottom-right", preset(0.5, 0.5, 0.5, 0.5)),
    ]
};

impl NormalizedRect {
    /// The named preset `name`, case-insensitively and ignoring
    /// surrounding whitespace: the one parser of preset names.
    ///
    /// # Returns
    ///
    /// The preset's rectangle, or `None` for any other spelling.
    pub(crate) fn preset(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase();
        PRESETS
            .iter()
            .find(|(preset, _)| *preset == name)
            .map(|&(_, rect)| rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preset names, in [`PRESETS`]' order.
    const NAMES: [&str; 9] = [
        "top-left",
        "top",
        "top-right",
        "left",
        "fullscreen",
        "right",
        "bottom-left",
        "bottom",
        "bottom-right",
    ];

    fn preset(name: &str) -> NormalizedRect {
        NormalizedRect::preset(name).unwrap_or_else(|| panic!("no preset {name:?}"))
    }

    /// The integer half-split formula the presets used before they were
    /// expressed as normalized rectangles. It is the reference oracle that
    /// proves the generalized mapping did not move any named placement.
    fn legacy_preset_bounds(name: &str, work_area: Rect) -> Option<Rect> {
        let Rect {
            x,
            y,
            width,
            height,
        } = work_area;
        if width <= 0 || height <= 0 {
            return None;
        }
        let half_width = width / 2;
        let half_height = height / 2;
        if matches!(
            name,
            "top-left" | "top-right" | "bottom-left" | "bottom-right" | "left" | "right"
        ) && half_width <= 0
        {
            return None;
        }
        if matches!(
            name,
            "top-left" | "top" | "top-right" | "bottom-left" | "bottom" | "bottom-right"
        ) && half_height <= 0
        {
            return None;
        }
        match name {
            "top-left" => Some(Rect::new(x, y, half_width, half_height)),
            "top" => Some(Rect::new(x, y, width, half_height)),
            "top-right" => Some(Rect::new(
                x.checked_add(half_width)?,
                y,
                width - half_width,
                half_height,
            )),
            "bottom-left" => Some(Rect::new(
                x,
                y.checked_add(half_height)?,
                half_width,
                height - half_height,
            )),
            "bottom" => Some(Rect::new(
                x,
                y.checked_add(half_height)?,
                width,
                height - half_height,
            )),
            "bottom-right" => Some(Rect::new(
                x.checked_add(half_width)?,
                y.checked_add(half_height)?,
                width - half_width,
                height - half_height,
            )),
            "left" => Some(Rect::new(x, y, half_width, height)),
            "fullscreen" => Some(Rect::new(x, y, width, height)),
            "right" => Some(Rect::new(
                x.checked_add(half_width)?,
                y,
                width - half_width,
                height,
            )),
            _ => unreachable!("{name:?} is no preset"),
        }
    }

    #[test]
    fn presets_match_legacy_half_split_bounds_for_every_small_work_area() {
        for name in NAMES {
            for width in 0..=257 {
                for height in [0, 1, 2, 3, 99, 100, 101, 1079, 1080, 2160] {
                    for origin in [(0, 0), (37, -12)] {
                        let work_area = Rect::new(origin.0, origin.1, width, height);
                        assert_eq!(
                            preset(name).to_work_area_bounds(work_area),
                            legacy_preset_bounds(name, work_area),
                            "{name} diverged for work area {work_area:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn presets_match_legacy_bounds_at_extreme_extents_and_origins() {
        let extents = [1, 2, 3, 4097, i32::MAX - 1, i32::MAX];
        let origins = [i32::MIN, -1, 0, 1, i32::MAX - 3, i32::MAX];
        for name in NAMES {
            for &width in &extents {
                for &height in &extents {
                    for &ox in &origins {
                        for &oy in &origins {
                            let work_area = Rect::new(ox, oy, width, height);
                            assert_eq!(
                                preset(name).to_work_area_bounds(work_area),
                                legacy_preset_bounds(name, work_area),
                                "{name} diverged for work area {work_area:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn custom_rect_floors_both_edges_inside_offset_work_area() {
        let rect = NormalizedRect::new(0.25, 0.1, 0.5, 0.8).unwrap();
        // Edges: x 0.25*1366=341.5→341, 0.75*1366=1024.5→1024;
        //        y 0.1*720=72, 0.9*720=648.
        assert_eq!(
            rect.to_work_area_bounds(Rect::new(10, 48, 1366, 720)),
            Some(Rect::new(351, 120, 683, 576))
        );
    }

    #[test]
    fn adjacent_thirds_share_boundaries_and_tile_the_work_area() {
        let third = 1.0 / 3.0;
        let rects = [
            NormalizedRect::new(0.0, 0.0, third, 1.0).unwrap(),
            NormalizedRect::new(third, 0.0, third, 1.0).unwrap(),
            NormalizedRect::new(2.0 * third, 0.0, third, 1.0).unwrap(),
        ];
        for width in [3, 100, 1366, 1920, 2559] {
            let bounds: Vec<_> = rects
                .iter()
                .map(|rect| {
                    rect.to_work_area_bounds(Rect::new(0, 0, width, 10))
                        .unwrap()
                })
                .collect();
            assert_eq!(bounds[0].x, 0);
            assert_eq!(bounds[0].x + bounds[0].width, bounds[1].x, "width {width}");
            assert_eq!(bounds[1].x + bounds[1].width, bounds[2].x, "width {width}");
            assert_eq!(bounds[2].x + bounds[2].width, width, "width {width}");
        }
    }

    #[test]
    fn decimal_sum_within_tolerance_above_one_is_accepted_and_snapped() {
        let (x, width) = (0.1, 0.900_000_000_000_000_1);
        assert!(
            x + width > 1.0,
            "the binary sum must overshoot for this test"
        );
        let rect = NormalizedRect::new(x, 0.7, width, 0.3).unwrap();
        let [left, _, snapped_width, _] = rect.components();
        assert_eq!(left + snapped_width, 1.0);
        assert_eq!(
            rect.to_work_area_bounds(Rect::new(0, 0, 1000, 1000)),
            Some(Rect::new(100, 700, 900, 300))
        );
    }

    #[test]
    fn mapping_refuses_degenerate_or_overflowing_bounds() {
        let sliver = NormalizedRect::new(0.3, 0.0, 0.05, 1.0).unwrap();
        // 0.3*5 = 1.5 → 1 and 0.35*5 = 1.75 → 1: empty after rounding.
        assert_eq!(sliver.to_work_area_bounds(Rect::new(0, 0, 5, 5)), None);
        // An interior edge collapsing onto the border is refused:
        // 0.1 * 5 = 0.5 floors onto the left border.
        let right_nine_tenths = NormalizedRect::new(0.1, 0.0, 0.9, 1.0).unwrap();
        assert_eq!(
            right_nine_tenths.to_work_area_bounds(Rect::new(0, 0, 5, 5)),
            None
        );
        assert_eq!(
            right_nine_tenths.to_work_area_bounds(Rect::new(0, 0, 10, 5)),
            Some(Rect::new(1, 0, 9, 5))
        );
        let full = preset("fullscreen");
        assert_eq!(full.to_work_area_bounds(Rect::new(0, 0, 0, 10)), None);
        assert_eq!(full.to_work_area_bounds(Rect::new(0, 0, 10, -1)), None);
        assert_eq!(
            full.to_work_area_bounds(Rect::new(0, 0, 1, 1)),
            Some(Rect::new(0, 0, 1, 1))
        );
        let right = preset("right");
        assert_eq!(
            right.to_work_area_bounds(Rect::new(i32::MAX, 0, 4, 4)),
            None
        );
        assert_eq!(right.to_work_area_bounds(Rect::new(0, 0, 1, 4)), None);
        assert_eq!(
            right.to_work_area_bounds(Rect::new(0, 0, 3, 4)),
            Some(Rect::new(1, 0, 2, 4))
        );
    }

    #[test]
    fn edges_near_a_border_snap_onto_it() {
        let rect = NormalizedRect::new(1e-12, 0.0, 1.0 - 1e-12, 1.0 - 2e-12).unwrap();
        assert_eq!(rect.components(), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(
            rect.to_work_area_bounds(Rect::new(0, 0, 7, 9)),
            Some(Rect::new(0, 0, 7, 9))
        );
    }

    #[test]
    fn new_rejects_each_invalid_component_class() {
        assert_eq!(
            NormalizedRect::new(f64::NAN, 0.0, 0.5, 0.5),
            Err(RectError::NonFinite)
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, f64::INFINITY, 0.5),
            Err(RectError::NonFinite)
        );
        assert_eq!(
            NormalizedRect::new(-0.1, 0.0, 0.5, 0.5),
            Err(RectError::OriginOutOfRange)
        );
        assert_eq!(
            NormalizedRect::new(0.0, 1.0, 0.5, 0.5),
            Err(RectError::OriginOutOfRange)
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, 0.0, 0.5),
            Err(RectError::SizeOutOfRange)
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.0, 0.5, 1.5),
            Err(RectError::SizeOutOfRange)
        );
        assert_eq!(
            NormalizedRect::new(0.6, 0.0, 0.5, 0.5),
            Err(RectError::ExceedsWorkArea)
        );
        assert_eq!(
            NormalizedRect::new(0.0, 0.5, 0.5, 0.500_001),
            Err(RectError::ExceedsWorkArea)
        );
    }

    /// The preset table, written independently of [`PRESETS`].
    const PRESET_COMPONENTS: [(&str, [f64; 4]); 9] = [
        ("top-left", [0.0, 0.0, 0.5, 0.5]),
        ("top", [0.0, 0.0, 1.0, 0.5]),
        ("top-right", [0.5, 0.0, 0.5, 0.5]),
        ("left", [0.0, 0.0, 0.5, 1.0]),
        ("fullscreen", [0.0, 0.0, 1.0, 1.0]),
        ("right", [0.5, 0.0, 0.5, 1.0]),
        ("bottom-left", [0.0, 0.5, 0.5, 0.5]),
        ("bottom", [0.0, 0.5, 1.0, 0.5]),
        ("bottom-right", [0.5, 0.5, 0.5, 0.5]),
    ];

    /// The `const fn` preset shortcut must never drift from validation.
    #[test]
    fn presets_equal_the_validated_rect_of_the_same_values() {
        assert_eq!(PRESET_COMPONENTS.map(|(name, _)| name), NAMES);
        assert_eq!(PRESETS.map(|(name, _)| name), NAMES);
        for (name, [x, y, width, height]) in PRESET_COMPONENTS {
            assert_eq!(
                NormalizedRect::new(x, y, width, height),
                Ok(preset(name)),
                "{name}"
            );
        }
    }

    /// The user-facing "Named actions" table must list every preset, each
    /// once, with the rectangle `rect` defines.
    #[test]
    fn named_actions_doc_table_matches_the_presets() {
        const DOC: &str = include_str!("../docs/WINDOW_SHORTCUT_CONFIGURATION.md");
        let rows: Vec<(&str, [f64; 4])> = DOC
            .lines()
            .skip_while(|line| line.trim() != "### Named actions")
            .skip(1)
            .skip_while(|line| !line.starts_with('|'))
            .take_while(|line| line.starts_with('|'))
            // The header row and the separator row.
            .skip(2)
            .map(|line| {
                let cells: Vec<&str> = line
                    .split('|')
                    .map(|cell| cell.trim().trim_matches('`'))
                    .filter(|cell| !cell.is_empty())
                    .collect();
                let [name, rect] = cells[..] else {
                    panic!("a row has an action and a rectangle: {line:?}");
                };
                let values: Vec<f64> = rect
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|value| value.trim().parse().expect("a number"))
                    .collect();
                let rect = values
                    .try_into()
                    .unwrap_or_else(|_| panic!("four values: {line:?}"));
                (name, rect)
            })
            .collect();

        assert_eq!(rows.len(), NAMES.len(), "one row per preset");
        for name in NAMES {
            assert_eq!(
                rows.iter().filter(|(row, _)| *row == name).count(),
                1,
                "{name} appears once"
            );
        }
        for (name, [x, y, width, height]) in rows {
            assert_eq!(
                NormalizedRect::new(x, y, width, height),
                Ok(preset(name)),
                "{name}"
            );
        }
    }

    /// The user-facing doc states each rounding constant once, by name and
    /// with the value the code uses.
    #[test]
    fn doc_states_each_rounding_constant_once() {
        const DOC: &str = include_str!("../docs/WINDOW_SHORTCUT_CONFIGURATION.md");
        // Match prose, not layout: a reflow or a capitalized sentence start
        // must not fail the test.
        let doc = DOC
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        for (name, constant) in [
            ("edge tolerance", EDGE_TOLERANCE),
            ("pixel bias", PIXEL_BIAS),
        ] {
            let value = format!("{constant:e}");
            assert_eq!(doc.matches(&value).count(), 1, "{value} is stated once");
            let statement = format!("{name} is `{value}`");
            assert!(doc.contains(&statement), "the doc says \"{statement}\"");
        }
    }

    #[test]
    fn preset_names_parse_case_insensitively_and_reject_others() {
        for (name, rect) in PRESETS {
            assert_eq!(NormalizedRect::preset(name), Some(rect), "{name}");
        }
        assert_eq!(
            NormalizedRect::preset(" Top-Left "),
            Some(preset("top-left"))
        );
        for other in ["", "tile", "top_left", "window.place"] {
            assert_eq!(NormalizedRect::preset(other), None, "{other:?}");
        }
    }

    #[test]
    fn edge_tolerance_is_one_symmetric_border_band() {
        // Inside the band on either side of 1: the edge is the border.
        for overshoot in [-0.5e-9, 0.0, 0.5e-9, 0.9e-9] {
            let rect = NormalizedRect::new(0.5, 0.0, 0.5 + overshoot, 1.0).unwrap();
            assert_eq!(rect.components()[2], 0.5, "overshoot {overshoot}");
        }
        // Past the band above 1: out of range.
        assert_eq!(
            NormalizedRect::new(0.5, 0.0, 0.5 + 2e-9, 1.0),
            Err(RectError::ExceedsWorkArea)
        );
        // Past the band below 1: an ordinary interior edge.
        let rect = NormalizedRect::new(0.5, 0.0, 0.5 - 2e-9, 1.0).unwrap();
        assert!(rect.components()[2] < 0.5);
    }

    /// Round-trip `value` through a decimal with 16 significant digits.
    fn sixteen_digits(value: f64) -> f64 {
        format!("{value:.15e}").parse().unwrap()
    }

    /// The precision contract in the configuration doc: 16 significant digits land
    /// every edge on the pixel its exact fraction names.
    #[test]
    fn sixteen_significant_digits_land_on_the_intended_pixel() {
        let extents = (1..=600).chain([1366, 1920, 2560, 3840, 7680, 65_536, 1_000_000]);
        for extent in extents {
            let step = usize::try_from(extent / 600).unwrap().max(1);
            for left in (0..extent).step_by(step) {
                // A pseudo-random right edge strictly after `left`.
                let spread = i64::from(extent - left);
                let right = left + 1 + i32::try_from(i64::from(left) * 7919 % spread).unwrap();
                let right = right.min(extent);
                let x = sixteen_digits(f64::from(left) / f64::from(extent));
                let width = sixteen_digits(f64::from(right - left) / f64::from(extent));
                let rect = NormalizedRect::new(x, 0.0, width, 1.0).unwrap();
                assert_eq!(
                    rect.to_work_area_bounds(Rect::new(0, 0, extent, 1)),
                    Some(Rect::new(left, 0, right - left, 1)),
                    "edges {left}..{right} of {extent} written as x={x:e} width={width:e}"
                );
            }
        }
    }

    #[test]
    fn a_short_decimal_rounded_down_can_land_one_pixel_short() {
        // The counterexample quoted in the configuration doc.
        let third = NormalizedRect::new(0.3333333, 0.0, 0.6666667, 1.0).unwrap();
        assert_eq!(
            third.to_work_area_bounds(Rect::new(0, 0, 1920, 1)),
            Some(Rect::new(639, 0, 1281, 1))
        );
    }
}
