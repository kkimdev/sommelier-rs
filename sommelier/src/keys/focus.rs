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

//! Keyboard focus: which guest surface each host keyboard is on, and
//! which superseded enters still owe the guest a leave. The keyboard core
//! owns it ([`crate::keys::KeyboardCore::focus`]), and only the core moves
//! it, as it steps a focus input, so a focus change and what it ends of the
//! keys happen together.

use std::collections::BTreeMap;

use crate::state::{GuestId, HostId};

/// One `wl_keyboard` focus generation.
///
/// Keep both object namespaces: the guest surface drives text-input events,
/// while the host surface identifies delayed `wl_keyboard.leave` events even
/// after the guest mapping has been retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KeyboardFocus {
    pub(crate) guest_seat: GuestId,
    pub(crate) guest_surface: GuestId,
    pub(crate) host_surface: HostId,
}

/// One authoritative seat focus transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SeatFocusChange {
    pub(super) guest_seat: GuestId,
    pub(super) previous_surface: Option<GuestId>,
    pub(super) current_surface: Option<GuestId>,
}

/// Result of mutating keyboard focus ownership.
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct KeyboardFocusUpdate {
    /// Whether the event changed focus or consumed one deliverable retired leave.
    pub(super) accepted: bool,
    /// Seat transitions that must be projected to text-input objects.
    pub(super) seat_changes: Vec<SeatFocusChange>,
    /// Keyboard sessions whose focus-scoped key state is no longer valid.
    pub(super) retired_keyboards: Vec<HostId>,
}

/// Canonical keyboard-to-seat/surface focus registry.
///
/// Every keyboard on one seat must own the same surface. Entering a different
/// surface retires older keyboard generations for that seat instead of
/// retaining a fallback focus that a delayed leave could revive, and so does
/// one keyboard's leave of the seat's surface ([`Self::leave`]).
#[derive(Debug, Default)]
pub(crate) struct KeyboardFocusRegistry {
    keyboards: BTreeMap<HostId, KeyboardFocus>,
    /// Focus generations superseded by a different keyboard resource.
    ///
    /// Their seat focus is no longer authoritative, but the guest resource
    /// received an enter and still needs exactly one matching leave. A newer
    /// enter on the same keyboard supersedes its older generation without a
    /// tombstone because forwarding that delayed leave would clear the newer
    /// resource focus.
    retired_guest_enters: BTreeMap<(HostId, HostId), KeyboardFocus>,
}

impl KeyboardFocusRegistry {
    /// The surface the keyboards of `guest_seat` focus. [`Self::enter`]
    /// retires every other keyboard of the seat on another surface, so
    /// they all focus the same one.
    pub(crate) fn surface_for_seat(&self, guest_seat: GuestId) -> Option<GuestId> {
        let surfaces = || {
            self.keyboards
                .values()
                .filter(move |focus| focus.guest_seat == guest_seat)
                .map(|focus| focus.guest_surface)
        };
        let surface = surfaces().next();
        debug_assert!(
            surfaces().all(|other| Some(other) == surface),
            "the keyboards of seat {guest_seat} focus different surfaces"
        );
        surface
    }

    /// Whether any keyboard has the focus.
    pub(crate) fn has_focus(&self) -> bool {
        !self.keyboards.is_empty()
    }

    pub(crate) fn focus_for_keyboard(&self, host_keyboard: HostId) -> Option<KeyboardFocus> {
        self.keyboards.get(&host_keyboard).copied()
    }

    pub(crate) fn keyboard_owns_surface(
        &self,
        host_keyboard: HostId,
        guest_surface: GuestId,
    ) -> bool {
        self.focus_for_keyboard(host_keyboard)
            .is_some_and(|focus| focus.guest_surface == guest_surface)
    }

