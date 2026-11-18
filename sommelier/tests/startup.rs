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

//! A proxy that cannot start logs why and exits with status 1, rather
//! than panicking.
//!
//! Each test runs the proxy with `--virtio-wl /dev/null`, which passes the
//! device check, so the failure comes from the step under test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long the proxy may take to fail. One that starts instead would
/// serve until killed.
const TIMEOUT: Duration = Duration::from_secs(10);

/// The proxy's output once it exits, run with `args` and `runtime_dir`
/// as `XDG_RUNTIME_DIR`, or with none. A proxy still running after
/// [`TIMEOUT`] is killed, and the test fails.
// A plain test thread, with no event loop, waits for the proxy.
#[allow(clippy::disallowed_methods)]
fn run_proxy(runtime_dir: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sommelier"));
    command
        .args(["--virtio-wl", "/dev/null"])
        .args(args)
        .env("RUST_LOG", "error")
        .env_remove("XDG_RUNTIME_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = runtime_dir {
        command.env("XDG_RUNTIME_DIR", dir);
    }
    let mut proxy = command.spawn().expect("start the proxy");
    let deadline = Instant::now() + TIMEOUT;
    while proxy.try_wait().expect("poll the proxy").is_none() {
        if Instant::now() >= deadline {
            proxy.kill().expect("kill the proxy");
            let output = proxy.wait_with_output().expect("reap the proxy");
            panic!(
                "the proxy started instead of failing:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    proxy.wait_with_output().expect("read the proxy's output")
}

/// Assert that the proxy exited with status 1 and logged `error`, without
/// a panic.
fn assert_failed_cleanly(output: &Output, error: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains(error), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn a_missing_runtime_directory_is_a_logged_startup_error() {
    let output = run_proxy(None, &[]);
    assert_failed_cleanly(&output, "XDG_RUNTIME_DIR");
}

#[test]
fn an_unbindable_socket_is_a_logged_startup_error() {
    let dir = PathBuf::from(format!(
        "{}/sommelier-startup-{}",
        std::env::temp_dir().display(),
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the runtime directory");
    // Longer than a socket address holds, so the proxy can take the lock
    // beside the socket but cannot bind it.
    let display = "w".repeat(120);
    let output = run_proxy(Some(&dir), &[&display]);
    std::fs::remove_dir_all(&dir).expect("remove the runtime directory");
    assert_failed_cleanly(&output, "Cannot bind the Wayland socket");
}
