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

//! Pure placement inputs and validated operation plans.
//!
//! Types in this module contain no connection maps and perform no wire I/O.
//! `WindowPlacementState` is the only component that can construct a plan
//! from live lifecycle state; adapters may only serialize the resulting value.

/// Geometry operation selected for one accepted shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPlacementGeometry {
    /// Send a direct Aura bounds request.
    Bounds,
    /// Resize at the current origin and then issue the self-parent probe.
    SelfParent {
        current_origin: (i32, i32),
        relative_position: (i32, i32),
    },
    /// Send a remote-shell v2 bounds request to the host-managed surface.
    RemoteShell,
}

/// Native/ARC identity transition required by transient ARC placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransientArcIdentity {
    arc_application_id: String,
    wl_surface_guest_id: u32,
}

impl TransientArcIdentity {
    /// Construct the identity transition owned by the placement state.
    pub(super) fn new(arc_application_id: String, wl_surface_guest_id: u32) -> Self {
        Self {
            arc_application_id,
            wl_surface_guest_id,
        }
    }

    /// Return the temporary ARC application ID to install on the Aura surface.
    pub(crate) fn arc_application_id(&self) -> &str {
        &self.arc_application_id
    }

    /// Return the guest surface whose native identity must be restored.
    pub(crate) const fn wl_surface_guest_id(&self) -> u32 {
        self.wl_surface_guest_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(arc_application_id: String, wl_surface_guest_id: u32) -> Self {
        Self::new(arc_application_id, wl_surface_guest_id)
    }
}

/// All object identities required to execute one placement operation.
///
/// The state owner resolves this tuple once from live connection maps. Keeping
/// the identities together prevents a caller from pairing a plan for one
/// surface with Aura objects belonging to another window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementTarget {
    guest_xdg_toplevel_id: u32,
    wl_surface_guest_id: u32,
    wl_surface_host_id: u32,
    host_xdg_toplevel_id: u32,
    zaura_toplevel_host_id: u32,
    zaura_surface_host_id: u32,
    zaura_surface_version: u32,
}

impl PlacementTarget {
    /// Construct one target after the placement state has resolved all live
    /// guest/host associations.
    pub(super) const fn new(
        guest_xdg_toplevel_id: u32,
        wl_surface_guest_id: u32,
        wl_surface_host_id: u32,
        host_xdg_toplevel_id: u32,
        zaura_toplevel_host_id: u32,
        zaura_surface_host_id: u32,
        zaura_surface_version: u32,
    ) -> Self {
        Self {
            guest_xdg_toplevel_id,
            wl_surface_guest_id,
            wl_surface_host_id,
            host_xdg_toplevel_id,
            zaura_toplevel_host_id,
            zaura_surface_host_id,
            zaura_surface_version,
        }
    }

    pub(crate) const fn guest_xdg_toplevel_id(self) -> u32 {
        self.guest_xdg_toplevel_id
    }

    pub(crate) const fn wl_surface_guest_id(self) -> u32 {
        self.wl_surface_guest_id
    }

    pub(crate) const fn wl_surface_host_id(self) -> u32 {
        self.wl_surface_host_id
    }

    pub(crate) const fn host_xdg_toplevel_id(self) -> u32 {
        self.host_xdg_toplevel_id
    }

    pub(crate) const fn zaura_toplevel_host_id(self) -> u32 {
        self.zaura_toplevel_host_id
    }

    pub(crate) const fn zaura_surface_host_id(self) -> u32 {
        self.zaura_surface_host_id
    }

    pub(crate) const fn zaura_surface_version(self) -> u32 {
        self.zaura_surface_version
    }

    #[cfg(test)]
    pub(crate) const fn for_test(
        guest_xdg_toplevel_id: u32,
        wl_surface_guest_id: u32,
        wl_surface_host_id: u32,
        host_xdg_toplevel_id: u32,
        zaura_toplevel_host_id: u32,
        zaura_surface_host_id: u32,
        zaura_surface_version: u32,
    ) -> Self {
        Self::new(
            guest_xdg_toplevel_id,
            wl_surface_guest_id,
            wl_surface_host_id,
            host_xdg_toplevel_id,
            zaura_toplevel_host_id,
            zaura_surface_host_id,
            zaura_surface_version,
        )
    }
}

