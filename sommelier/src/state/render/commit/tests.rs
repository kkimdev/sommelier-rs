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

use super::super::{BufferRect, ViewportState, MAX_PENDING_DAMAGE_RECTS};
use super::*;

/// The buffer the surface shows before each commit, 8x6 pixels.
const SHOWN: GuestId = GuestId::for_test(1);
/// A buffer a commit attaches.
const NEW: GuestId = GuestId::for_test(2);
/// An 8x6 buffer the host holds, and one it has released.
const HELD: ShownBuffer = ShownBuffer {
    size: Some((8, 6)),
    held: true,
};
const RELEASED: ShownBuffer = ShownBuffer {
    size: Some((8, 6)),
    held: false,
};
/// One 24.8 fixed-point unit, a viewport source's.
const ONE: i32 = 256;

/// What a plan decides, besides the next state and the damage.
#[derive(Debug, PartialEq)]
struct Decisions {
    fence: Option<GuestId>,
    copy: Option<GuestId>,
    attachment: Option<(Option<GuestId>, Option<GuestId>)>,
    relatable: Option<GuestId>,
    remaps: bool,
    moves_buffer: bool,
    full_damage: bool,
    viewport_problem: Option<&'static str>,
}

impl CommitPlan {
    fn decisions(&self) -> Decisions {
        Decisions {
            fence: self.fence,
            copy: self.copy,
            attachment: self.attachment,
            relatable: self.relatable,
            remaps: self.remaps,
            moves_buffer: self.moves_buffer,
            full_damage: self.full_damage,
            viewport_problem: self.viewport_problem,
        }
    }
}

/// A damage-only commit of `SHOWN`, which the host holds: the decisions
/// the cases below change.
const DAMAGE_ONLY: Decisions = Decisions {
    fence: None,
    copy: Some(SHOWN),
    attachment: None,
    relatable: Some(SHOWN),
    remaps: false,
    moves_buffer: false,
    full_damage: false,
    viewport_problem: None,
};

/// The surface before a commit: it shows `SHOWN`, 8x6, at scale 1,
/// without a viewport, and nothing is pending.
fn showing() -> SurfaceState {
    let mut surface = SurfaceState::default();
    surface.set_current_buffer_for_test(Some(SHOWN), Some((8, 6)));
    surface
}

/// Plan the commit of `surface`, whose shown buffer the registry describes
/// as `shown`, apply it, and return it.
fn commit(surface: &mut SurfaceState, shown: ShownBuffer) -> CommitPlan {
    let plan = surface.plan_commit(|_| shown);
    surface.apply_commit(&plan);
    plan
}

/// Damage one buffer pixel.
fn pixel(surface: &mut SurfaceState) {
    surface.damage_buffer(BufferRect::new(0, 0, 1, 1));
}

