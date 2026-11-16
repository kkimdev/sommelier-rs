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
//! [`NormalizedRect`] is the single description of *where* a placement
//! shortcut puts a window: `[x, y, width, height]` as fractions of the
//! output work area. The nine named [`WindowLayoutAction`] presets are
//! expressed as such rectangles ([`WindowLayoutAction::rect`]), and every
//! placement — named or configured — goes through
//! [`NormalizedRect::to_work_area_bounds`], so there is exactly one rounding
//! rule.
//!
//! # Rounding
//!
//! A rectangle is stored as its four edge fractions.
//!
//! For a work area `(X, Y, W, H)` each edge fraction `f` maps to the pixel
//! offset `floor(f * W + PIXEL_BIAS)` (or `H`), computed in `f64` and clamped
//! to `[0, W]`. The window is `(X + left, Y + top, right - left, bottom -
//! top)`. Flooring shared edge fractions keeps adjacent rectangles sharing one
//! boundary, and for the half splits used by the presets it reproduces integer
//! division (`W / 2`) exactly. The `PIXEL_BIAS` of one millionth of a pixel
//! only absorbs representation error, so `1 / 3` written as
//! `0.3333333333333333` still lands on the intended pixel.
//!
//! A placement is refused (`None`) instead of emitting degenerate bounds when
//! the work area is too small to represent the rectangle: an edge strictly
//! inside the unit interval that lands on the work-area border, or an empty
//! extent after rounding.

use crate::accelerator::WindowLayoutAction;

/// Sub-pixel bias added before flooring an edge to a pixel offset.
///
/// The worst-case product error for an `i32` extent is below `1e-6` pixels,
/// so the bias corrects representation error without moving any edge that
/// was meant to fall between pixels.
const PIXEL_BIAS: f64 = 1e-6;

