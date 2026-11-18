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

//! Damage rectangles: whether damage reaches a buffer, where it clips to
//! the buffer, and merging the rectangles of one commit.

use crate::state::{BufferRect, BufferSpace, BufferState, DamageRegion, Rect};

pub(super) fn clipped_damage(
    rect: BufferRect,
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
pub(crate) fn damage_reaches_buffer(
    damage: &DamageRegion<BufferSpace>,
    buffer: &BufferState,
) -> bool {
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
fn damage_edges<S>(rect: Rect<S>) -> Option<(i64, i64, i64, i64)> {
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

fn damage_from_edges<S>(edges: (i64, i64, i64, i64)) -> Rect<S> {
    let (x0, y0, x1, y1) = edges;
    Rect::new(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32)
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
pub(crate) fn coalesce_damage_rects<S>(damage: &[Rect<S>]) -> Vec<Rect<S>> {
    let mut merged = Vec::with_capacity(damage.len());

    for rect in damage {
        let Some(mut candidate) = damage_edges(*rect) else {
            continue;
        };

        // Merge until the expanded candidate no longer intersects another
        // rectangle.  The fixed-point loop is needed for A touching B and B
        // touching C while A and C do not touch directly.
        while let Some(index) = merged.iter().position(|existing: &Rect<S>| {
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

#[cfg(test)]
mod tests {
    use super::coalesce_damage_rects;
    use crate::state::BufferRect;

    #[test]
    fn coalesces_overlapping_and_compact_adjacent_damage() {
        let damage = [
            BufferRect::new(0, 0, 8, 4),
            BufferRect::new(6, 2, 8, 4),
            BufferRect::new(14, 2, 2, 4),
        ];

        assert_eq!(
            coalesce_damage_rects(&damage),
            vec![BufferRect::new(0, 0, 16, 6)]
        );
    }

    #[test]
    fn keeps_sparse_corner_damage_separate() {
        let damage = [BufferRect::new(0, 0, 2, 2), BufferRect::new(2, 2, 2, 2)];

        assert_eq!(coalesce_damage_rects(&damage), damage);
    }

    #[test]
    fn coalescing_discards_empty_damage_without_overflowing_edges() {
        let damage = [
            BufferRect::new(0, 0, 0, 8),
            BufferRect::new(i32::MAX, i32::MAX, 1, 1),
            BufferRect::new(4, 4, 2, 2),
        ];

        assert_eq!(
            coalesce_damage_rects(&damage),
            vec![
                BufferRect::new(i32::MAX, i32::MAX, 1, 1),
                BufferRect::new(4, 4, 2, 2)
            ]
        );
    }
}
