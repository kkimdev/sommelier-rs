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

mod plan;
mod runtime;
mod support;
mod transaction;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::warn;

use super::ShadowTable;
use crate::accelerator::Accelerator;
#[cfg(test)]
use crate::window_shortcuts::ShortcutConfigHandle;
use crate::window_shortcuts::{NormalizedRect, ShortcutConfig};

use self::support::{
    AuraShellBinding, BidirectionalLinks, GtkShellState, GtkSurfaceState, OutputRecord,
    PlacementBarrierRegistry, RemoteShellBinding, SurfaceApplicationState, ToplevelPlacementState,
};
use self::transaction::{
    ConfigureToken, DeferredPromotionRollback, HostResizeResult, OriginResult, SelfParentPhase,
};

pub(crate) use self::plan::{
    OutputState, PlacementBarrierCleanup, PlacementTarget, TransientArcIdentity,
    WindowPlacementGeometry, WindowPlacementPlan, WindowPlacementPlanError,
};
#[cfg(test)]
pub(crate) use self::runtime::ShortcutReloadResult;
#[cfg(test)]
pub(crate) use self::runtime::{
    resolve_vm_identifier, ARC_TASK_ID_POOL_END, ARC_TASK_ID_POOL_START,
};
pub(crate) use self::runtime::{
    WindowArcIdLifetime, WindowGeometryMethod, WindowHostPolicy, WindowPlacementMode,
    WindowPlacementRuntime, WindowPlacementRuntimeHandle, ARC_TASK_APPLICATION_ID_PREFIX,
};
pub(crate) use self::support::{PlacementBarrierCompletion, XdgToplevelRelease};

/// Host surface, authoritative origin, and relative self-parent delta that
/// become available after the resize phase is acknowledged.
pub(crate) type PendingSelfParentMove = (u32, (i32, i32), (i32, i32));
/// Deferred self-parent target plus the host surface and origin needed to
/// serialize its next parent request.
pub(crate) type DeferredSelfParentMove = (u32, (i32, i32), (i32, i32), (i32, i32, i32, i32));

/// Maximum client-size adjustment ChromeOS may make for a decorated window.
///
/// `zaura_toplevel.configure` reports the client rectangle, while placement
/// targets are expressed in screen/work-area coordinates. A decorated window
/// can therefore report a slightly smaller client size than the requested
/// work-area height. The custom host observed a 48 px adjustment; this bound
/// leaves room for frame variants without accepting an unrelated stale
/// configure from a previous half/full-screen size.
const MAX_HOST_CLIENT_SIZE_ADJUSTMENT: i32 = 256;

