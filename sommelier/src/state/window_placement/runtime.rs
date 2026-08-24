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

//! Process-wide configuration shared by every placement connection.
//!
//! This module contains no protocol-lifetime state. Keeping mode selection,
//! shortcut generations, VM naming, and the ARC task allocator here prevents
//! one connection from changing the policy observed by another connection.

use std::path::PathBuf;
use std::sync::Arc;

use log::warn;

use crate::accelerator::Accelerator;
use crate::arc_task_ids::ArcTaskIdAllocator;
use crate::window_shortcuts::{ShortcutConfig, ShortcutConfigHandle};

/// Application-ID policy used for compositor-owned window operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowHostPolicy {
    /// Keep the normal Crostini/guest application namespace.
    #[default]
    Guest,
    /// Use the ARC task-compatible namespace required by direct bounds.
    Arc,
}

/// Lifetime and shell-identity behavior of the ARC compatibility ID.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WindowArcIdLifetime {
    /// Keep the ARC task-form ID on the Aura surface for the window lifetime.
    #[default]
    Persistent,
    /// Install ARC around one direct-bounds placement, then restore the native
    /// Guest OS identity after the host barrier. This path does not install a
    /// parent relationship.
    Transient,
    /// Restore the native shell ID after installation while retaining ARC
    /// policy properties for later bounds requests.
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
    /// Resize at the current origin, then issue the experimental self-parent
    /// position probe.
    SelfParent,
    /// Use the host's experimental zcr_remote_shell_v2 role.
    RemoteShell,
}

/// A valid placement policy for one process.
///
/// The representation is intentionally closed: callers cannot construct an
/// ARC lifetime that is incompatible with the selected geometry operation.
/// The hidden CLI axes are converted into this type once at startup, and all
/// connection-local state observes the same validated value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementMode {
    Disabled,
    /// Keep an ARC identity on Aura surfaces without consuming placement
    /// shortcuts. This preserves the legacy hidden `host-policy=arc` probe.
    IdentityOnly,
    Bounds {
        host_policy: WindowHostPolicy,
        arc_id_lifetime: WindowArcIdLifetime,
    },
    SelfParent {
        host_policy: WindowHostPolicy,
    },
    RemoteShell,
}

/// Immutable placement policy shared by every connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowPlacementMode {
    mode: PlacementMode,
}

impl Default for WindowPlacementMode {
    fn default() -> Self {
        Self::disabled()
    }
}

impl WindowPlacementMode {
    /// Build the persistent form of one host-policy/geometry pair.
    ///
    /// This constructor remains for compatibility with focused tests and
    /// legacy in-process callers. New startup code should use
    /// [`Self::from_axes`] so incompatible ARC lifetimes are rejected.
    pub(crate) const fn new(
        host_policy: WindowHostPolicy,
        geometry_method: WindowGeometryMethod,
    ) -> Self {
        let mode = match geometry_method {
            WindowGeometryMethod::None => {
                if matches!(host_policy, WindowHostPolicy::Arc) {
                    PlacementMode::IdentityOnly
                } else {
                    PlacementMode::Disabled
                }
            }
            WindowGeometryMethod::Bounds => PlacementMode::Bounds {
                host_policy,
                arc_id_lifetime: WindowArcIdLifetime::Persistent,
            },
            WindowGeometryMethod::SelfParent => PlacementMode::SelfParent { host_policy },
            WindowGeometryMethod::RemoteShell => PlacementMode::RemoteShell,
        };
        Self { mode }
    }

    /// Build a bounds backend using the ARC authorization policy.
    pub(crate) const fn arc_bounds(arc_id_lifetime: WindowArcIdLifetime) -> Self {
        Self {
            mode: PlacementMode::Bounds {
                host_policy: WindowHostPolicy::Arc,
                arc_id_lifetime,
            },
        }
    }

    /// Build the self-parent backend. Self-parent always keeps its identity
    /// persistent because changing it around the probe resets IME focus.
    pub(crate) const fn self_parent(host_policy: WindowHostPolicy) -> Self {
        Self {
            mode: PlacementMode::SelfParent { host_policy },
        }
    }

