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

//! Fuzz cases through `Client::run` over real sockets.
//!
//! The scheduled cases ([`super::world`]) dispatch one message at a time.
//! These run the proxy's event loop: a guest thread writes a case's
//! submissions as fast as the socket takes them, waiting only for its
//! roundtrips, while the proxy and a host thread playing the case's
//! [`Host`] answer at once. Which side the loop serves first, and how the
//! pieces arrive, is the kernel's to decide, so these cases check what
//! holds whatever the timing: the loop serves both sides until the guest
//! leaves, keeps the protocol with both (the host holds what it receives
//! to it, and so does the guest of a case whose requests are whole and
//! undamaged), and leaves no descriptor open.
//!
//! A damaged case's guest does not judge what it receives: it learns each
//! submission as it writes it, but the loop reads ahead, and can pair a
//! descriptor a damaged request lacks with a later one, which a correct
//! proxy may do.

use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags};

use super::host::Host;
use super::{Config, Submission};
use crate::connection::WaylandConnection;
use crate::protocols::wayland::{wl_callback, wl_display};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::Client;
use crate::sim::sched::Choices;
use crate::test_support::frames;
use crate::test_support::wire_peer::{MayEnd, Peer, Role};
use crate::wire::{Framing, MessageHeader, Outgoing, WL_DISPLAY_ID};

/// How long one case may take before it counts as a hang.
const CASE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the proxy may take to finish once it has closed the guest
/// connection.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// A case over real sockets: the guest's submissions, whether their framing
/// holds, the callback of their closing sync, and the host's.
pub(super) struct Exchange {
    pub config: Config,
    pub seed: u64,
    pub globals: Vec<(String, u32)>,
    pub sends: Vec<Submission>,
    pub framed: bool,
    pub callback: u32,
    /// The interface whose live objects the host ends on its own, and the
    /// guest objects the proxy may end on its own.
    pub destroys: Option<&'static str>,
    pub may_end: MayEnd,
    /// Whether the guest's requests are whole and undamaged, so that it
    /// holds what it receives to the protocol.
    pub checked: bool,
}

/// Serve the guest of `exchange` from one `Client` behind its host, and
/// return the events the guest received, or `None` if the proxy neither
/// answered the guest's closing sync nor closed the connection within
/// [`CASE_TIMEOUT`]. A stream whose framing is broken ends once it is
/// sent.
///
/// Panics if the proxy panics, does not finish after the guest leaves, or
/// sends either peer what it refuses.
pub(super) fn exchange(exchange: Exchange) -> Option<Vec<(u32, AnyEvent)>> {
    let deadline = Instant::now() + CASE_TIMEOUT;
    let (guest, proxy_guest) = UnixStream::pair().expect("guest socket pair");
    let (host, proxy_host) = UnixStream::pair().expect("host socket pair");
    let (finished, proxy_finished) = std::sync::mpsc::channel();
    let config = exchange.config;
    let proxy = std::thread::spawn(move || {
        serve(proxy_guest, proxy_host, config);
        let _ = finished.send(());
    });
    let host = {
        let (globals, destroys, seed) = (exchange.globals, exchange.destroys, exchange.seed);
        std::thread::spawn(move || answer(&host, globals, destroys, seed))
    };
    let mut peer = Peer::new(Role::Client);
    peer.server_may_end(exchange.may_end);
    let peer = Arc::new(Mutex::new(peer));
    let (callbacks_done, done_callbacks) = std::sync::mpsc::channel();
    let writer = {
        let guest = guest.try_clone().expect("clone the guest socket");
        let (peer, sends, framed) = (Arc::clone(&peer), exchange.sends, exchange.framed);
        std::thread::spawn(move || {
            send_all(&guest, &peer, sends, &done_callbacks);
            // A broken stream may hold a partial message that the closing
            // sync completes; end it rather than wait for an answer.
            if !framed {
                let _ = guest.shutdown(Shutdown::Write);
            }
        })
    };

    let callback = exchange.callback;
    let reader = Reader {
        peer: &peer,
        checked: exchange.checked,
    };
    let events = reader.read_until_closed(&guest, callback, deadline, callbacks_done);
    let events = events?;
    writer.join().expect("the guest writer");
    proxy_finished
        .recv_timeout(EXIT_GRACE)
        .expect("the proxy finishes once the guest has left");
    proxy.join().expect("the proxy does not panic");
    host.join().expect("the host accepts every request");
    Some(events)
}