/// All mutable state owned by the window-placement feature.
///
/// The maps are private by design. In particular, callers must not update an
/// origin without also applying the pending-target rule, must not mutate one
/// side of an association without its reverse owner, and must not remove a
/// barrier mapping without retiring its active-to-toplevel entry.
#[derive(Debug)]
pub(crate) struct WindowPlacementState {
    runtime: WindowPlacementRuntimeHandle,
    aura_shell: Option<AuraShellBinding>,
    remote_shell: Option<RemoteShellBinding>,
    /// Remote-shell manager generations whose advertised global disappeared
    /// while one or more child remote surfaces remained alive.
    ///
    /// The host manager is a real protocol object with a lifetime independent
    /// of its registry advertisement. Keep its binding metadata until the
    /// final child is destroyed so the manager can then be addressed by its
    /// wire destructor.
    retired_remote_shells: HashMap<u32, RemoteShellBinding>,
    /// Native and compatibility application IDs for each guest surface.
    ///
    /// The XDG role always retains the native identity even when the Aura
    /// surface uses the persistent ARC task-form compatibility ID. The two
    /// values share one record so they cannot acquire independent lifetimes.
    application_ids: HashMap<u32, SurfaceApplicationState>,
    aura_surface_links: BidirectionalLinks,
    aura_toplevel_links: BidirectionalLinks,
    /// Host `wl_output` to its internal `zaura_output` child.
    ///
    /// ChromeOS publishes the usable work-area in the child's `insets`
    /// event.  Keep this association separate from guest-facing output
    /// mappings because the Aura child is host-only.
    aura_output_links: BidirectionalLinks,
    remote_surface_links: BidirectionalLinks,
    remote_toplevel_links: BidirectionalLinks,
    /// Manager generation that owns each host remote-surface child.
    remote_surface_owners: HashMap<u32, u32>,
    /// Number of live remote-surface children for each manager generation.
    remote_shell_child_counts: HashMap<u32, usize>,
    xdg_surface_links: BidirectionalLinks,
    xdg_toplevel_links: BidirectionalLinks,
    /// Synthetic `xdg_surface.configure` serials sent to a guest client.
    ///
    /// These serials are deliberately kept separate from host serials.  A
    /// guest acknowledgement for one of them is consumed locally because no
    /// matching configure exists in the host compositor.
    synthetic_xdg_configure_serials: HashMap<u32, HashSet<u32>>,
    next_synthetic_xdg_configure_serial: u32,
    toplevels: HashMap<u32, ToplevelPlacementState>,
    barriers: PlacementBarrierRegistry,
    outputs: Vec<OutputRecord>,
    gtk_shells: HashMap<u32, GtkShellState>,
    gtk_surfaces: HashMap<u32, GtkSurfaceState>,
    gtk_shell_capability_callbacks: HashMap<u32, u32>,
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
        debug_assert!(self.aura_surface_links.is_consistent());
        debug_assert!(self.aura_toplevel_links.is_consistent());
        debug_assert!(self.aura_output_links.is_consistent());
        debug_assert!(self.remote_surface_links.is_consistent());
        debug_assert!(self.remote_toplevel_links.is_consistent());
        debug_assert!(self
            .remote_surface_owners
            .iter()
            .all(|(remote_surface_id, _manager_id)| self
                .remote_surface_links
                .get_reverse(*remote_surface_id)
                .is_some()));
        debug_assert!(self
            .remote_shell_child_counts
            .values()
            .all(|count| *count > 0));
        debug_assert!(self
            .retired_remote_shells
            .keys()
            .all(|manager_id| self.remote_shell_has_children(*manager_id)));
        debug_assert_eq!(
            self.remote_surface_owners.len(),
            self.remote_shell_child_counts.values().sum::<usize>()
        );
        debug_assert!(self.xdg_surface_links.is_consistent());
        debug_assert!(self.xdg_toplevel_links.is_consistent());
        debug_assert!(self.outputs.iter().enumerate().all(|(index, output)| {
            self.outputs[..index]
                .iter()
                .all(|previous| previous.host_id != output.host_id)
        }));
        debug_assert!(self.gtk_shells.iter().all(|(shell_id, shell)| {
            shell.surfaces.iter().all(|surface_id| {
                self.gtk_surfaces
                    .get(surface_id)
                    .is_some_and(|surface| surface.shell_id == *shell_id)
            })
        }));
        debug_assert!(self.gtk_surfaces.iter().all(|(surface_id, surface)| {
            self.gtk_shells
                .get(&surface.shell_id)
                .is_some_and(|shell| shell.surfaces.contains(surface_id))
        }));
        debug_assert!(self.toplevels.keys().all(|zaura_toplevel_host_id| {
            self.aura_toplevel_links
                .get_reverse(*zaura_toplevel_host_id)
                .is_some()
        }));
        debug_assert!(self.barriers.is_consistent(|zaura_toplevel_host_id| {
            self.aura_toplevel_links
                .get_reverse(zaura_toplevel_host_id)
                .is_some()
        }));
    }

    /// Create empty placement state for one connection.
    #[cfg(test)]
    pub(crate) fn new(mode: WindowPlacementMode) -> Self {
        Self::with_runtime(WindowPlacementRuntime::new(
            mode,
            ShortcutConfigHandle::disabled(),
            None,
            Arc::new(Vec::new()),
            None,
        ))
    }

    /// Create placement state with the process-wide runtime owner.
    ///
    /// The runtime is shared by every client connection, while all geometry,
    /// object associations, and lifecycle state remain connection-local.
    pub(crate) fn with_runtime(runtime: WindowPlacementRuntimeHandle) -> Self {
        Self {
            runtime,
            aura_shell: None,
            remote_shell: None,
            retired_remote_shells: HashMap::new(),
            application_ids: HashMap::new(),
            aura_surface_links: BidirectionalLinks::default(),
            aura_toplevel_links: BidirectionalLinks::default(),
            aura_output_links: BidirectionalLinks::default(),
            remote_surface_links: BidirectionalLinks::default(),
            remote_toplevel_links: BidirectionalLinks::default(),
            remote_surface_owners: HashMap::new(),
            remote_shell_child_counts: HashMap::new(),
            xdg_surface_links: BidirectionalLinks::default(),
            xdg_toplevel_links: BidirectionalLinks::default(),
            synthetic_xdg_configure_serials: HashMap::new(),
            next_synthetic_xdg_configure_serial: 0xf000_0000,
            toplevels: HashMap::new(),
            barriers: PlacementBarrierRegistry::default(),
            outputs: Vec::new(),
            gtk_shells: HashMap::new(),
            gtk_surfaces: HashMap::new(),
            gtk_shell_capability_callbacks: HashMap::new(),
        }
    }

    /// Return the most recent mode-independent shortcut capability.
    pub(crate) fn handles_shortcuts(&self) -> bool {
        self.runtime.mode().handles_shortcuts()
    }

    /// Return the immutable process-wide placement mode for diagnostics.
    pub(crate) fn mode(&self) -> WindowPlacementMode {
        self.runtime.mode()
    }

    /// Return whether this connection uses the ARC application namespace.
    pub(crate) fn uses_arc_policy(&self) -> bool {
        self.runtime.mode().uses_arc_policy()
    }

    /// Return the configured ARC application-ID lifetime behavior.
    pub(crate) fn arc_id_lifetime(&self) -> WindowArcIdLifetime {
        self.runtime.mode().arc_id_lifetime()
    }

    /// Return whether placement must install and then restore ARC metadata.
    pub(crate) fn uses_transient_arc_id(&self) -> bool {
        self.runtime.mode().uses_transient_arc_id()
    }

    /// Return whether this connection uses direct Aura bounds.
    pub(crate) fn uses_bounds(&self) -> bool {
        self.runtime.mode().uses_bounds()
    }

    /// Return whether this connection uses the experimental self-parent path.
    pub(crate) fn uses_self_parent(&self) -> bool {
        self.runtime.mode().uses_self_parent()
    }

    pub(crate) fn uses_remote_shell(&self) -> bool {
        self.runtime.mode().uses_remote_shell()
    }

    /// Return the internally bound Aura shell manager, if one is live.
    pub(crate) const fn aura_shell_id(&self) -> Option<u32> {
        match self.aura_shell {
            Some(binding) => Some(binding.host_id),
            None => None,
        }
    }

    /// Return the negotiated version of the internally bound Aura shell.
    pub(crate) const fn aura_shell_version(&self) -> u32 {
        match self.aura_shell {
            Some(binding) => binding.version,
            None => 0,
        }
    }

    /// Return the host registry global that produced the Aura shell binding.
    pub(crate) const fn aura_shell_global_name(&self) -> Option<u32> {
        match self.aura_shell {
            Some(binding) => Some(binding.global_name),
            None => None,
        }
    }

    /// Return the internally bound remote-shell manager, if one is live.
    pub(crate) const fn remote_shell_id(&self) -> Option<u32> {
        match self.remote_shell {
            Some(binding) => Some(binding.host_id),
            None => None,
        }
    }

    pub(crate) const fn remote_shell_version(&self) -> u32 {
        match self.remote_shell {
            Some(binding) => binding.version,
            None => 0,
        }
    }

    pub(crate) const fn remote_shell_global_name(&self) -> Option<u32> {
        match self.remote_shell {
            Some(binding) => Some(binding.global_name),
            None => None,
        }
    }

    #[must_use = "a live remote-shell generation must not be overwritten"]
    pub(crate) fn set_remote_shell_binding(
        &mut self,
        host_id: u32,
        global_name: u32,
        version: u32,
    ) -> bool {
        if self.remote_shell.is_some() {
            return false;
        }
        self.remote_shell = Some(RemoteShellBinding {
            host_id,
            global_name,
            version,
        });
        self.debug_assert_consistent();
        true
    }

    pub(crate) fn take_remote_shell_for_global(&mut self, global_name: u32) -> Option<(u32, u32)> {
        if self.remote_shell?.global_name != global_name {
            return None;
        }
        let binding = self.remote_shell.take()?;
        if self.remote_shell_has_children(binding.host_id) {
            // `global_remove` invalidates only the advertisement. Keep the
            // manager generation addressable until every child role created
            // through it has completed its own destructor lifecycle.
            self.retired_remote_shells.insert(binding.host_id, binding);
        }
        self.debug_assert_consistent();
        Some((binding.host_id, binding.version))
    }

    /// Publish one complete Aura shell binding generation.
    ///
    /// Keeping ID, source global, and version in one record prevents a global
    /// replacement from accidentally retaining the old version or routing
    /// requests through an object whose manager has already been released.
    ///
    /// A live generation cannot be replaced in place. The caller must retire
    /// the old generation first so its host object and child lifetimes remain
    /// reachable until their protocol teardown completes.
    #[must_use = "a live Aura shell generation must not be overwritten"]
    pub(crate) fn set_aura_shell_binding(
        &mut self,
        host_id: u32,
        global_name: u32,
        version: u32,
    ) -> bool {
        if self.aura_shell.is_some() {
            return false;
        }
        self.aura_shell = Some(AuraShellBinding {
            host_id,
            global_name,
            version,
        });
        self.debug_assert_consistent();
        true
    }

    /// Retire the Aura shell only when the named global owns the live binding.
    pub(crate) fn take_aura_shell_for_global(&mut self, global_name: u32) -> Option<(u32, u32)> {
        if self.aura_shell?.global_name != global_name {
            return None;
        }
        let binding = self.aura_shell.take()?;
        self.debug_assert_consistent();
        Some((binding.host_id, binding.version))
    }

    #[cfg(test)]
    pub(crate) fn clear_aura_shell_for_test(&mut self) {
        self.aura_shell = None;
    }

    #[cfg(test)]
    pub(crate) fn set_aura_shell_binding_for_test(&mut self, host_id: u32, version: u32) {
        self.aura_shell = Some(AuraShellBinding {
            host_id,
            global_name: 0,
            version,
        });
        self.debug_assert_consistent();
    }

    /// Take one immutable binding generation for a key event.
    pub(crate) fn shortcut_config_snapshot(&self) -> Arc<ShortcutConfig> {
        self.runtime.shortcut_config_snapshot()
    }

    /// Return the process-wide host accelerator policy.
    pub(crate) fn host_accelerators(&self) -> &[Accelerator] {
        self.runtime.host_accelerators()
    }

    /// Format a native Wayland application ID in ChromeOS' Guest OS namespace.
    pub(crate) fn native_wayland_app_id(&self, app_id: &str) -> String {
        format!(
            "org.chromium.guest_os.{}.wayland.{}",
            self.runtime.vm_identifier(),
            app_id
        )
    }

    /// Remember the native Guest OS application ID for one guest surface.
    ///
    /// The value is updated whenever XDG or GTK reports a new application
    /// identity. ARC placement leaves this value on the host XDG role while
    /// the Aura surface uses its stable task-form identity.
    pub(crate) fn remember_native_application_id(
        &mut self,
        wl_surface_guest_id: u32,
        application_id: String,
    ) {
        self.application_ids
            .entry(wl_surface_guest_id)
            .or_default()
            .native = Some(application_id);
    }

    /// Return the latest native Guest OS application ID for one surface.
    pub(crate) fn native_application_id(&self, wl_surface_guest_id: u32) -> Option<String> {
        self.application_ids
            .get(&wl_surface_guest_id)
            .and_then(|identities| identities.native.clone())
    }

    /// Register one synthetic GTK shell binding.
    #[must_use = "duplicate GTK shell IDs must not replace live protocol state"]
    pub(crate) fn remember_gtk_shell(&mut self, shell_id: u32) -> bool {
        if self.gtk_shells.contains_key(&shell_id) {
            return false;
        }
        self.gtk_shells.insert(shell_id, GtkShellState::default());
        self.debug_assert_consistent();
        true
    }

    /// Return the startup ID and child-surface IDs owned by a GTK shell.
    pub(crate) fn gtk_shell_startup_and_surfaces(
        &self,
        shell_id: u32,
    ) -> Option<(Option<String>, Vec<u32>)> {
        let shell = self.gtk_shells.get(&shell_id)?;
        let mut surfaces = shell.surfaces.iter().copied().collect::<Vec<_>>();
        surfaces.sort_unstable();
        Some((shell.startup_id.clone(), surfaces))
    }

    /// Update a shell's startup ID and return its current child surfaces.
    pub(crate) fn update_gtk_shell_startup_id(
        &mut self,
        shell_id: u32,
        startup_id: Option<String>,
    ) -> Option<Vec<u32>> {
        let shell = self.gtk_shells.get_mut(&shell_id)?;
        shell.startup_id = startup_id;
        let mut surfaces = shell.surfaces.iter().copied().collect::<Vec<_>>();
        surfaces.sort_unstable();
        self.debug_assert_consistent();
        Some(surfaces)
    }

    /// Return the wl_surface backing one synthetic GTK surface.
    pub(crate) fn wl_surface_for_gtk_surface(&self, gtk_surface_id: u32) -> Option<u32> {
        self.gtk_surfaces
            .get(&gtk_surface_id)
            .map(|surface| surface.wl_surface_id)
    }

    /// Register a GTK surface and link it to its owning shell.
    #[must_use = "the GTK surface association may conflict with a live object"]
    pub(crate) fn remember_gtk_surface(
        &mut self,
        gtk_surface_id: u32,
        shell_id: u32,
        wl_surface_id: u32,
    ) -> bool {
        if !self.gtk_shells.contains_key(&shell_id)
            || self
                .gtk_surfaces
                .get(&gtk_surface_id)
                .is_some_and(|existing| {
                    *existing
                        != (GtkSurfaceState {
                            shell_id,
                            wl_surface_id,
                        })
                })
        {
            return false;
        }
        self.gtk_surfaces.insert(
            gtk_surface_id,
            GtkSurfaceState {
                shell_id,
                wl_surface_id,
            },
        );
        self.gtk_shells
            .get_mut(&shell_id)
            .expect("GTK shell was checked above")
            .surfaces
            .insert(gtk_surface_id);
        self.debug_assert_consistent();
        true
    }

    /// Remove one GTK surface and its reverse shell membership.
    fn take_gtk_surface(&mut self, gtk_surface_id: u32) -> Option<GtkSurfaceState> {
        let surface = self.gtk_surfaces.remove(&gtk_surface_id)?;
        if let Some(shell) = self.gtk_shells.get_mut(&surface.shell_id) {
            shell.surfaces.remove(&gtk_surface_id);
        }
        self.debug_assert_consistent();
        Some(surface)
    }

    /// Remove every synthetic GTK surface backed by one wl_surface.
    pub(crate) fn take_gtk_surfaces_for_wl_surface(&mut self, wl_surface_id: u32) -> Vec<u32> {
        let mut surface_ids = self
            .gtk_surfaces
            .iter()
            .filter_map(|(&gtk_surface_id, surface)| {
                (surface.wl_surface_id == wl_surface_id).then_some(gtk_surface_id)
            })
            .collect::<Vec<_>>();
        surface_ids.sort_unstable();
        for gtk_surface_id in &surface_ids {
            self.take_gtk_surface(*gtk_surface_id);
        }
        self.debug_assert_consistent();
        surface_ids
    }

    /// Return whether a synthetic GTK shell is still registered.
    pub(crate) fn has_gtk_shell(&self, shell_id: u32) -> bool {
        self.gtk_shells.contains_key(&shell_id)
    }

    /// Register a host capability barrier for one GTK shell.
    #[must_use = "a callback ID may only be registered once"]
    pub(crate) fn register_gtk_shell_capability_callback(
        &mut self,
        callback_host_id: u32,
        shell_id: u32,
    ) -> bool {
        if self
            .gtk_shell_capability_callbacks
            .contains_key(&callback_host_id)
            || !self.gtk_shells.contains_key(&shell_id)
        {
            return false;
        }
        self.gtk_shell_capability_callbacks
            .insert(callback_host_id, shell_id);
        self.debug_assert_consistent();
        true
    }

    /// Retire and resolve a completed GTK shell capability barrier.
    pub(crate) fn take_gtk_shell_capability_callback(
        &mut self,
        callback_host_id: u32,
    ) -> Option<u32> {
        let shell_id = self
            .gtk_shell_capability_callbacks
            .remove(&callback_host_id)?;
        self.debug_assert_consistent();
        Some(shell_id)
    }

    #[cfg(test)]
    pub(crate) fn gtk_shell_capability_callback_for_test(&self, shell_id: u32) -> Option<u32> {
        self.gtk_shell_capability_callbacks.iter().find_map(
            |(&callback_host_id, &registered_shell_id)| {
                (registered_shell_id == shell_id).then_some(callback_host_id)
            },
        )
    }

    /// Register a host output once and create its geometry record.
    #[must_use = "a duplicate output must not be forwarded"]
    pub(crate) fn remember_output(&mut self, host_output_id: u32) -> bool {
        if self
            .outputs
            .iter()
            .any(|output| output.host_id == host_output_id)
        {
            return false;
        }
        self.outputs.push(OutputRecord {
            host_id: host_output_id,
            state: OutputState::default(),
        });
        self.debug_assert_consistent();
        true
    }

    /// Retire one host output after the guest releases its wl_output object.
    ///
    /// Host output events can still be queued between the guest release
    /// request and the host's `wl_display.delete_id`. Removing the record at
    /// the request boundary, and making later updates ignore unknown IDs,
    /// prevents a stale event from resurrecting a released output as the
    /// primary placement target.
    #[must_use = "the caller must account for an untracked output release"]
    pub(crate) fn take_output(&mut self, host_output_id: u32) -> bool {
        let Some(index) = self
            .outputs
            .iter()
            .position(|output| output.host_id == host_output_id)
        else {
            return false;
        };
        self.outputs.remove(index);
        self.debug_assert_consistent();
        true
    }

    /// Return all live host output IDs.
    ///
    /// The registry adapter uses this snapshot when the internally-bound Aura
    /// shell arrives after a guest has already bound an output. Returning an
    /// owned vector avoids holding a state borrow while new host child objects
    /// are allocated.
    pub(crate) fn output_host_ids(&self) -> Vec<u32> {
        self.outputs.iter().map(|output| output.host_id).collect()
    }

    /// Associate an internal `zaura_output` child with its host `wl_output`.
    #[must_use = "a duplicate or conflicting Aura output child must not be used"]
    pub(crate) fn remember_aura_output(
        &mut self,
        wl_output_host_id: u32,
        zaura_output_host_id: u32,
    ) -> bool {
        let known_output = self
            .outputs
            .iter()
            .any(|output| output.host_id == wl_output_host_id);
        if !known_output
            || self
                .aura_output_links
                .get_forward(wl_output_host_id)
                .is_some()
        {
            return self.aura_output_links.get_forward(wl_output_host_id)
                == Some(zaura_output_host_id);
        }
        let inserted = self
            .aura_output_links
            .insert(wl_output_host_id, zaura_output_host_id);
        self.debug_assert_consistent();
        inserted
    }

    /// Resolve the internal Aura output child for one host output.
    pub(crate) fn aura_output_for_output(&self, wl_output_host_id: u32) -> Option<u32> {
        self.aura_output_links.get_forward(wl_output_host_id)
    }

    /// Remove and return the internal Aura output child for one host output.
    ///
    /// The caller owns the corresponding host-only `zaura_output.release`
    /// request. Removing the association first makes delayed inset events
    /// harmless while the release is in flight.
    pub(crate) fn take_aura_output_for_output(&mut self, wl_output_host_id: u32) -> Option<u32> {
        let zaura_output_host_id = self.aura_output_links.remove_forward(wl_output_host_id)?;
        self.debug_assert_consistent();
        Some(zaura_output_host_id)
    }

    /// Apply a ChromeOS work-area inset event to its associated output.
    ///
    /// `zaura_output.insets` is already expressed in logical screen
    /// coordinates, so it is stored without output-scale conversion.
    #[must_use = "insets from an unknown or retired Aura output are stale"]
    pub(crate) fn update_output_insets(
        &mut self,
        zaura_output_host_id: u32,
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    ) -> bool {
        let Some(wl_output_host_id) = self.aura_output_links.get_reverse(zaura_output_host_id)
        else {
            return false;
        };
        let Some(output) = self
            .outputs
            .iter_mut()
            .find(|output| output.host_id == wl_output_host_id)
        else {
            return false;
        };
        if [top, left, bottom, right].iter().any(|inset| *inset < 0) {
            log::warn!(
                "Ignoring negative zaura_output.insets for output {}: top={} left={} bottom={} right={}",
                wl_output_host_id,
                top,
                left,
                bottom,
                right
            );
            return false;
        }
        output.state.insets_top = top;
        output.state.insets_left = left;
        output.state.insets_bottom = bottom;
        output.state.insets_right = right;
        self.debug_assert_consistent();
        true
    }

    /// Return a mutable output record only while its host object is live.
    fn output_state_mut(&mut self, host_output_id: u32) -> Option<&mut OutputState> {
        self.outputs
            .iter_mut()
            .find(|output| output.host_id == host_output_id)
            .map(|output| &mut output.state)
    }

    /// Update the current mode for one output.
    ///
    /// Non-current mode events are ignored after the first mode is known, as
    /// required by Wayland's output mode advertisement semantics.
    pub(crate) fn update_output_mode(
        &mut self,
        host_output_id: u32,
        current: bool,
        width: i32,
        height: i32,
    ) {
        let Some(output) = self.output_state_mut(host_output_id) else {
            return;
        };
        let previous = *output;
        if current || output.mode_width == 0 || output.mode_height == 0 {
            output.mode_width = width;
            output.mode_height = height;
        }
        log::debug!(
            "placement output mode host={} current={} incoming={}x{} previous={:?} now={:?}",
            host_output_id,
            current,
            width,
            height,
            previous,
            *output
        );
        self.debug_assert_consistent();
    }

    /// Update the scale advertised by one output.
    pub(crate) fn update_output_scale(&mut self, host_output_id: u32, scale: i32) {
        let Some(output) = self.output_state_mut(host_output_id) else {
            return;
        };
        let previous = *output;
        output.scale = scale;
        log::debug!(
            "placement output scale host={} incoming={} previous={:?} now={:?}",
            host_output_id,
            scale,
            previous,
            *output
        );
        self.debug_assert_consistent();
    }

    /// Return the first usable output in stable host-advertisement order.
    pub(crate) fn primary_output(&self) -> Option<(u32, OutputState)> {
        self.outputs.iter().find_map(|output| {
            output
                .state
                .work_area()
                .map(|_| (output.host_id, output.state))
        })
    }

    /// Convert a normalized shortcut rectangle using the first usable output.
    ///
    /// Output selection and work-area conversion belong to placement state so
    /// handlers cannot accidentally apply a rectangle against a stale or
    /// differently scaled output record.
    pub(crate) fn bounds_for_rect(
        &self,
        rect: NormalizedRect,
    ) -> Option<(u32, (i32, i32, i32, i32))> {
        let (host_id, output) = self.primary_output()?;
        let bounds = rect.to_bounds(output.work_area()?)?;
        Some((host_id, bounds))
    }

    /// Resolve one focused guest role into the complete placement target.
    ///
    /// Guest/host object mappings come from the shadow table, while the
    /// placement-specific Aura/XDG associations come from this state owner.
    /// Keeping the join here prevents a protocol adapter from accidentally
    /// combining IDs belonging to different surfaces.
    fn resolve_target(
        &self,
        shadow_table: &ShadowTable,
        guest_xdg_toplevel_id: u32,
        guest_wl_surface_id: u32,
    ) -> Option<PlacementTarget> {
        if !shadow_table.guest_object_matches(guest_xdg_toplevel_id, "xdg_toplevel")
            || !shadow_table.guest_object_matches(guest_wl_surface_id, "wl_surface")
            || self.wl_surface_for_xdg_toplevel(guest_xdg_toplevel_id) != Some(guest_wl_surface_id)
        {
            return None;
        }
        let host_xdg_toplevel_id = shadow_table.get_host_id(guest_xdg_toplevel_id)?;
        let wl_surface_host_id = shadow_table.get_host_id(guest_wl_surface_id)?;
        if !shadow_table.host_object_matches(host_xdg_toplevel_id, "xdg_toplevel")
            || !shadow_table.host_object_matches(wl_surface_host_id, "wl_surface")
        {
            return None;
        }
        let (zaura_toplevel_host_id, zaura_surface_host_id, zaura_surface_version) = if self
            .uses_remote_shell()
        {
            let remote_surface_host_id = self.remote_surface_for_wl_surface(wl_surface_host_id)?;
            (
                0,
                remote_surface_host_id,
                shadow_table
                    .host_object_version(remote_surface_host_id)
                    .unwrap_or(self.remote_shell_version()),
            )
        } else {
            let zaura_toplevel_host_id =
                self.aura_toplevel_for_xdg_toplevel(guest_xdg_toplevel_id)?;
            let zaura_surface_host_id = self.aura_surface_for_wl_surface(wl_surface_host_id)?;
            let zaura_surface_version = shadow_table
                .host_object_version(zaura_surface_host_id)
                .unwrap_or(self.aura_shell_version());
            (
                zaura_toplevel_host_id,
                zaura_surface_host_id,
                zaura_surface_version,
            )
        };

        Some(PlacementTarget::new(
            guest_xdg_toplevel_id,
            guest_wl_surface_id,
            wl_surface_host_id,
            host_xdg_toplevel_id,
            zaura_toplevel_host_id,
            zaura_surface_host_id,
            zaura_surface_version,
        ))
    }

    /// Verify that a plan still refers to the same live role associations
    /// used when it was prepared.
    ///
    /// The wire adapter calls this immediately before serialization as a
    /// defensive boundary. Keeping the guest/host mapping and placement-link
    /// checks here prevents a caller from pairing IDs from different surfaces
    /// or replaying a plan after teardown.
    pub(crate) fn plan_is_current(
        &self,
        shadow_table: &ShadowTable,
        plan: &WindowPlacementPlan,
    ) -> bool {
        let target = plan.target();
        shadow_table.guest_object_matches(target.guest_xdg_toplevel_id(), "xdg_toplevel")
            && shadow_table.guest_object_matches(target.wl_surface_guest_id(), "wl_surface")
            && self
                .xdg_toplevel_links
                .get_forward(target.guest_xdg_toplevel_id())
                == Some(target.wl_surface_guest_id())
            && if self.uses_remote_shell() {
                target.zaura_toplevel_host_id() == 0
                    && self.remote_surface_for_wl_surface(target.wl_surface_host_id())
                        == Some(target.zaura_surface_host_id())
            } else {
                self.aura_toplevel_links
                    .get_forward(target.guest_xdg_toplevel_id())
                    == Some(target.zaura_toplevel_host_id())
                    && self
                        .aura_surface_links
                        .get_forward(target.wl_surface_host_id())
                        == Some(target.zaura_surface_host_id())
            }
            && shadow_table.get_host_id(target.guest_xdg_toplevel_id())
                == Some(target.host_xdg_toplevel_id())
            && shadow_table.get_host_id(target.wl_surface_guest_id())
                == Some(target.wl_surface_host_id())
            && shadow_table.host_object_matches(target.host_xdg_toplevel_id(), "xdg_toplevel")
            && shadow_table.host_object_matches(target.wl_surface_host_id(), "wl_surface")
            && if self.uses_remote_shell() {
                shadow_table
                    .host_object_matches(target.zaura_surface_host_id(), "zcr_remote_surface_v2")
            } else {
                shadow_table.host_object_matches(target.zaura_toplevel_host_id(), "zaura_toplevel")
                    && shadow_table
                        .host_object_matches(target.zaura_surface_host_id(), "zaura_surface")
            }
    }

    /// Validate and prepare one shortcut placement.
    ///
    /// This is the only state operation that turns a user rectangle into a
    /// backend-specific operation. It owns output selection, origin
    /// validation, and transient ARC identity allocation. The placement
    /// adapter commits the returned plan only after its complete wire batch
    /// and barrier have been queued.
    pub(crate) fn prepare_placement(
        &mut self,
        shadow_table: &ShadowTable,
        guest_xdg_toplevel_id: u32,
        guest_wl_surface_id: u32,
        rect: NormalizedRect,
    ) -> Result<WindowPlacementPlan, WindowPlacementPlanError> {
        if !self.handles_shortcuts() {
            return Err(WindowPlacementPlanError::Disabled);
        }
        let target = self
            .resolve_target(shadow_table, guest_xdg_toplevel_id, guest_wl_surface_id)
            .ok_or(WindowPlacementPlanError::TargetUnavailable)?;
        let Some((output_host_id, bounds)) = self.bounds_for_rect(rect) else {
            return Err(WindowPlacementPlanError::NoUsableOutput);
        };
        if self.uses_self_parent()
            && self.is_self_parent_target_current(
                target.zaura_toplevel_host_id(),
                (bounds.0, bounds.1, bounds.2, bounds.3),
            )
        {
            return Err(WindowPlacementPlanError::AlreadyAtTarget);
        }
        if self.uses_self_parent() && self.uses_transient_arc_id() {
            return Err(WindowPlacementPlanError::UnsupportedGeometry);
        }
        if self.uses_remote_shell() {
            return Ok(WindowPlacementPlan::new(
                target,
                output_host_id,
                bounds,
                WindowPlacementGeometry::RemoteShell,
                None,
                None,
            ));
        }
        // Transient placement needs set_application_id (v5) for the temporary
        // ARC task identity. Reject the whole plan before allocating/queueing
        // anything when that capability is unavailable.
        if self.uses_transient_arc_id() && target.zaura_surface_version() < 5 {
            return Err(WindowPlacementPlanError::UnsupportedSurfaceVersion);
        }

        let geometry = if self.uses_self_parent() {
            if target.zaura_surface_version() < 2 {
                return Err(WindowPlacementPlanError::UnsupportedSurfaceVersion);
            }
            let Some(current_origin) = self.confirmed_origin(target.zaura_toplevel_host_id())
            else {
                return Err(WindowPlacementPlanError::OriginUnknown);
            };
            let Some(relative_x) = bounds.0.checked_sub(current_origin.0) else {
                return Err(WindowPlacementPlanError::CoordinateOverflow);
            };
            let Some(relative_y) = bounds.1.checked_sub(current_origin.1) else {
                return Err(WindowPlacementPlanError::CoordinateOverflow);
            };
            WindowPlacementGeometry::SelfParent {
                current_origin,
                relative_position: (relative_x, relative_y),
            }
        } else if self.uses_bounds() {
            WindowPlacementGeometry::Bounds
        } else {
            return Err(WindowPlacementPlanError::UnsupportedGeometry);
        };

        let transient_arc_identity = if self.uses_transient_arc_id() {
            if self
                .native_application_id(target.wl_surface_guest_id())
                .is_none()
            {
                return Err(WindowPlacementPlanError::NativeApplicationIdMissing);
            }
            let Some(arc_application_id) =
                self.arc_policy_application_id(target.wl_surface_guest_id())
            else {
                return Err(WindowPlacementPlanError::ArcTaskIdUnavailable);
            };
            Some(TransientArcIdentity::new(
                arc_application_id,
                target.wl_surface_guest_id(),
            ))
        } else {
            None
        };

        let barrier_cleanup = if self.uses_self_parent() {
            Some(PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: target.zaura_surface_host_id(),
            })
        } else {
            transient_arc_identity.as_ref().map(|identity| {
                PlacementBarrierCleanup::RestoreNativeApplicationId {
                    zaura_surface_id: target.zaura_surface_host_id(),
                    wl_surface_guest_id: identity.wl_surface_guest_id(),
                }
            })
        };

        Ok(WindowPlacementPlan::new(
            target,
            output_host_id,
            bounds,
            geometry,
            transient_arc_identity,
            barrier_cleanup,
        ))
    }

    /// Commit the state transition represented by an already-queued plan.
    ///
    /// Preparation intentionally does not publish a predicted origin. This
    /// keeps a failed identity/message encoding from leaving lifecycle state
    /// that claims a request was sent. Direct bounds have no local transition;
    /// self-parent plans arm a resize/position state machine only after their
    /// first wire phase is queued.
    #[must_use = "a self-parent prediction may target a released toplevel"]
    pub(crate) fn commit_placement_plan(&mut self, plan: &WindowPlacementPlan) -> bool {
        self.commit_placement_plan_with_configure(plan, None)
    }

    /// Commit a placement plan and, for a self-parent resize, bind the exact
    /// synthetic configure token before publishing the wire batch.
    ///
    /// The optional token is supplied only by the wire adapter after all
    /// messages have been encoded successfully. Binding it here makes the
    /// reducer live before the guest can observe the configure, eliminating
    /// the previous queue-first/state-second race.
    pub(crate) fn commit_placement_plan_with_configure(
        &mut self,
        plan: &WindowPlacementPlan,
        configure: Option<(u32, u32)>,
    ) -> bool {
        match plan.geometry() {
            WindowPlacementGeometry::Bounds | WindowPlacementGeometry::RemoteShell => true,
            WindowPlacementGeometry::SelfParent { .. } => {
                let toplevel_id = plan.target().zaura_toplevel_host_id();
                let target = (
                    plan.bounds().0,
                    plan.bounds().1,
                    plan.bounds().2,
                    plan.bounds().3,
                );
                if self.self_parent_transaction_active(toplevel_id) {
                    // A transaction is already on the host stream. Retain
                    // only the newest user target; the event-driven phase
                    // transition will publish it after the current resize or
                    // cleanup barrier completes.
                    return self.defer_self_parent_target(toplevel_id, target);
                }
                if !self.arm_self_parent_target(
                    toplevel_id,
                    plan.target().zaura_surface_host_id(),
                    target,
                ) {
                    return false;
                }
                if let Some((xdg_surface_id, serial)) = configure {
                    if !self.gate_resize_on_guest_commit(toplevel_id, xdg_surface_id, serial) {
                        let _ = self.abort_self_parent_resize(toplevel_id);
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Record the xdg_surface → wl_surface role association.
    #[must_use = "the XDG surface association may conflict with a live role"]
    pub(crate) fn remember_xdg_surface(
        &mut self,
        xdg_surface_guest_id: u32,
        wl_surface_guest_id: u32,
    ) -> bool {
        if !self
            .xdg_surface_links
            .insert(xdg_surface_guest_id, wl_surface_guest_id)
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Resolve an xdg_surface to its backing wl_surface.
    pub(crate) fn wl_surface_for_xdg_surface(&self, xdg_surface_guest_id: u32) -> Option<u32> {
        self.xdg_surface_links.get_forward(xdg_surface_guest_id)
    }

    /// Resolve the xdg_surface role associated with a guest wl_surface.
    ///
    /// The reverse lookup is used when a compositor-owned placement needs to
    /// send a synthetic configure event to the client.  Keeping this
    /// association in the same bidirectional table prevents a configure from
    /// being sent to an unrelated role after object-ID reuse.
    pub(crate) fn xdg_surface_for_wl_surface(&self, wl_surface_guest_id: u32) -> Option<u32> {
        self.xdg_surface_links.get_reverse(wl_surface_guest_id)
    }

    /// Allocate a serial from the proxy-owned configure namespace.
    ///
    /// Wayland serials are opaque, but host serials are normally low and
    /// monotonically increasing.  Reserving the high `0xf...` range keeps
    /// locally generated acknowledgements distinguishable from host
    /// acknowledgements while the pending set prevents reuse within a live
    /// xdg_surface.
    pub(crate) fn allocate_synthetic_xdg_configure_serial(
        &mut self,
        xdg_surface_guest_id: u32,
    ) -> Option<u32> {
        self.xdg_surface_links.get_forward(xdg_surface_guest_id)?;

        for _ in 0..0x1000 {
            let serial = self.next_synthetic_xdg_configure_serial;
            self.next_synthetic_xdg_configure_serial =
                self.next_synthetic_xdg_configure_serial.wrapping_add(1);
            if self.next_synthetic_xdg_configure_serial < 0xf000_0000 {
                self.next_synthetic_xdg_configure_serial = 0xf000_0000;
            }
            let pending = self
                .synthetic_xdg_configure_serials
                .entry(xdg_surface_guest_id)
                .or_default();
            if pending.insert(serial) {
                self.debug_assert_consistent();
                return Some(serial);
            }
        }

        log::warn!(
            "Unable to allocate a synthetic xdg_surface.configure serial for guest xdg_surface {}",
            xdg_surface_guest_id
        );
        None
    }

    /// Consume one proxy-owned configure acknowledgement.
    pub(crate) fn consume_synthetic_xdg_configure_serial(
        &mut self,
        xdg_surface_guest_id: u32,
        serial: u32,
    ) -> bool {
        let Some(pending) = self
            .synthetic_xdg_configure_serials
            .get_mut(&xdg_surface_guest_id)
        else {
            return false;
        };
        let consumed = pending.remove(&serial);
        if pending.is_empty() {
            self.synthetic_xdg_configure_serials
                .remove(&xdg_surface_guest_id);
        }
        if consumed {
            if let Some(wl_surface_guest_id) = self.wl_surface_for_xdg_surface(xdg_surface_guest_id)
            {
                if let Some(xdg_toplevel_guest_id) =
                    self.xdg_toplevel_for_wl_surface(wl_surface_guest_id)
                {
                    if let Some(zaura_toplevel_host_id) =
                        self.aura_toplevel_links.get_forward(xdg_toplevel_guest_id)
                    {
                        if let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) {
                            let _ = state
                                .transaction
                                .note_client_ack(xdg_surface_guest_id, serial);
                        }
                    }
                }
            }
            self.debug_assert_consistent();
        }
        consumed
    }

    /// Release one synthetic configure serial when its wire batch was
    /// discarded before publication.
    pub(crate) fn cancel_synthetic_xdg_configure_serial(
        &mut self,
        xdg_surface_guest_id: u32,
        serial: u32,
    ) {
        let Some(pending) = self
            .synthetic_xdg_configure_serials
            .get_mut(&xdg_surface_guest_id)
        else {
            return;
        };
        pending.remove(&serial);
        if pending.is_empty() {
            self.synthetic_xdg_configure_serials
                .remove(&xdg_surface_guest_id);
        }
        self.debug_assert_consistent();
    }

    /// Record a guest commit for a surface after a proxy-owned configure.
    ///
    /// The host geometry request may produce a same-size Aura configure before
    /// the guest has committed the synthetic size. Requiring both events
    /// prevents focus/activation configures from advancing the position phase.
    pub(crate) fn note_guest_surface_commit(&mut self, wl_surface_guest_id: u32) -> bool {
        let Some(xdg_toplevel_guest_id) = self.xdg_toplevel_for_wl_surface(wl_surface_guest_id)
        else {
            return false;
        };
        let Some(zaura_toplevel_host_id) =
            self.aura_toplevel_links.get_forward(xdg_toplevel_guest_id)
        else {
            return false;
        };
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let _ = state.transaction.note_client_commit();
        let marked = state.transaction.client_gate_complete()
            || matches!(
                state.transaction.phase(),
                SelfParentPhase::ResizePending {
                    expected_size: None,
                    ..
                }
            );
        self.debug_assert_consistent();
        marked
    }

    /// Bind the synthetic configure token to the newly armed resize phase.
    ///
    /// The token is generated before the wire batch is published, then bound
    /// only after the state transition succeeds. Acknowledgements and commits
    /// are matched against this exact serial, so an old generation cannot
    /// satisfy a newer resize merely because it targets the same surface.
    pub(crate) fn gate_resize_on_guest_commit(
        &mut self,
        zaura_toplevel_host_id: u32,
        xdg_surface_guest_id: u32,
        configure_serial: u32,
    ) -> bool {
        let Some(wl_surface_guest_id) = self.wl_surface_for_xdg_surface(xdg_surface_guest_id)
        else {
            return false;
        };
        let Some(xdg_toplevel_guest_id) = self.xdg_toplevel_for_wl_surface(wl_surface_guest_id)
        else {
            return false;
        };
        if self.aura_toplevel_links.get_forward(xdg_toplevel_guest_id)
            != Some(zaura_toplevel_host_id)
        {
            return false;
        }
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        if !state.transaction.bind_configure(ConfigureToken {
            xdg_surface_id: xdg_surface_guest_id,
            serial: configure_serial,
        }) {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Drop all pending synthetic configure serials for a destroyed role.
    pub(crate) fn clear_synthetic_xdg_configures(&mut self, xdg_surface_guest_id: u32) {
        let removed_serials = self
            .synthetic_xdg_configure_serials
            .remove(&xdg_surface_guest_id)
            .is_some();
        if removed_serials {
            self.debug_assert_consistent();
        }
    }

    /// Remove an xdg_surface association.
    pub(crate) fn take_xdg_surface(&mut self, xdg_surface_guest_id: u32) -> Option<u32> {
        self.clear_synthetic_xdg_configures(xdg_surface_guest_id);
        let wl_surface_guest_id = self
            .xdg_surface_links
            .remove_forward(xdg_surface_guest_id)?;
        self.debug_assert_consistent();
        Some(wl_surface_guest_id)
    }

    /// Record the xdg_toplevel → wl_surface role association.
    ///
    /// The parent `xdg_surface` link must already be live. This mirrors the
    /// protocol's `get_xdg_surface → get_toplevel` ordering and prevents a
    /// synthetic role from outliving or bypassing its parent association.
    #[must_use = "the XDG toplevel association may conflict with a live role"]
    pub(crate) fn remember_xdg_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        wl_surface_guest_id: u32,
    ) -> bool {
        if self
            .xdg_surface_links
            .get_reverse(wl_surface_guest_id)
            .is_none()
        {
            return false;
        }
        if !self
            .xdg_toplevel_links
            .insert(xdg_toplevel_guest_id, wl_surface_guest_id)
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Resolve an xdg_toplevel to its backing wl_surface.
    pub(crate) fn wl_surface_for_xdg_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.xdg_toplevel_links.get_forward(xdg_toplevel_guest_id)
    }

    /// Find the XDG toplevel currently associated with a wl_surface.
    pub(crate) fn xdg_toplevel_for_wl_surface(&self, wl_surface_guest_id: u32) -> Option<u32> {
        self.xdg_toplevel_links.get_reverse(wl_surface_guest_id)
    }

    /// Remove one XDG toplevel role and all placement-owned child state.
    ///
    /// This is the only single-role teardown entry point. Removing the XDG
    /// link, its Aura child, origin prediction, and active barrier together
    /// means a handler cannot accidentally leave one half of the role alive.
    #[must_use = "the returned record identifies host-side teardown work"]
    pub(crate) fn take_xdg_toplevel_for_destroy(
        &mut self,
        xdg_toplevel_guest_id: u32,
    ) -> Option<XdgToplevelRelease> {
        let wl_surface_guest_id = self
            .xdg_toplevel_links
            .remove_forward(xdg_toplevel_guest_id)?;
        let zaura_toplevel_host_id = self.take_aura_toplevel(xdg_toplevel_guest_id);
        self.debug_assert_consistent();
        Some(XdgToplevelRelease {
            wl_surface_guest_id,
            zaura_toplevel_host_id,
        })
    }

    /// Remove the XDG role links for one wl_surface and their child state.
    ///
    /// Wayland permits only one role per surface, so the result is optional
    /// rather than a collection. The caller serializes the returned Aura
    /// release after this complete state transition. Origin prediction and
    /// active barriers are retired before any later client ID reuse can route
    /// an event to this surface.
    #[must_use = "the returned record identifies host-side teardown work"]
    pub(crate) fn take_xdg_links_for_wl_surface(
        &mut self,
        wl_surface_guest_id: u32,
    ) -> Option<XdgToplevelRelease> {
        if let Some(xdg_surface_guest_id) = self.xdg_surface_for_wl_surface(wl_surface_guest_id) {
            self.clear_synthetic_xdg_configures(xdg_surface_guest_id);
        }
        self.xdg_surface_links.remove_reverse(wl_surface_guest_id);
        let xdg_toplevel_guest_id = self.xdg_toplevel_links.remove_reverse(wl_surface_guest_id);
        let release = xdg_toplevel_guest_id.map(|xdg_toplevel_guest_id| XdgToplevelRelease {
            wl_surface_guest_id,
            zaura_toplevel_host_id: self.take_aura_toplevel(xdg_toplevel_guest_id),
        });
        self.debug_assert_consistent();
        release
    }

    /// Return the stable ARC task-form application ID for one guest surface.
    ///
    /// The mapping lasts until [`Self::take_aura_surface_for_wl_surface`] is
    /// called. XDG and GTK metadata paths therefore cannot disagree while the
    /// surface is alive. The result is `None` when the ARC host policy is not
    /// selected or when no process-wide task block is available.
    pub(crate) fn arc_policy_application_id(&mut self, wl_surface_guest_id: u32) -> Option<String> {
        if !self.uses_arc_policy() {
            return None;
        }
        if let Some(application_id) = self
            .application_ids
            .get(&wl_surface_guest_id)
            .and_then(|identities| identities.arc.clone())
        {
            return Some(application_id);
        }

        let task_id = match self.runtime.allocate_arc_task_id() {
            Ok(task_id) => task_id,
            Err(error) => {
                warn!(
                    "Unable to allocate an ARC task ID for guest surface {}: {}",
                    wl_surface_guest_id, error
                );
                return None;
            }
        };
        let application_id = format!("{ARC_TASK_APPLICATION_ID_PREFIX}{task_id}");
        self.application_ids
            .entry(wl_surface_guest_id)
            .or_default()
            .arc = Some(application_id.clone());
        Some(application_id)
    }

    /// Remove the ARC identity and Aura-surface link for a destroyed surface.
    ///
    /// The guest ID owns the ARC application ID while the host ID owns the
    /// Aura-surface association. The identity is removed only after the host
    /// association resolved from the authoritative shadow table is removed,
    /// keeping both lifetimes coupled at the teardown boundary that owns both
    /// objects. A stale or mismatched host mapping leaves the application
    /// record intact instead of clearing a newer surface generation's
    /// identity.
    #[must_use = "the returned Aura surface ID identifies host teardown work"]
    pub(crate) fn take_aura_surface_for_wl_surface(
        &mut self,
        shadow_table: &ShadowTable,
        wl_surface_guest_id: u32,
    ) -> Option<u32> {
        let wl_surface_host_id = shadow_table.get_host_id(wl_surface_guest_id)?;
        let zaura_surface_host_id = self.aura_surface_links.remove_forward(wl_surface_host_id);
        if zaura_surface_host_id.is_some() {
            self.application_ids.remove(&wl_surface_guest_id);
        }
        self.debug_assert_consistent();
        zaura_surface_host_id
    }

    /// Remove application identities for a surface that never acquired an
    /// Aura-surface child.
    ///
    /// ARC identity allocation can happen when an XDG role is created, before
    /// the first metadata request has caused `zaura_shell.get_aura_surface`.
    /// Once the guest `wl_surface` is actually destroyed, there is no host
    /// object left that could own that record. This explicit orphan path is
    /// intentionally separate from [`Self::take_aura_surface_for_wl_surface`]:
    /// the latter resolves the host surface through the shadow table, while
    /// this method is called only from the authoritative `wl_surface.destroy`
    /// path after that lookup returned no Aura link.
    pub(crate) fn take_orphaned_application_state_for_surface_destroy(
        &mut self,
        wl_surface_guest_id: u32,
    ) {
        self.application_ids.remove(&wl_surface_guest_id);
        self.debug_assert_consistent();
    }

    /// Return the Aura surface associated with a host wl_surface.
    pub(crate) fn aura_surface_for_wl_surface(&self, wl_surface_host_id: u32) -> Option<u32> {
        self.aura_surface_links.get_forward(wl_surface_host_id)
    }

    /// Record the host wl_surface → host remote_surface role created by the
    /// opt-in remote-shell backend.
    ///
    /// Test-only compatibility helper for fixtures that do not model a
    /// manager generation explicitly. Production creation uses
    /// [`Self::remember_remote_surface_for_manager`].
    #[cfg(test)]
    #[must_use = "the remote surface association may conflict with a live role"]
    pub(crate) fn remember_remote_surface(
        &mut self,
        wl_surface_host_id: u32,
        remote_surface_host_id: u32,
    ) -> bool {
        let manager_host_id = self.remote_shell_id().unwrap_or(0);
        self.remember_remote_surface_for_manager(
            wl_surface_host_id,
            remote_surface_host_id,
            manager_host_id,
        )
    }

    /// Record a remote-surface role and the manager generation that owns it.
    ///
    /// The manager ID is captured at creation time instead of looked up during
    /// teardown. A replacement `zcr_remote_shell_v2` may therefore coexist
    /// with children from an older generation without sharing lifecycle
    /// accounting.
    #[must_use = "the remote surface association may conflict with a live role"]
    pub(crate) fn remember_remote_surface_for_manager(
        &mut self,
        wl_surface_host_id: u32,
        remote_surface_host_id: u32,
        manager_host_id: u32,
    ) -> bool {
        if let Some(existing_manager) = self.remote_surface_owners.get(&remote_surface_host_id) {
            return *existing_manager == manager_host_id
                && self.remote_surface_links.get_forward(wl_surface_host_id)
                    == Some(remote_surface_host_id);
        }
        if !self
            .remote_surface_links
            .insert(wl_surface_host_id, remote_surface_host_id)
        {
            return false;
        }
        self.remote_surface_owners
            .insert(remote_surface_host_id, manager_host_id);
        *self
            .remote_shell_child_counts
            .entry(manager_host_id)
            .or_default() += 1;
        self.debug_assert_consistent();
        true
    }

    pub(crate) fn remote_surface_for_wl_surface(&self, wl_surface_host_id: u32) -> Option<u32> {
        self.remote_surface_links.get_forward(wl_surface_host_id)
    }

    /// Return whether one manager generation still owns remote-surface roles.
    ///
    /// This intentionally counts only children created by `manager_host_id`.
    /// Older retired generations must not prevent a replacement manager from
    /// being destroyed after its own global is removed.
    pub(crate) fn remote_shell_has_children(&self, manager_host_id: u32) -> bool {
        self.remote_shell_child_counts
            .get(&manager_host_id)
            .is_some_and(|count| *count > 0)
    }

    /// Rebuild remote-shell child counts from the surviving child-owner map.
    ///
    /// The owner map is normally updated atomically with the bidirectional
    /// surface link.  A stale host event or a future teardown path must not
    /// turn a bookkeeping discrepancy into a compositor panic, though.  In
    /// that situation the surviving links are the least destructive source
    /// of truth; retired managers with no discoverable children are dropped
    /// from the local retirement map and remain reserved by the shadow table
    /// until connection teardown.
    fn repair_remote_shell_accounting(&mut self) {
        let mut counts = HashMap::new();
        for manager_host_id in self.remote_surface_owners.values().copied() {
            *counts.entry(manager_host_id).or_insert(0usize) += 1;
        }
        let live_manager_ids: HashSet<u32> = counts.keys().copied().collect();
        self.remote_shell_child_counts = counts;
        self.retired_remote_shells
            .retain(|manager_host_id, _| live_manager_ids.contains(manager_host_id));
    }

    pub(crate) fn take_remote_surface_for_wl_surface(
        &mut self,
        wl_surface_host_id: u32,
    ) -> Option<u32> {
        self.take_remote_surface_for_wl_surface_with_cleanup(wl_surface_host_id)
            .map(|(remote_surface_host_id, _manager)| remote_surface_host_id)
    }

    /// Remove a remote-surface role and, if it was the final child of a
    /// retired manager generation, return that manager for destruction.
    ///
    /// The child is removed from ownership accounting before the optional
    /// manager binding is returned, so callers can queue the child destructor
    /// first and then the manager destructor in protocol order.
    pub(crate) fn take_remote_surface_for_wl_surface_with_cleanup(
        &mut self,
        wl_surface_host_id: u32,
    ) -> Option<(u32, Option<(u32, u32)>)> {
        let remote_surface_host_id = self
            .remote_surface_links
            .remove_forward(wl_surface_host_id)?;
        self.remote_toplevel_links
            .remove_reverse(remote_surface_host_id);
        let Some(manager_host_id) = self.remote_surface_owners.remove(&remote_surface_host_id)
        else {
            log::warn!(
                "remote surface {} has no manager owner; retiring the child \
                 without attempting manager destruction",
                remote_surface_host_id
            );
            self.repair_remote_shell_accounting();
            self.debug_assert_consistent();
            return Some((remote_surface_host_id, None));
        };
        let last_child = {
            let child_count = self
                .remote_shell_child_counts
                .entry(manager_host_id)
                .or_insert_with(|| {
                    log::warn!(
                        "remote-shell manager {} lost its child count; \
                         reconstructing accounting before child teardown",
                        manager_host_id
                    );
                    self.remote_surface_owners
                        .values()
                        .filter(|owner| **owner == manager_host_id)
                        .count()
                        .saturating_add(1)
                });
            *child_count -= 1;
            *child_count == 0
        };
        if last_child {
            self.remote_shell_child_counts.remove(&manager_host_id);
        }
        let retired_manager = if last_child {
            self.retired_remote_shells
                .remove(&manager_host_id)
                .map(|binding| (binding.host_id, binding.version))
        } else {
            None
        };
        self.debug_assert_consistent();
        Some((remote_surface_host_id, retired_manager))
    }

    #[must_use = "the remote toplevel association may conflict with a live role"]
    pub(crate) fn remember_remote_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        remote_surface_host_id: u32,
    ) -> bool {
        if !self
            .remote_toplevel_links
            .insert(xdg_toplevel_guest_id, remote_surface_host_id)
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    pub(crate) fn remote_surface_for_xdg_toplevel(
        &self,
        xdg_toplevel_guest_id: u32,
    ) -> Option<u32> {
        self.remote_toplevel_links
            .get_forward(xdg_toplevel_guest_id)
    }

    pub(crate) fn xdg_toplevel_for_remote_surface(
        &self,
        remote_surface_host_id: u32,
    ) -> Option<u32> {
        self.remote_toplevel_links
            .get_reverse(remote_surface_host_id)
    }

    pub(crate) fn take_remote_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let remote_surface_host_id = self
            .remote_toplevel_links
            .remove_forward(xdg_toplevel_guest_id)?;
        self.debug_assert_consistent();
        Some(remote_surface_host_id)
    }

    /// Resolve an Aura surface back to its live guest `wl_surface`.
    ///
    /// Cleanup callbacks retain only the host Aura child ID. Resolving the
    /// reverse link here keeps the text-input repair keyed by the authoritative
    /// guest surface instead of duplicating that association in barrier data.
    pub(crate) fn guest_wl_surface_for_aura_surface(
        &self,
        shadow_table: &ShadowTable,
        zaura_surface_host_id: u32,
    ) -> Option<u32> {
        let wl_surface_host_id = self.aura_surface_links.get_reverse(zaura_surface_host_id)?;
        shadow_table.get_guest_id(wl_surface_host_id)
    }

    /// Return the negotiated Aura-surface version for one guest surface.
    ///
    /// This read-only diagnostic keeps host-version lookup beside the
    /// guest-surface/Aura association instead of making a protocol adapter
    /// reconstruct the relationship from independent IDs.
    pub(crate) fn aura_surface_version_for_guest_surface(
        &self,
        shadow_table: &ShadowTable,
        wl_surface_guest_id: u32,
    ) -> Option<u32> {
        let wl_surface_host_id = shadow_table.get_host_id(wl_surface_guest_id)?;
        let zaura_surface_host_id = self.aura_surface_for_wl_surface(wl_surface_host_id)?;
        Some(
            shadow_table
                .host_object_version(zaura_surface_host_id)
                .unwrap_or(self.aura_shell_version()),
        )
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
        if !self
            .aura_surface_links
            .insert(wl_surface_host_id, zaura_surface_host_id)
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Return the Aura toplevel associated with a guest xdg_toplevel.
    pub(crate) fn aura_toplevel_for_xdg_toplevel(&self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        self.aura_toplevel_links.get_forward(xdg_toplevel_guest_id)
    }

    /// Record a one-to-one xdg_toplevel ↔ Aura toplevel association.
    ///
    /// The guest XDG role must already be registered. Aura children are
    /// created only for live XDG roles, so rejecting an unparented child here
    /// keeps host-only objects from becoming unreachable during teardown.
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
        if self
            .xdg_toplevel_links
            .get_forward(xdg_toplevel_guest_id)
            .is_none()
        {
            return false;
        }
        if !self
            .aura_toplevel_links
            .insert(xdg_toplevel_guest_id, zaura_toplevel_host_id)
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Remove and return the Aura toplevel for a guest xdg_toplevel.
    ///
    /// Removing the association also retires the toplevel's origin and active
    /// barrier state. Older barriers remain callback-owned until their
    /// terminal host lifecycle event.
    #[must_use = "the returned Aura toplevel ID identifies host teardown work"]
    fn take_aura_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let zaura_toplevel_host_id = self
            .aura_toplevel_links
            .remove_forward(xdg_toplevel_guest_id)?;
        self.release_toplevel(zaura_toplevel_host_id);
        self.debug_assert_consistent();
        Some(zaura_toplevel_host_id)
    }

    /// Resolve a host Aura toplevel event back to its guest xdg_toplevel.
    pub(crate) fn xdg_toplevel_for_aura_toplevel(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<u32> {
        self.aura_toplevel_links.get_reverse(zaura_toplevel_host_id)
    }

    /// Return the latest screen-space origin for an Aura toplevel.
    pub(crate) fn origin(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.origin)
    }

    /// Return the origin only while Aura has confirmed it as authoritative.
    ///
    /// `origin()` remains available for diagnostics after a liveness fallback
    /// retires a cleanup phase without a matching host event. Placement
    /// planning uses this stricter accessor so a retained but unconfirmed
    /// coordinate cannot become the baseline for a new relative request.
    pub(crate) fn confirmed_origin(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .filter(|state| state.origin_confirmed)
            .and_then(|state| state.origin)
    }

    /// Return the client size still waiting for host geometry application.
    ///
    /// A pending size is scoped to one live Aura toplevel.  It is cleared only
    /// after the host reports the requested dimensions, so an intermediate
    /// configure cannot resize the guest back to the old native window size.
    pub(crate) fn pending_resize_size(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| match state.transaction.phase() {
                SelfParentPhase::ResizePending { expected_size, .. } => expected_size,
                _ => None,
            })
    }

    /// Return whether a self-parent rectangle is already in flight or has
    /// completed without an intervening external geometry change.
    fn is_self_parent_target_current(
        &self,
        zaura_toplevel_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> bool {
        let Some(state) = self.toplevels.get(&zaura_toplevel_host_id) else {
            return false;
        };
        if state.transaction.deferred_target() == Some(target) {
            return true;
        }
        // If a newer target is already deferred, a repeat of the active
        // rectangle is meaningful: it cancels that deferred update and
        // restores the currently executing transaction as the latest user
        // choice. Only suppress the active target when no newer choice exists.
        if state.transaction.active_target() == Some(target)
            && state.transaction.deferred_target().is_none()
        {
            return true;
        }
        // `last_completed_target` is recorded only after the operation has
        // reached its settled origin. Keep using it as a deduplication key
        // only while the host still reports that target origin. Once an idle
        // origin event reports a focus animation or an external move, the
        // completed rectangle is stale and the same shortcut must be allowed
        // to re-establish it.
        matches!(state.transaction.phase(), SelfParentPhase::Idle)
            && state.origin_confirmed
            && state.transaction.last_completed_target() == Some(target)
            && state.origin == Some((target.0, target.1))
    }

    /// Return whether one self-parent transaction still owns this toplevel.
    ///
    /// The active target remains retained through the persistent self-parent
    /// and host-IME follow-up barriers. This makes a shortcut received during
    /// cleanup a deferred update rather than a second wire transaction.
    pub(crate) fn self_parent_transaction_active(&self, zaura_toplevel_host_id: u32) -> bool {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .is_some_and(|state| !matches!(state.transaction.phase(), SelfParentPhase::Idle))
    }

    /// Replace the deferred target with the newest requested rectangle.
    fn defer_self_parent_target(
        &mut self,
        zaura_toplevel_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        if !state.transaction.defer_target(target) {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Return the newest target waiting behind the current resize/cleanup.
    pub(crate) fn deferred_self_parent_target(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<(i32, i32, i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.transaction.deferred_target())
    }

    /// Resolve the relative parent delta for a deferred target without
    /// changing the active transaction. This is used to build the next
    /// parent request before atomically promoting the target in state.
    pub(crate) fn deferred_self_parent_move(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<DeferredSelfParentMove> {
        let state = self.toplevels.get(&zaura_toplevel_host_id)?;
        let target = state.transaction.deferred_target()?;
        // Use only the origin retained from an authoritative Aura event; a
        // deferred target must never be rebased from an intermediate focus or
        // animation coordinate.
        // A liveness fallback may retain the last coordinate for diagnostics
        // while explicitly marking it unconfirmed. Never use that stale
        // coordinate to rebase a deferred relative move.
        let current_origin = state.origin.filter(|_| state.origin_confirmed)?;
        let relative = (
            target.0.checked_sub(current_origin.0)?,
            target.1.checked_sub(current_origin.1)?,
        );
        match state.transaction.phase() {
            SelfParentPhase::ResizePending {
                expected_size: None,
                ..
            }
            | SelfParentPhase::CleanupPending {
                origin_acknowledged: true,
                cleanup_barrier_pending: false,
                ..
            } => {}
            _ => return None,
        }
        let surface_id = state.transaction.active_surface()?;
        Some((surface_id, current_origin, relative, target))
    }

    /// Return the size currently represented by the active target.
    pub(crate) fn active_self_parent_size(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| {
                state
                    .transaction
                    .active_target()
                    .map(|target| (target.2, target.3))
            })
    }

    /// Return the Aura surface retained by the active self-parent operation.
    pub(crate) fn pending_surface_for_toplevel(&self, zaura_toplevel_host_id: u32) -> Option<u32> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.transaction.active_surface())
    }

    /// Promote a deferred target and retain an exact rollback token for the
    /// wire adapter.
    pub(crate) fn promote_deferred_self_parent_target_with_rollback(
        &mut self,
        zaura_toplevel_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> Option<DeferredPromotionRollback> {
        let state = self.toplevels.get_mut(&zaura_toplevel_host_id)?;
        let rollback = state
            .transaction
            .promote_deferred_with_rollback(target, None)?;
        self.debug_assert_consistent();
        Some(rollback)
    }

    /// Promote a deferred target whose size still needs a host XDG
    /// configure. The caller publishes that configure/geometry pair before
    /// invoking this method.
    #[cfg(test)]
    pub(crate) fn promote_deferred_self_parent_resize(
        &mut self,
        zaura_toplevel_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        if !state
            .transaction
            .promote_deferred(target, Some((target.2, target.3)))
        {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Promote a deferred resize target and retain an exact rollback token for
    /// failures while its configure/barrier wire is being staged.
    pub(crate) fn promote_deferred_self_parent_resize_with_rollback(
        &mut self,
        zaura_toplevel_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> Option<DeferredPromotionRollback> {
        let state = self.toplevels.get_mut(&zaura_toplevel_host_id)?;
        let rollback = state
            .transaction
            .promote_deferred_with_rollback(target, Some((target.2, target.3)))?;
        self.debug_assert_consistent();
        Some(rollback)
    }

    /// Roll back a deferred promotion if no later reducer event superseded it.
    pub(crate) fn rollback_deferred_self_parent_promotion(
        &mut self,
        zaura_toplevel_host_id: u32,
        rollback: DeferredPromotionRollback,
    ) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let rolled_back = state.transaction.rollback_deferred_promotion(rollback);
        if rolled_back {
            self.debug_assert_consistent();
        }
        rolled_back
    }

    /// Finish a self-parent transaction that has no deferred target after the
    /// follow-up barrier.
    pub(crate) fn complete_self_parent_cleanup(&mut self, zaura_toplevel_host_id: u32) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let completed = state.transaction.complete();
        if completed {
            self.debug_assert_consistent();
        }
        completed
    }

    /// Settle cleanup when the ordered host barrier completed but Aura omitted
    /// the target `origin_change`.
    ///
    /// A sync callback has no geometry payload, but it is ordered after the
    /// self-parent request. In that narrow case the requested target is the
    /// only safe baseline available to the proxy. Keep the transaction alive
    /// long enough for [`crate::handler::placement::advance_self_parent_after_origin`]
    /// to promote a deferred shortcut (or complete the current one), and mark
    /// the target as confirmed so the next relative delta can be calculated.
    pub(crate) fn settle_self_parent_after_cleanup(&mut self, zaura_toplevel_host_id: u32) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        if !state.transaction.settle_origin_after_cleanup_barrier() {
            return false;
        }
        let Some(target) = state.transaction.active_target() else {
            return false;
        };
        state.origin = Some((target.0, target.1));
        state.origin_confirmed = true;
        self.debug_assert_consistent();
        true
    }

    /// Test/compatibility helper that retires cleanup without claiming a
    /// target origin. Runtime callbacks use
    /// [`Self::settle_self_parent_after_cleanup`] because dropping a deferred
    /// target here deadlocks subsequent shortcuts when Aura omits
    /// `origin_change`.
    #[cfg(test)]
    pub(crate) fn settle_self_parent_cleanup_without_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
    ) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let settled = state.transaction.settle_cleanup_without_origin();
        if settled {
            state.origin_confirmed = false;
            self.debug_assert_consistent();
        }
        settled
    }

    /// Abort a cleanup transition after a follow-up wire operation could not
    /// be encoded. This is a defensive terminal path for teardown or a
    /// released host object; it leaves the state internally idle rather than
    /// allowing a permanently active transaction to consume every shortcut.
    pub(crate) fn abort_self_parent_cleanup(&mut self, zaura_toplevel_host_id: u32) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let aborted = state.transaction.abort_cleanup();
        if aborted {
            self.debug_assert_consistent();
        }
        aborted
    }

    /// Roll back a resize phase whose encoded configure cannot be published.
    pub(crate) fn abort_self_parent_resize(&mut self, zaura_toplevel_host_id: u32) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let aborted = state.transaction.abort_resize();
        if aborted {
            self.debug_assert_consistent();
        }
        aborted
    }

    /// Arm the two-phase native self-parent operation after its resize wire
    /// batch has been published.
    #[must_use = "the target was rejected because the Aura toplevel is not live"]
    pub(crate) fn arm_self_parent_target(
        &mut self,
        zaura_toplevel_host_id: u32,
        zaura_surface_host_id: u32,
        target: (i32, i32, i32, i32),
    ) -> bool {
        if target.2 <= 0
            || target.3 <= 0
            || self
                .aura_toplevel_links
                .get_reverse(zaura_toplevel_host_id)
                .is_none()
            || self
                .aura_surface_links
                .get_reverse(zaura_surface_host_id)
                .is_none()
        {
            return false;
        }
        let state = self.toplevels.entry(zaura_toplevel_host_id).or_default();
        state
            .transaction
            .begin_resize(target, zaura_surface_host_id, (target.2, target.3));
        // Unit-level callers model the client commit implicitly. The wire
        // adapter binds a real configure token immediately after queueing and
        // resets this gate until the guest acknowledges and commits.
        let _ = state.transaction.assume_client_ready();
        self.debug_assert_consistent();
        true
    }

    /// Accept the host configure that reflects the requested client size.
    ///
    /// Returns `false` for a stale/intermediate size or an unowned toplevel.
    /// A small host-side decoration adjustment is accepted as the effective
    /// client size; unrelated previous sizes remain suppressed.
    #[cfg(test)]
    #[must_use = "the size was stale or the Aura toplevel is not live"]
    pub(crate) fn accept_pending_resize(
        &mut self,
        zaura_toplevel_host_id: u32,
        reported_size: (i32, i32),
    ) -> bool {
        let Some(origin) = self.origin(zaura_toplevel_host_id) else {
            return false;
        };
        self.accept_pending_resize_at_origin(zaura_toplevel_host_id, reported_size, origin)
            && self.pending_resize_size(zaura_toplevel_host_id).is_none()
    }

    /// Feed a host configure with its actual screen origin into the placement
    /// reducer. Keeping size and origin together prevents a host-first
    /// acknowledgement from being paired with a stale baseline.
    pub(crate) fn accept_pending_resize_at_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
        reported_size: (i32, i32),
        origin: (i32, i32),
    ) -> bool {
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
        {
            return false;
        }
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return true;
        };
        let phase_before_configure = state.transaction.phase();
        // A retained coordinate from the no-origin cleanup fallback is only
        // diagnostic. It must not reject the first real configure that carries
        // the fresh host origin used to recover the relative-placement
        // baseline.
        let known_origin = state.origin.filter(|_| state.origin_confirmed);
        let waiting_for_resize = matches!(
            phase_before_configure,
            SelfParentPhase::ResizePending {
                expected_size: Some(_),
                ..
            }
        );
        // A synthetic XDG geometry resize must preserve the current
        // screen-space origin.  Aura also emits focus/activation configures
        // while a resize is pending; those can have a plausible size but a
        // widget/animation origin.  Treating one as the resize ACK rebases
        // the subsequent self-parent delta and causes the window to drift.
        // When no authoritative origin exists yet, the first real configure
        // is still allowed to establish it.
        if waiting_for_resize
            && known_origin.is_some_and(|expected_origin| expected_origin != origin)
        {
            return false;
        }
        let result = state.transaction.note_host_resize(
            reported_size,
            origin,
            MAX_HOST_CLIENT_SIZE_ADJUSTMENT,
        );
        if let HostResizeResult::Accepted {
            origin: accepted_origin,
        } = result
        {
            // When the first valid configure arrives before any
            // `origin_change`, this configure is the only authoritative
            // screen-space coordinate available for the move phase. Record it
            // together with the accepted resize transition; leaving it for
            // the caller's later `record_origin` pass races the reducer phase
            // change and causes that pass to reject the otherwise valid first
            // origin.
            if !state.origin_confirmed {
                state.origin = Some(accepted_origin);
                state.origin_confirmed = true;
            }
        }
        if !matches!(result, HostResizeResult::Ignored) {
            state.observed_size = Some(reported_size);
        }
        self.debug_assert_consistent();
        if !matches!(result, HostResizeResult::Ignored) {
            return true;
        }

        // An Aura configure is also the host's normal state/resize signal.
        // Reject an unrelated configure while a native self-parent
        // transaction is still waiting for its requested size.  Treating an
        // ignored configure as accepted here lets a focus/activation event
        // open a deferred placement and move the window without a shortcut.
        // Once the resize gate has been accepted, or when no placement is
        // active, the configure continues through the normal XDG path.
        matches!(
            phase_before_configure,
            SelfParentPhase::Idle
                | SelfParentPhase::MovePending { .. }
                | SelfParentPhase::CleanupPending { .. }
                | SelfParentPhase::ResizePending {
                    expected_size: None,
                    ..
                }
        )
    }

    /// Return the self-parent move that becomes eligible once the host has
    /// acknowledged the requested size. The current origin is always read
    /// from authoritative host state; no predicted screen coordinate is used.
    pub(crate) fn pending_self_parent_move(
        &self,
        zaura_toplevel_host_id: u32,
    ) -> Option<PendingSelfParentMove> {
        let state = self.toplevels.get(&zaura_toplevel_host_id)?;
        let SelfParentPhase::ResizePending {
            target,
            surface_id,
            expected_size: None,
            ..
        } = state.transaction.phase()
        else {
            return None;
        };
        // The retained origin is the only safe baseline; an activation or
        // animation coordinate must never be substituted here.
        let current_origin = state.origin.filter(|_| state.origin_confirmed)?;
        let relative = (
            target.0.checked_sub(current_origin.0)?,
            target.1.checked_sub(current_origin.1)?,
        );
        Some((surface_id, current_origin, relative))
    }

    /// Mark the self-parent request as published. A failed wire encoding can
    /// leave the resize phase retryable by simply not calling this method.
    #[must_use = "the toplevel may have been released"]
    pub(crate) fn mark_self_parent_move_queued(&mut self, zaura_toplevel_host_id: u32) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        let SelfParentPhase::ResizePending { target, .. } = state.transaction.phase() else {
            return false;
        };
        let Some(origin) = state.origin else {
            return false;
        };
        if !state.transaction.mark_move_queued(origin, target) {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Retire the active self-parent target after its ordered cleanup barrier
    /// has been queued. This is the deduplication boundary for the next
    /// identical shortcut; the parent relationship itself remains installed.
    ///
    /// A newer shortcut may have entered the resize phase while the older
    /// target's host barrier was in flight. In that case the state no longer
    /// has a queued parent move for the barrier being completed; clearing it
    /// would discard the newer target and make its later configure a no-op.
    /// Treat that callback as stale and leave the newer transaction intact.
    pub(crate) fn finish_self_parent_move(&mut self, zaura_toplevel_host_id: u32) {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return;
        };
        let _ = state.transaction.begin_cleanup();
        self.debug_assert_consistent();
    }

    /// Return whether the active self-parent operation has reached its target
    /// screen origin.
    pub(crate) fn self_parent_origin_settled(&self, zaura_toplevel_host_id: u32) -> bool {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .is_some_and(|state| state.transaction.origin_settled())
    }

    /// Return whether ordered self-parent cleanup has completed its first
    /// barrier while the placement state is still active.
    pub(crate) fn self_parent_cleanup_pending(&self, zaura_toplevel_host_id: u32) -> bool {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .is_some_and(|state| {
                matches!(
                    state.transaction.phase(),
                    SelfParentPhase::CleanupPending { .. }
                )
            })
    }

    /// Return whether the ordered follow-up cleanup barrier is still in
    /// flight. An origin event may arrive before `sync.done`; it can mark the
    /// target as acknowledged, but must not promote a deferred shortcut until
    /// the barrier has crossed the host stream.
    pub(crate) fn self_parent_cleanup_barrier_pending(&self, zaura_toplevel_host_id: u32) -> bool {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .is_some_and(|state| {
                matches!(
                    state.transaction.phase(),
                    SelfParentPhase::CleanupPending {
                        cleanup_barrier_pending: true,
                        ..
                    }
                )
            })
    }

    /// Mark the follow-up self-parent/IME barrier as host-complete.
    ///
    /// An origin event can race this callback. Keeping the bit in the phase
    /// prevents `complete_self_parent_cleanup` from retiring the transaction
    /// before both acknowledgements have arrived.
    pub(crate) fn complete_self_parent_cleanup_barrier(
        &mut self,
        zaura_toplevel_host_id: u32,
    ) -> bool {
        let Some(state) = self.toplevels.get_mut(&zaura_toplevel_host_id) else {
            return false;
        };
        if !state.transaction.complete_cleanup_barrier() {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    pub(crate) fn pending_origin(&self, zaura_toplevel_host_id: u32) -> Option<(i32, i32)> {
        self.toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| {
                state
                    .transaction
                    .active_target()
                    .map(|target| (target.0, target.1))
            })
    }

    /// Record a host origin if it is authoritative for the current request.
    ///
    /// Returns `false` for an intermediate origin that conflicts with a
    /// pending self-parent target or for an Aura toplevel that is no longer
    /// associated with a live guest xdg_toplevel. After a completed
    /// self-parent generation, the reducer may temporarily reject late
    /// focus/animation origins until the next explicit placement starts; this
    /// prevents an intermediate frame from rebasing the next relative delta.
    #[must_use = "the origin was rejected or is still pending"]
    pub(crate) fn record_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
        origin: (i32, i32),
    ) -> bool {
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
        {
            return false;
        }
        let state = self.toplevels.entry(zaura_toplevel_host_id).or_default();
        let accepted = match state.transaction.phase() {
            SelfParentPhase::ResizePending {
                expected_size: Some(_),
                ..
            } => {
                // Before the matching size configure, retain the first host
                // baseline and reject focus/activation coordinates.
                state.origin.is_none() || state.origin == Some(origin)
            }
            SelfParentPhase::ResizePending {
                expected_size: None,
                ..
            } => {
                // The resize has been acknowledged, but the parent move has
                // not necessarily been queued yet. Do not let an
                // animation/widget coordinate from an unrelated configure
                // replace the settled baseline.
                state.origin == Some(origin)
            }
            _ => matches!(
                state.transaction.note_origin(origin),
                OriginResult::Accepted { .. }
            ),
        };
        if accepted {
            state.origin = Some(origin);
            state.origin_confirmed = true;
        }
        self.debug_assert_consistent();
        accepted
    }

    /// Predict the origin after a self-parent request.
    #[must_use = "the prediction was rejected because the Aura toplevel is not live"]
    #[cfg(test)]
    pub(crate) fn predict_origin(
        &mut self,
        zaura_toplevel_host_id: u32,
        target_origin: (i32, i32),
    ) -> bool {
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
        {
            return false;
        }
        let state = self.toplevels.entry(zaura_toplevel_host_id).or_default();
        state.origin = Some(target_origin);
        state.origin_confirmed = true;
        state
            .transaction
            .assume_move_pending((target_origin.0, target_origin.1, 0, 0), 0);
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
        self.barriers.release_toplevel(zaura_toplevel_host_id);
    }

    /// Register a newly queued host sync callback as the latest pending sync.
    ///
    /// A callback ID must be globally unique for the connection. Returning
    /// `false` instead of overwriting an existing entry keeps an older
    /// callback's terminal lifecycle reachable.
    #[cfg(test)]
    #[must_use = "the barrier was rejected and must not be queued"]
    pub(crate) fn register_barrier(
        &mut self,
        callback_host_id: u32,
        zaura_toplevel_host_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
    ) -> bool {
        self.register_barrier_with_trace(callback_host_id, zaura_toplevel_host_id, cleanup, None)
    }

    /// Register a placement barrier and carry an optional runtime trace ID.
    ///
    /// The trace ID is diagnostic metadata only. It does not alter barrier
    /// supersession, cleanup ownership, or callback lifetime.
    #[must_use = "the barrier was rejected and must not be queued"]
    pub(crate) fn register_barrier_with_trace(
        &mut self,
        callback_host_id: u32,
        zaura_toplevel_host_id: u32,
        cleanup: Option<PlacementBarrierCleanup>,
        trace_id: Option<u64>,
    ) -> bool {
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
        {
            return false;
        }
        let generation = self
            .toplevels
            .get(&zaura_toplevel_host_id)
            .and_then(|state| state.transaction.active_generation());
        if !self.barriers.register_with_generation(
            callback_host_id,
            zaura_toplevel_host_id,
            cleanup,
            generation,
            trace_id,
        ) {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Cancel a barrier that was staged but never published to the host.
    pub(crate) fn cancel_barrier(&mut self, callback_host_id: u32) -> bool {
        let cancelled = self.barriers.cancel(callback_host_id);
        if cancelled {
            self.debug_assert_consistent();
        }
        cancelled
    }

    /// Retire a completed barrier and return cleanup only if it is still newest.
    #[must_use = "the returned completion identifies which placement finished"]
    pub(crate) fn complete_barrier(
        &mut self,
        callback_host_id: u32,
    ) -> Option<PlacementBarrierCompletion> {
        let role_is_live = self
            .barriers
            .callback_for(callback_host_id)
            .is_some_and(|toplevel_id| self.aura_toplevel_links.get_reverse(toplevel_id).is_some());
        let mut completion = self.barriers.complete(callback_host_id, role_is_live)?;
        if let Some(generation) = completion.generation {
            let current_generation = self
                .toplevels
                .get(&completion.toplevel_id)
                .and_then(|state| state.transaction.active_generation());
            if current_generation != Some(generation) {
                // A transient ARC identity belongs to the wl_surface, not
                // the xdg_toplevel role.  The role may be destroyed while
                // its surface (and Aura child) remains alive; in that case
                // the callback must still restore the native identity.
                let role_was_released = !role_is_live;
                let restore_surface_identity = role_was_released
                    && matches!(
                        completion.cleanup.as_ref(),
                        Some(PlacementBarrierCleanup::RestoreNativeApplicationId { .. })
                    );
                log::debug!(
                    "discarding stale placement cleanup callback={} toplevel={} \
                     generation={} current={:?}",
                    callback_host_id,
                    completion.toplevel_id,
                    generation,
                    current_generation
                );
                if !restore_surface_identity {
                    completion.cleanup = None;
                }
            }
        }
        self.debug_assert_consistent();
        Some(completion)
    }

    /// Return whether a placement sync callback is pending for this toplevel.
    ///
    /// The callback only establishes host-stream ordering. Configure events
    /// continue to be forwarded; origin prediction handles stale positions.
    pub(crate) fn has_pending_barrier(&self, zaura_toplevel_host_id: u32) -> bool {
        self.barriers.has_pending(zaura_toplevel_host_id)
    }

    #[cfg(test)]
    pub(crate) fn barrier_for_callback(&self, callback_host_id: u32) -> Option<u32> {
        self.barriers.callback_for(callback_host_id)
    }

    #[cfg(test)]
    pub(crate) fn active_barrier_for_toplevel(&self, zaura_toplevel_host_id: u32) -> Option<u32> {
        self.barriers.active_callback_for(zaura_toplevel_host_id)
    }

    #[cfg(test)]
    pub(crate) fn has_any_barriers(&self) -> bool {
        !self.barriers.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn set_mode_for_test(&mut self, mode: WindowPlacementMode) {
        self.runtime = self.runtime.with_mode_for_test(
            mode,
            ShortcutConfigHandle::new(self.runtime.shortcut_config_snapshot()),
        );
    }
}

#[cfg(test)]
impl Default for WindowPlacementState {
    fn default() -> Self {
        Self::new(WindowPlacementMode::disabled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow_for_surface(guest_surface_id: u32, host_surface_id: u32) -> ShadowTable {
        let mut shadow_table = ShadowTable::new();
        shadow_table.map_id(guest_surface_id, host_surface_id);
        shadow_table
    }

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
    fn vm_identifier_defaults_and_native_ids_are_owned_by_placement_state() {
        assert_eq!(resolve_vm_identifier(None), "termina");
        assert_eq!(resolve_vm_identifier(Some(String::new())), "termina");
        assert_eq!(
            resolve_vm_identifier(Some("penguin".to_string())),
            "penguin"
        );

        let state = WindowPlacementState::default();
        let expected_vm_identifier =
            resolve_vm_identifier(std::env::var("SOMMELIER_VM_IDENTIFIER").ok());
        assert_eq!(
            state.native_wayland_app_id("com.example.Terminal"),
            format!("org.chromium.guest_os.{expected_vm_identifier}.wayland.com.example.Terminal")
        );

        let mut state = WindowPlacementState::default();
        state.remember_native_application_id(
            10,
            "org.chromium.guest_os.termina.wayland.com.example.Terminal".to_string(),
        );
        assert!(state.remember_aura_surface(20, 30));
        assert_eq!(
            state.native_application_id(10).as_deref(),
            Some("org.chromium.guest_os.termina.wayland.com.example.Terminal")
        );
        assert_eq!(
            state.take_aura_surface_for_wl_surface(&shadow_for_surface(10, 20), 10),
            Some(30)
        );
        assert_eq!(state.native_application_id(10), None);
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

    fn state_with_output(mode: WindowPlacementMode) -> WindowPlacementState {
        let mut state = WindowPlacementState::new(mode);
        assert!(state.remember_output(50));
        state.update_output_mode(50, true, 3840, 2160);
        state.update_output_scale(50, 1);
        state
    }

    #[test]
    fn aura_output_insets_define_the_host_work_area() {
        let mut state = state_with_output(WindowPlacementMode::disabled());
        assert!(state.remember_aura_output(50, 60));
        assert!(state.update_output_insets(60, 0, 0, 48, 0));
        assert_eq!(
            state.primary_output(),
            Some((
                50,
                OutputState {
                    mode_width: 3840,
                    mode_height: 2160,
                    scale: 1,
                    insets_top: 0,
                    insets_left: 0,
                    insets_bottom: 48,
                    insets_right: 0,
                },
            ))
        );
        assert_eq!(
            state.bounds_for_rect(NormalizedRect::new(0.0, 0.0, 1.0, 1.0)),
            Some((50, (0, 0, 3840, 2112)))
        );
    }

    #[test]
    fn stale_or_negative_aura_output_insets_are_ignored() {
        let mut state = state_with_output(WindowPlacementMode::disabled());
        assert!(state.remember_aura_output(50, 60));
        assert!(!state.update_output_insets(61, 0, 0, 48, 0));
        assert!(!state.update_output_insets(60, 0, 0, -1, 0));
        assert_eq!(state.primary_output().unwrap().1.insets_bottom, 0);
        assert_eq!(state.take_aura_output_for_output(50), Some(60));
        assert!(!state.update_output_insets(60, 0, 0, 48, 0));
    }

    fn placement_target(zaura_surface_version: u32) -> PlacementTarget {
        PlacementTarget::new(10, 20, 30, 60, 70, 80, zaura_surface_version)
    }

    fn register_target(state: &mut WindowPlacementState, target: PlacementTarget) -> ShadowTable {
        assert!(state.remember_xdg_surface(
            target.guest_xdg_toplevel_id() + 1000,
            target.wl_surface_guest_id()
        ));
        assert!(state
            .remember_xdg_toplevel(target.guest_xdg_toplevel_id(), target.wl_surface_guest_id()));
        assert!(state.remember_aura_toplevel(
            target.guest_xdg_toplevel_id(),
            target.zaura_toplevel_host_id()
        ));
        assert!(state
            .remember_aura_surface(target.wl_surface_host_id(), target.zaura_surface_host_id()));
        let mut shadow_table = ShadowTable::new();
        shadow_table.map_id(
            target.guest_xdg_toplevel_id(),
            target.host_xdg_toplevel_id(),
        );
        shadow_table.map_id(target.wl_surface_guest_id(), target.wl_surface_host_id());
        shadow_table.track_interface_with_version(
            target.guest_xdg_toplevel_id(),
            "xdg_toplevel".to_string(),
            6,
        );
        shadow_table.track_interface_with_version(
            target.wl_surface_guest_id(),
            "wl_surface".to_string(),
            6,
        );
        shadow_table.track_host_interface_with_version(
            target.host_xdg_toplevel_id(),
            "xdg_toplevel".to_string(),
            6,
        );
        shadow_table.track_host_interface_with_version(
            target.wl_surface_host_id(),
            "wl_surface".to_string(),
            6,
        );
        shadow_table.track_host_interface_with_version(
            target.zaura_toplevel_host_id(),
            "zaura_toplevel".to_string(),
            38,
        );
        shadow_table.track_host_interface_with_version(
            target.zaura_surface_host_id(),
            "zaura_surface".to_string(),
            target.zaura_surface_version(),
        );
        shadow_table
    }

    fn register_xdg_role(
        state: &mut WindowPlacementState,
        xdg_toplevel_id: u32,
        wl_surface_id: u32,
    ) {
        assert!(state.remember_xdg_surface(xdg_toplevel_id + 1000, wl_surface_id));
        assert!(state.remember_xdg_toplevel(xdg_toplevel_id, wl_surface_id));
    }

    #[test]
    fn placement_plan_is_the_single_backend_decision_point() {
        let mut bounds = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let direct_target = placement_target(1);
        let direct_shadow = register_target(&mut bounds, direct_target);
        let direct = bounds
            .prepare_placement(
                &direct_shadow,
                10,
                20,
                NormalizedRect::new(0.5, 0.0, 0.5, 1.0),
            )
            .expect("direct bounds plan should be valid");
        assert_eq!(direct.output_host_id(), 50);
        assert_eq!(direct.bounds(), (1920, 0, 1920, 2160));
        assert_eq!(direct.geometry(), WindowPlacementGeometry::Bounds);
        assert_eq!(direct.transient_arc_identity(), None);
        assert_eq!(direct.barrier_cleanup(), None);

        let mut self_parent = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        let probe_target = placement_target(2);
        let probe_shadow = register_target(&mut self_parent, probe_target);
        assert!(self_parent.record_origin(70, (100, 200)));
        let probe = self_parent
            .prepare_placement(
                &probe_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("self-parent plan should be valid");
        assert_eq!(
            probe.geometry(),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (-100, -200),
            }
        );
        assert_eq!(
            probe.barrier_cleanup(),
            Some(&PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: 80
            })
        );
        assert_eq!(self_parent.pending_origin(70), None);
        assert!(self_parent.commit_placement_plan(&probe));
        assert_eq!(self_parent.pending_origin(70), Some((0, 0)));
        assert_eq!(self_parent.pending_resize_size(70), Some((1920, 1080)));
        assert!(self_parent
            .prepare_placement(
                &probe_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget));
        assert!(!self_parent.accept_pending_resize(70, (800, 600)));
        assert!(self_parent.accept_pending_resize(70, (1920, 1080)));
        assert_eq!(
            self_parent.pending_self_parent_move(70),
            Some((80, (100, 200), (-100, -200)))
        );
        assert!(self_parent.mark_self_parent_move_queued(70));
        assert!(self_parent
            .prepare_placement(
                &probe_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget));
        self_parent.finish_self_parent_move(70);
        assert_eq!(
            self_parent.origin(70),
            Some((100, 200)),
            "the pre-move origin remains the only authoritative baseline until \
             the host reports the new origin"
        );
        assert!(
            !self_parent.complete_self_parent_cleanup(70),
            "cleanup cannot complete before the target origin is acknowledged"
        );
        // The host may deliver the final origin notification after the
        // ordered self-parent cleanup. A duplicate shortcut in that interval
        // must remain deferred rather than enqueueing another probe.
        assert!(self_parent
            .prepare_placement(
                &probe_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget));
        assert!(self_parent.record_origin(70, (0, 0)));
        assert!(self_parent.complete_self_parent_cleanup_barrier(70));
        assert!(self_parent.complete_self_parent_cleanup(70));
        assert!(self_parent
            .prepare_placement(
                &probe_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget));
    }

    #[test]
    fn first_accepted_resize_configure_establishes_origin_baseline() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(2);
        let _shadow = register_target(&mut state, target);

        assert!(state.arm_self_parent_target(
            target.zaura_toplevel_host_id(),
            target.zaura_surface_host_id(),
            (0, 0, 1920, 1080),
        ));
        assert_eq!(state.origin(target.zaura_toplevel_host_id()), None);
        assert!(state.accept_pending_resize_at_origin(
            target.zaura_toplevel_host_id(),
            (1920, 1080),
            (320, 180),
        ));
        assert_eq!(
            state.origin(target.zaura_toplevel_host_id()),
            Some((320, 180)),
            "the first accepted host configure must provide the relative-move baseline"
        );
        assert_eq!(
            state.pending_self_parent_move(target.zaura_toplevel_host_id()),
            Some((target.zaura_surface_host_id(), (320, 180), (-320, -180))),
            "a valid first configure must open the self-parent move phase"
        );
    }

    #[test]
    fn placement_target_resolution_rejects_an_unrelated_surface() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let target = placement_target(5);
        let shadow_table = register_target(&mut state, target);

        assert_eq!(
            state.prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id() + 1,
                NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
            ),
            Err(WindowPlacementPlanError::TargetUnavailable)
        );
    }

    #[test]
    fn resize_configure_does_not_rebase_self_parent_origin() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(6);
        let shadow_table = register_target(&mut state, target);
        let original_origin = (100, 200);
        assert!(state.record_origin(target.zaura_toplevel_host_id(), original_origin));
        let plan = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("self-parent target should be valid");
        assert!(state.commit_placement_plan(&plan));

        // A focus/activation configure can report a stale screen origin while
        // the synthetic resize is still pending. It must not replace the
        // baseline used to calculate the relative parent request.
        assert!(!state.record_origin(target.zaura_toplevel_host_id(), (1920, 1080)));
        assert_eq!(
            state.origin(target.zaura_toplevel_host_id()),
            Some(original_origin)
        );
        assert_eq!(
            state.pending_self_parent_move(target.zaura_toplevel_host_id()),
            None,
            "the resize must be acknowledged before the parent phase"
        );
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 1080)));
        assert!(
            !state.record_origin(target.zaura_toplevel_host_id(), (1520, 756)),
            "a resize-time animation coordinate must not rebase the settled origin"
        );
        assert_eq!(
            state.pending_self_parent_move(target.zaura_toplevel_host_id()),
            Some((
                target.zaura_surface_host_id(),
                original_origin,
                (-100, -200)
            ))
        );
    }

    #[test]
    fn resize_configure_origin_does_not_replace_settled_baseline() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(16);
        let shadow_table = register_target(&mut state, target);
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (0, 0),));
        let plan = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.5, 0.0, 0.5, 1.0),
            )
            .expect("self-parent target should be valid");
        assert!(state.commit_placement_plan(&plan));

        // The host may report an animation/widget coordinate while applying
        // the size. It must not replace the settled baseline used for the
        // parent delta.
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 2160),));
        assert_eq!(
            state.pending_self_parent_move(target.zaura_toplevel_host_id()),
            Some((target.zaura_surface_host_id(), (0, 0), (1920, 0),)),
            "the parent delta must use the last settled host origin"
        );
    }

    #[test]
    fn stale_focus_configure_cannot_open_deferred_self_parent_target() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(17);
        let shadow_table = register_target(&mut state, target);
        let host_id = target.zaura_toplevel_host_id();
        let original_origin = (100, 200);
        assert!(state.record_origin(host_id, original_origin));

        let first = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("first self-parent target should be valid");
        assert!(state.commit_placement_plan(&first));

        // A second shortcut is retained while the first synthetic resize is
        // in flight. It must not be published merely because focus/activation
        // emits an unrelated Aura configure.
        let second = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.5, 0.0, 0.5, 0.5),
            )
            .expect("a different shortcut should be deferred");
        assert!(state.commit_placement_plan(&second));
        assert!(state.deferred_self_parent_target(host_id).is_some());

        let pending_size = state
            .pending_resize_size(host_id)
            .expect("first synthetic resize should still be pending");
        assert!(
            !state.accept_pending_resize_at_origin(host_id, pending_size, (1920, 1080)),
            "a same-size focus configure with a different origin must not count \
             as the requested resize ACK"
        );
        assert!(
            state.pending_resize_size(host_id).is_some(),
            "the first synthetic resize must remain pending"
        );
        assert!(
            state.deferred_self_parent_target(host_id).is_some(),
            "the deferred shortcut must wait for a real size ACK"
        );
    }

    #[test]
    fn late_origin_after_cleanup_does_not_rebase_idle_baseline() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(9);
        let shadow_table = register_target(&mut state, target);
        let original_origin = (100, 200);
        assert!(state.record_origin(target.zaura_toplevel_host_id(), original_origin));
        let plan = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("self-parent target should be valid");
        assert!(state.commit_placement_plan(&plan));
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 1080),));
        assert!(state
            .pending_self_parent_move(target.zaura_toplevel_host_id())
            .is_some());
        assert!(state.mark_self_parent_move_queued(target.zaura_toplevel_host_id()));
        state.finish_self_parent_move(target.zaura_toplevel_host_id());
        assert!(
            state.complete_self_parent_cleanup_barrier(target.zaura_toplevel_host_id()),
            "the follow-up barrier must be acknowledged before cleanup can settle"
        );
        assert_eq!(
            state.origin(target.zaura_toplevel_host_id()),
            Some(original_origin),
            "the pre-move origin remains authoritative until the host reports the target"
        );

        // Once cleanup completes, late focus/animation origins are ignored
        // until a new placement generation explicitly clears the guard.
        // Rebasing on an intermediate coordinate is what caused repeated
        // shortcuts to drift toward the lower-right corner.
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (0, 0)));
        assert!(state.complete_self_parent_cleanup(target.zaura_toplevel_host_id()));
        assert!(!state.record_origin(target.zaura_toplevel_host_id(), (500, 700)));
        assert_eq!(state.origin(target.zaura_toplevel_host_id()), Some((0, 0)));
        assert!(
            state
                .prepare_placement(
                    &shadow_table,
                    target.guest_xdg_toplevel_id(),
                    target.wl_surface_guest_id(),
                    NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
                )
                .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget),
            "a late focus origin must not turn a duplicate shortcut into a new delta"
        );
        assert!(!state.record_origin(target.zaura_toplevel_host_id(), (0, 0)));
        assert_eq!(state.origin(target.zaura_toplevel_host_id()), Some((0, 0)));
        assert!(
            state
                .prepare_placement(
                    &shadow_table,
                    target.guest_xdg_toplevel_id(),
                    target.wl_surface_guest_id(),
                    NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
                )
                .is_err_and(|error| error == WindowPlacementPlanError::AlreadyAtTarget),
            "a duplicate shortcut must remain a no-op after cleanup"
        );

        // Starting a different generation still works after those ordinary
        // host position updates.
        let next = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.5, 0.0, 0.5, 1.0),
            )
            .expect("a different shortcut should supersede the completed target");
        assert!(state.commit_placement_plan(&next));
        assert!(!state.record_origin(target.zaura_toplevel_host_id(), (500, 700)));
        assert!(state.abort_self_parent_resize(target.zaura_toplevel_host_id()));
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (500, 700)));
        assert_eq!(
            state.origin(target.zaura_toplevel_host_id()),
            Some((500, 700))
        );
    }

    #[test]
    fn completed_target_is_not_deduplicated_while_newer_transaction_is_active() {
        let completed_target = (0, 0, 1920, 1080);
        let newer_target = (1920, 0, 1920, 1080);
        let mut transaction = super::transaction::PlacementTransaction::default();
        transaction.assume_move_pending(completed_target, 44);
        assert_eq!(
            transaction.note_origin((completed_target.0, completed_target.1)),
            OriginResult::Accepted {
                transaction_complete: false
            }
        );
        assert!(transaction.begin_cleanup());
        assert!(transaction.complete_cleanup_barrier());
        assert!(transaction.complete());
        assert_eq!(transaction.last_completed_target(), Some(completed_target));

        // A different target is now in its resize phase. Pressing the old
        // completed shortcut must become the deferred latest target, not be
        // suppressed merely because the physical origin still happens to be
        // the old target.
        transaction.begin_resize(newer_target, 44, (1920, 1080));
        let mut state = WindowPlacementState::default();
        state.toplevels.insert(
            77,
            ToplevelPlacementState {
                origin: Some((completed_target.0, completed_target.1)),
                origin_confirmed: true,
                transaction,
                observed_size: None,
            },
        );
        assert!(
            !state.is_self_parent_target_current(77, completed_target),
            "last-completed deduplication is valid only while the reducer is idle"
        );
    }

    #[test]
    fn late_origin_after_real_cleanup_ack_does_not_rebase_next_resize() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(18);
        let shadow_table = register_target(&mut state, target);
        let host_id = target.zaura_toplevel_host_id();
        assert!(state.record_origin(host_id, (100, 200)));

        let plan = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("self-parent target should be valid");
        assert!(state.commit_placement_plan(&plan));
        assert!(state.accept_pending_resize(host_id, (1920, 1080)));
        assert!(state.mark_self_parent_move_queued(host_id));
        state.finish_self_parent_move(host_id);

        // This models the normal host path where the requested origin arrives
        // before the ordered self-parent cleanup barrier.
        assert!(state.record_origin(host_id, (0, 0)));
        assert!(state.complete_self_parent_cleanup_barrier(host_id));
        assert!(state.complete_self_parent_cleanup(host_id));

        // Once cleanup completes, late focus/animation origins are ignored
        // until a new generation explicitly clears the guard.
        assert!(!state.record_origin(host_id, (2500, 1500)));
        assert_eq!(state.origin(host_id), Some((0, 0)));

        let next = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.5, 0.0, 0.5, 1.0),
            )
            .expect("the next shortcut should remain usable");
        assert!(state.commit_placement_plan(&next));
        assert_eq!(state.pending_origin(host_id), Some((1920, 0)));
    }

    #[test]
    fn omitted_origin_cleanup_marks_baseline_unconfirmed_and_allows_retry() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(19);
        let shadow_table = register_target(&mut state, target);
        let host_id = target.zaura_toplevel_host_id();
        let original_origin = (100, 200);
        assert!(state.record_origin(host_id, original_origin));

        let first = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("first self-parent target should be valid");
        assert!(state.commit_placement_plan(&first));
        assert!(state.accept_pending_resize(host_id, (1920, 1080)));
        assert!(state.pending_self_parent_move(host_id).is_some());
        assert!(state.mark_self_parent_move_queued(host_id));
        state.finish_self_parent_move(host_id);

        // The host has acknowledged the resize and the ordered cleanup
        // barrier, but omitted the target origin event. The fallback must not
        // fabricate `(0, 0)` or retain a deferred shortcut that has no safe
        // relative baseline.
        assert!(state.complete_self_parent_cleanup_barrier(host_id));
        let second_rect = NormalizedRect::new(0.5, 0.0, 0.5, 0.5);
        let second = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                second_rect,
            )
            .expect("a later shortcut should be retained while cleanup is active");
        assert!(state.commit_placement_plan(&second));
        assert!(state.deferred_self_parent_target(host_id).is_some());

        assert!(state.settle_self_parent_cleanup_without_origin(host_id));
        assert!(!state.self_parent_transaction_active(host_id));
        assert_eq!(state.origin(host_id), Some(original_origin));
        assert_eq!(
            state.confirmed_origin(host_id),
            None,
            "the retained coordinate is diagnostic only until Aura reports a fresh origin"
        );
        assert_eq!(
            state.deferred_self_parent_target(host_id),
            None,
            "the deferred target must be discarded instead of becoming a phantom phase"
        );

        // Before a fresh origin, the next press is consumed as OriginUnknown,
        // not suppressed as AlreadyAtTarget. Once Aura reports its actual
        // position, the same requested rectangle can be planned and queued.
        assert_eq!(
            state
                .prepare_placement(
                    &shadow_table,
                    target.guest_xdg_toplevel_id(),
                    target.wl_surface_guest_id(),
                    second_rect,
                )
                .expect_err("an unconfirmed baseline must block relative placement"),
            WindowPlacementPlanError::OriginUnknown
        );
        assert!(state.record_origin(host_id, (0, 0)));
        assert!(state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                second_rect,
            )
            .is_ok());

        // A later resize phase may already be armed by the protocol adapter
        // when the first fresh Aura configure arrives. The retained
        // diagnostic origin must not reject that configure merely because it
        // differs from the unconfirmed coordinate.
        assert!(state.arm_self_parent_target(
            host_id,
            target.zaura_surface_host_id(),
            (0, 0, 1920, 1080),
        ));
        assert!(state.accept_pending_resize_at_origin(host_id, (1920, 1080), (0, 0),));
        assert_eq!(
            state.confirmed_origin(host_id),
            Some((0, 0)),
            "the first post-fallback configure must restore an authoritative baseline"
        );
    }

    #[test]
    fn stale_self_parent_cleanup_preserves_a_newer_resize_transaction() {
        let mut state = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(8);
        let shadow_table = register_target(&mut state, target);
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (100, 200)));

        let first = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.0, 0.0, 0.5, 0.5),
            )
            .expect("first self-parent target should be valid");
        assert!(state.commit_placement_plan(&first));
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 1080),));
        assert!(state
            .pending_self_parent_move(target.zaura_toplevel_host_id())
            .is_some());
        assert!(state.mark_self_parent_move_queued(target.zaura_toplevel_host_id()));

        // The second shortcut supersedes the first while its ordered cleanup
        // barrier is still in flight. It is retained as a deferred target; no
        // second resize transaction is published until that barrier.
        let second = state
            .prepare_placement(
                &shadow_table,
                target.guest_xdg_toplevel_id(),
                target.wl_surface_guest_id(),
                NormalizedRect::new(0.5, 0.0, 0.5, 1.0),
            )
            .expect("newer self-parent target should supersede the first");
        assert!(state.commit_placement_plan(&second));
        assert_eq!(
            state.pending_origin(target.zaura_toplevel_host_id()),
            Some((0, 0))
        );
        assert_eq!(
            state.pending_resize_size(target.zaura_toplevel_host_id()),
            None
        );
        assert_eq!(
            state.deferred_self_parent_target(target.zaura_toplevel_host_id()),
            Some((1920, 0, 1920, 2160))
        );

        // The old barrier completion must not erase the deferred target.
        state.finish_self_parent_move(target.zaura_toplevel_host_id());
        assert_eq!(
            state.pending_origin(target.zaura_toplevel_host_id()),
            Some((0, 0))
        );
        assert_eq!(
            state.pending_resize_size(target.zaura_toplevel_host_id()),
            None
        );
        assert!(
            !state.promote_deferred_self_parent_resize(
                target.zaura_toplevel_host_id(),
                (1920, 0, 1920, 2160),
            ),
            "a deferred target must wait for the first move's final origin"
        );
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (0, 0)));
        assert!(state.complete_self_parent_cleanup_barrier(target.zaura_toplevel_host_id()));
        assert!(state.promote_deferred_self_parent_resize(
            target.zaura_toplevel_host_id(),
            (1920, 0, 1920, 2160),
        ));
        assert_eq!(
            state.pending_origin(target.zaura_toplevel_host_id()),
            Some((1920, 0))
        );
        assert_eq!(
            state.pending_resize_size(target.zaura_toplevel_host_id()),
            Some((1920, 2160))
        );
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 2112),));
        assert!(state
            .pending_self_parent_move(target.zaura_toplevel_host_id())
            .is_some());
    }

    #[test]
    fn placement_plan_preserves_transient_identity_and_consumes_until_origin() {
        let mut transient = state_with_output(WindowPlacementMode::arc_bounds(
            WindowArcIdLifetime::Transient,
        ));
        let transient_target = placement_target(5);
        let transient_shadow = register_target(&mut transient, transient_target);
        transient.remember_native_application_id(20, "org.chromium.guest_os.native".to_string());
        let plan = transient
            .prepare_placement(
                &transient_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
            )
            .expect("transient plan should allocate both identities");
        let identity = plan
            .transient_arc_identity()
            .expect("transient plan should carry restore identity");
        assert!(identity
            .arc_application_id()
            .starts_with(ARC_TASK_APPLICATION_ID_PREFIX));
        assert_eq!(
            plan.barrier_cleanup(),
            Some(&PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: 80,
                wl_surface_guest_id: 20,
            })
        );

        let mut waiting = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        let waiting_target = placement_target(2);
        let waiting_shadow = register_target(&mut waiting, waiting_target);
        let error = waiting
            .prepare_placement(
                &waiting_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
            )
            .expect_err("self-parent must wait for the first authoritative origin");
        assert_eq!(error, WindowPlacementPlanError::OriginUnknown);
        assert!(error.consumes_shortcut());

        let mut released = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        let released_target = placement_target(2);
        let released_shadow = register_target(&mut released, released_target);
        assert!(released.record_origin(70, (100, 200)));
        let plan = released
            .prepare_placement(
                &released_shadow,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
            )
            .expect("plan preparation should not mutate the role");
        assert_eq!(released.take_aura_toplevel(10), Some(70));
        assert!(!released.commit_placement_plan(&plan));
    }

    #[test]
    fn transient_arc_requires_application_id_capability() {
        let mut transient = state_with_output(WindowPlacementMode::arc_bounds(
            WindowArcIdLifetime::Transient,
        ));
        let target = placement_target(4);
        let shadow_table = register_target(&mut transient, target);
        transient.remember_native_application_id(20, "org.chromium.guest_os.native".to_string());
        let error = transient
            .prepare_placement(
                &shadow_table,
                10,
                20,
                NormalizedRect::new(0.0, 0.0, 1.0, 1.0),
            )
            .expect_err("transient ARC identity must not run without set_application_id v5");
        assert_eq!(error, WindowPlacementPlanError::UnsupportedSurfaceVersion);
    }

    #[test]
    fn non_arc_backends_cannot_allocate_arc_metadata() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        assert_eq!(state.arc_policy_application_id(10), None);
    }

    #[test]
    fn arc_policy_id_is_stable_per_surface_and_unique_within_a_process_block() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let first = state
            .arc_policy_application_id(10)
            .expect("ARC backend should allocate an application ID");
        let second = state
            .arc_policy_application_id(11)
            .expect("ARC backend should allocate a second application ID");
        assert_ne!(first, second);
        assert!(first.starts_with(ARC_TASK_APPLICATION_ID_PREFIX));
        assert!(second.starts_with(ARC_TASK_APPLICATION_ID_PREFIX));
        assert_eq!(state.arc_policy_application_id(10), Some(first.clone()));
        assert_eq!(state.arc_policy_application_id(11), Some(second));

        assert!(state.remember_aura_surface(20, 30));
        assert_eq!(
            state.take_aura_surface_for_wl_surface(&shadow_for_surface(10, 20), 10),
            Some(30)
        );
        let replacement = state
            .arc_policy_application_id(10)
            .expect("released surface can receive a new ID");
        assert_ne!(replacement, first);
    }

    #[test]
    fn native_and_arc_ids_share_one_surface_lifetime() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let arc_id = state
            .arc_policy_application_id(10)
            .expect("ARC backend should allocate an application ID");
        state.remember_native_application_id(
            10,
            "org.chromium.guest_os.termina.wayland.editor".to_string(),
        );
        assert_eq!(
            state.native_application_id(10).as_deref(),
            Some("org.chromium.guest_os.termina.wayland.editor")
        );
        assert_eq!(state.arc_policy_application_id(10), Some(arc_id.clone()));

        assert!(state.remember_aura_surface(20, 30));
        assert_eq!(
            state.take_aura_surface_for_wl_surface(&shadow_for_surface(10, 20), 10),
            Some(30)
        );
        assert_eq!(state.native_application_id(10), None);
        let replacement = state
            .arc_policy_application_id(10)
            .expect("a released surface can allocate a replacement ID");
        assert_ne!(replacement, arc_id);
    }

    #[test]
    fn arc_policy_id_stays_inside_the_private_task_pool() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        let application_id = state
            .arc_policy_application_id(10)
            .expect("ARC backend should allocate a task-form application ID");
        let task_id = application_id
            .strip_prefix(ARC_TASK_APPLICATION_ID_PREFIX)
            .expect("ARC task-form prefix");
        let task_id = task_id.parse::<u32>().expect("numeric ARC task ID");
        assert!((ARC_TASK_ID_POOL_START..=ARC_TASK_ID_POOL_END).contains(&task_id));
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
        state.remember_native_application_id(10, "native".to_string());
        assert_eq!(
            state.take_aura_surface_for_wl_surface(&shadow_for_surface(10, 50), 10),
            Some(60)
        );
        assert_eq!(state.aura_surface_for_wl_surface(50), None);
        assert_eq!(state.native_application_id(10), None);
        assert!(state.remember_aura_surface(52, 60));

        register_xdg_role(&mut state, 10, 100);
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
        register_xdg_role(&mut state, 11, 101);
        assert!(state.remember_aura_toplevel(11, 71));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), None);
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(71), Some(11));
        assert!(state.record_origin(71, (8, 9)));
        assert!(state.register_barrier(80, 71, None));
        assert_eq!(state.take_aura_toplevel(11), Some(71));
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(71), None);
        assert_eq!(state.origin(71), None);
        assert!(!state.has_pending_barrier(71));
        assert_eq!(state.barrier_for_callback(80), Some(71));
    }

    #[test]
    fn origin_prediction_rejects_stale_events_until_target_arrives() {
        let mut state = WindowPlacementState::default();
        register_xdg_role(&mut state, 10, 100);
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
        register_xdg_role(&mut state, 10, 100);
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.record_origin(77, (10, 20)));
        assert!(state.register_barrier(40, 77, None));
        assert_eq!(state.take_aura_toplevel(10), Some(77));

        assert_eq!(state.origin(77), None);
        assert!(!state.has_pending_barrier(77));
        assert_eq!(state.barrier_for_callback(40), Some(77));
        assert_eq!(
            state
                .complete_barrier(40)
                .map(|completion| completion.toplevel_id),
            Some(77)
        );
    }

    #[test]
    fn stale_barrier_completion_does_not_clear_newer_active_barrier() {
        let mut state = WindowPlacementState::default();
        register_xdg_role(&mut state, 10, 100);
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.register_barrier(40, 77, None));
        assert!(state.register_barrier(41, 77, None));

        assert_eq!(
            state
                .complete_barrier(40)
                .map(|completion| completion.toplevel_id),
            Some(77)
        );
        assert_eq!(state.active_barrier_for_toplevel(77), Some(41));
        assert_eq!(
            state
                .complete_barrier(41)
                .map(|completion| completion.toplevel_id),
            Some(77)
        );
        assert!(!state.has_any_barriers());
    }

    #[test]
    fn duplicate_barrier_callback_id_is_rejected_without_mutation() {
        let mut state = WindowPlacementState::default();
        register_xdg_role(&mut state, 10, 100);
        assert!(state.remember_aura_toplevel(10, 77));
        assert!(state.register_barrier(40, 77, None));
        assert!(!state.register_barrier(40, 88, None));
        assert_eq!(state.barrier_for_callback(40), Some(77));
        assert_eq!(state.active_barrier_for_toplevel(77), Some(40));
        assert_eq!(state.active_barrier_for_toplevel(88), None);
    }

    #[test]
    fn released_toplevel_cannot_recreate_origin_or_barrier_state() {
        let mut state = WindowPlacementState::default();
        register_xdg_role(&mut state, 10, 100);
        assert!(state.remember_aura_toplevel(10, 77));
        assert_eq!(state.take_aura_toplevel(10), Some(77));

        assert!(!state.record_origin(77, (1, 2)));
        assert!(!state.predict_origin(77, (3, 4)));
        assert!(!state.register_barrier(40, 77, None));
        assert_eq!(state.origin(77), None);
        assert_eq!(state.pending_origin(77), None);
        assert_eq!(state.barrier_for_callback(40), None);
    }

    #[test]
    fn mismatched_aura_surface_teardown_preserves_application_identity() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        state.remember_native_application_id(10, "org.chromium.guest_os.native".to_string());
        let arc_id = state
            .arc_policy_application_id(10)
            .expect("ARC identity should be allocated");
        assert!(state.remember_aura_surface(20, 30));

        assert_eq!(
            state.take_aura_surface_for_wl_surface(&shadow_for_surface(10, 21), 10),
            None
        );
        assert_eq!(state.aura_surface_for_wl_surface(20), Some(30));
        assert_eq!(
            state.native_application_id(10).as_deref(),
            Some("org.chromium.guest_os.native")
        );
        assert_eq!(state.arc_policy_application_id(10), Some(arc_id));
    }

    #[test]
    fn orphaned_application_state_is_released_when_no_aura_surface_exists() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        state.remember_native_application_id(10, "org.chromium.guest_os.native".to_string());
        let arc_id = state
            .arc_policy_application_id(10)
            .expect("ARC identity should be allocated before Aura creation");

        state.take_orphaned_application_state_for_surface_destroy(10);

        assert_eq!(state.native_application_id(10), None);
        let replacement = state
            .arc_policy_application_id(10)
            .expect("destroyed surface can allocate a replacement identity");
        assert_ne!(replacement, arc_id);
    }

    #[test]
    fn xdg_role_associations_are_one_to_one_and_teardown_is_bidirectional() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_xdg_surface(10, 20));
        assert!(state.remember_xdg_surface(10, 20));
        assert!(!state.remember_xdg_surface(11, 20));
        assert!(!state.remember_xdg_surface(10, 21));
        assert_eq!(state.wl_surface_for_xdg_surface(10), Some(20));

        assert!(state.remember_xdg_toplevel(30, 20));
        assert!(state.remember_xdg_toplevel(30, 20));
        assert!(!state.remember_xdg_toplevel(31, 20));
        assert!(!state.remember_xdg_toplevel(30, 21));
        assert_eq!(state.xdg_toplevel_for_wl_surface(20), Some(30));

        let release = state
            .take_xdg_toplevel_for_destroy(30)
            .expect("live XDG role should have a teardown record");
        assert_eq!(
            release,
            XdgToplevelRelease {
                wl_surface_guest_id: 20,
                zaura_toplevel_host_id: None,
            }
        );
        assert_eq!(state.xdg_toplevel_for_wl_surface(20), None);
        assert_eq!(state.take_xdg_surface(10), Some(20));
        assert_eq!(state.wl_surface_for_xdg_surface(10), None);
    }

    #[test]
    fn synthetic_xdg_configure_serials_are_high_and_scoped_to_surface() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::disabled());
        assert!(state.remember_xdg_surface(10, 20));

        let serial = state
            .allocate_synthetic_xdg_configure_serial(10)
            .expect("live xdg_surface should receive a synthetic serial");
        assert!(serial >= 0xf000_0000);
        assert!(!state.consume_synthetic_xdg_configure_serial(10, serial + 1));
        assert!(state.consume_synthetic_xdg_configure_serial(10, serial));
        assert!(!state.consume_synthetic_xdg_configure_serial(10, serial));
    }

    #[test]
    fn stale_resize_ack_cannot_open_a_new_configure_generation() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::new(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::SelfParent,
        ));
        let target = placement_target(5);
        let _shadow = register_target(&mut state, target);
        assert!(state.record_origin(target.zaura_toplevel_host_id(), (100, 200)));
        assert!(state.arm_self_parent_target(
            target.zaura_toplevel_host_id(),
            target.zaura_surface_host_id(),
            (0, 0, 1920, 1080),
        ));
        let xdg_surface_guest_id = target.guest_xdg_toplevel_id() + 1000;
        let first_serial = state
            .allocate_synthetic_xdg_configure_serial(xdg_surface_guest_id)
            .expect("first synthetic serial");
        assert!(state.gate_resize_on_guest_commit(
            target.zaura_toplevel_host_id(),
            xdg_surface_guest_id,
            first_serial,
        ));

        // A newer shortcut replaces the active resize before the first
        // configure acknowledgement arrives.
        assert!(state.arm_self_parent_target(
            target.zaura_toplevel_host_id(),
            target.zaura_surface_host_id(),
            (1920, 0, 1920, 2160),
        ));
        let second_serial = state
            .allocate_synthetic_xdg_configure_serial(xdg_surface_guest_id)
            .expect("second synthetic serial");
        assert!(state.gate_resize_on_guest_commit(
            target.zaura_toplevel_host_id(),
            xdg_surface_guest_id,
            second_serial,
        ));

        // The old ack plus a current commit must not satisfy the second
        // generation.
        assert!(state.consume_synthetic_xdg_configure_serial(xdg_surface_guest_id, first_serial));
        assert!(!state.note_guest_surface_commit(target.wl_surface_guest_id()));
        assert!(!state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 2160)));

        // Only the matching serial can arm the new configure generation.
        assert!(state.consume_synthetic_xdg_configure_serial(xdg_surface_guest_id, second_serial));
        // The old commit was intentionally ignored.  A current
        // `ack_configure` therefore still needs its own client commit before
        // the host resize acknowledgement can open the move phase.
        assert!(state.note_guest_surface_commit(target.wl_surface_guest_id()));
        assert!(state.accept_pending_resize(target.zaura_toplevel_host_id(), (1920, 2160)));
    }

    #[test]
    fn destroying_xdg_surface_clears_pending_synthetic_serials() {
        let mut state = WindowPlacementState::new(WindowPlacementMode::disabled());
        assert!(state.remember_xdg_surface(10, 20));
        let serial = state
            .allocate_synthetic_xdg_configure_serial(10)
            .expect("live xdg_surface should receive a synthetic serial");

        assert_eq!(state.take_xdg_surface(10), Some(20));
        assert!(!state.consume_synthetic_xdg_configure_serial(10, serial));
        assert_eq!(state.xdg_surface_for_wl_surface(20), None);
    }

    #[test]
    fn child_role_associations_require_their_parent_links() {
        let mut state = WindowPlacementState::default();
        assert!(!state.remember_xdg_toplevel(30, 20));
        assert!(!state.remember_aura_toplevel(30, 70));

        assert!(state.remember_xdg_surface(10, 20));
        assert!(state.remember_xdg_toplevel(30, 20));
        assert!(state.remember_aura_toplevel(30, 70));
    }

    #[test]
    fn surface_link_teardown_removes_role_and_placement_state_atomically() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_xdg_surface(10, 20));
        assert!(state.remember_xdg_toplevel(30, 20));
        assert!(state.remember_aura_toplevel(30, 70));
        assert!(state.record_origin(70, (100, 200)));
        assert!(state.predict_origin(70, (300, 400)));
        assert_eq!(state.pending_origin(70), Some((300, 400)));
        assert!(state.register_barrier(40, 70, None));
        assert_eq!(
            state.take_xdg_links_for_wl_surface(20),
            Some(XdgToplevelRelease {
                wl_surface_guest_id: 20,
                zaura_toplevel_host_id: Some(70),
            })
        );
        assert_eq!(state.wl_surface_for_xdg_surface(10), None);
        assert_eq!(state.wl_surface_for_xdg_toplevel(30), None);
        assert_eq!(state.xdg_toplevel_for_wl_surface(20), None);
        assert_eq!(state.xdg_toplevel_for_aura_toplevel(70), None);
        assert_eq!(state.origin(70), None);
        assert_eq!(state.pending_origin(70), None);
        assert!(!state.has_pending_barrier(70));
        assert_eq!(state.barrier_for_callback(40), Some(70));
        assert_eq!(
            state
                .complete_barrier(40)
                .expect("stale callback remains callback-owned")
                .cleanup,
            None
        );
        assert_eq!(state.take_xdg_links_for_wl_surface(20), None);
    }

    #[test]
    fn output_registry_is_idempotent_and_ignores_non_current_modes() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_output(50));
        assert!(!state.remember_output(50));
        state.update_output_mode(50, true, 3840, 2160);
        state.update_output_mode(50, false, 1920, 1080);
        state.update_output_scale(50, 2);
        assert_eq!(
            state.primary_output(),
            Some((
                50,
                OutputState {
                    mode_width: 3840,
                    mode_height: 2160,
                    scale: 2,
                    ..Default::default()
                }
            ))
        );
        assert_eq!(
            state.bounds_for_rect(NormalizedRect::new(0.0, 0.0, 0.5, 0.5)),
            Some((50, (0, 0, 960, 540)))
        );
    }

    #[test]
    fn released_outputs_cannot_be_resurrected_by_delayed_events() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_output(50));
        state.update_output_mode(50, true, 3840, 2160);
        state.update_output_scale(50, 1);
        assert!(state.take_output(50));
        assert_eq!(state.primary_output(), None);

        // A host mode/scale event can be queued before delete_id reaches the
        // proxy. It must not recreate placement state for the retired object.
        state.update_output_mode(50, true, 1920, 1080);
        state.update_output_scale(50, 2);
        assert_eq!(state.primary_output(), None);
        assert!(!state.take_output(50));
    }

    #[test]
    fn aura_shell_binding_teardown_is_owned_by_global_generation() {
        let mut state = WindowPlacementState::default();
        assert!(state.set_aura_shell_binding(24, 7, 38));
        assert!(!state.set_aura_shell_binding(25, 8, 38));

        assert_eq!(state.aura_shell_id(), Some(24));
        assert_eq!(state.aura_shell_global_name(), Some(7));
        assert_eq!(state.aura_shell_version(), 38);
        assert_eq!(
            state.take_aura_shell_for_global(8),
            None,
            "a stale generation must not release the current shell"
        );
        assert_eq!(state.aura_shell_id(), Some(24));
        assert_eq!(state.aura_shell_global_name(), Some(7));
        assert_eq!(state.aura_shell_version(), 38);

        assert_eq!(state.take_aura_shell_for_global(7), Some((24, 38)));
        assert_eq!(state.aura_shell_id(), None);
        assert_eq!(state.aura_shell_global_name(), None);
        assert_eq!(state.aura_shell_version(), 0);

        assert!(state.set_aura_shell_binding(25, 8, 37));
        assert_eq!(state.aura_shell_id(), Some(25));
        assert_eq!(state.aura_shell_global_name(), Some(8));
        assert_eq!(state.aura_shell_version(), 37);
    }

    #[test]
    fn remote_shell_children_are_owned_by_their_manager_generation() {
        let mut state = WindowPlacementState::default();
        assert!(state.set_remote_shell_binding(24, 7, 6));
        assert!(state.remember_remote_surface_for_manager(100, 200, 24));
        assert!(
            state.remember_remote_surface_for_manager(100, 200, 24),
            "registering an already paired role must be idempotent"
        );
        assert!(
            !state.remember_remote_surface_for_manager(101, 200, 25),
            "a child cannot be rebound to a different manager generation"
        );
        assert!(state.remote_shell_has_children(24));
        assert_eq!(state.take_remote_shell_for_global(7), Some((24, 6)));
        assert!(state.remote_shell_has_children(24));

        let (remote_surface_id, manager) = state
            .take_remote_surface_for_wl_surface_with_cleanup(100)
            .expect("the manager child should be removable");
        assert_eq!(remote_surface_id, 200);
        assert_eq!(manager, Some((24, 6)));
        assert!(!state.remote_shell_has_children(24));
    }

    #[test]
    fn orphaned_remote_surface_metadata_is_retired_without_panicking() {
        let mut state = WindowPlacementState::default();
        assert!(state.set_remote_shell_binding(24, 7, 6));
        assert!(state.remember_remote_surface_for_manager(100, 200, 24));

        // Model a stale teardown path that removed the ownership record before
        // the surface link. The host child is still safe to destroy, but there
        // is no trustworthy manager generation to release.
        assert_eq!(state.remote_surface_owners.remove(&200), Some(24));
        let result = state
            .take_remote_surface_for_wl_surface_with_cleanup(100)
            .expect("the orphaned child link should still be retired");
        assert_eq!(result, (200, None));
        assert_eq!(state.remote_surface_for_wl_surface(100), None);
        assert!(!state.remote_shell_has_children(24));
    }

    #[test]
    fn gtk_shell_and_surface_state_has_one_authoritative_bidirectional_link() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_gtk_shell(10));
        assert!(!state.remember_gtk_shell(10));
        assert_eq!(
            state.gtk_shell_startup_and_surfaces(10),
            Some((None, Vec::new()))
        );
        assert!(!state.remember_gtk_surface(11, 99, 20));
        assert!(state.remember_gtk_surface(11, 10, 20));
        assert!(!state.remember_gtk_surface(11, 10, 21));
        assert!(state.remember_gtk_surface(12, 10, 20));
        assert_eq!(state.wl_surface_for_gtk_surface(11), Some(20));

        assert_eq!(
            state.update_gtk_shell_startup_id(10, Some("startup".to_string())),
            Some(vec![11, 12])
        );
        assert_eq!(
            state.gtk_shell_startup_and_surfaces(10),
            Some((Some("startup".to_string()), vec![11, 12]))
        );

        assert!(state.register_gtk_shell_capability_callback(40, 10));
        assert!(!state.register_gtk_shell_capability_callback(40, 10));
        assert_eq!(state.gtk_shell_capability_callback_for_test(10), Some(40));
        assert_eq!(state.take_gtk_shell_capability_callback(40), Some(10));
        assert_eq!(state.gtk_shell_capability_callback_for_test(10), None);

        assert_eq!(state.take_gtk_surfaces_for_wl_surface(20), vec![11, 12]);
        assert_eq!(state.wl_surface_for_gtk_surface(11), None);
        assert_eq!(state.wl_surface_for_gtk_surface(12), None);
        assert_eq!(
            state.gtk_shell_startup_and_surfaces(10),
            Some((Some("startup".to_string()), Vec::new()))
        );
    }
}
