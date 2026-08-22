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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use log::warn;

use crate::accelerator::Accelerator;
use crate::arc_task_ids::ArcTaskIdAllocator;
use crate::window_shortcuts::{NormalizedRect, ShortcutConfig, ShortcutConfigHandle};

/// Application-ID policy used for compositor-owned window operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowHostPolicy {
    /// Keep the normal Crostini/guest application namespace.
    #[default]
    Guest,
    /// Use the ARC task-compatible namespace required by the direct bounds policy.
    Arc,
}

/// Lifetime and shell-identity behavior of the ARC compatibility ID.
///
/// The modes are deliberately explicit because ChromeOS keeps several ARC
/// properties sticky after `set_application_id` resolves an ARC ID.  The
/// default persistent mode is the only behavior already verified to preserve
/// placement and IME on the custom host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowArcIdLifetime {
    /// Keep the ARC task-form ID on the Aura surface for the window lifetime.
    #[default]
    Persistent,
    /// Install the native ID initially, switch to ARC around each placement,
    /// and restore the native shell ID after the bounds request.
    Transient,
    /// Install ARC properties once, then restore the native shell ID while
    /// retaining the host's ARC policy properties for later bounds requests.
    PersistentNativeShell,
}

/// Geometry operation used for compositor-owned window shortcuts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowGeometryMethod {
    /// Do not consume or execute Sommelier-owned window shortcuts.
    #[default]
    None,
    /// Send `zaura_toplevel.set_window_bounds`.
    Bounds,
    /// Resize at the current origin, then send the experimental self-parent
    /// position probe.
    ///
    /// The bounds half still needs an application-ID policy that ChromeOS
    /// authorizes for arbitrary geometry. The named `set-parent` CLI backend
    /// therefore combines this method with the persistent ARC task policy;
    /// `guest + self-parent` remains available as a position-only probe.
    SelfParent,
}

/// Independent host-policy and geometry selections for window shortcuts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WindowPlacementMode {
    pub(crate) host_policy: WindowHostPolicy,
    pub(crate) geometry_method: WindowGeometryMethod,
    pub(crate) arc_id_lifetime: WindowArcIdLifetime,
}

impl WindowPlacementMode {
    pub(crate) const fn new(
        host_policy: WindowHostPolicy,
        geometry_method: WindowGeometryMethod,
    ) -> Self {
        Self {
            host_policy,
            geometry_method,
            arc_id_lifetime: WindowArcIdLifetime::Persistent,
        }
    }

    pub(crate) const fn with_arc_id_lifetime(
        mut self,
        arc_id_lifetime: WindowArcIdLifetime,
    ) -> Self {
        self.arc_id_lifetime = arc_id_lifetime;
        self
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
                "SOMMELIER_WINDOW_BOUNDS_SELF_PARENT is experimental; it resizes \
                 at the current origin before the self-parent position probe and \
                 may be unstable on custom ChromeOS hosts"
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
    ///
    /// This policy is intentionally independent from geometry selection:
    /// `arc + none` still installs the task-form compatibility ID on Aura but
    /// does not consume shortcuts. Use `guest + none` for a completely
    /// inactive placement feature.
    pub(crate) const fn uses_arc_policy(self) -> bool {
        matches!(self.host_policy, WindowHostPolicy::Arc)
    }

    /// Return the configured ARC application-ID lifetime behavior.
    pub(crate) const fn arc_id_lifetime(self) -> WindowArcIdLifetime {
        self.arc_id_lifetime
    }

    /// Return whether the ARC ID is only used around a bounds request.
    pub(crate) const fn uses_transient_arc_id(self) -> bool {
        matches!(self.arc_id_lifetime, WindowArcIdLifetime::Transient)
    }

    /// Return whether direct Aura bounds are selected.
    pub(crate) const fn uses_bounds(self) -> bool {
        matches!(self.geometry_method, WindowGeometryMethod::Bounds)
    }

    /// Return whether this backend runs the self-parent position-and-bounds path.
    pub(crate) const fn uses_self_parent(self) -> bool {
        matches!(self.geometry_method, WindowGeometryMethod::SelfParent)
    }
}

/// Process-wide immutable placement configuration and shared runtime handles.
///
/// A Sommelier process may serve several guest connections. Configuration and
/// resources that must be identical for every connection therefore live here,
/// while [`WindowPlacementState`] owns only connection-local protocol
/// lifetimes. The shortcut handle is internally generation-based so SIGHUP can
/// publish a new validated snapshot without replacing this owner.
#[derive(Debug, Clone)]
pub(crate) struct WindowPlacementRuntime {
    mode: WindowPlacementMode,
    shortcut_config: ShortcutConfigHandle,
    shortcut_config_path: Option<PathBuf>,
    host_accelerators: Arc<Vec<Accelerator>>,
    arc_task_allocator: Option<Arc<ArcTaskIdAllocator>>,
    vm_identifier: String,
}

pub(crate) type WindowPlacementRuntimeHandle = Arc<WindowPlacementRuntime>;

/// Result of a SIGHUP shortcut-configuration reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutReloadResult {
    NoPath,
    Reloaded,
    RejectedWhileDisabled,
    Invalid,
}

