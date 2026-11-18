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

use crate::allocator::PendingFence;
use crate::connection::{OutgoingMessages, WaylandConnection};
use crate::placement_config::PlacementConfig;
use crate::protocols;
use crate::protocols::aura_shell::{zaura_output, zaura_toplevel};
use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
use crate::protocols::wayland::wl_buffer;
use crate::state::{Context, PostDispatch, SessionEnd};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{Framing, MessageHeader, Outgoing, ProtocolError, WireMessage};
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::net::{UnixListener, UnixStream};
use tokio::time::Instant;

#[cfg(test)]
mod fuzz;
#[cfg(test)]
pub(crate) mod harness;

type DispatchResult = Result<Option<Outgoing>, ProtocolError>;

// A frame can generate many small Wayland requests (damage, attach, commit,
// callbacks, and buffer bookkeeping).  Keep one client from processing an
// unbounded stream of those requests without returning to the executor:
// keyboard/text-input events arrive on the opposite direction and otherwise
// wait behind the entire render burst on the current-thread runtime.
// Keep the fairness quantum short enough that a render burst cannot hold a
// keyboard or text-input event behind several frames.  Ghostty can emit a
// dozen small requests per frame at 4K; 32 messages still amortizes a send
// while bounding the opposite direction to roughly one frame of work.
const MAX_MESSAGES_PER_BATCH: usize = 32;

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

fn pending_host_event_is_stale(ctx: &Context, sender_id: u32, opcode: u16) -> bool {
    let interface = ctx.shadow_table.get_host_interface(sender_id);
    let allows_deferred_buffer_release = interface.is_some_and(|name| name == wl_buffer::NAME)
        && opcode == protocols::wayland::wl_buffer::EVT_RELEASE
        && ctx.host_buffer_is_guest_destroyed(sender_id);
    // An asynchronous linux-dmabuf create can legally be followed by
    // params.destroy before the compositor emits `created` or `failed`.
    // The host still owns a newly-created wl_buffer in that case, so let
    // the event reach the handler, which turns it into a host-only buffer
    // and destroys it. Dropping `created` here would leak the host buffer
    // because generated new_id mapping only runs when the event is
    // forwarded.
    let allows_orphan_dmabuf_result = interface
        .is_some_and(|name| name == zwp_linux_buffer_params_v1::NAME)
        && (opcode == protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_CREATED
            || opcode == protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_FAILED);
    ctx.shadow_table.is_pending_destroy_host(sender_id)
        && !allows_deferred_buffer_release
        && !allows_orphan_dmabuf_result
}

/// How a message the proxy cannot frame, decode or dispatch ends the
/// session.
fn rejection(direction: Direction, error: &ProtocolError) -> SessionEnd {
    match direction {
        // Tell the guest why, as libwayland-server does.
        Direction::ClientToHost => crate::handler::display::request_error(error),
        // The host broke the protocol; the guest did nothing wrong.
        Direction::HostToClient => SessionEnd::Internal,
    }
}

/// Apply the work a handler deferred until its message was forwarded (see
/// [`PostDispatch`]).
fn apply_post_dispatch(ctx: &mut Context, effect: PostDispatch) {
    match effect {
        PostDispatch::XdgToplevelCreated {
            xdg_surface,
            xdg_toplevel,
        } => crate::handler::compositor::record_xdg_toplevel(ctx, xdg_surface, xdg_toplevel),
    }
}

struct SommelierHandler {
    display: crate::handler::display::DisplayHandler,
    registry: crate::handler::registry::RegistryHandler,
    compositor: crate::handler::compositor::CompositorHandler,
    output: crate::handler::output::OutputHandler,
    gtk_shell: crate::handler::gtk_shell::GtkShellHandler,
    remote_shell: crate::handler::window_placement::RemoteShellHandler,
    callback: crate::handler::callback::CallbackHandler,
    shm: crate::handler::shm::ShmHandler,
    linux_dmabuf: crate::handler::linux_dmabuf::LinuxDmabufHandler,
    data_device: crate::handler::data_device::DataDeviceHandler,
    text_input_manager_v1: crate::handler::text_input::TextInputManagerV1Handler,
    text_input_v1: crate::handler::text_input::TextInputV1Handler,
    text_input_extension_v1: crate::handler::text_input::TextInputExtensionV1Handler,
    extended_text_input_v1: crate::handler::text_input::ExtendedTextInputV1Handler,
    text_input_manager_v3: crate::handler::text_input::TextInputManagerV3Handler,
    text_input_v3: crate::handler::text_input::TextInputV3Handler,
    keyboard: crate::handler::keyboard::KeyboardHandler,
    seat: crate::handler::seat::SeatHandler,
}

impl SommelierHandler {
    fn new() -> Self {
        Self {
            display: crate::handler::display::DisplayHandler,
            registry: crate::handler::registry::RegistryHandler,
            compositor: crate::handler::compositor::CompositorHandler,
            output: crate::handler::output::OutputHandler,
            gtk_shell: crate::handler::gtk_shell::GtkShellHandler,
            remote_shell: crate::handler::window_placement::RemoteShellHandler,
            callback: crate::handler::callback::CallbackHandler,
            shm: crate::handler::shm::ShmHandler,
            linux_dmabuf: crate::handler::linux_dmabuf::LinuxDmabufHandler,
            data_device: crate::handler::data_device::DataDeviceHandler,
            text_input_manager_v1: crate::handler::text_input::TextInputManagerV1Handler,
            text_input_v1: crate::handler::text_input::TextInputV1Handler,
            text_input_extension_v1: crate::handler::text_input::TextInputExtensionV1Handler,
            extended_text_input_v1: crate::handler::text_input::ExtendedTextInputV1Handler,
            text_input_manager_v3: crate::handler::text_input::TextInputManagerV3Handler,
            text_input_v3: crate::handler::text_input::TextInputV3Handler,
            keyboard: crate::handler::keyboard::KeyboardHandler::new(),
            seat: crate::handler::seat::SeatHandler,
        }
    }
}

/// A guest request that waits for a fence ([`Context::wait_for_fence`]).
/// It stays first in the guest stream's buffer, and the guest stream is
/// neither read nor dispatched until the request is dispatched again.
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
static FENCE_REGISTRATION_FAILED: AtomicBool = AtomicBool::new(false);

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
                        if FENCE_REGISTRATION_FAILED.swap(true, Ordering::Relaxed) {
                            log::debug!("Polling a dma-buf fence that cannot be awaited: {error}");
                        } else {
                            log::warn!("Polling a dma-buf fence that cannot be awaited: {error}");
                        }
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

    /// Resolve when `parked` is due to be dispatched again: its fence may
    /// have signalled, or its deadline has passed. Never resolves for none.
    async fn due(parked: Option<&Self>) {
        let Some(parked) = parked else {
            return std::future::pending().await;
        };
        let fence = async {
            match &parked.fence {
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
            () = tokio::time::sleep_until(parked.deadline) => {}
        }
    }
}

struct Client {
    client_conn: WaylandConnection,
    host_conn: WaylandConnection,
    ctx: Context,
    handler: SommelierHandler,
    /// Whether a batch ended the session: one of its messages did, or a send
    /// failed. No later batch dispatches anything, so the guest gets at most
    /// one error. A failed receive or a closed peer stops `run` without it.
    ended: bool,
    /// The guest request waiting for a fence, if one is.
    parked: Option<ParkedRequest>,
}

impl Client {
    fn new(client_conn: WaylandConnection, host_conn: WaylandConnection, mut ctx: Context) -> Self {
        ctx.shadow_table.create_display();
        Self {
            client_conn,
            host_conn,
            ctx,
            handler: SommelierHandler::new(),
            ended: false,
            parked: None,
        }
    }

