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

//! Replaying a wire trace (`crate::trace`) through a whole proxy client,
//! so that a recorded host's traffic becomes a regression test.
//!
//! [`replay`] gives a [`Session`]'s proxy the trace's facts, has each
//! peer send it what the recorded proxy handled from that peer, a batch at
//! a time, and records the replay as the trace was recorded. It holds the
//! proxy to:
//!
//! - the protocol, with each peer, as every `Session` does
//!   (`test_support::wire_peer`), and no state left behind an ended object
//!   after any batch (`Context::assert_no_orphans`);
//! - an Exo mirror that never diverges from Exo: every event of Exo's
//!   agrees with the text the mirror predicts
//!   (`ime::take_divergences_for_test`);
//! - no delete or preedit region of Exo's that the IME core drops because
//!   its mirror cannot measure it (`ime::take_drops_for_test`): in a
//!   recorded scenario, each is a finding;
//! - the trace itself: the replay records every record of the trace, in
//!   its order, so the proxy handles each message as it did and sends each
//!   peer the same bytes, with as many descriptors in each message. Which
//!   descriptor a message carries is not compared: a trace holds none.
//!   The peers check the recorder: the bytes each got are the bytes it
//!   recorded.
//!
//! What a replay cannot reproduce it stands in for:
//!
//! - A keymap is the text the trace recorded of it. Any other descriptor
//!   is a large empty memfd, as a pool of zero pixels.
//! - Batches in one direction that sent nothing between them replay as
//!   one run, which `Session` cuts into batches as the proxy does. Where
//!   a batch ends shows only in what it sends (the proxy coalesces
//!   dma-buf feedback per batch), and these sent nothing.
//! - A VirtWL channel is a stand-in that allocates memfds and pipes
//!   (`VirtWaylandChannel::stand_in_for_test`), whose descriptors reach
//!   the host as the device's would.
//! - The other facts must be ones a test's proxy can have: no render
//!   node, placement, accelerators or `--gpu-accel`. A trace of other
//!   facts is refused, which names them.
//! - With data-control on, the proxy joins a clipboard hub of its own, so
//!   nothing reaches it from another connection: what another connection
//!   handed the recorded one (`handler::data_control`) is in no trace, and
//!   its output fails the replay.
//! - A host message the proxy did not track or could not read takes no
//!   descriptor, as it took none when recorded; a descriptor it left
//!   queued, which ends the session at the next message, is not in the
//!   trace, so that end replays otherwise.

use super::harness::Session;
use crate::handler::data_control::ClipboardHub;
use crate::protocols::wayland::wl_keyboard;
use crate::state::Context;
use crate::test_support::trace::{Record, Tape, Trace};
use crate::test_support::TestConnection;
use crate::trace::{Facts, Stream};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::Outgoing;
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::sync::Arc;

/// The size of the memfd that stands in for a descriptor other than a
/// keymap: larger than any pool a client draws in, and sparse.
const STAND_IN_SIZE: u64 = 1 << 30;

/// Replay `trace` through a proxy with its facts, on a runtime of its
/// own, as a DST case runs.
///
/// # Errors
///
/// Facts a test's proxy cannot have, a keymap that is not the size its
/// event states, a divergence of an Exo mirror, a delete or region the
/// IME core dropped, or the first record the replay does not repeat.
///
/// # Panics
///
/// When the proxy breaks the protocol with either peer, or leaves state
/// behind an ended object; or when the replay records other bytes than
/// the peers got, which is a bug of the recorder.
pub(crate) fn replay(trace: &Trace) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(replay_on(trace))
}