impl WindowPlacementRuntime {
    /// Construct the one process-wide placement runtime.
    pub(crate) fn new(
        mode: WindowPlacementMode,
        shortcut_config: ShortcutConfigHandle,
        shortcut_config_path: Option<PathBuf>,
        host_accelerators: Arc<Vec<Accelerator>>,
        arc_task_allocator: Option<Arc<ArcTaskIdAllocator>>,
    ) -> WindowPlacementRuntimeHandle {
        #[cfg(test)]
        let arc_task_allocator = if mode.uses_arc_policy() {
            arc_task_allocator
                .or_else(|| Some(ArcTaskIdAllocator::for_test(2_000_000_000, 2_000_000_999)))
        } else {
            arc_task_allocator
        };
        let arc_task_allocator = mode
            .uses_arc_policy()
            .then_some(arc_task_allocator)
            .flatten();

        Arc::new(Self {
            mode,
            shortcut_config,
            shortcut_config_path,
            host_accelerators,
            arc_task_allocator,
            vm_identifier: resolve_vm_identifier(std::env::var("SOMMELIER_VM_IDENTIFIER").ok()),
        })
    }

    /// Construct the runtime used by in-process/default contexts.
    pub(crate) fn from_environment() -> WindowPlacementRuntimeHandle {
        Self::new(
            WindowPlacementMode::from_environment(),
            ShortcutConfigHandle::disabled(),
            None,
            Arc::new(crate::accelerator::from_environment()),
            None,
        )
    }

    /// Return the process-wide placement mode.
    pub(crate) const fn mode(&self) -> WindowPlacementMode {
        self.mode
    }

    /// Return the currently published immutable shortcut generation.
    pub(crate) fn shortcut_config_snapshot(&self) -> Arc<ShortcutConfig> {
        self.shortcut_config.snapshot()
    }

    /// Return the host accelerators used to reject conflicting bindings and
    /// suppress host-owned shortcuts.
    pub(crate) fn host_accelerators(&self) -> &[Accelerator] {
        self.host_accelerators.as_ref()
    }

    /// Allocate one task ID from the process-owned block.
    pub(crate) fn allocate_arc_task_id(&self) -> std::io::Result<u32> {
        self.arc_task_allocator
            .as_ref()
            .ok_or_else(|| std::io::Error::other("ARC task allocator is disabled"))
            .and_then(|allocator| allocator.allocate())
    }

    /// Reload the optional shortcut file as one complete generation.
    ///
    /// Invalid input leaves the last known-good snapshot untouched. The typed
    /// result lets callers distinguish an absent path from rejected input
    /// without reimplementing reload policy outside this owner.
    pub(crate) fn reload_shortcuts(&self) -> ShortcutReloadResult {
        let Some(path) = self.shortcut_config_path.as_deref() else {
            log::debug!("Ignoring SIGHUP: no window shortcut config path was supplied");
            return ShortcutReloadResult::NoPath;
        };
        match ShortcutConfig::load_from_path(path, self.host_accelerators.as_ref()) {
            Ok(config) => {
                if !config.is_empty() && !self.mode.handles_shortcuts() {
                    log::error!(
                        "Keeping the previous window shortcut configuration; \
                         {} contains bindings but the geometry method is disabled",
                        path.display()
                    );
                    return ShortcutReloadResult::RejectedWhileDisabled;
                }
                self.shortcut_config.replace(config);
                log::info!(
                    "Reloaded window shortcut configuration from {}",
                    path.display()
                );
                ShortcutReloadResult::Reloaded
            }
            Err(error) => {
                log::error!(
                    "Keeping the previous window shortcut configuration; \
                     reload of {} failed: {}",
                    path.display(),
                    error
                );
                ShortcutReloadResult::Invalid
            }
        }
    }

