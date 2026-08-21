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

#![deny(clippy::debug_assert_with_mut_call)]

use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::{
    WindowArcIdLifetime, WindowGeometryMethod, WindowHostPolicy, WindowPlacementMode,
};
use crate::window_shortcuts::{ShortcutConfig, ShortcutConfigHandle};

mod accelerator;
mod allocator;
mod arc_task_ids;
mod connection;
mod handler;
mod proxy;
mod state;
mod virtwl;
mod virtwl_channel;
mod window_shortcuts;
mod wire;

mod protocols {
    // Generated dispatch functions intentionally use independent validation
    // guards with early returns. Compact token-stream output can make Clippy
    // misread adjacent guards as a missing `else`.
    #![allow(clippy::possible_missing_else)]
    #![allow(unused_macros)]
    include!(concat!(env!("OUT_DIR"), "/wayland_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/xdg_shell_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/linux_dmabuf_v1_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/viewporter_protocol.rs"));
    include!(concat!(
        env!("OUT_DIR"),
        "/text-input-unstable-v3_protocol.rs"
    ));
    include!(concat!(
        env!("OUT_DIR"),
        "/text-input-unstable-v1_protocol.rs"
    ));
    include!(concat!(
        env!("OUT_DIR"),
        "/text-input-extension-unstable-v1_protocol.rs"
    ));
    include!(concat!(
        env!("OUT_DIR"),
        "/xdg_decoration_unstable_v1_protocol.rs"
    ));
    include!(concat!(env!("OUT_DIR"), "/fractional_scale_v1_protocol.rs"));
    include!(concat!(
        env!("OUT_DIR"),
        "/keyboard_extension_unstable_v1_protocol.rs"
    ));
    include!(concat!(env!("OUT_DIR"), "/gtk_shell_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/aura_shell_protocol.rs"));
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Connect to a local compositor at PATH, used for debug only
    #[arg(long)]
    local_compositor: Option<String>,

    /// Enable host GPU buffer allocation through VirtWL linux-dmabuf.
    ///
    /// Guest wl_shm buffers are copied into host-backed PRIME buffers. If the
    /// kernel does not expose VirtWL dma-buf allocation, the proxy falls back
    /// to its validated shared-memory path.
    #[arg(long, default_value_t = false)]
    gpu_accel: bool,

    /// Enable XDG Decoration support
    #[arg(long)]
    xdg_decoration: bool,

    /// Use virtio-wayland channel at PATH (defaults to /dev/wl0 if --local-compositor is not specified)
    #[arg(long)]
    virtio_wl: Option<String>,

    /// Select one complete window-placement backend.
    ///
    /// This is the convenient switch for runtime experiments:
    /// `set-parent` keeps the custom self-parent position probe but uses the
    /// persistent ARC task identity needed for the accompanying bounds
    /// request, `transient-arc`
    /// installs an ARC task identity around each bounds request, and
    /// `persistent` keeps the ARC task identity for the window lifetime. Do
    /// not combine this option with the lower-level placement axis options
    /// below.
    #[arg(long, value_enum)]
    window_placement_backend: Option<PlacementBackendArg>,

    /// Select the application-ID policy used by window shortcuts.
    ///
    /// This is a lower-level option. Prefer `--window-placement-backend` when
    /// selecting one of the supported combinations.
    #[arg(long, value_enum)]
    window_host_policy: Option<HostPolicyArg>,

    /// Select the geometry operation used by window shortcuts.
    ///
    /// This is a lower-level option. The default is `none`, which leaves
    /// placement shortcuts disabled.
    #[arg(long, value_enum)]
    window_geometry_method: Option<GeometryMethodArg>,

    /// Select how the ARC compatibility ID is kept on Aura windows.
    ///
    /// This is a lower-level option. `persistent` is the default when the
    /// three lower-level axes are used directly.
    #[arg(long, value_enum)]
    window_arc_id_lifetime: Option<ArcIdLifetimeArg>,

    /// Read window shortcut bindings from PATH. No config is read by default.
    #[arg(long, value_name = "PATH")]
    window_shortcuts_config: Option<PathBuf>,

