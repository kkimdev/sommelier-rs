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

    /// Select the application-ID policy used by window shortcuts.
    #[arg(long, value_enum, default_value_t = HostPolicyArg::Guest)]
    window_host_policy: HostPolicyArg,

    /// Select the geometry operation used by window shortcuts.
    #[arg(long, value_enum, default_value_t = GeometryMethodArg::None)]
    window_geometry_method: GeometryMethodArg,

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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let env = env_logger::Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env).init();

    let args = Args::parse();

    let local_compositor = args.local_compositor;
    let gpu_accel = args.gpu_accel;
    let xdg_decoration = args.xdg_decoration;
    let mut virtio_wl = args.virtio_wl;
    let host_policy = match args.window_host_policy {
        HostPolicyArg::Guest => WindowHostPolicy::Guest,
        HostPolicyArg::Arc => WindowHostPolicy::Arc,
    };
    let geometry_method = match args.window_geometry_method {
        GeometryMethodArg::None => WindowGeometryMethod::None,
        GeometryMethodArg::Bounds => WindowGeometryMethod::Bounds,
        GeometryMethodArg::SelfParent => WindowGeometryMethod::SelfParent,
    };
    let placement_mode = WindowPlacementMode::new(host_policy, geometry_method);
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
        },
    )
    .await;
}
mod test_xkb;