    async fn run(mut self) {
        // When both streams already have complete messages buffered, alternate
        // directions. This gives host input (keyboard/IME) a bounded wait even
        // while a large client render burst is being drained.
        let mut prefer_host = true;
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
                () = ParkedRequest::due(self.parked.as_ref()) => {
                    if !self.handle_msgs(Direction::ClientToHost).await { break; }
                    prefer_host = true;
                }
            }
        }

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

    fn dispatch_request(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        msg: &mut WireMessage,
    ) -> DispatchResult {
        // XDG objects a placement backend implements never reach the host.
        if let Some(result) =
            crate::handler::window_placement::dispatch_facade_request(ctx, interface, msg)
        {
            return result;
        }
        let Some(family) = protocols::classify_interface(interface) else {
            return Ok(None);
        };

        // Aura and remote-shell objects are bound internally and have no
        // guest request path.
        if matches!(
            family,
            protocols::ProtocolFamily::AuraShell | protocols::ProtocolFamily::RemoteShellUnstableV2
        ) {
            return Ok(None);
        }

        protocols::dispatch_request(family, interface, msg, handler, ctx)
    }

    fn dispatch_event(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        msg: &mut WireMessage,
    ) -> DispatchResult {
        let Some(family) = protocols::classify_interface(interface) else {
            return Ok(None);
        };

        if family == protocols::ProtocolFamily::AuraShell {
            if interface == zaura_toplevel::NAME {
                return protocols::aura_shell::zaura_toplevel::dispatch_event(
                    msg,
                    &mut handler.compositor,
                    ctx,
                );
            }
            if interface == zaura_output::NAME {
                return protocols::aura_shell::zaura_output::dispatch_event(
                    msg,
                    &mut handler.output,
                    ctx,
                );
            }

            // Other Aura objects are bound internally and have no guest event
            // path. Consume their payload so an event from one of these
            // objects cannot tear down the client connection.
            msg.offset = msg.payload.len();
            return Ok(None);
        }

        protocols::dispatch_event(family, interface, msg, handler, ctx)
    }

    fn consume_event(interface: &str, msg: &mut WireMessage) -> Result<(), ProtocolError> {
        let Some(family) = protocols::classify_interface(interface) else {
            log::error!(
                "Cannot consume event for unsupported interface {} (id={}, opcode={})",
                interface,
                msg.sender_id,
                msg.opcode
            );
            return Err(ProtocolError::InvalidObjectId(msg.sender_id));
        };

        protocols::consume_event(family, interface, msg)
    }

    async fn handle_msgs(&mut self, direction: Direction) -> bool {
        if self.ended {
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

        // Preserve the ordered Wayland byte/FD stream while translating
        // messages. A single transport submission may contain several
        // messages; Wayland associates received descriptors by order, not by
        // an independently addressable byte offset. VirtWL's send path keeps
        // that same order when it chunks a large byte stream.
        let mut out = OutgoingMessages::default();
        // What the messages send back toward their source: host requests
        // while events are handled (an ack_key, say), guest events while
        // requests are.
        let mut reverse_out = OutgoingMessages::default();
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
                    // libwayland-server reports a length under 8 or over
                    // 4096 the same way (the alignment check is the proxy's
                    // own); the stream cannot be framed past it.
                    end = Some(rejection(
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
                end = Some(SessionEnd::Internal);
                break;
            }

            let packet = &conn.read_buf[offset..message_end];
            let queued_fds = conn.read_fds.len();

            let suppress_host_event = matches!(direction, Direction::HostToClient)
                && pending_host_event_is_stale(&self.ctx, sender_id, opcode);
            let interface = match direction {
                Direction::ClientToHost => self
                    .ctx
                    .shadow_table
                    .get_interface(sender_id)
                    .map(str::to_string),
                Direction::HostToClient => self
                    .ctx
                    .shadow_table
                    .get_host_interface(sender_id)
                    .map(str::to_string),
            };

            let result = if let Some(interface) = interface {
                // The message takes its descriptors from the front of the
                // queue; dropping it unforwarded closes them.
                let mut msg = WireMessage::new(sender_id, opcode, &packet[MessageHeader::LEN..])
                    .with_fds(&mut conn.read_fds);

                log::trace!("[{:?}] {}:{} (len={})", direction, interface, opcode, len);

                self.ctx.last_sender_id = sender_id;

                let res = match direction {
                    Direction::ClientToHost => Self::dispatch_request(
                        &mut self.handler,
                        &mut self.ctx,
                        &interface,
                        &mut msg,
                    ),
                    Direction::HostToClient if suppress_host_event => {
                        // A destructor has already been forwarded for this
                        // object. Decode the queued event so its payload and
                        // ordered SCM_RIGHTS descriptors are consumed, but do
                        // not invoke handlers or expose it to the guest.
                        Self::consume_event(&interface, &mut msg).map(|()| None)
                    }
                    Direction::HostToClient => {
                        Self::dispatch_event(&mut self.handler, &mut self.ctx, &interface, &mut msg)
                    }
                };
                let res = msg.check_consumed(res);
                if res.as_ref().err() == Some(&ProtocolError::TrailingData) {
                    log::error!(
                        "Protocol message left {} trailing payload byte(s): interface={}, opcode={}",
                        msg.payload.len() - msg.offset,
                        interface,
                        opcode
                    );
                }
                res
            } else if matches!(direction, Direction::ClientToHost) {
                // A guest request is only valid when its sender is a live
                // object owned by this connection. Unlike a stale host event
                // (which may have raced object teardown), silently dropping
                // an unknown client request makes the client believe the
                // request was accepted and can desynchronize protocol state.
                Err(ProtocolError::InvalidSender(sender_id))
            } else {
                if self.ctx.shadow_table.get_guest_id(sender_id).is_some() {
                    log::warn!(
                        "[{:?}] unknown id {} opcode {} (len={})",
                        direction,
                        sender_id,
                        opcode,
                        len
                    );
                } else {
                    log::debug!(
                        "[{:?}] untracked host id {} opcode {} (len={})",
                        direction,
                        sender_id,
                        opcode,
                        len
                    );
                }
                // SCM_RIGHTS descriptors form an ordered stream independent
                // of byte-buffer boundaries. A descriptor received alongside
                // a complete untracked message may belong to a later message
                // whose bytes are still partial in this buffer. Do not
                // terminate the connection merely because the unknown
                // message cannot tell us how many descriptors it consumes;
                // leave the descriptor queued for the next complete message
                // and let the connection close it if the peer goes
                // away before that message arrives.
                if !conn.read_fds.is_empty() {
                    conn.ambiguous_untracked_fd = true;
                }
                Ok(None)
            };

            let forward = match result {
                Ok(forward) => forward,
                Err(e) => {
                    let guest_id_for_log = match direction {
                        Direction::ClientToHost => Some(sender_id),
                        Direction::HostToClient => self.ctx.shadow_table.get_guest_id(sender_id),
                    };
                    let interface = guest_id_for_log
                        .and_then(|gid| {
                            self.ctx.shadow_table.get_interface(gid).map(str::to_string)
                        })
                        .unwrap_or_else(|| "unknown".to_string());
                    log::error!("Protocol error: {} (direction={:?}, id={}, guest_id={:?}, interface={}, opcode={})", e, direction, sender_id, guest_id_for_log, interface, opcode);
                    self.ctx.end_session(rejection(direction, &e));
                    None
                }
            };
            // Work deferred until the message is forwarded runs only if it
            // is, which a message that ends the session never is.
            let after_forward = self.ctx.take_post_dispatch();
            let deferred_work = after_forward.len();
            if forward.is_some() && !self.ctx.is_fatal() {
                for effect in after_forward {
                    apply_post_dispatch(&mut self.ctx, effect);
                }
            }
            // A message's effects are committed whole or not at all: one
            // that ends the session sends nothing, and dropping its forward
            // and effects closes their descriptors. The messages before it
            // were valid and are sent as usual.
            let effects = self.ctx.take_effects();
            if let Some(session_end) = effects.end {
                end = Some(session_end);
                break;
            }
            // A message that waits for a fence changed nothing, so it stays
            // buffered with everything after it, and is dispatched again
            // when it is due. Nothing overtakes it.
            if let Some(wait) = effects.wait {
                // Only a guest request waits, and only one that takes no
                // descriptors: they would be gone when it is dispatched
                // again. A host event that waited would spin the loop.
                debug_assert_eq!(direction, Direction::ClientToHost);
                debug_assert_eq!(conn.read_fds.len(), queued_fds);
                debug_assert!(
                    forward.is_none()
                        && deferred_work == 0
                        && effects.to_host.is_empty()
                        && effects.to_guest.is_empty(),
                    "a message that waits has no output"
                );
                let now = Instant::now();
                let timeout = wait.timeout;
                let parked = match resumed {
                    Some(parked) if offset == 0 => parked.wait_again(wait.fence),
                    _ => ParkedRequest::new(wait.fence, now + timeout),
                };
                if now >= parked.deadline {
                    log::error!(
                        "A dma-buf fence did not signal within {timeout:?} for a request \
                         from guest object {sender_id}; ending the session"
                    );
                    end = Some(SessionEnd::Internal);
                } else {
                    self.parked = Some(parked);
                }
                break;
            }
            // The generated dispatcher encoded the forward under the sender
            // it translated before the handler ran, so a handler that retires
            // the sender's mapping cannot change it.
            if let Some(message) = forward {
                log::debug!(
                    "  -> translated ({} bytes, {} fds)",
                    message.0.len(),
                    message.1.len()
                );
                out.push(message);
            }
            let (onward, back) = match direction {
                Direction::ClientToHost => (effects.to_host, effects.to_guest),
                Direction::HostToClient => (effects.to_guest, effects.to_host),
            };
            for message in onward {
                out.push(message);
            }
            for message in back {
                reverse_out.push(message);
            }

            offset = message_end;
            processed_messages += 1;
            if processed_messages >= MAX_MESSAGES_PER_BATCH {
                break;
            }
        }

        // A session that ends gets its one error after the valid messages.
        if let Some(end) = &end {
            let to_guest = match direction {
                Direction::ClientToHost => &mut reverse_out,
                Direction::HostToClient => &mut out,
            };
            to_guest.push(crate::handler::display::session_error(end));
        }

        // Host linux-dmabuf capability discovery emits one format event and
        // then a long modifier stream. The dmabuf handler coalesces feedback
        // refreshes within this dispatch batch; allow the next batch to
        // publish a newer table after more capabilities have arrived.
        self.ctx.synthetic_feedback_refresh_pending.clear();
        // A session that ends rolls nothing back, so in tests every batch
        // that keeps it must leave no state behind an ended object.
        #[cfg(test)]
        if end.is_none() {
            self.ctx.assert_no_orphans();
        }

        // A send passes the peer copies of the descriptors. `out` and
        // `reverse_out` still own them and close them when the batch ends,
        // whether or not their send succeeded, as on any early return.
        let mut success = end.is_none();
        if !out.is_empty() && other_conn.send_messages(&out).await.is_err() {
            success = false;
        }

        // The reverse output goes back through the source connection after
        // the forwarded messages. A forwarded wl_keyboard.key therefore
        // reaches the guest *before* its ack_key reaches the host. This is
        // intentional and safe: Exo holds the key in pending_key_acks_ with a
        // 1000 ms TTL, so the ack always arrives well within the window. The
        // C sommelier exhibits the same ordering.
        if !reverse_out.is_empty() && conn.send_messages(&reverse_out).await.is_err() {
            // success=false signals the caller (handle_msgs) to drop this
            // client connection.
            success = false;
        }

        conn.read_buf.drain(..offset);
        self.ended = !success;
        success
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::harness::Session;
    use super::*;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::state::PlaneFenceForTest;
    use crate::test_support::{
        any_fd_has_identity, fd_identity, fds_with_identity, msg_header, raw_dispatch, respell,
        shm_pool_file, stream_headers,
    };
    use crate::wire::{MessageBuilder, WL_DISPLAY_ID};
    use std::fs;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    #[test]
    fn generated_classifier_routes_each_protocol_family() {
        assert_eq!(
            protocols::classify_interface("wl_surface"),
            Some(protocols::ProtocolFamily::Wayland)
        );
        assert_eq!(
            protocols::classify_interface("xdg_toplevel"),
            Some(protocols::ProtocolFamily::XdgShell)
        );
        assert_eq!(
            protocols::classify_interface("gtk_shell1"),
            Some(protocols::ProtocolFamily::Gtk)
        );
        assert_eq!(
            protocols::classify_interface("zaura_toplevel"),
            Some(protocols::ProtocolFamily::AuraShell)
        );
        assert_eq!(protocols::classify_interface("unknown_interface"), None);
    }

    #[tokio::test]
    async fn fairness_probe_receives_preferred_host_events_between_render_batches() {
        use std::os::unix::net::UnixStream;

        let (_guest_peer, client_socket) =
            UnixStream::pair().expect("guest unix socket pair should be created");
        let (mut host_peer, host_socket) =
            UnixStream::pair().expect("host unix socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        let render_request = MessageBuilder::new().build_message(7, 0);
        client
            .client_conn
            .read_buf
            .extend_from_slice(&render_request);
        assert_eq!(
            client.next_buffered_direction(true).unwrap(),
            Some(Direction::ClientToHost),
            "an idle preferred transport must not block the buffered fallback"
        );

        let host_event = MessageBuilder::new().build_message(8, 0);
        host_peer
            .write_all(&host_event)
            .expect("queue host input event");
        assert_eq!(
            client.next_buffered_direction(true).unwrap(),
            Some(Direction::HostToClient),
            "host bytes queued after a render batch must be received before another batch"
        );
        assert_eq!(client.host_conn.read_buf, host_event);
    }

    /// A message whose header arrived in one read and whose body arrives in
    /// a later one must wait for that read, not be redispatched in a loop.
    #[test]
    fn run_receives_the_rest_of_a_partially_buffered_message() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (mut guest_peer, client_socket) =
            UnixStream::pair().expect("guest unix socket pair should be created");
        let (mut host_peer, host_socket) =
            UnixStream::pair().expect("host unix socket pair should be created");
        host_peer
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound the wait for the forwarded request");

        // wl_display.sync has opcode 0, so the opcode bytes of its header
        // read as an invalid zero length when decoded as the length field.
        const GUEST_CALLBACK: u32 = 2;
        let mut sync = MessageBuilder::new();
        sync.write_u32(GUEST_CALLBACK);
        let sync = sync.build_message(1, protocols::wayland::wl_display::REQ_SYNC);
        let (header, body) = sync.split_at(MessageHeader::LEN);
        // The body waits in the socket, so the proxy must read again to
        // complete the message whose header it has already buffered.
        guest_peer.write_all(body).expect("queue the message body");

        // Run the proxy on its own thread: a loop that spins without
        // awaiting would starve a runtime shared with this test.
        let header = header.to_vec();
        let proxy = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("proxy runtime")
                .block_on(async move {
                    let mut client = Client::new(
                        WaylandConnection::new(client_socket.into()).expect("register test socket"),
                        WaylandConnection::new(host_socket.into()).expect("register test socket"),
                        Context::new_for_test(false, false, Vec::new()),
                    );
                    client.client_conn.read_buf.extend_from_slice(&header);
                    client.run().await;
                });
        });

        let mut forwarded = [0u8; 12];
        host_peer
            .read_exact(&mut forwarded)
            .expect("the completed request must reach the host");
        let forwarded_header = msg_header(&forwarded);
        assert_eq!(
            (forwarded_header.sender_id, forwarded_header.opcode),
            (1, protocols::wayland::wl_display::REQ_SYNC)
        );

        drop(guest_peer);
        drop(host_peer);
        proxy
            .join()
            .expect("the proxy loop must end when its peers close");
    }

    /// A proxy whose peers are open sockets, with `wl_display` registered as
    /// `Client::run` registers it.
    fn client_with_socket_peers() -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        use std::os::unix::net::UnixStream;

        let (guest_peer, client_socket) =
            UnixStream::pair().expect("guest unix socket pair should be created");
        let (host_peer, host_socket) =
            UnixStream::pair().expect("host unix socket pair should be created");
        let client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );
        (client, guest_peer, host_peer)
    }

    /// The host delivers `events` to the proxy.
    async fn host_events(client: &mut Client, events: &[Vec<u8>]) {
        for event in events {
            client.host_conn.read_buf.extend_from_slice(event);
        }
        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert!(client.host_conn.read_buf.is_empty());
    }

    /// A message no encoder builds, such as a malformed one: `words` as
    /// the payload of `opcode` from `sender`.
    fn raw(sender: u32, opcode: u16, words: &[u32]) -> Outgoing {
        let mut message = MessageBuilder::new();
        words.iter().for_each(|&word| message.write_u32(word));
        (message.build_message(sender, opcode), Vec::new())
    }

    fn callback_done(host_callback: u32) -> Vec<u8> {
        let mut done = MessageBuilder::new();
        done.write_u32(0);
        done.build_message(host_callback, protocols::wayland::wl_callback::EVT_DONE)
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

    /// A guest may destroy a buffer the host still holds. Its guest object
    /// takes no more requests, but the host's `release` must still reach the
    /// handler, which then destroys the host buffer.
    #[tokio::test]
    async fn a_retired_buffer_still_routes_its_release() {
        use crate::protocols::wayland::wl_buffer;
        use crate::state::RenderBufferLifecycle;
        const GUEST: u32 = 20;
        const HOST: u32 = 30;
        let (mut client, _guest_peer, _host_peer) = client_with_socket_peers();
        let ctx = &mut client.ctx;
        ctx.shadow_table
            .insert_paired_with_version_for_test(GUEST, HOST, "wl_buffer", 1);
        assert!(ctx.register_native_buffer(HOST, (1, 1), Vec::new()));
        assert!(ctx.mark_buffer_submitted(GUEST));

        let (destroy, _) = wl_buffer::req::destroy(GUEST);
        client.client_conn.read_buf.extend_from_slice(&destroy);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        assert_eq!(client.ctx.shadow_table.request_sender(GUEST), None);
        assert!(client.ctx.shadow_table.is_pending_destroy_guest(GUEST));

        let (release, _) = wl_buffer::evt::release(HOST);
        host_events(&mut client, &[release]).await;
        assert_eq!(
            client
                .ctx
                .render_buffer_lifecycle_for_host(crate::state::HostId(HOST)),
            Some(RenderBufferLifecycle::HostDestroyQueued)
        );
    }

    /// CPU time this thread has used.
    fn thread_cpu_time() -> std::time::Duration {
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

    fn test_client_config() -> ClientConfig {
        ClientConfig {
            local_compositor: None,
            virtio_wayland: None,
            gpu_accel: false,
            xdg_decoration: false,
            vm_identifier: "termina".to_string(),
            placement: Arc::new(PlacementConfig::for_test(Vec::new())),
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
        let cpu_before = thread_cpu_time();
        let served = tokio::task::LocalSet::new()
            .run_until(tokio::time::timeout(
                window,
                serve(
                    listener,
                    test_client_config(),
                    None::<tokio::signal::unix::Signal>,
                ),
            ))
            .await;
        let cpu = thread_cpu_time() - cpu_before;

        assert!(served.is_err(), "serve runs until the process ends");
        assert!(
            cpu < window / 4,
            "the accept loop used {cpu:?} of CPU in {window:?} of failing accepts"
        );
    }

    /// A reload without a shortcuts file is a logged no-op: the proxy keeps
    /// serving, and keeps taking reloads. Tests send reloads through a
    /// channel; `SIGHUP` is the process's, so only `main`'s test and
    /// `a_real_sighup_reloads_the_shortcuts_file` raise it.
    #[tokio::test(start_paused = true)]
    async fn a_no_op_reload_keeps_the_proxy_and_its_reloads() {
        let dir = crate::test_support::TempDir::new("sighup");
        let listener = UnixListener::bind(dir.path().join("socket")).expect("bind listener");
        let config = test_client_config();
        assert!(config.placement.reload().is_none(), "no file to reload");
        let (reload, reloads) = tokio::sync::mpsc::channel(1);
        let local = tokio::task::LocalSet::new();
        let serving = local.spawn_local(serve(listener, config, Some(reloads)));
        local
            .run_until(async {
                // The channel holds one reload, so each send after the
                // first waits until the reload task has taken the one
                // before. The clock is paused, so it moves only once every
                // task waits: the timeout fails a reload nothing takes.
                for reload_number in 1..=3 {
                    tokio::time::timeout(std::time::Duration::from_secs(1), reload.send(()))
                        .await
                        .unwrap_or_else(|_| panic!("reload {reload_number} is never taken"))
                        .expect("the reload task takes reloads");
                }
            })
            .await;
        assert!(!serving.is_finished(), "the proxy keeps serving");
    }

    /// A real `SIGHUP` reaches a reload task, as `serve` runs one;
    /// `reload_does_not_wait_for_the_accept_retry_pause` covers `serve`
    /// running it. The signal is the process's, so any other test
    /// listening for it only reloads a configuration of its own once more.
    #[tokio::test]
    async fn a_real_sighup_reloads_the_shortcuts_file() {
        use crate::window_layout::WindowLayoutAction;

        let alt_q = crate::accelerator::parse_accelerator("<Alt>q").unwrap();
        let dir = crate::test_support::TempDir::new("sighup");
        let bind_alt_q = |action: &str| {
            dir.write(
                "shortcuts.toml",
                format!("version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"{action}\"\n")
                    .as_bytes(),
            )
        };
        let shortcuts = bind_alt_q("left");
        let placement = Arc::new(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1")],
            Some(&shortcuts),
            crate::arc_task_ids::ArcIdAllocator::block_for_test(),
        ));
        let reloads = reload_on_signal(
            install_reload_signal().expect("install the SIGHUP handler"),
            Arc::clone(&placement),
        );
        bind_alt_q("right");
        // Without the handler this would terminate the test process.
        assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
        let applied = async {
            while placement.action_for(&alt_q) != Some(WindowLayoutAction::Right) {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                () = reloads => panic!("the SIGHUP stream ended"),
                () = applied => {}
            }
        })
        .await
        .expect("the SIGHUP reload applies");
    }

    /// A reload must not wait for the accept loop's retry pause, and every
    /// reload applies the file as it is then.
    #[tokio::test(start_paused = true)]
    async fn reload_does_not_wait_for_the_accept_retry_pause() {
        use crate::window_layout::WindowLayoutAction;
        use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};

        let alt_q = crate::accelerator::parse_accelerator("<Alt>q").unwrap();
        let dir = crate::test_support::TempDir::new("reload-during-retry");
        let bind_alt_q = |action: &str| {
            dir.write(
                "shortcuts.toml",
                format!("version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"{action}\"\n")
                    .as_bytes(),
            )
        };
        let shortcuts = bind_alt_q("left");
        let placement = Arc::new(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1")],
            Some(&shortcuts),
            crate::arc_task_ids::ArcIdAllocator::block_for_test(),
        ));
        assert_eq!(placement.action_for(&alt_q), Some(WindowLayoutAction::Left));
        let config = ClientConfig {
            placement: Arc::clone(&placement),
            ..test_client_config()
        };
        // Every accept on a socket that never listened fails at once, so the
        // accept loop is in its retry pause from the first step on.
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

        let (reload, reloads) = tokio::sync::mpsc::channel(1);
        let local = tokio::task::LocalSet::new();
        local.spawn_local(serve(listener, config, Some(reloads)));
        local
            .run_until(async {
                tokio::task::yield_now().await;
                for (name, action) in [
                    ("right", WindowLayoutAction::Right),
                    ("left", WindowLayoutAction::Left),
                ] {
                    let started = tokio::time::Instant::now();
                    bind_alt_q(name);
                    reload
                        .send(())
                        .await
                        .expect("the reload task takes reloads");
                    // The clock is paused, so it moves only while every task
                    // waits; the timeout fails a reload that never applies
                    // instead of hanging.
                    let applied = async {
                        while placement.action_for(&alt_q) != Some(action) {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                    };
                    tokio::time::timeout(ACCEPT_RETRY_DELAY * 10, applied)
                        .await
                        .unwrap_or_else(|_| panic!("the reload to {action:?} applies"));
                    assert!(
                        started.elapsed() < ACCEPT_RETRY_DELAY,
                        "the reload waited {:?} for the accept loop",
                        started.elapsed()
                    );
                }
            })
            .await;
    }

    /// A VirtWL transaction carries at most 28 descriptors, so a batch that
    /// forwards more must be split, between messages.
    #[tokio::test]
    async fn batches_send_at_most_the_transport_descriptor_limit_at_once() {
        use crate::connection::MAX_FDS_PER_SEND;
        use std::os::fd::AsRawFd;

        const GUEST_OFFER: u32 = 30;
        const HOST_OFFER: u32 = 31;
        const RECEIVES: usize = MAX_FDS_PER_SEND + 2;
        let (mut client, guest_peer, host_peer) = client_with_socket_peers();
        client
            .ctx
            .shadow_table
            .insert_paired_for_test(GUEST_OFFER, HOST_OFFER, "wl_data_offer");

        // The guest asks for the selection once per descriptor, in one write
        // the proxy dispatches as one batch.
        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let mut receive = MessageBuilder::new();
        receive.write_string("text/plain");
        let receive =
            receive.build_message(GUEST_OFFER, protocols::wayland::wl_data_offer::REQ_RECEIVE);
        let requests = receive.repeat(RECEIVES);
        let fds = [pipe_write.as_raw_fd(); RECEIVES];
        let sent = nix::sys::socket::sendmsg::<()>(
            guest_peer.as_raw_fd(),
            &[std::io::IoSlice::new(&requests)],
            &[nix::sys::socket::ControlMessage::ScmRights(&fds)],
            nix::sys::socket::MsgFlags::empty(),
            None,
        )
        .expect("send the guest requests");
        assert_eq!(sent, requests.len());
        drop((pipe_read, pipe_write));
        assert_eq!(
            client
                .client_conn
                .recv()
                .await
                .expect("receive the requests"),
            requests.len()
        );
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        drop(client);

        // Read the host side one submission at a time.
        let mut received_bytes = 0;
        let mut per_submission = Vec::new();
        loop {
            let mut data = [0u8; 4096];
            let mut control = nix::cmsg_space!([RawFd; 253]);
            let mut iov = [std::io::IoSliceMut::new(&mut data)];
            let message = nix::sys::socket::recvmsg::<()>(
                host_peer.as_raw_fd(),
                &mut iov,
                Some(&mut control),
                nix::sys::socket::MsgFlags::empty(),
            )
            .expect("receive from the proxy");
            if message.bytes == 0 {
                break;
            }
            received_bytes += message.bytes;
            let mut fds = Vec::new();
            for cmsg in message.cmsgs().expect("control messages") {
                if let nix::sys::socket::ControlMessageOwned::ScmRights(received) = cmsg {
                    fds.extend(received);
                }
            }
            for &fd in &fds {
                unsafe { libc::close(fd) };
            }
            per_submission.push(fds.len());
        }
        drop(guest_peer);

        assert_eq!(received_bytes, requests.len());
        assert_eq!(per_submission.iter().sum::<usize>(), RECEIVES);
        assert!(
            per_submission
                .iter()
                .all(|&count| count <= MAX_FDS_PER_SEND),
            "descriptors per submission: {per_submission:?}"
        );
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

    #[tokio::test]
    async fn message_batches_leave_buffered_frames_for_fair_dispatch() {
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        // Use an untracked host event so dispatch has no protocol state to
        // mutate. The batch limit itself is what this regression test covers.
        let message = MessageBuilder::new().build_message(77, 0);
        for _ in 0..=MAX_MESSAGES_PER_BATCH {
            client.host_conn.read_buf.extend_from_slice(&message);
        }
        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert!(
            client.host_conn.has_complete_message(),
            "a bounded batch must leave complete frames for the next loop turn"
        );
        assert_eq!(
            client.host_conn.read_buf.len(),
            message.len(),
            "exactly one frame should remain after the first bounded batch"
        );

        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert!(client.host_conn.read_buf.is_empty());
    }

    fn assert_fd_released(fd: std::os::unix::io::RawFd, target: &std::path::Path) {
        let current = fs::read_link(format!("/proc/self/fd/{fd}"));
        assert!(
            current.as_ref().map_or(true, |current| current != target),
            "descriptor {fd} still refers to its owned resource: {current:?}"
        );
    }

    const STALE_GUEST_KEYBOARD: u32 = 10;
    const STALE_HOST_KEYBOARD: u32 = 20;
    const LIVE_GUEST_KEYBOARD: u32 = 11;
    const LIVE_HOST_KEYBOARD: u32 = 21;

    fn pending_keyboard_client() -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        use std::os::unix::net::UnixStream;

        let (guest_peer, guest_socket) =
            UnixStream::pair().expect("guest unix socket pair should be created");
        let (_host_peer, host_socket) =
            UnixStream::pair().expect("host unix socket pair should be created");
        guest_peer
            .set_nonblocking(true)
            .expect("guest peer should become nonblocking");
        let mut client = Client::new(
            WaylandConnection::new(guest_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        for (guest_id, host_id) in [
            (STALE_GUEST_KEYBOARD, STALE_HOST_KEYBOARD),
            (LIVE_GUEST_KEYBOARD, LIVE_HOST_KEYBOARD),
        ] {
            client.ctx.shadow_table.insert_paired_with_version_for_test(
                guest_id,
                host_id,
                "wl_keyboard",
                10,
            );
        }
        client.ctx.guest_destroyed(STALE_GUEST_KEYBOARD);

        (client, guest_peer, _host_peer)
    }

    /// A valid keymap event from the destroyed keyboard, and the identity
    /// of its descriptor, which travels apart from its bytes. Were its
    /// handler to run, it would load the keymap for the destroyed keyboard,
    /// state the orphan check after the batch reports.
    fn stale_keymap_event() -> (Vec<u8>, OwnedFd, crate::test_support::TestFdIdentity) {
        use crate::protocols::wayland::wl_keyboard;
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the default keymap");
        // The size counts the NUL that ends the keymap text.
        let mut text = keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        text.push(0);
        let file = OwnedFd::from(shm_pool_file(&text));
        let identity = fd_identity(file.as_raw_fd());
        let (event, mut fds) = wl_keyboard::evt::keymap(
            STALE_HOST_KEYBOARD,
            wl_keyboard::KEYMAP_FORMAT_XKB_V1,
            file,
            text.len() as u32,
        );
        (event, fds.pop().expect("the keymap's descriptor"), identity)
    }

    fn repeat_info_event(keyboard: u32) -> Vec<u8> {
        use crate::protocols::wayland::wl_keyboard;

        let mut repeat_info = MessageBuilder::new();
        repeat_info.write_i32(25);
        repeat_info.write_i32(400);
        repeat_info.build_message(keyboard, wl_keyboard::EVT_REPEAT_INFO)
    }

    fn assert_live_repeat_info_forwarded(
        guest_peer: &mut std::os::unix::net::UnixStream,
        expected: &[u8],
    ) {
        use std::io::Read;

        let mut forwarded = vec![0; expected.len()];
        guest_peer
            .read_exact(&mut forwarded)
            .expect("the live follow-up event must reach the guest");
        assert_eq!(forwarded, expected);
    }

    #[tokio::test]
    async fn pending_destroy_event_consumes_known_fd_before_live_event_in_same_batch() {
        let (mut client, mut guest_peer, _host_peer) = pending_keyboard_client();
        let (keymap, keymap_fd, keymap_identity) = stale_keymap_event();
        let live_repeat_info = repeat_info_event(LIVE_HOST_KEYBOARD);
        let expected_repeat_info = repeat_info_event(LIVE_GUEST_KEYBOARD);

        client.host_conn.read_buf.extend_from_slice(&keymap);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&live_repeat_info);
        client.host_conn.read_fds.push_back(keymap_fd);

        assert!(
            client.handle_msgs(Direction::HostToClient).await,
            "a known stale event with an FD must not terminate the connection"
        );
        assert!(client.host_conn.read_buf.is_empty());
        assert!(client.host_conn.read_fds.is_empty());
        assert!(
            !client.host_conn.ambiguous_untracked_fd,
            "schema-aware consumption must not poison later FD ordering"
        );
        assert!(
            client
                .ctx
                .shadow_table
                .is_pending_destroy_guest(STALE_GUEST_KEYBOARD),
            "dropping a stale event must not complete object teardown"
        );
        assert_live_repeat_info_forwarded(&mut guest_peer, &expected_repeat_info);
        assert!(
            !any_fd_has_identity(keymap_identity),
            "the stale keymap's descriptor is closed"
        );
    }

    #[tokio::test]
    async fn pending_destroy_event_consumes_known_fd_before_partial_followup() {
        use std::io::Read;

        let (mut client, mut guest_peer, _host_peer) = pending_keyboard_client();
        let (keymap, keymap_fd, keymap_identity) = stale_keymap_event();
        let live_repeat_info = repeat_info_event(LIVE_HOST_KEYBOARD);
        let expected_repeat_info = repeat_info_event(LIVE_GUEST_KEYBOARD);

        client.host_conn.read_buf.extend_from_slice(&keymap);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&live_repeat_info[..4]);
        client.host_conn.read_fds.push_back(keymap_fd);

        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert_eq!(
            client.host_conn.read_buf,
            live_repeat_info[..4],
            "the incomplete follow-up must remain buffered"
        );
        assert!(client.host_conn.read_fds.is_empty());
        assert!(!client.host_conn.ambiguous_untracked_fd);
        let mut premature = [0u8; 1];
        let read_error = guest_peer
            .read(&mut premature)
            .expect_err("an incomplete live event must not be forwarded");
        assert_eq!(read_error.kind(), io::ErrorKind::WouldBlock);
        assert!(
            !any_fd_has_identity(keymap_identity),
            "the stale keymap's descriptor is closed"
        );

        client
            .host_conn
            .read_buf
            .extend_from_slice(&live_repeat_info[4..]);
        assert!(
            client.handle_msgs(Direction::HostToClient).await,
            "the completed live follow-up must remain decodable"
        );
        assert!(client.host_conn.read_buf.is_empty());
        assert_live_repeat_info_forwarded(&mut guest_peer, &expected_repeat_info);
    }

    #[tokio::test]
    async fn untracked_message_fd_is_retained_when_followup_is_partial() {
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let pending_fd =
            unsafe { libc::fcntl(pipe_write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        drop((pipe_read, pipe_write));

        // Sender 77 is complete but deliberately untracked. The four bytes
        // after it are only a partial header for the next message. There is no
        // interface metadata with which to determine whether the descriptor
        // belongs to the stale message or the partial follow-up, so the
        // descriptor must remain queued while the complete stale message is
        // dropped. The connection stays alive for the rest of the follow-up.
        let stale_event = MessageBuilder::new().build_message(77, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        client.host_conn.read_buf.extend_from_slice(&[0, 0, 0, 0]);
        client
            .host_conn
            .read_fds
            .push_back(unsafe { OwnedFd::from_raw_fd(pending_fd) });

        assert!(
            client.handle_msgs(Direction::HostToClient).await,
            "a pending FD after an untracked complete message must not terminate the connection"
        );
        assert!(
            client.host_conn.read_buf.len() == 4,
            "only the complete stale message should be consumed"
        );
        assert_eq!(
            client
                .host_conn
                .read_fds
                .iter()
                .map(AsRawFd::as_raw_fd)
                .collect::<Vec<_>>(),
            vec![pending_fd],
            "the pending descriptor must remain ordered for the later message"
        );
        drop(client);

        assert_fd_released(pending_fd, &pending_target);
    }

    #[tokio::test]
    async fn untracked_message_fd_ambiguity_survives_separate_receive_calls() {
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let pending_fd =
            unsafe { libc::fcntl(pipe_write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        drop((pipe_read, pipe_write));

        let stale_event = MessageBuilder::new().build_message(77, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        // Keep only a partial header for the follow-up message in the first
        // receive batch, exactly as a stream socket may do.
        let known_event = MessageBuilder::new().build_message(88, 0);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&known_event[..4]);
        client
            .host_conn
            .read_fds
            .push_back(unsafe { OwnedFd::from_raw_fd(pending_fd) });
        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert_eq!(client.host_conn.read_buf.len(), 4);

        // The rest of the follow-up arrives in a later recv/handle_msgs call.
        // The ambiguity marker must survive that boundary and terminate
        // before the pending descriptor can be attached to this message.
        client
            .host_conn
            .read_buf
            .extend_from_slice(&known_event[4..]);
        client
            .ctx
            .shadow_table
            .insert_host_only_for_test(88, "wl_display", 1);
        assert!(
            !client.handle_msgs(Direction::HostToClient).await,
            "descriptor ambiguity must remain fatal across receive calls"
        );
        drop(client);

        assert_fd_released(pending_fd, &pending_target);
    }

    #[tokio::test]
    async fn untracked_message_fd_ambiguity_terminates_before_followup_complete() {
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        let (pipe_read, pipe_write) = crate::test_support::pipe();
        let pending_fd =
            unsafe { libc::fcntl(pipe_write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        drop((pipe_read, pipe_write));

        // Both messages are complete, but the first sender is untracked. The
        // descriptor arrived on the ordered stream with these bytes and
        // cannot be assigned to either message. The proxy must not guess and
        // accidentally deliver it to the known follow-up.
        let stale_event = MessageBuilder::new().build_message(77, 0);
        let known_event = MessageBuilder::new().build_message(88, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        client.host_conn.read_buf.extend_from_slice(&known_event);
        client
            .host_conn
            .read_fds
            .push_back(unsafe { OwnedFd::from_raw_fd(pending_fd) });
        client
            .ctx
            .shadow_table
            .insert_host_only_for_test(88, "wl_display", 1);

        assert!(
            !client.handle_msgs(Direction::HostToClient).await,
            "complete untracked message plus a complete follow-up is fatal FD ambiguity"
        );
        drop(client);

        assert_fd_released(pending_fd, &pending_target);
    }

    /// Descriptor ambiguity ends the session like any host protocol error:
    /// the valid events before it reach the guest, then one error.
    #[tokio::test]
    async fn descriptor_ambiguity_is_reported_after_the_valid_events() {
        use protocols::wayland::{wl_data_device, wl_display};
        const DATA_DEVICE: u32 = 10;
        const HOST_DATA_DEVICE: u32 = 80;
        let (mut client, mut guest_peer, _host_peer) = client_with_socket_peers();
        client.ctx.shadow_table.insert_paired_with_version_for_test(
            DATA_DEVICE,
            HOST_DATA_DEVICE,
            "wl_data_device",
            3,
        );
        let (pending, _) = std::os::unix::net::UnixStream::pair().expect("a descriptor");
        let mut offer = MessageBuilder::new();
        offer.write_u32(0xff00_0001);
        let offer = offer.build_message(HOST_DATA_DEVICE, wl_data_device::EVT_DATA_OFFER);
        // An untracked message with a descriptor it may or may not own, then
        // a complete follow-up the descriptor might belong to.
        let stale_event = MessageBuilder::new().build_message(77, 0);
        let follow_up = MessageBuilder::new().build_message(HOST_DATA_DEVICE, 0);
        for event in [offer, stale_event, follow_up] {
            client.host_conn.read_buf.extend_from_slice(&event);
        }
        client.host_conn.read_fds.push_back(pending.into());

        assert!(!client.handle_msgs(Direction::HostToClient).await);
        drop(client);
        let mut events = Vec::new();
        std::io::Read::read_to_end(&mut guest_peer, &mut events).expect("read the guest's events");
        let headers: Vec<_> = crate::test_support::frames(&events)
            .map(|(header, _)| (header.sender_id, header.opcode))
            .collect();
        assert_eq!(
            headers,
            [
                (DATA_DEVICE, wl_data_device::EVT_DATA_OFFER),
                (crate::wire::WL_DISPLAY_ID, wl_display::EVT_ERROR),
            ]
        );
        assert_eq!(
            display_errors_in(&events),
            [(crate::wire::WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)]
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

    /// A guest `xdg_toplevel` of a new surface, and the host's.
    async fn session_with_a_toplevel() -> (Session, super::harness::Object) {
        use protocols::xdg_shell::{xdg_surface, xdg_wm_base};
        let (mut s, surface) = session_with_a_surface().await;
        let wm_base = s.bind_paired("xdg_wm_base", xdg_wm_base::VERSION).await;
        let xdg_surface = s.new_id();
        s.create(xdg_wm_base::req::get_xdg_surface(
            wm_base.id,
            xdg_surface,
            surface.id,
        ))
        .await;
        let id = s.new_id();
        let host = s
            .create(xdg_surface::req::get_toplevel(xdg_surface, id))
            .await;
        (s, super::harness::Object { id, host })
    }

    /// libwayland passes a string that is not UTF-8 through, so a client
    /// that sets its title from Latin-1 bytes keeps its session: the host
    /// gets the title with each invalid sequence replaced by U+FFFD.
    #[tokio::test]
    async fn a_guest_string_that_is_not_utf8_reaches_the_host_with_replacement_characters() {
        use protocols::xdg_shell::xdg_toplevel;
        let (mut s, toplevel) = session_with_a_toplevel().await;
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
        let (mut s, toplevel) = session_with_a_toplevel().await;
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
        use protocols::wayland::wl_shm;
        let mut s = Session::new();
        let shm = s.bind("wl_shm", wl_shm::VERSION).await;
        let pool = s.new_id();
        s.guest([wl_shm::req::create_pool(
            shm,
            pool,
            shm_pool_file(&[0; 64]).into(),
            64,
        )])
        .await;
        assert_eq!(s.host_received(), [], "the pool is the proxy's own");
        (s, pool)
    }

    /// A handler that ends the session sends nothing, not even the message
    /// it returns to be forwarded: a `created` whose buffer the proxy cannot
    /// track must not hand the guest a buffer.
    #[tokio::test]
    async fn a_failed_handler_forwards_nothing() {
        use crate::state::{HostId, PendingNativeCreate};
        use protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
        use protocols::wayland::wl_display;
        const PARAMS: u32 = 10;
        const HOST_PARAMS: u32 = 80;
        const HOST_BUFFER: u32 = 0xff00_0001;
        let (mut client, mut guest_peer, _host_peer) = client_with_socket_peers();
        let ctx = &mut client.ctx;
        ctx.shadow_table
            .insert_paired_for_test(PARAMS, HOST_PARAMS, "zwp_linux_buffer_params_v1");
        ctx.pending_native_creates.insert(
            HostId(HOST_PARAMS),
            PendingNativeCreate {
                guest_params_id: PARAMS,
                dimensions: (16, 8),
                sync_fds: Vec::new(),
            },
        );
        // The proxy already tracks a buffer under the ID the host creates.
        assert!(ctx.register_native_buffer(HOST_BUFFER, (16, 8), Vec::new()));
        let mut created = MessageBuilder::new();
        created.write_u32(HOST_BUFFER);
        client
            .host_conn
            .read_buf
            .extend(created.build_message(HOST_PARAMS, zwp_linux_buffer_params_v1::EVT_CREATED));

        assert!(!client.handle_msgs(Direction::HostToClient).await);
        drop(client);
        let mut events = Vec::new();
        std::io::Read::read_to_end(&mut guest_peer, &mut events).expect("read the guest's events");
        assert_eq!(
            stream_headers(&events),
            [(crate::wire::WL_DISPLAY_ID, wl_display::EVT_ERROR)]
        );
        assert_eq!(
            display_errors_in(&events),
            [(crate::wire::WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)]
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
        use protocols::wayland::{wl_display, wl_registry};
        const REGISTRY: u32 = 10;
        const HOST_REGISTRY: u32 = 20;
        // The shell's ID, which is also its global's name. The object table
        // does not track it, so retiring it fails.
        const SHELL: u32 = 40;
        let (mut client, mut guest_peer, mut host_peer) = client_with_socket_peers();
        let ctx = &mut client.ctx;
        ctx.shadow_table
            .insert_paired_for_test(REGISTRY, HOST_REGISTRY, "wl_registry");
        ctx.bind_aura_shell_for_test(SHELL);
        ctx.registry_global_names
            .entry(HOST_REGISTRY)
            .or_default()
            .insert(SHELL);
        ctx.global_generations.insert(SHELL, 1);
        ctx.registry_global_generations
            .entry(HOST_REGISTRY)
            .or_default()
            .insert(SHELL, 1);
        let mut remove = MessageBuilder::new();
        remove.write_u32(SHELL);
        client
            .host_conn
            .read_buf
            .extend(remove.build_message(HOST_REGISTRY, wl_registry::EVT_GLOBAL_REMOVE));

        assert!(!client.handle_msgs(Direction::HostToClient).await);
        drop(client);
        assert_eq!(sent_messages(&mut host_peer), Vec::new());
        assert_eq!(
            display_errors(&mut guest_peer),
            [(crate::wire::WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)]
        );
    }

    /// A message that ends the session discards what its handler queued
    /// before the failure: a dma-buf `create` whose host params generation
    /// is already pending must not send its plane and create to the host.
    #[tokio::test]
    async fn a_handler_error_discards_its_earlier_output() {
        use crate::state::{HostId, PendingNativeCreate, PendingParam};
        use protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
        use protocols::wayland::wl_display;
        const PARAMS: u32 = 10;
        const HOST_PARAMS: u32 = 80;
        const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;
        let (mut client, mut guest_peer, mut host_peer) = client_with_socket_peers();
        let ctx = &mut client.ctx;
        ctx.shadow_table
            .insert_paired_for_test(PARAMS, HOST_PARAMS, "zwp_linux_buffer_params_v1");
        let (plane, _) = std::os::unix::net::UnixStream::pair().expect("a descriptor");
        ctx.pending_params.insert(
            PARAMS,
            vec![PendingParam {
                fd: plane.into(),
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        ctx.pending_native_creates.insert(
            HostId(HOST_PARAMS),
            PendingNativeCreate {
                guest_params_id: PARAMS,
                dimensions: (16, 8),
                sync_fds: Vec::new(),
            },
        );
        let mut create = MessageBuilder::new();
        for word in [16, 8, DRM_FORMAT_ARGB8888, 0] {
            create.write_u32(word);
        }
        client
            .client_conn
            .read_buf
            .extend(create.build_message(PARAMS, zwp_linux_buffer_params_v1::REQ_CREATE));

        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        drop(client);
        assert_eq!(sent_messages(&mut host_peer), Vec::new());
        assert_eq!(
            display_errors(&mut guest_peer),
            [(crate::wire::WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)]
        );
    }

    /// The object and code of each `wl_display.error` the guest received
    /// before the proxy closed the connection.
    fn display_errors(guest_peer: &mut std::os::unix::net::UnixStream) -> Vec<(u32, u32)> {
        use std::io::Read;
        let mut events = Vec::new();
        guest_peer
            .read_to_end(&mut events)
            .expect("read the guest's events");
        display_errors_in(&events)
    }

    /// The object and code of each `wl_display.error` in `events`.
    fn display_errors_in(events: &[u8]) -> Vec<(u32, u32)> {
        crate::test_support::frames(events)
            .filter(|(header, _)| {
                (header.sender_id, header.opcode)
                    == (
                        crate::wire::WL_DISPLAY_ID,
                        protocols::wayland::wl_display::EVT_ERROR,
                    )
            })
            .map(|(header, payload)| {
                let mut error = WireMessage::new(header.sender_id, header.opcode, payload);
                (error.read_u32().unwrap(), error.read_u32().unwrap())
            })
            .collect()
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

    /// The guest's `wl_surface` in [`client_with_a_surface`].
    const SURFACE: u32 = 5;
    /// The host's [`SURFACE`].
    const SURFACE_HOST: u32 = 6;

    /// A proxy whose guest has the v4 `wl_surface` [`SURFACE`].
    fn client_with_a_surface() -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        let (mut client, guest_peer, host_peer) = client_with_socket_peers();
        client.ctx.shadow_table.insert_paired_with_version_for_test(
            SURFACE,
            SURFACE_HOST,
            "wl_surface",
            4,
        );
        (client, guest_peer, host_peer)
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

    /// The native buffer of [`client_committing_a_fenced_buffer`], and its
    /// host ID.
    const FENCED_BUFFER: u32 = 7;
    const HOST_FENCED_BUFFER: u32 = 8;
    /// A callback the host can finish while the guest stream is parked.
    const CALLBACK: u32 = 9;
    const HOST_CALLBACK: u32 = 10;
    /// The callback of the `wl_display.sync` the guest sends after its commit.
    const SYNC_CALLBACK: u32 = 11;

    /// Thirty planes that share one fence give a commit a 30 s deadline,
    /// which no loaded test machine reaches, so a test that runs the proxy
    /// in real time fails only when the commit is held back.
    const SLOW_DEADLINE_PLANES: usize = 30;

    /// A proxy whose guest has buffered an attach of [`FENCED_BUFFER`] to
    /// [`SURFACE`], its commit and a `wl_display.sync`. The buffer has
    /// `planes` planes that share one fence, which `stand_in` describes;
    /// the guest GPU writes the buffer until the returned descriptor is
    /// written.
    fn client_committing_a_fenced_buffer(
        stand_in: PlaneFenceForTest,
        planes: usize,
    ) -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
        OwnedFd,
    ) {
        use protocols::wayland::{wl_display, wl_surface};
        let (mut client, guest_peer, host_peer) = client_with_a_surface();
        let ctx = &mut client.ctx;
        ctx.plane_fences_for_test = Some(stand_in);
        ctx.shadow_table.insert_paired_with_version_for_test(
            FENCED_BUFFER,
            HOST_FENCED_BUFFER,
            "wl_buffer",
            1,
        );
        let (fence, signal) = crate::test_support::pipe();
        let planes = (0..planes)
            .map(|_| fence.try_clone().expect("share the fence"))
            .collect();
        assert!(ctx.register_native_buffer(HOST_FENCED_BUFFER, (1, 1), planes));
        ctx.shadow_table
            .insert_paired_for_test(CALLBACK, HOST_CALLBACK, "wl_callback");
        for (request, _) in [
            wl_surface::req::attach(SURFACE, Some(FENCED_BUFFER), 0, 0),
            wl_surface::req::commit(SURFACE),
            wl_display::req::sync(crate::wire::WL_DISPLAY_ID, SYNC_CALLBACK),
        ] {
            client.client_conn.read_buf.extend_from_slice(&request);
        }
        (client, guest_peer, host_peer, signal)
    }

    /// `peer` as a Tokio stream, so a test can drive it beside
    /// `Client::run` on the same thread.
    fn async_peer(peer: std::os::unix::net::UnixStream) -> tokio::net::UnixStream {
        peer.set_nonblocking(true).expect("non-blocking peer");
        tokio::net::UnixStream::from_std(peer).expect("register the peer")
    }

    /// The next message the proxy sends `peer`.
    async fn read_frame(peer: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut frame = vec![0; MessageHeader::LEN];
        peer.read_exact(&mut frame).await.expect("read a header");
        frame.resize(msg_header(&frame).len, 0);
        peer.read_exact(&mut frame[MessageHeader::LEN..])
            .await
            .expect("read a body");
        frame
    }

    /// The headers of what the host reads up to the guest's
    /// `wl_display.sync`, which follows the commit, waiting at most `limit`.
    async fn forwarded_through_sync(
        host: &mut (impl tokio::io::AsyncRead + Unpin),
        limit: Duration,
    ) -> Vec<(u32, u16)> {
        let sync = (
            crate::wire::WL_DISPLAY_ID,
            protocols::wayland::wl_display::REQ_SYNC,
        );
        let read = async {
            let mut headers = Vec::new();
            while headers.last() != Some(&sync) {
                headers.extend(stream_headers(&read_frame(host).await));
            }
            headers
        };
        tokio::time::timeout(limit, read)
            .await
            .expect("the commit and the request after it reach the host")
    }

    /// Assert that `forwarded` has one commit, as the guest sent it (a
    /// commit has no arguments, so its header is all its bytes), and that
    /// the request after it follows it.
    fn assert_one_commit_then_sync(forwarded: &[(u32, u16)]) {
        use protocols::wayland::{wl_display, wl_surface};
        let commit = (SURFACE_HOST, wl_surface::REQ_COMMIT);
        assert_eq!(forwarded.iter().filter(|&&m| m == commit).count(), 1);
        assert_eq!(
            forwarded[forwarded.len() - 2..],
            [commit, (crate::wire::WL_DISPLAY_ID, wl_display::REQ_SYNC)]
        );
    }

    /// A commit whose fence is pending parks the guest stream: neither it
    /// nor the request after it is dispatched, fair dispatch does not offer
    /// the stream, and the commit keeps its deadline when it waits again,
    /// while host events still reach the guest.
    #[tokio::test]
    async fn a_pending_fence_parks_the_guest_stream_while_host_events_flow() {
        use protocols::wayland::{wl_callback, wl_surface};
        let (mut client, mut guest_peer, mut host_peer, _signal) =
            client_committing_a_fenced_buffer(PlaneFenceForTest::SyncFile, 1);

        assert!(client.handle_msgs(Direction::ClientToHost).await);
        assert!(client.parked.is_some());
        assert_eq!(
            client.ctx.shadow_table.get_host_id(SYNC_CALLBACK),
            None,
            "the request after the commit waits behind it"
        );
        assert_eq!(
            client.next_buffered_direction(false).unwrap(),
            None,
            "fair dispatch must not offer a parked stream"
        );
        let deadline = client.parked.as_ref().map(|parked| parked.deadline);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        assert_eq!(
            client.parked.as_ref().map(|parked| parked.deadline),
            deadline,
            "a request that waits again keeps its deadline"
        );

        client
            .host_conn
            .read_buf
            .extend_from_slice(&callback_done(HOST_CALLBACK));
        assert_eq!(
            client.next_buffered_direction(false).unwrap(),
            Some(Direction::HostToClient),
            "a parked client still serves the host"
        );
        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert!(client.parked.is_some());
        drop(client);
        assert_eq!(
            sent_messages(&mut host_peer),
            [(SURFACE_HOST, wl_surface::REQ_ATTACH)]
        );
        assert!(sent_messages(&mut guest_peer).contains(&(CALLBACK, wl_callback::EVT_DONE)));
    }

    #[tokio::test]
    async fn a_parked_commit_is_forwarded_once_when_its_fence_signals() {
        use protocols::wayland::{wl_display, wl_surface};
        let (mut s, surface) = session_with_a_surface().await;
        let (buffer, callback) = (s.new_id(), s.new_id());
        // A native buffer whose one plane the guest GPU writes until
        // `signal` is written.
        let (plane, signal) = crate::test_support::pipe();
        let fence = fd_identity(signal.as_raw_fd());
        let ctx = s.ctx();
        ctx.plane_fences_for_test = Some(PlaneFenceForTest::SyncFile);
        ctx.shadow_table.insert_paired_with_version_for_test(
            buffer,
            HOST_FENCED_BUFFER,
            "wl_buffer",
            1,
        );
        assert!(ctx.register_native_buffer(HOST_FENCED_BUFFER, (1, 1), vec![plane]));

        s.guest([
            wl_surface::req::attach(surface.id, Some(buffer), 0, 0),
            wl_surface::req::commit(surface.id),
            wl_display::req::sync(WL_DISPLAY_ID, callback),
        ])
        .await;
        assert!(s.parked());
        assert_eq!(
            s.host_received(),
            [(
                surface.host,
                wl_surface::Request::Attach {
                    buffer: HOST_FENCED_BUFFER,
                    x: 0,
                    y: 0
                }
                .into()
            )]
        );
        // The registered plane, the parked duplicate and `signal`.
        assert_eq!(fds_with_identity(fence), 3);

        assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
        s.resume().await;
        assert!(!s.parked());
        assert_eq!(fds_with_identity(fence), 2);
        let forwarded = s.host_received();
        assert!(
            matches!(
                &forwarded[..],
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
        drop(s);
        drop(signal);
        assert!(!any_fd_has_identity(fence));
    }

    /// Through `Client::run`: the parked commit waits while host events
    /// reach the guest, and is forwarded once its sync_file becomes
    /// readable, before the request after it and only once.
    #[tokio::test]
    async fn run_forwards_a_parked_commit_once_its_sync_file_signals() {
        use protocols::wayland::{wl_callback, wl_surface};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, guest_peer, host_peer, signal) =
            client_committing_a_fenced_buffer(PlaneFenceForTest::SyncFile, SLOW_DEADLINE_PLANES);
        let mut guest_peer = async_peer(guest_peer);
        let mut host_peer = async_peer(host_peer);

        let peers = async {
            let attach = read_frame(&mut host_peer).await;
            assert_eq!(
                stream_headers(&attach),
                [(SURFACE_HOST, wl_surface::REQ_ATTACH)]
            );
            host_peer
                .write_all(&callback_done(HOST_CALLBACK))
                .await
                .expect("send a host event");
            let done = read_frame(&mut guest_peer).await;
            assert_eq!(stream_headers(&done), [(CALLBACK, wl_callback::EVT_DONE)]);

            assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
            let forwarded = forwarded_through_sync(&mut host_peer, Duration::from_secs(5)).await;
            assert_one_commit_then_sync(&forwarded);

            drop(guest_peer);
            let mut rest = Vec::new();
            host_peer
                .read_to_end(&mut rest)
                .await
                .expect("read until the proxy closes");
            assert_eq!(stream_headers(&rest), []);
        };
        tokio::join!(client.run(), peers);
    }

    /// Host traffic must not postpone asking about a fence that gives no
    /// descriptor to wait on: the commit is forwarded within a few poll
    /// intervals of its fence signalling, while the host sends an event
    /// every 500 µs, each of which wakes the loop (a 1 kHz mouse wakes it
    /// every millisecond). Time moves only with the traffic, so no pause
    /// between events can let a postponed poll through.
    #[tokio::test(start_paused = true)]
    async fn host_traffic_does_not_postpone_polling_a_busy_fence() {
        use protocols::wayland::wl_surface;
        use tokio::io::AsyncWriteExt;
        /// A host object the proxy does not track, so it reads and drops
        /// what the object sends.
        const UNTRACKED_HOST_OBJECT: u32 = 0x7000;
        let (client, _guest_peer, host_peer, signal) =
            client_committing_a_fenced_buffer(PlaneFenceForTest::Busy, 1);
        let mut host_peer = async_peer(host_peer);

        let peers = async {
            let (mut reader, mut writer) = host_peer.split();
            let traffic = async {
                let event = MessageBuilder::new().build_message(UNTRACKED_HOST_OBJECT, 0);
                loop {
                    writer.write_all(&event).await.expect("send host traffic");
                    tokio::time::advance(Duration::from_micros(500)).await;
                }
            };
            let forwarded = async {
                let attach = read_frame(&mut reader).await;
                assert_eq!(
                    stream_headers(&attach),
                    [(SURFACE_HOST, wl_surface::REQ_ATTACH)]
                );
                assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
                // The first polls are 2, 4 and 8 ms apart; the deadline is
                // 15 s away.
                forwarded_through_sync(&mut reader, Duration::from_millis(100)).await
            };
            tokio::select! {
                forwarded = forwarded => assert_one_commit_then_sync(&forwarded),
                () = traffic => unreachable!("the traffic never ends"),
            }
        };
        tokio::select! {
            () = client.run() => panic!("the proxy loop ended"),
            () = peers => {}
        }
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
        use protocols::wayland::{wl_display, wl_surface};
        let (mut client, mut guest_peer, mut host_peer, signal) =
            client_committing_a_fenced_buffer(stand_in, 1);
        let fence = fd_identity(signal.as_raw_fd());

        let start = Instant::now();
        let mut alive = client.handle_msgs(Direction::ClientToHost).await;
        while alive {
            assert!(
                start.elapsed() < crate::allocator::VIRTGPU_WAIT_TIMEOUT * 4,
                "the session outlives its deadline"
            );
            ParkedRequest::due(client.parked.as_ref()).await;
            alive = client.handle_msgs(Direction::ClientToHost).await;
        }
        let lasted = start.elapsed();

        drop(client);
        assert_eq!(
            display_errors(&mut guest_peer),
            [(crate::wire::WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)]
        );
        assert_eq!(
            sent_messages(&mut host_peer),
            [(SURFACE_HOST, wl_surface::REQ_ATTACH)]
        );
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
            ParkedRequest::due(Some(&parked)).await;
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
        let (mut client, _guest_peer, mut host_peer, signal) =
            client_committing_a_fenced_buffer(PlaneFenceForTest::Busy, 1);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        let start = Instant::now();
        let gpu_time = crate::allocator::SYNC_FILE_TIMEOUT * 5;
        assert!(gpu_time < crate::allocator::VIRTGPU_WAIT_TIMEOUT);
        while client.parked.is_some() {
            if start.elapsed() >= gpu_time {
                assert_eq!(nix::unistd::write(&signal, &[1]), Ok(1));
            }
            ParkedRequest::due(client.parked.as_ref()).await;
            assert!(
                client.handle_msgs(Direction::ClientToHost).await,
                "a slow fallback fence must not end the session"
            );
        }
        assert!(start.elapsed() < gpu_time + MAX_FENCE_POLL_INTERVAL * 2);
        drop(client);
        assert_one_commit_then_sync(&sent_messages(&mut host_peer));
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
        s.guest_fatal([(header, Vec::new())]).await;
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
            (too_long, Vec::new()),
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

    const XDG_SURFACE: u32 = 10;
    const XDG_SURFACE_HOST: u32 = 20;
    const XDG_TOPLEVEL: u32 = 11;
    const WL_SURFACE: u32 = 12;
    const ZAURA_SHELL_HOST: u32 = 40;

    /// A client with the ARC bounds policy on, a bound `zaura_shell`, and the
    /// v6 xdg_surface `XDG_SURFACE` on wl_surface `WL_SURFACE`, with the
    /// peers of its guest and host connections.
    fn client_with_xdg_surface() -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        use std::os::unix::net::UnixStream;

        let (guest_proxy, guest_peer) =
            UnixStream::pair().expect("guest socket pair should be created");
        let (host_proxy, host_peer) =
            UnixStream::pair().expect("host socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(guest_proxy.into()).expect("register test socket"),
            WaylandConnection::new(host_proxy.into()).expect("register test socket"),
            Context::with_placement_for_test(PlacementConfig::arc_bounds_for_test("")),
        );
        let ctx = &mut client.ctx;
        ctx.bind_aura_shell_for_test(ZAURA_SHELL_HOST);
        ctx.shadow_table.insert_paired_with_version_for_test(
            XDG_SURFACE,
            XDG_SURFACE_HOST,
            "xdg_surface",
            6,
        );
        ctx.shadow_table
            .insert_paired_for_test(WL_SURFACE, 22, "wl_surface");
        ctx.xdg_roles.add_xdg_surface(XDG_SURFACE, WL_SURFACE);
        (client, guest_peer, host_peer)
    }

    /// `xdg_surface.get_toplevel` creating `XDG_TOPLEVEL`, followed by the
    /// extra payload words `trailing`.
    fn get_toplevel_request(trailing: &[u32]) -> Vec<u8> {
        let mut builder = MessageBuilder::new();
        builder.write_u32(XDG_TOPLEVEL);
        for &word in trailing {
            builder.write_u32(word);
        }
        builder.build_message(
            XDG_SURFACE,
            protocols::xdg_shell::xdg_surface::REQ_GET_TOPLEVEL,
        )
    }

    #[tokio::test]
    async fn failed_get_toplevel_does_not_create_internal_aura_object() {
        let (mut client, _guest_peer, _host_peer) = client_with_xdg_surface();
        client.ctx.guest_destroyed(XDG_SURFACE);
        let request = get_toplevel_request(&[]);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(
            !client.handle_msgs(Direction::ClientToHost).await,
            "a request from a pending xdg_surface must terminate the session"
        );
        assert!(
            client
                .ctx
                .window_placement
                .arc()
                .map_or(0, |arc| arc.aura_toplevel_count())
                == 0,
            "failed get_toplevel requests must not leave an internal Aura child"
        );
    }

    /// The (sender, opcode) of every message written to `peer` until EOF.
    fn sent_messages(peer: &mut std::os::unix::net::UnixStream) -> Vec<(u32, u16)> {
        use std::io::Read;

        let mut stream = Vec::new();
        peer.read_to_end(&mut stream)
            .expect("the peer stream should be readable");
        crate::test_support::stream_headers(&stream)
    }

    /// A forwarded `xdg_surface.get_toplevel` records the XDG role and
    /// attaches the placement Aura toplevel, whose requests follow the
    /// forwarded request on the host stream.
    #[tokio::test]
    async fn forwarded_get_toplevel_records_the_role_after_the_request() {
        let (mut client, _guest_peer, mut host_peer) = client_with_xdg_surface();
        let request = get_toplevel_request(&[]);
        client.client_conn.read_buf.extend_from_slice(&request);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        assert_eq!(
            client.ctx.xdg_roles.wl_surface_of_toplevel(XDG_TOPLEVEL),
            Some(WL_SURFACE)
        );
        let aura_toplevel = client
            .ctx
            .window_placement
            .arc()
            .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(XDG_TOPLEVEL))
            .expect("placement Aura toplevel");
        drop(client);

        assert_eq!(
            sent_messages(&mut host_peer),
            vec![
                (
                    XDG_SURFACE_HOST,
                    protocols::xdg_shell::xdg_surface::REQ_GET_TOPLEVEL
                ),
                (
                    ZAURA_SHELL_HOST,
                    protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL
                ),
                (
                    aura_toplevel,
                    protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES
                ),
            ]
        );
    }

    /// A get_toplevel whose handler ran but whose message is then rejected
    /// (here for trailing payload bytes) records no role and creates no
    /// Aura toplevel: the deferred work is discarded with the message.
    #[tokio::test]
    async fn rejected_get_toplevel_discards_its_deferred_work() {
        let (mut client, _guest_peer, _host_peer) = client_with_xdg_surface();
        let request = get_toplevel_request(&[0]);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        assert_eq!(
            client.ctx.xdg_roles.toplevel_of_xdg_surface(XDG_SURFACE),
            None
        );
        assert_eq!(
            client
                .ctx
                .window_placement
                .arc()
                .map_or(0, |arc| arc.aura_toplevel_count()),
            0
        );
    }

    /// A get_toplevel that is forwarded but ends the session (here, as if
    /// its handler had failed) runs none of its deferred work: no role is
    /// recorded and no Aura toplevel is created.
    #[tokio::test]
    async fn a_forward_that_ends_the_session_discards_its_deferred_work() {
        let (mut client, _guest_peer, _host_peer) = client_with_xdg_surface();
        client.ctx.end_session(SessionEnd::Internal);
        let request = get_toplevel_request(&[]);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        assert_eq!(
            client.ctx.xdg_roles.toplevel_of_xdg_surface(XDG_SURFACE),
            None
        );
        assert_eq!(
            client
                .ctx
                .window_placement
                .arc()
                .map_or(0, |arc| arc.aura_toplevel_count()),
            0
        );
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

    fn dispatch_raw_request_result(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        message: Vec<u8>,
    ) -> Option<crate::wire::Outgoing> {
        let header = msg_header(&message);
        let mut wire = WireMessage::new(
            header.sender_id,
            header.opcode,
            &message[MessageHeader::LEN..],
        );
        let result = Client::dispatch_request(handler, ctx, interface, &mut wire)
            .expect("raw guest request must dispatch");
        assert!(
            wire.is_payload_consumed(),
            "proxy dispatch must consume the complete raw request payload"
        );
        result
    }

    #[test]
    fn raw_gtk_surface_request_is_consumed_and_translated_to_aura() {
        const GTK_SHELL: u32 = 40;
        const GTK_SURFACE: u32 = 41;
        const GUEST_SURFACE: u32 = 42;
        const HOST_SURFACE: u32 = 142;
        const AURA_SHELL: u32 = 240;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.create_local(GTK_SHELL, "gtk_shell1", 1);
        ctx.gtk_shells
            .insert(GTK_SHELL, crate::state::GtkShellState::default());
        register_raw_surface(&mut ctx, GUEST_SURFACE, HOST_SURFACE);
        ctx.bind_aura_shell_for_test(AURA_SHELL);
        ctx.shadow_table
            .insert_host_only_for_test(AURA_SHELL, "zaura_shell", 38);
        let mut request = MessageBuilder::new();
        request.write_u32(GTK_SURFACE);
        request.write_u32(GUEST_SURFACE);
        let request =
            request.build_message(GTK_SHELL, protocols::gtk::gtk_shell1::REQ_GET_GTK_SURFACE);

        let mut handler = SommelierHandler::new();
        assert!(
            dispatch_raw_request_result(&mut handler, &mut ctx, "gtk_shell1", request).is_none()
        );
        assert!(ctx.gtk_surfaces.contains_key(&GTK_SURFACE));
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(
            crate::test_support::msg_sender(&ctx.client_to_host_queue[0].0),
            AURA_SHELL
        );

        let mut bell = MessageBuilder::new();
        bell.write_u32(GTK_SURFACE);
        let bell = bell.build_message(GTK_SHELL, protocols::gtk::gtk_shell1::REQ_SYSTEM_BELL);
        assert!(
            dispatch_raw_request_result(&mut handler, &mut ctx, "gtk_shell1", bell).is_none(),
            "a local gtk_surface object must be accepted by a local no-op request"
        );
    }

    fn register_raw_surface(ctx: &mut Context, guest_surface: u32, host_surface: u32) {
        ctx.shadow_table.insert_paired_with_version_for_test(
            guest_surface,
            host_surface,
            "wl_surface",
            1,
        );
    }

    #[test]
    fn synthetic_manager_destroy_emits_one_guest_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_id = 20;
        ctx.shadow_table
            .create_local(guest_id, "zwp_text_input_manager_v3", 1);
        let mut handler = SommelierHandler::new();

        let mut destroy = WireMessage::new(
            guest_id,
            protocols::text_input_unstable_v3::zwp_text_input_manager_v3::REQ_DESTROY,
            &[],
        );
        assert_eq!(
            raw_dispatch(
                protocols::text_input_unstable_v3::zwp_text_input_manager_v3::dispatch_request(
                    &mut destroy,
                    &mut handler,
                    &mut ctx,
                )
            ),
            Ok(None)
        );
        assert_eq!(ctx.shadow_table.get_interface(guest_id), None);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap()),
            guest_id
        );

        let mut duplicate_destroy = WireMessage::new(
            guest_id,
            protocols::text_input_unstable_v3::zwp_text_input_manager_v3::REQ_DESTROY,
            &[],
        );
        assert_eq!(
            raw_dispatch(
                protocols::text_input_unstable_v3::zwp_text_input_manager_v3::dispatch_request(
                    &mut duplicate_destroy,
                    &mut handler,
                    &mut ctx,
                )
            ),
            Err(ProtocolError::InvalidSender(guest_id))
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "a retired synthetic ID must not emit duplicate delete_id events"
        );
    }

    #[test]
    fn synthetic_shm_pool_destroy_emits_one_guest_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_id = 21;
        ctx.shadow_table.create_local(guest_id, "wl_shm_pool", 1);
        let mut handler = SommelierHandler::new();

        let mut destroy =
            WireMessage::new(guest_id, protocols::wayland::wl_shm_pool::REQ_DESTROY, &[]);
        assert_eq!(
            raw_dispatch(protocols::wayland::wl_shm_pool::dispatch_request(
                &mut destroy,
                &mut handler,
                &mut ctx,
            )),
            Ok(None)
        );
        assert_eq!(ctx.shadow_table.get_interface(guest_id), None);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap()),
            guest_id
        );
    }

    /// A batch that ends early sends nothing. The descriptors of the
    /// messages it translated close with it; the connection keeps the ones
    /// no message has taken yet until it closes.
    #[tokio::test]
    async fn early_output_cleanup_preserves_incoming_fd_ownership() {
        use crate::protocols::wayland::wl_keyboard;

        let (mut client, mut guest_peer, _host_peer) = pending_keyboard_client();
        // Memfds have identities of their own; pipe ends share one.
        let forwarded = shm_pool_file(&[]);
        let pending = shm_pool_file(&[]);
        let forwarded_identity = fd_identity(forwarded.as_raw_fd());
        let pending_identity = fd_identity(pending.as_raw_fd());

        // A keymap the proxy forwards, then an event with trailing bytes,
        // which ends the session. `pending` belongs to a later message.
        let mut keymap = MessageBuilder::new();
        keymap.write_u32(wl_keyboard::KEYMAP_FORMAT_NO_KEYMAP);
        keymap.write_u32(0);
        let keymap = keymap.build_message(LIVE_HOST_KEYBOARD, wl_keyboard::EVT_KEYMAP);
        let mut repeat_info = MessageBuilder::new();
        for word in [25, 400, 0] {
            repeat_info.write_i32(word);
        }
        let repeat_info =
            repeat_info.build_message(LIVE_HOST_KEYBOARD, wl_keyboard::EVT_REPEAT_INFO);
        client.host_conn.read_buf.extend_from_slice(&keymap);
        client.host_conn.read_buf.extend_from_slice(&repeat_info);
        client.host_conn.read_fds.push_back(forwarded.into());
        client.host_conn.read_fds.push_back(pending.into());

        assert!(
            !client.handle_msgs(Direction::HostToClient).await,
            "trailing bytes in a host event end the session"
        );
        assert!(
            !any_fd_has_identity(forwarded_identity),
            "the proxy closes a forwarded descriptor once it is sent"
        );
        assert!(
            any_fd_has_identity(pending_identity),
            "the connection still owns the descriptor no message took"
        );
        drop(client);
        assert!(
            !any_fd_has_identity(pending_identity),
            "closing the connection must close every pending descriptor"
        );
        // The fixture's peer is non-blocking; block until the connection's
        // close reaches it, which a process forked meanwhile may delay.
        guest_peer
            .set_nonblocking(false)
            .expect("guest peer should become blocking");
        let mut events = Vec::new();
        std::io::Read::read_to_end(&mut guest_peer, &mut events).expect("read the guest's events");
        let headers: Vec<_> = crate::test_support::frames(&events)
            .map(|(header, _)| (header.sender_id, header.opcode))
            .collect();
        assert_eq!(
            headers,
            [
                (LIVE_GUEST_KEYBOARD, wl_keyboard::EVT_KEYMAP),
                (
                    crate::wire::WL_DISPLAY_ID,
                    crate::protocols::wayland::wl_display::EVT_ERROR
                ),
            ],
            "the valid event reaches the guest ahead of the session's error"
        );
    }

    #[test]
    fn destructor_mapping_survives_until_host_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(1, 1, "wl_display", 1);
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "wl_region", 1);
        let mut handler = SommelierHandler::new();

        let mut destroy = WireMessage::new(10, protocols::wayland::wl_region::REQ_DESTROY, &[]);
        let forwarded =
            protocols::wayland::dispatch_request("wl_region", &mut destroy, &mut handler, &mut ctx)
                .expect("destructor request should dispatch");
        assert!(forwarded.is_some());
        assert_eq!(
            ctx.shadow_table.get_guest_id(20),
            Some(10),
            "host ID must remain mapped until host delete_id"
        );
        assert!(
            ctx.shadow_table.is_pending_destroy_guest(10),
            "destroyed guest object must be marked pending"
        );

        let payload = 20u32.to_ne_bytes();
        let mut delete_id =
            WireMessage::new(1, protocols::wayland::wl_display::EVT_DELETE_ID, &payload);
        assert_eq!(
            raw_dispatch(protocols::wayland::dispatch_event(
                "wl_display",
                &mut delete_id,
                &mut handler,
                &mut ctx
            )),
            Ok(None)
        );
        let (message, _) = ctx
            .host_to_client_queue
            .pop()
            .expect("host delete_id should be queued for the guest");
        assert_eq!(u32::from_ne_bytes(message[8..12].try_into().unwrap()), 10);
        assert_eq!(ctx.shadow_table.get_guest_id(20), None);
        assert!(!ctx.shadow_table.is_pending_destroy_guest(10));
    }

    #[test]
    fn wl_fixes_destroy_registry_forwards_host_id_until_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(1, 1, "wl_display", 1);
        ctx.shadow_table
            .insert_paired_with_version_for_test(5, 15, "wl_fixes", 1);
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "wl_registry", 1);
        let mut handler = SommelierHandler::new();

        let payload = 10u32.to_ne_bytes();
        let mut destroy_registry = WireMessage::new(
            5,
            protocols::wayland::wl_fixes::REQ_DESTROY_REGISTRY,
            &payload,
        );
        let (forwarded, fds) = protocols::wayland::dispatch_request(
            "wl_fixes",
            &mut destroy_registry,
            &mut handler,
            &mut ctx,
        )
        .expect("destroy_registry request should dispatch")
        .expect("destroy_registry request should be forwarded");
        assert!(fds.is_empty());
        assert_eq!(crate::test_support::msg_sender(&forwarded), 15);
        assert_eq!(
            crate::test_support::msg_opcode(&forwarded),
            protocols::wayland::wl_fixes::REQ_DESTROY_REGISTRY
        );
        assert_eq!(
            u32::from_ne_bytes(forwarded[8..12].try_into().unwrap()),
            20,
            "the registry object argument must use its host ID"
        );
        assert_eq!(ctx.shadow_table.get_guest_id(20), Some(10));
        assert!(ctx.shadow_table.is_pending_destroy_guest(10));

        let payload = 20u32.to_ne_bytes();
        let mut delete_id =
            WireMessage::new(1, protocols::wayland::wl_display::EVT_DELETE_ID, &payload);
        assert_eq!(
            raw_dispatch(protocols::wayland::dispatch_event(
                "wl_display",
                &mut delete_id,
                &mut handler,
                &mut ctx
            )),
            Ok(None)
        );
        let (message, _) = ctx
            .host_to_client_queue
            .pop()
            .expect("host delete_id should be queued for the guest");
        assert_eq!(u32::from_ne_bytes(message[8..12].try_into().unwrap()), 10);
        assert_eq!(ctx.shadow_table.get_guest_id(20), None);
        assert!(!ctx.shadow_table.is_pending_destroy_guest(10));
    }

    #[test]
    fn pending_destructor_host_events_are_suppressed_except_deferred_buffer_release() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "wl_region", 1);
        ctx.guest_destroyed(10);
        assert!(pending_host_event_is_stale(&ctx, 20, 0));

        ctx.shadow_table
            .insert_paired_with_version_for_test(11, 21, "wl_buffer", 1);
        assert!(ctx.register_local_buffer(11, 21, test_buffer_state()));
        assert!(ctx.mark_buffer_guest_destroyed(11));
        ctx.guest_destroyed_in_use(11);
        assert!(!pending_host_event_is_stale(
            &ctx,
            21,
            protocols::wayland::wl_buffer::EVT_RELEASE,
        ));

        ctx.shadow_table
            .insert_paired_with_version_for_test(12, 22, "wl_buffer", 1);
        assert!(ctx.register_native_buffer(22, (1, 1), Vec::new()));
        // Match the production native-buffer destroy path: the host
        // object still dispatches wl_buffer.release.
        assert!(ctx.mark_buffer_guest_destroyed(12));
        ctx.guest_destroyed_in_use(12);
        assert!(!pending_host_event_is_stale(
            &ctx,
            22,
            protocols::wayland::wl_buffer::EVT_RELEASE,
        ));
    }

    #[test]
    fn released_output_events_never_reach_the_output_handlers() {
        use crate::protocols::wayland::wl_output::{
            WlOutputHandler, EVT_DONE, EVT_GEOMETRY, EVT_MODE, EVT_SCALE,
        };
        // The wl_output handlers rely on this filter instead of checking for
        // a pending destructor themselves.
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "wl_output", 3);
        ctx.last_sender_id = 10;
        WlOutputHandler::on_release(&mut crate::handler::output::OutputHandler, &mut ctx);
        for opcode in [EVT_GEOMETRY, EVT_MODE, EVT_DONE, EVT_SCALE] {
            assert!(
                pending_host_event_is_stale(&ctx, 20, opcode),
                "opcode {opcode}"
            );
        }
    }

    fn test_buffer_state() -> crate::state::BufferState {
        use crate::state::{BufferState, StalePixels};
        use std::sync::Arc;
        BufferState {
            pool_file: Arc::new(crate::test_support::shm_pool_file(&[])),
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: 0,
            dmabuf_fd: None,
            dest_stride: 4,
            dmabuf_plane1_offset: 0,
            dmabuf_plane1_stride: 0,
            dmabuf_sync: false,
            dest_ptr: std::ptr::null_mut(),
            dest_size: 0,
            stale: StalePixels::all(),
        }
    }

    /// The proxy dispatches a host event by its sender's interface, so an
    /// event from a sender it does not track never reaches a handler.
    #[tokio::test]
    async fn untracked_host_event_is_dropped_before_handler_state_mutation() {
        let (mut client, _guest_peer, _host_peer) = client_with_socket_peers();
        let mut builder = MessageBuilder::new();
        builder.write_u32(7);
        builder.write_string("wl_seat");
        builder.write_u32(1);
        let global = builder.build_message(77, protocols::wayland::wl_registry::EVT_GLOBAL);

        host_events(&mut client, &[global]).await;
        assert!(
            client.ctx.host_globals.is_empty(),
            "an untracked host event must not reach a stateful handler"
        );
    }

    #[test]
    fn created_surface_keeps_separate_guest_and_host_versions() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let host_compositor = ctx.shadow_table.create_paired(10, "wl_compositor", 4, 3);
        let mut handler = SommelierHandler::new();
        let payload = 30u32.to_ne_bytes();
        let mut msg = WireMessage::new(
            10,
            protocols::wayland::wl_compositor::REQ_CREATE_SURFACE,
            &payload,
        );
        let (forwarded, fds) =
            protocols::wayland::dispatch_request("wl_compositor", &mut msg, &mut handler, &mut ctx)
                .expect("create_surface dispatch")
                .expect("create_surface forwarded");
        assert!(fds.is_empty());
        assert_eq!(crate::test_support::msg_sender(&forwarded), host_compositor);
        let host_surface = ctx.shadow_table.get_host_id(30).expect("host surface");
        assert_eq!(
            u32::from_ne_bytes(forwarded[8..12].try_into().unwrap()),
            host_surface
        );
        assert_eq!(ctx.shadow_table.guest_object_version(30), Some(4));
        assert_eq!(ctx.shadow_table.host_object_version(host_surface), Some(3));
    }

    #[test]
    fn request_since_guard_rejects_a_newer_opcode_for_an_old_guest_object() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "xdg_positioner", 1);

        let payload = [];
        let mut msg = WireMessage::new(
            10,
            protocols::xdg_shell::xdg_positioner::REQ_SET_REACTIVE,
            &payload,
        );
        let mut handler = SommelierHandler::new();
        let result = protocols::xdg_shell::dispatch_request(
            "xdg_positioner",
            &mut msg,
            &mut handler,
            &mut ctx,
        );

        assert_eq!(
            raw_dispatch(result),
            Err(ProtocolError::UnsupportedVersion {
                object_id: 10,
                required: 3,
                actual: 1,
            })
        );
    }

    #[test]
    fn request_rejects_object_argument_that_is_pending_destroy() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let sender = 10;
        let host_sender = 20;
        let surface = 30;
        let host_surface = 40;
        let parent = 31;
        let host_parent = 41;
        let new_subsurface: u32 = 50;

        ctx.shadow_table.insert_paired_with_version_for_test(
            sender,
            host_sender,
            "wl_subcompositor",
            1,
        );
        ctx.shadow_table.insert_paired_with_version_for_test(
            surface,
            host_surface,
            "wl_surface",
            1,
        );
        ctx.shadow_table
            .insert_paired_with_version_for_test(parent, host_parent, "wl_surface", 1);
        ctx.guest_destroyed(surface);

        let mut payload = Vec::new();
        payload.extend_from_slice(&new_subsurface.to_ne_bytes());
        payload.extend_from_slice(&surface.to_ne_bytes());
        payload.extend_from_slice(&parent.to_ne_bytes());
        let mut msg = WireMessage::new(
            sender,
            protocols::wayland::wl_subcompositor::REQ_GET_SUBSURFACE,
            &payload,
        );
        let mut handler = SommelierHandler::new();

        assert_eq!(
            raw_dispatch(protocols::wayland::wl_subcompositor::dispatch_request(
                &mut msg,
                &mut handler,
                &mut ctx,
            )),
            Err(ProtocolError::InvalidObjectId(surface)),
            "requests must not retain a pending-destroy object through another argument"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(new_subsurface),
            None,
            "rejected requests must not allocate their new object"
        );
    }

    #[test]
    fn event_since_guard_rejects_a_newer_event_for_an_old_host_object() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "xdg_toplevel", 3);

        let payload = [0u8; 8];
        let mut msg = WireMessage::new(
            20,
            protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE_BOUNDS,
            &payload,
        );
        let mut handler = SommelierHandler::new();
        let result =
            protocols::xdg_shell::dispatch_event("xdg_toplevel", &mut msg, &mut handler, &mut ctx);

        assert_eq!(
            raw_dispatch(result),
            Err(ProtocolError::UnsupportedVersion {
                object_id: 20,
                required: 4,
                actual: 3,
            })
        );
    }

    #[test]
    fn generated_forwarding_rejects_messages_that_exceed_wire_length() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "wl_data_source", 1);

        let mut builder = MessageBuilder::new();
        builder.write_string(&"x".repeat(4_084));
        let payload = builder.into_payload();
        let mut msg = WireMessage::new(10, protocols::wayland::wl_data_source::REQ_OFFER, &payload);
        let mut handler = SommelierHandler::new();

        assert_eq!(
            raw_dispatch(protocols::wayland::wl_data_source::dispatch_request(
                &mut msg,
                &mut handler,
                &mut ctx,
            )),
            Err(ProtocolError::MessageTooLarge(4_100)),
            "generated forwarding must not truncate the Wayland length field"
        );
    }
}

