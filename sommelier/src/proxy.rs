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

use crate::allocator::{PendingFence, VirtGpuNode};
use crate::client_kind;
use crate::connection::{LastSubmission, OutgoingMessages, WaylandConnection};
use crate::handler::data_control::{ClipboardHub, Mailbox};
use crate::handler::security_context::{SandboxListener, SandboxListeners};
use crate::placement_config::{self, PlacementConfig};
use crate::settings::{HostTransport, Settings};
use crate::state::{ConnectionFacts, Context, GuestId, HostId, SessionEnd, Wait};
use crate::trace::{Recorder, Stream, Traces};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::warn_once::WarnOnce;
use crate::wire::{Framing, MessageHeader, ProtocolError};
use std::io;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

mod dispatch;
#[cfg(test)]
mod frame_cost;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
pub(crate) mod harness;
#[cfg(test)]
mod replay;

// A frame can generate many small Wayland requests (damage, attach, commit,
// callbacks, and buffer bookkeeping).  Keep one client from processing an
// unbounded stream of those requests without returning to the executor:
// keyboard/text-input events arrive on the opposite direction and otherwise
// wait behind the entire render burst on the current-thread runtime.
// Keep the fairness quantum short enough that a render burst cannot hold a
// keyboard or text-input event behind several frames.  Ghostty can emit a
// dozen small requests per frame at 4K; 32 messages bound the opposite
// direction to roughly one frame of work. A batch's output may wait for the
// next ones' in the same direction (see `Client::flush`), so the quantum
// costs no send of its own.
const MAX_MESSAGES_PER_BATCH: usize = 32;

/// The most batches whose output waits unsent: before a further batch in
/// the same direction, `Client::handle_msgs` sends what they produced. A
/// batch in the other direction sends it first whatever the count, so no
/// output waits behind the other peer's stream, and a long one-way stream
/// still sends, and so awaits, every few batches.
const MAX_BATCHES_PER_FLUSH: usize = 4;

/// How long the accept loop waits after a failed accept.
///
/// A failed accept such as EMFILE or ENFILE leaves the pending connection
/// queued, so the listener stays readable and an immediate retry fails
/// again: without a pause the loop spins on the one thread every client
/// shares. Descriptors come back as clients close, which takes far longer
/// than this pause, while a new client waits at most this long.
const ACCEPT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// How soon a parked request whose fence gives no descriptor to wait on
/// (the virtio-gpu fallback) is first dispatched again to ask once more.
/// Each later ask waits twice as long, up to [`MAX_FENCE_POLL_INTERVAL`]:
/// an ask costs a dma-buf import, a wait and a GEM close, and the fence may
/// take seconds.
const FENCE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// The longest pause between two asks about a fallback fence: about a
/// frame, so a fence that signals late delays its commit by at most that.
const MAX_FENCE_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    ClientToHost,
    HostToClient,
}

impl Direction {
    /// The trace stream of the messages the proxy handles in this
    /// direction.
    fn handled(self) -> Stream {
        match self {
            Self::ClientToHost => Stream::FromGuest,
            Self::HostToClient => Stream::FromHost,
        }
    }
}

/// A guest request that waits. It stays first in the guest stream's
/// buffer, and the guest stream is neither read nor dispatched until the
/// request is dispatched again.
enum Parked {
    /// It waits for a fence ([`Context::wait_for_fence`]).
    Fence(ParkedRequest),
    /// It waits for the guest's globals ([`Context::wait_for_globals`]),
    /// which only host events settle: the host batch that settles them
    /// releases it (`Client::handle_msgs`).
    Globals,
}

impl Parked {
    /// Resolve when `parked` is due to be dispatched again on its own, as a
    /// fence's request is ([`ParkedRequest::due`]). Never resolves for
    /// none, or for one that a host batch releases.
    async fn due(parked: Option<&Self>) {
        match parked {
            Some(Self::Fence(request)) => request.due().await,
            Some(Self::Globals) | None => std::future::pending().await,
        }
    }
}

/// A guest request that waits for a fence ([`Context::wait_for_fence`]).
struct ParkedRequest {
    fence: ParkedFence,
    /// When the session ends if the fence has not signalled. Dispatching
    /// the request again keeps it.
    deadline: Instant,
    /// How long after parking a polled fence is asked about again.
    poll_interval: Duration,
}

/// What a parked request waits on.
enum ParkedFence {
    /// A sync_file, readable once the fence signals.
    Readable(AsyncFd<OwnedFd>),
    /// Nothing: the request is dispatched again at this instant to ask.
    /// It is absolute, so host traffic that wakes the loop before then
    /// does not postpone it.
    PollAt(Instant),
}

/// Whether a sync_file has failed to register with the event loop yet.
/// Only the first failure warns, so a kernel that refuses every one does
/// not flood the log.
static FENCE_REGISTRATION_FAILED: WarnOnce = WarnOnce::new();

impl ParkedRequest {
    /// Park a request that waits for the first time.
    fn new(fence: PendingFence, deadline: Instant) -> Self {
        Self::with(fence, deadline, FENCE_POLL_INTERVAL)
    }

    /// Park the request this one parked again, as it still waits: it keeps
    /// its deadline, and a polled fence is asked about less often.
    fn wait_again(self, fence: PendingFence) -> Self {
        let poll_interval = (self.poll_interval * 2).min(MAX_FENCE_POLL_INTERVAL);
        Self::with(fence, self.deadline, poll_interval)
    }

    fn with(fence: PendingFence, deadline: Instant, poll_interval: Duration) -> Self {
        let poll_at = || ParkedFence::PollAt(Instant::now() + poll_interval);
        let fence = match fence {
            PendingFence::SyncFile(sync_file) => {
                match AsyncFd::with_interest(sync_file, Interest::READABLE) {
                    Ok(sync_file) => ParkedFence::Readable(sync_file),
                    Err(error) => {
                        log::log!(
                            FENCE_REGISTRATION_FAILED.level(),
                            "Polling a dma-buf fence that cannot be awaited: {error}"
                        );
                        poll_at()
                    }
                }
            }
            PendingFence::Busy => poll_at(),
        };
        Self {
            fence,
            deadline,
            poll_interval,
        }
    }

    /// Resolve when the request is due to be dispatched again: its fence
    /// may have signalled, or its deadline has passed.
    async fn due(&self) {
        let fence = async {
            match &self.fence {
                // An error, like readiness, sends the request back to its
                // fence check, which tells them apart.
                ParkedFence::Readable(fence) => {
                    let _ = fence.readable().await;
                }
                ParkedFence::PollAt(at) => tokio::time::sleep_until(*at).await,
            }
        };
        tokio::select! {
            () = fence => {}
            () = tokio::time::sleep_until(self.deadline) => {}
        }
    }
}

struct Client {
    client_conn: WaylandConnection,
    host_conn: WaylandConnection,
    ctx: Context,
    /// Whether a batch ended the session: one of its messages did, or a send
    /// failed. No later batch dispatches anything, so the guest gets at most
    /// one error. A failed receive or a closed peer stops `run` without it.
    ended: bool,
    /// The guest request that waits, if one does.
    parked: Option<Parked>,
    /// What the batches dispatched so far send each peer and have not sent
    /// yet, in the order they produced it (see [`Self::flush`]).
    to_host: OutgoingMessages<HostId>,
    to_guest: OutgoingMessages<GuestId>,
    /// The direction of the batches whose output is pending, and how many
    /// there are; the next batch sends it first if it goes the other way
    /// or would make more than [`MAX_BATCHES_PER_FLUSH`].
    pending_from: Option<Direction>,
    pending_batches: usize,
    /// Where the connection's traffic is recorded, if it is
    /// (`SOMMELIER_TRACE_DIR`): each message `handle_msgs` handles, and
    /// what each batch gives the peers.
    trace: Option<Recorder>,
}

impl Client {
    fn new(client_conn: WaylandConnection, host_conn: WaylandConnection, mut ctx: Context) -> Self {
        ctx.shadow_table.create_display();
        Self {
            client_conn,
            host_conn,
            ctx,
            ended: false,
            parked: None,
            to_host: OutgoingMessages::default(),
            to_guest: OutgoingMessages::default(),
            pending_from: None,
            pending_batches: 0,
            trace: None,
        }
    }

    /// The client, recording its traffic with `trace`.
    fn recorded(self, trace: Option<Recorder>) -> Self {
        Self { trace, ..self }
    }

    async fn run(mut self) {
        // When both streams already have complete messages buffered, alternate
        // directions. This gives host input (keyboard/IME) a bounded wait even
        // while a large client render burst is being drained.
        let mut prefer_host = true;
        let mailbox = self.ctx.data_control.mailbox();
        loop {
            let buffered_direction = match self.next_buffered_direction(prefer_host) {
                Ok(direction) => direction,
                Err(error) => {
                    log::debug!("Wayland connection closed during fairness probe: {}", error);
                    break;
                }
            };
            if let Some(direction) = buffered_direction {
                if !self.handle_msgs(direction).await {
                    break;
                }
                prefer_host = matches!(direction, Direction::ClientToHost);
                continue;
            }

            // Everything received is dispatched: send what it produced
            // before waiting for more.
            if !self.flush().await {
                break;
            }
            tokio::select! {
                res = self.client_conn.recv(), if self.parked.is_none() => {
                    match res {
                        Ok(bytes) => {
                            if bytes == 0 { break; }
                            if !self.handle_msgs(Direction::ClientToHost).await { break; }
                            prefer_host = true;
                        }
                        Err(_) => break,
                    }
                }
                res = self.host_conn.recv() => {
                    match res {
                        Ok(bytes) => {
                            if bytes == 0 { break; }
                            if !self.handle_msgs(Direction::HostToClient).await { break; }
                            prefer_host = false;
                        }
                        Err(_) => break,
                    }
                }
                () = Parked::due(self.parked.as_ref()) => {
                    if !self.handle_msgs(Direction::ClientToHost).await { break; }
                    prefer_host = true;
                }
                () = Mailbox::ready(mailbox.as_deref()) => {
                    if !self.handle_deliveries().await { break; }
                }
            }
        }

        // A session that goes on until a peer closes may still hold output
        // for the other one.
        self.flush().await;

        // Clipboard receive pumps are tied to this client connection. Stop
        // them before dropping the connection so an unfinished VirtWL transfer
        // cannot keep its duplicated descriptors or task alive after either
        // endpoint disconnects.
        self.ctx.clipboard_pumps.stop().await;
    }

