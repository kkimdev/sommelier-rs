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
use std::time::Duration;

/// The setting's variable.
const VARIABLE: &str = "SOMMELIER_WHEEL_FROM_TOUCHPAD";

/// The executables a Wine process runs as, by file name.
const WINE_EXECUTABLES: [&str; 4] = ["wine-preloader", "wine64-preloader", "wine", "wine64"];

/// Environment entries only Wine's loader sets in its processes.
const WINE_ENVIRONMENT_MARKERS: [&[u8]; 2] = [b"WINESERVERSOCKET=", b"WINELOADERNOEXEC="];

/// How long a client waits to learn whether it is Wine before it is
/// taken not to be; a read of `/proc` takes microseconds unless it hangs.
/// The wait delays only that client's first message.
const DETECTION_TIMEOUT: Duration = Duration::from_millis(500);

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
    ///
    /// `auto` reads the process's `/proc` entries on a thread of their
    /// own: reading `environ` takes the process's memory lock, which a
    /// stalled process can hold, and the proxy's one thread must not wait
    /// on a client. The thread is detached, not one of Tokio's blocking
    /// pool, so a read that never ends holds up neither the pool nor the
    /// proxy's exit. The caller waits for it at most [`DETECTION_TIMEOUT`],
    /// and then takes the client not to be Wine.
    pub(crate) async fn applies_to(self, pid: Option<i32>) -> bool {
        match (self, pid) {
            (Self::Off, _) | (Self::Auto, None) => false,
            (Self::All, _) => true,
            (Self::Auto, Some(pid)) => {
                let (answer, wine) = tokio::sync::oneshot::channel();
                let detection = std::thread::Builder::new()
                    .name("client-kind".to_string())
                    .spawn(move || {
                        // The client may have gone; then no one asks.
                        let _ = answer.send(is_wine_process(pid));
                    });
                if let Err(error) = detection {
                    log::warn!("Cannot tell whether client process {pid} is Wine: {error}");
                    return false;
                }
                await_detection(pid, wine, DETECTION_TIMEOUT).await
            }
        }
    }
}

/// Wait at most `timeout` for whether process `pid` is `wine`; one that
/// does not answer in time, or whose detection ended without an answer,
/// is not.
async fn await_detection(
    pid: i32,
    wine: tokio::sync::oneshot::Receiver<bool>,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, wine).await {
        Ok(answer) => answer.unwrap_or(false),
        Err(_) => {
            log::info!(
                "Reading client process {pid}'s /proc took over {timeout:?}; it is not taken for Wine"
            );
            false
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

    /// A detection that has not answered by its deadline takes the client
    /// not to be Wine, so a hung `/proc` read cannot stall it.
    #[tokio::test(start_paused = true)]
    async fn a_detection_that_does_not_answer_in_time_is_not_wine() {
        let (answer, wine) = tokio::sync::oneshot::channel();
        let asked = tokio::time::Instant::now();
        assert!(!await_detection(1, wine, DETECTION_TIMEOUT).await);
        assert_eq!(asked.elapsed(), Duration::from_millis(500));
        // A late answer changes nothing.
        assert_eq!(answer.send(true), Err(true));

        let (answer, wine) = tokio::sync::oneshot::channel();
        answer.send(true).expect("the receiver waits");
        assert!(await_detection(1, wine, DETECTION_TIMEOUT).await);
    }

    #[tokio::test]
    async fn the_setting_chooses_wine_every_client_or_none() {
        let parse = |value: Option<&str>| WheelFromTouchpad::parse(value.map(OsStr::new));
        assert_eq!(parse(None), WheelFromTouchpad::Auto);
        assert_eq!(parse(Some(" ")), WheelFromTouchpad::Auto);
        assert_eq!(parse(Some("Off")), WheelFromTouchpad::Off);
        assert_eq!(parse(Some(" all ")), WheelFromTouchpad::All);
        assert_eq!(parse(Some("yes")), WheelFromTouchpad::Auto);

        // This test process is not Wine.
        let me = Some(std::process::id() as i32);
        assert!(!WheelFromTouchpad::Auto.applies_to(me).await);
        assert!(WheelFromTouchpad::All.applies_to(me).await);
        assert!(WheelFromTouchpad::All.applies_to(None).await);
        assert!(!WheelFromTouchpad::Off.applies_to(me).await);
        assert!(!WheelFromTouchpad::Auto.applies_to(None).await);

        // A process with Wine's loader environment is Wine. It says when it
        // runs, so its environment is the one `/proc` shows, and then waits
        // on its stdin without another exec, whose switch `/proc` could see.
        let mut wine = std::process::Command::new("sh")
            .args(["-c", "echo running; read _"])
            .env("WINELOADERNOEXEC", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn a stand-in Wine process");
        let mut running = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(wine.stdout.as_mut().expect("its stdout")),
            &mut running,
        )
        .expect("read that it runs");
        let detected = WheelFromTouchpad::Auto
            .applies_to(Some(wine.id() as i32))
            .await;
        wine.kill().expect("kill the stand-in");
        wine.wait().expect("reap the stand-in");
        assert_eq!(running, "running\n");
        assert!(detected);
    }
}
