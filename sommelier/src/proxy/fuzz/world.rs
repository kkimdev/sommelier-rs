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

//! A fuzz case's guest, proxy and host on one virtual clock.
//!
//! The guest's submissions, what the proxy sends each peer and the host's
//! events travel FIFO channels, each message after a drawn latency, on
//! the DST's scheduler ([`crate::sim::sched`]): the proxy gets one
//! submission or event at a time, and every draw, a latency or a choice of
//! the host's, comes from the case's seed. So the seed fixes the case's
//! timing as well as its input: how the pieces of the guest's stream and
//! the host's answers interleave, and which side the proxy serves first.
//!
//! The proxy is a [`Session`], whose two peers hold what it sends them to
//! the protocol (`test_support::wire_peer`), and the host a [`Host`].

use std::collections::VecDeque;

use super::host::Host;
use super::{Config, Counts, Submission};
use crate::protocols::wayland::{wl_callback, wl_display};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::Session;
use crate::sim::clock::Micros;
use crate::sim::sched::{earliest, Channel, Choices};
use crate::test_support::wire_peer::{MayEnd, SERVER_ID_START};
use crate::test_support::Rng;
use crate::wire::Outgoing;

/// The most messages a case delivers: one that goes on delivering hangs.
const MAX_DELIVERIES: usize = 100_000;

/// The channels' longest latencies, as the DST's.
const GUEST_LATENCY: Micros = Micros(2_000);
const HOST_LATENCY: Micros = Micros(3_000);

/// What reaching the guest means to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Note {
    /// `wl_callback.done` of this callback.
    Done(u32),
    /// `wl_display.error`.
    Error,
    Other,
}

/// One case's guest, proxy and host; see the module documentation.
pub(super) struct World<'t> {
    now: Micros,
    choices: Choices,
    session: Session,
    host: Host,
    c_to_p: Channel<Outgoing<u32>>,
    p_to_c: Channel<Note>,
    p_to_e: Channel<(u32, AnyRequest)>,
    e_to_p: Channel<Outgoing<u32>>,
    /// The guest's submissions it has yet to send, and the roundtrip
    /// whose `done` it waits for before it sends more.
    stream: VecDeque<Submission>,
    awaiting: Option<u32>,
    /// The guest stopped sending: it got its closing sync's `done`, or an
    /// error.
    closed: bool,
    /// The callback of the guest's closing sync.
    closing: Option<u32>,
    /// What the proxy sent the guest.
    received: Vec<(u32, AnyEvent)>,
    /// The proxy ended the session.
    ended: bool,
    /// The server objects the proxy created for the guest.
    guest_server_objects: u64,
    /// A line per message delivered, if the world keeps a trace.
    trace: Option<&'t mut Vec<String>>,
}

impl<'t> World<'t> {
    /// A world whose proxy runs as `config` says, its every draw from
    /// `seed`, behind a host that advertises `globals` ([`Host::new`]),
    /// with its trace in `trace`, if any, which a panic leaves there.
    pub fn new(
        config: Config,
        globals: Vec<(String, u32)>,
        seed: u64,
        trace: Option<&'t mut Vec<String>>,
    ) -> Self {
        let mut session = Session::with_context(config.context());
        // The proxy receives the case's bytes in pieces of a size the seed
        // draws apart from the schedule's choices.
        session.receive_in(1 + Rng::new(seed ^ 0x9ece_9ece).below(4096));
        Self {
            now: Micros(0),
            choices: Choices::new(seed ^ 0xf022_f022),
            session,
            host: Host::new(globals),
            c_to_p: Channel::new(GUEST_LATENCY),
            p_to_c: Channel::new(GUEST_LATENCY),
            p_to_e: Channel::new(HOST_LATENCY),
            e_to_p: Channel::new(HOST_LATENCY),
            stream: VecDeque::new(),
            awaiting: None,
            closed: false,
            closing: None,
            received: Vec::new(),
            ended: false,
            guest_server_objects: 0,
            trace,
        }
    }

    /// The host ends live objects of `interface` on its own, if any, and
    /// the proxy may end on its own the guest objects for which `may_end`
    /// holds ([`Session::host_may_end`]).
    pub fn host_ends(&mut self, interface: Option<&'static str>, may_end: MayEnd) {
        self.host.ends(interface);
        self.session.host_may_end(may_end);
    }

    /// The guest sends `sends`, and stops once it gets the `done` of
    /// `closing` or an error.
    pub fn send(&mut self, sends: Vec<Submission>, closing: u32) {
        self.stream.extend(sends);
        self.closing = Some(closing);
        self.closed = false;
    }

    /// What the proxy sent the guest, its sender's ID first.
    pub fn received(&self) -> &[(u32, AnyEvent)] {
        &self.received
    }

    /// What the case counted so far.
    pub fn counts(&self) -> Counts {
        Counts {
            guest_server_objects: self.guest_server_objects,
            ..self.host.counts()
        }
    }

