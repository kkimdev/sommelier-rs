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

//! The process's settings: what the command line ([`Cli`]) and the
//! environment ask for, resolved once, before the first client connects.
//!
//! [`Settings`] is immutable; every connection shares it (`Rc`). `main`
//! is the only code that reads the command line or the environment, and
//! hands both to [`Settings::resolve`], which takes the environment as a
//! lookup so that a test passes its own. This module is the one list of
//! the variables the proxy reads. A value that cannot be used is logged
//! once and replaced by its default: the proxy serves every app in the
//! container, so a setting must not stop it. Only a missing
//! `XDG_RUNTIME_DIR`, without which there is no socket to serve, does.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Parser;
use thiserror::Error;

use crate::accelerator::HostAccelerators;
use crate::client_kind::WheelFromTouchpad;

/// Chords the host compositor keeps (see [`HostAccelerators`]).
const ACCELERATORS: &str = "SOMMELIER_ACCELERATORS";
/// The virtio-gpu render node to allocate from.
const DRM_DEVICE: &str = "SOMMELIER_DRM_DEVICE";
/// The VM's name in ChromeOS application IDs.
const VM_IDENTIFIER: &str = "SOMMELIER_VM_IDENTIFIER";
/// Which clients get wheel steps from touchpad scrolling.
const WHEEL_FROM_TOUCHPAD: &str = "SOMMELIER_WHEEL_FROM_TOUCHPAD";
/// The directory to record each connection's traffic in.
const TRACE_DIR: &str = "SOMMELIER_TRACE_DIR";

/// The VirtWL device the proxy uses when the command line names no host.
const DEFAULT_VIRTWL_DEVICE: &str = "/dev/wl0";
/// The VM name of ChromeOS application IDs when none is set, as in
/// ChromiumOS Sommelier.
const DEFAULT_VM_IDENTIFIER: &str = "termina";

/// The command line.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub(crate) struct Cli {
    /// Connect to a local compositor at PATH, used for debug only
    #[arg(long, conflicts_with = "virtio_wl")]
    local_compositor: Option<PathBuf>,

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

    /// Use the virtio-wayland channel at PATH (default: /dev/wl0)
    #[arg(long)]
    virtio_wl: Option<PathBuf>,

    /// The display name (e.g. wayland-proxy-0)
    #[arg(default_value = "wayland-proxy-0")]
    display: String,
}

/// How the proxy reaches the host compositor, once per client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostTransport {
    /// A VirtWL context on the virtio-wayland device at this path: Exo,
    /// from a Crostini container (`--virtio-wl`, by default `/dev/wl0`).
    VirtWl(PathBuf),
    /// The compositor socket at this path, on this machine, for debugging
    /// (`--local-compositor`).
    Local(PathBuf),
}

/// Why the settings cannot be resolved.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum SettingsError {
    #[error("XDG_RUNTIME_DIR, which holds the Wayland socket, is not set or empty")]
    NoRuntimeDir,
}

/// Everything the process was asked for, resolved once by `main`.
#[derive(Debug)]
pub(crate) struct Settings {
    /// The Wayland socket the proxy serves: `$XDG_RUNTIME_DIR/<display>`,
    /// or the display name itself when it is an absolute path, as
    /// libwayland clients read an absolute `WAYLAND_DISPLAY`.
    pub(crate) socket: PathBuf,
    pub(crate) host: HostTransport,
    /// `--gpu-accel`: copy SHM buffers into host dma-bufs.
    pub(crate) gpu_accel: bool,
    /// `--xdg-decoration`: offer the guest `zxdg_decoration_manager_v1`.
    pub(crate) xdg_decoration: bool,
    /// The VM's name in ChromeOS application IDs
    /// (`org.chromium.guest_os.<vm_id>.wayland.<app-id>`).
    pub(crate) vm_id: String,
    /// The render node `SOMMELIER_DRM_DEVICE` names; `None` takes the
    /// first virtio-gpu render node (see `crate::allocator::VirtGpuNode`).
    pub(crate) drm_device: Option<PathBuf>,
    /// Which clients get wheel steps from touchpad scrolling.
    pub(crate) wheel_from_touchpad: WheelFromTouchpad,
    pub(crate) host_accelerators: HostAccelerators,
    /// The window placement bindings file
    /// (`crate::placement_config::config_path`); `None` when neither
    /// `XDG_CONFIG_HOME` nor `HOME` names an absolute directory, which
    /// leaves placement off for good.
    pub(crate) placement_bindings: Option<PathBuf>,
    /// The directory `SOMMELIER_TRACE_DIR` names, where each connection's
    /// traffic, typed text included, is recorded (`crate::trace`); `None`,
    /// the default, records nothing.
    pub(crate) trace_dir: Option<PathBuf>,
}