/// Cleanup serialized only after a placement barrier completes.
///
/// This is plan data rather than handler state: the state owner decides which
/// cleanup belongs to a placement generation, and the wire adapter only
/// encodes the selected operation after the matching barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlacementBarrierCleanup {
    /// Retain the self-parent relationship while the ordered cleanup barrier
    /// refreshes host activation/IME state.
    RetainSelfParent { zaura_surface_id: u32 },
    /// Restore the native Guest OS ID after a transient ARC bounds request.
    ///
    /// The bounds backend does not install a parent relationship, so its
    /// cleanup must not emit a speculative `set_parent(NULL)`. That request
    /// can create an unnecessary host focus/IME generation transition.
    RestoreNativeApplicationId {
        zaura_surface_id: u32,
        wl_surface_guest_id: u32,
    },
    /// Refresh the focused host text-input generation after a metadata
    /// transition has completed its own host-stream barrier.
    ///
    /// This is deliberately a separate cleanup phase.  Aura application-ID
    /// changes are asynchronous host state transitions; sending
    /// `text_input.deactivate` in the same batch can race the identity
    /// transition and leave the host IME permanently unusable.
    RefreshHostActivation { guest_surface_id: u32 },
}

/// Fully validated placement operation returned by the state owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowPlacementPlan {
    target: PlacementTarget,
    output_host_id: u32,
    bounds: (i32, i32, i32, i32),
    geometry: WindowPlacementGeometry,
    transient_arc_identity: Option<TransientArcIdentity>,
    /// Cleanup to enqueue after the host barrier for this exact operation.
    barrier_cleanup: Option<PlacementBarrierCleanup>,
}

impl WindowPlacementPlan {
    /// Construct a plan after the parent state has resolved all live IDs.
    pub(super) fn new(
        target: PlacementTarget,
        output_host_id: u32,
        bounds: (i32, i32, i32, i32),
        geometry: WindowPlacementGeometry,
        transient_arc_identity: Option<TransientArcIdentity>,
        barrier_cleanup: Option<PlacementBarrierCleanup>,
    ) -> Self {
        Self {
            target,
            output_host_id,
            bounds,
            geometry,
            transient_arc_identity,
            barrier_cleanup,
        }
    }

    /// Return the complete object-identity tuple resolved by placement state.
    pub(crate) const fn target(&self) -> PlacementTarget {
        self.target
    }

    /// Return the requested screen-space bounds.
    pub(crate) const fn bounds(&self) -> (i32, i32, i32, i32) {
        self.bounds
    }

    /// Return the host output receiving the bounds request.
    pub(crate) const fn output_host_id(&self) -> u32 {
        self.output_host_id
    }

    /// Return the geometry operation selected for this plan.
    pub(crate) const fn geometry(&self) -> WindowPlacementGeometry {
        self.geometry
    }

    /// Return the temporary identity transition, if this is transient ARC.
    pub(crate) fn transient_arc_identity(&self) -> Option<&TransientArcIdentity> {
        self.transient_arc_identity.as_ref()
    }

    /// Return cleanup that must be serialized after the matching host barrier.
    pub(crate) fn barrier_cleanup(&self) -> Option<&PlacementBarrierCleanup> {
        self.barrier_cleanup.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        target: PlacementTarget,
        output_host_id: u32,
        bounds: (i32, i32, i32, i32),
        geometry: WindowPlacementGeometry,
        transient_arc_identity: Option<TransientArcIdentity>,
        barrier_cleanup: Option<PlacementBarrierCleanup>,
    ) -> Self {
        Self::new(
            target,
            output_host_id,
            bounds,
            geometry,
            transient_arc_identity,
            barrier_cleanup,
        )
    }

    /// Verify the internal pairing and sequencing invariants of one plan.
    ///
    /// Plans are normally constructed by the parent `WindowPlacementState`, but the
    /// wire adapter intentionally accepts a complete value so it can remain
    /// independent of placement maps. This check is the defensive boundary
    /// between those layers: it rejects a plan whose cleanup or transient
    /// identity names a different surface, or whose self-parent delta cannot
    /// reach the requested screen origin.
    pub(crate) fn is_well_formed(&self) -> bool {
        if self.bounds.2 <= 0 || self.bounds.3 <= 0 {
            return false;
        }

        match (
            self.geometry,
            self.transient_arc_identity.as_ref(),
            self.barrier_cleanup.as_ref(),
        ) {
            (WindowPlacementGeometry::Bounds, None, None) => true,
            (
                WindowPlacementGeometry::Bounds,
                Some(identity),
                Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                    zaura_surface_id,
                    wl_surface_guest_id,
                }),
            ) => {
                !identity.arc_application_id().is_empty()
                    && identity.wl_surface_guest_id() == self.target.wl_surface_guest_id
                    && *zaura_surface_id == self.target.zaura_surface_host_id
                    && *wl_surface_guest_id == self.target.wl_surface_guest_id
            }
            (
                WindowPlacementGeometry::SelfParent {
                    current_origin,
                    relative_position,
                },
                None,
                Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
            ) => {
                *zaura_surface_id == self.target.zaura_surface_host_id
                    && current_origin.0.checked_add(relative_position.0) == Some(self.bounds.0)
                    && current_origin.1.checked_add(relative_position.1) == Some(self.bounds.1)
            }
            (WindowPlacementGeometry::RemoteShell, None, None) => true,
            _ => false,
        }
    }
}

