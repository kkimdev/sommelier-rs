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

//! What each host `wl_registry` has advertised: one [`RegistryRecord`] per
//! registry, keyed by its host ID in `Context::registries`.
//!
//! A client may create several registries, and the compositor sends each
//! the whole global list, so what a registry lists is its own. A record
//! exists from the registry's first global until the registry ends, when
//! `handler::registry::forget_registry` drops it whole. The policy that
//! reads and writes it is `handler::registry`'s.

use std::collections::{BTreeMap, BTreeSet};

/// What one host registry has advertised.
#[derive(Debug, Default)]
pub(crate) struct RegistryRecord {
    /// The global names the registry lists: advertised and not removed.
    /// Ordered, because a registry's end retires the globals it alone
    /// listed in that order.
    listed: BTreeSet<u32>,
    /// The advertisement generation of each global the registry advertised,
    /// listed or since removed.
    generations: BTreeMap<u32, u64>,
    /// The global names removed from the registry. A subsequent global
    /// event for a removed name represents a new advertisement generation;
    /// a bind of a removed name crossed its removal on the wire. Each name
    /// keeps the version the guest saw if it can be bound inert (see
    /// `INERT_BIND_INTERFACE`), and `None` otherwise.
    removed: BTreeMap<u32, Option<u32>>,
}

impl RegistryRecord {
    /// Whether the registry lists global `name`.
    pub(crate) fn lists(&self, name: u32) -> bool {
        self.listed.contains(&name)
    }

    /// The generation of global `name` that the registry advertised last,
    /// whether it still lists it or has since removed it.
    pub(crate) fn generation(&self, name: u32) -> Option<u64> {
        self.generations.get(&name).copied()
    }

    /// The generation of global `name`, if the registry lists it.
    pub(crate) fn listed_generation(&self, name: u32) -> Option<u64> {
        self.generation(name).filter(|_| self.lists(name))
    }

    /// The registry lists global `name` at `generation`.
    pub(crate) fn list(&mut self, name: u32, generation: u64) {
        self.listed.insert(name);
        self.removed.remove(&name);
        self.generations.insert(name, generation);
    }

    /// If the registry saw global `name` removed and has not listed it
    /// since, the version at which a bind that crossed the removal may
    /// create an inert object, if any.
    pub(crate) fn removal(&self, name: u32) -> Option<Option<u32>> {
        self.removed.get(&name).copied()
    }

    /// Whether the registry saw global `name` removed and has not listed it
    /// since.
    pub(crate) fn is_removed(&self, name: u32) -> bool {
        self.removal(name).is_some()
    }

    /// The registry saw global `name` removed; a bind that crossed the
    /// removal may create an inert object at up to `inert_bind_version`.
    pub(crate) fn record_removal(&mut self, name: u32, inert_bind_version: Option<u32>) {
        self.removed.insert(name, inert_bind_version);
    }

    /// The registry no longer lists global `name`.
    ///
    /// # Returns
    ///
    /// Whether it listed it.
    pub(crate) fn unlist(&mut self, name: u32) -> bool {
        self.listed.remove(&name)
    }

    /// The names the registry lists, in order, with their generations.
    pub(crate) fn listed(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.listed
            .iter()
            .filter_map(|&name| Some((name, self.generation(name)?)))
    }

    /// The generation of every global the registry advertised, listed or
    /// since removed.
    pub(crate) fn generations(&self) -> impl Iterator<Item = u64> + '_ {
        self.generations.values().copied()
    }
}