// Wayland Protocol
protocols::wayland::impl_sommelier_delegates!(SommelierHandler, {
    wl_display: display,
    wl_registry: registry,
    wl_callback: callback,
    wl_compositor: compositor,
    wl_surface: compositor,
    wl_subcompositor: compositor,
    wl_subsurface: compositor,
    wl_region: compositor,
    wl_shm: shm,
    wl_shm_pool: shm,
    wl_buffer: shm,
    wl_data_device_manager: data_device,
    wl_data_device: data_device,
    wl_data_source: data_device,
    wl_data_offer: data_device,
    wl_keyboard: keyboard,
    wl_seat: seat,
    wl_output: output,
    wl_fixes: registry
});
impl protocols::wayland::ProtocolHandler for SommelierHandler {}

// XDG Shell Protocol
protocols::xdg_shell::impl_sommelier_delegates!(SommelierHandler, {
    xdg_wm_base: compositor,
    xdg_surface: compositor,
    xdg_toplevel: compositor
});
impl protocols::xdg_shell::ProtocolHandler for SommelierHandler {}

// Linux DMABuf Protocol
protocols::linux_dmabuf_v1::impl_sommelier_delegates!(SommelierHandler, {
    zwp_linux_dmabuf_v1: linux_dmabuf,
    zwp_linux_buffer_params_v1: linux_dmabuf,
    zwp_linux_dmabuf_feedback_v1: linux_dmabuf
});
impl protocols::linux_dmabuf_v1::ProtocolHandler for SommelierHandler {}