/// Every branch of what a commit waits for, copies, stales and damages,
/// one pending state per row.
#[test]
fn a_plan_decides_the_fence_copy_attachment_and_damage() {
    type Setup = fn(&mut SurfaceState);
    let cases: [(&str, Setup, ShownBuffer, Decisions); 19] = [
        (
            "damage of a held buffer copies it",
            pixel,
            HELD,
            DAMAGE_ONLY,
        ),
        (
            "damage of a released buffer copies nothing",
            pixel,
            RELEASED,
            Decisions {
                copy: None,
                ..DAMAGE_ONLY
            },
        ),
        (
            "a commit of a surface without a buffer copies nothing",
            |surface| {
                surface.set_current_buffer_for_test(None, None);
                pixel(surface);
            },
            HELD,
            Decisions {
                copy: None,
                relatable: None,
                ..DAMAGE_ONLY
            },
        ),
        (
            "an attach waits for its fence, copies, and replaces the shown buffer",
            |surface| {
                surface.attach(Some(NEW), 0, 0);
                pixel(surface);
            },
            RELEASED,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: Some(NEW),
                ..DAMAGE_ONLY
            },
        ),
        (
            "an attach to an empty surface replaces nothing",
            |surface| {
                surface.set_current_buffer_for_test(None, None);
                surface.attach(Some(NEW), 0, 0);
            },
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((None, Some(NEW))),
                relatable: Some(NEW),
                ..DAMAGE_ONLY
            },
        ),
        (
            "a detach waits for nothing and copies nothing",
            |surface| surface.attach(None, 0, 0),
            HELD,
            Decisions {
                copy: None,
                attachment: Some((Some(SHOWN), None)),
                relatable: None,
                ..DAMAGE_ONLY
            },
        ),
        (
            "a scale change remaps the held buffer, which still relates",
            |surface| {
                surface.set_buffer_scale(2);
                pixel(surface);
            },
            HELD,
            Decisions {
                remaps: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "a new viewport remaps",
            |surface| {
                surface.add_viewport();
                pixel(surface);
            },
            HELD,
            Decisions {
                remaps: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "an attach with a new transform relates to no host copy",
            |surface| {
                surface.attach(Some(NEW), 0, 0);
                surface.set_buffer_transform(2);
                pixel(surface);
            },
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: None,
                remaps: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "overflowing surface damage is full",
            |surface| {
                for _ in 0..=MAX_PENDING_DAMAGE_RECTS {
                    surface.damage(SurfaceRect::new(0, 0, 1, 1));
                }
            },
            HELD,
            Decisions {
                full_damage: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "overflowing buffer damage is full",
            |surface| {
                for _ in 0..=MAX_PENDING_DAMAGE_RECTS {
                    pixel(surface);
                }
            },
            HELD,
            Decisions {
                full_damage: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "damage with an attach offset is full",
            |surface| {
                surface.attach(Some(NEW), 3, 0);
                pixel(surface);
            },
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: Some(NEW),
                moves_buffer: true,
                full_damage: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "an attach at an offset without damage is full",
            |surface| surface.attach(Some(NEW), 3, 0),
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: Some(NEW),
                moves_buffer: true,
                full_damage: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "an offset without an attach or damage invents none",
            |surface| surface.offset_for_test(3, 0),
            HELD,
            DAMAGE_ONLY,
        ),
        (
            "an attach through another mapping without damage is full",
            |surface| {
                surface.attach(Some(NEW), 0, 0);
                surface.set_buffer_scale(2);
            },
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: None,
                remaps: true,
                full_damage: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "an attach through an identity mapping without damage lists none",
            |surface| surface.attach(Some(NEW), 0, 0),
            HELD,
            Decisions {
                fence: Some(NEW),
                copy: Some(NEW),
                attachment: Some((Some(SHOWN), Some(NEW))),
                relatable: Some(NEW),
                ..DAMAGE_ONLY
            },
        ),
        (
            "a fractional viewport source without a destination is logged",
            |surface| {
                surface.add_viewport();
                surface.pending_viewport_mut().source = Some((0, 0, ONE + 1, ONE));
            },
            HELD,
            Decisions {
                remaps: true,
                viewport_problem: Some("a fractional wp_viewport source without a destination"),
                ..DAMAGE_ONLY
            },
        ),
        (
            "a fractional viewport source with a destination is not",
            |surface| {
                surface.add_viewport();
                let viewport = surface.pending_viewport_mut();
                viewport.source = Some((0, 0, ONE + 1, ONE));
                viewport.destination = Some((10, 10));
            },
            HELD,
            Decisions {
                remaps: true,
                ..DAMAGE_ONLY
            },
        ),
        (
            "a viewport source outside the buffer is logged",
            |surface| {
                surface.add_viewport();
                surface.pending_viewport_mut().source = Some((0, 0, 9 * ONE, 6 * ONE));
            },
            HELD,
            Decisions {
                remaps: true,
                viewport_problem: Some("a wp_viewport source outside its buffer"),
                ..DAMAGE_ONLY
            },
        ),
    ];
    for (name, setup, shown, decisions) in cases {
        let mut surface = showing();
        setup(&mut surface);
        let mut asked = None;
        let plan = surface.plan_commit(|buffer| {
            asked = Some(buffer);
            shown
        });
        assert_eq!(plan.decisions(), decisions, "{name}");
        let shows = plan.next.buffer_id();
        assert_eq!(asked, shows, "{name}: asks the registry of what it shows");
    }
}

/// What a commit stales in host copies, in buffer pixels: buffer damage
/// as it is, surface damage through the mapping, and the whole buffer
/// for what the mapping cannot invert.
#[test]
fn a_plan_stales_its_damage_in_buffer_pixels() {
    type Setup = fn(&mut SurfaceState);
    let full = DamageRegion::Full;
    let cases: [(&str, Setup, DamageRegion<BufferSpace>); 6] = [
        ("no damage", |_| {}, DamageRegion::Empty),
        (
            "buffer damage",
            |surface| surface.damage_buffer(BufferRect::new(1, 2, 3, 4)),
            vec![BufferRect::new(1, 2, 3, 4)].into(),
        ),
        (
            "surface damage at scale 2, after buffer damage",
            |surface| {
                surface.set_buffer_scale(2);
                surface.damage_buffer(BufferRect::new(1, 2, 3, 4));
                surface.damage(SurfaceRect::new(1, 1, 1, 1));
            },
            vec![BufferRect::new(1, 2, 3, 4), BufferRect::new(2, 2, 2, 2)].into(),
        ),
        (
            "overflowing surface damage",
            |surface| {
                for _ in 0..=MAX_PENDING_DAMAGE_RECTS {
                    surface.damage(SurfaceRect::new(0, 0, 1, 1));
                }
            },
            full.clone(),
        ),
        (
            "surface damage under a transform",
            |surface| {
                surface.set_buffer_transform(1);
                surface.damage(SurfaceRect::new(0, 0, 1, 1));
            },
            full.clone(),
        ),
        (
            "surface damage of a buffer attached at an offset",
            |surface| {
                surface.attach(Some(NEW), 1, 0);
                surface.damage(SurfaceRect::new(0, 0, 1, 1));
            },
            full,
        ),
    ];
    for (name, setup, damage) in cases {
        let mut surface = showing();
        setup(&mut surface);
        let plan = surface.plan_commit(|_| HELD);
        assert_eq!(*surface.copy_damage(&plan), damage, "{name}");
    }
    // Buffer damage alone is the pending damage itself, not a copy of it.
    let mut surface = showing();
    pixel(&mut surface);
    let plan = surface.plan_commit(|_| HELD);
    assert!(matches!(surface.copy_damage(&plan), Cow::Borrowed(_)));
}

/// The host gets each rectangle in surface units, after the whole surface
/// when the plan or the copy calls for it; full pending damage lists none.
#[test]
fn a_plan_damages_the_host_in_surface_units() {
    type Setup = fn(&mut SurfaceState);
    // The 8x6 buffer at scale 2 is a 4x3 surface.
    let whole = SurfaceRect::new(0, 0, 4, 3);
    let cases: [(&str, Setup, bool, Vec<SurfaceRect>); 4] = [
        (
            "listed damage",
            |surface| {
                surface.damage(SurfaceRect::new(1, 1, 1, 1));
                surface.damage_buffer(BufferRect::new(2, 2, 2, 2));
            },
            false,
            vec![SurfaceRect::new(0, 0, 3, 3), SurfaceRect::new(0, 0, 3, 3)],
        ),
        (
            "listed damage of a copy in full",
            |surface| surface.damage(SurfaceRect::new(1, 1, 1, 1)),
            true,
            vec![whole, SurfaceRect::new(0, 0, 3, 3)],
        ),
        (
            "damage at an offset",
            |surface| {
                surface.attach(Some(NEW), 1, 0);
                surface.damage(SurfaceRect::new(1, 1, 1, 1));
            },
            false,
            vec![whole, SurfaceRect::new(0, 0, 3, 3)],
        ),
        (
            "overflowing buffer damage beside surface damage",
            |surface| {
                surface.damage(SurfaceRect::new(1, 1, 1, 1));
                for _ in 0..=MAX_PENDING_DAMAGE_RECTS {
                    pixel(surface);
                }
            },
            false,
            vec![whole],
        ),
    ];
    for (name, setup, copies_in_full, damage) in cases {
        let mut surface = showing();
        surface.set_buffer_scale(2);
        commit(&mut surface, HELD);
        setup(&mut surface);
        let plan = surface.plan_commit(|_| HELD);
        assert_eq!(surface.host_damage(&plan, copies_in_full), damage, "{name}");
    }
}

#[test]
fn surface_commit_applies_every_pending_field_atomically() {
    let mut surface = showing();
    surface.attach(Some(NEW), 11, 12);
    surface.damage(SurfaceRect::new(1, 2, 3, 4));
    surface.damage_buffer(BufferRect::new(5, 6, 7, 8));
    surface.set_buffer_scale(2);
    surface.set_buffer_transform(3);
    surface.add_viewport();
    *surface.pending_viewport_mut() = ViewportState {
        source: Some((0, 0, ONE, ONE)),
        destination: Some((20, 30)),
    };

    let plan = commit(
        &mut surface,
        ShownBuffer {
            size: Some((10, 20)),
            held: false,
        },
    );

    assert!(plan.moves_buffer);
    assert_eq!(
        surface.current,
        SurfaceCurrent {
            content: Some(SurfaceContentSnapshot {
                buffer_id: Some(NEW),
                dimensions: Some((10, 20)),
            }),
            buffer_scale: 2,
            buffer_transform: 3,
            viewport: Some(ViewportState {
                source: Some((0, 0, ONE, ONE)),
                destination: Some((20, 30)),
            }),
        }
    );
    assert_eq!(surface.pending, SurfacePending::default());

    let current = surface.current;
    let next = commit(&mut surface, HELD);
    assert_eq!(next.attachment, None);
    assert!(!next.moves_buffer);
    assert_eq!(surface.current, current);
}

/// A commit without an attach keeps the shown buffer's size, which
/// outlives its destroyed `wl_buffer`, and maps damage by it.
#[test]
fn a_destroyed_buffers_size_outlives_it_until_a_detach() {
    let mut surface = showing();
    surface.clear_buffer_references(SHOWN);
    let plan = commit(&mut surface, ShownBuffer::default());
    assert_eq!(plan.copy, None);
    assert_eq!(plan.mapping.surface_rect(), SurfaceRect::new(0, 0, 8, 6));
    assert_eq!(surface.current_buffer_dimensions(), Some((8, 6)));

    surface.attach(None, 0, 0);
    commit(&mut surface, ShownBuffer::default());
    assert_eq!(surface.current_buffer_id(), None);
    assert_eq!(surface.current_buffer_dimensions(), None);
}

#[test]
fn content_snapshot_ignores_mismatched_destroy_generations() {
    let mut surface = SurfaceState::default();
    surface.set_current_buffer_for_test(Some(GuestId::for_test(7)), Some((100, 50)));

    surface.clear_buffer_references(GuestId::for_test(8));
    assert_eq!(surface.current_buffer_id(), Some(GuestId::for_test(7)));
    assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));

    surface.clear_buffer_references(GuestId::for_test(7));
    assert_eq!(surface.current_buffer_id(), None);
    assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));
}

