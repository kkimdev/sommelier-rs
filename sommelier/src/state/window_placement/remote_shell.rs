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

//! State of the remote-shell-v2 placement backend.
//!
//! [`RemoteShellPlacementState`] is one connection's state for the backend:
//! the host `zcr_remote_shell_v2` the proxy bound, the host
//! `zcr_remote_surface_v2` that gives each guest toplevel its role, and the
//! serial of the configures the proxy sends in the host's place. The guest's
//! XDG objects on a remote surface are local objects of the shadow table,
//! and their links are in `crate::state::XdgRoles` like every XDG role's.
//! Wire encoding stays in `crate::handler::window_placement::remote_shell`.

use std::collections::{BTreeMap, HashMap};

/// The host `zcr_remote_shell_v2` the proxy bound for its own use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteShellManager {
    /// The registry name it was bound from.
    pub(crate) global_name: u32,
    pub(crate) host_id: u32,
    pub(crate) version: u32,
}

/// One client connection's state of the remote-shell-v2 backend.
#[derive(Debug, Default)]
pub(crate) struct RemoteShellPlacementState {
    /// The manager new remote surfaces come from; `None` before the host
    /// advertises one, and after its global is removed until another is
    /// bound.
    manager: Option<RemoteShellManager>,
    /// Guest xdg_toplevel ID → its host zcr_remote_surface_v2.
    remote_surface_by_toplevel: HashMap<u32, u32>,
    /// Reverse of `remote_surface_by_toplevel`, for host events.
    toplevel_by_remote_surface: HashMap<u32, u32>,
    /// Guest toplevels whose remote surface has no bounds yet, with the
    /// size of the commit that showed each, once one did while no output
    /// could place it. Ordered, because an output that can place them
    /// places them in that order.
    unplaced: BTreeMap<u32, Option<(i32, i32)>>,
    /// Guest facade xdg_popup ID → its xdg_surface, whose role it is.
    popups: HashMap<u32, u32>,
    /// The serial of the last `xdg_surface.configure` the proxy sent.
    last_configure_serial: u32,
}

impl RemoteShellPlacementState {
    /// The manager new remote surfaces come from.
    pub(crate) const fn manager(&self) -> Option<RemoteShellManager> {
        self.manager
    }

    /// Record the manager the proxy bound.
    pub(crate) fn bind_manager(&mut self, manager: RemoteShellManager) {
        debug_assert!(self.manager.is_none(), "one manager is bound at a time");
        self.manager = Some(manager);
    }

    /// Forget the manager bound from global `global_name`, whose removal
    /// every registry has seen.
    ///
    /// # Returns
    ///
    /// The manager, whose host object the caller retires; `None` if another
    /// global's manager, or none, is bound.
    #[must_use = "the returned manager's host object must be retired"]
    pub(crate) fn take_manager_of(&mut self, global_name: u32) -> Option<RemoteShellManager> {
        self.manager
            .take_if(|manager| manager.global_name == global_name)
    }

    /// Give a guest toplevel its host remote surface.
    pub(crate) fn remember_remote_surface(
        &mut self,
        xdg_toplevel_guest_id: u32,
        remote_surface_host_id: u32,
    ) {
        debug_assert!(
            !self
                .remote_surface_by_toplevel
                .contains_key(&xdg_toplevel_guest_id),
            "replacing a live remote surface would orphan its destructor"
        );
        self.remote_surface_by_toplevel
            .insert(xdg_toplevel_guest_id, remote_surface_host_id);
        self.toplevel_by_remote_surface
            .insert(remote_surface_host_id, xdg_toplevel_guest_id);
        self.unplaced.insert(xdg_toplevel_guest_id, None);
    }

    /// Whether a guest toplevel's remote surface has no bounds yet.
    pub(crate) fn is_unplaced(&self, xdg_toplevel_guest_id: u32) -> bool {
        self.unplaced.contains_key(&xdg_toplevel_guest_id)
    }

    /// Record that an unplaced toplevel was shown at `size` while no output
    /// could place it.
    pub(crate) fn record_shown_size(&mut self, xdg_toplevel_guest_id: u32, size: (i32, i32)) {
        if let Some(shown) = self.unplaced.get_mut(&xdg_toplevel_guest_id) {
            *shown = Some(size);
        }
    }

    /// The unplaced toplevels that were shown, with their size.
    pub(crate) fn waiting_for_output(&self) -> Vec<(u32, (i32, i32))> {
        self.unplaced
            .iter()
            .filter_map(|(&toplevel, &size)| Some((toplevel, size?)))
            .collect()
    }

    /// Record that a guest toplevel's remote surface has bounds.
    pub(crate) fn mark_placed(&mut self, xdg_toplevel_guest_id: u32) {
        self.unplaced.remove(&xdg_toplevel_guest_id);
    }