/// Serve the guest on this thread, as the accept loop serves a client.
fn serve(guest: UnixStream, host: UnixStream, config: Config) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("proxy runtime");
    runtime.block_on(async {
        let client = Client::new(
            WaylandConnection::new(guest.into()).expect("register the guest socket"),
            WaylandConnection::new(host.into()).expect("register the host socket"),
            config.context(),
        );
        client.run().await;
    });
}

/// Answer the proxy on `host` as a case's [`Host`] does, which advertises
/// `globals`, ends live objects of `destroys` on its own, and draws its
/// choices from `seed`, until the proxy closes the connection.
fn answer(
    host: &UnixStream,
    globals: Vec<(String, u32)>,
    destroys: Option<&'static str>,
    seed: u64,
) {
    let mut model = Host::new(globals);
    model.ends(destroys);
    let mut choices = Choices::new(seed ^ 0xf022_f022);
    let start = Instant::now();
    let mut requests = Vec::new();
    let mut fds = VecDeque::new();
    while receive(host, &mut requests, &mut fds).expect("read host requests") > 0 {
        let mut parsed = 0;
        for (header, _) in frames(&requests) {
            let message = &requests[parsed..parsed + header.len];
            parsed += header.len;
            let (sender, request) = model
                .objects()
                .decode::<AnyRequest>(message, &mut fds)
                .unwrap_or_else(|error| panic!("the proxy sent the host {message:?}: {error}"));
            let time = u32::try_from(start.elapsed().as_millis()).expect("a short case");
            for event in model.answer(sender, &request, time, &mut choices) {
                // The proxy may already have closed the connection.
                let _ = send(host, event);
            }
        }
        assert!(
            !matches!(
                MessageHeader::frame(&requests[parsed..]),
                Framing::Invalid(_)
            ),
            "the proxy sent the host a header it cannot frame"
        );
        requests.drain(..parsed);
    }
}

/// Send each submission with its descriptors, which `peer` learns first,
/// waiting after a roundtrip for its callback among the `done_callbacks`
/// the guest receives, until the proxy goes away.
fn send_all(
    socket: &UnixStream,
    peer: &Mutex<Peer>,
    sends: Vec<Submission>,
    done_callbacks: &Receiver<u32>,
) {
    for Submission {
        bytes,
        fds,
        then_await,
    } in sends
    {
        let submission = Outgoing::from_parts(bytes, fds);
        peer.lock().expect("the guest's objects").send(&submission);
        if send(socket, submission).is_err() {
            return;
        }
        // The reader hangs up once the proxy has closed the connection.
        if then_await.is_some_and(|awaited| !done_callbacks.iter().any(|done| done == awaited)) {
            return;
        }
    }
}

/// Write `bytes` to `socket`, with `fds` on their first byte.
fn send(socket: &UnixStream, outgoing: Outgoing<u32>) -> io::Result<()> {
    let (bytes, fds) = outgoing.into_parts();
    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let rights = [ControlMessage::ScmRights(&raw)];
    let cmsgs: &[ControlMessage] = if raw.is_empty() { &[] } else { &rights };
    let iov = [IoSlice::new(&bytes)];
    let mut sent = sendmsg::<()>(
        socket.as_raw_fd(),
        &iov,
        cmsgs,
        MsgFlags::MSG_NOSIGNAL,
        None,
    )?;
    while sent < bytes.len() {
        sent += (&*socket).write(&bytes[sent..])?;
    }
    Ok(())
}

