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

//! State and invariants for compositor-owned window placement shortcuts.
//!
//! Placement is deliberately kept behind this type instead of exposing
//! independent flags and maps on [`Context`](super::Context). The handlers can
//! observe or advance placement state only through the methods below, which
//! keeps backend selection, origin prediction, barrier retirement, and ARC
//! application-ID lifetime consistent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use log::warn;

/// Application-ID policy used for compositor-owned window operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowHostPolicy {
    /// Keep the normal Crostini/guest application namespace.
    #[default]
    Guest,
    /// Use the ARC-session namespace required by the direct bounds policy.
    Arc,
}

/// Geometry operation used for compositor-owned window shortcuts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowGeometryMethod {
    /// Do not consume or execute Sommelier-owned window shortcuts.
    #[default]
    None,
    /// Send `zaura_toplevel.set_window_bounds`.
    Bounds,
    /// Send the unsupported position-only self-parent probe.
    SelfParent,
}

/// Independent host-policy and geometry selections for window shortcuts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WindowPlacementMode {
    pub(crate) host_policy: WindowHostPolicy,
    pub(crate) geometry_method: WindowGeometryMethod,
}

impl WindowPlacementMode {
    pub(crate) const fn new(
        host_policy: WindowHostPolicy,
        geometry_method: WindowGeometryMethod,
    ) -> Self {
        Self {
            host_policy,
            geometry_method,
        }
    }

    pub(crate) const fn disabled() -> Self {
        Self::new(WindowHostPolicy::Guest, WindowGeometryMethod::None)
    }

    /// Resolve the legacy environment flags for compatibility paths.
    ///
    /// The production binary uses explicit CLI values. Keeping this adapter
    /// preserves deterministic behavior for older in-process callers while
    /// making the two axes explicit internally.
    pub(crate) const fn from_flags(arc_bounds_enabled: bool, self_parent_enabled: bool) -> Self {
        let host_policy = if arc_bounds_enabled {
            WindowHostPolicy::Arc
        } else {
            WindowHostPolicy::Guest
        };
        let geometry_method = if arc_bounds_enabled {
            WindowGeometryMethod::Bounds
        } else if self_parent_enabled {
            WindowGeometryMethod::SelfParent
        } else {
            WindowGeometryMethod::None
        };
        Self::new(host_policy, geometry_method)
    }

    /// Resolve the backend from the process environment once at startup.
    pub(crate) fn from_environment() -> Self {
        let arc_bounds_enabled = std::env::var_os("SOMMELIER_WINDOW_BOUNDS_AS_ARC").is_some();
        let self_parent_enabled = std::env::var_os("SOMMELIER_WINDOW_BOUNDS_SELF_PARENT").is_some();
        let mode = Self::from_flags(arc_bounds_enabled, self_parent_enabled);

        if self_parent_enabled {
            warn!(
                "SOMMELIER_WINDOW_BOUNDS_SELF_PARENT is experimental and \
                 position-only; it cannot resize windows and may be unstable on \
                 custom ChromeOS hosts"
            );
        }
        if arc_bounds_enabled && self_parent_enabled {
            warn!(
                "Both window-placement backends are enabled; \
                 SOMMELIER_WINDOW_BOUNDS_AS_ARC takes precedence"
            );
        }

        mode
    }

    /// Return whether this geometry method consumes placement shortcuts.
    pub(crate) const fn handles_shortcuts(self) -> bool {
        !matches!(self.geometry_method, WindowGeometryMethod::None)
    }

    /// Return whether the ARC application namespace is selected.
    pub(crate) const fn uses_arc_policy(self) -> bool {
        matches!(self.host_policy, WindowHostPolicy::Arc)
    }

    /// Return whether direct Aura bounds are selected.
    pub(crate) const fn uses_bounds(self) -> bool {
        matches!(self.geometry_method, WindowGeometryMethod::Bounds)
    }

    /// Return whether this backend runs the position-only self-parent probe.
    pub(crate) const fn uses_self_parent(self) -> bool {
        matches!(self.geometry_method, WindowGeometryMethod::SelfParent)
    }
}

/// Application-ID namespace used by the ARC bounds backend.
pub(crate) const ARC_SESSION_APPLICATION_ID_PREFIX: &str = "org.chromium.arc.session";