    /// Reuse the runtime's current immutable inputs with a test-only mode.
    #[cfg(test)]
    fn with_mode_for_test(
        &self,
        mode: WindowPlacementMode,
        shortcut_config: ShortcutConfigHandle,
    ) -> WindowPlacementRuntimeHandle {
        Self::new(
            mode,
            shortcut_config,
            self.shortcut_config_path.clone(),
            self.host_accelerators.clone(),
            self.arc_task_allocator.clone(),
        )
    }
}

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
}

/// Native/ARC identity transition required by transient ARC placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransientArcIdentity {
    pub(crate) arc_application_id: String,
    pub(crate) native_application_id: String,
}

/// Fully validated placement operation returned by the state owner.
///
/// Handlers serialize this value into protocol messages but do not choose the
/// geometry backend, allocate ARC IDs, or mutate predicted origins themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowPlacementPlan {
    pub(crate) output_host_id: u32,
    pub(crate) bounds: (i32, i32, i32, i32),
    pub(crate) geometry: WindowPlacementGeometry,
    pub(crate) transient_arc_identity: Option<TransientArcIdentity>,
}

/// Why a shortcut could not produce a placement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowPlacementPlanError {
    Disabled,
    NoUsableOutput,
    UnsupportedGeometry,
    UnsupportedSurfaceVersion,
    OriginUnknown,
    CoordinateOverflow,
    NativeApplicationIdMissing,
    ArcTaskIdUnavailable,
}

impl WindowPlacementPlanError {
    /// Whether the keyboard event must be consumed while state catches up.
    pub(crate) const fn consumes_shortcut(self) -> bool {
        matches!(self, Self::OriginUnknown)
    }
}

/// Host output geometry used by compositor-owned window layout requests.
///
/// `wl_output.mode` reports pixel dimensions while Aura window bounds use
/// logical screen coordinates. `scale` converts the former into the latter;
/// output insets remove shelf/non-work-area margins when they are known.
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

/// Prefix for the numeric ARC task-form application IDs used by placement.
pub(crate) const ARC_TASK_APPLICATION_ID_PREFIX: &str = "org.chromium.arc.";
#[cfg(test)]
pub(crate) use crate::arc_task_ids::{ARC_TASK_ID_POOL_END, ARC_TASK_ID_POOL_START};
const DEFAULT_VM_IDENTIFIER: &str = "termina";

/// Resolve the VM namespace used in ChromeOS guest application IDs.
///
/// An exported-but-empty environment variable is equivalent to an unset one,
/// matching ChromiumOS Sommelier's fallback to the standard Crostini VM name.
fn resolve_vm_identifier(value: Option<String>) -> String {
    value
        .filter(|identifier| !identifier.is_empty())
        .unwrap_or_else(|| DEFAULT_VM_IDENTIFIER.to_string())
}

#[derive(Debug, Default)]
struct ToplevelPlacementState {
    /// Last authoritative or predicted screen-space origin.
    origin: Option<(i32, i32)>,
    /// Newest self-parent target waiting for a matching host notification.
    pending_origin: Option<(i32, i32)>,
}

#[derive(Debug, Clone, Copy)]
struct AuraShellBinding {
    host_id: u32,
    global_name: u32,
    version: u32,
}

#[derive(Debug, Clone, Copy)]
struct OutputRecord {
    host_id: u32,
    state: OutputState,
}

#[derive(Debug, Default)]
struct GtkShellState {
    /// Activation token supplied by GTK for windows created through this
    /// shell binding. ChromeOS validates the token before granting focus.
    startup_id: Option<String>,
    /// Synthetic gtk_surface1 objects created from this shell binding.
    surfaces: HashSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GtkSurfaceState {
    shell_id: u32,
    wl_surface_id: u32,
}

/// One-to-one association with both lookup directions owned together.
///
/// Placement lifecycle code frequently needs to resolve either side of a
/// guest/host relationship. Keeping the two maps behind this type prevents a
/// caller from updating one direction without the reverse direction and makes
/// conflict handling identical for every association kind.
#[derive(Debug, Default)]
struct BidirectionalLinks {
    forward: HashMap<u32, u32>,
    reverse: HashMap<u32, u32>,
}

impl BidirectionalLinks {
    /// Insert an association if both IDs are unused or already paired.
    #[must_use = "a conflicting association must not replace live state"]
    fn insert(&mut self, forward_id: u32, reverse_id: u32) -> bool {
        match (
            self.forward.get(&forward_id).copied(),
            self.reverse.get(&reverse_id).copied(),
        ) {
            (None, None) => {
                self.forward.insert(forward_id, reverse_id);
                self.reverse.insert(reverse_id, forward_id);
                true
            }
            (Some(existing_reverse), Some(existing_forward))
                if existing_reverse == reverse_id && existing_forward == forward_id =>
            {
                true
            }
            _ => false,
        }
    }

