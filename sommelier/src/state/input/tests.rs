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

use super::*;

const KEYBOARD_A: HostId = HostId(10);
const KEYBOARD_B: HostId = HostId(11);

/// Seat 1's focus on guest surface 20, host surface 200.
const ON_20: KeyboardFocus = KeyboardFocus {
    guest_seat: 1,
    guest_surface: 20,
    host_surface: 200,
};
/// Seat 1's focus on guest surface 21, host surface 201.
const ON_21: KeyboardFocus = KeyboardFocus {
    guest_seat: 1,
    guest_surface: 21,
    host_surface: 201,
};

fn accepted(
    seat_changes: Vec<SeatFocusChange>,
    retired_keyboards: Vec<HostId>,
) -> KeyboardFocusUpdate {
    KeyboardFocusUpdate {
        accepted: true,
        seat_changes,
        retired_keyboards,
    }
}

#[test]
fn keyboard_focus_registry_balances_replacement_and_delayed_leave() {
    let mut registry = KeyboardFocusRegistry::default();

    assert_eq!(
        registry.enter(KEYBOARD_A, ON_20),
        accepted(
            vec![SeatFocusChange {
                guest_seat: 1,
                previous_surface: None,
                current_surface: Some(20),
            }],
            Vec::new()
        )
    );
    assert_eq!(
        registry.enter(KEYBOARD_B, ON_21),
        accepted(
            vec![SeatFocusChange {
                guest_seat: 1,
                previous_surface: Some(20),
                current_surface: Some(21),
            }],
            vec![KEYBOARD_A]
        )
    );
    assert_eq!(registry.surface_for_seat(1), Some(21));
    assert_eq!(
        registry.leave(KEYBOARD_A, ON_20.host_surface),
        accepted(Vec::new(), Vec::new()),
        "a delayed leave must balance the retired guest enter once"
    );
    assert_eq!(
        registry.leave(KEYBOARD_A, ON_20.host_surface),
        KeyboardFocusUpdate::default(),
        "a duplicate delayed leave must be rejected"
    );
    assert_eq!(registry.surface_for_seat(1), Some(21));
}

/// A delayed leave answers an enter the guest saw. Once that enter is
/// gone, superseded by a newer enter on the same keyboard, released or
/// destroyed, the leave changes nothing.
#[test]
fn keyboard_focus_registry_rejects_a_leave_for_a_superseded_or_ended_enter() {
    struct Case {
        what_ended_the_enter: &'static str,
        end_the_enter: fn(&mut KeyboardFocusRegistry),
        expected_focus_of_a: Option<KeyboardFocus>,
    }
    let cases = [
        Case {
            what_ended_the_enter: "a newer enter on the same keyboard",
            end_the_enter: |registry| {
                registry.enter(KEYBOARD_A, ON_21);
            },
            expected_focus_of_a: Some(ON_21),
        },
        Case {
            what_ended_the_enter: "a release, after another keyboard took the seat",
            end_the_enter: |registry| {
                registry.enter(KEYBOARD_B, ON_21);
                registry.release(KEYBOARD_A);
            },
            expected_focus_of_a: None,
        },
        Case {
            what_ended_the_enter: "a destroy, after another keyboard took the seat",
            end_the_enter: |registry| {
                registry.enter(KEYBOARD_B, ON_21);
                registry.destroy_surface(ON_20.guest_surface);
            },
            expected_focus_of_a: None,
        },
    ];
    for case in cases {
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(KEYBOARD_A, ON_20);
        (case.end_the_enter)(&mut registry);

        let what = case.what_ended_the_enter;
        assert_eq!(
            registry.leave(KEYBOARD_A, ON_20.host_surface),
            KeyboardFocusUpdate::default(),
            "{what}"
        );
        assert_eq!(
            registry.focus_for_keyboard(KEYBOARD_A),
            case.expected_focus_of_a,
            "{what}"
        );
        assert_eq!(registry.surface_for_seat(1), Some(21), "{what}");
    }
}

/// The first keyboard's leave of a shared surface moves the seat's
/// focus off it and retires the other keyboard, whose own leave then
/// balances its enter and moves nothing.
#[test]
fn keyboard_focus_registry_ends_a_shared_surface_at_its_first_leave() {
    let mut registry = KeyboardFocusRegistry::default();
    registry.enter(KEYBOARD_A, ON_20);
    registry.enter(KEYBOARD_B, ON_20);

    assert_eq!(
        registry.leave(KEYBOARD_A, ON_20.host_surface),
        accepted(
            vec![SeatFocusChange {
                guest_seat: 1,
                previous_surface: Some(20),
                current_surface: None,
            }],
            vec![KEYBOARD_A, KEYBOARD_B]
        )
    );
    assert_eq!(registry.surface_for_seat(1), None);
    assert_eq!(
        registry.leave(KEYBOARD_B, ON_20.host_surface),
        accepted(Vec::new(), Vec::new())
    );
    assert_eq!(registry.surface_for_seat(1), None);
}

