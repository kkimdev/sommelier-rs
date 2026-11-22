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

//! A `wl_surface.commit`, decided before anything changes.
//!
//! [`SurfaceState::plan_commit`] decides everything a commit does from the
//! surface's pending and current state and what the render-buffer registry
//! says of the buffer the surface will show ([`ShownBuffer`]): the fence it
//! waits for, the buffer whose host copy it refreshes, what it stales in
//! the surface's other host copies, the damage the host gets, the
//! registry edge of its attach, and the surface's next current state. It
//! reads nothing else and changes nothing, so a commit that waits for a
//! fence or is dropped leaves the surface as it was, and its pending state
//! stays pending, as on the host, which never got the commit.
//! `handler::compositor` executes the plan, and
//! [`SurfaceState::apply_commit`] applies it once the commit goes through.
//!
//! A plan holds decisions, not rectangles: the damage stays in the pending
//! state, which [`SurfaceState::copy_damage`] and
//! [`SurfaceState::host_damage`] read through the plan, so planning copies
//! no frame's rectangles.

use super::{
    BufferSpace, DamageRegion, SurfaceAttachment, SurfaceContentSnapshot, SurfaceCurrent,
    SurfaceMapping, SurfacePending, SurfaceRect, SurfaceState, FIXED_ONE,
};
use crate::state::GuestId;
use std::borrow::Cow;

/// What the render-buffer registry knows of the buffer a commit leaves its
/// surface showing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShownBuffer {
    /// Its size in pixels, while the registry has it.
    pub(crate) size: Option<(i32, i32)>,
    /// Whether the host holds it: committed, and not released since.
    pub(crate) held: bool,
}

/// What one `wl_surface.commit` does ([`SurfaceState::plan_commit`]).
///
/// A plan holds for the pending state it was planned from, so it lives
/// within one dispatch of the commit: [`SurfaceState::copy_damage`],
/// [`SurfaceState::host_damage`] and [`SurfaceState::apply_commit`] take
/// it with the surface it was planned from, before any request changes
/// that surface, and a commit dispatched again after a fence wait plans
/// afresh.
#[derive(Debug)]
pub(crate) struct CommitPlan {
    /// The surface's current state once the commit applies.
    next: SurfaceCurrent,
    /// The buffer the commit attaches, whose fence it waits for if it is a
    /// native dma-buf: the fence belongs to the contents an attach
    /// submits, as in ChromiumOS Sommelier, whose sync point is set at
    /// attach.
    pub(crate) fence: Option<GuestId>,
    /// The buffer whose host copy the commit refreshes before the host
    /// sees it: one it attaches, or the one it keeps showing while the
    /// host holds it (see INVARIANTS, "Damage-only commits copy from a
    /// buffer the host holds"). Once the host has released a buffer the
    /// client owns its storage and may be drawing its next frame there:
    /// reading it could tear, and the host no longer samples it.
    pub(crate) copy: Option<GuestId>,
    /// The attach's edge in the render-buffer registry: the buffer the
    /// surface showed and the one it shows, or `None` without an attach.
    pub(crate) attachment: Option<(Option<GuestId>, Option<GuestId>)>,
    /// How the surface shows its buffer after the commit.
    pub(crate) mapping: SurfaceMapping,
    /// The buffer the surface shows after the commit, unless it is
    /// attached together with a new mapping: it was then last copied for
    /// the old mapping, while the client's damage is relative to the new
    /// one, so no host copy relates to the new contents pixel for pixel.
    pub(crate) relatable: Option<GuestId>,
    /// Whether the commit changes how buffer pixels map onto the surface:
    /// its scale, transform or viewport. Pixel damage in one buffer then no
    /// longer carries over to the surface's others. (An offset moves the
    /// surface, not its pixels.)
    pub(crate) remaps: bool,
    /// Whether an attach offset moves the new buffer against the surface,
    /// so that surface damage no longer relates to its pixels.
    moves_buffer: bool,
    /// Whether the host gets the whole surface as damage, whatever the
    /// copy: the pending damage overflowed, an offset moves it, or a new
    /// buffer shown through another mapping comes without damage.
    full_damage: bool,
    /// A viewport rule the commit breaks, which Exo accepts and the proxy
    /// therefore logs instead of raising (see INVARIANTS, "Protocol
    /// errors").
    pub(crate) viewport_problem: Option<&'static str>,
    /// The pending attachment the plan was made from, which the methods
    /// that take the plan check it against.
    planned_from: SurfaceAttachment,
}