    /// Every focus, current or retired, and its host keyboard.
    #[cfg(test)]
    pub(crate) fn focuses_for_test(&self) -> impl Iterator<Item = (HostId, KeyboardFocus)> + '_ {
        self.keyboards
            .iter()
            .map(|(&keyboard, &focus)| (keyboard, focus))
            .chain(
                self.retired_guest_enters
                    .iter()
                    .map(|(&(keyboard, _), &focus)| (keyboard, focus)),
            )
    }

    pub(super) fn enter(
        &mut self,
        host_keyboard: HostId,
        focus: KeyboardFocus,
    ) -> KeyboardFocusUpdate {
        if self.focus_for_keyboard(host_keyboard) == Some(focus) {
            return KeyboardFocusUpdate::default();
        }

        let mut affected_seats = vec![focus.guest_seat];
        if let Some(previous_focus) = self.focus_for_keyboard(host_keyboard) {
            affected_seats.push(previous_focus.guest_seat);
        }
        affected_seats.sort_unstable();
        affected_seats.dedup();
        let previous_surfaces = affected_seats
            .iter()
            .map(|&guest_seat| (guest_seat, self.surface_for_seat(guest_seat)))
            .collect::<Vec<_>>();

        // A newer enter on the same wl_keyboard resource supersedes every
        // older resource generation. Delayed leaves for those generations
        // must not become visible after the newer enter.
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, current)| {
                (keyboard == host_keyboard
                    || (current.guest_seat == focus.guest_seat
                        && current.guest_surface != focus.guest_surface))
                    .then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable();
        for keyboard in &retired_keyboards {
            if let Some(retired_focus) = self.keyboards.remove(keyboard) {
                if *keyboard != host_keyboard {
                    self.retired_guest_enters
                        .insert((*keyboard, retired_focus.host_surface), retired_focus);
                }
            }
        }
        self.keyboards.insert(host_keyboard, focus);

        let seat_changes = previous_surfaces
            .into_iter()
            .filter_map(|(guest_seat, previous_surface)| {
                let current_surface = self.surface_for_seat(guest_seat);
                (previous_surface != current_surface).then_some(SeatFocusChange {
                    guest_seat,
                    previous_surface,
                    current_surface,
                })
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards,
        }
    }

    pub(super) fn leave(
        &mut self,
        host_keyboard: HostId,
        host_surface: HostId,
    ) -> KeyboardFocusUpdate {
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return if self
                .retired_guest_enters
                .remove(&(host_keyboard, host_surface))
                .is_some()
            {
                KeyboardFocusUpdate {
                    accepted: true,
                    ..KeyboardFocusUpdate::default()
                }
            } else {
                KeyboardFocusUpdate::default()
            };
        };
        if focus.host_surface != host_surface {
            return KeyboardFocusUpdate::default();
        }
        // A host sends every keyboard of the seat its leave in turn, so the
        // first one moves the seat's focus off the surface, and the field's
        // preedit is committed before any keyboard leaves it (F1). The
        // seat's other keyboards, all on that surface, are retired, each
        // still owed its own leave, as at an enter of another surface.
        let mut update = self.remove_keyboard(host_keyboard, focus);
        let mut others = self
            .keyboards
            .iter()
            .filter(|(_, other)| other.guest_seat == focus.guest_seat)
            .map(|(&keyboard, &other)| (keyboard, other))
            .collect::<Vec<_>>();
        others.sort_unstable_by_key(|&(keyboard, _)| keyboard);
        for (keyboard, other) in others {
            self.keyboards.remove(&keyboard);
            self.retired_guest_enters
                .insert((keyboard, other.host_surface), other);
            update.retired_keyboards.push(keyboard);
        }
        update.seat_changes = vec![SeatFocusChange {
            guest_seat: focus.guest_seat,
            previous_surface: Some(focus.guest_surface),
            current_surface: None,
        }];
        update
    }

    pub(super) fn release(&mut self, host_keyboard: HostId) -> KeyboardFocusUpdate {
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return KeyboardFocusUpdate::default();
        };
        self.remove_keyboard(host_keyboard, focus)
    }

    pub(super) fn destroy_surface(&mut self, guest_surface: GuestId) -> KeyboardFocusUpdate {
        let mut affected_seats = self
            .keyboards
            .values()
            .filter_map(|focus| (focus.guest_surface == guest_surface).then_some(focus.guest_seat))
            .collect::<Vec<_>>();
        affected_seats.sort_unstable();
        affected_seats.dedup();

        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, focus)| {
                (focus.guest_surface == guest_surface).then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable();
        for keyboard in &retired_keyboards {
            self.keyboards.remove(keyboard);
        }
        self.retired_guest_enters
            .retain(|_, focus| focus.guest_surface != guest_surface);

        let seat_changes = affected_seats
            .into_iter()
            .map(|guest_seat| SeatFocusChange {
                guest_seat,
                previous_surface: Some(guest_surface),
                current_surface: self.surface_for_seat(guest_seat),
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: !retired_keyboards.is_empty(),
            seat_changes,
            retired_keyboards,
        }
    }

    fn remove_keyboard(
        &mut self,
        host_keyboard: HostId,
        focus: KeyboardFocus,
    ) -> KeyboardFocusUpdate {
        let previous_surface = self.surface_for_seat(focus.guest_seat);
        self.keyboards.remove(&host_keyboard);
        let current_surface = self.surface_for_seat(focus.guest_seat);
        let seat_changes = (previous_surface != current_surface)
            .then_some(SeatFocusChange {
                guest_seat: focus.guest_seat,
                previous_surface,
                current_surface,
            })
            .into_iter()
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards: vec![host_keyboard],
        }
    }
}

#[cfg(test)]
mod tests;