// ARC parses the numeric suffix with a signed 32-bit `%d`. Keep generated
// values positive and well below INT32_MAX while partitioning the range by
// process ID and a process-wide serial. The serial is intentionally global so
// multiple Context instances in one process cannot reuse an ID concurrently.
const ARC_SESSION_ID_BASE: u32 = 1_000_000_000;
const ARC_SESSION_ID_PID_MASK: u32 = (1 << 14) - 1;
const ARC_SESSION_ID_SERIAL_MASK: u32 = (1 << 14) - 1;
const ARC_SESSION_ID_SERIAL_SLOT_COUNT: u32 = ARC_SESSION_ID_SERIAL_MASK + 1;
static NEXT_ARC_SESSION_ID_SERIAL: AtomicU32 = AtomicU32::new(0);

fn next_arc_session_id() -> u32 {
    let pid_component = std::process::id() & ARC_SESSION_ID_PID_MASK;
    let serial =
        NEXT_ARC_SESSION_ID_SERIAL.fetch_add(1, Ordering::Relaxed) & ARC_SESSION_ID_SERIAL_MASK;
    ARC_SESSION_ID_BASE + 1 + pid_component * ARC_SESSION_ID_SERIAL_SLOT_COUNT + serial
}

#[derive(Debug, Default)]
struct ToplevelPlacementState {
    /// Last authoritative or predicted screen-space origin.
    origin: Option<(i32, i32)>,
    /// Newest self-parent target waiting for a matching host notification.
    pending_origin: Option<(i32, i32)>,
}

/// All mutable state owned by the window-placement feature.
///
/// The maps are private by design. In particular, callers must not update an
/// origin without also applying the pending-target rule, must not mutate one
/// side of an association without its reverse owner, and must not remove a
/// barrier mapping without retiring its active-to-toplevel entry.
#[derive(Debug)]
pub(crate) struct WindowPlacementState {
    mode: WindowPlacementMode,
    arc_session_application_ids: HashMap<u32, String>,
    aura_surface_by_wl_surface: HashMap<u32, u32>,
    wl_surface_by_aura_surface: HashMap<u32, u32>,
    aura_toplevel_by_xdg_toplevel: HashMap<u32, u32>,
    xdg_toplevel_by_aura_toplevel: HashMap<u32, u32>,
    toplevels: HashMap<u32, ToplevelPlacementState>,
    barrier_to_toplevel: HashMap<u32, u32>,
    active_barrier_by_toplevel: HashMap<u32, u32>,
}

impl WindowPlacementState {
    /// Verify the ownership graph while it is still private to this type.
    ///
    /// The reverse maps intentionally make teardown idempotent and stale host
    /// events harmless. Keeping these checks next to every mutator means a
    /// future lifecycle change fails at the mutation site in debug/test
    /// builds instead of silently corrupting a later destructor path.
    #[inline]
    fn debug_assert_consistent(&self) {
        debug_assert!(self.aura_surface_by_wl_surface.iter().all(
            |(wl_surface_host_id, zaura_surface_host_id)| {
                self.wl_surface_by_aura_surface.get(zaura_surface_host_id)
                    == Some(wl_surface_host_id)
            }
        ));
        debug_assert!(self.wl_surface_by_aura_surface.iter().all(
            |(zaura_surface_host_id, wl_surface_host_id)| {
                self.aura_surface_by_wl_surface.get(wl_surface_host_id)
                    == Some(zaura_surface_host_id)
            }
        ));
        debug_assert!(self.aura_toplevel_by_xdg_toplevel.iter().all(
            |(xdg_toplevel_guest_id, zaura_toplevel_host_id)| {
                self.xdg_toplevel_by_aura_toplevel
                    .get(zaura_toplevel_host_id)
                    == Some(xdg_toplevel_guest_id)
            }
        ));
        debug_assert!(self.xdg_toplevel_by_aura_toplevel.iter().all(
            |(zaura_toplevel_host_id, xdg_toplevel_guest_id)| {
                self.aura_toplevel_by_xdg_toplevel
                    .get(xdg_toplevel_guest_id)
                    == Some(zaura_toplevel_host_id)
            }
        ));
        debug_assert!(self.toplevels.keys().all(|zaura_toplevel_host_id| {
            self.xdg_toplevel_by_aura_toplevel
                .contains_key(zaura_toplevel_host_id)
        }));
        debug_assert!(self.active_barrier_by_toplevel.iter().all(
            |(zaura_toplevel_host_id, callback_host_id)| {
                self.barrier_to_toplevel.get(callback_host_id) == Some(zaura_toplevel_host_id)
                    && self
                        .xdg_toplevel_by_aura_toplevel
                        .contains_key(zaura_toplevel_host_id)
            }
        ));
    }

