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

//! Bookkeeping for the experimental `zcr_remote_shell_v2` placement backend.
//!
//! Exo gives a `wl_surface` at most one shell role, and the remote-surface
//! role cannot coexist with an XDG role. With this backend enabled, the proxy
//! therefore answers the guest's XDG objects locally ("facades") and gives
//! the host surface a `zcr_remote_surface_v2` role instead.
//! [`RemoteShellState`] records the host manager binding, which guest XDG
//! IDs are facades, and the facade toplevel ↔ host remote surface pairs.
//! Wire encoding lives in `handler::remote_shell`.

use std::collections::HashMap;

/// One bound host `zcr_remote_shell_v2` manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteShellManager {
    pub host_id: u32,
    pub version: u32,
}

/// A guest XDG object that the proxy implements locally because its host
/// surface has a remote-surface role instead of an XDG role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteShellFacade {
    /// `xdg_surface`. `constructed` is set while a toplevel or popup role
    /// object exists, mirroring xdg-shell's `already_constructed` rule.
    Surface {
        wl_surface_guest_id: u32,
        constructed: bool,
    },
    /// `xdg_toplevel`, backed by a host `zcr_remote_surface_v2`.
    Toplevel { xdg_surface_guest_id: u32 },
    /// `xdg_popup`. A remote surface cannot parent an XDG popup, so the
    /// facade dismisses the popup immediately.
    Popup { xdg_surface_guest_id: u32 },
}

/// Connection-scoped state of the remote-shell backend. It exists only when
/// the backend is enabled.
#[derive(Debug, Default)]
pub(crate) struct RemoteShellState {
    /// Newest manager bound from the host registry. It is kept after its
    /// global is removed: already created remote surfaces stay valid, and
    /// keeping the connection on the remote-surface path avoids mixing
    /// facade and host XDG toplevels.
    manager: Option<RemoteShellManager>,
    /// Registry name of the live manager advertisement, if any.
    manager_global_name: Option<u32>,
    /// Guest facade object ID → facade kind.
    facades: HashMap<u32, RemoteShellFacade>,
    /// Guest facade xdg_toplevel ID → host zcr_remote_surface_v2 ID.
    remote_surface_by_toplevel: HashMap<u32, u32>,
    /// Reverse of `remote_surface_by_toplevel`, for host events.
    toplevel_by_remote_surface: HashMap<u32, u32>,
    /// Last serial used for a synthesized `xdg_surface.configure`.
    last_configure_serial: u32,
    /// Whether the missing-manager warning was already logged.
    reported_missing_manager: bool,
}

impl RemoteShellState {
    fn debug_assert_consistent(&self) {
        debug_assert_eq!(
            self.remote_surface_by_toplevel.len(),
            self.toplevel_by_remote_surface.len()
        );
        debug_assert!(self
            .remote_surface_by_toplevel
            .iter()
            .all(|(toplevel, remote_surface)| {
                self.toplevel_by_remote_surface.get(remote_surface) == Some(toplevel)
                    && matches!(
                        self.facades.get(toplevel),
                        Some(RemoteShellFacade::Toplevel { .. })
                    )
            }));
    }

    // ── Manager binding ───────────────────────────────────────────────────

    /// Return the manager new remote surfaces are created from.
    pub(crate) fn manager(&self) -> Option<RemoteShellManager> {
        self.manager
    }

    /// Return whether a live advertisement is already bound, so another
    /// registry's copy of the same global must not bind a second manager.
    pub(crate) fn has_live_manager_global(&self) -> bool {
        self.manager_global_name.is_some()
    }

    /// Return whether `name` is the live manager advertisement.
    pub(crate) fn manager_global_matches(&self, name: u32) -> bool {
        self.manager_global_name == Some(name)
    }

    /// Record a manager bound from the advertisement `name`. A previously
    /// retired manager is superseded but not destroyed; its remote surfaces
    /// keep working.
    pub(crate) fn bind_manager(&mut self, name: u32, manager: RemoteShellManager) {
        debug_assert!(
            self.manager_global_name.is_none(),
            "a live manager advertisement is bound once per connection"
        );
        self.manager_global_name = Some(name);
        self.manager = Some(manager);
    }