    pub(crate) const fn remote_shell() -> Self {
        Self {
            mode: PlacementMode::RemoteShell,
        }
    }

    /// Convert the three hidden startup axes into one valid policy.
    pub(crate) fn from_axes(
        host_policy: WindowHostPolicy,
        geometry_method: WindowGeometryMethod,
        arc_id_lifetime: WindowArcIdLifetime,
    ) -> Result<Self, &'static str> {
        if !matches!(host_policy, WindowHostPolicy::Arc)
            && !matches!(arc_id_lifetime, WindowArcIdLifetime::Persistent)
        {
            return Err("a non-persistent ARC lifetime requires --window-host-policy=arc");
        }
        if matches!(geometry_method, WindowGeometryMethod::SelfParent)
            && !matches!(arc_id_lifetime, WindowArcIdLifetime::Persistent)
        {
            return Err(
                "self-parent requires exactly the persistent ARC identity lifetime; \
                 persistent-native-shell restoration is not supported",
            );
        }
        if matches!(geometry_method, WindowGeometryMethod::None)
            && !matches!(arc_id_lifetime, WindowArcIdLifetime::Persistent)
        {
            return Err("a non-persistent ARC lifetime requires a geometry method");
        }
        if matches!(geometry_method, WindowGeometryMethod::RemoteShell)
            && !matches!(host_policy, WindowHostPolicy::Guest)
        {
            return Err("remote-shell geometry does not support ARC host policy");
        }

        Ok(match geometry_method {
            WindowGeometryMethod::None => Self::new(host_policy, geometry_method),
            WindowGeometryMethod::Bounds if matches!(host_policy, WindowHostPolicy::Arc) => {
                Self::arc_bounds(arc_id_lifetime)
            }
            WindowGeometryMethod::Bounds => Self::new(host_policy, geometry_method),
            WindowGeometryMethod::SelfParent => Self::self_parent(host_policy),
            WindowGeometryMethod::RemoteShell => Self::remote_shell(),
        })
    }

    pub(crate) const fn disabled() -> Self {
        Self::new(WindowHostPolicy::Guest, WindowGeometryMethod::None)
    }

    /// Resolve the legacy environment flags for compatibility callers.
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

    /// Resolve the legacy backend from the process environment once.
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

    pub(crate) const fn handles_shortcuts(self) -> bool {
        matches!(
            self.mode,
            PlacementMode::Bounds { .. }
                | PlacementMode::SelfParent { .. }
                | PlacementMode::RemoteShell
        )
    }

    /// Return whether the ARC application namespace is selected.
    pub(crate) const fn uses_arc_policy(self) -> bool {
        matches!(self.host_policy(), WindowHostPolicy::Arc)
    }

    pub(crate) const fn host_policy(self) -> WindowHostPolicy {
        match self.mode {
            PlacementMode::Disabled => WindowHostPolicy::Guest,
            PlacementMode::IdentityOnly => WindowHostPolicy::Arc,
            PlacementMode::Bounds { host_policy, .. }
            | PlacementMode::SelfParent { host_policy } => host_policy,
            PlacementMode::RemoteShell => WindowHostPolicy::Guest,
        }
    }

    pub(crate) const fn arc_id_lifetime(self) -> WindowArcIdLifetime {
        match self.mode {
            PlacementMode::Bounds {
                arc_id_lifetime, ..
            } => arc_id_lifetime,
            PlacementMode::Disabled
            | PlacementMode::IdentityOnly
            | PlacementMode::SelfParent { .. }
            | PlacementMode::RemoteShell => WindowArcIdLifetime::Persistent,
        }
    }

    pub(crate) const fn uses_transient_arc_id(self) -> bool {
        matches!(
            self.mode,
            PlacementMode::Bounds {
                arc_id_lifetime: WindowArcIdLifetime::Transient,
                ..
            }
        )
    }

    pub(crate) const fn uses_bounds(self) -> bool {
        matches!(self.mode, PlacementMode::Bounds { .. })
    }

    pub(crate) const fn uses_self_parent(self) -> bool {
        matches!(self.mode, PlacementMode::SelfParent { .. })
    }

    pub(crate) const fn uses_remote_shell(self) -> bool {
        matches!(self.mode, PlacementMode::RemoteShell)
    }
}

