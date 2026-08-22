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

//! Small lifecycle records used by the placement state owner.
//!
//! These types deliberately have no public mutation surface outside their
//! parent module. `WindowPlacementState` remains the sole owner of all
//! connection-local state transitions.

use std::collections::{HashMap, HashSet};

use super::plan::{OutputState, PlacementBarrierCleanup};

#[derive(Debug, Clone, Copy)]
pub(super) struct AuraShellBinding {
    pub(super) host_id: u32,
    pub(super) global_name: u32,
    pub(super) version: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct OutputRecord {
    pub(super) host_id: u32,
    pub(super) state: OutputState,
}

#[derive(Debug, Default)]
pub(super) struct ToplevelPlacementState {
    /// Last authoritative or predicted screen-space origin.
    pub(super) origin: Option<(i32, i32)>,
    /// Newest self-parent target waiting for a matching host notification.
    pub(super) pending_origin: Option<(i32, i32)>,
}

#[derive(Debug, Default)]
pub(super) struct GtkShellState {
    /// Activation token supplied by GTK for windows created through this
    /// shell binding.
    pub(super) startup_id: Option<String>,
    /// Synthetic gtk_surface1 objects owned by this shell binding.
    pub(super) surfaces: HashSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GtkSurfaceState {
    pub(super) shell_id: u32,
    pub(super) wl_surface_id: u32,
}

/// All application identities associated with one guest `wl_surface`.
///
/// XDG/GTK can update the native Guest OS identity independently from the
/// ARC compatibility identity used by a placement backend. Keeping both
/// values in one record gives the placement state one lifecycle owner and
/// makes teardown remove the complete identity set atomically.
#[derive(Debug, Default)]
pub(super) struct SurfaceApplicationState {
    pub(super) native: Option<String>,
    pub(super) arc: Option<String>,
}

/// One-to-one association with both lookup directions owned together.
#[derive(Debug, Default)]
pub(super) struct BidirectionalLinks {
    forward: HashMap<u32, u32>,
    reverse: HashMap<u32, u32>,
}

impl BidirectionalLinks {
    /// Insert an association if both IDs are unused or already paired.
    #[must_use = "a conflicting association must not replace live state"]
    pub(super) fn insert(&mut self, forward_id: u32, reverse_id: u32) -> bool {
        match (
            self.forward.get(&forward_id).copied(),
            self.reverse.get(&reverse_id).copied(),
        ) {
            (None, None) => {
                self.forward.insert(forward_id, reverse_id);
                self.reverse.insert(reverse_id, forward_id);
                true
            }
            (Some(existing_reverse), Some(existing_forward))
                if existing_reverse == reverse_id && existing_forward == forward_id =>
            {
                true
            }
            _ => false,
        }
    }

    pub(super) fn get_forward(&self, forward_id: u32) -> Option<u32> {
        self.forward.get(&forward_id).copied()
    }

    pub(super) fn get_reverse(&self, reverse_id: u32) -> Option<u32> {
        self.reverse.get(&reverse_id).copied()
    }

    pub(super) fn remove_forward(&mut self, forward_id: u32) -> Option<u32> {
        let reverse_id = self.forward.remove(&forward_id)?;
        self.reverse.remove(&reverse_id);
        Some(reverse_id)
    }

    pub(super) fn remove_reverse(&mut self, reverse_id: u32) -> Option<u32> {
        let forward_id = self.reverse.remove(&reverse_id)?;
        self.forward.remove(&forward_id);
        Some(forward_id)
    }

    pub(super) fn is_consistent(&self) -> bool {
        self.forward.len() == self.reverse.len()
            && self
                .forward
                .iter()
                .all(|(forward_id, reverse_id)| self.reverse.get(reverse_id) == Some(forward_id))
            && self
                .reverse
                .iter()
                .all(|(reverse_id, forward_id)| self.forward.get(forward_id) == Some(reverse_id))
    }
}

/// Ordered host-sync callbacks retained for placement requests.
#[derive(Debug, Default)]
pub(super) struct PlacementBarrierRegistry {
    by_callback: HashMap<u32, PlacementBarrierRecord>,
    active_by_toplevel: HashMap<u32, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementBarrierRecord {
    toplevel_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
}

/// Result of completing one placement barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementBarrierCompletion {
    pub(crate) toplevel_id: u32,
    pub(crate) cleanup: Option<PlacementBarrierCleanup>,
}

impl PlacementBarrierRegistry {
    pub(super) fn contains_callback(&self, callback_id: u32) -> bool {
        self.by_callback.contains_key(&callback_id)
    }