    /// Forget the advertisement name after `global_remove`. The manager
    /// object itself stays bound for the rest of the connection.
    pub(crate) fn retire_manager_global(&mut self, name: u32) {
        if self.manager_global_name == Some(name) {
            self.manager_global_name = None;
        }
    }

    /// Return `true` the first time a toplevel falls back to the normal XDG
    /// path because no manager is bound, so the warning is logged once.
    pub(crate) fn note_missing_manager(&mut self) -> bool {
        !std::mem::replace(&mut self.reported_missing_manager, true)
    }

    // ── Facades ───────────────────────────────────────────────────────────

    /// Return the facade kind of a guest object, or `None` when the object
    /// is not a facade.
    pub(crate) fn facade(&self, guest_id: u32) -> Option<RemoteShellFacade> {
        self.facades.get(&guest_id).copied()
    }

    /// Record a new facade object.
    pub(crate) fn remember_facade(&mut self, guest_id: u32, facade: RemoteShellFacade) {
        let previous = self.facades.insert(guest_id, facade);
        debug_assert!(previous.is_none(), "facade IDs are validated as new");
        self.debug_assert_consistent();
    }

    /// Forget a destroyed facade object.
    ///
    /// A toplevel's remote surface must be taken with
    /// [`Self::take_remote_surface`] first so its host destructor is not
    /// lost.
    pub(crate) fn take_facade(&mut self, guest_id: u32) -> Option<RemoteShellFacade> {
        debug_assert!(
            !self.remote_surface_by_toplevel.contains_key(&guest_id),
            "take the remote surface before its toplevel facade"
        );
        let facade = self.facades.remove(&guest_id);
        self.debug_assert_consistent();
        facade
    }

    /// Mark an xdg_surface facade as having (or no longer having) a role
    /// object.
    ///
    /// # Returns
    ///
    /// `false`, without changing anything, when `xdg_surface_guest_id` is not
    /// an xdg_surface facade or when constructing a surface that already has
    /// a role object.
    pub(crate) fn set_xdg_surface_constructed(
        &mut self,
        xdg_surface_guest_id: u32,
        value: bool,
    ) -> bool {
        match self.facades.get_mut(&xdg_surface_guest_id) {
            Some(RemoteShellFacade::Surface { constructed, .. }) if !(value && *constructed) => {
                *constructed = value;
                true
            }
            _ => false,
        }
    }

    // ── Remote surfaces ───────────────────────────────────────────────────

    /// Pair a facade toplevel with its host remote surface.
    pub(crate) fn remember_remote_surface(
        &mut self,
        xdg_toplevel_guest_id: u32,
        remote_surface_host_id: u32,
    ) {
        debug_assert!(
            !self
                .remote_surface_by_toplevel
                .contains_key(&xdg_toplevel_guest_id)
                && !self
                    .toplevel_by_remote_surface
                    .contains_key(&remote_surface_host_id),
            "replacing a live remote surface would orphan its destructor"
        );
        self.remote_surface_by_toplevel
            .insert(xdg_toplevel_guest_id, remote_surface_host_id);
        self.toplevel_by_remote_surface
            .insert(remote_surface_host_id, xdg_toplevel_guest_id);
        self.debug_assert_consistent();
    }