// Text Input unstable v1 Protocol
protocols::text_input_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {
    zwp_text_input_manager_v1: text_input_manager_v1,
    zwp_text_input_v1: text_input_v1
});
impl protocols::text_input_unstable_v1::ProtocolHandler for SommelierHandler {}

// Text Input Extension unstable v1 Protocol
protocols::text_input_extension_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {
    zcr_text_input_extension_v1: text_input_extension_v1,
    zcr_extended_text_input_v1: extended_text_input_v1
});
impl protocols::text_input_extension_unstable_v1::ProtocolHandler for SommelierHandler {}

// Text Input unstable v3 Protocol
protocols::text_input_unstable_v3::impl_sommelier_delegates!(SommelierHandler, {
    zwp_text_input_manager_v3: text_input_manager_v3,
    zwp_text_input_v3: text_input_v3
});
impl protocols::text_input_unstable_v3::ProtocolHandler for SommelierHandler {}

// Viewporter Protocol
protocols::viewporter::impl_sommelier_delegates!(SommelierHandler, {
    wp_viewporter: compositor,
    wp_viewport: compositor
});
impl protocols::viewporter::ProtocolHandler for SommelierHandler {}

// XDG Decoration Protocol
protocols::xdg_decoration_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {});
impl protocols::xdg_decoration_unstable_v1::ProtocolHandler for SommelierHandler {}