#[test]
fn pending_damage_is_exact_until_bounded_then_becomes_full() {
    let mut surface = SurfaceState::default();
    let exact: Vec<_> = (0..MAX_PENDING_DAMAGE_RECTS)
        .map(|index| BufferRect::new(index as i32, 0, 1, 1))
        .collect();
    for rect in &exact {
        surface.damage_buffer(*rect);
    }

    assert_eq!(surface.pending.buffer_damage, DamageRegion::Rects(exact));

    surface.damage_buffer(BufferRect::new(-10, -20, 30, 40));
    assert_eq!(surface.pending.buffer_damage, DamageRegion::Full);

    for _ in 0..MAX_PENDING_DAMAGE_RECTS {
        surface.damage_buffer(BufferRect::new(1, 1, 1, 1));
    }
    assert_eq!(
        surface.pending.buffer_damage,
        DamageRegion::Full,
        "a collapsed region must remain bounded"
    );

    assert!(commit(&mut surface, HELD).full_damage);
    assert_eq!(surface.pending.buffer_damage, DamageRegion::Empty);
}

#[test]
fn pending_viewport_starts_from_the_pending_else_the_committed_one() {
    let source = Some((0, 0, ONE, ONE));
    let mut surface = SurfaceState::default();
    surface.pending_viewport_mut().source = source;
    commit(&mut surface, HELD);
    let committed = surface.current.viewport;

    // Nothing pending: the request edits a copy of the committed viewport.
    surface.pending_viewport_mut().destination = Some((10, 20));
    assert_eq!(surface.current.viewport, committed);
    assert_eq!(
        surface.pending.viewport,
        Some(Some(ViewportState {
            source,
            destination: Some((10, 20)),
        }))
    );

    // A viewport pending: the request edits it.
    surface.pending_viewport_mut().destination = Some((30, 40));
    commit(&mut surface, HELD);
    assert_eq!(
        surface.current.viewport,
        Some(ViewportState {
            source,
            destination: Some((30, 40)),
        })
    );

    // A destroyed viewport pending: the request edits a copy of the
    // committed viewport.
    surface.remove_viewport();
    surface.pending_viewport_mut().source = None;
    assert_eq!(
        surface.pending.viewport,
        Some(Some(ViewportState {
            source: None,
            destination: Some((30, 40)),
        }))
    );
}

