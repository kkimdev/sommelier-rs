// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Real clients through `sommelier --local-compositor` and a headless
//! weston.
//!
//! `wayland-info` enumerates every global through the proxy, then
//! `weston-simple-shm` draws SHM frames for a few seconds and quits on
//! SIGINT. Both must exit cleanly, the proxy must stay up, copy frames and
//! log no error.
//!
//! The programs come from the `weston` and `wayland-utils` packages. The
//! test passes with a note when one is missing, unless
//! `SOMMELIER_E2E_REQUIRED=1`, as CI sets it where it installs them.

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

/// How long a process may take to start serving or to exit.
const TIMEOUT: Duration = Duration::from_secs(20);
/// How long `weston-simple-shm` draws.
const DRAW_TIME: Duration = Duration::from_secs(3);
/// The fewest SHM copies a working proxy makes in [`DRAW_TIME`]; the client
/// draws at the 60 Hz of weston's headless output.
const MIN_FRAMES: usize = 30;

#[test]
fn real_clients_run_through_the_proxy_against_headless_weston() {
    let programs = ["weston", "wayland-info", "weston-simple-shm"].map(find_program);
    let [Some(weston), Some(wayland_info), Some(simple_shm)] = programs else {
        let message = "weston, wayland-info or weston-simple-shm is not installed \
                       (packages weston and wayland-utils)";
        assert!(
            std::env::var("SOMMELIER_E2E_REQUIRED").as_deref() != Ok("1"),
            "{message}"
        );
        eprintln!("skipping: {message}");
        return;
    };

    let dir = RuntimeDir::new();
    let compositor = dir.path().join("weston");
    let mut weston = dir.spawn(
        "weston",
        dir.command(weston).args([
            "--no-config",
            "--backend=headless",
            "--socket=weston",
            "--idle-time=0",
        ]),
    );
    dir.wait_for_socket(&compositor, "weston", &mut weston);
    let mut proxy = dir.spawn(
        "proxy",
        dir.command(env!("CARGO_BIN_EXE_sommelier"))
            .arg("--local-compositor")
            .arg(&compositor)
            .arg("proxy")
            .env("RUST_LOG", "info,sommelier::handler::compositor=trace"),
    );
    dir.wait_for_socket(&dir.path().join("proxy"), "proxy", &mut proxy);

    let mut info = dir.spawn(
        "wayland-info",
        dir.command(wayland_info).env("WAYLAND_DISPLAY", "proxy"),
    );
    let status = wait(&mut info.0);
    let globals = dir.log("wayland-info");
    assert!(status.success(), "wayland-info: {status}\n{globals}");
    for interface in ["wl_compositor", "wl_shm", "wl_output", "xdg_wm_base"] {
        assert!(
            globals.contains(&format!("'{interface}'")),
            "{interface} is missing:\n{globals}"
        );
    }

    let mut client = dir.spawn(
        "simple-shm",
        dir.command(simple_shm).env("WAYLAND_DISPLAY", "proxy"),
    );
    #[allow(clippy::disallowed_methods)] // A plain thread, no event loop.
    std::thread::sleep(DRAW_TIME);
    assert!(
        client.0.try_wait().expect("poll simple-shm").is_none(),
        "weston-simple-shm quit early:\n{}",
        dir.log("simple-shm")
    );
    // simple-shm quits cleanly on SIGINT.
    assert_eq!(unsafe { libc::kill(client.0.id() as i32, libc::SIGINT) }, 0);
    let status = wait(&mut client.0);
    assert!(
        status.success(),
        "weston-simple-shm: {status}\n{}",
        dir.log("simple-shm")
    );

    assert!(
        proxy.0.try_wait().expect("poll the proxy").is_none(),
        "the proxy exited"
    );
    let log = dir.log("proxy");
    assert!(
        !log.contains(" ERROR "),
        "the proxy logged an error:\n{log}"
    );
    // The proxy's trace of each commit's copy to the host buffer.
    let frames = log.matches("Copied damaged SHM buffer").count();
    assert!(
        frames >= MIN_FRAMES,
        "only {frames} frames reached the host:\n{log}"
    );
}

/// The path of `program` on `PATH`.
fn find_program(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

/// A private `XDG_RUNTIME_DIR` for one test run, holding the sockets and
/// each process's log; removed when dropped.
struct RuntimeDir(PathBuf);

impl RuntimeDir {
    fn new() -> Self {
        // Directly under /tmp, not TMPDIR, because a socket path must fit in
        // 108 bytes; the time keeps a directory a crashed run left behind
        // from colliding with this one.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is past 1970")
            .as_nanos();
        let path = PathBuf::from(format!("/tmp/sommelier-e2e-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path).expect("create the runtime directory");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make the runtime directory private");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// `program` in this runtime directory, with no display or
    /// configuration of the caller's.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env("XDG_RUNTIME_DIR", &self.0)
            .env("XDG_CONFIG_HOME", &self.0)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY");
        command
    }

    /// Start `command`, logging to `<name>.log`.
    fn spawn(&self, name: &str, command: &mut Command) -> Process {
        let log = File::create(self.0.join(format!("{name}.log"))).expect("create a log");
        let child = command
            .stdout(log.try_clone().expect("share the log"))
            .stderr(log)
            .spawn()
            .unwrap_or_else(|error| panic!("start {name}: {error}"));
        Process(child)
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.0.join(format!("{name}.log"))).unwrap_or_default()
    }

    /// Wait until `server` (logging as `name`) accepts connections on
    /// `socket`; fail as soon as it exits instead.
    fn wait_for_socket(&self, socket: &Path, name: &str, server: &mut Process) {
        let deadline = Instant::now() + TIMEOUT;
        while UnixStream::connect(socket).is_err() {
            if let Some(status) = server.0.try_wait().expect("poll a server") {
                panic!("{name} exited with {status}:\n{}", self.log(name));
            }
            assert!(
                Instant::now() < deadline,
                "{name} does not serve {}:\n{}",
                socket.display(),
                self.log(name)
            );
            #[allow(clippy::disallowed_methods)] // A plain thread, no event loop.
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A child process, killed and reaped when dropped.
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        // Already gone, or killed here; nothing to report while dropping.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wait for `child` to exit, killing it after [`TIMEOUT`].
fn wait(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll a child process") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("a child process did not exit: {:?}", child.wait());
        }
        #[allow(clippy::disallowed_methods)] // A plain thread, no event loop.
        std::thread::sleep(Duration::from_millis(10));
    }
}
