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

use std::sync::Arc;

use clap::Parser;

mod accelerator;
mod allocator;
mod arc_task_ids;
mod connection;
mod handler;
mod placement_config;
mod proxy;
mod state;
#[cfg(test)]
mod test_support;
mod virtwl;
mod virtwl_channel;
mod window_layout;
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
    include!(concat!(env!("OUT_DIR"), "/protocol_routing.rs"));
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

    /// Read window placement shortcut bindings from a versioned TOML file.
    ///
    /// Used only while the ARC bounds policy is active
    /// (SOMMELIER_WINDOW_BOUNDS_AS_ARC), and mutually exclusive with
    /// SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS. An invalid file is a startup
    /// error (exit 2); SIGHUP reloads it and keeps the previous bindings if
    /// the new file is invalid. See docs/WINDOW_SHORTCUT_CONFIGURATION.md.
    #[arg(long, value_name = "PATH")]
    window_shortcuts_config: Option<std::path::PathBuf>,

    /// The display name (e.g. wayland-proxy-0)
    #[arg(default_value = "wayland-proxy-0")]
    display: String,
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
    let shortcuts_file = args.window_shortcuts_config.as_deref();
    let placement = match placement_config::PlacementConfig::from_environment(shortcuts_file) {
        Ok(config) => Arc::new(config),
        Err(error) => {
            log::error!("{}", error);
            std::process::exit(2);
        }
    };
    let reload_signal = shortcuts_file.map(|_| install_reload_signal());

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
        placement,
        reload_signal,
    )
    .await;
}

/// Install the `SIGHUP` handler that reloads the shortcut configuration file.
///
/// Once installed, `SIGHUP` no longer terminates the process. Tokio delivers
/// the signal through its own self-pipe; the proxy's accept loop runs the
/// reload. Failure to install is fatal (exit 1) because the documented
/// reload contract could not be honored.
fn install_reload_signal() -> tokio::signal::unix::Signal {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
        Ok(signal) => signal,
        Err(error) => {
            log::error!(
                "Failed to install the SIGHUP shortcut reload handler: {}",
                error
            );
            std::process::exit(1);
        }
    }
}
mod test_xkb;
