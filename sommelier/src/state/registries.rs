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

use std::collections::BTreeMap;

/// What one host registry has advertised.
#[derive(Debug, Default)]
pub(crate) struct RegistryRecord {
    /// Every global name the registry advertised, listed or since removed.
    /// Ordered, because a registry's end retires the globals it alone
    /// listed in that order.
    globals: BTreeMap<u32, RegistryGlobal>,
}

/// One global name as one registry last advertised it.
#[derive(Clone, Copy, Debug)]
struct RegistryGlobal {
    /// The advertisement generation the registry advertised last.
    generation: u64,
    status: Status,
}

/// What a registry's removal of a global leaves behind for a bind that
/// crossed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Removal {
    /// The version the guest saw, if a bind that crossed the removal may
    /// create an inert object (see `INERT_BIND_INTERFACE`).
    pub(crate) inert_bind_version: Option<u32>,
}

/// Whether a registry still lists a global it advertised.
#[derive(Clone, Copy, Debug)]
enum Status {
    /// The registry advertised the global and has not removed it.
    Listed {
        /// Whether this advertisement was visible to the guest, once the
        /// proxy has decided. It is kept per registry and generation so
        /// that a delayed `global_remove` of an old hidden or visible
        /// generation is not classified by its replacement's metadata.
        visible: Option<bool>,
    },
    /// The registry removed the global. A later global event for the name
    /// is a new advertisement generation; a bind of it crossed its removal
    /// on the wire.
    Removed {
        /// The version the guest saw, if a bind that crossed the removal
        /// may create an inert object (see `INERT_BIND_INTERFACE`).
        inert_bind_version: Option<u32>,
    },
}

impl RegistryRecord {
    /// Whether the registry lists global `name`.
    pub(crate) fn lists(&self, name: u32) -> bool {
        self.listed_generation(name).is_some()
    }

    /// The generation of global `name` that the registry advertised last,
    /// whether it still lists it or has since removed it.
    pub(crate) fn generation(&self, name: u32) -> Option<u64> {
        self.globals.get(&name).map(|global| global.generation)
    }

    /// The generation of global `name`, if the registry lists it.
    pub(crate) fn listed_generation(&self, name: u32) -> Option<u64> {
        self.globals
            .get(&name)
            .filter(|global| matches!(global.status, Status::Listed { .. }))
            .map(|global| global.generation)
    }

    /// The registry lists global `name` at `generation`, an advertisement
    /// whose visibility the proxy has yet to decide.
    ///
    /// Listing a name the registry already lists resets its visibility to
    /// undecided. The old map kept the earlier decision; this differs only
    /// in theory, because a conforming compositor always sends
    /// `global_remove` before it advertises a name again.
    pub(crate) fn list(&mut self, name: u32, generation: u64) {
        self.globals.insert(
            name,
            RegistryGlobal {
                generation,
                status: Status::Listed { visible: None },
            },
        );
    }

    /// If the registry saw global `name` removed and has not listed it
    /// since, what the removal leaves for a bind that crossed it.
    pub(crate) fn removal(&self, name: u32) -> Option<Removal> {
        match self.globals.get(&name)?.status {
            Status::Removed { inert_bind_version } => Some(Removal { inert_bind_version }),
            Status::Listed { .. } => None,
        }
    }

    /// Whether the registry saw global `name` removed and has not listed it
    /// since.
    pub(crate) fn is_removed(&self, name: u32) -> bool {
        self.removal(name).is_some()
    }

    /// The registry removed global `name`, which it listed; a bind that
    /// crossed the removal may create an inert object at up to
    /// `inert_bind_version`.
    pub(crate) fn remove(&mut self, name: u32, inert_bind_version: Option<u32>) {
        debug_assert!(
            self.lists(name),
            "global {name} removed without being listed"
        );
        if let Some(global) = self.globals.get_mut(&name) {
            global.status = Status::Removed { inert_bind_version };
        }
    }

    /// Whether the registry's current advertisement of global `name` was
    /// visible to the guest, if it lists the name and the proxy has
    /// decided.
    pub(crate) fn visibility(&self, name: u32) -> Option<bool> {
        match self.globals.get(&name)?.status {
            Status::Listed { visible } => visible,
            Status::Removed { .. } => None,
        }
    }

    /// The registry's current advertisement of global `name`, which it
    /// lists, was `visible` to the guest, or not.
    pub(crate) fn set_visibility(&mut self, name: u32, visible: bool) {
        let global = self.globals.get_mut(&name);
        if let Some(RegistryGlobal {
            status: Status::Listed { visible: decided },
            ..
        }) = global
        {
            *decided = Some(visible);
        } else {
            debug_assert!(false, "visibility set on global {name} that is not listed");
        }
    }

    /// The names the registry lists, in order, with their generations.
    pub(crate) fn listed(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.globals.iter().filter_map(|(&name, global)| {
            matches!(global.status, Status::Listed { .. }).then_some((name, global.generation))
        })
    }

    /// The generation of every global the registry advertised, listed or
    /// since removed.
    pub(crate) fn generations(&self) -> impl Iterator<Item = u64> + '_ {
        self.globals.values().map(|global| global.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_goes_from_listed_to_removed_and_listed_again() {
        let mut record = RegistryRecord::default();
        record.list(3, 1);
        record.set_visibility(3, true);
        assert!(record.lists(3));
        assert!(!record.is_removed(3));
        assert_eq!(record.listed_generation(3), Some(1));
        assert_eq!(record.visibility(3), Some(true));

        record.remove(3, Some(4));
        assert!(!record.lists(3));
        assert!(record.is_removed(3));
        assert_eq!(record.listed_generation(3), None);
        assert_eq!(record.generation(3), Some(1));
        assert_eq!(record.visibility(3), None);
        assert_eq!(
            record.removal(3),
            Some(Removal {
                inert_bind_version: Some(4)
            })
        );

        record.list(3, 2);
        assert!(record.lists(3));
        assert!(!record.is_removed(3));
        assert_eq!(record.listed_generation(3), Some(2));
        assert_eq!(record.generation(3), Some(2));
        assert_eq!(record.visibility(3), None);
        assert_eq!(record.removal(3), None);
    }
}