/// Why a shortcut could not produce a placement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPlacementPlanError {
    Disabled,
    /// The requested rectangle is already pending or is the last
    /// authoritative rectangle completed by this backend. The shortcut is
    /// still consumed, but no compositor wire request is necessary.
    AlreadyAtTarget,
    NoUsableOutput,
    UnsupportedGeometry,
    UnsupportedSurfaceVersion,
    TargetUnavailable,
    OriginUnknown,
    CoordinateOverflow,
    NativeApplicationIdMissing,
    ArcTaskIdUnavailable,
}

impl WindowPlacementPlanError {
    /// Whether the keyboard event must be consumed while state catches up.
    pub(crate) const fn consumes_shortcut(self) -> bool {
        matches!(self, Self::OriginUnknown | Self::AlreadyAtTarget)
    }
}

/// Host output geometry used by compositor-owned window layout requests.
///
/// `wl_output.mode` reports pixel dimensions while Aura window bounds use
/// logical screen coordinates. `scale` converts the former into the latter;
/// output insets remove shelf/non-work-area margins when known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct OutputState {
    pub(crate) mode_width: i32,
    pub(crate) mode_height: i32,
    pub(crate) scale: i32,
    pub(crate) insets_top: i32,
    pub(crate) insets_left: i32,
    pub(crate) insets_bottom: i32,
    pub(crate) insets_right: i32,
}

impl OutputState {
    pub(crate) fn work_area(self) -> Option<(i32, i32, i32, i32)> {
        let scale = self.scale.max(1);
        let width = self.mode_width.checked_div(scale)?;
        let height = self.mode_height.checked_div(scale)?;
        let x = self.insets_left;
        let y = self.insets_top;
        let width = width
            .checked_sub(self.insets_left)?
            .checked_sub(self.insets_right)?;
        let height = height
            .checked_sub(self.insets_top)?
            .checked_sub(self.insets_bottom)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some((x, y, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        OutputState, PlacementBarrierCleanup, PlacementTarget, TransientArcIdentity,
        WindowPlacementGeometry, WindowPlacementPlan,
    };

    fn target() -> PlacementTarget {
        PlacementTarget::for_test(1, 2, 3, 4, 5, 6, 5)
    }

    fn base_plan() -> WindowPlacementPlan {
        WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            None,
            None,
        )
    }

    #[test]
    fn work_area_converts_scale_and_insets() {
        let output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            scale: 2,
            insets_top: 20,
            insets_left: 10,
            insets_bottom: 30,
            insets_right: 40,
        };
        assert_eq!(output.work_area(), Some((10, 20, 1870, 1030)));
    }

    #[test]
    fn invalid_work_area_is_rejected() {
        assert_eq!(
            OutputState {
                mode_width: 100,
                mode_height: 100,
                scale: 1,
                insets_top: 60,
                insets_left: 0,
                insets_bottom: 50,
                insets_right: 0,
            }
            .work_area(),
            None
        );
    }

    #[test]
    fn zero_scale_uses_protocol_safe_default() {
        assert_eq!(
            OutputState {
                mode_width: 800,
                mode_height: 600,
                scale: 0,
                ..OutputState::default()
            }
            .work_area(),
            Some((0, 0, 800, 600))
        );
    }

    #[test]
    fn plan_invariants_accept_direct_self_parent_and_transient_sequences() {
        assert!(base_plan().is_well_formed());

        let mut self_parent = base_plan();
        self_parent.geometry = WindowPlacementGeometry::SelfParent {
            current_origin: (50, 100),
            relative_position: (50, 100),
        };
        self_parent.barrier_cleanup = Some(PlacementBarrierCleanup::RetainSelfParent {
            zaura_surface_id: 6,
        });
        assert!(self_parent.is_well_formed());

        let transient = WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            Some(TransientArcIdentity::for_test(
                "org.chromium.arc.2000000001".to_string(),
                2,
            )),
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: 6,
                wl_surface_guest_id: 2,
            }),
        );
        assert!(transient.is_well_formed());
    }

    #[test]
    fn plan_invariants_reject_mismatched_lifecycle_data() {
        let malformed = WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 0, 600),
            WindowPlacementGeometry::Bounds,
            None,
            None,
        );
        assert!(!malformed.is_well_formed());

        let malformed = WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: 99,
            }),
        );
        assert!(!malformed.is_well_formed());

        let malformed = WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            Some(TransientArcIdentity::for_test(
                "org.chromium.arc.2000000001".to_string(),
                99,
            )),
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: 6,
                wl_surface_guest_id: 2,
            }),
        );
        assert!(!malformed.is_well_formed());

        let malformed = WindowPlacementPlan::for_test(
            target(),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::SelfParent {
                current_origin: (i32::MAX, 0),
                relative_position: (1, 200),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: 6,
            }),
        );
        assert!(!malformed.is_well_formed());
    }
}
