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

//! Wire traces: what one client connection's proxy dispatched and sent,
//! recorded so that tests can replay it (`traces/README.md`).
//!
//! Recording is off unless `SOMMELIER_TRACE_DIR` names a directory
//! ([`crate::settings::Settings::trace_dir`]); then each connection writes
//! one file there, which holds everything the user typed into the
//! client. A trace is text, one record a line, in the order the proxy
//! made them:
//!
//! ```text
//! sommelier-trace 1
//! facts host=virtwl-dmabuf render-node=no gpu-accel=no xdg-decoration=no placement=off accelerators=none vm-id=termina
//! g> 0 0100000000000c0002000000
//! >h 0 0100000000000c0002000000
//! ```
//!
//! - The first line names the format and its version, [`HEADER`].
//! - The second states the [`Facts`] of the connection, which decide,
//!   besides the messages, what the proxy sends.
//! - Each other line is a [`Stream`]'s tag, the number of descriptors the
//!   message carried, and the message's bytes in hex. A message the proxy
//!   dispatched (`g>` from the guest, `h>` from the host) is recorded once
//!   it is handled, or once it ends the session, handled or not; a stream
//!   the proxy cannot frame is recorded whole, from where framing failed.
//!   What one batch of them gave each peer (`>h`, `>g`) follows the batch,
//!   one line per message, or per run of messages the proxy sends together
//!   ([`OutgoingMessages`]). The line's place is the record's sequence
//!   number: nothing else orders the records.
//! - A host `wl_keyboard.keymap` is followed by a line of its keymap's
//!   text ([`KEYMAP`], then the bytes in hex; none if the proxy could not
//!   read it).
//! - A line that starts with `#` is a comment, such as what a trace's
//!   scenario was; the proxy writes none.
//!
//! A change of the format is a new version, which converts the traces of
//! the corpus (`traces/`).
//!
//! Of the descriptors, a trace records how many each message carried, and
//! only a keymap's contents: a keyboard layout, which decides what the
//! proxy makes of each key, and which the replay cannot guess. A buffer's
//! pixels, a clipboard transfer and the like are never recorded.
//!
//! Recording never ends a session: a write that fails, or a trace that
//! reaches [`MAX_TRACE_BYTES`], ends only the recording, which logs why.
//! A batch's records are written whole, at its end, so a recording that
//! ends leaves no torn record unless the write itself failed midway.

use std::cell::Cell;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::connection::OutgoingMessages;
use crate::state::Context;

/// The first line of every trace: its format, and the format's version.
pub(crate) const HEADER: &str = "sommelier-trace 1";

/// What starts the line of a trace's [`Facts`].
pub(crate) const FACTS: &str = "facts ";

/// The tag of a keymap's line.
pub(crate) const KEYMAP: &str = "km";

/// The most bytes a trace takes: a client that draws keeps sending, and a
/// recording left on must not fill the disk.
pub(crate) const MAX_TRACE_BYTES: u64 = 256 << 20;

/// Which way a recorded message went, as the proxy saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    /// A request of the guest's that the proxy dispatched.
    FromGuest,
    /// An event of the host's that the proxy dispatched.
    FromHost,
    /// What the proxy sent the guest.
    ToGuest,
    /// What the proxy sent the host.
    ToHost,
}

impl Stream {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] =
        [Self::FromGuest, Self::FromHost, Self::ToGuest, Self::ToHost];

    /// The tag that starts the stream's lines.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::FromGuest => "g>",
            Self::FromHost => "h>",
            Self::ToGuest => ">g",
            Self::ToHost => ">h",
        }
    }
}

/// What decides, besides the messages, what a connection's proxy sends:
/// the settings and the connection's facts that change the wire, which a
/// replay reproduces or refuses. A client's wheel steps are not among
/// them, since a connection learns them only later (`client_kind.rs`); a
/// replay gives none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// How the proxy reaches the host: a VirtWL channel that can allocate
    /// dma-bufs or one that cannot, or a socket.
    host: &'static str,
    render_node: bool,
    gpu_accel: bool,
    xdg_decoration: bool,
    placement: bool,
    accelerators: bool,
    vm_id: String,
}

