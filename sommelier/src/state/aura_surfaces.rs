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

//! The host `zaura_surface` of each host `wl_surface`.
//!
//! Exo rejects a second `zaura_surface` for one `wl_surface`, so every
//! feature that sends Aura surface metadata (XDG and GTK application IDs,
//! the GTK startup ID, and window placement) shares the one recorded here.
//! [`AuraSurfaces`] only records the association;
//! `crate::handler::aura_objects` creates and releases the host objects.

use std::collections::BTreeMap;

use super::HostId;

/// One connection's host `wl_surface` → host `zaura_surface` associations.
#[derive(Debug, Default)]
pub(crate) struct AuraSurfaces {
    by_wl_surface: BTreeMap<HostId, HostId>,
}

impl AuraSurfaces {
    /// Return the Aura surface of a host wl_surface, if one was created.
    pub(crate) fn get(&self, wl_surface_host_id: HostId) -> Option<HostId> {
        self.by_wl_surface.get(&wl_surface_host_id).copied()
    }

    /// Record the Aura surface created for a host wl_surface.
    pub(crate) fn remember(&mut self, wl_surface_host_id: HostId, zaura_surface_host_id: HostId) {
        let previous = self
            .by_wl_surface
            .insert(wl_surface_host_id, zaura_surface_host_id);
        debug_assert!(
            previous.is_none_or(|previous| previous == zaura_surface_host_id),
            "replacing a live Aura surface would orphan its destructor"
        );
    }

    /// Forget the Aura surface of a destroyed host wl_surface.
    ///
    /// # Returns
    ///
    /// The Aura surface whose host teardown the caller must perform.
    #[must_use = "the returned Aura surface ID identifies host teardown work"]
    pub(crate) fn take(&mut self, wl_surface_host_id: HostId) -> Option<HostId> {
        self.by_wl_surface.remove(&wl_surface_host_id)
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.by_wl_surface.len()
    }

    /// Every host wl_surface that has an Aura surface.
    #[cfg(test)]
    pub(super) fn wl_surfaces_for_test(&self) -> impl Iterator<Item = HostId> + '_ {
        self.by_wl_surface.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn association_resolves_until_taken() {
        let (wl_surface, aura_surface) = (HostId::for_test(50), HostId::for_test(60));
        let mut surfaces = AuraSurfaces::default();
        assert_eq!(surfaces.get(wl_surface), None);
        surfaces.remember(wl_surface, aura_surface);
        surfaces.remember(wl_surface, aura_surface);
        assert_eq!(surfaces.get(wl_surface), Some(aura_surface));
        assert_eq!(surfaces.count(), 1);

        assert_eq!(surfaces.take(wl_surface), Some(aura_surface));
        assert_eq!(surfaces.get(wl_surface), None);
        assert_eq!(surfaces.take(wl_surface), None, "take is idempotent");
    }
}
