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

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use crate::wire::{Framing, MessageHeader, Outgoing, ProtocolError, WireMessage};

/// A message's bytes and its descriptors' numbers.
pub type RawMessage = (Vec<u8>, Vec<RawFd>);

/// Set in the child process [`in_own_process`] runs a test in.
const OWN_PROCESS_ENV: &str = "SOMMELIER_TEST_OWN_PROCESS";

/// Run `body`, the body of test `test_path` (its path in the test binary,
/// such as `proxy::fuzz::guest_streams_cannot_crash_hang_or_leak`), in a
/// child copy of the test binary that runs only that test, for a test that
/// needs the process to itself: its descriptors or its resource limits.
///
/// # Returns
///
/// In the test's own process, the child's standard output once the test
/// passed there; in the child, which runs `body`, nothing.
// A test may wait for its child; no client runs on the test's thread.
#[allow(clippy::disallowed_methods)]
pub fn in_own_process(test_path: &str, body: impl FnOnce()) -> String {
    if std::env::var_os(OWN_PROCESS_ENV).is_some() {
        body();
        return String::new();
    }
    let output = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args([test_path, "--exact", "--test-threads=1", "--nocapture"])
        .env(OWN_PROCESS_ENV, "1")
        .output()
        .expect("run the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // A path that names no test runs nothing and still succeeds.
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test_path} failed in its own process:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// `message` as a [`RawMessage`], which compares where owned descriptors
/// do not. The descriptors close, so the returned numbers name nothing.
pub fn raw_message((message, fds): Outgoing) -> RawMessage {
    (message, fds.iter().map(AsRawFd::as_raw_fd).collect())
}

/// `message`, an encoded message that carries the string `text`, with
/// `text` spelled as `bytes`: a string no encoder writes, such as one that
/// is not UTF-8 or has a NUL inside. `bytes` is as long as `text`, so the
/// framing holds. Only the first occurrence of `text` in the payload
/// changes, so `text` must not be empty or appear earlier in it.
pub fn respell((mut message, fds): Outgoing, text: &str, bytes: &[u8]) -> Outgoing {
    assert!(!text.is_empty(), "an empty text names no bytes to respell");
    assert_eq!(text.len(), bytes.len(), "a respelling keeps the length");
    let payload = &mut message[MessageHeader::LEN..];
    let at = payload
        .windows(text.len())
        .position(|window| window == text.as_bytes())
        .expect("the message carries the text");
    payload[at..at + bytes.len()].copy_from_slice(bytes);
    (message, fds)
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

/// The sender of `message` and the message decoded by `decode`, which
/// takes its descriptors from the front of `fds`. `message` must be one
/// whole message, and `decode` must read all of its payload.
pub fn decode<M>(
    message: &[u8],
    fds: &mut VecDeque<OwnedFd>,
    decode: impl FnOnce(&mut WireMessage) -> Result<M, ProtocolError>,
) -> Result<(u32, M), ProtocolError> {
    let header = match MessageHeader::frame(message) {
        Framing::Complete(header) if header.len == message.len() => header,
        _ => return Err(ProtocolError::InvalidMessageLength(message.len())),
    };
    let mut wire = WireMessage::new(
        header.sender_id,
        header.opcode,
        &message[MessageHeader::LEN..],
    )
    .with_fds(fds);
    let decoded = decode(&mut wire)?;
    if !wire.is_payload_consumed() {
        return Err(ProtocolError::TrailingData);
    }
    Ok((header.sender_id, decoded))
}

/// Each message of a proxy message queue, with its sender, decoded by
/// `decode`, such as `wl_keyboard::Event::from_wire`. The decoded
/// descriptors are duplicates, so the queue keeps its own. Panics if a
/// message does not decode whole, or leaves a descriptor unread.
pub fn decoded<M>(
    queue: &[Outgoing],
    decode: fn(&mut WireMessage) -> Result<M, ProtocolError>,
) -> Vec<(u32, M)> {
    queue
        .iter()
        .map(|(message, fds)| {
            let mut fds: VecDeque<OwnedFd> = fds
                .iter()
                .map(|fd| fd.try_clone().expect("duplicate a queued descriptor"))
                .collect();
            let decoded = self::decode(message, &mut fds, decode)
                .unwrap_or_else(|error| panic!("a queued message does not decode: {error}"));
            assert!(
                fds.is_empty(),
                "a queued message carries unread descriptors"
            );
            decoded
        })
        .collect()
}

/// Whether `a` and `b` refer to the same file, by device and inode, which
/// is how decoded messages compare their descriptors in tests. Two open
/// file descriptions of one file compare equal. Panics for an anonymous
/// inode, such as an eventfd or a sync_file: all of them share one inode,
/// so the comparison would prove nothing.
pub fn same_file(a: &impl AsFd, b: &impl AsFd) -> bool {
    let identity = |fd: &dyn AsFd| {
        let fd = fd.as_fd().as_raw_fd();
        let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .expect("read the descriptor's target");
        assert!(
            !target.to_string_lossy().starts_with("anon_inode:"),
            "descriptor {fd} is an anonymous inode, {target:?}, which compares equal to every other"
        );
        fd_identity(fd)
    };
    identity(a) == identity(b)
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

/// A new pipe's read and write ends. They close on exec, so the child
/// copies of the test binary that some tests start (see
/// [`in_own_process`]) do not inherit them.
pub fn pipe() -> (OwnedFd, OwnedFd) {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("create a pipe")
}

/// A memfd holding `bytes`, as a guest client creates a `wl_shm` pool file.
pub fn shm_pool_file(bytes: &[u8]) -> std::fs::File {
    use std::io::Write;
    let fd = nix::sys::memfd::memfd_create(
        c"sommelier-test-shm-pool",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )
    .expect("memfd_create");
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes).expect("fill the SHM pool file");
    file
}

/// How the host gets a test buffer's pixels: a copy of a guest SHM
/// buffer, or the guest's dma-buf itself.
#[derive(Clone, Copy, Debug)]
pub enum BufferKind {
    Shm,
    Native,
}

pub const BUFFER_KINDS: [BufferKind; 2] = [BufferKind::Shm, BufferKind::Native];

/// Register a 1x1 `kind` buffer `guest_id`, paired with host buffer
/// `host_id`. An SHM buffer's host copy is wholly stale, as a new one's
/// is, so a commit of it copies its pixels.
pub fn register_test_buffer(
    ctx: &mut crate::state::Context,
    kind: BufferKind,
    guest_id: u32,
    host_id: u32,
) {
    use crate::state::StalePixels;
    ctx.shadow_table
        .insert_paired_with_version_for_test(guest_id, host_id, "wl_buffer", 1);
    assert!(match kind {
        BufferKind::Shm => ctx.register_local_buffer(
            guest_id,
            host_id,
            mapped_test_buffer(1, 1, 4, StalePixels::all())
        ),
        BufferKind::Native => ctx.register_native_buffer(host_id, (1, 1), Vec::new()),
    });
}

/// The state of a `width`x`height` local-copy buffer with rows of `stride`
/// bytes, from a pool of zeros, whose host copy is a private mapping a test
/// can read.
pub fn mapped_test_buffer(
    width: i32,
    height: i32,
    stride: u32,
    stale: crate::state::StalePixels,
) -> crate::state::BufferState {
    let size = usize::try_from(height).unwrap() * stride as usize;
    let destination = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(destination, libc::MAP_FAILED);
    crate::state::BufferState {
        pool_file: std::sync::Arc::new(shm_pool_file(&vec![0; size])),
        offset: 0,
        width,
        height,
        stride,
        format: 0,
        dmabuf_fd: None,
        dest_stride: stride,
        dmabuf_plane1_offset: 0,
        dmabuf_plane1_stride: 0,
        dmabuf_sync: false,
        dest_ptr: destination.cast(),
        dest_size: size,
        stale,
    }
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
/// `mkdtemp` picks a name no other directory has, so tests running in
/// parallel, and a directory a crashed run left behind, never collide.
/// The name stays short: a socket bound inside must fit its whole path
/// in the 108 bytes of `sun_path`.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a fresh directory, `sommelier-<label>-` and six random
    /// characters, under the system temporary directory.
    pub fn new(label: &str) -> Self {
        let template = std::env::temp_dir().join(format!("sommelier-{label}-XXXXXX"));
        let path = nix::unistd::mkdtemp(&template).expect("create test directory");
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
    use super::{decode, decoded, shm_pool_file, stream_headers};
    use crate::protocols::wayland::{wl_callback, wl_keyboard};
    use crate::wire::{MessageBuilder, ProtocolError};
    use std::collections::VecDeque;
    use std::os::fd::OwnedFd;

    #[test]
    fn decoded_queues_compare_descriptors_by_the_file_they_refer_to() {
        const KEYBOARD: u32 = 5;
        let keymap = shm_pool_file(b"keymap");
        let keymap_event = |file: &std::fs::File| {
            wl_keyboard::evt::keymap(
                KEYBOARD,
                wl_keyboard::KEYMAP_FORMAT_XKB_V1,
                file.try_clone().unwrap().into(),
                6,
            )
        };
        let queue = vec![
            keymap_event(&keymap),
            wl_keyboard::evt::leave(KEYBOARD, 7, 8),
        ];
        let expected = |file: &std::fs::File| wl_keyboard::Event::Keymap {
            format: wl_keyboard::KEYMAP_FORMAT_XKB_V1,
            fd: OwnedFd::from(file.try_clone().unwrap()),
            size: 6,
        };

        assert_eq!(
            decoded(&queue, wl_keyboard::Event::from_wire),
            [
                (KEYBOARD, expected(&keymap)),
                (
                    KEYBOARD,
                    wl_keyboard::Event::Leave {
                        serial: 7,
                        surface: 8
                    }
                ),
            ]
        );
        assert_eq!(queue[0].1.len(), 1, "the queue keeps its descriptors");
        let other = shm_pool_file(b"keymap");
        assert_ne!(
            decoded(&queue[..1], wl_keyboard::Event::from_wire),
            [(KEYBOARD, expected(&other))],
            "another file with the same bytes is another descriptor"
        );
    }

    #[test]
    #[should_panic(expected = "is an anonymous inode")]
    fn same_file_refuses_anonymous_inodes() {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "an eventfd");
        let event = unsafe { OwnedFd::from_raw_fd(fd) };
        super::same_file(&event, &event);
    }

    #[test]
    fn decode_takes_one_whole_message_and_reads_all_of_it() {
        let decode_done =
            |message: &[u8]| decode(message, &mut VecDeque::new(), wl_callback::Event::from_wire);
        let (done, _) = wl_callback::evt::done(3, 9);
        assert_eq!(
            decode_done(&done),
            Ok((3, wl_callback::Event::Done { callback_data: 9 }))
        );
        assert_eq!(
            decode_done(&done[..done.len() - 1]),
            Err(ProtocolError::InvalidMessageLength(done.len() - 1))
        );
        let mut long = MessageBuilder::new();
        long.write_u32(9);
        long.write_u32(0);
        assert_eq!(
            decode_done(&long.build_message(3, wl_callback::EVT_DONE)),
            Err(ProtocolError::TrailingData)
        );
    }

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
