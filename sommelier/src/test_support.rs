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

//! Shared helpers for unit tests.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

/// The sender object ID of an encoded Wayland message.
pub fn msg_sender(message: &[u8]) -> u32 {
    u32::from_ne_bytes(message[0..4].try_into().unwrap())
}

/// The opcode of an encoded Wayland message.
pub fn msg_opcode(message: &[u8]) -> u16 {
    (u32::from_ne_bytes(message[4..8].try_into().unwrap()) & 0xffff) as u16
}

/// The `(sender, opcode)` of every message in a proxy message queue.
pub fn queued_headers(queue: &[(Vec<u8>, Vec<RawFd>)]) -> Vec<(u32, u16)> {
    queue
        .iter()
        .map(|(message, _)| (msg_sender(message), msg_opcode(message)))
        .collect()
}

/// The `(sender, opcode)` of every message in a Wayland byte stream.
pub fn stream_headers(mut stream: &[u8]) -> Vec<(u32, u16)> {
    let mut headers = Vec::new();
    while stream.len() >= 8 {
        let length = (u32::from_ne_bytes(stream[4..8].try_into().unwrap()) >> 16) as usize;
        headers.push((msg_sender(stream), msg_opcode(stream)));
        stream = &stream[length..];
    }
    headers
}

/// The `(device, inode)` pair of an open file description.
///
/// Descriptor numbers are recycled immediately after `close`, so under the
/// parallel test harness another test can reuse a number between a close and
/// an assertion about it. The identity of the underlying object is stable, so
/// ownership assertions compare identities instead of numbers. Use a pipe or a
/// memfd for such descriptors: anonymous-inode objects such as eventfds share
/// one inode on Linux.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestFdIdentity {
    device: libc::dev_t,
    inode: libc::ino_t,
}

fn try_fd_identity(fd: RawFd) -> Option<TestFdIdentity> {
    let mut metadata = unsafe { std::mem::zeroed::<libc::stat>() };
    (unsafe { libc::fstat(fd, &mut metadata) } == 0).then_some(TestFdIdentity {
        device: metadata.st_dev,
        inode: metadata.st_ino,
    })
}

/// Returns the identity of `fd`, which must be open.
pub fn fd_identity(fd: RawFd) -> TestFdIdentity {
    try_fd_identity(fd).expect("stat test descriptor")
}

/// Returns whether any descriptor in this process refers to `identity`.
pub fn any_fd_has_identity(identity: TestFdIdentity) -> bool {
    std::fs::read_dir("/proc/self/fd")
        .expect("read process descriptor directory")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
        .filter_map(try_fd_identity)
        .any(|current| current == identity)
}

/// A uniquely named scratch directory removed when dropped.
///
/// Tests run in parallel inside one process, so the name combines the process
/// ID, a caller label, and a per-process counter.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory under the system temporary directory.
    pub fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sommelier-test-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create test directory");
        Self { path }
    }

    /// Return the directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replace the file `name` with `contents` and return its path.
    pub fn write(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.path.join(name);
        std::fs::write(&path, contents).expect("write test file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