// Fractional Scale Protocol
protocols::fractional_scale_v1::impl_sommelier_delegates!(SommelierHandler, {});
impl protocols::fractional_scale_v1::ProtocolHandler for SommelierHandler {}

// XDG Output Protocol
protocols::xdg_output_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {
    zxdg_output_manager_v1: output
});
impl protocols::xdg_output_unstable_v1::ProtocolHandler for SommelierHandler {}

// Keyboard Extension Protocol (ChromeOS-specific)
protocols::keyboard_extension_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {
    zcr_keyboard_extension_v1: keyboard,
    zcr_extended_keyboard_v1: keyboard
});
impl protocols::keyboard_extension_unstable_v1::ProtocolHandler for SommelierHandler {}

// GTK shell protocol, translated to ChromeOS Aura surface metadata.
protocols::gtk::impl_sommelier_delegates!(SommelierHandler, {
    gtk_shell1: gtk_shell,
    gtk_surface1: gtk_shell
});
impl protocols::gtk::ProtocolHandler for SommelierHandler {}

// Host-only remote-shell objects of the remote-shell-v2 placement backend.
// Guests never see the global, so only host events reach this handler.
protocols::remote_shell_unstable_v2::impl_sommelier_delegates!(SommelierHandler, {
    zcr_remote_shell_v2: remote_shell,
    zcr_remote_surface_v2: remote_shell
});
impl protocols::remote_shell_unstable_v2::ProtocolHandler for SommelierHandler {}