    /// Create empty placement state for one connection.
    pub(crate) fn new(mode: WindowPlacementMode) -> Self {
        Self {
            mode,
            arc_session_application_ids: HashMap::new(),
            aura_surface_by_wl_surface: HashMap::new(),
            wl_surface_by_aura_surface: HashMap::new(),
            aura_toplevel_by_xdg_toplevel: HashMap::new(),
            xdg_toplevel_by_aura_toplevel: HashMap::new(),
            toplevels: HashMap::new(),
            barrier_to_toplevel: HashMap::new(),
            active_barrier_by_toplevel: HashMap::new(),
        }
    }

    /// Return the most recent mode-independent shortcut capability.
    pub(crate) const fn handles_shortcuts(&self) -> bool {
        self.mode.handles_shortcuts()
    }

    /// Return whether this connection uses the ARC application namespace.
    pub(crate) const fn uses_arc_policy(&self) -> bool {
        self.mode.uses_arc_policy()
    }

    /// Return whether this connection uses direct Aura bounds.
    pub(crate) const fn uses_bounds(&self) -> bool {
        self.mode.uses_bounds()
    }

    /// Return whether this connection uses the experimental self-parent path.
    pub(crate) const fn uses_self_parent(&self) -> bool {
        self.mode.uses_self_parent()
    }

    /// Return the stable ARC application ID for one guest wl_surface.
    ///
    /// The mapping lasts until [`Self::remove_surface`] is called, so XDG and
    /// GTK metadata paths cannot disagree while the surface is alive. The
    /// result is `None` when the ARC host policy is not selected.
    pub(crate) fn arc_session_application_id(
        &mut self,
        wl_surface_guest_id: u32,
    ) -> Option<String> {
        if !self.uses_arc_policy() {
            return None;
        }
        Some(
            self.arc_session_application_ids
                .entry(wl_surface_guest_id)
                .or_insert_with(|| {
                    format!(
                        "{ARC_SESSION_APPLICATION_ID_PREFIX}.{}",
                        next_arc_session_id()
                    )
                })
                .clone(),
        )
    }

    /// Remove all placement metadata owned by a destroyed guest surface.
    ///
    /// The guest ID owns the ARC application ID while the host ID owns the
    /// Aura-surface association. Requiring both IDs keeps their lifetimes
    /// coupled at the only teardown boundary that has authoritative ownership
    /// of both objects.
    #[must_use = "the returned Aura surface ID identifies host teardown work"]
    pub(crate) fn remove_surface(
        &mut self,
        wl_surface_guest_id: u32,
        wl_surface_host_id: u32,
    ) -> Option<u32> {
        self.arc_session_application_ids
            .remove(&wl_surface_guest_id);
        let zaura_surface_host_id = self.aura_surface_by_wl_surface.remove(&wl_surface_host_id);
        if let Some(zaura_surface_host_id) = zaura_surface_host_id {
            self.wl_surface_by_aura_surface
                .remove(&zaura_surface_host_id);
        }
        self.debug_assert_consistent();
        zaura_surface_host_id
    }

    /// Return the Aura surface associated with a host wl_surface.
    pub(crate) fn aura_surface_for_wl_surface(&self, wl_surface_host_id: u32) -> Option<u32> {
        self.aura_surface_by_wl_surface
            .get(&wl_surface_host_id)
            .copied()
    }

    /// Record the one-to-one Aura surface association for a host wl_surface.
    ///
    /// Returns `false` when either side is already associated with a
    /// different object; silently replacing either entry would orphan a live
    /// host object and make its destructor impossible to route.
    #[must_use = "the association may conflict with an existing live object"]
    pub(crate) fn remember_aura_surface(
        &mut self,
        wl_surface_host_id: u32,
        zaura_surface_host_id: u32,
    ) -> bool {
        if self
            .aura_surface_by_wl_surface
            .get(&wl_surface_host_id)
            .is_some_and(|existing| *existing != zaura_surface_host_id)
            || self
                .wl_surface_by_aura_surface
                .get(&zaura_surface_host_id)
                .is_some_and(|existing| *existing != wl_surface_host_id)
        {
            return false;
        }
        self.aura_surface_by_wl_surface
            .insert(wl_surface_host_id, zaura_surface_host_id);
        self.wl_surface_by_aura_surface
            .insert(zaura_surface_host_id, wl_surface_host_id);
        self.debug_assert_consistent();
        true
    }

