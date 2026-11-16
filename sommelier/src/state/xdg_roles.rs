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

//! The guest's XDG role links.
//!
//! `xdg_toplevel.set_app_id` and placement name an `xdg_toplevel`, while the
//! Aura surface and keyboard focus name a `wl_surface`. The links come from
//! two requests: `xdg_wm_base.get_xdg_surface(xdg_surface, wl_surface)` and
//! `xdg_surface.get_toplevel(xdg_toplevel)`. [`XdgRoles`] keeps one record
//! per `xdg_surface`, so the `xdg_toplevel` → `wl_surface` link is derived
//! rather than stored, and each destructor ends exactly the roles it
//! removes. All IDs are guest IDs.

use std::collections::HashMap;

/// The role an `xdg_surface` gives its `wl_surface`.
#[derive(Debug, Clone, Copy)]
struct XdgSurfaceRole {
    wl_surface: u32,
    /// Set once `get_toplevel` has been forwarded, until the toplevel or
    /// this record is destroyed.
    toplevel: Option<u32>,
}

/// One connection's guest `xdg_surface` and `xdg_toplevel` links.
#[derive(Debug, Default)]
pub(crate) struct XdgRoles {
    by_xdg_surface: HashMap<u32, XdgSurfaceRole>,
}

impl XdgRoles {
    /// Record `xdg_wm_base.get_xdg_surface`.
    pub(crate) fn add_xdg_surface(&mut self, xdg_surface: u32, wl_surface: u32) {
        self.by_xdg_surface.insert(
            xdg_surface,
            XdgSurfaceRole {
                wl_surface,
                toplevel: None,
            },
        );
    }

    /// Record a forwarded `xdg_surface.get_toplevel`.
    ///
    /// # Returns
    ///
    /// The toplevel's wl_surface, or `None`, recording nothing, when the
    /// xdg_surface is unknown.
    pub(crate) fn add_toplevel(&mut self, xdg_surface: u32, xdg_toplevel: u32) -> Option<u32> {
        let role = self.by_xdg_surface.get_mut(&xdg_surface)?;
        // A second get_toplevel is `already_constructed`, which ends the
        // session; the replaced toplevel's own destructor releases it.
        role.toplevel = Some(xdg_toplevel);
        Some(role.wl_surface)
    }

    /// The xdg_toplevel created from an xdg_surface, if any.
    pub(crate) fn toplevel_of_xdg_surface(&self, xdg_surface: u32) -> Option<u32> {
        self.by_xdg_surface.get(&xdg_surface)?.toplevel
    }

    /// The wl_surface an xdg_toplevel gives its role to.
    pub(crate) fn wl_surface_of_toplevel(&self, xdg_toplevel: u32) -> Option<u32> {
        self.by_xdg_surface
            .values()
            .find(|role| role.toplevel == Some(xdg_toplevel))
            .map(|role| role.wl_surface)
    }

    /// The xdg_toplevel whose role a wl_surface has, if any.
    pub(crate) fn toplevel_on_wl_surface(&self, wl_surface: u32) -> Option<u32> {
        self.by_xdg_surface
            .values()
            .filter(|role| role.wl_surface == wl_surface)
            .find_map(|role| role.toplevel)
    }

    /// Every recorded toplevel with its wl_surface.
    pub(crate) fn toplevels(&self) -> Vec<(u32, u32)> {
        self.by_xdg_surface
            .values()
            .filter_map(|role| Some((role.toplevel?, role.wl_surface)))
            .collect()
    }

    /// Forget a destroyed xdg_toplevel. Its xdg_surface stays recorded.
    pub(crate) fn remove_toplevel(&mut self, xdg_toplevel: u32) {
        for role in self.by_xdg_surface.values_mut() {
            if role.toplevel == Some(xdg_toplevel) {
                role.toplevel = None;
            }
        }
    }

    /// Forget a destroyed xdg_surface with its toplevel link.
    ///
    /// # Returns
    ///
    /// The toplevel whose role ended with it, which the caller must tear
    /// down.
    #[must_use = "the returned toplevel's role has ended"]
    pub(crate) fn remove_xdg_surface(&mut self, xdg_surface: u32) -> Option<u32> {
        self.by_xdg_surface.remove(&xdg_surface)?.toplevel
    }