    /// Retain a callback and make it the latest sync for its toplevel.
    ///
    /// A superseded callback remains retained until its terminal host event.
    pub(super) fn register(
        &mut self,
        callback_id: u32,
        toplevel_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
    ) -> bool {
        if self.contains_callback(callback_id) {
            return false;
        }
        self.by_callback.insert(
            callback_id,
            PlacementBarrierRecord {
                toplevel_id,
                cleanup,
            },
        );
        self.active_by_toplevel.insert(toplevel_id, callback_id);
        true
    }

    /// Complete one callback and return cleanup only when it is still active.
    pub(super) fn complete(&mut self, callback_id: u32) -> Option<PlacementBarrierCompletion> {
        let record = self.by_callback.remove(&callback_id)?;
        let is_active = self.active_by_toplevel.get(&record.toplevel_id) == Some(&callback_id);
        if is_active {
            self.active_by_toplevel.remove(&record.toplevel_id);
        }
        Some(PlacementBarrierCompletion {
            toplevel_id: record.toplevel_id,
            cleanup: is_active.then_some(record.cleanup).flatten(),
        })
    }

    pub(super) fn release_toplevel(&mut self, toplevel_id: u32) {
        self.active_by_toplevel.remove(&toplevel_id);
    }

    pub(super) fn has_pending(&self, toplevel_id: u32) -> bool {
        self.active_by_toplevel.contains_key(&toplevel_id)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.by_callback.is_empty() && self.active_by_toplevel.is_empty()
    }

    pub(super) fn is_consistent(&self, is_live_toplevel: impl Fn(u32) -> bool) -> bool {
        self.active_by_toplevel
            .iter()
            .all(|(toplevel_id, callback_id)| {
                self.by_callback
                    .get(callback_id)
                    .is_some_and(|record| record.toplevel_id == *toplevel_id)
                    && is_live_toplevel(*toplevel_id)
            })
    }

    #[cfg(test)]
    pub(super) fn callback_for(&self, callback_id: u32) -> Option<u32> {
        self.by_callback
            .get(&callback_id)
            .map(|record| record.toplevel_id)
    }

    #[cfg(test)]
    pub(super) fn active_callback_for(&self, toplevel_id: u32) -> Option<u32> {
        self.active_by_toplevel.get(&toplevel_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::{BidirectionalLinks, PlacementBarrierCleanup, PlacementBarrierRegistry};

    #[test]
    fn links_reject_conflicting_pairs_without_partial_mutation() {
        let mut links = BidirectionalLinks::default();
        assert!(links.insert(1, 10));
        assert!(!links.insert(1, 11));
        assert!(!links.insert(2, 10));
        assert_eq!(links.get_forward(1), Some(10));
        assert_eq!(links.get_reverse(10), Some(1));
        assert!(links.is_consistent());
        assert_eq!(links.remove_reverse(10), Some(1));
        assert_eq!(links.remove_forward(1), None);
        assert!(links.is_consistent());

        assert!(links.insert(3, 30));
        assert!(links.insert(3, 30));
    }

    #[test]
    fn superseded_barriers_retain_callback_ownership() {
        let mut barriers = PlacementBarrierRegistry::default();
        assert!(barriers.register(40, 77, None));
        assert!(barriers.is_consistent(|toplevel_id| toplevel_id == 77));
        assert!(barriers.register(41, 77, None));
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(barriers.callback_for(40), Some(77));
        assert_eq!(barriers.callback_for(41), Some(77));
        assert!(!barriers.register(41, 88, None));
        assert_eq!(barriers.complete(40).unwrap().toplevel_id, 77);
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(barriers.complete(41).unwrap().toplevel_id, 77);
        assert!(barriers.is_empty());

        let mut released = PlacementBarrierRegistry::default();
        assert!(released.register(50, 99, None));
        assert!(!released.is_consistent(|_| false));
        released.release_toplevel(99);
        assert!(released.is_consistent(|_| false));
        assert_eq!(released.complete(50).unwrap().toplevel_id, 99);
        assert!(released.is_empty());
    }

    #[test]
    fn only_active_barrier_returns_cleanup() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::Unparent {
            zaura_surface_id: 55,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup));
        assert_eq!(barriers.complete(40).unwrap().cleanup, None);
        assert_eq!(
            barriers.complete(41).unwrap().cleanup,
            Some(PlacementBarrierCleanup::Unparent {
                zaura_surface_id: 55
            })
        );
    }
}
