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

//! One connection's window placement state: the backend it uses, and that
//! backend's own state.
//!
//! [`WindowPlacementState`] has one variant per placement backend, and
//! `Off`, the default, which holds nothing. Each backend's state lives in
//! its own submodule and is reachable only through its variant, so no
//! other code, nor another backend, can read or change it, and an object's
//! end reaches it only through the backend's `forget_*` functions (see
//! `crate::state::lifecycle`). Which backend a connection uses is decided
//! once per process by `crate::placement_config::PlacementConfig`.

mod arc;

#[cfg(test)]
pub(crate) use self::arc::ARC_APPLICATION_ID_PREFIX;
pub(crate) use self::arc::{
    ArcPlacementState, PendingWindowBoundsConfigure, PendingWindowBoundsSurfaceConfigure,
};

use crate::arc_task_ids::ArcIdAllocator;

/// One connection's window placement state.
#[derive(Debug)]
pub(crate) enum WindowPlacementState {
    /// Placement is off: the proxy behaves and sends exactly as it would
    /// without placement.
    Off,
    /// The ARC backend: `zaura_toplevel.set_window_bounds` on a surface
    /// that carries an ARC policy identity.
    Arc(Box<ArcPlacementState>),
}

impl WindowPlacementState {
    /// Create one connection's placement state.
    ///
    /// # Arguments
    ///
    /// * `arc_id_allocator` - `PlacementConfig::arc_id_allocator`: the
    ///   process's ARC task-ID block while the ARC backend is active, `None`
    ///   while placement is off.
    pub(crate) fn new(arc_id_allocator: Option<&'static ArcIdAllocator>) -> Self {
        match arc_id_allocator {
            Some(allocator) => Self::Arc(Box::new(ArcPlacementState::new(allocator))),
            None => Self::Off,
        }
    }

    /// Return whether a placement backend is active.
    pub(crate) const fn is_active(&self) -> bool {
        !matches!(self, Self::Off)
    }

    /// The ARC backend's state, while it is the active backend.
    pub(crate) fn arc(&self) -> Option<&ArcPlacementState> {
        match self {
            Self::Arc(state) => Some(state),
            Self::Off => None,
        }
    }

    /// The ARC backend's state, while it is the active backend.
    pub(crate) fn arc_mut(&mut self) -> Option<&mut ArcPlacementState> {
        match self {
            Self::Arc(state) => Some(state),
            Self::Off => None,
        }
    }

    /// Panic if the active backend's state outlived its object, checking
    /// with `guest` and `host_only` as `Context::assert_no_orphans` does.
    #[cfg(test)]
    pub(crate) fn assert_no_orphans(
        &self,
        guest: &dyn Fn(&str, u32, &str),
        host_only: &dyn Fn(&str, u32, &str),
    ) {
        match self {
            Self::Off => {}
            Self::Arc(state) => state.assert_no_orphans(guest, host_only),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_policy_cannot_allocate_arc_identities() {
        let state = WindowPlacementState::new(None);
        assert!(!state.is_active());
        assert!(state.arc().is_none(), "placement off holds no ARC state");
    }
}