    /// Return the host remote surface backing a facade toplevel.
    pub(crate) fn remote_surface_for_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.remote_surface_by_toplevel
            .get(&xdg_toplevel_guest_id)
            .copied()
    }

    /// Resolve a host remote-surface event back to its facade toplevel.
    pub(crate) fn toplevel_for_remote_surface(&self, remote_surface_host_id: u32) -> Option<u32> {
        self.toplevel_by_remote_surface
            .get(&remote_surface_host_id)
            .copied()
    }

    /// Unpair a facade toplevel from its remote surface.
    ///
    /// # Returns
    ///
    /// The remote surface whose host destructor the caller must queue.
    #[must_use = "the returned remote surface ID identifies host teardown work"]
    pub(crate) fn take_remote_surface(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let remote_surface_host_id = self
            .remote_surface_by_toplevel
            .remove(&xdg_toplevel_guest_id)?;
        self.toplevel_by_remote_surface
            .remove(&remote_surface_host_id);
        self.debug_assert_consistent();
        Some(remote_surface_host_id)
    }

    /// Allocate the serial for a synthesized `xdg_surface.configure`.
    pub(crate) fn next_configure_serial(&mut self) -> u32 {
        self.last_configure_serial = self.last_configure_serial.wrapping_add(1);
        self.last_configure_serial
    }

    #[cfg(test)]
    pub(crate) fn remote_surface_count(&self) -> usize {
        self.remote_surface_by_toplevel.len()
    }

    #[cfg(test)]
    pub(crate) fn facade_count(&self) -> usize {
        self.facades.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGER: RemoteShellManager = RemoteShellManager {
        host_id: 30,
        version: 6,
    };

    fn toplevel_fixture() -> RemoteShellState {
        let mut state = RemoteShellState::default();
        state.remember_facade(
            11,
            RemoteShellFacade::Surface {
                wl_surface_guest_id: 10,
                constructed: false,
            },
        );
        assert!(state.set_xdg_surface_constructed(11, true));
        state.remember_facade(
            12,
            RemoteShellFacade::Toplevel {
                xdg_surface_guest_id: 11,
            },
        );
        state.remember_remote_surface(12, 40);
        state
    }

    #[test]
    fn manager_survives_global_removal_until_replaced() {
        let mut state = RemoteShellState::default();
        assert_eq!(state.manager(), None);
        assert!(!state.has_live_manager_global());

        state.bind_manager(7, MANAGER);
        assert!(state.has_live_manager_global());
        assert!(state.manager_global_matches(7));
        assert!(!state.manager_global_matches(8));

        state.retire_manager_global(8);
        assert!(state.manager_global_matches(7), "other names are ignored");
        state.retire_manager_global(7);
        assert!(!state.has_live_manager_global());
        assert_eq!(state.manager(), Some(MANAGER));

        let replacement = RemoteShellManager {
            host_id: 31,
            version: 4,
        };
        state.bind_manager(7, replacement);
        assert_eq!(state.manager(), Some(replacement));
    }

    #[test]
    fn missing_manager_is_reported_once() {
        let mut state = RemoteShellState::default();
        assert!(state.note_missing_manager());
        assert!(!state.note_missing_manager());
    }

    #[test]
    fn toplevel_pairs_with_remote_surface_until_taken() {
        let mut state = toplevel_fixture();
        assert_eq!(state.remote_surface_for_toplevel(12), Some(40));
        assert_eq!(state.toplevel_for_remote_surface(40), Some(12));

        assert_eq!(state.take_remote_surface(12), Some(40));
        assert_eq!(
            state.take_remote_surface(12),
            None,
            "teardown is idempotent"
        );
        assert_eq!(state.toplevel_for_remote_surface(40), None);
        assert_eq!(
            state.take_facade(12),
            Some(RemoteShellFacade::Toplevel {
                xdg_surface_guest_id: 11
            })
        );
        assert_eq!(state.remote_surface_count(), 0);
        assert_eq!(state.facade_count(), 1);
    }

    #[test]
    fn xdg_surface_accepts_one_role_object_at_a_time() {
        let mut state = toplevel_fixture();
        assert!(
            !state.set_xdg_surface_constructed(11, true),
            "a second role object is already_constructed"
        );
        assert!(state.set_xdg_surface_constructed(11, false));
        assert!(state.set_xdg_surface_constructed(11, true));
        assert!(
            !state.set_xdg_surface_constructed(12, true),
            "only xdg_surface facades have a construction state"
        );
        assert!(!state.set_xdg_surface_constructed(99, true));
    }

    #[test]
    fn configure_serials_are_monotonic_and_wrap() {
        let mut state = RemoteShellState::default();
        assert_eq!(state.next_configure_serial(), 1);
        assert_eq!(state.next_configure_serial(), 2);
        state.last_configure_serial = u32::MAX;
        assert_eq!(state.next_configure_serial(), 0);
    }
}
