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

//! One connection's window placement state.
//!
//! [`WindowPlacementState`] is `Off`, the default, which holds nothing, or
//! `Active` when the bindings file was valid as the connection started
//! (`crate::placement_config::PlacementConfig::is_active`), decided once
//! per connection.
//! [`PlacementState`] is the single owner of what placement knows of each
//! XDG toplevel: its host-only Aura toplevel, where Exo last said the
//! window is, the configure the guest last got, and the placement in
//! flight. Its maps are private, so a handler cannot update one side of the
//! toplevel ↔ Aura toplevel association, or retire a toplevel, without the
//! matching cleanup. An object's end reaches it only through
//! `crate::handler::window_placement::release_toplevel` (see
//! `crate::state::lifecycle`).
//!
//! Wire encoding stays in `crate::handler::window_placement`.

use std::collections::BTreeMap;

use crate::window_layout::Rect;

/// One connection's window placement state.
#[derive(Debug)]
pub(crate) enum WindowPlacementState {
    /// Placement is off: the proxy behaves and sends exactly as it would
    /// without placement.
    Off,
    /// Placement is on.
    Active(Box<PlacementState>),
}

impl WindowPlacementState {
    /// Create one connection's placement state.
    ///
    /// # Arguments
    ///
    /// * `active` - `PlacementConfig::is_active`: whether the bindings
    ///   file is valid.
    pub(crate) fn new(active: bool) -> Self {
        if active {
            Self::Active(Box::default())
        } else {
            Self::Off
        }
    }

    /// Return whether placement is on.
    pub(crate) const fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// The placement state, while placement is on.
    pub(crate) fn active(&self) -> Option<&PlacementState> {
        match self {
            Self::Active(state) => Some(state),
            Self::Off => None,
        }
    }

    /// The placement state, while placement is on.
    pub(crate) fn active_mut(&mut self) -> Option<&mut PlacementState> {
        match self {
            Self::Active(state) => Some(state),
            Self::Off => None,
        }
    }

    /// Panic if placement state outlived its object, checking with `guest`
    /// and `host_only` as `Context::assert_no_orphans` does.
    #[cfg(test)]
    pub(crate) fn assert_no_orphans(
        &self,
        guest: &dyn Fn(&str, u32, &str),
        host_only: &dyn Fn(&str, u32, &str),
    ) {
        if let Self::Active(state) = self {
            state.assert_no_orphans(guest, host_only);
        }
    }
}

/// A point in screen DIPs.
pub(crate) type Point = (i32, i32);

/// The step a placement waits on. Each step ends with a host
/// `wl_display.sync`, whose `done` says that Exo has handled the step's
/// requests and sent the configures they cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementStep {
    /// The window leaves fullscreen and maximized state; Exo then reports
    /// the origin the window has in its restored state.
    Restore,
    /// `zaura_surface.set_parent` moved the window by `offset` from
    /// `from`, its origin before the move; Exo then reports the origin it
    /// moved to.
    Move { from: Point, offset: Point },
}

/// One placement in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    /// The host `wl_callback` whose `done` starts the next step. Only the
    /// newest placement's barrier does: a newer chord replaces this one.
    pub(crate) barrier: u32,
    /// The window's new bounds in screen DIPs, its frame included.
    pub(crate) target: Rect,
    pub(crate) step: PlacementStep,
}

/// What placement knows of one XDG toplevel.
#[derive(Debug)]
struct PlacedToplevel {
    /// The host-only `zaura_toplevel` in screen coordinates, through which
    /// Exo reports the window's origin.
    aura_toplevel: u32,
    /// The screen origin of the window's client area, from the newest
    /// `zaura_toplevel.configure` or `origin_change`; `None` before the
    /// first.
    origin: Option<Point>,
    /// The states of the newest `xdg_toplevel.configure` the guest got,
    /// already filtered to its version.
    states: Vec<u8>,
    /// The serial of the newest host `xdg_surface.configure`.
    serial: Option<u32>,
    placement: Option<Placement>,
}

/// One client connection's placement state.
#[derive(Debug, Default)]
pub(crate) struct PlacementState {
    /// Guest xdg_toplevel ID → what placement knows of it.
    toplevels: BTreeMap<u32, PlacedToplevel>,
    /// Host zaura_toplevel ID → guest xdg_toplevel ID, the reverse of
    /// `toplevels`' Aura toplevels, which routes host Aura events.
    xdg_toplevel_by_aura_toplevel: BTreeMap<u32, u32>,
}