/// Prefix for numeric ARC task-form application IDs used by placement.
pub(crate) const ARC_TASK_APPLICATION_ID_PREFIX: &str = "org.chromium.arc.";

#[cfg(test)]
pub(crate) use crate::arc_task_ids::{ARC_TASK_ID_POOL_END, ARC_TASK_ID_POOL_START};

const DEFAULT_VM_IDENTIFIER: &str = "termina";

/// Resolve the VM namespace used in Guest OS application IDs.
pub(crate) fn resolve_vm_identifier(value: Option<String>) -> String {
    value
        .filter(|identifier| !identifier.is_empty())
        .unwrap_or_else(|| DEFAULT_VM_IDENTIFIER.to_string())
}

/// Process-wide immutable placement configuration and shared runtime handles.
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

    pub(crate) const fn mode(&self) -> WindowPlacementMode {
        self.mode
    }

    pub(crate) fn shortcut_config_snapshot(&self) -> Arc<ShortcutConfig> {
        self.shortcut_config.snapshot()
    }

    pub(crate) fn host_accelerators(&self) -> &[Accelerator] {
        self.host_accelerators.as_ref()
    }

    pub(crate) fn vm_identifier(&self) -> &str {
        &self.vm_identifier
    }

    pub(crate) fn allocate_arc_task_id(&self) -> std::io::Result<u32> {
        self.arc_task_allocator
            .as_ref()
            .ok_or_else(|| std::io::Error::other("ARC task allocator is disabled"))
            .and_then(|allocator| allocator.allocate())
    }

    /// Reload the optional shortcut file as one complete generation.
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

    #[cfg(test)]
    pub(crate) fn with_mode_for_test(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_vm_identifier_uses_the_crostini_default() {
        assert_eq!(resolve_vm_identifier(None), "termina");
        assert_eq!(resolve_vm_identifier(Some(String::new())), "termina");
    }

    #[test]
    fn non_empty_vm_identifier_is_preserved() {
        assert_eq!(
            resolve_vm_identifier(Some("custom-vm".to_string())),
            "custom-vm"
        );
    }

    #[test]
    fn legacy_flags_select_one_geometry_axis() {
        assert_eq!(
            WindowPlacementMode::from_flags(true, true),
            WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
        );
        assert_eq!(
            WindowPlacementMode::from_flags(false, true),
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::SelfParent)
        );
        assert!(!WindowPlacementMode::disabled().handles_shortcuts());
    }

    #[test]
    fn from_axes_rejects_incompatible_lifetimes() {
        assert!(WindowPlacementMode::from_axes(
            WindowHostPolicy::Guest,
            WindowGeometryMethod::Bounds,
            WindowArcIdLifetime::Transient,
        )
        .is_err());
        assert!(WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
            WindowArcIdLifetime::Transient,
        )
        .is_err());
        assert!(WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::SelfParent,
            WindowArcIdLifetime::PersistentNativeShell,
        )
        .is_err());
        assert!(WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::None,
            WindowArcIdLifetime::PersistentNativeShell,
        )
        .is_err());
        assert!(WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::RemoteShell,
            WindowArcIdLifetime::Persistent,
        )
        .is_err());
    }

    #[test]
    fn valid_axes_collapse_to_one_policy_value() {
        let persistent = WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::Bounds,
            WindowArcIdLifetime::Persistent,
        )
        .expect("persistent ARC bounds are valid");
        assert!(persistent.uses_arc_policy());
        assert!(persistent.uses_bounds());
        assert_eq!(
            persistent.arc_id_lifetime(),
            WindowArcIdLifetime::Persistent
        );

        let identity_only = WindowPlacementMode::from_axes(
            WindowHostPolicy::Arc,
            WindowGeometryMethod::None,
            WindowArcIdLifetime::Persistent,
        )
        .expect("ARC identity without geometry is valid");
        assert!(identity_only.uses_arc_policy());
        assert!(!identity_only.handles_shortcuts());
    }
}
