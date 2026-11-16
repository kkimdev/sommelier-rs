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

//! State and lifecycle invariants for compositor-owned window placement.
//!
//! [`WindowPlacementState`] is the single owner of one connection's state
//! for the opt-in ARC bounds policy: the per-surface ARC policy identities,
//! the host Aura surface/toplevel associations, the host-stream sync barriers
//! that order `zaura_toplevel.set_window_bounds`, and the configures held
//! behind those barriers. Whether the policy is active is decided once per
//! process by `crate::placement_config::PlacementConfig`, which hands this
//! state the process's ARC task-ID block. The maps are private so handlers
//! cannot update one side of an association, or retire a toplevel, without
//! the matching cleanup.
//!
//! Wire encoding stays in the protocol handlers; this type only answers
//! ownership questions and returns the IDs whose host teardown the caller must
//! perform.

use std::collections::{HashMap, HashSet};

use crate::arc_task_ids::ArcIdAllocator;

/// Namespace used by the opt-in bounds-policy workaround. ChromeOS's Exo
/// security delegate recognizes the ARC task-form identity and allows Aura
/// bounds requests for it. The suffix is generated per surface; sharing one
/// fabricated ID across every window would merge their task identity. The
/// `.session.*` namespace is reserved for restore/ghost sessions and must not
/// be fabricated by the proxy.
pub(crate) const ARC_APPLICATION_ID_PREFIX: &str = "org.chromium.arc.";

/// Latest host configure retained while a window-bounds sync barrier is
/// active.  The barrier establishes ordering; flushing only this newest event
/// avoids forwarding stale intermediate configures while still delivering the
/// final state once the host has processed the placement request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWindowBoundsConfigure {
    pub guest_xdg_toplevel_id: u32,
    pub width: i32,
    pub height: i32,
    pub states: Vec<u8>,
}

/// The serial-bearing `xdg_surface.configure` paired with a retained
/// toplevel configure while a window-bounds sync barrier is active.
///
/// Wayland requires the toplevel event to precede this event.  Keep the pair
/// separate because either event may arrive first from the host stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingWindowBoundsSurfaceConfigure {
    pub guest_xdg_surface_id: u32,
    pub serial: u32,
}

/// Configures released when the newest barrier of a toplevel completes.
///
/// Both fields are `None` when the completed callback was superseded by a
/// newer barrier, or when its toplevel was already released.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HeldWindowBoundsConfigures {
    pub configure: Option<PendingWindowBoundsConfigure>,
    pub surface_configure: Option<PendingWindowBoundsSurfaceConfigure>,
}

/// All mutable state owned by the window-placement feature for one client
/// connection.
#[derive(Debug)]
pub(crate) struct WindowPlacementState {
    /// The process's ARC task-ID block, from `PlacementConfig`. `Some` is
    /// the experimental host-policy workaround being active: the host
    /// surface is identified as an ARC window so Exo permits
    /// `zaura_toplevel.set_window_bounds`.
    arc_id_allocator: Option<&'static ArcIdAllocator>,
    /// Guest wl_surface ID → ARC policy identity, stable for the surface's
    /// lifetime.
    arc_application_ids: HashMap<u32, String>,
    /// Guest surfaces whose ARC policy identity has already been sent to the
    /// paired host Aura surface. Keeping this separate from the allocated
    /// identity avoids repeating the metadata request on every placement.
    arc_application_ids_applied: HashSet<u32>,
    /// Host wl_surface ID → host zaura_surface ID. XDG, GTK, and placement
    /// metadata share this one Aura child per surface.
    aura_surface_by_wl_surface: HashMap<u32, u32>,
    /// Guest xdg_toplevel ID → host zaura_toplevel ID.
    aura_toplevel_by_xdg_toplevel: HashMap<u32, u32>,
    /// Reverse of `aura_toplevel_by_xdg_toplevel`, used to route host Aura
    /// toplevel events back to the guest role.
    xdg_toplevel_by_aura_toplevel: HashMap<u32, u32>,
    /// Host sync callback ID → Aura toplevel ID. A callback stays here until
    /// its terminal `done`, even after it is superseded or its toplevel is
    /// released, so its host ID is never mistaken for an ordinary callback.
    barrier_toplevels: HashMap<u32, u32>,
    /// The newest barrier for each live Aura toplevel; only it gates
    /// configure delivery.
    active_barriers: HashMap<u32, u32>,
    /// Latest toplevel configure held while the active barrier is live.
    held_configures: HashMap<u32, PendingWindowBoundsConfigure>,
    /// Latest serial-bearing surface configure held while the active barrier
    /// is live.
    held_surface_configures: HashMap<u32, PendingWindowBoundsSurfaceConfigure>,
}