/// The guest's reader: its objects, and whether it holds what it receives
/// to the protocol.
struct Reader<'p> {
    peer: &'p Mutex<Peer>,
    checked: bool,
}

impl Reader<'_> {
    /// Read the guest's events, which the peer decodes, until the proxy
    /// closes the connection, or `None` once `deadline` passes.
    ///
    /// The guest stops sending once the closing sync (`callback`) is
    /// answered or an error arrives, as a client would. The sender of each
    /// event with `wl_callback.done`'s opcode goes to `done_callbacks`, for
    /// the writer to find the callback of a roundtrip among them; no other
    /// object has its ID.
    ///
    /// # Panics
    ///
    /// For a checked guest, on an event that breaks the protocol. An
    /// unchecked one keeps the events it can decode.
    fn read_until_closed(
        &self,
        guest: &UnixStream,
        callback: u32,
        deadline: Instant,
        done_callbacks: Sender<u32>,
    ) -> Option<Vec<(u32, AnyEvent)>> {
        let mut events = Vec::new();
        let mut bytes = Vec::new();
        let mut fds = VecDeque::new();
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            guest
                .set_read_timeout(Some(remaining.max(Duration::from_millis(1))))
                .expect("set the guest read timeout");
            match receive(guest, &mut bytes, &mut fds) {
                Ok(0) => return Some(events),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return None,
                Err(error) => panic!("read guest events: {error}"),
            }
            let mut parsed = 0;
            let mut finished = false;
            for (header, _) in frames(&bytes) {
                let message = &bytes[parsed..parsed + header.len];
                parsed += header.len;
                let event = (header.sender_id, header.opcode);
                if header.opcode == wl_callback::EVT_DONE {
                    // The writer is gone once it has sent everything.
                    let _ = done_callbacks.send(header.sender_id);
                }
                finished |= event == (callback, wl_callback::EVT_DONE)
                    || event == (WL_DISPLAY_ID, wl_display::EVT_ERROR);
                events.extend(self.decode(message, &mut fds));
            }
            bytes.drain(..parsed);
            if finished {
                let _ = guest.shutdown(Shutdown::Write);
            }
        }
    }

    /// `message`, decoded and followed by the peer: checked, for a checked
    /// guest; `None` if it does not decode for an unchecked one.
    fn decode(&self, message: &[u8], fds: &mut VecDeque<OwnedFd>) -> Option<(u32, AnyEvent)> {
        let mut peer = self.peer.lock().expect("the guest's objects");
        if self.checked {
            let event = peer.receive(message, fds);
            return Some(
                event.unwrap_or_else(|violation| panic!("the proxy sent the guest {violation}")),
            );
        }
        let (sender, event) = peer.decode::<AnyEvent>(message, fds).ok()?;
        let _ = peer.event(sender, &event);
        Some((sender, event))
    }
}

/// Receive once from `socket`, appending the bytes to `bytes` and the
/// descriptors that arrive to `fds`, in order; returns the byte count, 0
/// once the peer has closed.
fn receive(
    socket: &UnixStream,
    bytes: &mut Vec<u8>,
    fds: &mut VecDeque<OwnedFd>,
) -> io::Result<usize> {
    let mut buffer = [0u8; 4096];
    let mut control = nix::cmsg_space!([RawFd; 253]);
    let mut iov = [IoSliceMut::new(&mut buffer)];
    let message = match recvmsg::<()>(
        socket.as_raw_fd(),
        &mut iov,
        Some(&mut control),
        MsgFlags::MSG_CMSG_CLOEXEC,
    ) {
        Ok(message) => message,
        // The peer closed with requests of ours still unread.
        Err(nix::errno::Errno::ECONNRESET) => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let count = message.bytes;
    for cmsg in message.cmsgs().expect("control messages") {
        if let ControlMessageOwned::ScmRights(received) = cmsg {
            // SAFETY: the kernel installed each descriptor for this call
            // alone, so each is owned once.
            fds.extend(
                received
                    .into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    bytes.extend_from_slice(&buffer[..count]);
    Ok(count)
}