    /// Deliver until every channel is empty, the guest has sent all it
    /// can and the host holds nothing back.
    ///
    /// # Panics
    ///
    /// After [`MAX_DELIVERIES`]: the proxy, or the host, goes on forever.
    pub async fn run(&mut self) {
        self.guest_sends();
        for _ in 0..MAX_DELIVERIES {
            let ready = [
                self.c_to_p.ready_at(),
                self.p_to_c.ready_at(),
                self.p_to_e.ready_at(),
                self.e_to_p.ready_at(),
            ];
            let Some(source) = earliest(&ready, &mut self.choices) else {
                // Nothing else releases what the host holds back.
                let held = self.host.release();
                if held.is_empty() {
                    return;
                }
                self.host_sends(held);
                continue;
            };
            self.now = self.now.max(ready[source].expect("a ready source"));
            match source {
                0 => {
                    let submission = self.c_to_p.take();
                    if !self.ended {
                        let (bytes, fds) = (submission.bytes().len(), submission.fds().len());
                        self.log(|| format!("C→P {bytes} bytes, {fds} fds"));
                        let goes_on = self.session.try_guest([submission]).await;
                        self.proxy_sent(goes_on);
                    }
                }
                1 => {
                    let note = self.p_to_c.take();
                    self.guest_gets(note);
                }
                2 => {
                    let (sender, request) = self.p_to_e.take();
                    let time = u32::try_from(self.now.0 / 1_000).expect("a short case");
                    let answer = self.host.answer(sender, &request, time, &mut self.choices);
                    self.host_sends(answer);
                }
                _ => {
                    let event = self.e_to_p.take();
                    if !self.ended {
                        if self.trace.is_some() {
                            let (sender, decoded) = self.session.decode_host_event(&event);
                            self.log(|| format!("E→P {sender} {}", shown(&decoded)));
                        }
                        let goes_on = self.session.try_host([event]).await;
                        self.proxy_sent(goes_on);
                    }
                }
            }
        }
        panic!("the case did not settle within {MAX_DELIVERIES} deliveries: it hangs");
    }

    /// The guest sends what it can: up to a roundtrip, whose `done` it
    /// waits for, unless it has stopped.
    fn guest_sends(&mut self) {
        while !self.closed && self.awaiting.is_none() {
            let Some(submission) = self.stream.pop_front() else {
                return;
            };
            self.awaiting = submission.then_await;
            let sent = Outgoing::from_parts(submission.bytes, submission.fds);
            self.c_to_p.send(self.now, sent, &mut self.choices);
        }
    }

    /// An event reaches the guest.
    fn guest_gets(&mut self, note: Note) {
        match note {
            Note::Done(callback) => {
                if self.awaiting == Some(callback) {
                    self.awaiting = None;
                }
                self.closed |= self.closing == Some(callback);
            }
            Note::Error => self.closed = true,
            Note::Other => {}
        }
        self.guest_sends();
    }

    /// What the proxy sent goes out on P→E and P→C.
    fn proxy_sent(&mut self, goes_on: bool) {
        for (sender, request) in self.session.host_received() {
            self.log(|| format!("P→E {sender} {}", shown(&request)));
            let sent = (sender, request);
            self.p_to_e.send(self.now, sent, &mut self.choices);
        }
        assert!(
            !self.session.parked(),
            "a request waits for a fence, which no case can signal"
        );
        for (sender, event) in self.session.guest_received() {
            self.log(|| format!("P→C {sender} {}", shown(&event)));
            let note = match event {
                AnyEvent::WlCallback(wl_callback::Event::Done { .. }) => Note::Done(sender),
                AnyEvent::WlDisplay(wl_display::Event::Error { .. }) => Note::Error,
                _ => Note::Other,
            };
            if event
                .new_object()
                .is_some_and(|(id, _)| id >= SERVER_ID_START)
            {
                self.guest_server_objects += 1;
            }
            self.p_to_c.send(self.now, note, &mut self.choices);
            self.received.push((sender, event));
        }
        self.ended |= !goes_on;
    }

    /// The host sends `events` on E→P.
    fn host_sends(&mut self, events: Vec<Outgoing<u32>>) {
        for event in events {
            self.e_to_p.send(self.now, event, &mut self.choices);
        }
    }

    /// Trace `line`, if the world keeps a trace.
    fn log(&mut self, line: impl FnOnce() -> String) {
        if let Some(trace) = &mut self.trace {
            let ms = self.now.0 as f64 / 1_000.0;
            trace.push(format!("{ms:10.3} {}", line()));
        }
    }
}

/// `message` as a trace shows it: without its descriptors' numbers, which
/// a replay need not repeat, and a long one cut short.
fn shown(message: &impl std::fmt::Debug) -> String {
    let mut text = format!("{message:?}");
    while let Some(at) = text.find("OwnedFd { fd: ") {
        let end = text[at..].find('}').map_or(text.len(), |end| at + end + 1);
        text.replace_range(at..end, "OwnedFd");
    }
    if text.len() > 200 {
        let mut end = 200;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}
