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
use super::transaction::PlacementTransaction;

#[derive(Debug, Clone, Copy)]
pub(super) struct AuraShellBinding {
    pub(super) host_id: u32,
    pub(super) global_name: u32,
    pub(super) version: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RemoteShellBinding {
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
    /// Last authoritative screen-space origin observed from Aura.
    pub(super) origin: Option<(i32, i32)>,
    /// Whether `origin` is still confirmed by an idle Aura event.
    ///
    /// A completed self-parent transaction may have no matching
    /// `origin_change` on some Exo/Ash versions. In that case we retire the
    /// transaction at the ordered cleanup barrier but retain the last known
    /// coordinate only as a diagnostic value; it must not be used as the
    /// baseline for another relative placement until Aura reports a fresh
    /// origin.
    pub(super) origin_confirmed: bool,
    /// Reducer-owned placement transaction. All phase, generation, deferred
    /// target, and cleanup state lives here; handlers can only feed events
    /// through the methods exposed by `WindowPlacementState`.
    pub(super) transaction: PlacementTransaction,
    /// Last size accepted from an authoritative host configure.
    pub(super) observed_size: Option<(i32, i32)>,
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

/// The complete state transition produced when an XDG toplevel role dies.
///
/// The placement owner removes the XDG role, its Aura child, and all
/// per-toplevel origin/barrier state before returning this record. The handler
/// only serializes the optional Aura release request; it cannot forget one
/// half of the lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct XdgToplevelRelease {
    pub(crate) wl_surface_guest_id: u32,
    pub(crate) zaura_toplevel_host_id: Option<u32>,
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
    /// Surface IDs whose transient native-identity restore has already been
    /// handed to the wire adapter. A role can outlive several callbacks, so
    /// this one-shot claim prevents stale callbacks after teardown from
    /// emitting the same restore again.
    released_restore_claims: HashSet<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementBarrierRecord {
    toplevel_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
    generation: Option<u64>,
    /// Optional diagnostic operation ID carried across asynchronous barriers.
    ///
    /// The ID is deliberately metadata only: it never participates in
    /// lifecycle decisions or host protocol serialization.
    trace_id: Option<u64>,
    /// Whether this callback displaced a previously emitted transient-ARC
    /// restore claim. If the transaction is cancelled before publication,
    /// the previous claim must be put back.
    displaced_restore_claim: bool,
}

/// Result of completing one placement barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlacementBarrierCompletion {
    pub(crate) toplevel_id: u32,
    pub(crate) cleanup: Option<PlacementBarrierCleanup>,
    pub(crate) trace_id: Option<u64>,
    pub(crate) generation: Option<u64>,
}

impl PlacementBarrierRegistry {
    pub(super) fn contains_callback(&self, callback_id: u32) -> bool {
        self.by_callback.contains_key(&callback_id)
    }

    /// Retain a callback and make it the latest sync for its toplevel.
    ///
    /// A superseded callback remains retained until its terminal host event.
    #[cfg(test)]
    pub(super) fn register(
        &mut self,
        callback_id: u32,
        toplevel_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
    ) -> bool {
        self.register_with_trace(callback_id, toplevel_id, cleanup, None)
    }

    /// Retain a callback and attach an optional diagnostic operation ID.
    ///
    /// The trace ID is not part of the placement state machine. It exists so
    /// an asynchronous `wl_callback.done` can be connected to the exact wire
    /// batch that created it in runtime logs.
    #[cfg(test)]
    pub(super) fn register_with_trace(
        &mut self,
        callback_id: u32,
        toplevel_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
        trace_id: Option<u64>,
    ) -> bool {
        self.register_with_generation(callback_id, toplevel_id, cleanup, None, trace_id)
    }

