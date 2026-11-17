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

use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

use crate::wire::{Framing, MessageHeader, Outgoing, ProtocolError};

/// A message's bytes and its descriptors' numbers.
pub type RawMessage = (Vec<u8>, Vec<RawFd>);

/// `message` as a [`RawMessage`], which compares where owned descriptors
/// do not. The descriptors close, so the returned numbers name nothing.
pub fn raw_message((message, fds): Outgoing) -> RawMessage {
    (message, fds.iter().map(AsRawFd::as_raw_fd).collect())
}

/// A dispatch result whose message compares; see [`raw_message`].
pub fn raw_dispatch(
    result: Result<Option<Outgoing>, ProtocolError>,
) -> Result<Option<RawMessage>, ProtocolError> {
    result.map(|message| message.map(raw_message))
}

/// The header of an encoded Wayland message.
pub fn msg_header(message: &[u8]) -> MessageHeader {
    MessageHeader::parse(message).expect("a complete Wayland message header")
}

/// The sender object ID of an encoded Wayland message.
pub fn msg_sender(message: &[u8]) -> u32 {
    msg_header(message).sender_id
}

/// The opcode of an encoded Wayland message.
pub fn msg_opcode(message: &[u8]) -> u16 {
    msg_header(message).opcode
}

/// The object and code of the `wl_display.error` that ends the session
/// with the message `ctx` is dispatching.
pub fn protocol_error(ctx: &crate::state::Context) -> (u32, u32) {
    let end = ctx.session_end().expect("the message ends the session");
    let (message, _) = crate::handler::display::session_error(end);
    assert_eq!(msg_sender(&message), 1);
    assert_eq!(
        msg_opcode(&message),
        crate::protocols::wayland::wl_display::EVT_ERROR
    );
    let word = |at: usize| u32::from_ne_bytes(message[at..at + 4].try_into().unwrap());
    (word(8), word(12))
}

/// The code of the `wl_display.error` that ends the session with the
/// message `ctx` is dispatching.
pub fn protocol_error_code(ctx: &crate::state::Context) -> u32 {
    protocol_error(ctx).1
}

/// The `(sender, opcode)` of every message in a proxy message queue.
pub fn queued_headers(queue: &[crate::wire::Outgoing]) -> Vec<(u32, u16)> {
    queue
        .iter()
        .map(|(message, _)| (msg_sender(message), msg_opcode(message)))
        .collect()
}

/// Each complete message at the start of a Wayland byte stream, up to a
/// partial or corrupt one, with its payload.
pub fn frames(mut stream: &[u8]) -> impl Iterator<Item = (MessageHeader, &[u8])> {
    std::iter::from_fn(move || {
        let Framing::Complete(header) = MessageHeader::frame(stream) else {
            return None;
        };
        let (message, rest) = stream.split_at(header.len);
        stream = rest;
        Some((header, &message[MessageHeader::LEN..]))
    })
}

/// The `(sender, opcode)` of every complete message at the start of a
/// Wayland byte stream, up to a partial or corrupt one.
pub fn stream_headers(stream: &[u8]) -> Vec<(u32, u16)> {
    frames(stream)
        .map(|(header, _)| (header.sender_id, header.opcode))
        .collect()
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
    fds_with_identity(identity) > 0
}

/// Returns how many descriptors in this process refer to `identity`.
pub fn fds_with_identity(identity: TestFdIdentity) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read process descriptor directory")
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
        .filter_map(try_fd_identity)
        .filter(|&current| current == identity)
        .count()
}

/// A memfd holding `bytes`, as a guest client creates a `wl_shm` pool file.
pub fn shm_pool_file(bytes: &[u8]) -> std::fs::File {
    use std::io::Write;
    let fd = nix::sys::memfd::memfd_create(
        c"sommelier-test-shm-pool",
        nix::sys::memfd::MFdFlags::empty(),
    )
    .expect("memfd_create");
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).expect("fill the SHM pool file");
    file
}

/// A seeded pseudo-random generator (SplitMix64) for randomized tests.
///
/// Small and dependency-free; the same seed always yields the same values,
/// so a failing case's input reproduces from the seed it reports. A test
/// whose outcome also depends on timing may need a few replays.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A value in `0..bound`; `bound` must be positive.
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    /// True once in `n` calls on average.
    pub fn one_in(&mut self, n: u64) -> bool {
        self.next_u64().is_multiple_of(n)
    }

    /// One element of the non-empty slice `items`.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// `len` random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u64() as u8).collect()
    }
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

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::stream_headers;
    use crate::wire::MessageBuilder;

    #[test]
    fn stream_headers_stop_at_a_partial_or_corrupt_message() {
        let mut stream = MessageBuilder::new().build_message(7, 3);
        stream.extend(MessageBuilder::new().build_message(8, 4));
        assert_eq!(stream_headers(&stream), vec![(7, 3), (8, 4)]);

        let complete = stream.len();
        stream.extend_from_slice(&9_u32.to_ne_bytes());
        stream.extend_from_slice(&((16_u32 << 16) | 5).to_ne_bytes());
        assert_eq!(
            stream_headers(&stream),
            vec![(7, 3), (8, 4)],
            "a message longer than the stream is partial"
        );

        stream.truncate(complete);
        stream.extend_from_slice(&9_u32.to_ne_bytes());
        stream.extend_from_slice(&0_u32.to_ne_bytes());
        assert_eq!(
            stream_headers(&stream),
            vec![(7, 3), (8, 4)],
            "a zero length must not loop forever"
        );
    }
}