impl Settings {
    /// Resolve the settings from the command line and the environment,
    /// which `var` reads.
    ///
    /// # Errors
    ///
    /// [`SettingsError::NoRuntimeDir`] when `XDG_RUNTIME_DIR` is unset or
    /// empty: an empty one would put the socket in the startup directory.
    /// Every other problem is logged and leaves that setting at its
    /// default.
    pub(crate) fn resolve(
        cli: Cli,
        var: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, SettingsError> {
        let runtime_dir = var("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .ok_or(SettingsError::NoRuntimeDir)?;
        let host = match (cli.virtio_wl, cli.local_compositor) {
            (Some(device), _) => HostTransport::VirtWl(device),
            (None, Some(socket)) => HostTransport::Local(socket),
            (None, None) => HostTransport::VirtWl(DEFAULT_VIRTWL_DEVICE.into()),
        };
        let placement_bindings = crate::placement_config::config_path(
            var("XDG_CONFIG_HOME").as_deref(),
            var("HOME").as_deref(),
        );
        if placement_bindings.is_none() {
            log::debug!(
                "Neither XDG_CONFIG_HOME nor HOME names an absolute directory, so there is no \
                 window placement configuration"
            );
        }
        Ok(Self {
            socket: PathBuf::from(runtime_dir).join(cli.display),
            host,
            gpu_accel: cli.gpu_accel,
            xdg_decoration: cli.xdg_decoration,
            vm_id: vm_id(var(VM_IDENTIFIER)),
            drm_device: var(DRM_DEVICE)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from),
            wheel_from_touchpad: wheel_from_touchpad(var(WHEEL_FROM_TOUCHPAD)),
            host_accelerators: host_accelerators(var(ACCELERATORS)),
            placement_bindings,
            trace_dir: var(TRACE_DIR)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        })
    }

    /// The settings of a test: no host, no feature switch, no bindings
    /// file, and every variable at its default.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            socket: PathBuf::from("/nonexistent/wayland-test"),
            host: HostTransport::Local(PathBuf::from("/nonexistent/host")),
            gpu_accel: false,
            xdg_decoration: false,
            vm_id: DEFAULT_VM_IDENTIFIER.to_string(),
            drm_device: None,
            wheel_from_touchpad: WheelFromTouchpad::Off,
            host_accelerators: HostAccelerators::default(),
            placement_bindings: None,
            trace_dir: None,
        }
    }
}

/// `SOMMELIER_VM_IDENTIFIER`: unset, empty or not UTF-8 is the default,
/// as ChromiumOS Sommelier's `strlen(vm_id) != 0` fallback keeps
/// application IDs valid; not UTF-8 is logged.
fn vm_id(value: Option<OsString>) -> String {
    value
        .and_then(|value| {
            value
                .into_string()
                .inspect_err(|value| {
                    log::warn!("Ignoring non-UTF-8 {VM_IDENTIFIER}={value:?}; using the default");
                })
                .ok()
        })
        .filter(|identifier| !identifier.is_empty())
        .unwrap_or_else(|| DEFAULT_VM_IDENTIFIER.to_string())
}

/// `SOMMELIER_WHEEL_FROM_TOUCHPAD`: unset is `auto`, and so is a value
/// [`WheelFromTouchpad::parse`] rejects, which is logged.
fn wheel_from_touchpad(value: Option<OsString>) -> WheelFromTouchpad {
    let Some(value) = value else {
        return WheelFromTouchpad::Auto;
    };
    value
        .to_str()
        .and_then(WheelFromTouchpad::parse)
        .unwrap_or_else(|| {
            log::warn!(
                "Ignoring {WHEEL_FROM_TOUCHPAD}={value:?}, which is not auto, off or all; \
                 using auto"
            );
            WheelFromTouchpad::Auto
        })
}

