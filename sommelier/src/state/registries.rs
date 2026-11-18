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
        self.generations.insert(name, generation);
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