    /// Retain a callback together with the placement generation that created
    /// it. A callback from an older generation may still need to be retired,
    /// but it must never run cleanup against a newer transaction.
    pub(super) fn register_with_generation(
        &mut self,
        callback_id: u32,
        toplevel_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
        generation: Option<u64>,
        trace_id: Option<u64>,
    ) -> bool {
        if self.contains_callback(callback_id) {
            return false;
        }
        let displaced_restore_claim = match cleanup.as_ref() {
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id, ..
            }) => self.released_restore_claims.remove(zaura_surface_id),
            _ => false,
        };
        self.by_callback.insert(
            callback_id,
            PlacementBarrierRecord {
                toplevel_id,
                cleanup,
                generation,
                trace_id,
                displaced_restore_claim,
            },
        );
        self.active_by_toplevel.insert(toplevel_id, callback_id);
        true
    }

    /// Complete one callback and return cleanup only when it is still active.
    ///
    /// A callback can outlive its XDG role. In that teardown case a transient
    /// ARC identity still needs to be restored because it belongs to the
    /// backing `wl_surface`. The caller supplies authoritative role
    /// liveness so an older callback that completes after a newer direct
    /// bounds operation cannot be mistaken for a released role.
    pub(super) fn complete(
        &mut self,
        callback_id: u32,
        role_is_live: bool,
    ) -> Option<PlacementBarrierCompletion> {
        let record = self.by_callback.remove(&callback_id)?;
        let is_active = self.active_by_toplevel.get(&record.toplevel_id) == Some(&callback_id);
        if is_active {
            self.active_by_toplevel.remove(&record.toplevel_id);
        }
        // A transient ARC identity is owned by the wl_surface rather than
        // the xdg_toplevel role. If that role was released before the sync
        // callback arrived, retain only one restore claim so multiple stale
        // callbacks cannot re-emit the same native identity. The claim is
        // also recorded for the live active callback: a later role teardown
        // must not replay cleanup that was already handed to the wire
        // adapter.
        let cleanup = match record.cleanup {
            Some(
                cleanup @ PlacementBarrierCleanup::RestoreNativeApplicationId {
                    zaura_surface_id,
                    ..
                },
            ) if is_active || !role_is_live => self
                .released_restore_claims
                .insert(zaura_surface_id)
                .then_some(cleanup),
            Some(cleanup) if is_active => Some(cleanup),
            _ => None,
        };
        Some(PlacementBarrierCompletion {
            toplevel_id: record.toplevel_id,
            cleanup,
            trace_id: record.trace_id,
            generation: record.generation,
        })
    }

    /// Cancel a barrier that was registered during a wire transaction that
    /// was never published.
    ///
    /// This is distinct from `complete`: no host callback can arrive for an
    /// unpublished `wl_display.sync`, so retaining the record would leak
    /// cleanup ownership and make a later callback ID appear live forever.
    pub(super) fn cancel(&mut self, callback_id: u32) -> bool {
        let Some(record) = self.by_callback.remove(&callback_id) else {
            return false;
        };
        if record.displaced_restore_claim {
            if let Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id,
                ..
            }) = record.cleanup.as_ref()
            {
                self.released_restore_claims.insert(*zaura_surface_id);
            }
        }
        if self.active_by_toplevel.get(&record.toplevel_id) == Some(&callback_id) {
            self.active_by_toplevel.remove(&record.toplevel_id);
        }
        true
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
        assert_eq!(barriers.complete(40, true).unwrap().toplevel_id, 77);
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(barriers.complete(41, true).unwrap().toplevel_id, 77);
        assert!(barriers.is_empty());

        let mut released = PlacementBarrierRegistry::default();
        assert!(released.register(50, 99, None));
        assert!(!released.is_consistent(|_| false));
        released.release_toplevel(99);
        assert!(released.is_consistent(|_| false));
        assert_eq!(released.complete(50, false).unwrap().toplevel_id, 99);
        assert!(released.is_empty());
    }

    #[test]
    fn only_active_barrier_returns_cleanup() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::RetainSelfParent {
            zaura_surface_id: 55,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup));
        assert_eq!(barriers.complete(40, true).unwrap().cleanup, None);
        assert_eq!(
            barriers.complete(41, true).unwrap().cleanup,
            Some(PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: 55
            })
        );
    }

    #[test]
    fn stale_restore_cleanup_is_not_replayed_after_live_role_finishes() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
            zaura_surface_id: 55,
            wl_surface_guest_id: 10,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup));

        // The newer generation completes first. The old callback is still
        // host-owned, but the role is alive; it must not be interpreted as a
        // released role and restore an identity after the newer generation.
        assert!(barriers.complete(41, true).unwrap().cleanup.is_some());
        assert!(
            barriers.complete(40, true).unwrap().cleanup.is_none(),
            "a stale callback on a live role must not replay transient cleanup"
        );
    }

    #[test]
    fn live_restore_claim_is_not_replayed_after_role_teardown() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
            zaura_surface_id: 55,
            wl_surface_guest_id: 10,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup));

        assert!(
            barriers.complete(41, true).unwrap().cleanup.is_some(),
            "the newest live callback must own native identity restoration"
        );
        barriers.release_toplevel(77);
        assert!(
            barriers.complete(40, false).unwrap().cleanup.is_none(),
            "role teardown must not replay a restore already emitted by the newest callback"
        );
    }

    #[test]
    fn released_role_allows_only_one_transient_restore_callback() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
            zaura_surface_id: 55,
            wl_surface_guest_id: 10,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup));
        barriers.release_toplevel(77);

        assert!(
            barriers.complete(40, false).unwrap().cleanup.is_some(),
            "the first stale callback owns the released surface restore"
        );
        assert!(
            barriers.complete(41, false).unwrap().cleanup.is_none(),
            "a second stale callback must not replay native identity restore"
        );
    }

    #[test]
    fn cancelled_transient_registration_restores_displaced_claim() {
        let mut barriers = PlacementBarrierRegistry::default();
        let cleanup = Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
            zaura_surface_id: 55,
            wl_surface_guest_id: 10,
        });
        assert!(barriers.register(40, 77, cleanup.clone()));
        assert!(barriers.register(41, 77, cleanup.clone()));
        assert!(barriers.complete(41, true).unwrap().cleanup.is_some());
        assert!(barriers.register(42, 77, cleanup));
        assert!(barriers.cancel(42));
        barriers.release_toplevel(77);
        assert!(
            barriers.complete(40, false).unwrap().cleanup.is_none(),
            "cancelling the replacement must preserve the prior one-shot claim"
        );
    }
}