/// What every client connection needs from the proxy's configuration.
struct ClientConfig {
    local_compositor: Option<String>,
    virtio_wayland: Option<String>,
    gpu_accel: bool,
    xdg_decoration: bool,
    vm_identifier: String,
    placement: Arc<PlacementConfig>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    display: &str,
    local_compositor: Option<String>,
    gpu_accel: bool,
    xdg_decoration: bool,
    virtio_wayland: Option<String>,
    vm_identifier: String,
    placement: Arc<PlacementConfig>,
    reload_signal: Option<tokio::signal::unix::Signal>,
) {
    if let Some(path) = &virtio_wayland {
        if let Err(e) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            log::error!("Failed to open virtio-wayland device {}: {}", path, e);
            std::process::exit(1);
        }
    } else if let Some(path) = &local_compositor {
        if let Err(e) = std::os::unix::net::UnixStream::connect(path) {
            log::error!(
                "Failed to connect to local compositor socket {}: {}",
                path,
                e
            );
            std::process::exit(1);
        }
    }

    // Held until the process exits, like the socket itself.
    let _socket_lock =
        claim_display_socket(std::path::Path::new(display)).unwrap_or_else(|error| {
            log::error!("Cannot claim the Wayland socket {display}: {error}");
            std::process::exit(1);
        });
    let listener = UnixListener::bind(display).expect("Failed to bind socket");
    log::info!("Listening on {}", display);

    let config = ClientConfig {
        local_compositor,
        virtio_wayland,
        gpu_accel,
        xdg_decoration,
        vm_identifier,
        placement,
    };
    // Each client's Context holds xkb state, which is !Send. Keep each
    // client task on this executor's thread instead of using `tokio::spawn`
    // (whose Send bound would require an unsound manual Send implementation).
    // `LocalSet::run_until` also makes the ownership requirement visible in
    // this function's future type: callers cannot accidentally move the
    // proxy's client tasks to another executor thread.
    tokio::task::LocalSet::new()
        .run_until(serve(listener, config, reload_signal))
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