    /// Record facade popup `xdg_popup_guest_id`, the role of
    /// `xdg_surface_guest_id`.
    pub(crate) fn remember_popup(&mut self, xdg_popup_guest_id: u32, xdg_surface_guest_id: u32) {
        self.popups.insert(xdg_popup_guest_id, xdg_surface_guest_id);
    }

    /// Whether facade xdg_surface `xdg_surface_guest_id` has a live popup.
    pub(crate) fn has_popup(&self, xdg_surface_guest_id: u32) -> bool {
        self.popups
            .values()
            .any(|&xdg_surface| xdg_surface == xdg_surface_guest_id)
    }

    /// Forget a facade popup that ended.
    pub(crate) fn forget_popup(&mut self, xdg_popup_guest_id: u32) {
        self.popups.remove(&xdg_popup_guest_id);
    }

    /// The host remote surface of a guest toplevel.
    pub(crate) fn remote_surface_for_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.remote_surface_by_toplevel
            .get(&xdg_toplevel_guest_id)
            .copied()
    }

    /// The guest toplevel of a host remote surface.
    pub(crate) fn toplevel_for_remote_surface(&self, remote_surface_host_id: u32) -> Option<u32> {
        self.toplevel_by_remote_surface
            .get(&remote_surface_host_id)
            .copied()
    }

    /// Forget a guest toplevel's remote surface.
    ///
    /// # Returns
    ///
    /// The remote surface whose host destructor the caller queues.
    #[must_use = "the returned remote surface must be destroyed on the host"]
    pub(crate) fn take_remote_surface(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let remote_surface_host_id = self
            .remote_surface_by_toplevel
            .remove(&xdg_toplevel_guest_id)?;
        self.toplevel_by_remote_surface
            .remove(&remote_surface_host_id);
        self.unplaced.remove(&xdg_toplevel_guest_id);
        Some(remote_surface_host_id)
    }

    /// The serial of the next `xdg_surface.configure` the proxy sends.
    pub(crate) fn next_configure_serial(&mut self) -> u32 {
        self.last_configure_serial = self.last_configure_serial.wrapping_add(1);
        self.last_configure_serial
    }

    /// Panic if state outlived its object, checking with `guest` and
    /// `host_only` as `Context::assert_no_orphans` does.
    #[cfg(test)]
    pub(crate) fn assert_no_orphans(
        &self,
        guest: &dyn Fn(&str, u32, &str),
        host_only: &dyn Fn(&str, u32, &str),
    ) {
        if let Some(manager) = self.manager {
            host_only("remote shell", manager.host_id, "zcr_remote_shell_v2");
        }
        let pairs = self
            .remote_surface_by_toplevel
            .iter()
            .map(|(&toplevel, &remote)| (toplevel, remote))
            .chain(
                self.toplevel_by_remote_surface
                    .iter()
                    .map(|(&remote, &toplevel)| (toplevel, remote)),
            );
        for (toplevel, remote_surface) in pairs {
            guest("remote surface", toplevel, "xdg_toplevel");
            host_only("remote surface", remote_surface, "zcr_remote_surface_v2");
        }
        for &toplevel in self.unplaced.keys() {
            assert!(
                self.remote_surface_by_toplevel.contains_key(&toplevel),
                "unplaced toplevel {toplevel} outlived its remote surface"
            );
        }
        for (&popup, &xdg_surface) in &self.popups {
            guest("facade popup", popup, "xdg_popup");
            guest("facade popup", xdg_surface, "xdg_surface");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGER: RemoteShellManager = RemoteShellManager {
        global_name: 7,
        host_id: 30,
        version: 6,
    };

    #[test]
    fn a_toplevel_pairs_with_its_remote_surface_until_taken() {
        let mut state = RemoteShellPlacementState::default();
        state.remember_remote_surface(12, 40);
        assert_eq!(state.remote_surface_for_toplevel(12), Some(40));
        assert_eq!(state.toplevel_for_remote_surface(40), Some(12));
        assert_eq!(state.take_remote_surface(12), Some(40));
        assert_eq!(
            state.take_remote_surface(12),
            None,
            "teardown is idempotent"
        );
        assert_eq!(state.toplevel_for_remote_surface(40), None);
    }

    #[test]
    fn only_the_bound_globals_removal_takes_the_manager() {
        let mut state = RemoteShellPlacementState::default();
        state.bind_manager(MANAGER);
        assert_eq!(state.take_manager_of(8), None);
        assert_eq!(state.manager(), Some(MANAGER));
        assert_eq!(state.take_manager_of(7), Some(MANAGER));
        assert_eq!(state.manager(), None);
    }

    #[test]
    fn configure_serials_increase_and_wrap() {
        let mut state = RemoteShellPlacementState::default();
        assert_eq!(state.next_configure_serial(), 1);
        assert_eq!(state.next_configure_serial(), 2);
        state.last_configure_serial = u32::MAX;
        assert_eq!(state.next_configure_serial(), 0);
    }
}
