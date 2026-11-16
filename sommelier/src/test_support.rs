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