/// Accept guest clients for the life of the process, and reload the
/// shortcut configuration on each of `reload_signal`'s requests.
///
/// A failure concerning one client is logged and ends only that client.
/// Must run inside a `LocalSet`, like the clients it spawns.
async fn serve(
    listener: UnixListener,
    config: ClientConfig,
    reload_signal: Option<impl ReloadRequests>,
) {
    if let Some(signal) = reload_signal {
        let reloads =
            tokio::task::spawn_local(reload_on_signal(signal, Arc::clone(&config.placement)));
        // SIGHUP stays handled after the task ends (its signal stream
        // closed, or a reload panicked), so say that reloads have stopped.
        tokio::task::spawn_local(async move {
            match reloads.await {
                Ok(()) => log::error!("SIGHUP reloads have stopped: the signal stream ended"),
                Err(error) => log::error!("SIGHUP reloads have stopped: {error}"),
            }
        });
    }
    // Log a run of failed accepts once, not every retry.
    let mut accept_failing = false;
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                if std::mem::take(&mut accept_failing) {
                    log::info!("Accepting clients again");
                }
                log::info!("New client connected");
                if let Err(error) = start_client(stream, &config).await {
                    log::error!("Closing the new client: {error}");
                }
            }
            Err(e) => {
                if !std::mem::replace(&mut accept_failing, true) {
                    log::error!(
                        "Accept error: {e}; retrying every {ACCEPT_RETRY_DELAY:?} until one succeeds"
                    );
                }
                tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
            }
        }
    }
}