impl WindowPlacementState {
    /// Create one connection's placement state.
    ///
    /// # Arguments
    ///
    /// * `arc_id_allocator` - `PlacementConfig::arc_id_allocator`: the
    ///   process's ARC task-ID block while the ARC bounds policy is active,
    ///   `None` while it is off.
    pub(crate) fn new(arc_id_allocator: Option<&'static ArcIdAllocator>) -> Self {
        Self {
            arc_id_allocator,
            arc_application_ids: HashMap::new(),
            arc_application_ids_applied: HashSet::new(),
            aura_surface_by_wl_surface: HashMap::new(),
            aura_toplevel_by_xdg_toplevel: HashMap::new(),
            xdg_toplevel_by_aura_toplevel: HashMap::new(),
            barrier_toplevels: HashMap::new(),
            active_barriers: HashMap::new(),
            held_configures: HashMap::new(),
            held_surface_configures: HashMap::new(),
        }
    }

    /// Return whether the opt-in ARC bounds placement policy is active.
    pub(crate) const fn uses_arc_bounds(&self) -> bool {
        self.arc_id_allocator.is_some()
    }

    /// Verify the ownership graph after every mutation in debug/test builds,
    /// so a lifecycle change fails at the mutation site instead of silently
    /// corrupting a later destructor path.
    fn debug_assert_consistent(&self) {
        debug_assert!(self.aura_toplevel_by_xdg_toplevel.iter().all(
            |(xdg_toplevel_guest_id, zaura_toplevel_host_id)| {
                self.xdg_toplevel_by_aura_toplevel
                    .get(zaura_toplevel_host_id)
                    == Some(xdg_toplevel_guest_id)
            }
        ));
        debug_assert_eq!(
            self.aura_toplevel_by_xdg_toplevel.len(),
            self.xdg_toplevel_by_aura_toplevel.len()
        );
        debug_assert!(self.active_barriers.iter().all(
            |(zaura_toplevel_host_id, callback_host_id)| {
                self.barrier_toplevels.get(callback_host_id) == Some(zaura_toplevel_host_id)
            }
        ));
        debug_assert!(self
            .held_configures
            .keys()
            .chain(self.held_surface_configures.keys())
            .all(|zaura_toplevel_host_id| self
                .active_barriers
                .contains_key(zaura_toplevel_host_id)));
        debug_assert!(self
            .arc_application_ids_applied
            .iter()
            .all(|wl_surface_guest_id| self.arc_application_ids.contains_key(wl_surface_guest_id)));
    }

    // ── ARC policy identities ─────────────────────────────────────────────

    /// Return the stable ARC policy identity for one guest surface,
    /// allocating it on first use.
    ///
    /// The identity is scoped to the surface rather than the application ID:
    /// two windows from one application must remain distinct.
    ///
    /// # Returns
    ///
    /// `None` when the ARC bounds policy is off or the process's block is
    /// exhausted (which the allocator logs once).
    pub(crate) fn arc_application_id(&mut self, wl_surface_guest_id: u32) -> Option<String> {
        let allocator = self.arc_id_allocator?;
        if let Some(application_id) = self.arc_application_ids.get(&wl_surface_guest_id) {
            return Some(application_id.clone());
        }
        let id = allocator.allocate()?;
        let application_id = format!("{ARC_APPLICATION_ID_PREFIX}{id}");
        self.arc_application_ids
            .insert(wl_surface_guest_id, application_id.clone());
        Some(application_id)
    }