async fn replay_on(trace: &Trace) -> Result<(), String> {
    let mut s = Session::with_context(context(trace)?);
    let replayed = Tape::default();
    s.record(replayed.clone());
    crate::ime::take_divergences_for_test();
    crate::ime::take_drops_for_test();
    let mut run: Vec<Outgoing<u32>> = Vec::new();
    let mut from = Stream::FromGuest;
    let mut goes_on = true;
    for (at, record) in trace.records.iter().enumerate() {
        let handled = matches!(record.stream, Stream::FromGuest | Stream::FromHost);
        if !run.is_empty() && (!handled || from != record.stream) {
            goes_on = send(&mut s, from, std::mem::take(&mut run)).await;
        }
        if handled {
            if !goes_on {
                return Err(format!("the session ended before record {at}"));
            }
            from = record.stream;
            run.push(stand_ins(at, record)?);
        }
    }
    if !run.is_empty() {
        send(&mut s, from, run).await;
    }
    let divergences = crate::ime::take_divergences_for_test();
    if divergences > 0 {
        return Err(format!(
            "{divergences} of Exo's events disagreed with its mirror"
        ));
    }
    let drops = crate::ime::take_drops_for_test();
    if drops.total() > 0 {
        return Err(format!(
            "the IME core dropped deletes or regions of Exo's it could not measure: {drops:?}"
        ));
    }
    let replayed = Trace::parse(&replayed.text())?;
    let (guest, host) = s.take_received_bytes();
    for (stream, peer, got) in [
        (Stream::ToGuest, "guest", guest),
        (Stream::ToHost, "host", host),
    ] {
        let recorded: Vec<u8> = replayed
            .of(stream)
            .iter()
            .flat_map(|record| record.message.iter().copied())
            .collect();
        assert!(
            recorded == got,
            "the replay recorded other bytes than the {peer} got"
        );
    }
    let (trace, replayed) = (&trace.records, &replayed.records);
    match (0..trace.len().max(replayed.len())).find(|&at| trace.get(at) != replayed.get(at)) {
        Some(at) => Err(format!(
            "record {at} was {}, and is now {}",
            describe(trace.get(at)),
            describe(replayed.get(at))
        )),
        None => Ok(()),
    }
}

/// `record` as a trace writes it, but a keymap's text, which only its size
/// stands for.
fn describe(record: Option<&Record>) -> String {
    let Some(record) = record else {
        return "nothing".to_string();
    };
    let hex: String = record
        .message
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let keymap = record
        .keymap
        .as_ref()
        .map(|keymap| format!(", with a keymap of {} bytes", keymap.len()));
    format!(
        "{} {} {hex}{}",
        record.stream.tag(),
        record.fds,
        keymap.unwrap_or_default()
    )
}

/// A test's proxy with the facts of `trace`, which it states.
fn context(trace: &Trace) -> Result<Context, String> {
    let fact = |name| {
        trace
            .fact(name)
            .ok_or_else(|| format!("the trace states no {name}"))
    };
    let vm_id = fact("vm-id")?;
    let mut connection = TestConnection::default();
    let settings = &mut connection.settings;
    settings.xdg_decoration = fact("xdg-decoration")? == "yes";
    settings.vm_id =
        crate::trace::unescape(vm_id).ok_or_else(|| format!("{vm_id:?} is no VM name"))?;
    let facts = &mut connection.facts;
    facts.virtwl = match fact("host")? {
        "socket" => None,
        host => Some(Arc::new(
            VirtWaylandChannel::stand_in_for_test(host == "virtwl-dmabuf")
                .expect("a stand-in VirtWL channel"),
        )),
    };
    facts.clipboard = (trace.fact("data-control") == Some("on")).then(ClipboardHub::default);
    // A security context the replay commits goes nowhere.
    facts.sandbox_listeners = (trace.fact("security-context") == Some("on"))
        .then(|| crate::handler::security_context::SandboxListeners::new().0);
    facts.sandboxed = trace.fact("sandboxed") == Some("yes");
    let ctx = connection.context();
    let facts = Facts::of(&ctx).to_string();
    if facts != trace.facts {
        return Err(format!(
            "a test's proxy cannot have the facts {}; the nearest it has are {facts}",
            trace.facts
        ));
    }
    Ok(ctx)
}

/// `run`, messages that the peer of stream `from` sent, sent by that
/// peer; returns whether the session goes on.
async fn send(s: &mut Session, from: Stream, run: Vec<Outgoing<u32>>) -> bool {
    let goes_on = match from {
        Stream::FromGuest => s.try_guest(run).await,
        _ => s.try_host(run).await,
    };
    s.guest_received();
    s.host_received();
    goes_on
}