/// Pair an accepted guest connection with a new host connection and serve
/// the pair on its own task.
async fn start_client(stream: UnixStream, config: &ClientConfig) -> io::Result<()> {
    let client_conn = WaylandConnection::new(stream.into_std()?.into())?;
    let (host_conn, virtwayland_channel) = connect_host(config).await?;
    let mut client = Client::new(
        client_conn,
        host_conn,
        Context::new(
            config.gpu_accel,
            config.xdg_decoration,
            config.vm_identifier.clone(),
            Arc::clone(&config.placement),
        ),
    );
    client.ctx.virtwayland_channel = virtwayland_channel;
    tokio::task::spawn_local(async move {
        client.run().await;
        log::info!("Client disconnected");
    });
    Ok(())
}

/// Open this client's connection to the host compositor: a VirtWL channel
/// when configured, else (or when that fails) the local compositor socket.
async fn connect_host(
    config: &ClientConfig,
) -> io::Result<(WaylandConnection, Option<Arc<VirtWaylandChannel>>)> {
    if let Some(path) = &config.virtio_wayland {
        match VirtWaylandChannel::new(path) {
            Ok(channel) => {
                let channel = Arc::new(channel);
                let conn = WaylandConnection::new_virtwayland(Arc::clone(&channel));
                return Ok((conn, Some(channel)));
            }
            Err(e) => {
                log::error!("Failed to open virtio-wayland channel at {}: {}", path, e);
            }
        }
    }
    let Some(path) = &config.local_compositor else {
        return Err(io::Error::other(
            "no host connection: virtio-wayland is unavailable and no local compositor is set",
        ));
    };
    let stream = UnixStream::connect(path).await?;
    Ok((WaylandConnection::new(stream.into_std()?.into())?, None))
}

/// Install the `SIGHUP` handler that reloads the shortcut configuration file.
///
/// Once installed, `SIGHUP` no longer terminates the process. Tokio delivers
/// the signal through its own self-pipe; [`reload_on_signal`] runs the
/// reload. `main` installs it before anything else can take long, whatever
/// the configuration.
///
/// # Returns
///
/// `None` when the handler cannot be installed. That is logged and is not
/// fatal: the proxy runs without reloads.
pub(crate) fn install_reload_signal() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .inspect_err(|error| {
            log::error!(
                "Failed to install the SIGHUP shortcut reload handler; reloads are disabled: {}",
                error
            );
        })
        .ok()
}

/// Where requests to reload the shortcut configuration come from: the
/// process's `SIGHUP`, or, in tests, a channel, since a signal reaches
/// every test in the process.
trait ReloadRequests: 'static {
    /// Wait for the next request; `None` once no more can come.
    async fn recv(&mut self) -> Option<()>;
}

impl ReloadRequests for tokio::signal::unix::Signal {
    async fn recv(&mut self) -> Option<()> {
        tokio::signal::unix::Signal::recv(self).await
    }
}

#[cfg(test)]
impl ReloadRequests for tokio::sync::mpsc::Receiver<()> {
    async fn recv(&mut self) -> Option<()> {
        tokio::sync::mpsc::Receiver::recv(self).await
    }
}

/// Reload the shortcut configuration on each of `signal`'s requests.
///
/// A task of its own, so a reload never waits behind the accept loop (its
/// retry pause, or a client's host connection). It runs on the proxy's one
/// thread between other tasks' steps, so it never races a key event.
/// `PlacementConfig::reload_and_log` owns what a reload means, including
/// the logged no-op when there is no file.
async fn reload_on_signal(mut signal: impl ReloadRequests, placement: Arc<PlacementConfig>) {
    while signal.recv().await.is_some() {
        placement.reload_and_log();
    }
}