impl PlacementState {
    /// Verify the association after every mutation in debug and test
    /// builds, so a lifecycle change fails where it breaks it.
    fn debug_assert_consistent(&self) {
        debug_assert_eq!(
            self.toplevels.len(),
            self.xdg_toplevel_by_aura_toplevel.len()
        );
        debug_assert!(self.toplevels.iter().all(|(xdg_toplevel, placed)| self
            .xdg_toplevel_by_aura_toplevel
            .get(&placed.aura_toplevel)
            == Some(xdg_toplevel)));
    }

    /// The Aura toplevel of a guest xdg_toplevel.
    pub(crate) fn aura_toplevel(&self, xdg_toplevel: u32) -> Option<u32> {
        Some(self.toplevels.get(&xdg_toplevel)?.aura_toplevel)
    }

    /// The guest xdg_toplevel of a host Aura toplevel.
    pub(crate) fn xdg_toplevel(&self, aura_toplevel: u32) -> Option<u32> {
        self.xdg_toplevel_by_aura_toplevel
            .get(&aura_toplevel)
            .copied()
    }

    /// Record the one-to-one xdg_toplevel ↔ Aura toplevel association of a
    /// newly created Aura toplevel.
    pub(crate) fn attach(&mut self, xdg_toplevel: u32, aura_toplevel: u32) {
        debug_assert!(
            !self.toplevels.contains_key(&xdg_toplevel)
                && !self
                    .xdg_toplevel_by_aura_toplevel
                    .contains_key(&aura_toplevel),
            "replacing a live Aura toplevel would orphan its destructor"
        );
        self.toplevels.insert(
            xdg_toplevel,
            PlacedToplevel {
                aura_toplevel,
                origin: None,
                states: Vec::new(),
                serial: None,
                placement: None,
            },
        );
        self.xdg_toplevel_by_aura_toplevel
            .insert(aura_toplevel, xdg_toplevel);
        self.debug_assert_consistent();
    }

    /// Forget a guest xdg_toplevel whose role ended, with its placement in
    /// flight: that placement's barrier stays pending in
    /// `crate::state::InternalCallbacks` until its `done`, but finds
    /// nothing to continue.
    ///
    /// # Returns
    ///
    /// The Aura toplevel whose host teardown the caller must perform.
    #[must_use = "the returned Aura toplevel ID identifies host teardown work"]
    pub(crate) fn take(&mut self, xdg_toplevel: u32) -> Option<u32> {
        let placed = self.toplevels.remove(&xdg_toplevel)?;
        self.xdg_toplevel_by_aura_toplevel
            .remove(&placed.aura_toplevel);
        self.debug_assert_consistent();
        Some(placed.aura_toplevel)
    }

    /// Record a `zaura_toplevel.configure` of `xdg_toplevel`: the window's
    /// client-area origin, and the states the guest gets with it.
    pub(crate) fn configured(&mut self, xdg_toplevel: u32, origin: Point, states: Vec<u8>) {
        if let Some(placed) = self.toplevels.get_mut(&xdg_toplevel) {
            placed.origin = Some(origin);
            placed.states = states;
        }
    }

    /// Record a `zaura_toplevel.origin_change` of `xdg_toplevel`.
    pub(crate) fn moved(&mut self, xdg_toplevel: u32, origin: Point) {
        if let Some(placed) = self.toplevels.get_mut(&xdg_toplevel) {
            placed.origin = Some(origin);
        }
    }

    /// Record the serial of a host `xdg_surface.configure` of
    /// `xdg_toplevel`'s surface.
    pub(crate) fn serial_sent(&mut self, xdg_toplevel: u32, serial: u32) {
        if let Some(placed) = self.toplevels.get_mut(&xdg_toplevel) {
            placed.serial = Some(serial);
        }
    }

    /// The window's client-area origin, as Exo last reported it.
    pub(crate) fn origin(&self, xdg_toplevel: u32) -> Option<Point> {
        self.toplevels.get(&xdg_toplevel)?.origin
    }

    /// The states and serial of the newest configure, which a configure the
    /// proxy makes repeats; `None` before the host's first.
    pub(crate) fn last_configure(&self, xdg_toplevel: u32) -> Option<(&[u8], u32)> {
        let placed = self.toplevels.get(&xdg_toplevel)?;
        Some((&placed.states, placed.serial?))
    }