#[test]
fn keyboard_focus_registry_destroys_surface_across_seats_without_fallback() {
    let mut registry = KeyboardFocusRegistry::default();
    registry.enter(KEYBOARD_A, ON_20);
    registry.enter(
        KEYBOARD_B,
        KeyboardFocus {
            guest_seat: 2,
            ..ON_20
        },
    );
    registry.enter(
        HostId(12),
        KeyboardFocus {
            guest_seat: 3,
            guest_surface: 30,
            host_surface: 300,
        },
    );

    assert_eq!(
        registry.destroy_surface(20),
        accepted(
            vec![
                SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: None,
                },
                SeatFocusChange {
                    guest_seat: 2,
                    previous_surface: Some(20),
                    current_surface: None,
                },
            ],
            vec![KEYBOARD_A, KEYBOARD_B]
        )
    );
    assert_eq!(registry.surface_for_seat(1), None);
    assert_eq!(registry.surface_for_seat(2), None);
    assert_eq!(registry.surface_for_seat(3), Some(30));
}

#[test]
fn keyboard_focus_registry_preserves_invariants_for_all_short_sequences() {
    #[derive(Clone, Copy)]
    enum Operation {
        Enter(HostId, KeyboardFocus),
        /// The keyboard leaves the focus's host surface.
        Leave(HostId, KeyboardFocus),
        Release(HostId),
        /// The focus's guest surface is destroyed.
        Destroy(KeyboardFocus),
    }

    let mut operations = Vec::new();
    for keyboard in [KEYBOARD_A, KEYBOARD_B] {
        for focus in [ON_20, ON_21] {
            operations.push(Operation::Enter(keyboard, focus));
            operations.push(Operation::Leave(keyboard, focus));
        }
        operations.push(Operation::Release(keyboard));
    }
    operations.extend([Operation::Destroy(ON_20), Operation::Destroy(ON_21)]);
    let sequence_len = 5;
    let sequence_count = operations.len().pow(sequence_len);

    for mut encoded in 0..sequence_count {
        let mut registry = KeyboardFocusRegistry::default();
        // Model the focus currently visible to each guest wl_keyboard
        // resource. A newer enter on the same resource supersedes its
        // previous surface; active and retired registry generations must
        // account for this model exactly.
        let mut guest_focus = BTreeMap::new();
        for _ in 0..sequence_len {
            let operation = operations[encoded % operations.len()];
            encoded /= operations.len();
            let update = match operation {
                Operation::Enter(keyboard, focus) => {
                    let update = registry.enter(keyboard, focus);
                    if update.accepted {
                        guest_focus.insert(keyboard, focus);
                    }
                    update
                }
                Operation::Leave(keyboard, focus) => {
                    let update = registry.leave(keyboard, focus.host_surface);
                    if update.accepted {
                        assert_eq!(
                            guest_focus.get(&keyboard).map(|focus| focus.host_surface),
                            Some(focus.host_surface),
                            "only a guest-visible enter can accept a leave"
                        );
                        guest_focus.remove(&keyboard);
                    }
                    update
                }
                Operation::Release(keyboard) => {
                    guest_focus.remove(&keyboard);
                    registry.release(keyboard)
                }
                Operation::Destroy(focus) => {
                    guest_focus.retain(|_, guest| guest.guest_surface != focus.guest_surface);
                    registry.destroy_surface(focus.guest_surface)
                }
            };

            for change in &update.seat_changes {
                assert_ne!(change.previous_surface, change.current_surface);
                assert_eq!(
                    registry.surface_for_seat(change.guest_seat),
                    change.current_surface
                );
            }
            for left in registry.keyboards.values() {
                for right in registry.keyboards.values() {
                    if left.guest_seat == right.guest_seat {
                        assert_eq!(
                            left.guest_surface, right.guest_surface,
                            "one seat must never retain competing surface generations"
                        );
                    }
                }
            }
            for (&(keyboard, host_surface), retired) in &registry.retired_guest_enters {
                assert_eq!(retired.host_surface, host_surface);
                assert!(
                    !registry.keyboards.contains_key(&keyboard),
                    "one keyboard cannot have active and retired guest enters"
                );
                assert_eq!(
                    registry
                        .retired_guest_enters
                        .keys()
                        .filter(|(candidate, _)| *candidate == keyboard)
                        .count(),
                    1,
                    "one keyboard can have at most one deliverable retired leave"
                );
            }
            assert_eq!(
                guest_focus.len(),
                registry.keyboards.len() + registry.retired_guest_enters.len(),
                "every guest-visible focus must be active or await one retired leave"
            );
            for (&keyboard, &focus) in &guest_focus {
                assert!(
                    registry.focus_for_keyboard(keyboard) == Some(focus)
                        || registry
                            .retired_guest_enters
                            .get(&(keyboard, focus.host_surface))
                            == Some(&focus),
                    "the registry must account for the guest resource's current focus"
                );
            }
        }
    }
}