/// The message of `record`, the trace's record `at`, with as many
/// stand-ins as it carried descriptors: a keymap's own text, which must be
/// as long as its event states unless the proxy could not read it, or a
/// large empty memfd.
fn stand_ins(at: usize, record: &Record) -> Result<Outgoing<u32>, String> {
    let memfd = |bytes: &[u8], size: u64| {
        let file = crate::test_support::shm_pool_file(bytes);
        file.set_len(size).expect("size a stand-in");
        OwnedFd::from(file)
    };
    let message = record.message.clone();
    let Some(keymap) = &record.keymap else {
        let fds = (0..record.fds).map(|_| memfd(&[], STAND_IN_SIZE)).collect();
        return Ok(Outgoing::from_parts(message, fds));
    };
    let keymap_fd = || memfd(keymap, keymap.len() as u64);
    let mut fds = VecDeque::from([keymap_fd()]);
    let decoded = crate::test_support::decode(&message, &mut fds, wl_keyboard::Event::from_wire);
    let Ok((_, wl_keyboard::Event::Keymap { size, .. })) = decoded else {
        return Err(format!("record {at} has a keymap, but is no keymap event"));
    };
    if !keymap.is_empty() && keymap.len() != size as usize {
        return Err(format!(
            "record {at}'s keymap has {} bytes, and its event states {size}",
            keymap.len()
        ));
    }
    Ok(Outgoing::from_parts(message, vec![keymap_fd()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DST case's wire trace, which its proxy recorded behind the
    /// simulated Exo, replays: the proxy handles every message and sends
    /// each peer what it sent in the case, keeping the protocol, and its
    /// Exo mirrors agree with every event.
    #[test]
    fn a_dst_cases_wire_trace_replays() {
        for seed in [0, 1, 2, 3] {
            let trace = Trace::parse(&crate::sim::wire_trace(seed)).unwrap();
            let host_events = trace.of(Stream::FromHost).len();
            assert!(host_events > 50, "seed {seed}: {host_events} host events");
            replay(&trace).unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        }
    }

    /// A replay fails where the proxy no longer does what the trace
    /// recorded: a message it sends otherwise, or a fact it cannot have.
    #[test]
    fn a_replay_finds_what_the_proxy_no_longer_does() {
        let trace = Trace::parse(&crate::sim::wire_trace(0)).unwrap();
        let mut changed = trace.clone();
        let at = changed
            .records
            .iter()
            .rposition(|record| record.stream == Stream::ToGuest)
            .expect("the guest got something");
        let last = changed.records[at].message.len() - 1;
        changed.records[at].message[last] ^= 1;
        let why = replay(&changed).unwrap_err();
        assert!(why.starts_with(&format!("record {at} was")), "{why}");

        let mut other = trace.clone();
        other.facts = other.facts.replace("placement=off", "placement=on");
        let why = replay(&other).unwrap_err();
        assert!(why.contains("cannot have the facts"), "{why}");
    }

    /// A connection with data-control replays: its own peers' messages
    /// are in its trace.
    #[test]
    fn a_data_control_connections_trace_replays() {
        use crate::protocols::ext_data_control_v1::{
            ext_data_control_device_v1, ext_data_control_manager_v1,
        };
        use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
        let tape = Tape::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let mut connection = TestConnection::default();
            connection.facts.clipboard = Some(ClipboardHub::default());
            let mut s = Session::with_context(connection.context());
            s.record(tape.clone());
            let extension = zcr_keyboard_extension_v1::VERSION;
            let manager = s.bind(zcr_keyboard_extension_v1::NAME, extension).await;
            let seat = s.bind("wl_seat", 5).await;
            let device = s.new_id();
            let get = ext_data_control_manager_v1::req::get_data_device(manager, device, seat);
            s.guest([get]).await;
            let selection = ext_data_control_device_v1::Event::Selection { id: 0 };
            assert_eq!(s.guest_received(), [(device, selection.into())]);
        });
        let trace = Trace::parse(&tape.text()).unwrap();
        assert!(trace.facts.ends_with(" data-control=on"), "{}", trace.facts);
        replay(&trace).unwrap();
    }

    /// A keymap replays as the text the trace recorded of it, which must
    /// be as long as its event states; a keymap event the trace gives no
    /// text fails the replay, which records the text it reads.
    #[test]
    fn a_replay_gives_each_keymap_its_recorded_text() {
        let trace = Trace::parse(&crate::sim::wire_trace(0)).unwrap();
        let at = trace
            .records
            .iter()
            .position(|record| record.keymap.is_some())
            .expect("the host sent a keymap");
        let keymap = trace.records[at].keymap.as_ref().unwrap();
        assert_eq!(*keymap, super::super::harness::default_keymap());

        let mut short = trace.clone();
        short.records[at].keymap.as_mut().unwrap().pop();
        let why = replay(&short).unwrap_err();
        assert!(
            why.starts_with(&format!("record {at}'s keymap has")),
            "{why}"
        );

        let mut none = trace.clone();
        none.records[at].keymap = None;
        let why = replay(&none).unwrap_err();
        assert!(why.starts_with(&format!("record {at} was")), "{why}");
        let (was, now) = why.split_once(", and is now ").unwrap();
        assert!(
            !was.contains("keymap") && now.contains("with a keymap"),
            "{why}"
        );
    }

    /// A trace reads as the messages it holds, each decoded, typed text
    /// included, and the size of each keymap.
    #[test]
    fn a_trace_reads_as_the_messages_it_holds() {
        let trace = Trace::parse(&crate::sim::wire_trace(1)).unwrap();
        let lines = trace.decoded();
        let undecoded: Vec<_> = lines
            .iter()
            .filter(|line| {
                line.split(' ')
                    .nth(2)
                    .is_none_or(|sender| sender.starts_with('['))
            })
            .collect();
        assert_eq!(undecoded, Vec::<&String>::new());
        assert!(lines.iter().any(|line| line.contains("PreeditString")));
        assert!(lines.iter().any(|line| line.contains(" km a keymap of ")));
    }

    /// A message that ends the session is recorded, whether the proxy
    /// handled it or could not frame it, and replays.
    #[test]
    fn a_message_that_ends_the_session_replays() {
        use crate::protocols::wayland::wl_surface;
        let unframed = Outgoing::from_parts(vec![1, 0, 0, 0, 0, 0, 4, 0], Vec::new());
        for fatal in [wl_surface::req::commit(999), unframed] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            let text = runtime.block_on(async {
                let mut s = Session::new();
                let recorded = Tape::default();
                s.record(recorded.clone());
                s.guest_fatal([fatal]).await;
                s.guest_received_and_error();
                recorded.text()
            });
            let trace = Trace::parse(&text).unwrap();
            let streams: Vec<_> = trace.records.iter().map(|record| record.stream).collect();
            assert_eq!(streams, [Stream::FromGuest, Stream::ToGuest]);
            replay(&trace).unwrap();
        }
    }

    /// Host events are recorded as the proxy handled them, and replay so:
    /// those it cannot read from an object only it uses, which it drops;
    /// one from an object it does not track; and one it cannot read from
    /// an object the guest has, which ends the session.
    #[test]
    fn host_events_the_proxy_cannot_read_replay_as_handled() {
        use crate::protocols::aura_shell::zaura_shell;
        use crate::protocols::wayland::{wl_output, wl_registry};
        use crate::protocols::AnyRequest;
        /// A message from `sender` with `opcode` and `words` as its payload.
        fn raw(sender: u32, opcode: u16, words: &[u32]) -> Outgoing<u32> {
            let len = (8 + 4 * words.len()) as u32;
            let header = [sender, len << 16 | u32::from(opcode)];
            let bytes = header
                .iter()
                .chain(words)
                .flat_map(|word| word.to_le_bytes());
            Outgoing::from_parts(bytes.collect(), Vec::new())
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (text, unread) = runtime.block_on(async {
            let mut s = Session::new();
            let recorded = Tape::default();
            s.record(recorded.clone());
            s.advertise("zaura_shell", zaura_shell::EVT_LAYOUT_MODE_SINCE)
                .await;
            let shell = match &s.host_received()[..] {
                [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] => id.2,
                other => panic!("the proxy did not bind zaura_shell alone: {other:?}"),
            };
            s.guest_received();
            let dropped = [
                raw(shell, 99, &[]),
                zaura_shell::evt::bug_fix(shell, 1),
                raw(shell, zaura_shell::EVT_LAYOUT_MODE, &[1, 2]),
                raw(0xdead, 0, &[]),
            ];
            let mut unread: Vec<Vec<u8>> =
                dropped.iter().map(|event| event.bytes().to_vec()).collect();
            s.host(dropped).await;
            let output = s.bind_paired("wl_output", wl_output::VERSION).await;
            let fatal = raw(output.host, 99, &[]);
            unread.push(fatal.bytes().to_vec());
            s.host_fatal([fatal]).await;
            s.guest_received_and_error();
            s.host_received();
            (recorded.text(), unread)
        });
        let trace = Trace::parse(&text).unwrap();
        let handled: Vec<_> = trace
            .of(Stream::FromHost)
            .into_iter()
            .map(|record| &record.message)
            .collect();
        assert!(
            handled.ends_with(&unread[4..].iter().collect::<Vec<_>>()),
            "the session ends at the event the guest's object cannot read"
        );
        assert!(unread[..4].iter().all(|event| handled.contains(&event)));
        replay(&trace).unwrap();
    }

    /// A replay fails on an event of Exo's that the Exo mirror cannot
    /// agree with: here a preedit whose text to commit is not its text,
    /// which Exo never sends.
    #[test]
    fn a_replay_finds_an_exo_event_the_mirror_disagrees_with() {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::{self, Event};
        use crate::wire::IndexedString;
        let mut trace = Trace::parse(&crate::sim::wire_trace(1)).unwrap();
        let preedit = trace.records.iter_mut().find_map(|record| {
            let mut fds = std::collections::VecDeque::new();
            let decoded = crate::test_support::decode(&record.message, &mut fds, Event::from_wire);
            match decoded {
                Ok((
                    sender,
                    Event::PreeditString {
                        serial,
                        text: IndexedString::Utf8(text),
                        commit,
                    },
                )) if record.stream == Stream::FromHost && !text.is_empty() && commit == text => {
                    Some((record, sender, serial, text))
                }
                _ => None,
            }
        });
        let (record, sender, serial, text) = preedit.expect("Exo sent a preedit");
        record.message = zwp_text_input_v1::evt::preedit_string(sender, serial, &text, "x")
            .unwrap()
            .into_parts()
            .0;
        let why = replay(&trace).unwrap_err();
        assert_eq!(why, "1 of Exo's events disagreed with its mirror");
    }

    /// A trace recorded over a VirtWL channel that allocates dma-bufs
    /// replays over a stand-in: the proxy binds the host's linux-dmabuf for
    /// itself, and gives a SHM buffer the channel's shared memory.
    #[test]
    fn a_trace_over_a_virtwl_channel_replays() {
        use crate::protocols::wayland::wl_surface;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let text = runtime.block_on(async {
            let channel = VirtWaylandChannel::stand_in_for_test(true).unwrap();
            let mut connection = TestConnection::default();
            connection.facts.virtwl = Some(Arc::new(channel));
            let mut s = Session::with_context(connection.context());
            let recorded = Tape::default();
            s.record(recorded.clone());
            s.advertise("zwp_linux_dmabuf_v1", 3).await;
            s.guest_received();
            s.host_received();
            let shm = s.bind("wl_shm", 1).await;
            let pool = s
                .shm_pool(shm, crate::test_support::shm_pool_file(&[0; 16]))
                .await;
            let buffer = s.shm_buffer(pool, 2, 2).await;
            let surface = s.surface().await;
            s.guest([
                wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
                wl_surface::req::commit(surface.id),
            ])
            .await;
            s.guest_received();
            s.host_received();
            recorded.text()
        });
        let trace = Trace::parse(&text).unwrap();
        assert_eq!(trace.fact("host"), Some("virtwl-dmabuf"));
        let bound = |name: &[u8]| {
            trace.of(Stream::ToHost).iter().any(|record| {
                record
                    .message
                    .windows(name.len())
                    .any(|bytes| bytes == name)
            })
        };
        assert!(
            bound(b"zwp_linux_dmabuf_v1"),
            "the proxy bound linux-dmabuf"
        );
        replay(&trace).unwrap();
    }

    /// Every trace in the corpus (`traces/`) replays. A fact that cites
    /// a trace (`docs/IME.md`) is pinned here.
    #[test]
    fn every_recorded_trace_replays() {
        let corpus = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../traces"));
        let mut paths: Vec<_> = std::fs::read_dir(corpus)
            .expect("read traces/")
            .map(|entry| entry.expect("a corpus entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "trace")
            })
            .collect();
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path).expect("read a trace");
            let trace =
                Trace::parse(&text).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
            replay(&trace).unwrap_or_else(|why| panic!("{}: {why}", path.display()));
        }
    }
}