    /// Make `placement` the one in flight for `xdg_toplevel`, replacing any
    /// older one.
    pub(crate) fn start(&mut self, xdg_toplevel: u32, placement: Placement) {
        if let Some(placed) = self.toplevels.get_mut(&xdg_toplevel) {
            placed.placement = Some(placement);
        }
    }

    /// Take the placement in flight for `xdg_toplevel` if `barrier` is its
    /// barrier: a replaced placement's barrier finds none.
    pub(crate) fn take_placement(&mut self, xdg_toplevel: u32, barrier: u32) -> Option<Placement> {
        let placed = self.toplevels.get_mut(&xdg_toplevel)?;
        if placed.placement?.barrier != barrier {
            return None;
        }
        placed.placement.take()
    }

    /// The placement in flight for `xdg_toplevel`.
    #[cfg(test)]
    pub(crate) fn placement(&self, xdg_toplevel: u32) -> Option<Placement> {
        self.toplevels.get(&xdg_toplevel)?.placement
    }

    #[cfg(test)]
    pub(crate) fn toplevel_count(&self) -> usize {
        self.toplevels.len()
    }

    /// Panic if state outlived its object: `guest` checks a live guest
    /// object and `host_only` a live object only the proxy uses, as
    /// `Context::assert_no_orphans` does.
    #[cfg(test)]
    fn assert_no_orphans(
        &self,
        guest: &dyn Fn(&str, u32, &str),
        host_only: &dyn Fn(&str, u32, &str),
    ) {
        for (&xdg_toplevel, placed) in &self.toplevels {
            guest("Aura toplevel", xdg_toplevel, "xdg_toplevel");
            host_only("Aura toplevel", placed.aura_toplevel, "zaura_toplevel");
        }
        for (&aura_toplevel, &xdg_toplevel) in &self.xdg_toplevel_by_aura_toplevel {
            guest("Aura toplevel", xdg_toplevel, "xdg_toplevel");
            host_only("Aura toplevel", aura_toplevel, "zaura_toplevel");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(barrier: u32) -> Placement {
        Placement {
            barrier,
            target: Rect::new(0, 0, 10, 20),
            step: PlacementStep::Restore,
        }
    }

    #[test]
    fn placement_off_holds_nothing() {
        let state = WindowPlacementState::new(false);
        assert!(!state.is_active());
        assert!(state.active().is_none());
        assert!(WindowPlacementState::new(true).is_active());
    }

    #[test]
    fn toplevel_association_resolves_both_directions_until_taken() {
        let mut state = PlacementState::default();
        state.attach(10, 70);
        assert_eq!(state.aura_toplevel(10), Some(70));
        assert_eq!(state.xdg_toplevel(70), Some(10));

        assert_eq!(state.take(10), Some(70));
        assert_eq!(state.aura_toplevel(10), None);
        assert_eq!(state.xdg_toplevel(70), None);
        assert_eq!(state.take(10), None, "release is idempotent");
    }

    /// The newest configure's origin, states and serial are what a
    /// placement reads; an origin change moves only the origin.
    #[test]
    fn the_newest_report_wins() {
        let mut state = PlacementState::default();
        state.attach(10, 70);
        assert_eq!(state.origin(10), None);
        assert_eq!(state.last_configure(10), None, "no serial yet");

        state.configured(10, (5, 6), vec![4, 0, 0, 0]);
        state.serial_sent(10, 9);
        state.moved(10, (7, 8));
        assert_eq!(state.origin(10), Some((7, 8)));
        assert_eq!(state.last_configure(10), Some((&[4, 0, 0, 0][..], 9)));

        // Reports of an unknown toplevel change nothing.
        state.configured(11, (1, 1), Vec::new());
        state.moved(11, (1, 1));
        state.serial_sent(11, 1);
        assert_eq!(state.origin(11), None);
    }

    /// A newer placement replaces the older one, whose barrier then finds
    /// nothing to continue, and a released toplevel's barrier neither.
    #[test]
    fn only_the_newest_placement_continues() {
        let mut state = PlacementState::default();
        state.attach(10, 70);
        state.start(10, placement(40));
        state.start(10, placement(41));
        assert_eq!(state.take_placement(10, 40), None);
        assert_eq!(state.take_placement(10, 41), Some(placement(41)));
        assert_eq!(state.take_placement(10, 41), None, "taken once");

        state.start(10, placement(42));
        assert_eq!(state.take(10), Some(70));
        assert_eq!(state.take_placement(10, 42), None);
    }
}