    /// Return the Aura toplevel associated with a guest xdg_toplevel.
    pub(crate) fn aura_toplevel_for_xdg_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.aura_toplevel_by_xdg_toplevel
            .get(&xdg_toplevel_guest_id)
            .copied()
    }

    /// Record a one-to-one xdg_toplevel ↔ Aura toplevel association.
    ///
    /// Returns `false` when either side is already associated with a
    /// different object. Replacing a live association would orphan the old
    /// host object and make its destructor impossible to route.
    #[must_use = "the association may conflict with an existing live object"]
    pub(crate) fn remember_aura_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        zaura_toplevel_host_id: u32,
    ) -> bool {
        let existing_host = self
            .aura_toplevel_by_xdg_toplevel
            .get(&xdg_toplevel_guest_id)
            .copied();
        let existing_guest = self
            .xdg_toplevel_by_aura_toplevel
            .get(&zaura_toplevel_host_id)
            .copied();
        if existing_host.is_some_and(|existing| existing != zaura_toplevel_host_id)
            || existing_guest.is_some_and(|existing| existing != xdg_toplevel_guest_id)
        {
            return false;
        }
        self.aura_toplevel_by_xdg_toplevel
            .insert(xdg_toplevel_guest_id, zaura_toplevel_host_id);
        self.xdg_toplevel_by_aura_toplevel
            .insert(zaura_toplevel_host_id, xdg_toplevel_guest_id);
        self.debug_assert_consistent();
        true
    }

    /// Remove and return the Aura toplevel for a guest xdg_toplevel.
    ///
    /// Removing the association also retires the toplevel's origin and active
    /// barrier state. Older barriers remain callback-owned until their
    /// terminal host lifecycle event.
    #[must_use = "the returned Aura toplevel ID identifies host teardown work"]
    pub(crate) fn take_aura_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let zaura_toplevel_host_id = self
            .aura_toplevel_by_xdg_toplevel
            .remove(&xdg_toplevel_guest_id)?;
        self.xdg_toplevel_by_aura_toplevel
            .remove(&zaura_toplevel_host_id);
        self.release_toplevel(zaura_toplevel_host_id);
        self.debug_assert_consistent();
        Some(zaura_toplevel_host_id)
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

    /// Return the latest screen-space origin for an Aura toplevel.
    pub(crate) fn origin(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.origin)
    }

    #[cfg(test)]
    pub(crate) fn pending_origin(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.pending_origin)
    }

    /// Record a host origin if it is authoritative for the current request.
    ///
    /// Returns `false` for an intermediate origin that conflicts with a
    /// pending self-parent target or for an Aura toplevel that is no longer
    /// associated with a live guest xdg_toplevel. The caller can log either
    /// event, but cannot accidentally recreate state after teardown or
    /// overwrite the origin used by the next shortcut.
    #[must_use = "the origin was rejected or is still pending"]
    pub(crate) fn record_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
        origin: (i32, i32),
    ) -> bool {
        if !self
            .xdg_toplevel_by_aura_toplevel
            .contains_key(&zaura_toplevel_host_id)
        {
            return false;
        }
        let state = self.toplevels.entry(zaura_toplevel_host_id).or_default();
        if let Some(target) = state.pending_origin {
            if target != origin {
                return false;
            }
            state.pending_origin = None;
        }
        state.origin = Some(origin);
        self.debug_assert_consistent();
        true
    }

    /// Predict the origin after a self-parent request.
    #[must_use = "the prediction was rejected because the Aura toplevel is not live"]
    pub(crate) fn predict_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
        target_origin: (i32, i32),
    ) -> bool {
        if !self
            .xdg_toplevel_by_aura_toplevel
            .contains_key(&zaura_toplevel_host_id)
        {
            return false;
        }
        let state = self.toplevels.entry(zaura_toplevel_host_id).or_default();
        state.origin = Some(target_origin);
        state.pending_origin = Some(target_origin);
        self.debug_assert_consistent();
        true
    }

    /// Forget origin state when the internal Aura toplevel is released.
    ///
    /// Barrier callbacks are intentionally not removed here: a callback can
    /// still arrive after the toplevel destructor and must keep its host ID
    /// reserved until its terminal `delete_id`.
    fn release_toplevel(&mut self, zaura_toplevel_host_id: u32) {
        self.toplevels.remove(&zaura_toplevel_host_id);
        self.active_barrier_by_toplevel
            .remove(&zaura_toplevel_host_id);
    }

    /// Register a newly queued host sync callback as the active barrier.
    ///
    /// A callback ID must be globally unique for the connection. Returning
    /// `false` instead of overwriting an existing entry keeps an older
    /// callback's terminal lifecycle reachable.
    #[must_use = "the barrier was rejected and must not be queued"]
    pub(crate) fn register_barrier(
        &mut self,
        callback_host_id: u32,
        zaura_toplevel_host_id: u32,
    ) -> bool {
        if self.barrier_to_toplevel.contains_key(&callback_host_id)
            || !self
                .xdg_toplevel_by_aura_toplevel
                .contains_key(&zaura_toplevel_host_id)
        {
            return false;
        }
        self.barrier_to_toplevel
            .insert(callback_host_id, zaura_toplevel_host_id);
        self.active_barrier_by_toplevel
            .insert(zaura_toplevel_host_id, callback_host_id);
        self.debug_assert_consistent();
        true
    }

    /// Retire a completed barrier and clear it only if it is still newest.
    #[must_use = "the returned Aura toplevel identifies which placement completed"]
    pub(crate) fn complete_barrier(&mut self, callback_host_id: u32) -> Option<u32> {
        let zaura_toplevel_host_id = self.barrier_to_toplevel.remove(&callback_host_id)?;
        if self.active_barrier_by_toplevel.get(&zaura_toplevel_host_id) == Some(&callback_host_id) {
            self.active_barrier_by_toplevel
                .remove(&zaura_toplevel_host_id);
        }
        self.debug_assert_consistent();
        Some(zaura_toplevel_host_id)
    }

    /// Return whether a placement barrier currently gates this toplevel.
    pub(crate) fn has_active_barrier(&self, zaura_toplevel_host_id: u32) -> bool {
        self.active_barrier_by_toplevel
            .contains_key(&zaura_toplevel_host_id)
    }

    #[cfg(test)]
    pub(crate) fn barrier_for_callback(&self, callback_host_id: u32) -> Option<u32> {
        self.barrier_to_toplevel.get(&callback_host_id).copied()
    }

    #[cfg(test)]
    pub(crate) fn active_barrier_for_toplevel(&self, zaura_toplevel_host_id: u32) -> Option<u32> {
        self.active_barrier_by_toplevel
            .get(&zaura_toplevel_host_id)
            .copied()
    }

    #[cfg(test)]
    pub(crate) fn has_any_barriers(&self) -> bool {
        !self.barrier_to_toplevel.is_empty() || !self.active_barrier_by_toplevel.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn set_mode_for_test(&mut self, mode: WindowPlacementMode) {
        self.mode = mode;
    }
}