impl Facts {
    /// The facts of `ctx`'s connection.
    pub(crate) fn of(ctx: &Context) -> Self {
        let settings = ctx.settings();
        Self {
            host: match ctx.virtwayland_channel() {
                Some(channel) if channel.supports_dmabuf() => "virtwl-dmabuf",
                Some(_) => "virtwl",
                None => "socket",
            },
            render_node: ctx.virtgpu().is_some(),
            gpu_accel: settings.gpu_accel,
            xdg_decoration: settings.xdg_decoration,
            placement: ctx.placement().is_some(),
            accelerators: settings.host_accelerators != Default::default(),
            vm_id: settings.vm_id.clone(),
        }
    }
}

impl fmt::Display for Facts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let yes = |on: bool| if on { "yes" } else { "no" };
        write!(
            f,
            "host={} render-node={} gpu-accel={} xdg-decoration={} placement={} accelerators={} \
             vm-id={}",
            self.host,
            yes(self.render_node),
            yes(self.gpu_accel),
            yes(self.xdg_decoration),
            if self.placement { "on" } else { "off" },
            if self.accelerators { "some" } else { "none" },
            escape(&self.vm_id),
        )
    }
}

/// `value` as a fact states it: each byte but an ASCII letter, digit, `.`,
/// `_` or `-` as `%` and two hex digits, so no value holds a blank or an
/// `=`.
fn escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02x}"),
        })
        .collect()
}

/// The value a fact states as `escaped` ([`escape`]); `None` if it is not
/// one.
#[cfg(test)]
pub(crate) fn unescape(escaped: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut rest = escaped.as_bytes();
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex = std::str::from_utf8(rest.get(..2)?).ok()?;
        bytes.push(u8::from_str_radix(hex, 16).ok()?);
        rest = &rest[2..];
    }
    String::from_utf8(bytes).ok()
}

/// The traces of the process's connections: the directory they go to,
/// and how many connections have started one.
pub(crate) struct Traces {
    dir: PathBuf,
    started: Cell<u64>,
}

impl Traces {
    /// Record every connection in `dir`; `None`, logged, if it is not a
    /// directory, so that nothing is recorded.
    pub(crate) fn new(dir: PathBuf) -> Option<Self> {
        if !dir.is_dir() {
            log::error!("Recording nothing: {} is not a directory", dir.display());
            return None;
        }
        log::warn!(
            "Recording every client's Wayland traffic, typed text included, in {}",
            dir.display()
        );
        Some(Self {
            dir,
            started: Cell::new(0),
        })
    }

    /// Start recording the connection of `ctx`, in a new file of its own
    /// that only this user can read, named for when the process started
    /// it, the process and the connection; `None`, logged, if it cannot be
    /// created.
    pub(crate) fn start(&self, ctx: &Context) -> Option<Recorder> {
        let connection = self.started.get();
        self.started.set(connection + 1);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let pid = std::process::id();
        let path = self.dir.join(format!("{started}-{pid}-{connection}.trace"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path);
        match file.and_then(|file| Recorder::new(Box::new(file), &Facts::of(ctx))) {
            Ok(recorder) => {
                log::info!("Recording this client's traffic in {}", path.display());
                Some(recorder)
            }
            Err(error) => {
                log::error!(
                    "Cannot record this client's traffic in {}: {error}",
                    path.display()
                );
                None
            }
        }
    }
}

/// One connection's trace as it is written.
pub(crate) struct Recorder {
    out: Box<dyn Write>,
    /// The records of the batch so far, which [`Self::flush`] writes.
    batch: Vec<u8>,
    /// How many bytes the trace holds.
    written: u64,
    /// The most bytes it may hold ([`MAX_TRACE_BYTES`]).
    limit: u64,
    /// The recording ended: a write failed, or the trace is full.
    ended: bool,
}

impl Recorder {
    /// A trace of a connection with `facts`, written to `out`.
    pub(crate) fn new(mut out: Box<dyn Write>, facts: &Facts) -> io::Result<Self> {
        let header = format!("{HEADER}\n{FACTS}{facts}\n");
        out.write_all(header.as_bytes())?;
        Ok(Self {
            out,
            batch: Vec::new(),
            written: header.len() as u64,
            limit: MAX_TRACE_BYTES,
            ended: false,
        })
    }