    /// Resolve the reverse-side ID for one forward-side ID.
    fn get_forward(&self, forward_id: u32) -> Option<u32> {
        self.forward.get(&forward_id).copied()
    }

    /// Resolve the forward-side ID for one reverse-side ID.
    fn get_reverse(&self, reverse_id: u32) -> Option<u32> {
        self.reverse.get(&reverse_id).copied()
    }

    /// Remove an association by its forward-side ID.
    fn remove_forward(&mut self, forward_id: u32) -> Option<u32> {
        let reverse_id = self.forward.remove(&forward_id)?;
        self.reverse.remove(&reverse_id);
        Some(reverse_id)
    }

    /// Remove an association by its reverse-side ID.
    fn remove_reverse(&mut self, reverse_id: u32) -> Option<u32> {
        let forward_id = self.reverse.remove(&reverse_id)?;
        self.forward.remove(&forward_id);
        Some(forward_id)
    }

    /// Return whether both lookup directions contain exactly the same pairs.
    fn is_consistent(&self) -> bool {
        self.forward.len() == self.reverse.len()
            && self
                .forward
                .iter()
                .all(|(forward_id, reverse_id)| self.reverse.get(reverse_id) == Some(forward_id))
            && self
                .reverse
                .iter()
                .all(|(reverse_id, forward_id)| self.forward.get(forward_id) == Some(reverse_id))
    }
}

/// Ordered host-sync callbacks retained for placement requests.
///
/// A toplevel may have several callbacks in flight when the user presses
/// shortcuts quickly. `by_callback` retains every callback until its terminal
/// host event, while `active_by_toplevel` identifies the newest callback for
/// each toplevel. The callback establishes host-stream ordering; it does not
/// suppress `xdg_toplevel.configure` events. Keeping these maps behind one
/// owner prevents stale callback completion from clearing newer placement
/// bookkeeping.
#[derive(Debug, Default)]
struct PlacementBarrierRegistry {
    by_callback: HashMap<u32, PlacementBarrierRecord>,
    active_by_toplevel: HashMap<u32, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlacementBarrierRecord {
    toplevel_id: u32,
    /// Surface that received a self-parent request and must be unparented
    /// after this barrier, if this callback is still the active placement.
    self_parent_surface_id: Option<u32>,
}

/// Result of completing one placement barrier.
///
/// A stale callback still returns a completion so its host object can finish
/// its lifecycle, but it does not carry cleanup for a superseded placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlacementBarrierCompletion {
    pub(crate) toplevel_id: u32,
    pub(crate) self_parent_surface_id: Option<u32>,
}

impl PlacementBarrierRegistry {
    /// Return whether a callback ID is already retained by this registry.
    fn contains_callback(&self, callback_id: u32) -> bool {
        self.by_callback.contains_key(&callback_id)
    }

    /// Retain a callback and make it the latest sync for its toplevel.
    ///
    /// An existing active callback is intentionally superseded but remains in
    /// `by_callback` until its host-side terminal event arrives.
    fn register(
        &mut self,
        callback_id: u32,
        toplevel_id: u32,
        self_parent_surface_id: Option<u32>,
    ) -> bool {
        if self.contains_callback(callback_id) {
            return false;
        }
        self.by_callback.insert(
            callback_id,
            PlacementBarrierRecord {
                toplevel_id,
                self_parent_surface_id,
            },
        );
        self.active_by_toplevel.insert(toplevel_id, callback_id);
        true
    }

    /// Complete one callback and return the cleanup for the active placement.
    ///
    /// Only the callback that is still active for that toplevel may clear the
    /// active entry; an older superseded callback remains callback-owned until
    /// its own completion without disturbing the newer barrier or unparenting
    /// the newer placement.
    fn complete(&mut self, callback_id: u32) -> Option<PlacementBarrierCompletion> {
        let record = self.by_callback.remove(&callback_id)?;
        let is_active = self.active_by_toplevel.get(&record.toplevel_id) == Some(&callback_id);
        if is_active {
            self.active_by_toplevel.remove(&record.toplevel_id);
        }
        Some(PlacementBarrierCompletion {
            toplevel_id: record.toplevel_id,
            self_parent_surface_id: is_active.then_some(record.self_parent_surface_id).flatten(),
        })
    }