    /// The display name (e.g. wayland-proxy-0)
    #[arg(default_value = "wayland-proxy-0")]
    display: String,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HostPolicyArg {
    Guest,
    Arc,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum GeometryMethodArg {
    None,
    Bounds,
    SelfParent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum PlacementBackendArg {
    /// Persistent ARC task identity plus the experimental set_parent-and-bounds path.
    SetParent,
    /// ARC task identity only while a direct Aura bounds request is queued.
    TransientArc,
    /// ARC task identity retained for the full window lifetime.
    Persistent,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ArcIdLifetimeArg {
    Persistent,
    Transient,
    PersistentNativeShell,
}

impl PlacementBackendArg {
    fn mode(self) -> WindowPlacementMode {
        match self {
            Self::SetParent => {
                // set_parent only supplies a position. The companion
                // set_window_bounds request is still subject to ChromeOS's
                // CanSetBounds policy, which accepts the ARC task-form Aura
                // identity proven by PR #2. Keep the ID persistent here:
                // transiently changing it around a shortcut causes an
                // enter/leave cycle that resets IME focus on the custom host.
                WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::SelfParent)
            }
            Self::TransientArc => {
                WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
                    .with_arc_id_lifetime(WindowArcIdLifetime::Transient)
            }
            Self::Persistent => {
                WindowPlacementMode::new(WindowHostPolicy::Arc, WindowGeometryMethod::Bounds)
            }
        }
    }
}

fn resolve_placement_mode(
    backend: Option<PlacementBackendArg>,
    host_policy: Option<HostPolicyArg>,
    geometry_method: Option<GeometryMethodArg>,
    arc_id_lifetime: Option<ArcIdLifetimeArg>,
) -> Result<WindowPlacementMode, String> {
    if backend.is_some()
        && (host_policy.is_some() || geometry_method.is_some() || arc_id_lifetime.is_some())
    {
        return Err("--window-placement-backend cannot be combined with \
             --window-host-policy, --window-geometry-method, or \
             --window-arc-id-lifetime"
            .to_string());
    }

    if let Some(backend) = backend {
        return Ok(backend.mode());
    }

    let host_policy = match host_policy.unwrap_or(HostPolicyArg::Guest) {
        HostPolicyArg::Guest => WindowHostPolicy::Guest,
        HostPolicyArg::Arc => WindowHostPolicy::Arc,
    };
    let geometry_method = match geometry_method.unwrap_or(GeometryMethodArg::None) {
        GeometryMethodArg::None => WindowGeometryMethod::None,
        GeometryMethodArg::Bounds => WindowGeometryMethod::Bounds,
        GeometryMethodArg::SelfParent => WindowGeometryMethod::SelfParent,
    };
    let arc_id_lifetime = match arc_id_lifetime.unwrap_or(ArcIdLifetimeArg::Persistent) {
        ArcIdLifetimeArg::Persistent => WindowArcIdLifetime::Persistent,
        ArcIdLifetimeArg::Transient => WindowArcIdLifetime::Transient,
        ArcIdLifetimeArg::PersistentNativeShell => WindowArcIdLifetime::PersistentNativeShell,
    };

    if !matches!(host_policy, WindowHostPolicy::Arc)
        && !matches!(arc_id_lifetime, WindowArcIdLifetime::Persistent)
    {
        return Err(format!(
            "--window-arc-id-lifetime={arc_id_lifetime:?} requires \
             --window-host-policy=arc",
        ));
    }

    Ok(
        WindowPlacementMode::new(host_policy, geometry_method)
            .with_arc_id_lifetime(arc_id_lifetime),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let env = env_logger::Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env).init();

    let args = Args::parse();

    let local_compositor = args.local_compositor;
    let gpu_accel = args.gpu_accel;
    let xdg_decoration = args.xdg_decoration;
    let mut virtio_wl = args.virtio_wl;
    let placement_mode = match resolve_placement_mode(
        args.window_placement_backend,
        args.window_host_policy,
        args.window_geometry_method,
        args.window_arc_id_lifetime,
    ) {
        Ok(mode) => mode,
        Err(error) => {
            log::error!("{error}");
            std::process::exit(2);
        }
    };
    if let Some(backend) = args.window_placement_backend {
        log::info!("Using window placement backend: {backend:?}");
    }
    if placement_mode.uses_self_parent() {
        log::warn!(
            "--window-geometry-method=self-parent is experimental; it sends \
             bounds at the current origin followed by set_parent and may be \
             unstable on custom ChromeOS hosts"
        );
        if !placement_mode.uses_arc_policy() {
            log::warn!(
                "self-parent with Guest OS identity is position-only on this host: \
                 set_window_bounds may be rejected; use \
                 --window-placement-backend=set-parent or \
                 --window-host-policy=arc for resize"
            );
        }
    }
    if placement_mode.uses_self_parent() && placement_mode.uses_arc_policy() {
        log::warn!(
            "--window-host-policy=arc with self-parent enables the ARC task-form \
             bounds authorization required for resize; the self-parent probe \
             still remains experimental"
        );
    }

    let host_accelerators = Arc::new(crate::accelerator::from_environment());
    let shortcut_config_path = args.window_shortcuts_config.map(|path| {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .expect("current working directory is required for a relative config path")
                .join(path)
        }
    });
    let shortcut_config = match shortcut_config_path.as_deref() {
        Some(path) => match ShortcutConfig::load_from_path(path, host_accelerators.as_ref()) {
            Ok(config) => config,
            Err(error) => {
                log::error!("Unable to load window shortcut config: {}", error);
                std::process::exit(2);
            }
        },
        None => ShortcutConfig::disabled(),
    };
    if !shortcut_config.is_empty() && !placement_mode.handles_shortcuts() {
        log::error!(
            "window shortcut config contains bindings, but \
             --window-geometry-method=none disables window placement"
        );
        std::process::exit(2);
    }
    let shortcut_config_handle = ShortcutConfigHandle::new(shortcut_config);

    let arc_task_allocator = if placement_mode.uses_arc_policy() {
        match arc_task_ids::ArcTaskIdAllocator::acquire() {
            Ok(allocator) => Some(allocator),
            Err(error) => {
                log::error!("Unable to reserve an ARC task ID block: {}", error);
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    if local_compositor.is_none() && virtio_wl.is_none() {
        virtio_wl = Some("/dev/wl0".to_string());
    }

    // Need XDG_RUNTIME_DIR
    let xdg_runtime = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");
    let socket_path = format!("{}/{}", xdg_runtime, args.display);

    // Clean up old socket
    let _ = std::fs::remove_file(&socket_path);

    proxy::run(
        &socket_path,
        local_compositor,
        gpu_accel,
        xdg_decoration,
        virtio_wl,
        proxy::ProxyRuntimeConfig {
            placement_mode,
            shortcut_config: shortcut_config_handle,
            shortcut_config_path,
            host_accelerators,
            arc_task_allocator,
        },
    )
    .await;
}
mod test_xkb;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_parent_backend_keeps_arc_authorization_for_bounds() {
        let mode = PlacementBackendArg::SetParent.mode();

        assert_eq!(mode.host_policy, WindowHostPolicy::Arc);
        assert_eq!(mode.geometry_method, WindowGeometryMethod::SelfParent);
        assert_eq!(mode.arc_id_lifetime, WindowArcIdLifetime::Persistent);
        assert!(mode.uses_arc_policy());
        assert!(mode.uses_self_parent());
        assert!(mode.handles_shortcuts());
    }

    #[test]
    fn guest_self_parent_is_explicitly_position_only() {
        let mode =
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::SelfParent);

        assert!(!mode.uses_arc_policy());
        assert!(mode.uses_self_parent());
        assert!(mode.handles_shortcuts());
    }

    #[test]
    fn placement_backend_rejects_mixed_axis_overrides() {
        let error = resolve_placement_mode(
            Some(PlacementBackendArg::SetParent),
            Some(HostPolicyArg::Arc),
            None,
            None,
        )
        .expect_err("a named backend must not be partially overridden");

        assert!(error.contains("--window-placement-backend"));
        assert!(error.contains("--window-host-policy"));
    }
}