    /// Return whether the surface's ARC identity has been queued to its Aura
    /// surface.
    pub(crate) fn arc_application_id_applied(&self, wl_surface_guest_id: u32) -> bool {
        self.arc_application_ids_applied
            .contains(&wl_surface_guest_id)
    }

    /// Record that the surface's ARC identity request has been queued.
    pub(crate) fn mark_arc_application_id_applied(&mut self, wl_surface_guest_id: u32) {
        self.arc_application_ids_applied.insert(wl_surface_guest_id);
        self.debug_assert_consistent();
    }

    #[cfg(test)]
    pub(crate) fn has_arc_application_ids(&self) -> bool {
        !self.arc_application_ids.is_empty()
    }

    // ── Aura surfaces ─────────────────────────────────────────────────────

    /// Return the Aura surface associated with a host wl_surface.
    pub(crate) fn aura_surface_for_wl_surface(&self, wl_surface_host_id: u32) -> Option<u32> {
        self.aura_surface_by_wl_surface
            .get(&wl_surface_host_id)
            .copied()
    }

    /// Record the Aura surface created for a host wl_surface.
    pub(crate) fn remember_aura_surface(
        &mut self,
        wl_surface_host_id: u32,
        zaura_surface_host_id: u32,
    ) {
        let previous = self
            .aura_surface_by_wl_surface
            .insert(wl_surface_host_id, zaura_surface_host_id);
        debug_assert!(
            previous.is_none_or(|previous| previous == zaura_surface_host_id),
            "replacing a live Aura surface would orphan its destructor"
        );
    }

    /// Remove all placement state owned by a destroyed guest wl_surface.
    ///
    /// The guest ID owns the ARC identity while the host ID owns the Aura
    /// surface association; the destructor is the one boundary that knows
    /// both.
    ///
    /// # Returns
    ///
    /// The Aura surface whose host teardown the caller must perform.
    #[must_use = "the returned Aura surface ID identifies host teardown work"]
    pub(crate) fn release_surface(
        &mut self,
        wl_surface_guest_id: u32,
        wl_surface_host_id: u32,
    ) -> Option<u32> {
        self.arc_application_ids.remove(&wl_surface_guest_id);
        self.arc_application_ids_applied
            .remove(&wl_surface_guest_id);
        let zaura_surface_host_id = self.aura_surface_by_wl_surface.remove(&wl_surface_host_id);
        self.debug_assert_consistent();
        zaura_surface_host_id
    }

    #[cfg(test)]
    pub(crate) fn aura_surface_count(&self) -> usize {
        self.aura_surface_by_wl_surface.len()
    }

    // ── Aura toplevels ────────────────────────────────────────────────────