impl SurfaceState {
    /// Decide what committing this surface does, given what the registry
    /// says of the buffer it will show (`shown`, asked of that buffer
    /// only).
    pub(crate) fn plan_commit(&self, shown: impl FnOnce(GuestId) -> ShownBuffer) -> CommitPlan {
        // Name every field: one the commit never applies is then an unused
        // variable (see docs/DESIGN.md, "Double-buffered state").
        let SurfacePending {
            attachment,
            surface_damage,
            buffer_damage,
            buffer_scale,
            buffer_transform,
            attach_offset,
            viewport,
        } = &self.pending;
        let current = self.current;
        let shown_id = match *attachment {
            SurfaceAttachment::Unchanged => current.buffer_id(),
            SurfaceAttachment::Detach => None,
            SurfaceAttachment::Attach(buffer) => Some(buffer),
        };
        let shown = shown_id.map(shown).unwrap_or_default();
        let next = SurfaceCurrent {
            content: match *attachment {
                SurfaceAttachment::Unchanged => current.content,
                SurfaceAttachment::Detach => None,
                SurfaceAttachment::Attach(buffer) => Some(SurfaceContentSnapshot {
                    buffer_id: Some(buffer),
                    dimensions: shown.size,
                }),
            },
            buffer_scale: buffer_scale.unwrap_or(current.buffer_scale),
            buffer_transform: buffer_transform.unwrap_or(current.buffer_transform),
            viewport: viewport.unwrap_or(current.viewport),
        };
        // The size of a destroyed buffer outlives it in the contents.
        let mapping = next.mapping(shown.size.or(next.buffer_dimensions()));
        let attaches = attachment.buffer_id().is_some();
        let offset = attach_offset.unwrap_or((0, 0));
        let moves_buffer = attaches && offset != (0, 0);
        let remaps = (next.buffer_scale, next.buffer_transform, next.viewport)
            != (
                current.buffer_scale,
                current.buffer_transform,
                current.viewport,
            );
        // `None`: full damage, which lists no rectangle.
        let listed = surface_damage.rects().zip(buffer_damage.rects());
        let has_rects =
            listed.is_some_and(|(surface, buffer)| !surface.is_empty() || !buffer.is_empty());
        let full_damage = listed.is_none()
            || (offset != (0, 0) && has_rects)
            || (attaches && (moves_buffer || !mapping.is_identity()) && !has_rects);
        // wp_viewport.bad_size and out_of_buffer are deliberately not
        // raised: Exo accepts both, so ending the session would break apps
        // that work today.
        let viewport_problem = if next.has_fractional_source_without_destination() {
            Some("a fractional wp_viewport source without a destination")
        } else if !mapping.source_fits_buffer() {
            Some("a wp_viewport source outside its buffer")
        } else {
            None
        };
        CommitPlan {
            fence: attachment.buffer_id(),
            copy: match *attachment {
                SurfaceAttachment::Attach(buffer) => Some(buffer),
                SurfaceAttachment::Unchanged => shown_id.filter(|_| shown.held),
                SurfaceAttachment::Detach => None,
            },
            attachment: match *attachment {
                SurfaceAttachment::Unchanged => None,
                SurfaceAttachment::Detach | SurfaceAttachment::Attach(_) => {
                    Some((current.buffer_id(), attachment.buffer_id()))
                }
            },
            relatable: shown_id.filter(|_| !(attaches && remaps)),
            remaps,
            moves_buffer,
            full_damage,
            viewport_problem,
            mapping,
            next,
            planned_from: *attachment,
        }
    }

    /// The commit `plan` of this surface's damage in the pixels of the
    /// buffer it shows: what it stales in the host copies of the surface's
    /// buffers.
    ///
    /// Buffer damage is already in buffer pixels. Surface damage maps to them
    /// through the plan's mapping (see [`SurfaceMapping::to_buffer`]); a
    /// transform, or an attach offset, falls back to the whole buffer.
    pub(crate) fn copy_damage(&self, plan: &CommitPlan) -> Cow<'_, DamageRegion<BufferSpace>> {
        self.debug_assert_planned(plan);
        let pending = &self.pending;
        let Some(surface_damage) = pending.surface_damage.rects() else {
            return Cow::Owned(DamageRegion::Full);
        };
        // Without surface damage, the buffer damage is all there is, and is
        // not copied.
        if surface_damage.is_empty() {
            return Cow::Borrowed(&pending.buffer_damage);
        }
        // Full buffer damage stays full: pushing to it adds nothing.
        let mut damage = pending.buffer_damage.clone();
        for &rect in surface_damage {
            match plan.mapping.to_buffer(rect).filter(|_| !plan.moves_buffer) {
                Some(rect) => damage.push(rect),
                None => return Cow::Owned(DamageRegion::Full),
            }
        }
        Cow::Owned(damage)
    }

    /// The damage the host gets with commit `plan` of this surface, in
    /// surface-local units, which is all the host takes: the whole surface
    /// if the plan says so or the copy replaces the whole image
    /// (`copies_in_full`), and each pending rectangle, buffer damage
    /// through the plan's mapping.
    pub(crate) fn host_damage(&self, plan: &CommitPlan, copies_in_full: bool) -> Vec<SurfaceRect> {
        self.debug_assert_planned(plan);
        let pending = &self.pending;
        let (surface_damage, buffer_damage) = pending
            .surface_damage
            .rects()
            .zip(pending.buffer_damage.rects())
            .unwrap_or_default();
        let mut damage = Vec::new();
        if plan.full_damage || copies_in_full {
            damage.push(plan.mapping.surface_rect());
        }
        damage.extend(surface_damage.iter().map(|&rect| rect.outset()));
        damage.extend(
            buffer_damage
                .iter()
                .map(|&rect| plan.mapping.to_surface(rect)),
        );
        damage
    }

    /// Apply commit `plan` of this surface, which went through: the plan's
    /// current state replaces this one, and the pending state, which it
    /// was planned from, ends.
    pub(crate) fn apply_commit(&mut self, plan: &CommitPlan) {
        self.debug_assert_planned(plan);
        self.current = plan.next;
        self.pending = SurfacePending::default();
    }
}

impl SurfaceState {
    /// Check that `plan` was planned from this surface's pending state, as
    /// far as one field tells: the pending attachment.
    fn debug_assert_planned(&self, plan: &CommitPlan) {
        debug_assert_eq!(
            plan.planned_from, self.pending.attachment,
            "a commit plan used with a pending state it was not planned from"
        );
    }
}

impl SurfaceCurrent {
    /// Whether the viewport crops a fractional source without a
    /// destination, which `wp_viewport.bad_size` forbids.
    fn has_fractional_source_without_destination(&self) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let Some((_, _, width, height)) = viewport.source else {
            return false;
        };
        viewport.destination.is_none() && (width % FIXED_ONE != 0 || height % FIXED_ONE != 0)
    }
}

#[cfg(test)]
mod tests;