    /// Clear the latest pending sync for a released toplevel.
    ///
    /// Retained callbacks are deliberately left untouched because the host
    /// may still emit their terminal events and their numeric IDs remain
    /// reserved until then.
    fn release_toplevel(&mut self, toplevel_id: u32) {
        self.active_by_toplevel.remove(&toplevel_id);
    }

    /// Return whether a toplevel currently has a pending sync callback.
    fn has_pending(&self, toplevel_id: u32) -> bool {
        self.active_by_toplevel.contains_key(&toplevel_id)
    }

    /// Return whether no callback ownership remains.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.by_callback.is_empty() && self.active_by_toplevel.is_empty()
    }

    /// Check the forward/reverse barrier invariants.
    fn is_consistent(&self, is_live_toplevel: impl Fn(u32) -> bool) -> bool {
        self.active_by_toplevel
            .iter()
            .all(|(toplevel_id, callback_id)| {
                self.by_callback
                    .get(callback_id)
                    .is_some_and(|record| record.toplevel_id == *toplevel_id)
                    && is_live_toplevel(*toplevel_id)
            })
    }

    #[cfg(test)]
    fn callback_for(&self, callback_id: u32) -> Option<u32> {
        self.by_callback
            .get(&callback_id)
            .map(|record| record.toplevel_id)
    }

