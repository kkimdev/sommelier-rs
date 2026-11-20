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
use std::sync::mpsc;

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

/// Tells whether the process with a pid is Wine. It runs on a thread of
/// its own, so it may block; [`is_wine_process`] reads `/proc`, and a
/// test passes its own.
pub(crate) type ClientProbe = fn(i32) -> bool;

impl WheelFromTouchpad {
    /// Parse the setting's value, case-insensitively and ignoring
    /// surrounding whitespace; blank is `auto`. `None` for any other word.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Start deciding whether the client whose process is `pid` (`None`
    /// when the socket did not say) gets wheel steps.
    ///
    /// `auto` asks `probe` on a thread of its own: reading a process's
    /// `environ` takes its memory lock, which a stalled process can hold,
    /// and the proxy's one thread must not wait on a client. The thread
    /// is detached, not one of Tokio's blocking pool, so a read that never
    /// ends holds up neither the pool nor the proxy's exit. Nothing waits
    /// for it: the client's first scroll after the answer settles the
    /// decision ([`WheelClient::settle`]).
    pub(crate) fn decide(self, pid: Option<i32>, probe: ClientProbe) -> WheelClient {
        match (self, pid) {
            (Self::Off, _) | (Self::Auto, None) => WheelClient::No,
            (Self::All, _) => WheelClient::Yes,
            (Self::Auto, Some(pid)) => {
                let (answer, wine) = mpsc::sync_channel(1);
                let detection = std::thread::Builder::new()
                    .name("client-kind".to_string())
                    .spawn(move || {
                        // The client may have gone; then no one asks.
                        let _ = answer.send(probe(pid));
                    });
                match detection {
                    Ok(_) => WheelClient::Pending(wine),
                    Err(error) => {
                        log::warn!("Cannot tell whether client process {pid} is Wine: {error}");
                        WheelClient::No
                    }
                }
            }
        }
    }
}

/// Whether a connection's client gets wheel steps, as far as known.
#[derive(Debug)]
pub(crate) enum WheelClient {
    No,
    Yes,
    /// Its process is still being read; the receiver gets the answer.
    Pending(mpsc::Receiver<bool>),
}

impl WheelClient {
    /// Return whether the client gets wheel steps, settling a pending
    /// decision whose detection has answered. One that has not answered
    /// yet is no for now and asked again next time; one whose detection
    /// ended without an answer is no for good.
    pub(crate) fn settle(&mut self) -> bool {
        if let Self::Pending(wine) = self {
            *self = match wine.try_recv() {
                Ok(true) => Self::Yes,
                Ok(false) | Err(mpsc::TryRecvError::Disconnected) => Self::No,
                Err(mpsc::TryRecvError::Empty) => return false,
            };
        }
        matches!(self, Self::Yes)
    }
}

/// Return whether process `pid` is Wine, from what `/proc` shows of it.
/// Pid 0, which a peer in another PID namespace reports, and a process
/// `/proc` does not show are not.
pub(crate) fn is_wine_process(pid: i32) -> bool {
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

    /// A detection settles only once it has answered: until then the
    /// client gets no steps, and nothing waits for it.
    #[test]
    fn a_client_scrolls_unchanged_until_its_detection_answers() {
        let (answer, wine) = mpsc::sync_channel(1);
        let mut client = WheelClient::Pending(wine);
        assert!(!client.settle(), "no answer yet");
        assert!(matches!(client, WheelClient::Pending(_)));
        answer.send(true).expect("the client waits");
        assert!(client.settle());
        assert!(matches!(client, WheelClient::Yes));

        let (answer, wine) = mpsc::sync_channel(1);
        let mut client = WheelClient::Pending(wine);
        drop(answer);
        assert!(!client.settle(), "a detection that ended without an answer");
        assert!(matches!(client, WheelClient::No));
    }

    /// Wait for `client`'s detection to answer, then settle it.
    fn settled(client: WheelClient) -> bool {
        match client {
            WheelClient::Pending(wine) => wine.recv().unwrap_or(false),
            WheelClient::Yes => true,
            WheelClient::No => false,
        }
    }

    #[test]
    fn the_setting_chooses_wine_every_client_or_none() {
        let parse = WheelFromTouchpad::parse;
        assert_eq!(parse(" "), Some(WheelFromTouchpad::Auto));
        assert_eq!(parse("Auto"), Some(WheelFromTouchpad::Auto));
        assert_eq!(parse("Off"), Some(WheelFromTouchpad::Off));
        assert_eq!(parse(" all "), Some(WheelFromTouchpad::All));
        assert_eq!(parse("yes"), None);

        let wine: ClientProbe = |_| true;
        let not_wine: ClientProbe = |_| false;
        let decide = |setting: WheelFromTouchpad, pid, probe| settled(setting.decide(pid, probe));
        assert!(decide(WheelFromTouchpad::Auto, Some(7), wine));
        assert!(!decide(WheelFromTouchpad::Auto, Some(7), not_wine));
        assert!(!decide(WheelFromTouchpad::Auto, None, wine), "no pid");
        assert!(decide(WheelFromTouchpad::All, Some(7), not_wine));
        assert!(decide(WheelFromTouchpad::All, None, not_wine));
        assert!(!decide(WheelFromTouchpad::Off, Some(7), wine));

        // This test process is not Wine.
        assert!(!is_wine_process(std::process::id() as i32));

        // A process with Wine's loader environment is Wine. It says when it
        // runs, so its environment is the one `/proc` shows, and then waits
        // on its stdin without another exec, whose switch `/proc` could see.
        let mut stand_in = std::process::Command::new("sh")
            .args(["-c", "echo running; read _"])
            .env("WINELOADERNOEXEC", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn a stand-in Wine process");
        let mut running = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(stand_in.stdout.as_mut().expect("its stdout")),
            &mut running,
        )
        .expect("read that it runs");
        let detected = is_wine_process(stand_in.id() as i32);
        stand_in.kill().expect("kill the stand-in");
        stand_in.wait().expect("reap the stand-in");
        assert_eq!(running, "running\n");
        assert!(detected);
    }
}