/// The mapping of a commit that attaches a buffer of `size` pixels to
/// `surface`, after `setup`.
fn attached_mapping(size: (i32, i32), setup: impl FnOnce(&mut SurfaceState)) -> CommitPlan {
    let mut surface = SurfaceState::default();
    surface.attach(Some(NEW), 0, 0);
    setup(&mut surface);
    surface.plan_commit(|_| ShownBuffer {
        size: Some(size),
        held: false,
    })
}

#[test]
fn damage_buffer_mapping_applies_buffer_scale_and_filtering_outset() {
    let plan = attached_mapping((100, 100), |surface| surface.set_buffer_scale(2));
    let mapped = plan.mapping.to_surface(BufferRect::new(20, 10, 4, 6));
    assert_eq!(mapped, SurfaceRect::new(9, 4, 4, 5));
}

#[test]
fn damage_buffer_mapping_does_not_include_surface_offset() {
    let plan = attached_mapping((100, 100), |surface| {
        surface.attach(Some(NEW), 80, 90);
    });
    assert!(plan.moves_buffer);
    assert_eq!(
        plan.mapping.to_surface(BufferRect::new(4, 5, 6, 7)),
        SurfaceRect::new(3, 4, 8, 9),
        "wl_surface.attach/offset changes content placement, not buffer damage coordinates"
    );
}

#[test]
fn rotated_buffer_damage_covers_the_transformed_surface() {
    let mapping = attached_mapping((8, 6), |surface| surface.set_buffer_transform(1)).mapping;
    assert_eq!(
        mapping.surface_rect(),
        SurfaceRect::new(0, 0, 6, 8),
        "90-degree transforms expose a height-by-width surface extent"
    );
    assert_eq!(
        mapping.to_surface(BufferRect::new(1, 1, 1, 1)),
        mapping.surface_rect(),
        "the bridge does not rotate damage, so it damages the whole surface"
    );
}

#[test]
fn rotated_scaled_buffer_full_damage_uses_logical_extent() {
    let plan = attached_mapping((8, 6), |surface| {
        surface.set_buffer_scale(2);
        surface.set_buffer_transform(1);
    });
    assert_eq!(
        plan.mapping.surface_rect(),
        SurfaceRect::new(0, 0, 3, 4),
        "buffer scale converts the rotated extent to surface coordinates"
    );
}