    #[cfg(test)]
    fn active_callback_for(&self, toplevel_id: u32) -> Option<u32> {
        self.active_by_toplevel.get(&toplevel_id).copied()
    }
}

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
    /// Latest native Guest OS application ID for each guest surface.
    ///
    /// The XDG role always retains this identity even when the Aura surface
    /// uses the persistent ARC task-form compatibility ID. Keeping the value
    /// here documents the two namespaces and leaves a safe restore target for
    /// future host capabilities without changing the current placement wire
    /// sequence.
    native_application_ids: HashMap<u32, String>,
    arc_application_ids: HashMap<u32, String>,
    aura_surface_links: BidirectionalLinks,
    aura_toplevel_links: BidirectionalLinks,
    xdg_surface_links: BidirectionalLinks,
    xdg_toplevel_links: BidirectionalLinks,
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
            native_application_ids: HashMap::new(),
            arc_application_ids: HashMap::new(),
            aura_surface_links: BidirectionalLinks::default(),
            aura_toplevel_links: BidirectionalLinks::default(),
            xdg_surface_links: BidirectionalLinks::default(),
            xdg_toplevel_links: BidirectionalLinks::default(),
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
            self.runtime.vm_identifier, app_id
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
        self.native_application_ids
            .insert(wl_surface_guest_id, application_id);
    }

    /// Return the latest native Guest OS application ID for one surface.
    pub(crate) fn native_application_id(&self, wl_surface_guest_id: u32) -> Option<String> {
        self.native_application_ids
            .get(&wl_surface_guest_id)
            .cloned()
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
        if current || output.mode_width == 0 || output.mode_height == 0 {
            output.mode_width = width;
            output.mode_height = height;
        }
        self.debug_assert_consistent();
    }

    /// Update the scale advertised by one output.
    pub(crate) fn update_output_scale(&mut self, host_output_id: u32, scale: i32) {
        let Some(output) = self.output_state_mut(host_output_id) else {
            return;
        };
        output.scale = scale;
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

    /// Validate and prepare one shortcut placement.
    ///
    /// This is the only state operation that turns a user rectangle into a
    /// backend-specific operation. It owns output selection, origin
    /// validation, and transient ARC identity allocation; callers only encode
    /// the returned plan and enqueue the sync barrier before committing it.
    pub(crate) fn prepare_placement(
        &mut self,
        rect: NormalizedRect,
        zaura_toplevel_host_id: u32,
        wl_surface_guest_id: u32,
        zaura_surface_version: u32,
    ) -> Result<WindowPlacementPlan, WindowPlacementPlanError> {
        if !self.handles_shortcuts() {
            return Err(WindowPlacementPlanError::Disabled);
        }
        let Some((output_host_id, bounds)) = self.bounds_for_rect(rect) else {
            return Err(WindowPlacementPlanError::NoUsableOutput);
        };
        if self.uses_self_parent() && self.uses_transient_arc_id() {
            return Err(WindowPlacementPlanError::UnsupportedGeometry);
        }

        let geometry = if self.uses_self_parent() {
            if zaura_surface_version < 2 {
                return Err(WindowPlacementPlanError::UnsupportedSurfaceVersion);
            }
            let Some(current_origin) = self.origin(zaura_toplevel_host_id) else {
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
            let Some(native_application_id) = self.native_application_id(wl_surface_guest_id)
            else {
                return Err(WindowPlacementPlanError::NativeApplicationIdMissing);
            };
            let Some(arc_application_id) = self.arc_policy_application_id(wl_surface_guest_id)
            else {
                return Err(WindowPlacementPlanError::ArcTaskIdUnavailable);
            };
            Some(TransientArcIdentity {
                arc_application_id,
                native_application_id,
            })
        } else {
            None
        };

        Ok(WindowPlacementPlan {
            output_host_id,
            bounds,
            geometry,
            transient_arc_identity,
        })
    }

    /// Commit the state transition represented by an already-queued plan.
    ///
    /// Preparation intentionally does not publish a predicted origin. This
    /// keeps a failed identity/message encoding from leaving lifecycle state
    /// that claims a request was sent. Direct bounds have no local transition;
    /// self-parent plans publish the target only after their wire sequence is
    /// queued.
    #[must_use = "a self-parent prediction may target a released toplevel"]
    pub(crate) fn commit_placement_plan(
        &mut self,
        zaura_toplevel_host_id: u32,
        plan: &WindowPlacementPlan,
    ) -> bool {
        match plan.geometry {
            WindowPlacementGeometry::Bounds => true,
            WindowPlacementGeometry::SelfParent { .. } => {
                self.predict_origin(zaura_toplevel_host_id, (plan.bounds.0, plan.bounds.1))
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

    /// Remove an xdg_surface association.
    pub(crate) fn take_xdg_surface(&mut self, xdg_surface_guest_id: u32) -> Option<u32> {
        let wl_surface_guest_id = self
            .xdg_surface_links
            .remove_forward(xdg_surface_guest_id)?;
        self.debug_assert_consistent();
        Some(wl_surface_guest_id)
    }

    /// Record the xdg_toplevel → wl_surface role association.
    #[must_use = "the XDG toplevel association may conflict with a live role"]
    pub(crate) fn remember_xdg_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        wl_surface_guest_id: u32,
    ) -> bool {
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

    /// Remove one xdg_toplevel role association.
    pub(crate) fn take_xdg_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
        let wl_surface_guest_id = self
            .xdg_toplevel_links
            .remove_forward(xdg_toplevel_guest_id)?;
        self.debug_assert_consistent();
        Some(wl_surface_guest_id)
    }

    /// Remove all XDG role links for one wl_surface and return its toplevels.
    ///
    /// The caller releases each returned Aura child after this state
    /// transition, so a malformed destroy ordering cannot leave stale links
    /// that route a later app-id request to an unrelated surface.
    pub(crate) fn take_xdg_links_for_wl_surface(&mut self, wl_surface_guest_id: u32) -> Vec<u32> {
        self.xdg_surface_links.remove_reverse(wl_surface_guest_id);
        let toplevels = self
            .xdg_toplevel_links
            .remove_reverse(wl_surface_guest_id)
            .into_iter()
            .collect::<Vec<_>>();
        self.debug_assert_consistent();
        toplevels
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
        if let Some(application_id) = self.arc_application_ids.get(&wl_surface_guest_id) {
            return Some(application_id.clone());
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
        self.arc_application_ids
            .insert(wl_surface_guest_id, application_id.clone());
        Some(application_id)
    }

    /// Remove the ARC identity and Aura-surface link for a destroyed surface.
    ///
    /// The guest ID owns the ARC application ID while the host ID owns the
    /// Aura-surface association. Requiring both IDs keeps their lifetimes
    /// coupled at the only teardown boundary that has authoritative ownership
    /// of both objects.
    #[must_use = "the returned Aura surface ID identifies host teardown work"]
    pub(crate) fn take_aura_surface_for_wl_surface(
        &mut self,
        wl_surface_guest_id: u32,
        wl_surface_host_id: u32,
    ) -> Option<u32> {
        self.native_application_ids.remove(&wl_surface_guest_id);
        self.arc_application_ids.remove(&wl_surface_guest_id);
        let zaura_surface_host_id = self.aura_surface_links.remove_forward(wl_surface_host_id);
        self.debug_assert_consistent();
        zaura_surface_host_id
    }

    /// Return the Aura surface associated with a host wl_surface.
    pub(crate) fn aura_surface_for_wl_surface(&self, wl_surface_host_id: u32) -> Option<u32> {
        self.aura_surface_links.get_forward(wl_surface_host_id)
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
    /// Returns `false` when either side is already associated with a
    /// different object. Replacing a live association would orphan the old
    /// host object and make its destructor impossible to route.
    #[must_use = "the association may conflict with an existing live object"]
    pub(crate) fn remember_aura_toplevel(
        &mut self,
        xdg_toplevel_guest_id: u32,
        zaura_toplevel_host_id: u32,
    ) -> bool {
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
    pub(crate) fn take_aura_toplevel(&mut self, xdg_toplevel_guest_id: u32) -> Option<u32> {
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
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
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
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
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
        self.barriers.release_toplevel(zaura_toplevel_host_id);
    }

    /// Register a newly queued host sync callback as the latest pending sync.
    ///
    /// A callback ID must be globally unique for the connection. Returning
    /// `false` instead of overwriting an existing entry keeps an older
    /// callback's terminal lifecycle reachable.
    #[must_use = "the barrier was rejected and must not be queued"]
    pub(crate) fn register_barrier(
        &mut self,
        callback_host_id: u32,
        zaura_toplevel_host_id: u32,
        self_parent_surface_id: Option<u32>,
    ) -> bool {
        if self
            .aura_toplevel_links
            .get_reverse(zaura_toplevel_host_id)
            .is_none()
        {
            return false;
        }
        if !self.barriers.register(
            callback_host_id,
            zaura_toplevel_host_id,
            self_parent_surface_id,
        ) {
            return false;
        }
        self.debug_assert_consistent();
        true
    }

    /// Retire a completed barrier and return cleanup only if it is still newest.
    #[must_use = "the returned completion identifies which placement finished"]
    pub(crate) fn complete_barrier(
        &mut self,
        callback_host_id: u32,
    ) -> Option<PlacementBarrierCompletion> {
        let completion = self.barriers.complete(callback_host_id)?;
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

impl Default for WindowPlacementState {
    fn default() -> Self {
        Self::new(WindowPlacementMode::disabled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bidirectional_links_are_atomic_and_reject_conflicts() {
        let mut links = BidirectionalLinks::default();
        assert!(links.insert(1, 10));
        assert!(links.is_consistent());
        assert_eq!(links.get_forward(1), Some(10));
        assert_eq!(links.get_reverse(10), Some(1));

        assert!(links.insert(1, 10));
        assert!(!links.insert(1, 11));
        assert!(!links.insert(2, 10));
        assert!(links.is_consistent());

        assert_eq!(links.remove_reverse(10), Some(1));
        assert!(links.is_consistent());
        assert_eq!(links.remove_forward(1), None);

        links.forward.insert(3, 30);
        assert!(!links.is_consistent());
        assert!(!links.insert(3, 30));
        assert!(!links.is_consistent());
    }

    #[test]
    fn placement_barriers_retain_superseded_callbacks_until_completion() {
        let mut barriers = PlacementBarrierRegistry::default();
        assert!(barriers.register(40, 77, None));
        assert!(barriers.is_consistent(|toplevel_id| toplevel_id == 77));
        assert_eq!(barriers.active_callback_for(77), Some(40));

        assert!(barriers.register(41, 77, None));
        assert!(barriers.is_consistent(|toplevel_id| toplevel_id == 77));
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(barriers.callback_for(40), Some(77));
        assert_eq!(barriers.callback_for(41), Some(77));

        assert!(!barriers.register(41, 88, None));
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(
            barriers
                .complete(40)
                .map(|completion| completion.toplevel_id),
            Some(77)
        );
        assert_eq!(barriers.active_callback_for(77), Some(41));
        assert_eq!(
            barriers
                .complete(41)
                .map(|completion| completion.toplevel_id),
            Some(77)
        );
        assert!(barriers.is_empty());

        let mut released = PlacementBarrierRegistry::default();
        assert!(released.register(50, 99, None));
        assert!(!released.is_consistent(|_| false));
        released.release_toplevel(99);
        assert!(released.is_consistent(|_| false));
        assert_eq!(
            released
                .complete(50)
                .map(|completion| completion.toplevel_id),
            Some(99)
        );
        assert!(released.is_empty());
    }

    #[test]
    fn only_active_barrier_returns_self_parent_cleanup() {
        let mut barriers = PlacementBarrierRegistry::default();
        assert!(barriers.register(40, 77, Some(55)));
        assert!(barriers.register(41, 77, Some(55)));

        assert_eq!(
            barriers
                .complete(40)
                .expect("superseded callback must complete")
                .self_parent_surface_id,
            None,
            "stale completion must not unparent the newer placement"
        );
        assert_eq!(
            barriers
                .complete(41)
                .expect("active callback must complete")
                .self_parent_surface_id,
            Some(55)
        );
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
        assert_eq!(
            state.native_application_id(10).as_deref(),
            Some("org.chromium.guest_os.termina.wayland.com.example.Terminal")
        );
        assert_eq!(state.take_aura_surface_for_wl_surface(10, 20), None);
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
    fn placement_plan_is_the_single_backend_decision_point() {
        let mut bounds = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
        ));
        assert!(bounds.remember_aura_toplevel(10, 70));
        let direct = bounds
            .prepare_placement(NormalizedRect::new(0.5, 0.0, 0.5, 1.0), 70, 20, 1)
            .expect("direct bounds plan should be valid");
        assert_eq!(direct.output_host_id, 50);
        assert_eq!(direct.bounds, (1920, 0, 1920, 2160));
        assert_eq!(direct.geometry, WindowPlacementGeometry::Bounds);
        assert_eq!(direct.transient_arc_identity, None);

        let mut self_parent = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        assert!(self_parent.remember_aura_toplevel(10, 70));
        assert!(self_parent.record_origin(70, (100, 200)));
        let probe = self_parent
            .prepare_placement(NormalizedRect::new(0.0, 0.0, 0.5, 0.5), 70, 20, 2)
            .expect("self-parent plan should be valid");
        assert_eq!(
            probe.geometry,
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (-100, -200),
            }
        );
        assert_eq!(self_parent.pending_origin(70), None);
        assert!(self_parent.commit_placement_plan(70, &probe));
        assert_eq!(self_parent.pending_origin(70), Some((0, 0)));
    }

    #[test]
    fn placement_plan_preserves_transient_identity_and_consumes_until_origin() {
        let mut transient = state_with_output(
            WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
                .with_arc_id_lifetime(WindowArcIdLifetime::Transient),
        );
        assert!(transient.remember_aura_toplevel(10, 70));
        transient.remember_native_application_id(20, "org.chromium.guest_os.native".to_string());
        let plan = transient
            .prepare_placement(NormalizedRect::new(0.0, 0.0, 1.0, 1.0), 70, 20, 1)
            .expect("transient plan should allocate both identities");
        let identity = plan
            .transient_arc_identity
            .expect("transient plan should carry restore identity");
        assert_eq!(
            identity.native_application_id,
            "org.chromium.guest_os.native"
        );
        assert!(identity
            .arc_application_id
            .starts_with(ARC_TASK_APPLICATION_ID_PREFIX));

        let mut waiting = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        assert!(waiting.remember_aura_toplevel(10, 70));
        let error = waiting
            .prepare_placement(NormalizedRect::new(0.0, 0.0, 1.0, 1.0), 70, 20, 2)
            .expect_err("self-parent must wait for the first authoritative origin");
        assert_eq!(error, WindowPlacementPlanError::OriginUnknown);
        assert!(error.consumes_shortcut());

        let mut released = state_with_output(WindowPlacementMode::new(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
        ));
        assert!(released.remember_aura_toplevel(10, 70));
        assert!(released.record_origin(70, (100, 200)));
        let plan = released
            .prepare_placement(NormalizedRect::new(0.0, 0.0, 1.0, 1.0), 70, 20, 2)
            .expect("plan preparation should not mutate the role");
        assert_eq!(released.take_aura_toplevel(10), Some(70));
        assert!(!released.commit_placement_plan(70, &plan));
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

        assert_eq!(state.take_aura_surface_for_wl_surface(10, 20), None);
        let replacement = state
            .arc_policy_application_id(10)
            .expect("released surface can receive a new ID");
        assert_ne!(replacement, first);
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
        assert_eq!(state.take_aura_surface_for_wl_surface(10, 50), Some(60));
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

        assert_eq!(state.take_xdg_toplevel(30), Some(20));
        assert_eq!(state.xdg_toplevel_for_wl_surface(20), None);
        assert_eq!(state.take_xdg_surface(10), Some(20));
        assert_eq!(state.wl_surface_for_xdg_surface(10), None);
    }

    #[test]
    fn surface_link_teardown_removes_all_role_directions() {
        let mut state = WindowPlacementState::default();
        assert!(state.remember_xdg_surface(10, 20));
        assert!(state.remember_xdg_toplevel(30, 20));
        assert_eq!(state.take_xdg_links_for_wl_surface(20), vec![30]);
        assert_eq!(state.wl_surface_for_xdg_surface(10), None);
        assert_eq!(state.wl_surface_for_xdg_toplevel(30), None);
        assert_eq!(state.xdg_toplevel_for_wl_surface(20), None);
        assert_eq!(state.take_xdg_links_for_wl_surface(20), Vec::<u32>::new());
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