    /// Choose the next buffered direction while opportunistically receiving
    /// from the preferred transport.
    ///
    /// A bounded dispatch batch only improves fairness for events already in
    /// `read_buf`. Under a continuous client render burst, host keyboard/IME
    /// bytes may still be waiting in the VirtWL kernel queue. Probe the
    /// preferred side once before consuming another fallback batch so input
    /// latency is bounded by one batch rather than the entire buffered burst.
    fn next_buffered_direction(&mut self, prefer_host: bool) -> io::Result<Option<Direction>> {
        // A parked guest stream has nothing to dispatch until its request
        // is due (see `run`).
        let guest_parked = self.parked.is_some();
        let (preferred_direction, fallback_direction) = if prefer_host || guest_parked {
            (Direction::HostToClient, Direction::ClientToHost)
        } else {
            (Direction::ClientToHost, Direction::HostToClient)
        };

        if self.connection(preferred_direction).has_complete_message() {
            return Ok(Some(preferred_direction));
        }
        if guest_parked || !self.connection(fallback_direction).has_complete_message() {
            return Ok(None);
        }

        if self
            .connection_mut(preferred_direction)
            .try_recv()?
            .is_some_and(|bytes| bytes == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "preferred Wayland transport closed",
            ));
        }
        if self.connection(preferred_direction).has_complete_message() {
            Ok(Some(preferred_direction))
        } else {
            Ok(Some(fallback_direction))
        }
    }

    fn connection(&self, direction: Direction) -> &WaylandConnection {
        match direction {
            Direction::ClientToHost => &self.client_conn,
            Direction::HostToClient => &self.host_conn,
        }
    }

    fn connection_mut(&mut self, direction: Direction) -> &mut WaylandConnection {
        match direction {
            Direction::ClientToHost => &mut self.client_conn,
            Direction::HostToClient => &mut self.host_conn,
        }
    }

    async fn handle_msgs(&mut self, direction: Direction) -> bool {
        if self.ended {
            return false;
        }
        // What earlier batches produced goes out before a batch in the other
        // direction, so a key event, say, never waits behind the guest's
        // render requests, and after a few batches in the same one.
        let other_direction = self.pending_from.is_some_and(|from| from != direction);
        if (other_direction || self.pending_batches >= MAX_BATCHES_PER_FLUSH) && !self.flush().await
        {
            return false;
        }
        // A parked request is the first in the guest stream (see
        // `ParkedRequest::wait_again`).
        let resumed = match direction {
            Direction::ClientToHost => self.parked.take(),
            Direction::HostToClient => None,
        };
        let (conn, other_conn) = match direction {
            Direction::ClientToHost => (&mut self.client_conn, &mut self.host_conn),
            Direction::HostToClient => (&mut self.host_conn, &mut self.client_conn),
        };
        // What this batch gives each peer starts here.
        let marks = (self.to_host.len(), self.to_guest.len());

        // Preserve the ordered Wayland byte/FD stream while translating
        // messages. A single transport submission may contain several
        // messages; Wayland associates received descriptors by order, not by
        // an independently addressable byte offset. VirtWL's send path keeps
        // that same order when it chunks a large byte stream. The batch
        // appends to what earlier batches left for each peer.
        // A message's forward goes to its direction's peer, and its effects
        // to the peer whose IDs they carry, so each peer gets its share in
        // the order the messages produced it.
        let mut end = None;
        let mut offset: usize = 0;
        let mut processed_messages = 0;
        // A complete untracked message cannot tell us whether descriptors
        // currently queued on the ordered stream belong to it or to a later
        // message. Retain them while the next message is partial (the bytes
        // may still complete the descriptor-owning message), but once another
        // complete message is available the association is irrecoverably
        // ambiguous and the connection must be torn down.
        loop {
            let MessageHeader {
                sender_id,
                opcode,
                len,
            } = match MessageHeader::frame(&conn.read_buf[offset..]) {
                Framing::Partial => break,
                Framing::Invalid(header) => {
                    log::error!("Invalid message length: {}", header.len);
                    if let Some(trace) = &mut self.trace {
                        let unframed = &conn.read_buf[offset..];
                        trace.record(direction.handled(), unframed, conn.read_fds.len());
                    }
                    // libwayland-server reports a length under 8 or over
                    // 4096 the same way (the alignment check is the proxy's
                    // own); the stream cannot be framed past it.
                    end = Some(dispatch::rejection(
                        direction,
                        &ProtocolError::InvalidMessageLength(header.len),
                    ));
                    break;
                }
                Framing::Complete(header) => header,
            };
            let message_end = offset + len;

            if conn.ambiguous_untracked_fd {
                log::error!(
                    "Ambiguous SCM_RIGHTS ordering after a complete untracked message; \
                     terminating connection before assigning descriptors to another message"
                );
                // Only host events can be untracked: a guest request from an
                // unknown sender is an error. The events before this one were
                // valid and reach the guest ahead of its one error.
                if let Some(trace) = &mut self.trace {
                    let message = &conn.read_buf[offset..message_end];
                    trace.record(direction.handled(), message, 0);
                }
                end = Some(SessionEnd::internal());
                break;
            }

            let packet = &conn.read_buf[offset..message_end];
            let frame = dispatch::Frame {
                direction,
                header: MessageHeader {
                    sender_id,
                    opcode,
                    len,
                },
                payload: &packet[MessageHeader::LEN..],
                fds: &mut conn.read_fds,
            };
            let mut out = dispatch::Outbox {
                to_host: &mut self.to_host,
                to_guest: &mut self.to_guest,
            };
            let report = dispatch::dispatch(&mut self.ctx, frame, &mut out, self.trace.is_some());
            conn.ambiguous_untracked_fd |= report.ambiguous_fds;
            // A message is recorded once it is handled, or ends the
            // session: one that waits for a fence is handled again.
            if let Some(trace) = &mut self.trace {
                if !matches!(report.dispatched, dispatch::Dispatched::Waits(_)) {
                    trace.record(direction.handled(), packet, report.fds);
                }
                if let Some(keymap) = &report.keymap {
                    trace.keymap(keymap);
                }
            }
            match report.dispatched {
                dispatch::Dispatched::Handled => {}
                dispatch::Dispatched::Ends(session_end) => {
                    end = Some(session_end);
                    break;
                }
                // A message that waits changed nothing, so it stays
                // buffered with everything after it, and is dispatched
                // again when it is due. Nothing overtakes it.
                dispatch::Dispatched::Waits(Wait::Fence(wait)) => {
                    let now = Instant::now();
                    let timeout = wait.timeout;
                    let parked = match resumed {
                        Some(Parked::Fence(parked)) if offset == 0 => parked.wait_again(wait.fence),
                        _ => ParkedRequest::new(wait.fence, now + timeout),
                    };
                    if now >= parked.deadline {
                        if let Some(trace) = &mut self.trace {
                            trace.record(direction.handled(), packet, 0);
                        }
                        log::error!(
                            "A dma-buf fence did not signal within {timeout:?} for a request \
                             from guest object {sender_id}; ending the session"
                        );
                        end = Some(SessionEnd::internal());
                    } else {
                        self.parked = Some(Parked::Fence(parked));
                    }
                    break;
                }
                dispatch::Dispatched::Waits(Wait::Globals) => {
                    self.parked = Some(Parked::Globals);
                    break;
                }
            }

            offset = message_end;
            processed_messages += 1;
            if processed_messages >= MAX_MESSAGES_PER_BATCH {
                break;
            }
        }

        // A session that ends gets its one error after the valid messages.
        if let Some(end) = &end {
            self.to_guest
                .push(crate::handler::display::session_error(end));
        }
        if let Some(trace) = &mut self.trace {
            trace.record_since(Stream::ToHost, &self.to_host, marks.0);
            trace.record_since(Stream::ToGuest, &self.to_guest, marks.1);
            trace.flush();
        }

        // Host events may have settled the globals a parked sync waits for:
        // the guest's stream goes on, from the sync.
        if matches!(self.parked, Some(Parked::Globals))
            && crate::handler::registry::globals_settled(&self.ctx)
        {
            self.parked = None;
        }
        self.ctx.end_batch();
        // A session that ends rolls nothing back, so in tests every batch
        // that keeps it must leave no state behind an ended object.
        #[cfg(test)]
        if end.is_none() {
            crate::test_support::counting::uncounted(|| self.ctx.assert_no_orphans());
        }

        // A send passes the peer copies of the descriptors, which the
        // pending output then closes; a failed send drops it all. Only full
        // submissions go out while the session goes on: later batches may
        // fill the last one, which `flush` sends before the proxy waits. A
        // session that ends sends everything, its error last.
        let last = if end.is_none() {
            LastSubmission::Keep
        } else {
            LastSubmission::Send
        };
        // The forwards go out first. The reverse output goes back through
        // the source connection, apart from them, so a forwarded
        // wl_keyboard.key may reach the guest before or after its ack_key
        // reaches the host. Either is safe: Exo holds the key in
        // pending_key_acks_ with a 1000 ms TTL, so the ack always arrives
        // well within the window. The C sommelier sends the key first.
        let sent = match direction {
            Direction::ClientToHost => {
                let to_host = other_conn.send_submissions(&mut self.to_host, last).await;
                (
                    to_host,
                    conn.send_submissions(&mut self.to_guest, last).await,
                )
            }
            Direction::HostToClient => {
                let to_guest = other_conn.send_submissions(&mut self.to_guest, last).await;
                (
                    conn.send_submissions(&mut self.to_host, last).await,
                    to_guest,
                )
            }
        };
        // A failed send drops this client connection (see `handle_msgs`).
        let success = end.is_none() && sent.0.is_ok() && sent.1.is_ok();

        conn.read_buf.drain(..offset);
        self.ended = !success;
        self.pending_from = Some(direction);
        self.pending_batches += 1;
        success
    }

    /// Handle what other connections delivered through the clipboard hub
    /// (`handler::data_control`), as one batch of its own, and send what it
    /// gives each peer. Returns whether the session goes on.
    async fn handle_deliveries(&mut self) -> bool {
        let Some(mailbox) = self.ctx.data_control.mailbox() else {
            return true;
        };
        if self.ended || !self.flush().await {
            return false;
        }
        let mut out = dispatch::Outbox {
            to_host: &mut self.to_host,
            to_guest: &mut self.to_guest,
        };
        let mut end = None;
        while let Some(delivery) = mailbox.take() {
            end = dispatch::deliver(&mut self.ctx, delivery, &mut out);
            if end.is_some() {
                break;
            }
        }
        if let Some(end) = &end {
            self.to_guest
                .push(crate::handler::display::session_error(end));
        }
        // A delivery is no message of either peer's, so a trace records
        // only what it sends; a replay cannot repeat it, and refuses a
        // trace of a connection with data-control (`proxy::replay`).
        if let Some(trace) = &mut self.trace {
            trace.record_since(Stream::ToHost, &self.to_host, 0);
            trace.record_since(Stream::ToGuest, &self.to_guest, 0);
            trace.flush();
        }
        self.ctx.end_batch();
        #[cfg(test)]
        if end.is_none() {
            crate::test_support::counting::uncounted(|| self.ctx.assert_no_orphans());
        }
        let sent = self.flush().await;
        self.ended |= end.is_some();
        sent && end.is_none()
    }

    /// Send all the output the batches dispatched so far left for either
    /// peer, as `run` does before it waits and when the session ends, and
    /// `handle_msgs` before a batch in the other direction or after
    /// [`MAX_BATCHES_PER_FLUSH`]; the peers' streams are independent, so
    /// only each one's order matters. Returns whether the session goes on:
    /// a failed send ends it.
    async fn flush(&mut self) -> bool {
        self.pending_from = None;
        self.pending_batches = 0;
        let to_host = self.host_conn.send_messages(&mut self.to_host).await;
        let to_guest = self.client_conn.send_messages(&mut self.to_guest).await;
        if to_host.is_err() || to_guest.is_err() {
            self.ended = true;
        }
        !self.ended
    }
}

/// What every client connection shares: the process's settings and the
/// resources built from them once.
struct Shared {
    settings: Rc<Settings>,
    virtgpu: VirtGpuNode,
    placement: PlacementConfig,
    /// Where each connection's traffic is recorded, if it is.
    traces: Option<Traces>,
    /// The clipboard state the connections share, with data-control on.
    clipboard: Option<ClipboardHub>,
    /// Where a connection hands the guest's committed security contexts,
    /// whose sockets `serve` accepts sandboxed clients on.
    sandbox_listeners: SandboxListeners,
}

impl Shared {
    /// Build what the connections share from `settings`, and start
    /// watching the placement bindings file on the current `LocalSet`.
    /// Returns it, and where the guest's committed security contexts
    /// arrive, which `serve` reads.
    fn new(settings: Settings) -> (Self, UnboundedReceiver<SandboxListener>) {
        let settings = Rc::new(settings);
        let (sandbox_listeners, sandboxes) = SandboxListeners::new();
        let shared = Self {
            virtgpu: VirtGpuNode::new(settings.drm_device.clone()),
            placement: placement_config::load_and_watch(&settings),
            traces: settings.trace_dir.clone().and_then(Traces::new),
            clipboard: settings.data_control.then(ClipboardHub::default),
            sandbox_listeners,
            settings,
        };
        (shared, sandboxes)
    }
}

/// Serve guest clients on `settings.socket` for the life of the process.
///
/// Exits the process when the host cannot be reached or the socket cannot
/// be claimed, before any client connects.
pub(crate) async fn run(settings: Settings) {
    match &settings.host {
        HostTransport::VirtWl(path) => {
            if let Err(e) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
            {
                log::error!(
                    "Failed to open virtio-wayland device {}: {}",
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        }
        HostTransport::Local(path) => {
            if let Err(e) = std::os::unix::net::UnixStream::connect(path) {
                log::error!(
                    "Failed to connect to local compositor socket {}: {}",
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        }
    }

    let display = settings.socket.clone();
    // Held until the process exits, like the socket itself.
    let _socket_lock = claim_display_socket(&display).unwrap_or_else(|error| {
        log::error!(
            "Cannot claim the Wayland socket {}: {error}",
            display.display()
        );
        std::process::exit(1);
    });
    let listener = UnixListener::bind(&display).unwrap_or_else(|error| {
        log::error!(
            "Cannot bind the Wayland socket {}: {error}",
            display.display()
        );
        std::process::exit(1);
    });
    log::info!("Listening on {}", display.display());

    // Each client's Context holds xkb state, which is !Send. Keep each
    // client task on this executor's thread instead of using `tokio::spawn`
    // (whose Send bound would require an unsound manual Send implementation).
    // `LocalSet::run_until` also makes the ownership requirement visible in
    // this function's future type: callers cannot accidentally move the
    // proxy's client tasks to another executor thread.
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (shared, sandboxes) = Shared::new(settings);
            serve(listener, shared, sandboxes).await
        })
        .await;
}

/// Claim the socket path `display` for this process, as libwayland claims
/// its socket: take an exclusive lock on `<display>.lock` and, only while
/// holding it, remove a socket a previous owner left behind.
///
/// The lock lasts as long as the returned value. Fails, leaving the socket
/// alone, while another process holds the lock: that process is serving
/// the socket, and removing it would cut its clients off from new
/// connections.
fn claim_display_socket(display: &std::path::Path) -> io::Result<nix::fcntl::Flock<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut lock_path = display.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o660)
        .open(&lock_path)?;
    let lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, errno)| match errno {
            nix::errno::Errno::EWOULDBLOCK => io::Error::new(
                io::ErrorKind::AddrInUse,
                "another process holds its lock and is serving it",
            ),
            errno => io::Error::from(errno),
        })?;
    match std::fs::remove_file(display) {
        Ok(()) => log::info!("Removed the stale socket {}", display.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(lock)
}

/// Accept guest clients for the life of the process: on the proxy's
/// socket, and on each socket a guest's committed security context hands
/// it (`handler::security_context`), whose clients are sandboxed.
///
/// A failure concerning one client is logged and ends only that client.
/// Must run inside a `LocalSet`, like the clients it spawns.
async fn serve(
    listener: UnixListener,
    shared: Shared,
    mut sandboxes: UnboundedReceiver<SandboxListener>,
) {
    let shared = Rc::new(shared);
    let mut accepts = Accepts::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => accepts.client(accepted, &shared, false).await,
            Some(sandbox) = sandboxes.recv() => {
                tokio::task::spawn_local(serve_sandbox(sandbox, Rc::clone(&shared)));
            }
        }
    }
}

/// Accept sandboxed clients on `sandbox`'s socket until its close
/// descriptor signals, as `wp_security_context_v1` asks, whether or not
/// the client that made the context is still connected.
///
/// The descriptor is never read, so it cannot block the loop: any signal
/// ends the context. Its hangup is an error on a pipe's write end, which
/// Flatpak passes, and the end of the stream on a pipe's read end or a
/// socket; bytes written to it mean nothing.
async fn serve_sandbox(sandbox: SandboxListener, shared: Rc<Shared>) {
    let SandboxListener {
        listen,
        close,
        metadata,
        live: _live,
    } = sandbox;
    let signals = Interest::READABLE | Interest::ERROR;
    let listener = std::os::unix::net::UnixListener::from(listen);
    let listener = listener
        .set_nonblocking(true)
        .and_then(|()| UnixListener::from_std(listener));
    let close = AsyncFd::with_interest(close, signals);
    let (listener, close) = match (listener, close) {
        (Ok(listener), Ok(close)) => (listener, close),
        (Err(error), _) | (_, Err(error)) => {
            log::error!("Ignoring a security context ({metadata:?}): {error}");
            return;
        }
    };
    log::info!("Accepting sandboxed clients through a security context ({metadata:?})");
    let mut accepts = Accepts::default();
    loop {
        tokio::select! {
            accepted = listener.accept() => accepts.client(accepted, &shared, true).await,
            _ = close.ready(signals) => break,
        }
    }
    log::info!("A security context hung up ({metadata:?}); accepting no more clients through it");
}

/// The accept loop's handling of one accept: start the client, or log a
/// run of failed accepts once and back off between them, rather than spin
/// on the shared thread.
#[derive(Default)]
struct Accepts {
    failing: bool,
}

impl Accepts {
    async fn client(
        &mut self,
        accepted: io::Result<(UnixStream, tokio::net::unix::SocketAddr)>,
        shared: &Shared,
        sandboxed: bool,
    ) {
        match accepted {
            Ok((stream, _)) => {
                if std::mem::take(&mut self.failing) {
                    log::info!("Accepting clients again");
                }
                log::info!(
                    "New {}client connected",
                    if sandboxed { "sandboxed " } else { "" }
                );
                if let Err(error) = start_client(stream, shared, sandboxed).await {
                    log::error!("Closing the new client: {error}");
                }
            }
            Err(e) => {
                if !std::mem::replace(&mut self.failing, true) {
                    log::error!(
                        "Accept error: {e}; retrying every {ACCEPT_RETRY_DELAY:?} until one succeeds"
                    );
                }
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            }
        }
    }
}

/// Pair an accepted guest connection, `sandboxed` if it came through a
/// security context, with a new host connection and serve the pair on its
/// own task.
async fn start_client(stream: UnixStream, shared: &Shared, sandboxed: bool) -> io::Result<()> {
    // The peer's process, as it was when it connected.
    let pid = stream.peer_cred().ok().and_then(|cred| cred.pid());
    let client_conn = WaylandConnection::new(stream.into_std()?.into())?;
    let (host_conn, virtwl) = connect_host(&shared.settings.host).await?;
    let facts = ConnectionFacts {
        virtwl,
        virtgpu: shared.virtgpu.get(),
        wheel: shared
            .settings
            .wheel_from_touchpad
            .decide(pid, client_kind::is_wine_process),
        placement: shared.placement.clone(),
        clipboard: shared.clipboard.clone(),
        sandbox_listeners: Some(shared.sandbox_listeners.clone()),
        sandboxed,
    };
    let ctx = Context::new(Rc::clone(&shared.settings), facts);
    let trace = shared.traces.as_ref().and_then(|traces| traces.start(&ctx));
    let client = Client::new(client_conn, host_conn, ctx).recorded(trace);
    tokio::task::spawn_local(async move {
        client.run().await;
        log::info!("Client disconnected");
    });
    Ok(())
}