    /// Forget every role of a destroyed wl_surface, so a later guest ID
    /// reuse cannot link a new role to it.
    ///
    /// # Returns
    ///
    /// The toplevels whose roles ended, which the caller must tear down.
    #[must_use = "the returned toplevels' roles have ended"]
    pub(crate) fn remove_wl_surface(&mut self, wl_surface: u32) -> Vec<u32> {
        let mut ended = Vec::new();
        self.by_xdg_surface.retain(|_, role| {
            if role.wl_surface != wl_surface {
                return true;
            }
            ended.extend(role.toplevel);
            false
        });
        ended
    }

    /// Record a whole toplevel role in one call.
    #[cfg(test)]
    pub(crate) fn add_toplevel_role_for_test(
        &mut self,
        xdg_surface: u32,
        wl_surface: u32,
        xdg_toplevel: u32,
    ) {
        self.add_xdg_surface(xdg_surface, wl_surface);
        self.add_toplevel(xdg_surface, xdg_toplevel);
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_xdg_surface.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XDG_SURFACE: u32 = 11;
    const WL_SURFACE: u32 = 12;
    const XDG_TOPLEVEL: u32 = 13;

    fn with_toplevel() -> XdgRoles {
        let mut roles = XdgRoles::default();
        roles.add_xdg_surface(XDG_SURFACE, WL_SURFACE);
        assert_eq!(
            roles.add_toplevel(XDG_SURFACE, XDG_TOPLEVEL),
            Some(WL_SURFACE)
        );
        roles
    }

    #[test]
    fn toplevel_links_resolve_in_every_direction() {
        let roles = with_toplevel();
        assert_eq!(
            roles.toplevel_of_xdg_surface(XDG_SURFACE),
            Some(XDG_TOPLEVEL)
        );
        assert_eq!(roles.wl_surface_of_toplevel(XDG_TOPLEVEL), Some(WL_SURFACE));
        assert_eq!(roles.toplevel_on_wl_surface(WL_SURFACE), Some(XDG_TOPLEVEL));
        assert_eq!(roles.toplevels(), vec![(XDG_TOPLEVEL, WL_SURFACE)]);
    }

    #[test]
    fn toplevel_of_an_unknown_xdg_surface_is_not_recorded() {
        let mut roles = XdgRoles::default();
        assert_eq!(roles.add_toplevel(XDG_SURFACE, XDG_TOPLEVEL), None);
        assert!(roles.is_empty());
    }

    #[test]
    fn toplevel_destroy_keeps_the_xdg_surface() {
        let mut roles = with_toplevel();
        roles.remove_toplevel(XDG_TOPLEVEL);
        assert_eq!(roles.wl_surface_of_toplevel(XDG_TOPLEVEL), None);
        assert_eq!(roles.toplevel_of_xdg_surface(XDG_SURFACE), None);
        assert_eq!(
            roles.add_toplevel(XDG_SURFACE, XDG_TOPLEVEL + 1),
            Some(WL_SURFACE)
        );
    }

    #[test]
    fn xdg_surface_destroy_ends_its_toplevel() {
        let mut roles = with_toplevel();
        assert_eq!(roles.remove_xdg_surface(XDG_SURFACE), Some(XDG_TOPLEVEL));
        assert_eq!(roles.wl_surface_of_toplevel(XDG_TOPLEVEL), None);
        assert!(roles.is_empty());
        assert_eq!(roles.remove_xdg_surface(XDG_SURFACE), None);
    }

    #[test]
    fn wl_surface_destroy_ends_only_its_roles() {
        let mut roles = with_toplevel();
        roles.add_xdg_surface(XDG_SURFACE + 10, WL_SURFACE + 10);
        assert_eq!(roles.remove_wl_surface(WL_SURFACE), vec![XDG_TOPLEVEL]);
        assert_eq!(roles.toplevel_on_wl_surface(WL_SURFACE), None);
        assert_eq!(roles.remove_wl_surface(WL_SURFACE + 10), Vec::<u32>::new());
        assert!(roles.is_empty());
    }
}