    /// Record `message`, on `stream` with `fds` descriptors.
    pub(crate) fn record(&mut self, stream: Stream, message: &[u8], fds: usize) {
        if !self.ended {
            self.batch
                .extend_from_slice(format!("{} {fds} ", stream.tag()).as_bytes());
            self.hex_line(message);
        }
    }

    /// Record `text`, the keymap of the host `wl_keyboard.keymap` just
    /// recorded, as `crate::keymap::bytes` reads it: empty if it could not
    /// be read.
    pub(crate) fn keymap(&mut self, text: &[u8]) {
        if !self.ended {
            self.batch
                .extend_from_slice(format!("{KEYMAP} ").as_bytes());
            self.hex_line(text);
        }
    }

    fn hex_line(&mut self, bytes: &[u8]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            let digits = [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0xf)]];
            self.batch.extend_from_slice(&digits);
        }
        self.batch.push(b'\n');
    }

    /// Record what `messages` gained since it held `mark` entries, on
    /// `stream`.
    pub(crate) fn record_since(
        &mut self,
        stream: Stream,
        messages: &OutgoingMessages,
        mark: usize,
    ) {
        for (message, fds) in messages.since(mark) {
            self.record(stream, message, fds);
        }
    }

    /// Write the batch's records, as each batch ends, so a proxy that is
    /// stopped leaves its traces whole up to its last batch.
    pub(crate) fn flush(&mut self) {
        let batch = std::mem::take(&mut self.batch);
        if self.ended || batch.is_empty() {
            return;
        }
        if self.written + batch.len() as u64 > self.limit {
            log::error!(
                "This client's trace reached {} bytes, which ends its recording",
                self.limit
            );
            self.ended = true;
            return;
        }
        match self.out.write_all(&batch).and_then(|()| self.out.flush()) {
            Ok(()) => self.written += batch.len() as u64,
            Err(error) => {
                log::error!("Recording this client's traffic failed, which ends it: {error}");
                self.ended = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's connection.
    fn ctx() -> Context {
        Context::new_for_test(false, false, Vec::new())
    }

    /// Each connection gets a trace file of its own, which only the user
    /// can read, named for when and by which process and connection it
    /// started, and which starts with the header and the connection's
    /// facts. A path that is no directory records nothing, and a
    /// connection whose file cannot be created is not recorded.
    #[test]
    fn each_connection_records_into_a_private_file_of_its_own() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::TempDir::new("trace-files");
        let traces_dir = dir.path().join("traces");
        std::fs::create_dir(&traces_dir).unwrap();
        let traces = Traces::new(traces_dir.clone()).expect("a directory");
        let pid = std::process::id();
        for connection in 0..2 {
            let mut recorder = traces.start(&ctx()).expect("a recording");
            recorder.record(Stream::FromGuest, &[1, 0, 0, 0, 1, 0, 12, 0], 0);
            recorder.flush();
            let suffix = format!("-{pid}-{connection}.trace");
            let path = std::fs::read_dir(&traces_dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.to_str().unwrap().ends_with(&suffix))
                .expect("the connection's file");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", path.display());
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!(
                    "{HEADER}\n{FACTS}{}\ng> 0 0100000001000c00\n",
                    Facts::of(&ctx())
                ),
            );
        }

        assert!(Traces::new(dir.path().join("missing")).is_none());
        let file = dir.write("file", b"");
        assert!(Traces::new(file).is_none());
        std::fs::remove_dir_all(&traces_dir).unwrap();
        assert!(traces.start(&ctx()).is_none());
    }

    /// A write that fails ends the recording, which writes nothing more,
    /// and nothing else: recording a message returns as usual.
    #[test]
    fn a_failed_write_ends_only_the_recording() {
        struct Full(std::rc::Rc<std::cell::Cell<usize>>);
        impl Write for Full {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.set(self.0.get() + 1);
                match self.0.get() {
                    1 => Ok(bytes.len()),
                    _ => Err(io::ErrorKind::StorageFull.into()),
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let writes = std::rc::Rc::default();
        let out = Box::new(Full(std::rc::Rc::clone(&writes)));
        let mut recorder = Recorder::new(out, &Facts::of(&ctx())).expect("the header fits");
        recorder.record(Stream::ToGuest, &[0; 8], 0);
        recorder.flush();
        assert_eq!((writes.get(), recorder.ended), (2, true));
        recorder.record(Stream::ToGuest, &[0; 8], 0);
        recorder.flush();
        assert_eq!(writes.get(), 2, "a failed recording writes nothing more");
    }

    /// A batch that would take the trace past its limit ends the
    /// recording, and none of it is written: the trace ends with the last
    /// whole batch.
    #[test]
    fn a_full_trace_ends_its_recording_after_its_last_whole_batch() {
        let tape = crate::test_support::trace::Tape::default();
        let mut recorder = Recorder::new(Box::new(tape.clone()), &Facts::of(&ctx())).unwrap();
        recorder.limit = recorder.written + 2 * "g> 0 00\n".len() as u64;
        for _ in 0..3 {
            recorder.record(Stream::FromGuest, &[0], 0);
            recorder.flush();
        }
        assert!(recorder.ended);
        assert!(
            tape.text().ends_with("\ng> 0 00\ng> 0 00\n"),
            "{}",
            tape.text()
        );
        assert!(!tape.text().contains("g> 0 00\ng> 0 00\ng> 0 00"));
    }

    /// The facts of a test's connection: no VirtWL channel, render node,
    /// placement or switch, and the default VM name. A VM name is escaped,
    /// so that a fact holds no blank or `=`, and reads back.
    #[test]
    fn a_test_connections_facts_are_all_off() {
        assert_eq!(
            Facts::of(&ctx()).to_string(),
            "host=socket render-node=no gpu-accel=no xdg-decoration=no placement=off \
             accelerators=none vm-id=termina"
        );
        let name = "my vm=1%\u{ac00}";
        assert_eq!(escape(name), "my%20vm%3d1%25%ea%b0%80");
        assert_eq!(unescape(&escape(name)).as_deref(), Some(name));
        assert_eq!(unescape("%4"), None);
    }

    /// A trace records the text of each host keymap, as the proxy maps
    /// it, after its event; none if its file holds less than the event
    /// states. The guest got the bytes the trace says it was sent.
    #[tokio::test]
    async fn a_keymap_is_recorded_from_its_descriptor() {
        use crate::protocols::wayland::wl_keyboard;
        use crate::proxy::harness::{default_keymap, Session};
        let mut s = Session::new();
        let tape = crate::test_support::trace::Tape::default();
        s.record(tape.clone());
        let (_, keyboard) = s.keyboard().await;
        let keymap = default_keymap();
        let (format, size) = (wl_keyboard::KEYMAP_FORMAT_XKB_V1, keymap.len() as u32);
        s.keymap_of(keyboard, format, &keymap[..10], size).await;
        s.guest_received();
        s.host_received();
        let trace = crate::test_support::trace::Trace::parse(&tape.text()).unwrap();
        let keymaps: Vec<_> = trace
            .records
            .iter()
            .filter_map(|record| record.keymap.clone())
            .collect();
        assert_eq!(keymaps, [keymap, Vec::new()]);
        // The guest got what the trace says it was sent.
        let (guest, _) = s.take_received_bytes();
        let sent: Vec<u8> = trace
            .of(Stream::ToGuest)
            .iter()
            .flat_map(|record| record.message.clone())
            .collect();
        assert_eq!(guest, sent);
    }
}