impl Default for WindowPlacementState {
    fn default() -> Self {
        Self::new(WindowPlacementMode::disabled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_flags_are_mutually_exclusive_with_arc_precedence() {
        assert_eq!(
            WindowPlacementMode::from_flags(false, false),
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::None)
        );
        assert_eq!(
            WindowPlacementMode::from_flags(true, false),
            WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
        );
        assert_eq!(
            WindowPlacementMode::from_flags(false, true),
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::SelfParent)
        );
        assert_eq!(
            WindowPlacementMode::from_flags(true, true),
            WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
        );
    }

    #[test]
    fn mode_capabilities_match_backend() {
        let disabled = WindowPlacementMode::disabled();
        let arc_bounds =
            WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds);
        let self_parent =
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::SelfParent);
        assert!(!disabled.handles_shortcuts());
        assert!(arc_bounds.handles_shortcuts());
        assert!(self_parent.handles_shortcuts());
        assert!(arc_bounds.uses_arc_policy());
        assert!(arc_bounds.uses_bounds());
        assert!(self_parent.uses_self_parent());
        assert!(!disabled.uses_arc_policy());
        assert!(!disabled.uses_bounds());
        assert!(!disabled.uses_self_parent());
    }

    #[test]
    fn non_arc_backends_cannot_allocate_arc_metadata() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        assert_eq!(state.arc_session_application_id(10), None);
    }

    #[test]
    fn arc_session_ids_are_stable_until_surface_release() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let first = state
            .arc_session_application_id(10)
            .expect("ARC backend should allocate an application ID");
        assert_eq!(state.arc_session_application_id(10), Some(first.clone()));
        let suffix = first
            .strip_prefix(&format!("{ARC_SESSION_APPLICATION_ID_PREFIX}."))
            .expect("ARC session prefix");
        assert!(suffix.parse::<u32>().expect("numeric ARC session ID") > ARC_SESSION_ID_BASE);

        assert_eq!(state.remove_surface(10, 20), None);
        assert_ne!(state.arc_session_application_id(10), Some(first));
    }

    #[test]
    fn surface_and_toplevel_release_paths_clear_all_owned_state() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_surface(50, 60));
        assert!(state.remember_aura_surface(50, 60));
        assert_eq!(state.aura_surface_for_wl_surface(50), Some(60));
        assert!(!state.remember_aura_surface(50, 61));
        assert!(state.remember_aura_surface(51, 61));
        assert!(!state.remember_aura_surface(51, 60));
        assert!(!state.remember_aura_surface(52, 61));
        assert_eq!(state.remove_surface(10, 50), Some(60));
        assert_eq!(state.aura_surface_for_wl_surface(50), None);
        assert!(state.remember_aura_surface(52, 60));

        assert!(state.remember_aura_toplevel(10, 70));
        assert!(state.remember_aura_toplevel(10, 70));
        assert_eq!(state.aura_toplevel_for_xdg_toplevel(10), Some(70));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), Some(10));

        assert!(state.record_origin(70, (1, 2)));
        assert!(!state.remember_aura_toplevel(10, 71));
        assert_eq!(state.aura_toplevel_for_xdg_toplevel(10), Some(70));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), Some(10));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(71), None);
        assert_eq!(state.origin(70), Some((1, 2)));

        assert!(!state.remember_aura_toplevel(11, 70));
        assert_eq!(state.aura_toplevel_for_xdg_toplevel(11), None);
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), Some(10));

        assert_eq!(state.take_aura_toplevel(10), Some(70));
        assert!(state.remember_aura_toplevel(11, 71));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), None);
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(71), Some(11));
        assert!(state.record_origin(71, (8, 9)));
        assert!(state.register_barrier(80, 71));
        assert_eq!(state.take_aura_toplevel(11), Some(71));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(71), None);
        assert_eq!(state.origin(71), None);
        assert!(!state.has_active_barrier(71));
        assert_eq!(state.barrier_for_callback(80), Some(71));
    }

    #[test]
    fn origin_prediction_rejects_stale_events_until_target_arrives() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.record_origin(77, (100, 200)));
        assert!(state.predict_origin(77, (0, 0)));

        assert!(!state.record_origin(77, (80, 160)));
        assert_eq!(state.origin(77), Some((0, 0)));
        assert!(state.record_origin(77, (0, 0)));
        assert_eq!(state.origin(77), Some((0, 0)));
    }

    #[test]
    fn releasing_toplevel_clears_origin_and_active_barrier_but_not_callback_ownership() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.record_origin(77, (10, 20)));
        assert!(state.register_barrier(40, 77));
        assert_eq!(state.take_aura_toplevel(10), Some(77));

        assert_eq!(state.origin(77), None);
        assert!(!state.has_active_barrier(77));
        assert_eq!(state.barrier_for_callback(40), Some(77));
        assert_eq!(state.complete_barrier(40), Some(77));
    }

    #[test]
    fn stale_barrier_completion_does_not_clear_newer_active_barrier() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.register_barrier(40, 77));
        assert!(state.register_barrier(41, 77));

        assert_eq!(state.complete_barrier(40), Some(77));
        assert_eq!(state.active_barrier_for_toplevel(77), Some(41));
        assert_eq!(state.complete_barrier(41), Some(77));
        assert!(!state.has_any_barriers());
    }

    #[test]
    fn duplicate_barrier_callback_id_is_rejected_without_mutation() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.register_barrier(40, 77));
        assert!(!state.register_barrier(40, 88));
        assert_eq!(state.barrier_for_callback(40), Some(77));
        assert_eq!(state.active_barrier_for_toplevel(77), Some(40));
        assert_eq!(state.active_barrier_for_toplevel(88), None);
    }

    #[test]
    fn released_toplevel_cannot_recreate_origin_or_barrier_state() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_aura_toplevel(10, 77));
        assert_eq!(state.take_aura_toplevel(10), Some(77));

        assert!(!state.record_origin(77, (1, 2)));
        assert!(!state.predict_origin(77, (3, 4)));
        assert!(!state.register_barrier(40, 77));
        assert_eq!(state.origin(77), None);
        assert_eq!(state.pending_origin(77), None);
        assert_eq!(state.barrier_for_callback(40), None);
    }
}