/// Open this client's connection to the host compositor over `host`,
/// with its VirtWL channel when there is one.
async fn connect_host(
    host: &HostTransport,
) -> io::Result<(WaylandConnection, Option<Arc<VirtWaylandChannel>>)> {
    match host {
        HostTransport::VirtWl(path) => {
            let channel = Arc::new(VirtWaylandChannel::new(path).map_err(|error| {
                io::Error::other(format!(
                    "cannot open a virtio-wayland channel at {}: {error}",
                    path.display()
                ))
            })?);
            let conn = WaylandConnection::new_virtwayland(Arc::clone(&channel));
            Ok((conn, Some(channel)))
        }
        HostTransport::Local(path) => {
            let stream = UnixStream::connect(path).await?;
            Ok((WaylandConnection::new(stream.into_std()?.into())?, None))
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::harness::{default_keymap, Object, Session};
    use super::Direction::{ClientToHost, HostToClient};
    use super::*;
    use crate::handler::test_support::session_with_shm;
    use crate::protocols;
    use crate::protocols::{AnyEvent, AnyRequest, Iface};
    use crate::state::{PlaneFenceForTest, PostDispatch};
    use crate::test_support::TestConnection;
    use crate::test_support::{
        any_fd_has_identity, fd_identity, fds_with_identity, raw_dispatch, respell, shm_pool_file,
        TestFdIdentity,
    };
    use crate::wire::Outgoing;
    use crate::wire::WireMessage;
    use crate::wire::{MessageBuilder, WL_DISPLAY_ID};
    use std::os::fd::{AsRawFd, OwnedFd};

    /// Every vendored interface has one `Iface`, which the wire names by
    /// its `NAME` and versions by its `VERSION`.
    #[test]
    fn every_interface_resolves_by_its_name() {
        for iface in Iface::ALL {
            assert_eq!(Iface::from_name(iface.name()), Some(iface));
        }
        use crate::protocols::wayland::wl_surface;
        assert_eq!(Iface::from_name(wl_surface::NAME), Some(Iface::WlSurface));
        assert_eq!(Iface::WlSurface.version(), wl_surface::VERSION);
        assert_eq!(
            Iface::from_name(protocols::aura_shell::zaura_toplevel::NAME),
            Some(Iface::ZauraToplevel)
        );
        assert_eq!(Iface::from_name("unknown_interface"), None);
    }

    /// `Client::run` asks the preferred peer for more before it dispatches
    /// another batch of the other's, so bytes still in the kernel's queue
    /// do not wait behind a render burst the proxy has already received.
    #[tokio::test]
    async fn fairness_probe_receives_preferred_host_events_between_render_batches() {
        use protocols::wayland::{wl_region, wl_seat};
        let mut s = Session::new();
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let region = region(&mut s).await;

        s.queue(ClientToHost, [wl_region::req::add(region.id, 0, 0, 1, 1)]);
        assert_eq!(
            s.next_direction(true),
            Some(ClientToHost),
            "an idle preferred transport must not block the buffered fallback"
        );

        s.write(
            HostToClient,
            [wl_seat::evt::name(seat.host, "seat0").unwrap()],
        );
        assert_eq!(
            s.next_direction(true),
            Some(HostToClient),
            "host bytes queued after a render batch must be received before another batch"
        );
        assert!(s.batch(HostToClient).await && s.flush().await);
        let name = "seat0".to_string();
        assert_eq!(
            s.guest_received(),
            [(seat.id, wl_seat::Event::Name { name }.into())]
        );
        assert!(s.batch(ClientToHost).await && s.flush().await);
        assert_eq!(s.host_received(), [added(region, 0)]);
    }

    /// A message whose header arrived in one read and whose body arrives in
    /// a later one must wait for that read, not be redispatched in a loop.
    #[test]
    fn run_receives_the_rest_of_a_partially_buffered_message() {
        use protocols::wayland::wl_display;
        use std::sync::mpsc::RecvTimeoutError;

        // Run the session on its own thread: a loop that spins without
        // awaiting would starve a runtime shared with this test.
        let (done, finished) = std::sync::mpsc::channel();
        let session = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async {
                let mut s = Session::new();
                // wl_display.sync has opcode 0, so the opcode bytes of its
                // header read as an invalid zero length when decoded as the
                // length field.
                let (sync, _) = wl_display::req::sync(WL_DISPLAY_ID, s.new_id()).into_parts();
                let (header, body) = sync.split_at(MessageHeader::LEN);
                // The body waits in the socket, so the proxy must read again
                // to complete the message whose header it has already
                // buffered.
                s.queue(
                    ClientToHost,
                    [Outgoing::from_parts(header.to_vec(), Vec::new())],
                );
                s.write(
                    ClientToHost,
                    [Outgoing::from_parts(body.to_vec(), Vec::new())],
                );
                s.run(async |peers| {
                    let forwarded = peers.host_received().await;
                    assert!(
                        matches!(
                            forwarded[..],
                            [(
                                WL_DISPLAY_ID,
                                AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
                            )]
                        ),
                        "the completed request must reach the host: {forwarded:?}"
                    );
                })
                .await;
            });
            let _ = done.send(());
        });
        let waited = finished.recv_timeout(Duration::from_secs(10));
        assert!(
            !matches!(waited, Err(RecvTimeoutError::Timeout)),
            "the proxy loop must read the rest of the message"
        );
        if let Err(panic) = session.join() {
            std::panic::resume_unwind(panic);
        }
    }

    /// A new `wl_region`, from a `wl_compositor` the guest binds.
    async fn region(s: &mut Session) -> Object {
        use protocols::wayland::wl_compositor;
        let compositor = s.bind("wl_compositor", wl_compositor::VERSION).await;
        let id = s.new_id();
        let host = s
            .create(wl_compositor::req::create_region(compositor, id))
            .await;
        Object { id, host }
    }

    /// The forward of the guest's `wl_region.add(x, 0, 1, 1)` to `region`.
    fn added(region: Object, x: i32) -> (u32, AnyRequest) {
        use protocols::wayland::wl_region;
        let add = wl_region::Request::Add {
            x,
            y: 0,
            width: 1,
            height: 1,
        };
        (region.host, add.into())
    }

    /// A `wl_pointer` whose focus is on a surface.
    async fn pointer(s: &mut Session) -> Object {
        use protocols::wayland::wl_seat;
        let seat = s.bind("wl_seat", wl_seat::VERSION).await;
        let surface = s.surface().await;
        s.pointer_on(seat, surface).await
    }

    /// A message no encoder builds, such as a malformed one: `words` as
    /// the payload of `opcode` from `sender`.
    fn raw(sender: u32, opcode: u16, words: &[u32]) -> Outgoing<u32> {
        let mut message = MessageBuilder::new();
        words.iter().for_each(|&word| message.write_u32(word));
        Outgoing::from_parts(message.build_message(sender, opcode), Vec::new())
    }

    /// libwayland-server rejects client object IDs past its map limit, so a
    /// host ID that grew with every object ever created would end the
    /// session after enough frame callbacks.
    #[tokio::test]
    async fn host_ids_are_recycled_through_delete_id() {
        use protocols::wayland::{wl_callback, wl_display};
        let mut s = Session::new();
        let callback = s.new_id();
        let first = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;
        for _ in 0..1000 {
            s.host([
                wl_callback::evt::done(first, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, first),
            ])
            .await;
            assert_eq!(
                s.guest_received(),
                [
                    (
                        callback,
                        wl_callback::Event::Done { callback_data: 0 }.into()
                    ),
                    (
                        WL_DISPLAY_ID,
                        wl_display::Event::DeleteId { id: callback }.into()
                    ),
                ]
            );
            assert_eq!(
                s.create(wl_display::req::sync(WL_DISPLAY_ID, callback))
                    .await,
                first,
                "a deleted host ID must be reused"
            );
        }
    }

    #[tokio::test]
    async fn host_ids_are_not_reused_before_delete_id() {
        use protocols::wayland::{wl_callback, wl_display};
        let mut s = Session::new();
        let (callback, later_callback) = (s.new_id(), s.new_id());
        let sync = |callback| wl_display::req::sync(WL_DISPLAY_ID, callback);

        // `done` destroys the callback on both sides and sends the guest its
        // delete_id, so the guest may reuse its ID at once; the host ID must
        // wait for the host's own delete_id.
        let first = s.create(sync(callback)).await;
        s.host([wl_callback::evt::done(first, 0)]).await;
        assert_eq!(
            s.guest_received(),
            [
                (
                    callback,
                    wl_callback::Event::Done { callback_data: 0 }.into()
                ),
                (
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: callback }.into()
                ),
            ]
        );
        assert_ne!(s.create(sync(callback)).await, first);

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, first)])
            .await;
        assert_eq!(s.create(sync(later_callback)).await, first);
    }

    /// A compositor destroys the pending frame callbacks of a destroyed
    /// surface without `done`, as Exo, weston and wlroots do, and
    /// libwayland-server then sends their `delete_id`. The callback's host
    /// ID must be reusable, and the guest must get its `delete_id`.
    #[tokio::test]
    async fn a_host_destroyed_frame_callback_reaches_the_guest() {
        use crate::protocols::wayland::{wl_display, wl_surface};
        let mut s = Session::new();
        let surface = s.surface().await;
        let callback = s.new_id();
        let host_callback = s.create(wl_surface::req::frame(surface.id, callback)).await;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(surface.host, wl_surface::Request::Destroy {}.into())]
        );

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, host_callback)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: callback }.into()
            )]
        );
        // Both IDs are free again: the guest reuses its own, and the proxy
        // the callback's host ID first.
        assert_eq!(
            s.create(wl_display::req::sync(WL_DISPLAY_ID, callback))
                .await,
            host_callback
        );
    }

    /// A session whose proxy has GPU access, without which the guest does
    /// not get the host's linux-dmabuf, and whose guest has bound it. The
    /// plane of each native buffer stands in for its own sync_file.
    async fn session_with_dmabuf() -> (Session, Object) {
        let mut s = Session::with_context(TestConnection::default().gpu_accel().context());
        let dmabuf = s.bind_paired("zwp_linux_dmabuf_v1", 3).await;
        s.guest_received();
        s.host_received();
        // Injects what no peer can provide: each plane stands in for its
        // own sync_file, which a readable pipe is once the guest GPU is done.
        s.ctx().plane_fences_for_test = Some(PlaneFenceForTest::SyncFile);
        (s, dmabuf)
    }

    /// A guest may destroy a buffer the host still holds. Its guest object
    /// takes no more requests, but the host's `release` must still reach the
    /// handler, which then destroys the host buffer.
    #[tokio::test]
    async fn a_retired_buffer_still_routes_its_release() {
        use protocols::wayland::{wl_buffer, wl_surface};
        let (mut s, dmabuf) = session_with_dmabuf().await;
        let surface = s.surface().await;
        // A native buffer the guest GPU has finished writing.
        let (plane, signal) = crate::test_support::pipe();
        assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
        let buffer = s.native_buffer(dmabuf.id, plane, 1, 1).await;
        s.guest([
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        assert_eq!(
            s.host_received(),
            [
                attached(surface, buffer),
                (surface.host, wl_surface::Request::Commit {}.into())
            ]
        );

        s.guest([wl_buffer::req::destroy(buffer.id)]).await;
        assert_eq!(s.host_received(), [], "the host still shows the buffer");
        s.host([wl_buffer::evt::release(buffer.host)]).await;
        assert_eq!(
            s.host_received(),
            [(buffer.host, wl_buffer::Request::Destroy {}.into())]
        );
        assert_eq!(s.guest_received(), []);
    }

    /// CPU time this thread has used.
    pub(super) fn thread_cpu_time() -> std::time::Duration {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut now) },
            0
        );
        std::time::Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
    }

    /// What a test's connections share, and where their committed security
    /// contexts arrive.
    fn test_shared() -> (Shared, UnboundedReceiver<SandboxListener>) {
        let (sandbox_listeners, sandboxes) = SandboxListeners::new();
        let shared = Shared {
            settings: Rc::new(TestConnection::default().settings),
            virtgpu: VirtGpuNode::absent_for_test(),
            placement: PlacementConfig::default(),
            traces: None,
            clipboard: None,
            sandbox_listeners,
        };
        (shared, sandboxes)
    }

    /// A security context's socket serves the engine's clients, each
    /// started (and here closed, the test's host being unreachable), until
    /// its close descriptor signals: Flatpak passes a pipe's write end,
    /// which signals an error once the sandbox closes the read end; a
    /// pipe's read end signals the end of the stream. Then the proxy closes
    /// the socket.
    #[tokio::test]
    async fn a_security_contexts_socket_serves_until_it_hangs_up() {
        use tokio::io::AsyncReadExt;
        for flatpak in [true, false] {
            let dir = crate::test_support::TempDir::new("sandbox-listener");
            let path = dir.path().join("sandbox");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let (reader, writer) = std::io::pipe().unwrap();
            let (close, hang_up): (OwnedFd, OwnedFd) = if flatpak {
                (writer.into(), reader.into())
            } else {
                (reader.into(), writer.into())
            };
            let sandbox = SandboxListener::for_test(listener.into(), close);
            let (shared, _sandboxes) = test_shared();
            tokio::task::LocalSet::new()
                .run_until(async {
                    let served = tokio::task::spawn_local(serve_sandbox(sandbox, Rc::new(shared)));
                    let mut client = UnixStream::connect(&path)
                        .await
                        .expect("the socket listens");
                    let mut byte = [0];
                    assert_eq!(
                        client.read(&mut byte).await.unwrap(),
                        0,
                        "the client was accepted"
                    );
                    drop(hang_up);
                    let ended = tokio::time::timeout(Duration::from_secs(10), served).await;
                    ended.expect("the hangup ends the accept loop").unwrap();
                    assert!(std::os::unix::net::UnixStream::connect(&path).is_err());
                })
                .await;
        }
    }

    #[tokio::test]
    async fn failing_accepts_back_off_instead_of_spinning() {
        use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};

        // A stream socket that never listened polls as hung up, so the event
        // loop sees it readable, and every accept on it fails at once, as
        // accepts do while the descriptor table is full.
        let not_listening = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )
        .expect("create a socket");
        let listener =
            UnixListener::from_std(std::os::unix::net::UnixListener::from(not_listening))
                .expect("register the socket");

        let window = std::time::Duration::from_millis(500);
        let (shared, sandboxes) = test_shared();
        let cpu_before = thread_cpu_time();
        let served = tokio::task::LocalSet::new()
            .run_until(tokio::time::timeout(
                window,
                serve(listener, shared, sandboxes),
            ))
            .await;
        let cpu = thread_cpu_time() - cpu_before;

        assert!(served.is_err(), "serve runs until the process ends");
        assert!(
            cpu < window / 4,
            "the accept loop used {cpu:?} of CPU in {window:?} of failing accepts"
        );
    }

    /// A burst of requests longer than one fairness batch reaches the host
    /// in as few submissions as fit it, not one per batch: each submission
    /// is a synchronous VirtWL round trip. No batch sends its output, which
    /// fills no submission, so the burst leaves at once when the proxy
    /// flushes before it waits.
    #[tokio::test]
    async fn a_burst_of_batches_reaches_the_host_in_one_submission() {
        use crate::connection::MAX_BYTES_PER_SEND;
        use protocols::wayland::wl_region;
        const REQUESTS: i32 = 100;
        const { assert!(REQUESTS as usize > 3 * MAX_MESSAGES_PER_BATCH) };
        let mut s = Session::new();
        let region = region(&mut s).await;
        let add = |x| wl_region::req::add(region.id, x, 0, 1, 1);
        assert!(REQUESTS as usize * add(0).bytes().len() <= MAX_BYTES_PER_SEND);

        s.queue(ClientToHost, (0..REQUESTS).map(add));
        while s.next_direction(false).is_some() {
            assert!(s.batch(ClientToHost).await);
            assert_eq!(s.host_received(), [], "a batch sends nothing of its own");
        }
        assert!(s.flush().await);
        let forwarded: Vec<_> = (0..REQUESTS).map(|x| added(region, x)).collect();
        assert_eq!(s.host_received(), forwarded);
    }

    /// A host batch's output reaches the guest before the proxy dispatches
    /// a guest batch: a key or IME event must not wait behind the rest of
    /// a render burst the guest has already sent.
    #[tokio::test]
    async fn a_host_batchs_output_reaches_the_guest_before_the_next_guest_batch() {
        use protocols::wayland::{wl_pointer, wl_region};
        let mut s = Session::new();
        let pointer = pointer(&mut s).await;
        let region = region(&mut s).await;

        // An event from the host, and a render burst longer than a batch
        // from the guest.
        s.queue(
            HostToClient,
            [wl_pointer::evt::motion(pointer.host, 1, 2, 3)],
        );
        let burst = 0..=MAX_MESSAGES_PER_BATCH as i32;
        let add = |x| wl_region::req::add(region.id, x, 0, 1, 1);
        s.queue(ClientToHost, burst.clone().map(add));
        assert!(s.batch(HostToClient).await);
        assert!(s.batch(ClientToHost).await);
        let motion = wl_pointer::Event::Motion {
            time: 1,
            surface_x: 2,
            surface_y: 3,
        };
        assert_eq!(s.guest_received(), [(pointer.id, motion.into())]);

        // The rest of the burst, and the whole of it once the proxy waits.
        assert_eq!(s.next_direction(true), Some(ClientToHost));
        assert!(s.batch(ClientToHost).await && s.flush().await);
        let forwarded: Vec<_> = burst.map(|x| added(region, x)).collect();
        assert_eq!(s.host_received(), forwarded);
    }

    /// A long stream in one direction still sends what it produced every
    /// `MAX_BATCHES_PER_FLUSH` batches, though no submission is full.
    #[tokio::test]
    async fn a_one_way_stream_sends_every_few_batches() {
        use crate::connection::MAX_BYTES_PER_SEND;
        use protocols::wayland::wl_region;
        let mut s = Session::new();
        let region = region(&mut s).await;
        let batches = MAX_BATCHES_PER_FLUSH + 1;
        let requests = (batches * MAX_MESSAGES_PER_BATCH) as i32;
        let add = |x| wl_region::req::add(region.id, x, 0, 1, 1);
        let sent = (MAX_BATCHES_PER_FLUSH * MAX_MESSAGES_PER_BATCH) as i32;
        // The batches' output fits one submission, so only the count sends it.
        assert!(sent as usize * add(0).bytes().len() < MAX_BYTES_PER_SEND);

        s.queue(ClientToHost, (0..requests).map(add));
        for _ in 0..batches {
            assert!(s.batch(ClientToHost).await);
        }
        let forwarded: Vec<_> = (0..sent).map(|x| added(region, x)).collect();
        assert_eq!(s.host_received(), forwarded, "the first batches");
        assert!(s.flush().await);
        let forwarded: Vec<_> = (sent..requests).map(|x| added(region, x)).collect();
        assert_eq!(s.host_received(), forwarded);
    }

    /// A guest `wl_data_device` of a seat, and its host object.
    async fn data_device(s: &mut Session) -> Object {
        use protocols::wayland::{wl_data_device_manager, wl_seat};
        let seat = s.bind("wl_seat", wl_seat::VERSION).await;
        let manager = s
            .bind("wl_data_device_manager", wl_data_device_manager::VERSION)
            .await;
        let id = s.new_id();
        let host = s
            .create(wl_data_device_manager::req::get_data_device(
                manager, id, seat,
            ))
            .await;
        Object { id, host }
    }

    /// A VirtWL transaction carries at most 28 descriptors, so a batch that
    /// forwards more must be split, between messages: the batch sends its
    /// full submission at once, and the rest when the proxy flushes.
    #[tokio::test]
    async fn batches_send_at_most_the_transport_descriptor_limit_at_once() {
        use crate::connection::MAX_FDS_PER_SEND;
        use protocols::wayland::{wl_data_device, wl_data_offer};
        const RECEIVES: usize = MAX_FDS_PER_SEND + 2;
        const { assert!(RECEIVES <= MAX_MESSAGES_PER_BATCH) };
        let mut s = Session::new();
        let device = data_device(&mut s).await;
        const HOST_OFFER: u32 = crate::test_support::wire_peer::SERVER_ID_START;
        s.host([wl_data_device::evt::data_offer(device.host, HOST_OFFER)])
            .await;
        let [(_, AnyEvent::WlDataDevice(wl_data_device::Event::DataOffer { id: offer }))] =
            s.guest_received()[..]
        else {
            panic!("the guest got no offer");
        };

        // The guest asks for the selection once per descriptor, which the
        // proxy dispatches as one batch.
        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let receive = |_| {
            let fd = pipe_write.try_clone().expect("duplicate the pipe");
            wl_data_offer::req::receive(offer, "text/plain", fd).unwrap()
        };
        s.queue(ClientToHost, (0..RECEIVES).map(receive));
        drop((pipe_read, pipe_write));
        let receives = |received: Vec<(u32, AnyRequest)>| {
            assert!(
                received.iter().all(|request| matches!(
                    request,
                    (
                        HOST_OFFER,
                        AnyRequest::WlDataOffer(wl_data_offer::Request::Receive { .. })
                    )
                )),
                "{received:?}"
            );
            received.len()
        };
        assert!(s.batch(ClientToHost).await);
        assert_eq!(receives(s.host_received()), MAX_FDS_PER_SEND);
        assert!(s.flush().await);
        assert_eq!(receives(s.host_received()), RECEIVES - MAX_FDS_PER_SEND);
    }

    #[test]
    fn a_held_socket_lock_protects_the_live_socket() {
        let dir = crate::test_support::TempDir::new("socket-lock");
        let socket = dir.path().join("wayland-test");
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("bind a stale socket");
        drop(stale);

        // No one holds the lock, so the socket is stale and is removed.
        let lock = claim_display_socket(&socket).expect("claim the free socket path");
        assert!(!socket.exists(), "the stale socket must be removed");

        // While the lock is held, a second claim fails and leaves the
        // owner's socket in place.
        let live = std::os::unix::net::UnixListener::bind(&socket).expect("bind the live socket");
        let error = claim_display_socket(&socket).expect_err("the path is claimed");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(socket.exists(), "a live socket must not be removed");

        // Once the owner is gone, the path can be claimed again.
        drop(live);
        drop(lock);
        claim_display_socket(&socket).expect("claim the released socket path");
        assert!(!socket.exists());
    }

    /// A batch dispatches at most `MAX_MESSAGES_PER_BATCH` messages and
    /// leaves the rest buffered for the next turn of the loop.
    #[tokio::test]
    async fn message_batches_leave_buffered_frames_for_fair_dispatch() {
        use protocols::wayland::wl_pointer;
        let mut s = Session::new();
        let pointer = pointer(&mut s).await;
        let batch = MAX_MESSAGES_PER_BATCH as u32;
        let motion = |time| wl_pointer::evt::motion(pointer.host, time, 0, 0);
        let moved = |time| {
            let motion = wl_pointer::Event::Motion {
                time,
                surface_x: 0,
                surface_y: 0,
            };
            (pointer.id, AnyEvent::from(motion))
        };

        s.queue(HostToClient, (0..=batch).map(motion));
        assert!(s.batch(HostToClient).await && s.flush().await);
        let forwarded: Vec<_> = (0..batch).map(moved).collect();
        assert_eq!(s.guest_received(), forwarded);
        assert_eq!(
            s.next_direction(true),
            Some(HostToClient),
            "a bounded batch must leave complete frames for the next loop turn"
        );
        assert!(s.batch(HostToClient).await && s.flush().await);
        assert_eq!(
            s.guest_received(),
            [moved(batch)],
            "exactly one frame should remain after the first bounded batch"
        );
        assert_eq!(s.next_direction(true), None);
    }

    /// A seat's two keyboards: one the guest has released, whose host
    /// `delete_id` has not come yet, and a live one.
    async fn released_and_live_keyboards(s: &mut Session) -> (Object, Object) {
        use protocols::wayland::wl_keyboard;
        let (seat, released) = s.keyboard().await;
        let live = s.keyboard_of(seat).await;
        s.guest([wl_keyboard::req::release(released.id)]).await;
        assert_eq!(
            s.host_received(),
            [(released.host, wl_keyboard::Request::Release {}.into())]
        );
        (released, live)
    }

    /// A valid keymap event of host `keyboard`, and the identity of its
    /// descriptor. Were its handler to run for a released keyboard, it
    /// would load the keymap for it, state the orphan check after the batch
    /// reports.
    fn keymap_event(keyboard: u32) -> (Outgoing<u32>, TestFdIdentity) {
        use protocols::wayland::wl_keyboard;
        let keymap = default_keymap();
        let file = shm_pool_file(&keymap);
        let identity = fd_identity(file.as_raw_fd());
        let format = wl_keyboard::KEYMAP_FORMAT_XKB_V1;
        let size = keymap.len() as u32;
        let event = wl_keyboard::evt::keymap(keyboard, format, file.into(), size);
        (event, identity)
    }

    /// The `repeat_info` the host sends a live keyboard after a stale
    /// keymap, as `keyboard` sends it on.
    fn repeat_info(keyboard: u32) -> Outgoing<u32> {
        protocols::wayland::wl_keyboard::evt::repeat_info(keyboard, 25, 400)
    }

    /// The guest's `repeat_info` of `keyboard`.
    fn repeated(keyboard: Object) -> (u32, AnyEvent) {
        use protocols::wayland::wl_keyboard;
        let repeat_info = wl_keyboard::Event::RepeatInfo {
            rate: 25,
            delay: 400,
        };
        (keyboard.id, repeat_info.into())
    }

    #[tokio::test]
    async fn pending_destroy_event_consumes_known_fd_before_live_event_in_same_batch() {
        use protocols::wayland::wl_display;
        let mut s = Session::new();
        let (released, live) = released_and_live_keyboards(&mut s).await;
        let (keymap, keymap_identity) = keymap_event(released.host);

        // A known stale event with a descriptor does not end the session.
        s.host([keymap, repeat_info(live.host)]).await;
        assert_eq!(s.guest_received(), [repeated(live)]);
        assert!(
            !any_fd_has_identity(keymap_identity),
            "the stale keymap's descriptor is closed"
        );
        // Schema-aware consumption does not poison later descriptor
        // ordering: the next descriptor reaches its own event.
        s.keymap(live).await;
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, released.host)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: released.id }.into()
            )],
            "dropping a stale event must not complete object teardown"
        );
    }

    #[tokio::test]
    async fn pending_destroy_event_consumes_known_fd_before_partial_followup() {
        let mut s = Session::new();
        let (released, live) = released_and_live_keyboards(&mut s).await;
        let (keymap, keymap_identity) = keymap_event(released.host);
        let (live_repeat_info, _) = repeat_info(live.host).into_parts();
        let (head, rest) = live_repeat_info.split_at(4);

        s.host([keymap, Outgoing::from_parts(head.to_vec(), Vec::new())])
            .await;
        assert_eq!(
            s.guest_received(),
            [],
            "an incomplete live event must not be forwarded"
        );
        assert!(
            !any_fd_has_identity(keymap_identity),
            "the stale keymap's descriptor is closed"
        );

        // The completed live follow-up remains decodable.
        s.host([Outgoing::from_parts(rest.to_vec(), Vec::new())])
            .await;
        assert_eq!(s.guest_received(), [repeated(live)]);
    }

    /// An event of host object 77, which the proxy does not track, with a
    /// descriptor it may or may not own, and that descriptor's identity.
    fn untracked_event_with_a_descriptor() -> (Outgoing<u32>, TestFdIdentity) {
        // Memfds have identities of their own; pipe ends share one.
        let file = shm_pool_file(&[]);
        let identity = fd_identity(file.as_raw_fd());
        let event = MessageBuilder::new().build_message(77, 0);
        (Outgoing::from_parts(event, vec![file.into()]), identity)
    }

    /// A seat the guest has bound, and the `name` event the host sends it,
    /// which the guest gets as it is.
    async fn seat_and_its_name(s: &mut Session) -> (Object, Vec<u8>) {
        use protocols::wayland::wl_seat;
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let (name, _) = wl_seat::evt::name(seat.host, "seat0").unwrap().into_parts();
        (seat, name)
    }

    #[tokio::test]
    async fn untracked_message_fd_is_retained_when_followup_is_partial() {
        let mut s = Session::new();
        let (_, follow_up) = seat_and_its_name(&mut s).await;
        let (stale_event, pending) = untracked_event_with_a_descriptor();

        // Sender 77 is complete but deliberately untracked. The four bytes
        // after it are only a partial header for the next message. There is no
        // interface metadata with which to determine whether the descriptor
        // belongs to the stale message or the partial follow-up, so the
        // descriptor must remain queued while the complete stale message is
        // dropped. The connection stays alive for the rest of the follow-up.
        s.host([
            stale_event,
            Outgoing::from_parts(follow_up[..4].to_vec(), Vec::new()),
        ])
        .await;
        assert_eq!(s.guest_received(), []);
        // The proxy's, and the host's own, which it keeps for the messages
        // it has not finished sending.
        assert_eq!(
            fds_with_identity(pending),
            2,
            "the pending descriptor must remain ordered for the later message"
        );
        drop(s);
        assert!(!any_fd_has_identity(pending));
    }

    #[tokio::test]
    async fn untracked_message_fd_ambiguity_survives_separate_receive_calls() {
        use protocols::wayland::wl_display;
        let mut s = Session::new();
        let (_, follow_up) = seat_and_its_name(&mut s).await;
        let (stale_event, pending) = untracked_event_with_a_descriptor();

        // Only a partial header of the follow-up message arrives with the
        // first receive, exactly as a stream socket may deliver it.
        s.host([
            stale_event,
            Outgoing::from_parts(follow_up[..4].to_vec(), Vec::new()),
        ])
        .await;
        // The rest of the follow-up arrives in a later receive. The
        // ambiguity marker must survive that boundary and end the session
        // before the pending descriptor can be attached to this message.
        s.host_fatal([Outgoing::from_parts(follow_up[4..].to_vec(), Vec::new())])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
            "descriptor ambiguity must remain fatal across receive calls"
        );
        drop(s);
        assert!(!any_fd_has_identity(pending));
    }

    #[tokio::test]
    async fn untracked_message_fd_ambiguity_terminates_before_followup_complete() {
        use protocols::wayland::wl_display;
        let mut s = Session::new();
        let (_, follow_up) = seat_and_its_name(&mut s).await;
        let (stale_event, pending) = untracked_event_with_a_descriptor();

        // Both messages are complete, but the first sender is untracked. The
        // descriptor arrived on the ordered stream with these bytes and
        // cannot be assigned to either message. The proxy must not guess and
        // accidentally deliver it to the known follow-up.
        s.host_fatal([stale_event, Outgoing::from_parts(follow_up, Vec::new())])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
            "complete untracked message plus a complete follow-up is fatal FD ambiguity"
        );
        drop(s);
        assert!(!any_fd_has_identity(pending));
    }

    /// A batch that ends early sends nothing more. The descriptors of the
    /// messages it translated close with it; the connection keeps the ones
    /// no message has taken yet until it closes.
    #[tokio::test]
    async fn early_output_cleanup_preserves_incoming_fd_ownership() {
        use protocols::wayland::{wl_display, wl_keyboard};
        let mut s = Session::new();
        let (_, keyboard) = s.keyboard().await;
        // Memfds have identities of their own; pipe ends share one.
        let forwarded = shm_pool_file(&[]);
        let pending = shm_pool_file(&[]);
        let forwarded_identity = fd_identity(forwarded.as_raw_fd());
        let pending_identity = fd_identity(pending.as_raw_fd());

        // A keymap the proxy forwards, then an event with trailing bytes,
        // which ends the session, then a keymap the proxy never reads,
        // which `pending` belongs to.
        let no_keymap = wl_keyboard::KEYMAP_FORMAT_NO_KEYMAP;
        s.host_fatal([
            wl_keyboard::evt::keymap(keyboard.host, no_keymap, forwarded.into(), 0),
            raw(keyboard.host, wl_keyboard::EVT_REPEAT_INFO, &[25, 400, 0]),
            wl_keyboard::evt::keymap(keyboard.host, no_keymap, pending.into(), 0),
        ])
        .await;
        let (events, error) = s.guest_received_and_error();
        assert_eq!(
            error,
            (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION),
            "trailing bytes in a host event end the session"
        );
        assert!(
            matches!(
                &events[..],
                [(id, AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { format, size: 0, .. }))]
                    if *id == keyboard.id && *format == no_keymap
            ),
            "the valid event reaches the guest ahead of the session's error: {events:?}"
        );
        drop(events);
        assert!(
            !any_fd_has_identity(forwarded_identity),
            "the proxy closes a forwarded descriptor once it is sent"
        );
        assert!(
            any_fd_has_identity(pending_identity),
            "the connection still owns the descriptor no message took"
        );
        drop(s);
        assert!(
            !any_fd_has_identity(pending_identity),
            "closing the connection must close every pending descriptor"
        );
    }

    /// Descriptor ambiguity ends the session like any host protocol error:
    /// the valid events before it reach the guest, then one error.
    #[tokio::test]
    async fn descriptor_ambiguity_is_reported_after_the_valid_events() {
        use protocols::wayland::{wl_data_device, wl_display};
        let mut s = Session::new();
        let device = data_device(&mut s).await;
        // An untracked message with a descriptor it may or may not own, then
        // a complete follow-up the descriptor might belong to.
        let (stale_event, _) = untracked_event_with_a_descriptor();
        s.host_fatal([
            wl_data_device::evt::data_offer(device.host, 0xff00_0001),
            stale_event,
            wl_data_device::evt::leave(device.host),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (
                vec![(
                    device.id,
                    wl_data_device::Event::DataOffer { id: 0xff00_0000 }.into()
                )],
                (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)
            )
        );
    }

    /// A rejected host event sends nothing its handler queued: a
    /// `wl_callback.done` with trailing bytes must not reach the guest as a
    /// done and a delete_id ahead of the error.
    #[tokio::test]
    async fn a_rejected_host_event_sends_nothing_of_its_own() {
        use protocols::wayland::{wl_callback, wl_display};
        let mut s = Session::new();
        let callback = s.new_id();
        let host_callback = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;
        s.host_fatal([raw(host_callback, wl_callback::EVT_DONE, &[0, 0xdead])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    /// A rejected guest request sends nothing its handler queued: a local
    /// `wl_shm_pool.destroy` with trailing bytes must not reach the guest as
    /// a delete_id ahead of the error.
    #[tokio::test]
    async fn a_rejected_request_sends_nothing_of_its_own() {
        use protocols::wayland::{wl_display, wl_shm_pool};
        let (mut s, pool) = session_with_a_shm_pool().await;
        s.guest_fatal([raw(pool, wl_shm_pool::REQ_DESTROY, &[0xdead])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
    }

    /// A request whose decode fails part-way also leaves bytes unread; its
    /// error names why the decode failed, not the bytes left over.
    #[tokio::test]
    async fn a_failed_decode_reports_its_own_error_not_the_bytes_left() {
        use protocols::wayland::{wl_display, wl_registry};
        let mut s = Session::new();
        let registry = s.registry().await;
        // The interface string has no terminating NUL; the version and new
        // ID after it stay unread.
        let interface = u32::from_ne_bytes(*b"abcd");
        s.guest_fatal([raw(
            registry.id,
            wl_registry::REQ_BIND,
            &[1, 4, interface, 1, 50],
        )])
        .await;
        assert_eq!(
            s.guest_received_and_diagnosed_error(),
            (
                vec![],
                (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD),
                ProtocolError::InvalidString.to_string()
            )
        );
    }

    /// libwayland passes a string that is not UTF-8 through, so a client
    /// that sets its title from Latin-1 bytes keeps its session: the host
    /// gets the title with each invalid sequence replaced by U+FFFD.
    #[tokio::test]
    async fn a_guest_string_that_is_not_utf8_reaches_the_host_with_replacement_characters() {
        use protocols::xdg_shell::xdg_toplevel;
        let mut s = Session::new();
        let toplevel = s.toplevel().await.toplevel;
        let title = xdg_toplevel::req::set_title(toplevel.id, "Cafe creme").unwrap();
        s.guest([respell(title, "Cafe creme", b"Caf\xe9 cr\xe8me")])
            .await;
        let title = "Caf\u{fffd} cr\u{fffd}me".to_string();
        assert_eq!(
            s.host_received(),
            [(
                toplevel.host,
                xdg_toplevel::Request::SetTitle { title }.into()
            )]
        );
    }

    /// The host direction follows the same rule.
    #[tokio::test]
    async fn a_host_string_that_is_not_utf8_reaches_the_guest_with_replacement_characters() {
        use protocols::wayland::wl_seat;
        let mut s = Session::new();
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let name = wl_seat::evt::name(seat.host, "seat-e").unwrap();
        s.host([respell(name, "seat-e", b"seat-\xe9")]).await;
        let name = "seat-\u{fffd}".to_string();
        assert_eq!(
            s.guest_received(),
            [(seat.id, wl_seat::Event::Name { name }.into())]
        );
    }

    /// An interface name that is not UTF-8 names nothing the proxy knows:
    /// a host global of one is not advertised, and a guest bind with one is
    /// an invalid bind, as a bind of any other interface is.
    #[tokio::test]
    async fn an_interface_name_that_is_not_utf8_names_no_global() {
        use protocols::wayland::{wl_display, wl_registry, wl_seat};
        let mut s = Session::new();
        let registry = s.registry().await;
        let global = wl_registry::evt::global(registry.host, 100, "wl_seaX", 1).unwrap();
        s.host([respell(global, "wl_seaX", b"wl_sea\xf4")]).await;
        assert_eq!(s.guest_received(), []);

        let name = s.advertise("wl_seat", wl_seat::VERSION).await;
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            s.guest_received()[..]
        else {
            panic!("the guest was not advertised the seat");
        };
        let id = s.new_id();
        let bind = wl_registry::req::bind(registry.id, name, "wl_seaX", version, id).unwrap();
        s.guest_fatal([respell(bind, "wl_seaX", b"wl_sea\xf4")])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (registry.id, wl_display::ERROR_INVALID_OBJECT))
        );
        assert_eq!(s.host_received(), []);
    }

    /// libwayland rejects a string with a NUL inside, and so does the
    /// proxy.
    #[tokio::test]
    async fn a_string_with_a_nul_inside_is_an_invalid_method() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::xdg_toplevel;
        let mut s = Session::new();
        let toplevel = s.toplevel().await.toplevel;
        let title = xdg_toplevel::req::set_title(toplevel.id, "a b").unwrap();
        s.guest_fatal([respell(title, "a b", b"a\0b")]).await;
        assert_eq!(
            s.guest_received_and_diagnosed_error(),
            (
                vec![],
                (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD),
                ProtocolError::InvalidString.to_string()
            )
        );
        assert_eq!(s.host_received(), []);
    }

    /// What earlier messages of the batch send back toward the guest still
    /// reaches it, ahead of the error: a local `wl_shm_pool.destroy` gets
    /// its delete_id although the next request is malformed.
    #[tokio::test]
    async fn earlier_reverse_output_reaches_the_guest_before_the_error() {
        use protocols::wayland::{wl_display, wl_shm_pool};
        let (mut s, pool) = session_with_a_shm_pool().await;
        s.guest_fatal([wl_shm_pool::req::destroy(pool), raw(WL_DISPLAY_ID, 99, &[])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (
                vec![(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: pool }.into()
                )],
                (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD)
            )
        );
    }

    /// A session whose guest has a `wl_shm_pool`, which the proxy implements.
    async fn session_with_a_shm_pool() -> (Session, u32) {
        let (mut s, shm) = session_with_shm().await;
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 64])).await;
        (s, pool)
    }

    /// A handler that ends the session sends nothing, not even the message
    /// it returns to be forwarded: a `created` whose buffer the proxy cannot
    /// track must not hand the guest a buffer.
    #[tokio::test]
    async fn a_failed_handler_forwards_nothing() {
        use protocols::linux_dmabuf_v1::{
            zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1,
        };
        use protocols::wayland::wl_display;
        const XRGB8888: u32 = 0x3432_5258;
        const BUFFER: u32 = crate::test_support::wire_peer::SERVER_ID_START;
        let (mut s, dmabuf) = session_with_dmabuf().await;
        let id = s.new_id();
        let host = s
            .create(zwp_linux_dmabuf_v1::req::create_params(dmabuf.id, id))
            .await;
        let (plane, _) = crate::test_support::pipe();
        s.guest([
            params::req::add(id, plane, 0, 0, 64, 0, 0),
            params::req::create(id, 16, 8, XRGB8888, 0),
        ])
        .await;
        s.host_received();

        // Injects a fault no peer can cause: the proxy already tracks a
        // buffer under the ID the host creates. (A host that reuses a live
        // ID is caught before the handler runs.)
        let buffer = HostId::for_test(BUFFER);
        assert!(s.ctx().register_native_buffer(buffer, (16, 8), Vec::new()));
        s.host_fatal([params::evt::created(host, BUFFER)]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    /// `Session::bind` binds a global the proxy advertises under another
    /// interface, which the proxy itself binds on the host when it sees it.
    #[tokio::test]
    async fn the_harness_binds_globals_the_guest_sees_under_another_name() {
        use protocols::{
            aura_shell::zaura_shell, text_input_unstable_v1::zwp_text_input_manager_v1,
        };
        let mut s = Session::new();
        s.bind("zaura_shell", zaura_shell::VERSION).await;
        s.bind(
            "zwp_text_input_manager_v1",
            zwp_text_input_manager_v1::VERSION,
        )
        .await;
    }

    /// A send that fails ends the session, whether it carries the batch's
    /// forwards or its output back to the source: the batch reports it,
    /// and no later batch dispatches anything.
    #[tokio::test]
    async fn a_failed_send_ends_the_session() {
        use protocols::wayland::{wl_callback, wl_display, wl_shm_pool};

        // The forward of a host event.
        let mut s = Session::new();
        let callback = s.new_id();
        let host_callback = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;
        s.break_guest_connection();
        s.host_fatal([wl_callback::evt::done(host_callback, 0)])
            .await;
        let later = s.new_id();
        s.guest_fatal([wl_display::req::sync(WL_DISPLAY_ID, later)])
            .await;
        assert_eq!(s.host_received(), []);

        // The delete_id a guest request sends back to the guest.
        let (mut s, pool) = session_with_a_shm_pool().await;
        s.break_guest_connection();
        s.guest_fatal([wl_shm_pool::req::destroy(pool)]).await;
        let later = s.new_id();
        s.guest_fatal([wl_display::req::sync(WL_DISPLAY_ID, later)])
            .await;
        assert_eq!(s.host_received(), []);
    }

    /// A message that ends the session sends nothing back to its source
    /// either: a `global_remove` whose internal `zaura_shell` the proxy
    /// cannot retire must not release the shell on the host.
    #[tokio::test]
    async fn a_failed_message_sends_no_reverse_output() {
        use protocols::aura_shell::zaura_shell;
        use protocols::wayland::{wl_display, wl_registry};
        let mut s = Session::new();
        let registry = s.registry().await;
        let name = s.advertise(zaura_shell::NAME, zaura_shell::VERSION).await;
        let shell = match &s.host_received()[..] {
            [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] => id.2,
            other => panic!("the proxy did not bind zaura_shell alone: {other:?}"),
        };
        // The guest sees gtk_shell1 in its place.
        s.guest_received();

        // Injects a fault no peer can cause: the object table no longer
        // tracks the proxy's shell as a live host-only object, so retiring
        // it fails.
        s.ctx().shadow_table.retire_host(HostId::for_test(shell));
        s.host_fatal([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        assert_eq!(s.host_received(), []);
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    /// A message that ends the session discards what its handler queued
    /// before the failure: a dma-buf `create_immed` whose host buffer the
    /// proxy cannot record must not send its plane and create to the host.
    #[tokio::test]
    async fn a_handler_error_discards_its_earlier_output() {
        use crate::state::HostId;
        use protocols::linux_dmabuf_v1::{
            zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1,
        };
        use protocols::wayland::wl_display;
        const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
        let (mut s, dmabuf) = session_with_dmabuf().await;
        let id = s.new_id();
        let host = s
            .create(zwp_linux_dmabuf_v1::req::create_params(dmabuf.id, id))
            .await;
        let (plane, _) = crate::test_support::pipe();
        s.guest([params::req::add(id, plane, 0, 0, 64, 0, 0)]).await;
        assert_eq!(s.host_received(), [], "the plane waits for the create");

        // Injects a fault no peer can cause: a render buffer already under
        // the host ID the proxy allocates next, for the buffer.
        let next = HostId::for_test(host + 1);
        assert!(s.ctx().register_native_buffer(next, (16, 8), Vec::new()));
        let buffer = s.new_id();
        s.guest_fatal([params::req::create_immed(
            id,
            buffer,
            16,
            8,
            DRM_FORMAT_ARGB8888,
            0,
        )])
        .await;
        assert_eq!(s.host_received(), []);
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    #[tokio::test]
    async fn unknown_client_sender_is_a_protocol_error() {
        use protocols::wayland::wl_display;
        let mut s = Session::new();

        // A client request with an unknown sender is malformed Wayland
        // traffic. It must not be silently discarded because doing so leaves
        // the client believing that the request succeeded.
        s.guest_fatal([wl_display::req::get_registry(999, 20)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_OBJECT)),
            "the guest learns why, as libwayland-server reports an unknown sender"
        );
    }

    #[tokio::test]
    async fn undecodable_request_is_reported_after_forwarding_earlier_requests() {
        use protocols::wayland::wl_display;
        let mut s = Session::new();
        let callback = s.new_id();

        // A whole wl_display.sync, then one without its callback argument.
        s.guest_fatal([
            wl_display::req::sync(WL_DISPLAY_ID, callback),
            raw(WL_DISPLAY_ID, wl_display::REQ_SYNC, &[]),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
        assert!(
            matches!(
                s.host_received()[..],
                [(
                    WL_DISPLAY_ID,
                    AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
                )]
            ),
            "the valid request before the bad one still reaches the host"
        );
    }

    /// A session whose guest has a `wl_surface`.
    async fn session_with_a_surface() -> (Session, super::harness::Object) {
        let mut s = Session::new();
        let surface = s.surface().await;
        (s, surface)
    }

    /// libwayland-server reports every rejected request on `wl_display`,
    /// and only an unusable sender as `invalid_object`: the code is read
    /// against the interface of the error's object.
    #[tokio::test]
    async fn rejected_requests_report_invalid_method_on_the_display() {
        use protocols::wayland::wl_display::ERROR_INVALID_METHOD;
        use protocols::wayland::wl_surface;
        let expected = (vec![], (WL_DISPLAY_ID, ERROR_INVALID_METHOD));

        let (mut s, surface) = session_with_a_surface().await;
        s.guest_fatal([raw(surface.id, wl_surface::REQ_ATTACH, &[0])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            expected,
            "an attach without its offsets does not decode"
        );

        let (mut s, surface) = session_with_a_surface().await;
        s.guest_fatal([wl_surface::req::attach(surface.id, Some(999), 0, 0)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            expected,
            "an attach of an unknown buffer names an unusable object argument"
        );

        let (mut s, surface) = session_with_a_surface().await;
        s.guest_fatal([wl_surface::req::attach(surface.id, Some(surface.id), 0, 0)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            expected,
            "a bad object argument is invalid_method even when it is the sender"
        );
    }

    #[tokio::test]
    async fn a_destroyed_sender_is_an_invalid_object_whatever_it_sends() {
        use protocols::wayland::{wl_display, wl_surface};
        let (mut s, surface) = session_with_a_surface().await;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(surface.host, wl_surface::Request::Destroy {}.into())]
        );
        s.guest_fatal([raw(surface.id, 99, &[])]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_OBJECT)),
            "the sender is checked before its opcode, as in libwayland-server"
        );
    }

    #[tokio::test]
    async fn a_session_gets_exactly_one_error() {
        use protocols::wayland::{wl_display, wl_surface};
        // The handler rejects scale 0 with wl_surface.invalid_scale; the
        // extra word then leaves trailing data, which must not add another.
        let (mut s, surface) = session_with_a_surface().await;
        s.guest_fatal([raw(surface.id, wl_surface::REQ_SET_BUFFER_SCALE, &[0, 0])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (surface.id, wl_surface::ERROR_INVALID_SCALE))
        );

        // A handler can end the session without queueing an error (a host
        // storage failure, say); the trailing data still gets reported.
        let (mut s, surface) = session_with_a_surface().await;
        s.ctx().end_session(SessionEnd::Internal);
        s.guest_fatal([raw(surface.id, wl_surface::REQ_COMMIT, &[0])])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
    }

    #[tokio::test]
    async fn an_internal_failure_ends_the_session_with_an_implementation_error() {
        use protocols::wayland::{wl_display, wl_surface};
        // As a failed dma-buf sync during a commit does: fatal, with no
        // error of its own.
        let (mut s, surface) = session_with_a_surface().await;
        s.ctx().end_session(SessionEnd::Internal);
        s.guest_fatal([wl_surface::req::commit(surface.id)]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    /// A session whose guest has a surface and a 1x1 native buffer whose
    /// one plane the guest GPU writes until the returned descriptor is
    /// written; `stand_in` describes the plane's fence.
    async fn session_with_a_fenced_buffer(
        stand_in: PlaneFenceForTest,
    ) -> (Session, Object, Object, OwnedFd) {
        let (mut s, dmabuf) = session_with_dmabuf().await;
        // Injects what no peer can provide (see `session_with_dmabuf`).
        s.ctx().plane_fences_for_test = Some(stand_in);
        let surface = s.surface().await;
        let (plane, signal) = crate::test_support::pipe();
        let buffer = s.native_buffer(dmabuf.id, plane, 1, 1).await;
        (s, surface, buffer, signal)
    }

    /// The guest's attach of `buffer` to `surface`, its commit, and a
    /// `wl_display.sync` that creates `callback`.
    fn commit_and_sync(surface: Object, buffer: Object, callback: u32) -> [Outgoing<u32>; 3] {
        use protocols::wayland::{wl_display, wl_surface};
        [
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
            wl_display::req::sync(WL_DISPLAY_ID, callback),
        ]
    }

    /// The forward of the guest's attach of `buffer` to `surface`.
    fn attached(surface: Object, buffer: Object) -> (u32, AnyRequest) {
        use protocols::wayland::wl_surface;
        let attach = wl_surface::Request::Attach {
            buffer: buffer.host,
            x: 0,
            y: 0,
        };
        (surface.host, attach.into())
    }

    /// Assert that `forwarded` is `surface`'s commit, once, and the
    /// `wl_display.sync` after it.
    fn assert_commit_then_sync(forwarded: &[(u32, AnyRequest)], surface: Object) {
        use protocols::wayland::{wl_display, wl_surface};
        assert!(
            matches!(
                forwarded,
                [
                    (commit, AnyRequest::WlSurface(wl_surface::Request::Commit {})),
                    (
                        WL_DISPLAY_ID,
                        AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
                    ),
                ] if *commit == surface.host
            ),
            "the commit, once, then the request after it: {forwarded:?}"
        );
    }

    /// A commit whose fence is pending parks the guest stream: neither it
    /// nor the request after it is dispatched, also when it is dispatched
    /// again while the fence is pending, and fair dispatch does not offer
    /// the stream, while host events still reach the guest. (That it keeps
    /// its deadline when it waits again
    /// `a_fence_that_never_signals_ends_the_session_at_its_deadline` pins.)
    #[tokio::test]
    async fn a_pending_fence_parks_the_guest_stream_while_host_events_flow() {
        use protocols::wayland::{wl_callback, wl_display};
        let (mut s, surface, buffer, _signal) =
            session_with_a_fenced_buffer(PlaneFenceForTest::SyncFile).await;
        // A callback the host can finish while the guest stream is parked.
        let callback = s.new_id();
        let host_callback = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;

        let sync_callback = s.new_id();
        s.guest(commit_and_sync(surface, buffer, sync_callback))
            .await;
        assert!(s.parked());
        assert_eq!(
            s.host_received(),
            [attached(surface, buffer)],
            "the request after the commit waits behind it"
        );
        assert_eq!(
            s.next_direction(false),
            None,
            "fair dispatch must not offer a parked stream"
        );
        s.guest([]).await;
        assert!(s.parked());
        assert_eq!(s.host_received(), []);

        s.queue(HostToClient, [wl_callback::evt::done(host_callback, 0)]);
        assert_eq!(
            s.next_direction(false),
            Some(HostToClient),
            "a parked client still serves the host"
        );
        assert!(s.batch(HostToClient).await && s.flush().await);
        assert!(s.parked());
        assert_eq!(
            s.guest_received(),
            [
                (
                    callback,
                    wl_callback::Event::Done { callback_data: 0 }.into()
                ),
                (
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: callback }.into()
                ),
            ]
        );
    }

    /// A message that waits for a fence after it queued output or deferred
    /// work is a proxy bug: the session ends instead of losing the output,
    /// before the deferred work runs.
    #[tokio::test]
    async fn a_wait_with_output_ends_the_session() {
        use protocols::wayland::{wl_callback, wl_display, wl_surface};
        type Inject = fn(&mut Context, Object, u32);
        let faults: [(&str, Inject); 3] = [
            ("an event for the guest", |ctx, _, callback| {
                let callback = GuestId::for_test(callback);
                ctx.send_guest(wl_callback::evt::done(callback, 0));
            }),
            ("a request for the host", |ctx, surface, _| {
                let surface = HostId::for_test(surface.host);
                ctx.send_host(wl_surface::req::commit(surface));
            }),
            ("deferred work", |ctx, surface, callback| {
                ctx.defer_until_forwarded(PostDispatch::XdgToplevelCreated {
                    xdg_surface: GuestId::for_test(surface.id),
                    xdg_toplevel: GuestId::for_test(callback),
                });
            }),
        ];
        for (fault, inject) in faults {
            let (mut s, surface, buffer, _signal) =
                session_with_a_fenced_buffer(PlaneFenceForTest::SyncFile).await;
            let callback = s.new_id();
            s.guest([wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0)])
                .await;
            assert_eq!(s.host_received(), [attached(surface, buffer)]);

            // Injects a fault no peer can cause: what the commit has when
            // it waits, as from a handler that queued it first.
            inject(s.ctx(), surface, callback);
            s.guest_fatal([wl_surface::req::commit(surface.id)]).await;
            assert!(!s.parked(), "{fault}");
            assert_eq!(s.host_received(), [], "{fault}");
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
                "{fault}"
            );
        }
    }

    #[tokio::test]
    async fn a_parked_commit_is_forwarded_once_when_its_fence_signals() {
        let (mut s, surface, buffer, signal) =
            session_with_a_fenced_buffer(PlaneFenceForTest::SyncFile).await;
        let fence = fd_identity(signal.as_raw_fd());
        // What the proxy keeps of the plane, and `signal`.
        let idle = fds_with_identity(fence);

        let callback = s.new_id();
        s.guest(commit_and_sync(surface, buffer, callback)).await;
        assert!(s.parked());
        assert_eq!(s.host_received(), [attached(surface, buffer)]);
        assert_eq!(
            fds_with_identity(fence),
            idle + 1,
            "the parked request holds the fence"
        );

        assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
        s.resume().await;
        assert!(!s.parked());
        assert_eq!(fds_with_identity(fence), idle);
        assert_commit_then_sync(&s.host_received(), surface);
        drop(s);
        drop(signal);
        assert!(!any_fd_has_identity(fence));
    }

    /// Thirty planes that share one fence give a commit a 30 s deadline,
    /// which no loaded test machine reaches, so a test that runs the proxy
    /// in real time fails only when the commit is held back.
    const SLOW_DEADLINE_PLANES: usize = 30;

    /// Through `Client::run`: the parked commit waits while host events
    /// reach the guest, and is forwarded once its sync_file becomes
    /// readable, before the request after it and only once.
    #[tokio::test]
    async fn run_forwards_a_parked_commit_once_its_sync_file_signals() {
        use protocols::wayland::{wl_callback, wl_display};
        let mut s = Session::new();
        let surface = s.surface().await;
        let buffer = Object {
            id: s.new_id(),
            host: 0x100,
        };
        // Injects what no peer can provide: a native buffer of
        // `SLOW_DEADLINE_PLANES` planes (a guest's has at most four) that
        // share one fence, the guest GPU's write until `signal` is written,
        // each plane standing in for its own sync_file.
        let (plane, signal) = crate::test_support::pipe();
        s.pair_for_test(buffer.id, buffer.host, "wl_buffer", 1);
        let ctx = s.ctx();
        ctx.plane_fences_for_test = Some(PlaneFenceForTest::SyncFile);
        let planes = (0..SLOW_DEADLINE_PLANES)
            .map(|_| plane.try_clone().expect("share the fence"))
            .collect();
        assert!(ctx.register_native_buffer(HostId::for_test(buffer.host), (1, 1), planes));
        let callback = s.new_id();
        let host_callback = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;
        let sync_callback = s.new_id();
        s.queue(
            ClientToHost,
            commit_and_sync(surface, buffer, sync_callback),
        );

        s.run(async |peers| {
            assert_eq!(peers.host_received().await, [attached(surface, buffer)]);
            peers.host([wl_callback::evt::done(host_callback, 0)]);
            assert_eq!(
                peers.guest_received().await,
                [
                    (
                        callback,
                        wl_callback::Event::Done { callback_data: 0 }.into()
                    ),
                    (
                        WL_DISPLAY_ID,
                        wl_display::Event::DeleteId { id: callback }.into()
                    ),
                ]
            );
            assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
            assert_commit_then_sync(&peers.host_received().await, surface);
        })
        .await;
        assert_eq!(s.host_received(), []);
        assert_eq!(s.guest_received(), []);
    }

    /// `Client::run` holds a guest sync that waits for the guest's
    /// globals, reads no more of the guest's stream while host events
    /// flow, and dispatches the sync once the host batch that settles the
    /// globals ends, after the proxy's binding of the linux-dmabuf global
    /// the registry listed and its capability sync, so the guest gets the
    /// global before the sync's `done`.
    #[tokio::test]
    async fn the_host_batch_that_settles_the_globals_releases_a_waiting_sync() {
        use protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
        use protocols::wayland::{wl_callback, wl_display, wl_registry};
        let mut ctx = TestConnection::default().gpu_accel().context();
        // Stands in for a virtio-gpu render node, which the proxy
        // synthesizes dma-buf feedback for and a test cannot have.
        ctx.dmabuf.set_feedback_available_for_test(true);
        let mut s = Session::with_context(ctx);
        let (registry, callback, later) = (s.new_id(), s.new_id(), s.new_id());
        s.queue(
            ClientToHost,
            [
                wl_display::req::get_registry(WL_DISPLAY_ID, registry),
                wl_display::req::sync(WL_DISPLAY_ID, callback),
                wl_display::req::sync(WL_DISPLAY_ID, later),
            ],
        );
        let sync = |request: &(u32, AnyRequest)| match request {
            (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) => {
                *callback
            }
            _ => panic!("{request:?} is no sync"),
        };
        let answer = |callback| {
            [
                wl_callback::evt::done(callback, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
            ]
        };
        let done = |callback| {
            [
                (
                    callback,
                    wl_callback::Event::Done { callback_data: 0 }.into(),
                ),
                (
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: callback }.into(),
                ),
            ]
        };

        s.run(async |peers| {
            let host = peers.host_received().await;
            let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry: host_registry })), ref barrier] =
                host[..]
            else {
                panic!("the host got {host:?}, not a get_registry and a sync");
            };
            let barrier = sync(barrier);
            let global = wl_registry::evt::global(host_registry, 1, zwp_linux_dmabuf_v1::NAME, 4);
            peers.host([global.unwrap()]);
            peers.host(answer(barrier));
            let mut host = Vec::new();
            while host.len() < 4 {
                host.extend(peers.host_received().await);
            }
            let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { ref id, .. })), ref capabilities, ref first, ref second] =
                host[..]
            else {
                panic!("the host got {host:?}, not a bind and three syncs");
            };
            peers.host([zwp_linux_dmabuf_v1::evt::modifier(id.2, 0x3432_5258, 0, 0)]);
            for callback in [capabilities, first, second] {
                peers.host(answer(sync(callback)));
            }
            let mut received = Vec::new();
            while received.len() < 5 {
                received.extend(peers.guest_received().await);
            }
            let global = wl_registry::Event::Global {
                name: 1,
                interface: zwp_linux_dmabuf_v1::NAME.to_owned(),
                version: 4,
            };
            let mut expected = vec![(registry, global.into())];
            expected.extend(done(callback));
            expected.extend(done(later));
            assert_eq!(received, expected);
        })
        .await;
        assert_eq!(s.host_received(), []);
        assert_eq!(s.guest_received(), []);
    }

    /// Host traffic must not postpone asking about a fence that gives no
    /// descriptor to wait on: the commit is forwarded within a few poll
    /// intervals of its fence signalling, while the host sends an event
    /// every 500 µs, each of which wakes the loop (a 1 kHz mouse wakes it
    /// every millisecond). Time moves only with the traffic, so no pause
    /// between events can let a postponed poll through.
    #[tokio::test(start_paused = true)]
    async fn host_traffic_does_not_postpone_polling_a_busy_fence() {
        /// A host object the proxy does not track, so it reads and drops
        /// what the object sends.
        const UNTRACKED_HOST_OBJECT: u32 = 0x7000;
        let (mut s, surface, buffer, signal) =
            session_with_a_fenced_buffer(PlaneFenceForTest::Busy).await;
        let callback = s.new_id();
        s.queue(ClientToHost, commit_and_sync(surface, buffer, callback));

        s.run(async |peers| {
            assert_eq!(peers.host_received().await, [attached(surface, buffer)]);
            assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
            // The first polls are 2, 4 and 8 ms apart; the deadline is 15 s
            // away.
            let start = Instant::now();
            let forwarded = loop {
                let event = MessageBuilder::new().build_message(UNTRACKED_HOST_OBJECT, 0);
                peers.host([Outgoing::from_parts(event, Vec::new())]);
                tokio::select! {
                    forwarded = peers.host_received() => break forwarded,
                    () = tokio::time::advance(Duration::from_micros(500)) => {}
                }
                assert!(
                    start.elapsed() < Duration::from_millis(100),
                    "host traffic postpones polling the fence"
                );
            };
            assert_commit_then_sync(&forwarded, surface);
        })
        .await;
        assert_eq!(s.host_received(), []);
    }

    /// Dispatch a commit behind a fence, which `stand_in` describes, that
    /// never signals, each time it is due, and assert that the session
    /// ends with one `implementation` error and without the commit or
    /// anything after it. Returns how long the session lasted. A deadline
    /// that never comes fails the test instead of hanging it.
    ///
    /// The loop is driven by hand rather than by `Client::run`: paused
    /// time jumps to the next timer whenever the runtime waits on a
    /// socket, so a guard timer beside `run` would be jumped to.
    async fn session_with_a_fence_that_never_signals(stand_in: PlaneFenceForTest) -> Duration {
        use protocols::wayland::wl_display;
        let (mut s, surface, buffer, signal) = session_with_a_fenced_buffer(stand_in).await;
        let fence = fd_identity(signal.as_raw_fd());

        let callback = s.new_id();
        let start = Instant::now();
        let mut alive = s
            .try_guest(commit_and_sync(surface, buffer, callback))
            .await;
        assert_eq!(s.host_received(), [attached(surface, buffer)]);
        while alive {
            assert!(
                start.elapsed() < crate::allocator::VIRTGPU_WAIT_TIMEOUT * 4,
                "the session outlives its deadline"
            );
            s.fence_due().await;
            alive = s.try_guest([]).await;
        }
        let lasted = start.elapsed();

        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(s.host_received(), []);
        drop(s);
        drop(signal);
        assert!(!any_fd_has_identity(fence));
        lasted
    }

    /// A sync_file that never signals ends the session at its deadline.
    #[tokio::test(start_paused = true)]
    async fn a_fence_that_never_signals_ends_the_session_at_its_deadline() {
        let lasted = session_with_a_fence_that_never_signals(PlaneFenceForTest::SyncFile).await;
        let deadline = crate::allocator::SYNC_FILE_TIMEOUT;
        assert!(deadline <= lasted && lasted < deadline * 2, "{lasted:?}");
    }

    /// A virtio-gpu fallback fence that never signals ends the session at
    /// the kernel's bound, though it is polled all the while.
    #[tokio::test(start_paused = true)]
    async fn a_fallback_fence_that_never_signals_ends_the_session_at_its_deadline() {
        let lasted = session_with_a_fence_that_never_signals(PlaneFenceForTest::Busy).await;
        let deadline = crate::allocator::VIRTGPU_WAIT_TIMEOUT;
        assert!(
            deadline <= lasted && lasted < deadline + MAX_FENCE_POLL_INTERVAL * 2,
            "{lasted:?}"
        );
    }

    /// A fence with no descriptor to wait on is asked about again after a
    /// pause that doubles each time, up to about a frame.
    #[tokio::test(start_paused = true)]
    async fn a_fence_without_a_descriptor_is_polled_less_often_as_it_waits() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut parked = ParkedRequest::new(PendingFence::Busy, deadline);
        for millis in [2, 4, 8, 16, 16] {
            let start = Instant::now();
            parked.due().await;
            assert_eq!(start.elapsed(), Duration::from_millis(millis));
            parked = parked.wait_again(PendingFence::Busy);
        }
        assert_eq!(parked.deadline, deadline);
    }

    /// The virtio-gpu fallback keeps the kernel's bound of its old blocking
    /// wait: its commit goes through after a fence that took longer than a
    /// sync_file may, asked about all the while.
    #[tokio::test(start_paused = true)]
    async fn a_slow_fallback_fence_outlasts_the_sync_file_bound() {
        let (mut s, surface, buffer, signal) =
            session_with_a_fenced_buffer(PlaneFenceForTest::Busy).await;
        let callback = s.new_id();
        s.guest(commit_and_sync(surface, buffer, callback)).await;
        assert_eq!(s.host_received(), [attached(surface, buffer)]);
        let start = Instant::now();
        let gpu_time = crate::allocator::SYNC_FILE_TIMEOUT * 5;
        assert!(gpu_time < crate::allocator::VIRTGPU_WAIT_TIMEOUT);
        while s.parked() {
            if start.elapsed() >= gpu_time {
                assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
            }
            // A slow fallback fence must not end the session.
            s.resume().await;
        }
        assert!(start.elapsed() < gpu_time + MAX_FENCE_POLL_INTERVAL * 2);
        assert_commit_then_sync(&s.host_received(), surface);
    }

    /// The host's error is the guest's one error: a malformed request that
    /// follows it gets none of its own, even in a later batch.
    #[tokio::test]
    async fn a_host_error_is_the_sessions_only_error() {
        use protocols::wayland::{wl_display, wl_surface};
        let (mut s, surface) = session_with_a_surface().await;
        s.host_fatal([wl_display::evt::error(
            WL_DISPLAY_ID,
            surface.host,
            wl_surface::ERROR_INVALID_SIZE,
            "host failure",
        )
        .unwrap()])
            .await;
        s.guest_fatal([raw(surface.id, 99, &[])]).await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (surface.id, wl_surface::ERROR_INVALID_SIZE))
        );
    }

    /// The host may report an error on an object only the proxy uses, such
    /// as its own `zaura_shell` or a temporary pool. The guest cannot be
    /// told which object failed, but the session still ends with one error.
    #[tokio::test]
    async fn a_host_error_on_a_host_only_object_ends_the_session() {
        use protocols::aura_shell::zaura_shell;
        use protocols::wayland::{wl_display, wl_registry};
        let mut s = Session::new();
        s.advertise("zaura_shell", zaura_shell::VERSION).await;
        let host_shell = match &s.host_received()[..] {
            [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))]
                if id.0 == "zaura_shell" =>
            {
                id.2
            }
            other => panic!("the proxy did not bind zaura_shell alone: {other:?}"),
        };
        // The guest sees gtk_shell1 in its place.
        s.guest_received();

        s.host_fatal([wl_display::evt::error(
            WL_DISPLAY_ID,
            host_shell,
            zaura_shell::ERROR_AURA_SURFACE_EXISTS,
            "host failure",
        )
        .unwrap()])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    /// An event the proxy cannot read, from an object only the proxy uses
    /// (its own `zaura_shell`), is dropped with a warning: an unknown
    /// opcode, an event newer than the binding, a truncated payload and
    /// trailing bytes. Nothing reaches either peer, and the session goes on.
    #[tokio::test]
    async fn an_unreadable_event_of_a_host_only_object_is_dropped() {
        use protocols::aura_shell::zaura_shell;
        use protocols::wayland::wl_registry;
        let mut s = Session::new();
        // Bound at v11, the version of `layout_mode`.
        s.advertise("zaura_shell", zaura_shell::EVT_LAYOUT_MODE_SINCE)
            .await;
        let shell = match &s.host_received()[..] {
            [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))]
                if id.0 == "zaura_shell" =>
            {
                id.2
            }
            other => panic!("the proxy did not bind zaura_shell alone: {other:?}"),
        };
        s.guest_received();
        const { assert!(zaura_shell::EVT_BUG_FIX_SINCE > zaura_shell::EVT_LAYOUT_MODE_SINCE) };

        s.host([
            raw(shell, 99, &[]),
            zaura_shell::evt::bug_fix(shell, 1),
            raw(shell, zaura_shell::EVT_LAYOUT_MODE, &[]),
            raw(shell, zaura_shell::EVT_LAYOUT_MODE, &[1, 2]),
            zaura_shell::evt::layout_mode(shell, 1),
        ])
        .await;
        assert!(
            dispatch::UNREADABLE_INTERNAL_EVENT.logged(),
            "the first warns"
        );
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.host_received(), []);
    }

    /// The same events from a host object the guest has end the session.
    #[tokio::test]
    async fn an_unreadable_event_of_a_guest_visible_object_ends_the_session() {
        use protocols::wayland::{wl_display, wl_output};
        let output_version = wl_output::EVT_NAME_SINCE - 1;
        let unreadable = |output: u32| {
            [
                raw(output, 99, &[]),
                wl_output::evt::name(output, "too new").unwrap(),
                raw(output, wl_output::EVT_SCALE, &[]),
                raw(output, wl_output::EVT_SCALE, &[1, 2]),
            ]
        };
        for event in 0..unreadable(0).len() {
            let mut s = Session::new();
            let output = s.bind_paired("wl_output", output_version).await;
            let unreadable = unreadable(output.host).into_iter().nth(event);
            s.host_fatal(unreadable).await;
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
                "event {event}"
            );
        }
    }

    /// A host event that breaks the protocol ends the session like a guest
    /// request does: the events before it still reach the guest, followed
    /// by one error.
    #[tokio::test]
    async fn an_invalid_host_event_is_reported_after_the_valid_ones() {
        use protocols::wayland::{wl_data_device, wl_data_device_manager, wl_display, wl_seat};
        let mut s = Session::new();
        let seat = s.bind("wl_seat", wl_seat::VERSION).await;
        let manager = s
            .bind("wl_data_device_manager", wl_data_device_manager::VERSION)
            .await;
        let device = s.new_id();
        let host_device = s
            .create(wl_data_device_manager::req::get_data_device(
                manager, device, seat,
            ))
            .await;
        // A server-range offer, then one from the client range, which only
        // the proxy allocates from.
        s.host_fatal([
            wl_data_device::evt::data_offer(host_device, 0xff00_0001),
            wl_data_device::evt::data_offer(host_device, 123),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (
                vec![(
                    device,
                    wl_data_device::Event::DataOffer { id: 0xff00_0000 }.into()
                )],
                (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)
            )
        );
    }

    #[tokio::test]
    async fn an_invalid_message_length_is_reported_before_closing() {
        let mut s = Session::new();
        // A header claiming 6 bytes, less than the header itself.
        let header = [WL_DISPLAY_ID.to_ne_bytes(), (6u32 << 16).to_ne_bytes()].concat();
        s.guest_fatal([Outgoing::from_parts(header, Vec::new())])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (
                vec![],
                (
                    WL_DISPLAY_ID,
                    protocols::wayland::wl_display::ERROR_INVALID_METHOD
                )
            )
        );
    }

    #[tokio::test]
    async fn a_request_over_4096_bytes_is_an_invalid_method() {
        use protocols::wayland::{wl_data_device_manager, wl_data_source, wl_display};
        let mut s = Session::new();
        let manager = s
            .bind("wl_data_device_manager", wl_data_device_manager::VERSION)
            .await;
        let source = s.new_id();
        let host_source = s
            .create(wl_data_device_manager::req::create_data_source(
                manager, source,
            ))
            .await;
        // `wl_data_source.offer` of a MIME type that makes the message
        // `bytes` long: its length word, its bytes and NUL.
        let mime_type = |bytes: usize| "x".repeat(bytes - MessageHeader::LEN - 4 - 1);
        // libwayland-server's limit is the message's length, not its
        // arguments': one of 4096 bytes is valid, one word more is not, and
        // no encoder builds it, so its header is written by hand.
        let too_long = {
            let mut offer = MessageBuilder::new();
            offer.write_string(&mime_type(4100));
            let word = (4100u32 << 16) | u32::from(wl_data_source::REQ_OFFER);
            [
                source.to_ne_bytes().as_slice(),
                &word.to_ne_bytes(),
                &offer.into_payload(),
            ]
            .concat()
        };
        assert_eq!(too_long.len(), 4100);

        s.guest_fatal([
            wl_data_source::req::offer(source, &mime_type(4096)).unwrap(),
            Outgoing::from_parts(too_long, Vec::new()),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
        assert_eq!(
            s.host_received(),
            [(
                host_source,
                wl_data_source::Request::Offer {
                    mime_type: mime_type(4096)
                }
                .into()
            )]
        );
    }

    /// A session whose proxy applies the placement policy and has bound
    /// the host's `zaura_shell`, and whose guest has an `xdg_surface`;
    /// returns the host shell and the `xdg_surface`.
    async fn session_with_an_xdg_surface() -> (Session, u32, Object) {
        use protocols::xdg_shell::xdg_wm_base;
        let placement = PlacementConfig::active_for_test(&[]);
        let mut s = Session::with_context(TestConnection::default().placement(placement).context());
        let shell = s.aura_shell().await;
        let wm_base = s.bind("xdg_wm_base", xdg_wm_base::VERSION).await;
        let surface = s.surface().await;
        let id = s.new_id();
        let host = s
            .create(xdg_wm_base::req::get_xdg_surface(wm_base, id, surface.id))
            .await;
        (s, shell, Object { id, host })
    }

    /// Whether the proxy has an Aura toplevel for any guest toplevel, or
    /// recorded an XDG toplevel role for `xdg_surface`. Reads state no peer
    /// can observe: a session that ended sends nothing more that would
    /// show it.
    fn has_toplevel_state(s: &mut Session, xdg_surface: Object) -> bool {
        let ctx = s.ctx();
        ctx.placement()
            .is_some_and(|state| state.toplevel_count() > 0)
            || ctx
                .xdg_roles
                .toplevel_of_xdg_surface(GuestId::for_test(xdg_surface.id))
                .is_some()
    }

    #[tokio::test]
    async fn failed_get_toplevel_does_not_create_internal_aura_object() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::xdg_surface;
        let (mut s, _, xdg_surface) = session_with_an_xdg_surface().await;
        s.guest([xdg_surface::req::destroy(xdg_surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(xdg_surface.host, xdg_surface::Request::Destroy {}.into())]
        );

        let toplevel = s.new_id();
        s.guest_fatal([xdg_surface::req::get_toplevel(xdg_surface.id, toplevel)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_OBJECT)),
            "a request from a destroyed xdg_surface must end the session"
        );
        assert_eq!(s.host_received(), []);
        assert!(
            !has_toplevel_state(&mut s, xdg_surface),
            "failed get_toplevel requests must not leave an internal Aura child"
        );
    }

    /// A forwarded `xdg_surface.get_toplevel` records the XDG role and
    /// attaches the placement Aura toplevel, whose requests follow the
    /// forwarded request on the host stream.
    #[tokio::test]
    async fn forwarded_get_toplevel_records_the_role_after_the_request() {
        use protocols::aura_shell::{zaura_shell, zaura_toplevel};
        use protocols::xdg_shell::xdg_surface;
        let (mut s, shell, xdg_surface) = session_with_an_xdg_surface().await;
        let toplevel = s.new_id();
        s.guest([xdg_surface::req::get_toplevel(xdg_surface.id, toplevel)])
            .await;
        let forwarded = s.host_received();
        let [(
            sender,
            AnyRequest::XdgSurface(xdg_surface::Request::GetToplevel { id: host_toplevel }),
        ), (
            aura_shell,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                id: aura_toplevel,
                toplevel: xdg_toplevel,
            }),
        ), (
            supporting,
            AnyRequest::ZauraToplevel(zaura_toplevel::Request::SetSupportsScreenCoordinates {}),
        )] = forwarded[..]
        else {
            panic!("the host did not get the toplevel and its Aura toplevel: {forwarded:?}");
        };
        assert_eq!(
            (sender, aura_shell, xdg_toplevel, supporting),
            (xdg_surface.host, shell, host_toplevel, aura_toplevel)
        );
    }

    /// A get_toplevel whose handler ran but whose message is then rejected
    /// (here for trailing payload bytes) records no role and creates no
    /// Aura toplevel: the deferred work is discarded with the message.
    #[tokio::test]
    async fn rejected_get_toplevel_discards_its_deferred_work() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::xdg_surface;
        let (mut s, _, xdg_surface) = session_with_an_xdg_surface().await;
        let toplevel = s.new_id();
        s.guest_fatal([raw(
            xdg_surface.id,
            xdg_surface::REQ_GET_TOPLEVEL,
            &[toplevel, 0],
        )])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
        assert_eq!(s.host_received(), []);
        assert!(!has_toplevel_state(&mut s, xdg_surface));
    }

    /// A get_toplevel that is forwarded but ends the session (here, as if
    /// its handler had failed) runs none of its deferred work: no role is
    /// recorded and no Aura toplevel is created.
    #[tokio::test]
    async fn a_forward_that_ends_the_session_discards_its_deferred_work() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::xdg_surface;
        let (mut s, _, xdg_surface) = session_with_an_xdg_surface().await;
        // Injects a fault no peer can cause: a handler that failed.
        s.ctx().end_session(SessionEnd::Internal);
        let toplevel = s.new_id();
        s.guest_fatal([xdg_surface::req::get_toplevel(xdg_surface.id, toplevel)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(s.host_received(), []);
        assert!(!has_toplevel_state(&mut s, xdg_surface));
    }

    #[tokio::test]
    async fn invalid_registry_bind_flushes_display_error_before_disconnect() {
        use protocols::wayland::{wl_display, wl_registry};
        let mut s = Session::new();
        let registry = s.registry().await;
        let name = s.advertise("wl_compositor", 1).await;
        let compositor = s.new_id();

        // Ask for the advertised global with a mismatched interface. The
        // proxy must send wl_display.error to the guest and then end the
        // session rather than silently dropping the request. Like
        // libwayland-server, it reports the bind on the registry as
        // `wl_display.invalid_object`.
        s.guest_fatal([
            wl_registry::req::bind(registry.id, name, "wl_seat", 1, compositor).unwrap(),
        ])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (
                vec![(
                    registry.id,
                    wl_registry::Event::Global {
                        name,
                        interface: "wl_compositor".to_string(),
                        version: 1
                    }
                    .into()
                )],
                (registry.id, wl_display::ERROR_INVALID_OBJECT)
            )
        );
    }

    /// The proxy implements `gtk_shell1` itself, over the host's
    /// `zaura_shell`: a `get_gtk_surface` is consumed whole and becomes
    /// the surface's Aura surface, and the proxy's own `gtk_surface1`
    /// takes the shell's local requests.
    #[tokio::test]
    async fn raw_gtk_surface_request_is_consumed_and_translated_to_aura() {
        use protocols::aura_shell::zaura_shell;
        use protocols::gtk::gtk_shell1;
        let mut s = Session::new();
        let gtk_shell = s.bind(zaura_shell::NAME, zaura_shell::VERSION).await;
        let surface = s.surface().await;
        let gtk_surface = s.new_id();
        s.guest([gtk_shell1::req::get_gtk_surface(
            gtk_shell,
            gtk_surface,
            surface.id,
        )])
        .await;
        let forwarded = s.host_received();
        assert!(
            matches!(
                &forwarded[..],
                [
                    (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { surface: host_surface, .. })),
                    (_, AnyRequest::ZauraSurface(_)),
                ] if *host_surface == surface.host
            ),
            "{forwarded:?}"
        );

        s.guest([gtk_shell1::req::system_bell(gtk_shell, Some(gtk_surface))])
            .await;
        assert_eq!(
            s.host_received(),
            [],
            "a local gtk_surface object must be accepted by a local no-op request"
        );
    }

    #[tokio::test]
    async fn synthetic_manager_destroy_emits_one_guest_delete_id() {
        use protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
        use protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
        use protocols::wayland::wl_display;
        let mut s = Session::new();
        // The guest sees the host's v1 manager as a v3 one.
        let manager = s
            .bind(
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
            )
            .await;
        s.guest([zwp_text_input_manager_v3::req::destroy(manager)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: manager }.into()
            )]
        );
        assert_eq!(s.host_received(), []);

        s.guest_fatal([zwp_text_input_manager_v3::req::destroy(manager)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_OBJECT)),
            "a retired synthetic ID must not emit duplicate delete_id events"
        );
    }

    #[tokio::test]
    async fn synthetic_shm_pool_destroy_emits_one_guest_delete_id() {
        use protocols::wayland::{wl_display, wl_shm_pool};
        let (mut s, pool) = session_with_a_shm_pool().await;
        s.guest([wl_shm_pool::req::destroy(pool)]).await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: pool }.into()
            )]
        );
        assert_eq!(s.host_received(), []);
    }

    /// The destroy of an object the host shares keeps its host ID mapped
    /// until the host's `delete_id`, which reaches the guest under its own
    /// ID.
    #[tokio::test]
    async fn destructor_mapping_survives_until_host_delete_id() {
        use protocols::wayland::{wl_display, wl_region};
        let mut s = Session::new();
        let region = region(&mut s).await;
        s.guest([wl_region::req::destroy(region.id)]).await;
        assert_eq!(
            s.host_received(),
            [(region.host, wl_region::Request::Destroy {}.into())]
        );
        assert_eq!(s.guest_received(), [], "the guest's ID waits for the host");

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, region.host)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: region.id }.into()
            )],
            "host ID must remain mapped until host delete_id"
        );
    }

    #[tokio::test]
    async fn wl_fixes_destroy_registry_forwards_host_id_until_delete_id() {
        use protocols::wayland::{wl_display, wl_fixes};
        let mut s = Session::new();
        let fixes = s.bind_paired("wl_fixes", 1).await;
        let registry = s.registry().await;
        s.guest([wl_fixes::req::destroy_registry(fixes.id, registry.id)])
            .await;
        assert_eq!(
            s.host_received(),
            [(
                fixes.host,
                wl_fixes::Request::DestroyRegistry {
                    registry: registry.host
                }
                .into()
            )],
            "the registry object argument must use its host ID"
        );
        assert_eq!(s.guest_received(), []);

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, registry.host)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: registry.id }.into()
            )]
        );
    }

    /// A host event of an object the guest has destroyed reaches neither
    /// the guest nor a handler, except a buffer's `release`, which the
    /// proxy needs to destroy a host buffer the host still showed (a native
    /// one: `a_retired_buffer_still_routes_its_release`).
    #[tokio::test]
    async fn pending_destructor_host_events_are_suppressed_except_deferred_buffer_release() {
        use protocols::wayland::{wl_buffer, wl_surface};
        let (mut s, shm) = session_with_shm().await;
        let output = s.bind_paired("wl_output", 3).await;
        let surface = s.surface().await;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(surface.host, wl_surface::Request::Destroy {}.into())]
        );
        s.host([wl_surface::evt::enter(surface.host, output.host)])
            .await;
        assert_eq!(s.guest_received(), []);

        let surface = s.surface().await;
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4])).await;
        let buffer = s.shm_buffer(pool, 1, 1).await;
        s.guest([
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        s.host_received();
        s.guest([wl_buffer::req::destroy(buffer.id)]).await;
        assert_eq!(s.host_received(), [], "the host still shows the buffer");
        s.host([wl_buffer::evt::release(buffer.host)]).await;
        assert_eq!(
            s.host_received(),
            [(buffer.host, wl_buffer::Request::Destroy {}.into())]
        );
    }

    /// The `wl_output` handlers rely on the stale-event filter instead of
    /// checking for a pending destructor themselves: no event of a
    /// released output reaches them, or the guest.
    #[tokio::test]
    async fn released_output_events_never_reach_the_output_handlers() {
        use protocols::wayland::wl_output;
        let mut s = Session::new();
        let output = s.bind_paired("wl_output", 3).await;
        s.guest([wl_output::req::release(output.id)]).await;
        assert_eq!(
            s.host_received(),
            [(output.host, wl_output::Request::Release {}.into())]
        );
        s.host([
            wl_output::evt::geometry(output.host, 0, 0, 0, 0, 0, "", "", 0).unwrap(),
            wl_output::evt::mode(output.host, 0, 1, 1, 60),
            wl_output::evt::done(output.host),
            wl_output::evt::scale(output.host, 1),
        ])
        .await;
        assert_eq!(s.guest_received(), []);
    }

    /// The proxy dispatches a host event by its sender's interface, so an
    /// event from a sender it does not track reaches no handler: a
    /// registry's global from one reaches no registry of the guest.
    #[tokio::test]
    async fn untracked_host_event_is_dropped_before_handler_state_mutation() {
        use protocols::wayland::wl_registry;
        let mut s = Session::new();
        s.registry().await;
        s.host([wl_registry::evt::global(77, 7, "wl_seat", 1).unwrap()])
            .await;
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.host_received(), []);
    }

    /// A v3 host `wl_compositor` is offered as v4, whose
    /// `wl_surface.damage_buffer` the proxy emulates: a guest surface is a
    /// v4 object, and its host surface a v3 one, which gets no v4 request.
    #[tokio::test]
    async fn created_surface_keeps_separate_guest_and_host_versions() {
        use protocols::wayland::{wl_compositor, wl_surface};
        let mut s = Session::new();
        let compositor = s.bind("wl_compositor", 3).await;
        let id = s.new_id();
        let host = s
            .create(wl_compositor::req::create_surface(compositor, id))
            .await;
        s.guest([
            wl_surface::req::damage_buffer(id, 0, 0, 1, 1),
            wl_surface::req::commit(id),
        ])
        .await;
        // The buffer damage reaches the v3 host surface as surface damage,
        // at its commit.
        let damage = wl_surface::Request::Damage {
            x: -1,
            y: -1,
            width: 3,
            height: 3,
        };
        assert_eq!(
            s.host_received(),
            [
                (host, damage.into()),
                (host, wl_surface::Request::Commit {}.into())
            ]
        );
    }

    #[tokio::test]
    async fn request_since_guard_rejects_a_newer_opcode_for_an_old_guest_object() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::{xdg_positioner, xdg_wm_base};
        const { assert!(xdg_positioner::REQ_SET_REACTIVE_SINCE > 1) };
        let mut s = Session::new();
        let wm_base = s.bind("xdg_wm_base", 1).await;
        let positioner = s.new_id();
        s.create(xdg_wm_base::req::create_positioner(wm_base, positioner))
            .await;
        s.guest_fatal([xdg_positioner::req::set_reactive(positioner)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD))
        );
        assert_eq!(s.host_received(), []);
    }

    /// The generated dispatcher rejects an event newer than its host
    /// object, which ends the session.
    #[tokio::test]
    async fn event_since_guard_rejects_a_newer_event_for_an_old_host_object() {
        use protocols::wayland::wl_display;
        use protocols::xdg_shell::{xdg_surface, xdg_toplevel, xdg_wm_base};
        const { assert!(xdg_toplevel::EVT_CONFIGURE_BOUNDS_SINCE > 3) };
        let mut s = Session::new();
        let wm_base = s.bind("xdg_wm_base", 3).await;
        let surface = s.surface().await;
        let xdg = s.new_id();
        s.create(xdg_wm_base::req::get_xdg_surface(wm_base, xdg, surface.id))
            .await;
        let toplevel = s.new_id();
        let host_toplevel = s
            .create(xdg_surface::req::get_toplevel(xdg, toplevel))
            .await;
        s.host_fatal([xdg_toplevel::evt::configure_bounds(host_toplevel, 0, 0)])
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
    }

    #[tokio::test]
    async fn request_rejects_object_argument_that_is_pending_destroy() {
        use protocols::wayland::{wl_display, wl_subcompositor, wl_surface};
        let mut s = Session::new();
        let subcompositor = s.bind("wl_subcompositor", 1).await;
        let surface = s.surface().await;
        let parent = s.surface().await;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [(surface.host, wl_surface::Request::Destroy {}.into())]
        );

        let subsurface = s.new_id();
        s.guest_fatal([wl_subcompositor::req::get_subsurface(
            subcompositor,
            subsurface,
            surface.id,
            parent.id,
        )])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_INVALID_METHOD)),
            "requests must not retain a pending-destroy object through another argument"
        );
        assert_eq!(s.host_received(), []);
        // Reads state no peer can observe: the session has ended.
        assert_eq!(
            s.ctx()
                .shadow_table
                .get_host_id(GuestId::for_test(subsurface)),
            None,
            "rejected requests must not allocate their new object"
        );
    }

    #[test]
    fn generated_forwarding_rejects_messages_that_exceed_wire_length() {
        let mut ctx = TestConnection::default().context();
        ctx.shadow_table.insert_paired_with_version_for_test(
            GuestId::for_test(10),
            HostId::for_test(20),
            Iface::WlDataSource,
            1,
        );

        let mut builder = MessageBuilder::new();
        builder.write_string(&"x".repeat(4_084));
        let payload = builder.into_payload();
        let mut msg = WireMessage::new(10, protocols::wayland::wl_data_source::REQ_OFFER, &payload);

        assert_eq!(
            raw_dispatch(crate::test_support::dispatch_request(&mut ctx, &mut msg)),
            Err(ProtocolError::MessageTooLarge(4_100)),
            "generated forwarding must not truncate the Wayland length field"
        );
    }
}