    /// Return the Aura toplevel associated with a guest xdg_toplevel.
    pub(crate) fn aura_toplevel_for_xdg_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.aura_toplevel_by_xdg_toplevel
            .get(&xdg_toplevel_guest_id)
            .copied()
    }

    /// Resolve a host Aura toplevel event back to its guest xdg_toplevel.
    pub(crate) fn xdg_toplevel_for_aura_toplevel(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<u32> {
        self.xdg_toplevel_by_aura_toplevel
            .get(&zaura_toplevel_host_id)
            .copied()
    }

    /// Record the one-to-one xdg_toplevel ↔ Aura toplevel association.
    pub(crate) fn remember_aura_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        zaura_toplevel_host_id: u32,
    ) {
        debug_assert!(
            self.aura_toplevel_for_xdg_toplevel(xdg_toplevel_guest_id)
                .is_none_or(|existing| existing == zaura_toplevel_host_id)
                && self
                    .xdg_toplevel_for_aura_toplevel(zaura_toplevel_host_id)
                    .is_none_or(|existing| existing == xdg_toplevel_guest_id),
            "replacing a live Aura toplevel would orphan its destructor"
        );
        self.aura_toplevel_by_xdg_toplevel
            .insert(xdg_toplevel_guest_id, zaura_toplevel_host_id);
        self.xdg_toplevel_by_aura_toplevel
            .insert(zaura_toplevel_host_id, xdg_toplevel_guest_id);
        self.debug_assert_consistent();
    }

    /// Remove the Aura toplevel of a guest xdg_toplevel with its placement
    /// state.
    ///
    /// A toplevel can be destroyed while a placement barrier is still in
    /// flight. Its callback remains owned until the terminal host event, but
    /// it no longer gates configures, and configures held for the released
    /// toplevel are discarded.
    ///
    /// # Returns
    ///
    /// The Aura toplevel whose host teardown the caller must perform.
    #[must_use = "the returned Aura toplevel ID identifies host teardown work"]
    pub(crate) fn take_aura_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let zaura_toplevel_host_id = self
            .aura_toplevel_by_xdg_toplevel
            .remove(&xdg_toplevel_guest_id)?;
        self.xdg_toplevel_by_aura_toplevel
            .remove(&zaura_toplevel_host_id);
        self.active_barriers.remove(&zaura_toplevel_host_id);
        self.held_configures.remove(&zaura_toplevel_host_id);
        self.held_surface_configures.remove(&zaura_toplevel_host_id);
        self.debug_assert_consistent();
        Some(zaura_toplevel_host_id)
    }

    #[cfg(test)]
    pub(crate) fn aura_toplevel_count(&self) -> usize {
        self.aura_toplevel_by_xdg_toplevel.len()
    }

    // ── Bounds barriers and held configures ───────────────────────────────

    /// Register a newly queued host sync callback as the newest barrier for
    /// an Aura toplevel. The previous barrier stays owned until its terminal
    /// event but no longer gates configures.
    pub(crate) fn register_barrier(&mut self, callback_host_id: u32, zaura_toplevel_host_id: u32) {
        debug_assert!(
            !self.barrier_toplevels.contains_key(&callback_host_id),
            "a live barrier callback ID cannot be reused"
        );
        self.active_barriers
            .insert(zaura_toplevel_host_id, callback_host_id);
        self.barrier_toplevels
            .insert(callback_host_id, zaura_toplevel_host_id);
        self.debug_assert_consistent();
    }

    /// Retire a barrier callback at its terminal `done` event.
    ///
    /// # Returns
    ///
    /// `None` when `callback_host_id` is not a placement barrier. Otherwise
    /// the configures held behind it, which are empty unless it was still the
    /// newest barrier of its toplevel.
    #[must_use = "held configures must be delivered to the guest"]
    pub(crate) fn complete_barrier(
        &mut self,
        callback_host_id: u32,
    ) -> Option<HeldWindowBoundsConfigures> {
        let zaura_toplevel_host_id = self.barrier_toplevels.remove(&callback_host_id)?;
        let mut held = HeldWindowBoundsConfigures::default();
        if self.active_barriers.get(&zaura_toplevel_host_id) == Some(&callback_host_id) {
            self.active_barriers.remove(&zaura_toplevel_host_id);
            held.configure = self.held_configures.remove(&zaura_toplevel_host_id);
            held.surface_configure = self.held_surface_configures.remove(&zaura_toplevel_host_id);
        }
        self.debug_assert_consistent();
        Some(held)
    }

    /// Return whether a placement barrier currently gates this toplevel.
    pub(crate) fn has_active_barrier(&self, zaura_toplevel_host_id: u32) -> bool {
        self.active_barriers.contains_key(&zaura_toplevel_host_id)
    }

    /// Hold the newest toplevel configure behind the active barrier.
    ///
    /// # Returns
    ///
    /// `false`, without storing anything, when no barrier gates the toplevel
    /// and the caller must deliver the configure itself.
    #[must_use = "an unheld configure must be delivered by the caller"]
    pub(crate) fn hold_configure(
        &mut self,
        zaura_toplevel_host_id: u32,
        configure: PendingWindowBoundsConfigure,
    ) -> bool {
        if !self.has_active_barrier(zaura_toplevel_host_id) {
            return false;
        }
        self.held_configures
            .insert(zaura_toplevel_host_id, configure);
        true
    }

    /// Hold the newest serial-bearing surface configure behind the active
    /// barrier. Returns `false` under the same rule as [`Self::hold_configure`].
    #[must_use = "an unheld configure must be delivered by the caller"]
    pub(crate) fn hold_surface_configure(
        &mut self,
        zaura_toplevel_host_id: u32,
        configure: PendingWindowBoundsSurfaceConfigure,
    ) -> bool {
        if !self.has_active_barrier(zaura_toplevel_host_id) {
            return false;
        }
        self.held_surface_configures
            .insert(zaura_toplevel_host_id, configure);
        true
    }

    #[cfg(test)]
    pub(crate) fn barrier_for_callback(&self, callback_host_id: u32) -> Option<u32> {
        self.barrier_toplevels.get(&callback_host_id).copied()
    }

    #[cfg(test)]
    pub(crate) fn active_barrier_for_toplevel(&self, zaura_toplevel_host_id: u32) -> Option<u32> {
        self.active_barriers.get(&zaura_toplevel_host_id).copied()
    }

    #[cfg(test)]
    pub(crate) fn barrier_count(&self) -> usize {
        self.barrier_toplevels.len()
    }

    #[cfg(test)]
    pub(crate) fn has_held_configure(&self, zaura_toplevel_host_id: u32) -> bool {
        self.held_configures.contains_key(&zaura_toplevel_host_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(guest_xdg_toplevel_id: u32) -> PendingWindowBoundsConfigure {
        PendingWindowBoundsConfigure {
            guest_xdg_toplevel_id,
            width: 10,
            height: 20,
            states: Vec::new(),
        }
    }

    fn surface_configure(serial: u32) -> PendingWindowBoundsSurfaceConfigure {
        PendingWindowBoundsSurfaceConfigure {
            guest_xdg_surface_id: 5,
            serial,
        }
    }

    /// State for a connection with the ARC bounds policy active.
    fn active() -> WindowPlacementState {
        WindowPlacementState::new(Some(ArcIdAllocator::block_for_test()))
    }

    /// State for a connection with the ARC bounds policy off.
    fn inactive() -> WindowPlacementState {
        WindowPlacementState::new(None)
    }

    #[test]
    fn disabled_policy_cannot_allocate_arc_identities() {
        let mut state = inactive();
        assert!(!state.uses_arc_bounds());
        assert_eq!(state.arc_application_id(10), None);
        assert!(!state.has_arc_application_ids());
    }

    #[test]
    fn exhausted_allocator_fails_closed() {
        let mut state = WindowPlacementState::new(Some(ArcIdAllocator::exhausted_for_test()));
        assert!(state.uses_arc_bounds());
        assert_eq!(state.arc_application_id(10), None);
        assert!(!state.has_arc_application_ids());
    }

    #[test]
    fn surface_release_retires_identity_applied_flag_and_aura_surface() {
        let mut state = active();
        let first = state.arc_application_id(10).expect("ARC identity");
        assert_eq!(state.arc_application_id(10), Some(first.clone()));
        state.mark_arc_application_id_applied(10);
        state.remember_aura_surface(50, 60);
        assert_eq!(state.aura_surface_for_wl_surface(50), Some(60));

        assert_eq!(state.release_surface(10, 50), Some(60));
        assert_eq!(state.aura_surface_for_wl_surface(50), None);
        assert!(!state.arc_application_id_applied(10));
        assert!(!state.has_arc_application_ids());
        assert_eq!(state.release_surface(10, 50), None, "release is idempotent");

        // Identities are never recycled: a reused guest ID gets a new one.
        let second = state.arc_application_id(10).expect("ARC identity");
        assert_ne!(first, second);
    }

    #[test]
    fn toplevel_association_resolves_both_directions_until_taken() {
        let mut state = inactive();
        state.remember_aura_toplevel(10, 70);
        assert_eq!(state.aura_toplevel_for_xdg_toplevel(10), Some(70));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), Some(10));

        assert_eq!(state.take_aura_toplevel(10), Some(70));
        assert_eq!(state.aura_toplevel_for_xdg_toplevel(10), None);
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), None);
        assert_eq!(state.take_aura_toplevel(10), None, "release is idempotent");
    }

    #[test]
    fn taking_toplevel_clears_gate_and_held_configures_but_keeps_callback_owned() {
        let mut state = inactive();
        state.remember_aura_toplevel(10, 77);
        state.register_barrier(40, 77);
        assert!(state.hold_configure(77, configure(10)));
        assert!(state.hold_surface_configure(77, surface_configure(3)));

        assert_eq!(state.take_aura_toplevel(10), Some(77));
        assert!(!state.has_active_barrier(77));
        assert!(!state.has_held_configure(77));
        assert_eq!(state.barrier_for_callback(40), Some(77));
        assert_eq!(
            state.complete_barrier(40),
            Some(HeldWindowBoundsConfigures::default()),
            "a released toplevel's held configures must not be delivered"
        );
        assert_eq!(state.barrier_count(), 0);
    }

    #[test]
    fn configures_are_held_only_behind_an_active_barrier() {
        let mut state = inactive();
        state.remember_aura_toplevel(10, 77);
        assert!(!state.hold_configure(77, configure(10)));
        assert!(!state.hold_surface_configure(77, surface_configure(1)));

        state.register_barrier(40, 77);
        assert!(state.hold_configure(77, configure(10)));
        let newest = PendingWindowBoundsConfigure {
            width: 30,
            ..configure(10)
        };
        assert!(state.hold_configure(77, newest.clone()));
        assert!(state.hold_surface_configure(77, surface_configure(2)));

        assert_eq!(
            state.complete_barrier(40),
            Some(HeldWindowBoundsConfigures {
                configure: Some(newest),
                surface_configure: Some(surface_configure(2)),
            })
        );
        assert!(!state.has_active_barrier(77));
    }

    #[test]
    fn stale_barrier_completion_keeps_newer_barrier_and_held_configures() {
        let mut state = inactive();
        state.remember_aura_toplevel(10, 77);
        state.register_barrier(40, 77);
        state.register_barrier(41, 77);
        assert!(state.hold_configure(77, configure(10)));

        assert_eq!(
            state.complete_barrier(40),
            Some(HeldWindowBoundsConfigures::default())
        );
        assert_eq!(state.active_barrier_for_toplevel(77), Some(41));
        assert!(state.has_held_configure(77));

        let held = state.complete_barrier(41).expect("placement barrier");
        assert_eq!(held.configure, Some(configure(10)));
        assert_eq!(state.barrier_count(), 0);
    }

    #[test]
    fn unknown_callback_is_not_a_placement_barrier() {
        let mut state = inactive();
        assert_eq!(state.complete_barrier(40), None);
        state.remember_aura_toplevel(10, 77);
        state.register_barrier(40, 77);
        assert!(state.complete_barrier(40).is_some());
        assert_eq!(state.complete_barrier(40), None, "done is terminal");
    }
}
