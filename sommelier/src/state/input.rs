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

//! Pointer focus: which guest surface each host pointer is on. The
//! keyboard focus is the keyboard core's (`crate::keys::focus`).

use std::collections::BTreeMap;

use super::{GuestId, HostId};

/// The guest surface each host `wl_pointer` has entered. A
/// `wl_pointer.button` names no surface, and a press on the surface of a
/// composition ends it (`handler::text_input::end_composition_on_press`).
#[derive(Debug, Default)]
pub struct PointerFocus(BTreeMap<HostId, GuestId>);

impl PointerFocus {
    /// `pointer` entered guest surface `surface`.
    pub fn enter(&mut self, pointer: HostId, surface: GuestId) {
        self.0.insert(pointer, surface);
    }

    /// `pointer` left guest surface `surface`. A `leave` of a surface it
    /// is not on, which a host can deliver late, changes nothing, as for
    /// a keyboard (`crate::keys::focus::KeyboardFocusRegistry::leave`).
    pub fn leave(&mut self, pointer: HostId, surface: GuestId) {
        if self.surface(pointer) == Some(surface) {
            self.0.remove(&pointer);
        }
    }

    /// `pointer` ended.
    pub fn forget_pointer(&mut self, pointer: HostId) {
        self.0.remove(&pointer);
    }

    /// The surface `pointer` is on, if any.
    pub fn surface(&self, pointer: HostId) -> Option<GuestId> {
        self.0.get(&pointer).copied()
    }

    /// Guest surface `surface` ended: no pointer is on it.
    pub fn forget_surface(&mut self, surface: GuestId) {
        self.0.retain(|_, &mut entered| entered != surface);
    }

    #[cfg(test)]
    pub(super) fn focuses_for_test(&self) -> impl Iterator<Item = (HostId, GuestId)> + '_ {
        self.0.iter().map(|(&pointer, &surface)| (pointer, surface))
    }
}
