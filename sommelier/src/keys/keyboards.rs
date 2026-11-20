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

//! The host keyboards of a connection, one [`Keyboard`] record each, which
//! holds every fact of the keyboard itself: its guest object and seat, its
//! keymap, its modifiers and its extended keyboard. Its keys' generations
//! are the key registry's (`super::generations`), and its focus the focus
//! registry's (`super::focus`). Only
//! [`super::KeyboardCore`] changes a record; the rest of the proxy reads
//! them.

use std::collections::BTreeMap;

use crate::keymap::{KeyboardModifiers, Keymap};
use crate::state::{GuestId, HostId};

/// One host `wl_keyboard`, from its guest's `wl_seat.get_keyboard` to its
/// end.
#[derive(Debug)]
pub(crate) struct Keyboard {
    /// The guest `wl_keyboard`.
    pub(crate) guest: GuestId,
    /// The guest `wl_seat` it was created from, which may have ended since
    /// (INVARIANTS "Releasing a parent does not end its children").
    pub(crate) seat: GuestId,
    /// The keymap the host sent, while the proxy can use it.
    pub(crate) keymap: Option<Keymap>,
    /// The modifiers the host last sent. Exo sends them only when they
    /// change, before `enter` rather than after it, and not again with a
    /// new keymap, so neither focus changes nor keymaps reset them.
    pub(crate) modifiers: KeyboardModifiers,
    /// Its host `zcr_extended_keyboard_v1`, once bound, through which keys
    /// are acknowledged and peeked.
    pub(crate) extended: Option<HostId>,
}

impl Keyboard {
    pub(super) fn new(guest: GuestId, seat: GuestId) -> Self {
        Self {
            guest,
            seat,
            keymap: None,
            modifiers: KeyboardModifiers::default(),
            extended: None,
        }
    }
}

/// The host keyboards of a connection, by host ID.
#[derive(Debug, Default)]
pub(crate) struct Keyboards(BTreeMap<HostId, Keyboard>);

impl Keyboards {
    pub(super) fn insert(&mut self, host: HostId, keyboard: Keyboard) {
        let replaced = self.0.insert(host, keyboard);
        debug_assert!(replaced.is_none(), "host keyboard {host} exists");
    }

    pub(super) fn remove(&mut self, host: HostId) -> Option<Keyboard> {
        self.0.remove(&host)
    }

    pub(crate) fn get(&self, host: HostId) -> Option<&Keyboard> {
        self.0.get(&host)
    }

    pub(super) fn get_mut(&mut self, host: HostId) -> Option<&mut Keyboard> {
        self.0.get_mut(&host)
    }

    /// The guest seat of host keyboard `host`.
    pub(crate) fn seat(&self, host: HostId) -> Option<GuestId> {
        self.get(host).map(|keyboard| keyboard.seat)
    }

    /// The host keyboard whose extended keyboard is `extended`.
    pub(crate) fn of_extended(&self, extended: HostId) -> Option<HostId> {
        let mut keyboards = self.0.iter();
        keyboards
            .find_map(|(&host, keyboard)| (keyboard.extended == Some(extended)).then_some(host))
    }

    /// The host keyboards of guest seat `seat`, in the order of their guest
    /// IDs.
    pub(crate) fn of_seat(&self, seat: GuestId) -> Vec<HostId> {
        let mut keyboards: Vec<_> = self
            .0
            .iter()
            .filter(|(_, keyboard)| keyboard.seat == seat)
            .map(|(&host, keyboard)| (keyboard.guest, host))
            .collect();
        keyboards.sort_unstable();
        keyboards.into_iter().map(|(_, host)| host).collect()
    }

    /// Every keyboard, by host ID.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = (HostId, &Keyboard)> {
        self.0.iter().map(|(&host, keyboard)| (host, keyboard))
    }
}

/// What tests read of the keyboards, as no peer can.
#[cfg(test)]
impl Keyboards {
    /// The keymap of host keyboard `host`, if it has one.
    pub(crate) fn keymap_for_test(&self, host: HostId) -> Option<&Keymap> {
        self.get(host).and_then(|keyboard| keyboard.keymap.as_ref())
    }
}