/// `SOMMELIER_ACCELERATORS`: unset or blank is no chord. A malformed list
/// must not stop the proxy: it degrades to no filtering, logged once, and
/// so binds no chord out of reach of a placement binding either.
fn host_accelerators(value: Option<OsString>) -> HostAccelerators {
    let Some(value) = value else {
        return HostAccelerators::default();
    };
    value
        .to_str()
        .ok_or_else(|| "it is not UTF-8".to_string())
        .and_then(|text| {
            HostAccelerators::parse(text).map_err(|error| format!("{text:?}: {error}"))
        })
        .unwrap_or_else(|error| {
            log::warn!("Invalid {ACCELERATORS}, {error}. Accelerator filtering is disabled.");
            HostAccelerators::default()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accelerator::parse_accelerator;
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStringExt;

    /// Resolve `args` (after the program name) in the environment `env`.
    fn resolve(args: &[&str], env: &[(&str, &str)]) -> Result<Settings, SettingsError> {
        let env: BTreeMap<String, OsString> = env
            .iter()
            .map(|&(name, value)| (name.to_string(), value.into()))
            .collect();
        let cli = Cli::try_parse_from(std::iter::once("sommelier").chain(args.iter().copied()))
            .expect("a valid command line");
        Settings::resolve(cli, |name| env.get(name).cloned())
    }

    const RUNTIME: (&str, &str) = ("XDG_RUNTIME_DIR", "/run/user/1000");

    #[test]
    fn defaults_serve_wayland_proxy_0_over_dev_wl0() {
        let settings = resolve(&[], &[RUNTIME]).unwrap();
        assert_eq!(
            settings.socket,
            PathBuf::from("/run/user/1000/wayland-proxy-0")
        );
        assert_eq!(settings.host, HostTransport::VirtWl("/dev/wl0".into()));
        assert!(!settings.gpu_accel && !settings.xdg_decoration);
        assert_eq!(settings.vm_id, "termina");
        assert_eq!(settings.drm_device, None);
        assert_eq!(settings.wheel_from_touchpad, WheelFromTouchpad::Auto);
        assert_eq!(settings.host_accelerators, HostAccelerators::default());
        assert_eq!(settings.placement_bindings, None, "no HOME");
        assert_eq!(settings.trace_dir, None, "nothing is recorded");
        assert_eq!(resolve(&[], &[]).unwrap_err(), SettingsError::NoRuntimeDir);
        assert_eq!(
            resolve(&[], &[("XDG_RUNTIME_DIR", "")]).unwrap_err(),
            SettingsError::NoRuntimeDir
        );
        assert_eq!(
            resolve(&["/tmp/wayland-test"], &[RUNTIME]).unwrap().socket,
            PathBuf::from("/tmp/wayland-test"),
            "an absolute display name is the socket's path"
        );
    }

    /// The host is a VirtWL device or a local compositor, never both:
    /// the command line rejects the pair instead of falling back from one
    /// to the other.
    #[test]
    fn the_host_is_one_transport() {
        let host = |args: &[&str]| resolve(args, &[RUNTIME]).unwrap().host;
        assert_eq!(
            host(&["--virtio-wl", "/dev/wl1"]),
            HostTransport::VirtWl("/dev/wl1".into())
        );
        assert_eq!(
            host(&["--local-compositor", "/run/weston"]),
            HostTransport::Local("/run/weston".into())
        );
        let both = Cli::try_parse_from([
            "sommelier",
            "--virtio-wl",
            "/dev/wl0",
            "--local-compositor",
            "/run/weston",
        ]);
        assert_eq!(
            both.unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn switches_display_and_variables_reach_the_settings() {
        let settings = resolve(
            &["--gpu-accel", "--xdg-decoration", "wayland-1"],
            &[
                RUNTIME,
                ("SOMMELIER_ACCELERATORS", "<Alt>a"),
                ("SOMMELIER_DRM_DEVICE", "/dev/dri/renderD129"),
                ("SOMMELIER_VM_IDENTIFIER", "penguin"),
                ("SOMMELIER_WHEEL_FROM_TOUCHPAD", " ALL "),
                ("SOMMELIER_TRACE_DIR", "/tmp/traces"),
                ("HOME", "/home/u"),
            ],
        )
        .unwrap();
        assert_eq!(settings.socket, PathBuf::from("/run/user/1000/wayland-1"));
        assert!(settings.gpu_accel && settings.xdg_decoration);
        assert_eq!(
            settings.host_accelerators,
            vec![parse_accelerator("<Alt>a").unwrap()].into()
        );
        assert_eq!(
            settings.drm_device,
            Some(PathBuf::from("/dev/dri/renderD129"))
        );
        assert_eq!(settings.vm_id, "penguin");
        assert_eq!(settings.wheel_from_touchpad, WheelFromTouchpad::All);
        assert_eq!(settings.trace_dir, Some(PathBuf::from("/tmp/traces")));
        assert_eq!(
            settings.placement_bindings,
            Some(PathBuf::from(
                "/home/u/.config/sommelier-rs/window-placement.toml"
            ))
        );
    }

    /// A value that cannot be used means the default: an empty or non-UTF-8
    /// VM name, an empty render node or trace directory, an unknown wheel
    /// choice, and a malformed or non-UTF-8 accelerator list.
    #[test]
    fn unusable_values_fall_back_to_their_defaults() {
        let not_utf8 = || Some(OsString::from_vec(b"\xff".to_vec()));
        assert_eq!(vm_id(None), "termina");
        assert_eq!(vm_id(Some("".into())), "termina");
        assert_eq!(vm_id(not_utf8()), "termina");
        assert_eq!(vm_id(Some("penguin".into())), "penguin");

        let empty = [("SOMMELIER_DRM_DEVICE", ""), ("SOMMELIER_TRACE_DIR", "")];
        let settings = resolve(&[], &[RUNTIME, empty[0], empty[1]]).unwrap();
        assert_eq!(settings.drm_device, None);
        assert_eq!(settings.trace_dir, None);

        assert_eq!(wheel_from_touchpad(None), WheelFromTouchpad::Auto);
        assert_eq!(
            wheel_from_touchpad(Some("yes".into())),
            WheelFromTouchpad::Auto
        );
        assert_eq!(wheel_from_touchpad(not_utf8()), WheelFromTouchpad::Auto);
        assert_eq!(
            wheel_from_touchpad(Some("off".into())),
            WheelFromTouchpad::Off
        );

        let none = HostAccelerators::default();
        assert_eq!(host_accelerators(None), none);
        assert_eq!(host_accelerators(Some("  ".into())), none);
        assert_eq!(host_accelerators(Some("<Hyper>a".into())), none);
        assert_eq!(host_accelerators(not_utf8()), none);
    }

    /// `main` is the only code that reads the command line or the
    /// environment, through [`Settings::resolve`], and this module names
    /// every variable the proxy reads: no other production source reads
    /// one or names one, so a new setting cannot bypass it. Test-only
    /// sources, and every file's test module, read variables of their own
    /// (`SOMMELIER_DST_*`, `SOMMELIER_FUZZ_*`, `SOMMELIER_TEST_*`).
    #[test]
    fn only_main_reads_the_environment() {
        let src = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        for path in source_files(src) {
            let relative = path.strip_prefix(src).unwrap().to_str().unwrap();
            let test_only = ["sim/", "test_support", "proxy/fuzz", "proxy/harness"]
                .iter()
                .any(|prefix| relative.starts_with(prefix));
            if test_only || relative == "settings.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read a source file");
            let production = source.split("\nmod tests {").next().unwrap();
            if relative != "main.rs" {
                for read in ["env::var", "env::args"] {
                    assert!(
                        !production.contains(read),
                        "{relative} reads the environment ({read}); resolve it in settings.rs"
                    );
                }
            }
            let named: Vec<_> = production
                .split("\"SOMMELIER_")
                .skip(1)
                .filter_map(|rest| rest.split_once('"').map(|(name, _)| name))
                .filter(|name| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
                })
                .collect();
            assert!(
                named.is_empty(),
                "{relative} names SOMMELIER_{named:?}; settings.rs owns the variables"
            );
        }
    }

    /// Every `.rs` file under `dir`.
    fn source_files(dir: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .expect("read a source directory")
            .map(|entry| entry.expect("a source directory entry").path())
            .flat_map(|path| {
                if path.is_dir() {
                    source_files(&path)
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    vec![path]
                } else {
                    Vec::new()
                }
            })
            .collect()
    }
}
