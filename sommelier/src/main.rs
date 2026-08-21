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

use crate::state::{WindowGeometryMethod, WindowHostPolicy, WindowPlacementMode};
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

    /// Enable the experimental compositor-owned window-placement subsystem.
    ///
    /// Without this gate Sommelier preserves the upstream behavior and rejects
    /// every placement-specific option or config path.
    #[arg(long)]
    experimental_window_placement: bool,

    /// Internal experiment: select the application-ID policy used by window
    /// shortcuts. Prefer the explicit experimental gate and config contract.
    #[arg(long, value_enum, hide = true)]
    window_host_policy: Option<HostPolicyArg>,

    /// Internal experiment: select the geometry operation used by window
    /// shortcuts. Prefer the explicit experimental gate and config contract.
    #[arg(long, value_enum, hide = true)]
    window_geometry_method: Option<GeometryMethodArg>,

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

/// Resolve the startup placement policy without allowing an accidental
/// production opt-in.
///
/// The public configuration surface is intentionally two-stage: the explicit
/// experimental gate must be present, and the shortcut file is optional. Once
/// gated, an otherwise unspecified policy uses native Guest identity plus the
/// self-parent experiment; an explicitly supplied hidden axis remains honored
/// for focused host testing.
fn resolve_placement_mode(
    experimental_enabled: bool,
    config_path_supplied: bool,
    host_policy: Option<HostPolicyArg>,
    geometry_method: Option<GeometryMethodArg>,
) -> Result<WindowPlacementMode, String> {
    if !experimental_enabled {
        if config_path_supplied || host_policy.is_some() || geometry_method.is_some() {
            return Err("window placement is experimental; pass \
                 --experimental-window-placement before selecting a placement \
                 policy or config file"
                .to_string());
        }
        return Ok(WindowPlacementMode::disabled());
    }

    let host_policy_value = match host_policy.unwrap_or(HostPolicyArg::Guest) {
        HostPolicyArg::Guest => WindowHostPolicy::Guest,
        HostPolicyArg::Arc => WindowHostPolicy::Arc,
    };
    let geometry_method_value = match geometry_method {
        Some(GeometryMethodArg::None) => WindowGeometryMethod::None,
        Some(GeometryMethodArg::Bounds) => WindowGeometryMethod::Bounds,
        Some(GeometryMethodArg::SelfParent) => WindowGeometryMethod::SelfParent,
        None if host_policy.is_none() => WindowGeometryMethod::SelfParent,
        None => WindowGeometryMethod::None,
    };

    Ok(WindowPlacementMode::new(
        host_policy_value,
        geometry_method_value,
    ))
}

/// Resolve a CLI-supplied shortcut path without panicking when the process
/// current directory is unavailable.
///
/// Absolute paths are returned unchanged. Relative paths are interpreted
/// against the startup working directory, matching normal CLI behavior while
/// keeping filesystem failures on the ordinary startup-error path.
fn resolve_shortcut_config_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .map_err(|error| format!("unable to resolve relative window shortcut config path: {error}"))
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
        args.experimental_window_placement,
        args.window_shortcuts_config.is_some(),
        args.window_host_policy,
        args.window_geometry_method,
    ) {
        Ok(mode) => mode,
        Err(error) => {
            log::error!("{error}");
            std::process::exit(2);
        }
    };
    if args.experimental_window_placement
        && args.window_host_policy.is_none()
        && args.window_geometry_method.is_none()
        && args.window_shortcuts_config.is_none()
    {
        log::info!(
            "Experimental window placement is enabled with its default \
             native Guest/self-parent policy, but no shortcut config was supplied"
        );
    }
    if placement_mode.uses_self_parent() {
        log::warn!(
            "--window-geometry-method=self-parent is experimental and position-only; \
             it cannot resize windows and may be unstable on custom ChromeOS hosts"
        );
    }
    if placement_mode.uses_self_parent() && placement_mode.uses_arc_policy() {
        log::warn!(
            "--window-host-policy=arc with self-parent enables ARC-specific metadata \
             even though the self-parent method does not require it"
        );
    }

    let host_accelerators = match crate::accelerator::try_from_environment() {
        Ok(accelerators) => Arc::new(accelerators),
        Err(error) => {
            log::error!(
                "Invalid SOMMELIER_ACCELERATORS; refusing to start: {}",
                error
            );
            std::process::exit(2);
        }
    };
    let shortcut_config_path = match args.window_shortcuts_config {
        Some(path) => match resolve_shortcut_config_path(path) {
            Ok(path) => Some(path),
            Err(error) => {
                log::error!("{error}");
                std::process::exit(2);
            }
        },
        None => None,
    };
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
             the selected geometry method disables window placement"
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
    fn production_default_keeps_placement_disabled() {
        assert_eq!(
            resolve_placement_mode(false, false, None, None)
                .expect("production default should resolve"),
            WindowPlacementMode::disabled()
        );
    }

    #[test]
    fn placement_options_require_the_experimental_gate() {
        let error = resolve_placement_mode(
            false,
            true,
            Some(HostPolicyArg::Guest),
            Some(GeometryMethodArg::SelfParent),
        )
        .expect_err("placement options must be gated");
        assert!(error.contains("--experimental-window-placement"));
    }

    #[test]
    fn config_path_alone_requires_the_experimental_gate() {
        let error = resolve_placement_mode(false, true, None, None)
            .expect_err("a config path must not opt into placement implicitly");
        assert!(error.contains("--experimental-window-placement"));
    }

    #[test]
    fn gated_default_uses_native_guest_self_parent() {
        let mode =
            resolve_placement_mode(true, false, None, None).expect("gated default should resolve");
        assert_eq!(
            mode,
            WindowPlacementMode::new(WindowHostPolicy::Guest, WindowGeometryMethod::SelfParent)
        );
    }

    #[test]
    fn explicit_none_remains_disabled_after_gate() {
        assert_eq!(
            resolve_placement_mode(
                true,
                false,
                Some(HostPolicyArg::Guest),
                Some(GeometryMethodArg::None),
            )
            .expect("explicit disabled policy should resolve"),
            WindowPlacementMode::disabled()
        );
    }

    #[test]
    fn absolute_shortcut_config_path_is_preserved() {
        let path = PathBuf::from("/tmp/window-shortcuts.toml");
        assert_eq!(
            resolve_shortcut_config_path(path.clone()).expect("absolute path should resolve"),
            path
        );
    }

    #[test]
    fn relative_shortcut_config_path_uses_startup_working_directory() {
        let relative = PathBuf::from("window-shortcuts.toml");
        let expected = std::env::current_dir()
            .expect("test working directory should be available")
            .join(&relative);
        assert_eq!(
            resolve_shortcut_config_path(relative).expect("relative path should resolve"),
            expected
        );
    }
}