/// A rectangle expressed as fractions of the active output work area.
///
/// Every edge is finite and inside `[0, 1]` with `left < right` and
/// `top < bottom` (before pixel rounding). The fields are private so no
/// caller can bypass that invariant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NormalizedRect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl NormalizedRect {
    /// Map this rectangle to integer bounds inside a work area.
    ///
    /// # Arguments
    ///
    /// * `work_area` - `(x, y, width, height)` in logical screen coordinates,
    ///   as returned by [`crate::state::OutputState::work_area`].
    ///
    /// # Returns
    ///
    /// `(x, y, width, height)` screen bounds using the module-level rounding
    /// rule, or `None` when the work area is empty, too small to represent the
    /// rectangle without collapsing an interior edge, or the absolute
    /// coordinates overflow `i32`.
    pub(crate) fn to_work_area_bounds(
        self,
        work_area: (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        let (origin_x, origin_y, work_width, work_height) = work_area;
        let (left, right) = span(self.left, self.right, work_width)?;
        let (top, bottom) = span(self.top, self.bottom, work_height)?;
        Some((
            origin_x.checked_add(left)?,
            origin_y.checked_add(top)?,
            right - left,
            bottom - top,
        ))
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

/// Round an edge fraction to a pixel offset; see the module rounding rule.
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

impl WindowLayoutAction {
    /// Return the normalized work-area rectangle for this action.
    ///
    /// The named presets split the work area into halves.
    pub(crate) fn rect(self) -> NormalizedRect {
        const fn preset(x: f64, y: f64, width: f64, height: f64) -> NormalizedRect {
            NormalizedRect {
                left: x,
                top: y,
                right: x + width,
                bottom: y + height,
            }
        }
        match self {
            Self::TopLeft => preset(0.0, 0.0, 0.5, 0.5),
            Self::Top => preset(0.0, 0.0, 1.0, 0.5),
            Self::TopRight => preset(0.5, 0.0, 0.5, 0.5),
            Self::Left => preset(0.0, 0.0, 0.5, 1.0),
            Self::Fullscreen => preset(0.0, 0.0, 1.0, 1.0),
            Self::Right => preset(0.5, 0.0, 0.5, 1.0),
            Self::BottomLeft => preset(0.0, 0.5, 0.5, 0.5),
            Self::Bottom => preset(0.0, 0.5, 1.0, 0.5),
            Self::BottomRight => preset(0.5, 0.5, 0.5, 0.5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRESETS: [WindowLayoutAction; 9] = [
        WindowLayoutAction::TopLeft,
        WindowLayoutAction::Top,
        WindowLayoutAction::TopRight,
        WindowLayoutAction::Left,
        WindowLayoutAction::Fullscreen,
        WindowLayoutAction::Right,
        WindowLayoutAction::BottomLeft,
        WindowLayoutAction::Bottom,
        WindowLayoutAction::BottomRight,
    ];

    /// The integer half-split formula the presets used before they were
    /// expressed as normalized rectangles. It is the reference oracle that
    /// proves the generalized mapping did not move any named placement.
    fn legacy_preset_bounds(
        action: WindowLayoutAction,
        (x, y, width, height): (i32, i32, i32, i32),
    ) -> Option<(i32, i32, i32, i32)> {
        use WindowLayoutAction::*;
        if width <= 0 || height <= 0 {
            return None;
        }
        let half_width = width / 2;
        let half_height = height / 2;
        if matches!(
            action,
            TopLeft | TopRight | BottomLeft | BottomRight | Left | Right
        ) && half_width <= 0
        {
            return None;
        }
        if matches!(
            action,
            TopLeft | Top | TopRight | BottomLeft | Bottom | BottomRight
        ) && half_height <= 0
        {
            return None;
        }
        match action {
            TopLeft => Some((x, y, half_width, half_height)),
            Top => Some((x, y, width, half_height)),
            TopRight => Some((
                x.checked_add(half_width)?,
                y,
                width - half_width,
                half_height,
            )),
            BottomLeft => Some((
                x,
                y.checked_add(half_height)?,
                half_width,
                height - half_height,
            )),
            Bottom => Some((x, y.checked_add(half_height)?, width, height - half_height)),
            BottomRight => Some((
                x.checked_add(half_width)?,
                y.checked_add(half_height)?,
                width - half_width,
                height - half_height,
            )),
            Left => Some((x, y, half_width, height)),
            Fullscreen => Some((x, y, width, height)),
            Right => Some((x.checked_add(half_width)?, y, width - half_width, height)),
        }
    }

    #[test]
    fn presets_match_legacy_half_split_bounds_for_every_small_work_area() {
        for action in PRESETS {
            for width in 0..=257 {
                for height in [0, 1, 2, 3, 99, 100, 101, 1079, 1080, 2160] {
                    for origin in [(0, 0), (37, -12)] {
                        let work_area = (origin.0, origin.1, width, height);
                        assert_eq!(
                            action.rect().to_work_area_bounds(work_area),
                            legacy_preset_bounds(action, work_area),
                            "{action:?} diverged for work area {work_area:?}"
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
        for action in PRESETS {
            for &width in &extents {
                for &height in &extents {
                    for &ox in &origins {
                        for &oy in &origins {
                            let work_area = (ox, oy, width, height);
                            assert_eq!(
                                action.rect().to_work_area_bounds(work_area),
                                legacy_preset_bounds(action, work_area),
                                "{action:?} diverged for work area {work_area:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn preset_mapping_refuses_degenerate_or_overflowing_bounds() {
        let full = WindowLayoutAction::Fullscreen.rect();
        assert_eq!(full.to_work_area_bounds((0, 0, 0, 10)), None);
        assert_eq!(full.to_work_area_bounds((0, 0, 10, -1)), None);
        assert_eq!(full.to_work_area_bounds((0, 0, 1, 1)), Some((0, 0, 1, 1)));
        let right = WindowLayoutAction::Right.rect();
        assert_eq!(right.to_work_area_bounds((i32::MAX, 0, 4, 4)), None);
        assert_eq!(right.to_work_area_bounds((0, 0, 1, 4)), None);
        assert_eq!(right.to_work_area_bounds((0, 0, 3, 4)), Some((1, 0, 2, 4)));
    }
}
