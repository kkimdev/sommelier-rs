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

//! Which clients get wheel steps synthesized from touchpad scrolling
//! (`handler::pointer::WheelSteps`), decided once per connection.
//!
//! Wine's Wayland driver turns only `wl_pointer.axis_discrete` into
//! `WM_MOUSEWHEEL`; its `axis` handler is empty, and Exo sends a touchpad
//! scroll as `axis` alone. `SOMMELIER_WHEEL_FROM_TOUCHPAD` chooses the
//! clients: `auto` (the default) the ones this module recognises as Wine,
//! `all` every client, `off` none. This module is the one owner of "is
//! this client Wine".

use std::ffi::OsStr;
use std::path::Path;

/// The setting's variable.
const VARIABLE: &str = "SOMMELIER_WHEEL_FROM_TOUCHPAD";

/// The executables a Wine process runs as, by file name.
const WINE_EXECUTABLES: [&str; 4] = ["wine-preloader", "wine64-preloader", "wine", "wine64"];

/// Environment entries only Wine's loader sets in its processes.
const WINE_ENVIRONMENT_MARKERS: [&[u8]; 2] = [b"WINESERVERSOCKET=", b"WINELOADERNOEXEC="];

/// `SOMMELIER_WHEEL_FROM_TOUCHPAD`: which clients get wheel steps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WheelFromTouchpad {
    /// Wine clients.
    #[default]
    Auto,
    /// No client.
    Off,
    /// Every client.
    All,
}

impl WheelFromTouchpad {
    /// Read the setting; `main` calls this once. An unusable value is
    /// logged and means `auto`.
    pub(crate) fn from_environment() -> Self {
        Self::parse(std::env::var_os(VARIABLE).as_deref())
    }

    /// Parse the setting's value, case-insensitively and ignoring
    /// surrounding whitespace; unset or blank is `auto`.
    fn parse(value: Option<&OsStr>) -> Self {
        let Some(value) = value else {
            return Self::Auto;
        };
        match value
            .to_str()
            .map(|value| value.trim().to_ascii_lowercase())
        {
            Some(value) if value.is_empty() || value == "auto" => Self::Auto,
            Some(value) if value == "off" => Self::Off,
            Some(value) if value == "all" => Self::All,
            _ => {
                log::warn!(
                    "Ignoring {VARIABLE}={value:?}, which is not auto, off or all; using auto"
                );
                Self::Auto
            }
        }
    }

    /// Return whether the client whose process is `pid` (`None` when the
    /// socket did not say) gets wheel steps.
    pub(crate) fn applies_to(self, pid: Option<i32>) -> bool {
        match self {
            Self::Off => false,
            Self::All => true,
            Self::Auto => pid.is_some_and(is_wine_process),
        }
    }
}

/// Return whether process `pid` is Wine, from what `/proc` shows of it.
/// Pid 0, which a peer in another PID namespace reports, and a process
/// `/proc` does not show are not.
fn is_wine_process(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let proc = Path::new("/proc").join(pid.to_string());
    let wine = is_wine(
        std::fs::read_link(proc.join("exe")).ok().as_deref(),
        std::fs::read(proc.join("environ")).ok().as_deref(),
    );
    if wine {
        log::info!("Client process {pid} is Wine: it gets wheel steps from touchpad scrolling");
    }
    wine
}

/// Return whether a process whose executable is `exe` and whose
/// environment is `environ` (`/proc/<pid>/environ`'s NUL-separated
/// entries) is Wine; `None` is what could not be read.
fn is_wine(exe: Option<&Path>, environ: Option<&[u8]>) -> bool {
    let wine_exe = exe
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .is_some_and(|name| WINE_EXECUTABLES.contains(&name));
    let wine_environ = environ.is_some_and(|environ| {
        environ.split(|&byte| byte == 0).any(|entry| {
            WINE_ENVIRONMENT_MARKERS
                .iter()
                .any(|marker| entry.starts_with(marker))
        })
    });
    wine_exe || wine_environ
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_is_recognised_by_its_executable_or_its_loader_environment() {
        for name in WINE_EXECUTABLES {
            let exe = Path::new("/usr/lib/wine").join(name);
            assert!(is_wine(Some(&exe), None), "{name}");
        }
        let kakao = b"HOME=/home/u\0WINESERVERSOCKET=3\0";
        assert!(is_wine(None, Some(kakao)));
        assert!(is_wine(None, Some(b"WINELOADERNOEXEC=1\0")));

        // Neither: another program, a marker inside a value, nothing read.
        assert!(!is_wine(Some(Path::new("/usr/bin/winecfg-not")), None));
        assert!(!is_wine(
            Some(Path::new("/usr/bin/foot")),
            Some(b"X=WINESERVERSOCKET=3\0")
        ));
        assert!(!is_wine(None, None));
        // A process `/proc` does not show, and pid 0.
        assert!(!is_wine_process(0));
        assert!(!is_wine_process(i32::MAX));
    }

    #[test]
    fn the_setting_chooses_wine_every_client_or_none() {
        let parse = |value: Option<&str>| WheelFromTouchpad::parse(value.map(OsStr::new));
        assert_eq!(parse(None), WheelFromTouchpad::Auto);
        assert_eq!(parse(Some(" ")), WheelFromTouchpad::Auto);
        assert_eq!(parse(Some("Off")), WheelFromTouchpad::Off);
        assert_eq!(parse(Some(" all ")), WheelFromTouchpad::All);
        assert_eq!(parse(Some("yes")), WheelFromTouchpad::Auto);

        // This test process is not Wine.
        let me = Some(std::process::id() as i32);
        assert!(!WheelFromTouchpad::Auto.applies_to(me));
        assert!(WheelFromTouchpad::All.applies_to(me));
        assert!(WheelFromTouchpad::All.applies_to(None));
        assert!(!WheelFromTouchpad::Off.applies_to(me));
        assert!(!WheelFromTouchpad::Auto.applies_to(None));
    }
}
