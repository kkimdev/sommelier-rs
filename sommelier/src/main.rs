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

use clap::Parser;

mod accelerator;
mod allocator;
mod client_kind;
mod connection;
mod handler;
mod ime;
mod placement_config;
mod proxy;
mod settings;
#[cfg(test)]
mod sim;
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
    // The generated encoders and forwarding are where messages are built.
    #![allow(clippy::disallowed_methods)]
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
        "/xdg_output_unstable_v1_protocol.rs"
    ));
    include!(concat!(
        env!("OUT_DIR"),
        "/keyboard_extension_unstable_v1_protocol.rs"
    ));
    include!(concat!(env!("OUT_DIR"), "/gtk_shell_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/aura_shell_protocol.rs"));
    include!(concat!(env!("OUT_DIR"), "/protocol_routing.rs"));
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let env = env_logger::Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env).init();

    // The only reads of the command line and the environment.
    let settings =
        settings::Settings::resolve(settings::Cli::parse(), |name| std::env::var_os(name))
            .unwrap_or_else(|error| {
                log::error!("{error}");
                std::process::exit(1);
            });
    raise_open_file_limit();
    proxy::run(settings).await;
}

/// Raise the soft open-file limit to the hard limit.
///
/// Every client holds the proxy's descriptors for its two connections, its
/// buffers and its pending SCM_RIGHTS (up to `MAX_PENDING_FDS` each), all in
/// one process. The usual soft limit of 1024 would let a few busy clients
/// exhaust the table for everyone, while the hard limit is the bound the
/// system intends. The proxy waits on descriptors with epoll, never
/// `select`, so descriptors above 1024 are safe. A failure is logged and
/// not fatal: the proxy runs with the limit it has.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        log::warn!(
            "Cannot read the open-file limit: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    if limit.rlim_cur >= limit.rlim_max {
        return;
    }
    let soft = limit.rlim_cur;
    limit.rlim_cur = limit.rlim_max;
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        log::warn!(
            "Keeping the open-file limit at {soft}; raising it to {} failed: {}",
            limit.rlim_max,
            std::io::Error::last_os_error()
        );
        return;
    }
    log::info!(
        "Raised the open-file limit from {soft} to {}",
        limit.rlim_max
    );
}
