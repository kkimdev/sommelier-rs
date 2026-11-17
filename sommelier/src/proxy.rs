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

use crate::connection::{OutgoingMessages, WaylandConnection};
use crate::placement_config::PlacementConfig;
use crate::protocols;
use crate::state::{Context, PostDispatch, ShadowTable};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{Framing, MessageHeader, ProtocolError, WireMessage};
use std::io;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

#[cfg(test)]
mod fuzz;

type DispatchResult = Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError>;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    ClientToHost,
    HostToClient,
}

fn translated_sender_id(
    shadow_table: &ShadowTable,
    direction: Direction,
    sender_id: u32,
) -> Option<u32> {
    match direction {
        Direction::ClientToHost => shadow_table.get_host_id(sender_id),
        Direction::HostToClient => shadow_table.get_guest_id(sender_id),
    }
}

fn close_pending_output_fds(out: &mut OutgoingMessages, still_owned_fds: &[RawFd]) {
    // FDs decoded from an incoming message remain owned by the connection
    // until the read buffer is drained (or the connection is dropped). A
    // handler can also queue newly-created descriptors that are not present in
    // that list. On an early protocol-error return, close only the latter;
    // closing a descriptor in both collections would let a later fd reuse
    // turn the connection drop into a double-close.
    let protected: std::collections::HashSet<_> = still_owned_fds.iter().copied().collect();
    let mut closed = std::collections::HashSet::new();
    for fd in out.take_fds() {
        if fd >= 0 && !protected.contains(&fd) && closed.insert(fd) {
            let _ = nix::unistd::close(fd);
        }
    }
}

fn pending_host_event_is_stale(
    ctx: &Context,
    sender_id: u32,
    opcode: u16,
    guest_id: Option<u32>,
) -> bool {
    let host_interface = ctx.shadow_table.get_host_interface(sender_id);
    let guest_interface = guest_id.and_then(|gid| ctx.shadow_table.get_interface(gid));
    let allows_deferred_buffer_release = host_interface
        .or(guest_interface)
        .is_some_and(|name| name == "wl_buffer")
        && opcode == protocols::wayland::wl_buffer::EVT_RELEASE
        && ctx.host_buffer_is_guest_destroyed(sender_id);
    // An asynchronous linux-dmabuf create can legally be followed by
    // params.destroy before the compositor emits `created` or `failed`.
    // The host still owns a newly-created wl_buffer in that case, so let
    // the event reach the handler, which turns it into a host-only buffer
    // and destroys it. Dropping `created` here would leak the host buffer
    // because generated new_id mapping only runs when the event is
    // forwarded.
    let allows_orphan_dmabuf_result = host_interface
        .or(guest_interface)
        .is_some_and(|name| name == "zwp_linux_buffer_params_v1")
        && (opcode == protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_CREATED
            || opcode == protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_FAILED);
    ctx.shadow_table.is_pending_destroy_host(sender_id)
        && !allows_deferred_buffer_release
        && !allows_orphan_dmabuf_result
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

struct Client {
    client_conn: WaylandConnection,
    host_conn: WaylandConnection,
    ctx: Context,
    handler: SommelierHandler,
}

impl Client {
    fn new(client_conn: WaylandConnection, host_conn: WaylandConnection, ctx: Context) -> Self {
        Self {
            client_conn,
            host_conn,
            ctx,
            handler: SommelierHandler::new(),
        }
    }

    async fn run(mut self) {
        self.ctx.shadow_table.map_id(1, 1);
        self.ctx
            .shadow_table
            .track_interface(1, "wl_display".to_string());

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
                res = self.client_conn.recv() => {
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
            }
        }

        // Clipboard receive pumps are tied to this client connection. Stop
        // them before dropping the connection so an unfinished VirtWL transfer
        // cannot keep its duplicated descriptors or task alive after either
        // endpoint disconnects.
        self.ctx.stop_clipboard_pumps().await;
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
        let (preferred_direction, fallback_direction) = if prefer_host {
            (Direction::HostToClient, Direction::ClientToHost)
        } else {
            (Direction::ClientToHost, Direction::HostToClient)
        };

        if self.connection(preferred_direction).has_complete_message() {
            return Ok(Some(preferred_direction));
        }
        if !self.connection(fallback_direction).has_complete_message() {
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
        let Some(family) = protocols::classify_interface(interface) else {
            return Ok(None);
        };

        // Aura objects are bound internally and have no guest request path.
        if family == protocols::ProtocolFamily::AuraShell {
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
            if interface == "zaura_toplevel" {
                return protocols::aura_shell::zaura_toplevel::dispatch_event(
                    msg,
                    &mut handler.compositor,
                    ctx,
                );
            }
            if interface == "zaura_output" {
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
        self.ctx.reap_clipboard_pumps();
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
        let mut offset: usize = 0;
        let mut fd_offset: usize = 0;
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
                    if matches!(direction, Direction::ClientToHost) {
                        // libwayland-server reports a length under 8 or
                        // over 4096 the same way (the alignment check is
                        // the proxy's own); the stream cannot be framed
                        // past it, so the session ends.
                        crate::handler::display::queue_request_error(
                            &mut self.ctx,
                            &ProtocolError::InvalidMessageLength(header.len),
                        );
                        break;
                    }
                    close_pending_output_fds(&mut out, &conn.read_fds);
                    return false;
                }
                Framing::Complete(header) => header,
            };
            let message_end = offset + len;

            if conn.ambiguous_untracked_fd {
                log::error!(
                    "Ambiguous SCM_RIGHTS ordering after a complete untracked message; \
                     terminating connection before assigning descriptors to another message"
                );
                close_pending_output_fds(&mut out, &conn.read_fds);
                return false;
            }

            let packet = &conn.read_buf[offset..message_end];

            let guest_id = match direction {
                Direction::ClientToHost => Some(sender_id),
                Direction::HostToClient => self.ctx.shadow_table.get_guest_id(sender_id),
            };
            // Destructors retain their numeric mapping until host
            // wl_display.delete_id, but the translated sender is still
            // captured before dispatch so custom handlers may retire state
            // without changing the forwarded request's sender.
            let target_sender_id =
                translated_sender_id(&self.ctx.shadow_table, direction, sender_id);

            debug_assert!(
                !self.ctx.has_post_dispatch(),
                "deferred work belongs to the message that deferred it"
            );
            let suppress_host_event = matches!(direction, Direction::HostToClient)
                && pending_host_event_is_stale(&self.ctx, sender_id, opcode, guest_id);
            let interface = match direction {
                Direction::ClientToHost => self.ctx.shadow_table.get_interface(sender_id).cloned(),
                Direction::HostToClient => {
                    let host_interface = self.ctx.shadow_table.get_host_interface(sender_id);
                    let guest_interface =
                        guest_id.and_then(|gid| self.ctx.shadow_table.get_interface(gid));
                    host_interface.or(guest_interface).cloned()
                }
            };

            let mut consumed_fds = 0;
            let result = if let Some(interface) = interface {
                let mut msg = WireMessage::new(
                    sender_id,
                    opcode,
                    &packet[MessageHeader::LEN..],
                    &conn.read_fds[fd_offset..],
                );

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
                consumed_fds = msg.fd_offset;
                if !msg.is_payload_consumed() {
                    log::error!(
                        "Protocol message left {} trailing payload byte(s): interface={}, opcode={}",
                        msg.payload.len() - msg.offset,
                        interface,
                        opcode
                    );
                    Err(ProtocolError::TrailingData)
                } else {
                    res
                }
            } else if matches!(direction, Direction::ClientToHost) {
                // A guest request is only valid when its sender is a live
                // object owned by this connection. Unlike a stale host event
                // (which may have raced object teardown), silently dropping
                // an unknown client request makes the client believe the
                // request was accepted and can desynchronize protocol state.
                Err(ProtocolError::InvalidSender(sender_id))
            } else {
                if guest_id.is_some() {
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
                // and let WaylandConnection::Drop close it if the peer goes
                // away before that message arrives.
                if fd_offset < conn.read_fds.len() {
                    conn.ambiguous_untracked_fd = true;
                }
                Ok(None)
            };

            // Deferred work belongs to this message alone: it is applied
            // only if the message is forwarded, and discarded otherwise.
            let post_dispatch = self.ctx.take_post_dispatch();
            match result {
                Ok(Some((mut data, fds))) => {
                    log::debug!("  -> translated ({} bytes, {} fds)", data.len(), fds.len());

                    // Patch ID in the forwarded message
                    if let Some(gid) = guest_id {
                        if let Some(tid) = target_sender_id {
                            MessageHeader::set_sender(&mut data, tid);
                        } else {
                            // If we can't map the ID, it might be a new object or error.
                            // For ClientToHost, it's usually sender which should be mapped.
                            // We log a warning but send anyway (might fail on host).
                            log::warn!("Could not map sender ID {} to host ID", gid);
                        }
                    }

                    out.push(&data, &fds);
                    for effect in post_dispatch {
                        apply_post_dispatch(&mut self.ctx, effect);
                    }
                }
                Ok(None) => {
                    log::debug!("  -> dropped (unhandled)");
                }
                Err(e) => {
                    let guest_id_for_log = match direction {
                        Direction::ClientToHost => Some(sender_id),
                        Direction::HostToClient => self.ctx.shadow_table.get_guest_id(sender_id),
                    };
                    let interface = guest_id_for_log
                        .and_then(|gid| self.ctx.shadow_table.get_interface(gid).cloned())
                        .unwrap_or_else(|| "unknown".to_string());
                    log::error!("Protocol error: {} (direction={:?}, id={}, guest_id={:?}, interface={}, opcode={})", e, direction, sender_id, guest_id_for_log, interface, opcode);
                    if matches!(direction, Direction::ClientToHost) {
                        // Tell the guest why before closing, as
                        // libwayland-server does. The requests before this
                        // one were valid, so they are forwarded as usual.
                        crate::handler::display::queue_request_error(&mut self.ctx, &e);
                        break;
                    }
                    close_pending_output_fds(&mut out, &conn.read_fds);
                    return false;
                }
            }

            let Some(new_fd_offset) = fd_offset.checked_add(consumed_fds) else {
                log::error!(
                    "Consumed fd offset overflows usize: offset={}, consumed={}",
                    fd_offset,
                    consumed_fds
                );
                close_pending_output_fds(&mut out, &conn.read_fds);
                return false;
            };
            if new_fd_offset > conn.read_fds.len() {
                log::error!(
                    "Protocol consumed more fds than received: consumed={}, received={}",
                    new_fd_offset,
                    conn.read_fds.len()
                );
                close_pending_output_fds(&mut out, &conn.read_fds);
                return false;
            }
            fd_offset = new_fd_offset;

            // Add pending messages from context
            let queue = match direction {
                Direction::ClientToHost => &mut self.ctx.client_to_host_queue,
                Direction::HostToClient => &mut self.ctx.host_to_client_queue,
            };
            for (p_data, p_fds) in queue.drain(..) {
                log::debug!(
                    "  -> adding queued message ({} bytes, {} fds)",
                    p_data.len(),
                    p_fds.len()
                );
                out.push(&p_data, &p_fds);
            }

            offset = message_end;
            processed_messages += 1;
            if self.ctx.fatal_protocol_error {
                break;
            }
            if processed_messages >= MAX_MESSAGES_PER_BATCH {
                break;
            }
        }

        // A session ending here gets its one error even when the cause did
        // not queue one. Toward the guest it goes with the forwarded events;
        // otherwise it is in the reverse queue sent below.
        crate::handler::display::queue_error_if_none_queued(&mut self.ctx);
        if self.ctx.fatal_protocol_error && matches!(direction, Direction::HostToClient) {
            for (p_data, p_fds) in self.ctx.host_to_client_queue.drain(..) {
                out.push(&p_data, &p_fds);
            }
        }

        // Host linux-dmabuf capability discovery emits one format event and
        // then a long modifier stream. The dmabuf handler coalesces feedback
        // refreshes within this dispatch batch; allow the next batch to
        // publish a newer table after more capabilities have arrived.
        self.ctx.synthetic_feedback_refresh_pending.clear();

        let mut success = true;
        if !out.is_empty() && other_conn.send_messages(&out).await.is_err() {
            success = false;
        }

        let mut fds_to_close: std::collections::HashSet<std::os::unix::io::RawFd> =
            std::collections::HashSet::new();
        fds_to_close.extend(out.fds().iter().copied());

        // Bidirectional queue: when processing events in one direction, the
        // handler may queue messages for the OPPOSITE direction. For example,
        // processing a host→client wl_keyboard.key event queues an ack_key
        // message back to the host via client_to_host_queue. Flush that queue
        // now by sending it back through the source connection.
        //
        // Ordering note: the forwarded wl_keyboard.key arrives at the guest
        // *before* the ack_key reaches the host, because forwarded messages are sent
        // first (above). This is intentional and safe: Exo holds the key in
        // pending_key_acks_ with a 1000 ms TTL, so the ack always arrives well
        // within the window. The C sommelier exhibits the same ordering.
        let reverse_queue = match direction {
            Direction::ClientToHost => &mut self.ctx.host_to_client_queue,
            Direction::HostToClient => &mut self.ctx.client_to_host_queue,
        };
        let mut reverse_out = OutgoingMessages::default();
        for (p_data, p_fds) in reverse_queue.drain(..) {
            log::debug!(
                "  -> adding reverse queued message ({} bytes, {} fds)",
                p_data.len(),
                p_fds.len()
            );
            reverse_out.push(&p_data, &p_fds);
        }
        if !reverse_out.is_empty() && conn.send_messages(&reverse_out).await.is_err() {
            // success=false signals the caller (handle_msgs) to drop this
            // client connection. We still close every original below.
            success = false;
        }
        // FDs are closed unconditionally — we own them and must not leak
        // regardless of whether the send succeeded or was skipped. When
        // sendmsg(SCM_RIGHTS) succeeds it duplicates FDs into the kernel
        // cmsg buffer; the originals are still ours to close. When the send
        // fails or the buffer was empty, we obviously retain ownership.
        fds_to_close.extend(reverse_out.fds().iter().copied());

        fds_to_close.extend(conn.read_fds.iter().take(fd_offset));

        for fd in fds_to_close {
            let _ = nix::unistd::close(fd);
        }

        // Remove consumed FDs and data
        conn.read_fds.drain(..fd_offset);
        conn.read_buf.drain(..offset);

        if self.ctx.fatal_protocol_error {
            // A wl_display.error is fatal, but the queued diagnostic must
            // reach the guest before the proxy tears down this session.
            success = false;
        }
        success
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::test_support::{
        any_fd_has_identity, fd_identity, msg_header, msg_opcode, msg_sender, stream_headers,
    };
    use crate::wire::MessageBuilder;
    use nix::sys::memfd::{memfd_create, MFdFlags};
    use std::ffi::CString;
    use std::fs;
    use std::io::Write;
    use std::os::fd::IntoRawFd;

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
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into()).expect("register test socket"),
            WaylandConnection::new(host_socket.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );
        client.ctx.shadow_table.map_id(1, 1);
        client
            .ctx
            .shadow_table
            .track_interface(1, "wl_display".to_string());
        (client, guest_peer, host_peer)
    }

    /// The guest sends `wl_display.sync(callback)`; return the host ID the
    /// proxy allocated for the callback.
    async fn guest_sync(client: &mut Client, callback: u32) -> u32 {
        let mut sync = MessageBuilder::new();
        sync.write_u32(callback);
        let sync = sync.build_message(1, protocols::wayland::wl_display::REQ_SYNC);
        client.client_conn.read_buf.extend_from_slice(&sync);
        assert!(client.handle_msgs(Direction::ClientToHost).await);
        client
            .ctx
            .shadow_table
            .get_host_id(callback)
            .expect("the callback is mapped to a host object")
    }

    /// The host delivers `events` to the proxy.
    async fn host_events(client: &mut Client, events: &[Vec<u8>]) {
        for event in events {
            client.host_conn.read_buf.extend_from_slice(event);
        }
        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert!(client.host_conn.read_buf.is_empty());
    }

    fn callback_done(host_callback: u32) -> Vec<u8> {
        let mut done = MessageBuilder::new();
        done.write_u32(0);
        done.build_message(host_callback, protocols::wayland::wl_callback::EVT_DONE)
    }

    /// Discard what the proxy forwarded to `peer`, so a long exchange does
    /// not fill the socket buffer.
    fn drain(peer: &mut std::os::unix::net::UnixStream) {
        use std::io::Read;
        peer.set_nonblocking(true).expect("non-blocking peer");
        let mut sink = [0u8; 4096];
        loop {
            match peer.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("drain peer: {error}"),
            }
        }
    }

    fn delete_id(host_id: u32) -> Vec<u8> {
        let mut delete_id = MessageBuilder::new();
        delete_id.write_u32(host_id);
        delete_id.build_message(1, protocols::wayland::wl_display::EVT_DELETE_ID)
    }

    /// libwayland-server rejects client object IDs past its map limit, so a
    /// host ID that grew with every object ever created would end the
    /// session after enough frame callbacks.
    #[tokio::test]
    async fn host_ids_are_recycled_through_delete_id() {
        const GUEST_CALLBACK: u32 = 3;
        let (mut client, mut guest_peer, mut host_peer) = client_with_socket_peers();

        let first = guest_sync(&mut client, GUEST_CALLBACK).await;
        host_events(&mut client, &[callback_done(first), delete_id(first)]).await;
        for _ in 0..1000 {
            let host_callback = guest_sync(&mut client, GUEST_CALLBACK).await;
            assert_eq!(host_callback, first, "a deleted host ID must be reused");
            host_events(
                &mut client,
                &[callback_done(host_callback), delete_id(host_callback)],
            )
            .await;
            drain(&mut guest_peer);
            drain(&mut host_peer);
        }
    }

    #[tokio::test]
    async fn host_ids_are_not_reused_before_delete_id() {
        let (mut client, _guest_peer, _host_peer) = client_with_socket_peers();

        // `done` destroys the callback on both sides and sends the guest its
        // delete_id, so the guest may reuse its ID at once; the host ID must
        // wait for the host's own delete_id.
        let first = guest_sync(&mut client, 3).await;
        host_events(&mut client, &[callback_done(first)]).await;
        let second = guest_sync(&mut client, 3).await;
        assert_ne!(second, first);

        host_events(&mut client, &[delete_id(first)]).await;
        assert_eq!(guest_sync(&mut client, 4).await, first);
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
                serve(listener, test_client_config(), None),
            ))
            .await;
        let cpu = thread_cpu_time() - cpu_before;

        assert!(served.is_err(), "serve runs until the process ends");
        assert!(
            cpu < window / 4,
            "the accept loop used {cpu:?} of CPU in {window:?} of failing accepts"
        );
    }

    #[tokio::test]
    async fn sighup_without_a_shortcuts_file_does_not_end_the_proxy() {
        let dir = crate::test_support::TempDir::new("sighup");
        let listener = UnixListener::bind(dir.path().join("socket")).expect("bind listener");
        let config = test_client_config();
        assert!(config.placement.reload().is_none(), "no file to reload");
        let local = tokio::task::LocalSet::new();
        let serving = local.spawn_local(serve(listener, config, install_reload_signal()));
        local
            .run_until(async {
                // Let serve start its reload task and wait for clients.
                tokio::task::yield_now().await;
                // Without a handler this would terminate the test process.
                assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            })
            .await;
        assert!(
            !serving.is_finished(),
            "the proxy keeps serving after the logged no-op reload"
        );
    }

    /// A reload must not wait for the accept loop's retry pause.
    #[tokio::test(start_paused = true)]
    async fn reload_does_not_wait_for_the_accept_retry_pause() {
        use crate::window_layout::WindowLayoutAction;
        use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};

        let alt_q = crate::accelerator::parse_accelerator("<Alt>q").unwrap();
        let dir = crate::test_support::TempDir::new("reload-during-retry");
        let shortcuts = dir.write(
            "shortcuts.toml",
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n",
        );
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

        let local = tokio::task::LocalSet::new();
        local.spawn_local(serve(listener, config, install_reload_signal()));
        local
            .run_until(async {
                tokio::task::yield_now().await;
                let started = tokio::time::Instant::now();
                dir.write(
                    "shortcuts.toml",
                    b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"right\"\n",
                );
                assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);
                // The clock is paused, so it moves only while every task waits.
                while placement.action_for(&alt_q) != Some(WindowLayoutAction::Right) {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert!(
                    started.elapsed() < ACCEPT_RETRY_DELAY,
                    "the reload waited {:?} for the accept loop",
                    started.elapsed()
                );
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
        client.ctx.shadow_table.map_id(GUEST_OFFER, HOST_OFFER);
        client
            .ctx
            .shadow_table
            .track_interface(GUEST_OFFER, "wl_data_offer".to_string());

        // The guest asks for the selection once per descriptor, in one write
        // the proxy dispatches as one batch.
        let mut pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let mut receive = MessageBuilder::new();
        receive.write_string("text/plain");
        let receive =
            receive.build_message(GUEST_OFFER, protocols::wayland::wl_data_offer::REQ_RECEIVE);
        let requests = receive.repeat(RECEIVES);
        let fds = [pipe[1]; RECEIVES];
        let sent = nix::sys::socket::sendmsg::<()>(
            guest_peer.as_raw_fd(),
            &[std::io::IoSlice::new(&requests)],
            &[nix::sys::socket::ControlMessage::ScmRights(&fds)],
            nix::sys::socket::MsgFlags::empty(),
            None,
        )
        .expect("send the guest requests");
        assert_eq!(sent, requests.len());
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
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
            client.ctx.shadow_table.map_id(guest_id, host_id);
            client.ctx.shadow_table.track_interface_with_version(
                guest_id,
                "wl_keyboard".to_string(),
                10,
            );
            client.ctx.shadow_table.track_host_interface_with_version(
                host_id,
                "wl_keyboard".to_string(),
                10,
            );
        }
        client
            .ctx
            .shadow_table
            .mark_pending_destroy(STALE_GUEST_KEYBOARD);
        client.ctx.keyboard_repeatable_keys.insert(
            crate::state::HostId(STALE_HOST_KEYBOARD),
            std::collections::HashSet::from([57]),
        );

        (client, guest_peer, _host_peer)
    }

    fn duplicate_pipe_read_fd() -> (RawFd, std::path::PathBuf) {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let keymap_fd = unsafe { libc::fcntl(pipe_fds[0], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(keymap_fd >= 1000, "fd duplication should succeed");
        let keymap_target =
            fs::read_link(format!("/proc/self/fd/{keymap_fd}")).expect("keymap fd target");
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
        (keymap_fd, keymap_target)
    }

    fn stale_keymap_event(keymap_fd: RawFd) -> Vec<u8> {
        use crate::protocols::wayland::wl_keyboard;

        let mut keymap = MessageBuilder::new();
        keymap.write_u32(1);
        keymap.write_fd(keymap_fd);
        keymap.write_u32(0);
        keymap.build_message(STALE_HOST_KEYBOARD, wl_keyboard::EVT_KEYMAP)
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
        let (keymap_fd, keymap_target) = duplicate_pipe_read_fd();
        let keymap = stale_keymap_event(keymap_fd);
        let live_repeat_info = repeat_info_event(LIVE_HOST_KEYBOARD);
        let expected_repeat_info = repeat_info_event(LIVE_GUEST_KEYBOARD);

        client.host_conn.read_buf.extend_from_slice(&keymap);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&live_repeat_info);
        client.host_conn.read_fds.push(keymap_fd);

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
        assert_eq!(
            client.ctx.keyboard_repeatable_keys[&crate::state::HostId(STALE_HOST_KEYBOARD)],
            std::collections::HashSet::from([57]),
            "schema consumption must not invoke the keymap handler"
        );
        assert_live_repeat_info_forwarded(&mut guest_peer, &expected_repeat_info);
        assert_fd_released(keymap_fd, &keymap_target);
    }

    #[tokio::test]
    async fn pending_destroy_event_consumes_known_fd_before_partial_followup() {
        use std::io::Read;

        let (mut client, mut guest_peer, _host_peer) = pending_keyboard_client();
        let (keymap_fd, keymap_target) = duplicate_pipe_read_fd();
        let keymap = stale_keymap_event(keymap_fd);
        let live_repeat_info = repeat_info_event(LIVE_HOST_KEYBOARD);
        let expected_repeat_info = repeat_info_event(LIVE_GUEST_KEYBOARD);

        client.host_conn.read_buf.extend_from_slice(&keymap);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&live_repeat_info[..4]);
        client.host_conn.read_fds.push(keymap_fd);

        assert!(client.handle_msgs(Direction::HostToClient).await);
        assert_eq!(
            client.host_conn.read_buf,
            live_repeat_info[..4],
            "the incomplete follow-up must remain buffered"
        );
        assert!(client.host_conn.read_fds.is_empty());
        assert!(!client.host_conn.ambiguous_untracked_fd);
        assert_eq!(
            client.ctx.keyboard_repeatable_keys[&crate::state::HostId(STALE_HOST_KEYBOARD)],
            std::collections::HashSet::from([57]),
            "schema consumption must not invoke the keymap handler"
        );
        let mut premature = [0u8; 1];
        let read_error = guest_peer
            .read(&mut premature)
            .expect_err("an incomplete live event must not be forwarded");
        assert_eq!(read_error.kind(), io::ErrorKind::WouldBlock);
        assert_fd_released(keymap_fd, &keymap_target);

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

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let pending_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        // Sender 77 is complete but deliberately untracked. The four bytes
        // after it are only a partial header for the next message. There is no
        // interface metadata with which to determine whether the descriptor
        // belongs to the stale message or the partial follow-up, so the
        // descriptor must remain queued while the complete stale message is
        // dropped. The connection stays alive for the rest of the follow-up.
        let stale_event = MessageBuilder::new().build_message(77, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        client.host_conn.read_buf.extend_from_slice(&[0, 0, 0, 0]);
        client.host_conn.read_fds.push(pending_fd);

        assert!(
            client.handle_msgs(Direction::HostToClient).await,
            "a pending FD after an untracked complete message must not terminate the connection"
        );
        assert!(
            client.host_conn.read_buf.len() == 4,
            "only the complete stale message should be consumed"
        );
        assert_eq!(
            client.host_conn.read_fds,
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

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let pending_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        let stale_event = MessageBuilder::new().build_message(77, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        // Keep only a partial header for the follow-up message in the first
        // receive batch, exactly as a stream socket may do.
        let known_event = MessageBuilder::new().build_message(88, 0);
        client
            .host_conn
            .read_buf
            .extend_from_slice(&known_event[..4]);
        client.host_conn.read_fds.push(pending_fd);
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
            .track_host_interface(88, "wl_display".to_string());
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

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let pending_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000, "fd duplication should succeed");
        let pending_target =
            fs::read_link(format!("/proc/self/fd/{pending_fd}")).expect("pending fd target");
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        // Both messages are complete, but the first sender is untracked. The
        // descriptor arrived on the ordered stream with these bytes and
        // cannot be assigned to either message. The proxy must not guess and
        // accidentally deliver it to the known follow-up.
        let stale_event = MessageBuilder::new().build_message(77, 0);
        let known_event = MessageBuilder::new().build_message(88, 0);
        client.host_conn.read_buf.extend_from_slice(&stale_event);
        client.host_conn.read_buf.extend_from_slice(&known_event);
        client.host_conn.read_fds.push(pending_fd);
        client
            .ctx
            .shadow_table
            .track_host_interface(88, "wl_display".to_string());

        assert!(
            !client.handle_msgs(Direction::HostToClient).await,
            "complete untracked message plus a complete follow-up is fatal FD ambiguity"
        );
        drop(client);

        assert_fd_released(pending_fd, &pending_target);
    }

    /// The object and code of each `wl_display.error` the guest received
    /// before the proxy closed the connection.
    fn display_errors(guest_peer: &mut std::os::unix::net::UnixStream) -> Vec<(u32, u32)> {
        use std::io::Read;
        let mut events = Vec::new();
        guest_peer
            .read_to_end(&mut events)
            .expect("read the guest's events");
        crate::test_support::frames(&events)
            .filter(|(header, _)| {
                (header.sender_id, header.opcode)
                    == (
                        crate::wire::WL_DISPLAY_ID,
                        protocols::wayland::wl_display::EVT_ERROR,
                    )
            })
            .map(|(header, payload)| {
                let mut error = WireMessage::new(header.sender_id, header.opcode, payload, &[]);
                (error.read_u32().unwrap(), error.read_u32().unwrap())
            })
            .collect()
    }

    #[tokio::test]
    async fn unknown_client_sender_is_a_protocol_error() {
        let (mut client, mut guest_peer, _host_peer) = client_with_socket_peers();

        // A client request with an unknown sender is malformed Wayland
        // traffic. It must not be silently discarded because doing so leaves
        // the client believing that the request succeeded.
        let mut builder = MessageBuilder::new();
        builder.write_u32(20);
        let request = builder.build_message(999, protocols::wayland::wl_display::REQ_GET_REGISTRY);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(
            !client.handle_msgs(Direction::ClientToHost).await,
            "unknown client sender IDs must terminate the connection"
        );
        drop(client);
        assert_eq!(
            display_errors(&mut guest_peer),
            [(
                crate::wire::WL_DISPLAY_ID,
                protocols::wayland::wl_display::ERROR_INVALID_OBJECT
            )],
            "the guest learns why, as libwayland-server reports an unknown sender"
        );
    }

    #[tokio::test]
    async fn undecodable_request_is_reported_after_forwarding_earlier_requests() {
        use std::io::Read;
        let (mut client, mut guest_peer, mut host_peer) = client_with_socket_peers();

        // A whole wl_display.sync, then one without its callback argument.
        let mut sync = MessageBuilder::new();
        sync.write_u32(2);
        let sync = sync.build_message(1, protocols::wayland::wl_display::REQ_SYNC);
        let truncated =
            MessageBuilder::new().build_message(1, protocols::wayland::wl_display::REQ_SYNC);
        client.client_conn.read_buf.extend_from_slice(&sync);
        client.client_conn.read_buf.extend_from_slice(&truncated);

        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        drop(client);
        assert_eq!(
            display_errors(&mut guest_peer),
            [(1, protocols::wayland::wl_display::ERROR_INVALID_METHOD)]
        );
        let mut forwarded = Vec::new();
        host_peer
            .read_to_end(&mut forwarded)
            .expect("read the forwarded requests");
        assert_eq!(
            stream_headers(&forwarded),
            [(1, protocols::wayland::wl_display::REQ_SYNC)],
            "the valid request before the bad one still reaches the host"
        );
    }

    /// The guest's `wl_surface` in [`client_with_a_surface`].
    const SURFACE: u32 = 5;

    /// A proxy whose guest has the v4 `wl_surface` [`SURFACE`].
    fn client_with_a_surface() -> (
        Client,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        let (mut client, guest_peer, host_peer) = client_with_socket_peers();
        let host_surface = 6;
        client.ctx.shadow_table.map_id(SURFACE, host_surface);
        client
            .ctx
            .shadow_table
            .track_interface_with_version(SURFACE, "wl_surface".to_string(), 4);
        client.ctx.shadow_table.track_host_interface_with_version(
            host_surface,
            "wl_surface".to_string(),
            4,
        );
        (client, guest_peer, host_peer)
    }

    /// The errors the guest receives when, after `prepare`, `SURFACE`
    /// sends request `opcode` with `payload`.
    async fn surface_request_errors(
        prepare: impl FnOnce(&mut Client),
        opcode: u16,
        payload: &[u32],
    ) -> Vec<(u32, u32)> {
        let (mut client, mut guest_peer, _host_peer) = client_with_a_surface();
        prepare(&mut client);
        let mut request = MessageBuilder::new();
        payload.iter().for_each(|&word| request.write_u32(word));
        client
            .client_conn
            .read_buf
            .extend(request.build_message(SURFACE, opcode));
        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        drop(client);
        display_errors(&mut guest_peer)
    }

    /// libwayland-server reports every rejected request on `wl_display`,
    /// and only an unusable sender as `invalid_object`: the code is read
    /// against the interface of the error's object.
    #[tokio::test]
    async fn rejected_requests_report_invalid_method_on_the_display() {
        use protocols::wayland::wl_display::ERROR_INVALID_METHOD;
        use protocols::wayland::wl_surface::REQ_ATTACH;
        let expected = [(crate::wire::WL_DISPLAY_ID, ERROR_INVALID_METHOD)];

        assert_eq!(
            surface_request_errors(|_| {}, REQ_ATTACH, &[0]).await,
            expected,
            "an attach without its offsets does not decode"
        );
        assert_eq!(
            surface_request_errors(|_| {}, REQ_ATTACH, &[999, 0, 0]).await,
            expected,
            "an attach of an unknown buffer names an unusable object argument"
        );
        assert_eq!(
            surface_request_errors(|_| {}, REQ_ATTACH, &[SURFACE, 0, 0]).await,
            expected,
            "a bad object argument is invalid_method even when it is the sender"
        );
    }

    #[tokio::test]
    async fn a_destroyed_sender_is_an_invalid_object_whatever_it_sends() {
        let errors = surface_request_errors(
            |client| client.ctx.shadow_table.mark_pending_destroy(SURFACE),
            99,
            &[],
        )
        .await;
        assert_eq!(
            errors,
            [(
                crate::wire::WL_DISPLAY_ID,
                protocols::wayland::wl_display::ERROR_INVALID_OBJECT
            )],
            "the sender is checked before its opcode, as in libwayland-server"
        );
    }

    #[tokio::test]
    async fn a_session_gets_exactly_one_error() {
        use protocols::wayland::wl_surface::{REQ_COMMIT, REQ_SET_BUFFER_SCALE};
        // The handler rejects scale 0 with wl_surface.invalid_scale; the
        // extra word then leaves trailing data, which must not add another.
        assert_eq!(
            surface_request_errors(|_| {}, REQ_SET_BUFFER_SCALE, &[0, 0]).await,
            [(SURFACE, 0)]
        );
        // A handler can end the session without queueing an error (a host
        // storage failure, say); the trailing data still gets reported.
        assert_eq!(
            surface_request_errors(
                |client| client.ctx.fatal_protocol_error = true,
                REQ_COMMIT,
                &[0]
            )
            .await,
            [(
                crate::wire::WL_DISPLAY_ID,
                protocols::wayland::wl_display::ERROR_INVALID_METHOD
            )]
        );
    }

    #[tokio::test]
    async fn an_internal_failure_ends_the_session_with_an_implementation_error() {
        use protocols::wayland::wl_surface::REQ_COMMIT;
        // As a failed dma-buf sync during a commit does: fatal, with no
        // error of its own.
        assert_eq!(
            surface_request_errors(
                |client| client.ctx.fatal_protocol_error = true,
                REQ_COMMIT,
                &[]
            )
            .await,
            [(
                crate::wire::WL_DISPLAY_ID,
                protocols::wayland::wl_display::ERROR_IMPLEMENTATION
            )]
        );
    }

    /// The host's error is the guest's one error: a malformed request that
    /// follows it gets none of its own.
    #[tokio::test]
    async fn a_host_error_is_the_sessions_only_error() {
        let (mut client, mut guest_peer, _host_peer) = client_with_a_surface();
        let host_surface = client.ctx.shadow_table.get_host_id(SURFACE).unwrap();
        let mut error = MessageBuilder::new();
        error.write_u32(host_surface);
        error.write_u32(2);
        error.write_string("host failure");
        client.host_conn.read_buf.extend(error.build_message(
            crate::wire::WL_DISPLAY_ID,
            protocols::wayland::wl_display::EVT_ERROR,
        ));
        assert!(!client.handle_msgs(Direction::HostToClient).await);
        client
            .client_conn
            .read_buf
            .extend(MessageBuilder::new().build_message(SURFACE, 99));
        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        drop(client);
        assert_eq!(display_errors(&mut guest_peer), [(SURFACE, 2)]);
    }

    #[tokio::test]
    async fn an_invalid_message_length_is_reported_before_closing() {
        let (mut client, mut guest_peer, _host_peer) = client_with_socket_peers();
        // A header claiming 6 bytes, less than the header itself.
        client
            .client_conn
            .read_buf
            .extend([1u32.to_ne_bytes(), (6u32 << 16).to_ne_bytes()].concat());
        assert!(!client.handle_msgs(Direction::ClientToHost).await);
        drop(client);
        assert_eq!(
            display_errors(&mut guest_peer),
            [(
                crate::wire::WL_DISPLAY_ID,
                protocols::wayland::wl_display::ERROR_INVALID_METHOD
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
        ctx.shadow_table.map_id(XDG_SURFACE, XDG_SURFACE_HOST);
        ctx.shadow_table
            .track_interface_with_version(XDG_SURFACE, "xdg_surface".to_string(), 6);
        ctx.shadow_table.set_host_version(XDG_SURFACE_HOST, 6);
        ctx.shadow_table.map_id(WL_SURFACE, 22);
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
        client.ctx.shadow_table.mark_pending_destroy(XDG_SURFACE);
        let request = get_toplevel_request(&[]);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(
            !client.handle_msgs(Direction::ClientToHost).await,
            "a request from a pending xdg_surface must terminate the session"
        );
        assert!(
            client.ctx.window_placement.aura_toplevel_count() == 0,
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
            .aura_toplevel_for_xdg_toplevel(XDG_TOPLEVEL)
            .expect("placement Aura toplevel");
        assert!(!client.ctx.has_post_dispatch());
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
        assert_eq!(client.ctx.window_placement.aura_toplevel_count(), 0);
        assert!(!client.ctx.has_post_dispatch());
    }

    #[tokio::test]
    async fn invalid_registry_bind_flushes_display_error_before_disconnect() {
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        let (guest_proxy, guest_peer) =
            UnixStream::pair().expect("guest socket pair should be created");
        let (host_proxy, host_peer) =
            UnixStream::pair().expect("host socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(guest_proxy.into()).expect("register test socket"),
            WaylandConnection::new(host_proxy.into()).expect("register test socket"),
            Context::new_for_test(false, false, Vec::new()),
        );

        // Register one advertised global under the host-side registry ID so
        // the bind reaches the registry handler's validation path.
        client.ctx.shadow_table.map_id(10, 20);
        client
            .ctx
            .shadow_table
            .track_interface(10, "wl_registry".to_string());
        client.ctx.host_globals.insert(
            7,
            crate::state::HostGlobal {
                interface: "wl_compositor".to_string(),
                version: 1,
            },
        );
        client
            .ctx
            .registry_global_names
            .entry(20)
            .or_default()
            .insert(7);
        client.ctx.global_generations.insert(7, 1);
        client
            .ctx
            .registry_global_generations
            .entry(20)
            .or_default()
            .insert(7, 1);

        // Ask for the advertised global with a mismatched interface. The
        // proxy must send wl_display.error to the guest and then terminate
        // this direction rather than silently dropping the request.
        let mut builder = MessageBuilder::new();
        builder.write_u32(7);
        builder.write_string("wl_seat");
        builder.write_u32(1);
        builder.write_u32(30);
        let request = builder.build_message(10, protocols::wayland::wl_registry::REQ_BIND);
        client.client_conn.read_buf.extend_from_slice(&request);

        assert!(
            !client.handle_msgs(Direction::ClientToHost).await,
            "an invalid bind must terminate the guest session"
        );
        drop(host_peer);
        drop(client);

        let mut header = [0u8; 8];
        let mut guest_peer = guest_peer;
        guest_peer
            .read_exact(&mut header)
            .expect("guest must receive the fatal display error before disconnect");
        let header = msg_header(&header);
        assert_eq!(header.sender_id, 1);
        assert_eq!(header.opcode, protocols::wayland::wl_display::EVT_ERROR);
        assert!(
            header.len >= 20,
            "display.error must include object, code, and a diagnostic string"
        );
    }

    #[test]
    fn raw_proxy_dispatch_balances_each_keyboard_focus_generation_once() {
        use crate::protocols::wayland::wl_keyboard;
        use crate::state::HostId;

        fn dispatch_keyboard_event(
            handler: &mut SommelierHandler,
            ctx: &mut Context,
            message: Vec<u8>,
        ) -> Option<(Vec<u8>, Vec<std::os::unix::io::RawFd>)> {
            let header = msg_header(&message);
            let mut wire = WireMessage::new(
                header.sender_id,
                header.opcode,
                &message[MessageHeader::LEN..],
                &[],
            );
            let result = Client::dispatch_event(handler, ctx, "wl_keyboard", &mut wire)
                .expect("raw keyboard event must dispatch");
            assert!(
                wire.is_payload_consumed(),
                "proxy dispatch must consume the complete keyboard event"
            );
            result
        }

        fn enter(keyboard: u32, serial: u32, surface: u32) -> Vec<u8> {
            let mut builder = MessageBuilder::new();
            builder.write_u32(serial);
            builder.write_u32(surface);
            builder.write_array(&[]);
            builder.build_message(keyboard, wl_keyboard::EVT_ENTER)
        }

        fn leave(keyboard: u32, serial: u32, surface: u32) -> Vec<u8> {
            let mut builder = MessageBuilder::new();
            builder.write_u32(serial);
            builder.write_u32(surface);
            builder.build_message(keyboard, wl_keyboard::EVT_LEAVE)
        }

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut handler = SommelierHandler::new();
        let guest_seat = 1;
        let guest_surface = 20;
        let host_surface = 200;
        let replacement_guest_surface = 21;
        let replacement_host_surface = 201;
        let keyboard_a = (10, 100);
        let keyboard_b = (11, 101);

        for (guest, host) in [
            (guest_surface, host_surface),
            (replacement_guest_surface, replacement_host_surface),
        ] {
            ctx.shadow_table.map_id(guest, host);
            ctx.shadow_table
                .track_interface_with_version(guest, "wl_surface".to_string(), 1);
            ctx.shadow_table
                .track_host_interface_with_version(host, "wl_surface".to_string(), 1);
        }
        for (guest_keyboard, host_keyboard) in [keyboard_a, keyboard_b] {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.shadow_table.track_interface_with_version(
                guest_keyboard,
                "wl_keyboard".to_string(),
                10,
            );
            ctx.shadow_table.track_host_interface_with_version(
                host_keyboard,
                "wl_keyboard".to_string(),
                10,
            );
            ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        }

        assert!(dispatch_keyboard_event(
            &mut handler,
            &mut ctx,
            enter(keyboard_a.1, 1, host_surface)
        )
        .is_some());
        assert!(
            dispatch_keyboard_event(&mut handler, &mut ctx, enter(keyboard_b.1, 2, host_surface))
                .is_some(),
            "a second keyboard resource needs its own forwarded enter"
        );
        assert!(
            dispatch_keyboard_event(&mut handler, &mut ctx, enter(keyboard_b.1, 3, host_surface))
                .is_none(),
            "an exact duplicate enter must be suppressed"
        );
        assert!(ctx.host_to_client_queue.is_empty());

        assert!(dispatch_keyboard_event(
            &mut handler,
            &mut ctx,
            leave(keyboard_a.1, 4, host_surface)
        )
        .is_some());
        assert_eq!(
            ctx.keyboard_focus
                .focus_for_keyboard(HostId(keyboard_b.1))
                .map(|focus| focus.guest_surface),
            Some(guest_surface)
        );
        assert!(
            dispatch_keyboard_event(&mut handler, &mut ctx, leave(keyboard_a.1, 5, host_surface))
                .is_none(),
            "a duplicate leave must not escape the proxy"
        );
        assert!(dispatch_keyboard_event(
            &mut handler,
            &mut ctx,
            leave(keyboard_b.1, 6, host_surface)
        )
        .is_some());
        assert_eq!(ctx.keyboard_focus.surface_for_seat(guest_seat), None);

        // A different keyboard can replace the seat's surface before the
        // old resource receives its leave. That retired enter still needs one
        // guest-visible leave, without changing the replacement IME focus.
        assert!(dispatch_keyboard_event(
            &mut handler,
            &mut ctx,
            enter(keyboard_a.1, 7, host_surface)
        )
        .is_some());
        assert!(dispatch_keyboard_event(
            &mut handler,
            &mut ctx,
            enter(keyboard_b.1, 8, replacement_host_surface)
        )
        .is_some());
        assert!(
            dispatch_keyboard_event(&mut handler, &mut ctx, leave(keyboard_a.1, 9, host_surface))
                .is_some(),
            "the replaced keyboard's enter must receive one balancing leave"
        );
        assert_eq!(
            ctx.keyboard_focus.surface_for_seat(guest_seat),
            Some(replacement_guest_surface)
        );
        assert!(
            dispatch_keyboard_event(
                &mut handler,
                &mut ctx,
                leave(keyboard_a.1, 10, host_surface)
            )
            .is_none(),
            "the balancing delayed leave must be consumed exactly once"
        );
    }

    const GUEST_KEYBOARD: u32 = 10;
    const HOST_KEYBOARD: u32 = 100;
    const HOST_EXTENDED_KEYBOARD: u32 = 1_000;
    const GUEST_TEXT_INPUT: u32 = 40;
    const GUEST_TEXT_INPUT_MANAGER: u32 = 41;
    const HOST_TEXT_INPUT: u32 = 140;
    const HOST_TEXT_INPUT_MANAGER: u32 = 141;
    const HOST_EXTENDED_TEXT_INPUT: u32 = 2_000;
    const GUEST_SEAT: u32 = 1;
    const HOST_SEAT: u32 = 902;
    const GUEST_SURFACE: u32 = 900;
    const HOST_SURFACE: u32 = 901;
    const KEY_BACKSPACE: u32 = 14;
    const KEY_SPACE: u32 = 57;
    const KEY_RELEASED: u32 = 0;
    const KEY_PRESSED: u32 = 1;
    const KEY_REPEATED: u32 = 2;

    fn dispatch_raw_event_result(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        message: Vec<u8>,
    ) -> Option<(Vec<u8>, Vec<std::os::unix::io::RawFd>)> {
        let header = msg_header(&message);
        let mut wire = WireMessage::new(
            header.sender_id,
            header.opcode,
            &message[MessageHeader::LEN..],
            &[],
        );
        let result = Client::dispatch_event(handler, ctx, interface, &mut wire)
            .expect("raw host event must dispatch");
        assert!(
            wire.is_payload_consumed(),
            "proxy dispatch must consume the complete raw event payload"
        );
        result
    }

    fn dispatch_raw_event(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        message: Vec<u8>,
    ) {
        assert!(
            dispatch_raw_event_result(handler, ctx, interface, message).is_none(),
            "raw host event must be consumed by the proxy handler"
        );
    }

    fn dispatch_raw_request_result(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        message: Vec<u8>,
    ) -> Option<(Vec<u8>, Vec<std::os::unix::io::RawFd>)> {
        let header = msg_header(&message);
        let mut wire = WireMessage::new(
            header.sender_id,
            header.opcode,
            &message[MessageHeader::LEN..],
            &[],
        );
        let result = Client::dispatch_request(handler, ctx, interface, &mut wire)
            .expect("raw guest request must dispatch");
        assert!(
            wire.is_payload_consumed(),
            "proxy dispatch must consume the complete raw request payload"
        );
        result
    }

    fn sender(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u32 {
        msg_sender(&message.0)
    }

    fn opcode(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u16 {
        msg_opcode(&message.0)
    }

    #[test]
    fn raw_gtk_surface_request_is_consumed_and_translated_to_aura() {
        const GTK_SHELL: u32 = 40;
        const GTK_SURFACE: u32 = 41;
        const GUEST_SURFACE: u32 = 42;
        const HOST_SURFACE: u32 = 142;
        const AURA_SHELL: u32 = 240;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table
            .track_interface_with_version(GTK_SHELL, "gtk_shell1".to_string(), 1);
        ctx.gtk_shells
            .insert(GTK_SHELL, crate::state::GtkShellState::default());
        register_raw_surface(&mut ctx, GUEST_SURFACE, HOST_SURFACE);
        ctx.bind_aura_shell_for_test(AURA_SHELL);
        ctx.shadow_table.track_host_interface_with_version(
            AURA_SHELL,
            "zaura_shell".to_string(),
            38,
        );
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
        ctx.shadow_table.map_id(guest_surface, host_surface);
        ctx.shadow_table
            .track_interface_with_version(guest_surface, "wl_surface".to_string(), 1);
        ctx.shadow_table.track_host_interface_with_version(
            host_surface,
            "wl_surface".to_string(),
            1,
        );
    }

    fn raw_keyboard_enter(serial: u32, host_surface: u32) -> Vec<u8> {
        use crate::protocols::wayland::wl_keyboard;

        let mut event = MessageBuilder::new();
        event.write_u32(serial);
        event.write_u32(host_surface);
        event.write_array(&[]);
        event.build_message(HOST_KEYBOARD, wl_keyboard::EVT_ENTER)
    }

    fn raw_keyboard_leave(serial: u32, host_surface: u32) -> Vec<u8> {
        use crate::protocols::wayland::wl_keyboard;

        let mut event = MessageBuilder::new();
        event.write_u32(serial);
        event.write_u32(host_surface);
        event.build_message(HOST_KEYBOARD, wl_keyboard::EVT_LEAVE)
    }

    fn raw_text_input_request(opcode: u16) -> Vec<u8> {
        MessageBuilder::new().build_message(GUEST_TEXT_INPUT, opcode)
    }

    fn raw_text_input_request_for(guest_text_input: u32, opcode: u16) -> Vec<u8> {
        MessageBuilder::new().build_message(guest_text_input, opcode)
    }

    fn raw_peek_key(serial: u32, time: u32, key: u32, state: u32) -> Vec<u8> {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;

        let mut event = MessageBuilder::new();
        event.write_u32(serial);
        event.write_u32(time);
        event.write_u32(key);
        event.write_u32(state);
        event.build_message(
            HOST_EXTENDED_KEYBOARD,
            zcr_extended_keyboard_v1::EVT_PEEK_KEY,
        )
    }

    fn raw_preedit(host_text_input: u32, serial: u32, text: &str) -> Vec<u8> {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;

        let mut event = MessageBuilder::new();
        event.write_u32(serial);
        event.write_string(text);
        event.write_string("");
        event.build_message(host_text_input, zwp_text_input_v1::EVT_PREEDIT_STRING)
    }

    fn raw_commit_string(host_text_input: u32, serial: u32, text: &str) -> Vec<u8> {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;

        let mut event = MessageBuilder::new();
        event.write_u32(serial);
        event.write_string(text);
        event.build_message(host_text_input, zwp_text_input_v1::EVT_COMMIT_STRING)
    }

    fn raw_confirm_preedit() -> Vec<u8> {
        use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;

        let mut event = MessageBuilder::new();
        event.write_u32(1);
        event.build_message(
            HOST_EXTENDED_TEXT_INPUT,
            zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
        )
    }

    fn raw_callback_done(callback_id: u32) -> Vec<u8> {
        use crate::protocols::wayland::wl_callback;

        let mut event = MessageBuilder::new();
        event.write_u32(0);
        event.build_message(callback_id, wl_callback::EVT_DONE)
    }

    fn setup_raw_ime_dispatch() -> (Context, SommelierHandler) {
        use crate::state::{HostId, TextInputState};

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table
            .track_host_interface_with_version(1, "wl_display".to_string(), 1);
        ctx.shadow_table.map_id(GUEST_SEAT, HOST_SEAT);
        ctx.shadow_table
            .track_interface_with_version(GUEST_SEAT, "wl_seat".to_string(), 1);
        ctx.shadow_table
            .track_host_interface_with_version(HOST_SEAT, "wl_seat".to_string(), 1);

        ctx.shadow_table.map_id(GUEST_KEYBOARD, HOST_KEYBOARD);
        ctx.shadow_table.track_interface_with_version(
            GUEST_KEYBOARD,
            "wl_keyboard".to_string(),
            10,
        );
        ctx.shadow_table.set_host_version(HOST_KEYBOARD, 10);
        ctx.shadow_table.track_host_interface_with_version(
            HOST_EXTENDED_KEYBOARD,
            "zcr_extended_keyboard_v1".to_string(),
            2,
        );
        ctx.keyboard_to_seat.insert(GUEST_KEYBOARD, GUEST_SEAT);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(HOST_KEYBOARD), HostId(HOST_EXTENDED_KEYBOARD));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(HOST_EXTENDED_KEYBOARD), HostId(HOST_KEYBOARD));
        ctx.keyboard_repeatable_keys
            .entry(HostId(HOST_KEYBOARD))
            .or_default()
            .extend([KEY_BACKSPACE, KEY_SPACE]);
        ctx.keyboard_focus.set_for_test(
            HostId(HOST_KEYBOARD),
            GUEST_SEAT,
            GUEST_SURFACE,
            HOST_SURFACE,
        );
        ctx.shadow_table.map_id(GUEST_SURFACE, HOST_SURFACE);
        ctx.shadow_table
            .track_interface_with_version(GUEST_SURFACE, "wl_surface".to_string(), 1);
        ctx.shadow_table.track_host_interface_with_version(
            HOST_SURFACE,
            "wl_surface".to_string(),
            1,
        );
        ctx.host_text_input_manager_v1_id = Some(HOST_TEXT_INPUT_MANAGER);
        ctx.shadow_table.track_host_interface_with_version(
            HOST_TEXT_INPUT_MANAGER,
            "zwp_text_input_manager_v1".to_string(),
            1,
        );
        ctx.shadow_table.track_interface_with_version(
            GUEST_TEXT_INPUT_MANAGER,
            "zwp_text_input_manager_v3".to_string(),
            1,
        );

        ctx.shadow_table.map_id(GUEST_TEXT_INPUT, HOST_TEXT_INPUT);
        ctx.shadow_table.track_interface_with_version(
            GUEST_TEXT_INPUT,
            "zwp_text_input_v3".to_string(),
            1,
        );
        ctx.shadow_table.track_host_interface_with_version(
            HOST_TEXT_INPUT,
            "zwp_text_input_v1".to_string(),
            1,
        );
        ctx.shadow_table.track_host_interface_with_version(
            HOST_EXTENDED_TEXT_INPUT,
            "zcr_extended_text_input_v1".to_string(),
            11,
        );
        ctx.text_inputs.insert(GUEST_TEXT_INPUT, {
            let mut state = TextInputState::new(
                HOST_TEXT_INPUT,
                Some(HOST_EXTENDED_TEXT_INPUT),
                GUEST_SEAT,
                Some(GUEST_SURFACE),
            );
            state.pending_enabled = true;
            state.committed_enabled = true;
            state.committed_surrounding_text = Some(("가".to_string(), 3, 3));
            state.set_guest_commits(1);
            state.host_activation = crate::state::HostActivationState::Active;
            state
        });

        (ctx, SommelierHandler::new())
    }

    fn arm_raw_backspace_repeat(
        ctx: &mut Context,
        handler: &mut SommelierHandler,
        host_text_input: u32,
        guest_text_input: u32,
    ) {
        use crate::state::HostId;

        dispatch_raw_event(
            handler,
            ctx,
            "zcr_extended_keyboard_v1",
            raw_peek_key(700, 1_234, KEY_BACKSPACE, KEY_PRESSED),
        );
        for text in ["가", ""] {
            dispatch_raw_event(
                handler,
                ctx,
                "zwp_text_input_v1",
                raw_preedit(host_text_input, 1, text),
            );
        }
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            Some(guest_text_input)
        );
    }

    #[test]
    fn raw_proxy_dispatch_retires_equal_serial_peek_release() {
        use crate::state::HostId;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        let shared_serial = 18_857;

        for (time, state) in [(100, KEY_PRESSED), (110, KEY_RELEASED)] {
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zcr_extended_keyboard_v1",
                raw_peek_key(shared_serial, time, KEY_BACKSPACE, state),
            );
        }
        assert!(
            !ctx.key_generations
                .physically_held(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            "generated dispatch must retire a release that shares its press serial"
        );

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            raw_peek_key(shared_serial, 120, KEY_BACKSPACE, KEY_PRESSED),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            Some(GUEST_TEXT_INPUT),
            "the next generation must remain eligible for IME repeat recovery"
        );
    }

    #[test]
    fn raw_confirm_preedit_preserves_delete_until_commit_string() {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_preedit(HOST_TEXT_INPUT, 1, "가"),
        );

        let mut delete = MessageBuilder::new();
        delete.write_i32(-3);
        delete.write_u32(3);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            delete.build_message(
                HOST_TEXT_INPUT,
                zwp_text_input_v1::EVT_DELETE_SURROUNDING_TEXT,
            ),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );

        ctx.host_to_client_queue.clear();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_commit_string(HOST_TEXT_INPUT, 1, "나"),
        );

        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(
            opcode(&ctx.host_to_client_queue[0]),
            zwp_text_input_v3::EVT_DELETE_SURROUNDING_TEXT
        );
        assert_eq!(
            opcode(&ctx.host_to_client_queue[1]),
            zwp_text_input_v3::EVT_COMMIT_STRING
        );
        assert_eq!(
            opcode(&ctx.host_to_client_queue[2]),
            zwp_text_input_v3::EVT_DONE
        );
        let mut delete = WireMessage::new(
            GUEST_TEXT_INPUT,
            zwp_text_input_v3::EVT_DELETE_SURROUNDING_TEXT,
            &ctx.host_to_client_queue[0].0[8..],
            &[],
        );
        assert_eq!(delete.read_u32().unwrap(), 3);
        assert_eq!(delete.read_u32().unwrap(), 0);
        assert!(delete.is_payload_consumed());
    }

    #[test]
    fn raw_sibling_lifecycle_preserves_active_repeat_owner() {
        use crate::protocols::text_input_unstable_v3::{
            zwp_text_input_manager_v3, zwp_text_input_v3,
        };
        use crate::protocols::wayland::wl_keyboard;
        use crate::state::HostId;

        const SIBLING_TEXT_INPUT: u32 = 42;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        arm_raw_backspace_repeat(&mut ctx, &mut handler, HOST_TEXT_INPUT, GUEST_TEXT_INPUT);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );

        let mut create = MessageBuilder::new();
        create.write_u32(SIBLING_TEXT_INPUT);
        create.write_u32(GUEST_SEAT);
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_manager_v3",
            create.build_message(
                GUEST_TEXT_INPUT_MANAGER,
                zwp_text_input_manager_v3::REQ_GET_TEXT_INPUT,
            ),
        )
        .is_none());
        for request in [zwp_text_input_v3::REQ_ENABLE, zwp_text_input_v3::REQ_COMMIT] {
            assert!(dispatch_raw_request_result(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v3",
                raw_text_input_request_for(SIBLING_TEXT_INPUT, request),
            )
            .is_none());
        }
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request_for(SIBLING_TEXT_INPUT, zwp_text_input_v3::REQ_DESTROY),
        )
        .is_none());

        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            Some(GUEST_TEXT_INPUT),
            "a rejected enable and destroy on a sibling must not cancel the active lease"
        );

        ctx.host_to_client_queue.clear();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(sender(&ctx.host_to_client_queue[0]), GUEST_KEYBOARD);
        assert_eq!(opcode(&ctx.host_to_client_queue[0]), wl_keyboard::EVT_KEY);
        assert_eq!(sender(&ctx.host_to_client_queue[1]), GUEST_KEYBOARD);
        assert_eq!(opcode(&ctx.host_to_client_queue[1]), wl_keyboard::EVT_KEY);
        assert_eq!(sender(&ctx.host_to_client_queue[2]), GUEST_TEXT_INPUT);
        assert_eq!(
            opcode(&ctx.host_to_client_queue[2]),
            zwp_text_input_v3::EVT_DONE
        );
    }

    #[test]
    fn raw_destroy_recreate_does_not_inherit_repeat_owner() {
        use crate::protocols::text_input_unstable_v3::{
            zwp_text_input_manager_v3, zwp_text_input_v3,
        };
        use crate::state::HostId;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        arm_raw_backspace_repeat(&mut ctx, &mut handler, HOST_TEXT_INPUT, GUEST_TEXT_INPUT);

        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_DESTROY),
        )
        .is_none());
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            None,
            "destroy must retire the lease before the guest ID becomes reusable"
        );

        let mut recreate = MessageBuilder::new();
        recreate.write_u32(GUEST_TEXT_INPUT);
        recreate.write_u32(GUEST_SEAT);
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_manager_v3",
            recreate.build_message(
                GUEST_TEXT_INPUT_MANAGER,
                zwp_text_input_manager_v3::REQ_GET_TEXT_INPUT,
            ),
        )
        .is_none());
        let replacement_host_id = ctx.text_inputs[&GUEST_TEXT_INPUT].host_v1_id;
        assert_ne!(replacement_host_id, HOST_TEXT_INPUT);
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            None,
            "the replacement object must not inherit the old numeric ID's lease"
        );

        for request in [zwp_text_input_v3::REQ_ENABLE, zwp_text_input_v3::REQ_COMMIT] {
            assert!(dispatch_raw_request_result(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v3",
                raw_text_input_request(request),
            )
            .is_none());
        }

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            raw_peek_key(701, 1_300, KEY_BACKSPACE, KEY_RELEASED),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            raw_peek_key(702, 1_400, KEY_BACKSPACE, KEY_PRESSED),
        );
        for text in ["나", ""] {
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v1",
                raw_preedit(replacement_host_id, 1, text),
            );
        }
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(HOST_KEYBOARD), KEY_BACKSPACE),
            Some(GUEST_TEXT_INPUT),
            "a fresh physical generation may establish a new replacement-object lease"
        );
    }

    #[test]
    fn raw_proxy_dispatch_repeats_held_space_after_korean_commit() {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
        use crate::protocols::wayland::wl_keyboard;
        use crate::state::HostId;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();

        let mut peek_press = MessageBuilder::new();
        peek_press.write_u32(700);
        peek_press.write_u32(1_234);
        peek_press.write_u32(KEY_SPACE);
        peek_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_press.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );

        let mut preedit = MessageBuilder::new();
        preedit.write_u32(1);
        preedit.write_string("가");
        preedit.write_string("");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            preedit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_PREEDIT_STRING),
        );

        let mut commit = MessageBuilder::new();
        commit.write_u32(1);
        commit.write_string("가 ");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            commit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_COMMIT_STRING),
        );

        // Exercise the real overlap that previously stopped held-Space after
        // its first Korean commit: the text-input channel emits a balanced
        // pair before the matching wl_keyboard press arrives.
        for state in [KEY_PRESSED, KEY_RELEASED] {
            let mut keysym = MessageBuilder::new();
            keysym.write_u32(if state == KEY_PRESSED { 1_234 } else { 1_235 });
            keysym.write_u32(if state == KEY_PRESSED { 700 } else { 701 });
            keysym.write_u32(xkbcommon::xkb::keysyms::KEY_space);
            keysym.write_u32(state);
            keysym.write_u32(0);
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v1",
                keysym.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_KEYSYM),
            );
        }
        let mut delayed_key_press = MessageBuilder::new();
        delayed_key_press.write_u32(700);
        delayed_key_press.write_u32(1_234);
        delayed_key_press.write_u32(KEY_SPACE);
        delayed_key_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            delayed_key_press.build_message(HOST_KEYBOARD, wl_keyboard::EVT_KEY),
        );

        let initial_commits: Vec<_> = ctx
            .host_to_client_queue
            .iter()
            .filter(|message| {
                sender(message) == GUEST_TEXT_INPUT
                    && opcode(message) == zwp_text_input_v3::EVT_COMMIT_STRING
            })
            .collect();
        assert_eq!(
            initial_commits.len(),
            1,
            "the initial Space must commit exactly one string"
        );
        let mut initial_commit = WireMessage::new(
            GUEST_TEXT_INPUT,
            zwp_text_input_v3::EVT_COMMIT_STRING,
            &initial_commits[0].0[8..],
            &[],
        );
        assert_eq!(initial_commit.read_string().unwrap(), "가 ");
        assert!(initial_commit.is_payload_consumed());
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .filter(|message| {
                    sender(message) == GUEST_KEYBOARD && opcode(message) == wl_keyboard::EVT_KEY
                })
                .count(),
            2,
            "delayed physical delivery must not duplicate the balanced keysym pair"
        );

        ctx.host_to_client_queue.clear();
        for _ in 0..3 {
            let mut confirm = MessageBuilder::new();
            confirm.write_u32(1);
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zcr_extended_text_input_v1",
                confirm.build_message(
                    HOST_EXTENDED_TEXT_INPUT,
                    zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
                ),
            );
        }

        assert_eq!(
            ctx.host_to_client_queue.len(),
            9,
            "each repeat confirmation must emit press, release, and done"
        );
        let (transactions, remainder) = ctx.host_to_client_queue.as_chunks::<3>();
        assert!(
            remainder.is_empty(),
            "synthetic key transactions must have three messages"
        );
        for transaction in transactions {
            for (message, expected_state) in
                transaction[..2].iter().zip([KEY_PRESSED, KEY_RELEASED])
            {
                assert_eq!(sender(message), GUEST_KEYBOARD);
                assert_eq!(opcode(message), wl_keyboard::EVT_KEY);
                let mut key =
                    WireMessage::new(GUEST_KEYBOARD, wl_keyboard::EVT_KEY, &message.0[8..], &[]);
                key.read_u32().unwrap();
                assert_eq!(key.read_u32().unwrap(), 1_234);
                assert_eq!(key.read_u32().unwrap(), KEY_SPACE);
                assert_eq!(key.read_u32().unwrap(), expected_state);
                assert!(key.is_payload_consumed());
            }
            assert_eq!(sender(&transaction[2]), GUEST_TEXT_INPUT);
            assert_eq!(opcode(&transaction[2]), zwp_text_input_v3::EVT_DONE);
        }

        ctx.host_to_client_queue.clear();
        let mut peek_release = MessageBuilder::new();
        peek_release.write_u32(701);
        peek_release.write_u32(1_300);
        peek_release.write_u32(KEY_SPACE);
        peek_release.write_u32(KEY_RELEASED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_release.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );

        let mut confirm_after_release = MessageBuilder::new();
        confirm_after_release.write_u32(1);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            confirm_after_release.build_message(
                HOST_EXTENDED_TEXT_INPUT,
                zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
            ),
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "release and a later confirmation must not emit guest input"
        );

        // A second physical hold must start a fresh generation. This catches
        // tombstones or repeat-cancellation state leaking across release,
        // which otherwise produces the reported first-hold/second-hold
        // asymmetry.
        let mut second_peek_press = MessageBuilder::new();
        second_peek_press.write_u32(702);
        second_peek_press.write_u32(1_400);
        second_peek_press.write_u32(KEY_SPACE);
        second_peek_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            second_peek_press.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );

        let mut second_preedit = MessageBuilder::new();
        second_preedit.write_u32(2);
        second_preedit.write_string("나");
        second_preedit.write_string("");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            second_preedit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_PREEDIT_STRING),
        );
        let mut second_commit = MessageBuilder::new();
        second_commit.write_u32(2);
        second_commit.write_string("나 ");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            second_commit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_COMMIT_STRING),
        );
        ctx.host_to_client_queue.clear();

        for _ in 0..2 {
            let mut confirm = MessageBuilder::new();
            confirm.write_u32(1);
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zcr_extended_text_input_v1",
                confirm.build_message(
                    HOST_EXTENDED_TEXT_INPUT,
                    zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
                ),
            );
        }
        assert_eq!(
            ctx.host_to_client_queue.len(),
            6,
            "the second hold must repeat with the same balanced transactions"
        );

        ctx.host_to_client_queue.clear();
        let mut second_peek_release = MessageBuilder::new();
        second_peek_release.write_u32(703);
        second_peek_release.write_u32(1_500);
        second_peek_release.write_u32(KEY_SPACE);
        second_peek_release.write_u32(KEY_RELEASED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            second_peek_release.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );
        assert!(!ctx
            .key_generations
            .physically_held(HostId(HOST_KEYBOARD), KEY_SPACE));
        assert!(ctx
            .key_generations
            .peek(HostId(HOST_KEYBOARD), KEY_SPACE)
            .is_none());

        // Replay the Backspace variant through raw protocol dispatch as well.
        // This covers Korean preedit clearing, delayed duplicate channels,
        // release cleanup, and a fresh second hold.
        let mut backspace_press = MessageBuilder::new();
        backspace_press.write_u32(800);
        backspace_press.write_u32(1_600);
        backspace_press.write_u32(KEY_BACKSPACE);
        backspace_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            backspace_press.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );
        for preedit_text in ["가", ""] {
            let mut backspace_preedit = MessageBuilder::new();
            backspace_preedit.write_u32(3);
            backspace_preedit.write_string(preedit_text);
            backspace_preedit.write_string("");
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v1",
                backspace_preedit
                    .build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_PREEDIT_STRING),
            );
        }
        ctx.host_to_client_queue.clear();
        for _ in 0..2 {
            let mut confirm = MessageBuilder::new();
            confirm.write_u32(1);
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zcr_extended_text_input_v1",
                confirm.build_message(
                    HOST_EXTENDED_TEXT_INPUT,
                    zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
                ),
            );
        }
        assert_eq!(
            ctx.host_to_client_queue.len(),
            6,
            "held Backspace must emit two balanced repeat transactions"
        );

        for state in [KEY_PRESSED, KEY_RELEASED] {
            let mut keysym = MessageBuilder::new();
            keysym.write_u32(if state == KEY_PRESSED { 1_600 } else { 1_601 });
            keysym.write_u32(if state == KEY_PRESSED { 800 } else { 801 });
            keysym.write_u32(xkbcommon::xkb::keysyms::KEY_BackSpace);
            keysym.write_u32(state);
            keysym.write_u32(0);
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v1",
                keysym.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_KEYSYM),
            );
        }
        let mut delayed_backspace_press = MessageBuilder::new();
        delayed_backspace_press.write_u32(800);
        delayed_backspace_press.write_u32(1_600);
        delayed_backspace_press.write_u32(KEY_BACKSPACE);
        delayed_backspace_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            delayed_backspace_press.build_message(HOST_KEYBOARD, wl_keyboard::EVT_KEY),
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            6,
            "delayed Backspace channels must not duplicate recovered pairs"
        );

        ctx.host_to_client_queue.clear();
        for interface in ["zcr_extended_keyboard_v1", "wl_keyboard"] {
            let mut release = MessageBuilder::new();
            release.write_u32(801);
            release.write_u32(1_700);
            release.write_u32(KEY_BACKSPACE);
            release.write_u32(KEY_RELEASED);
            let (sender_id, event) = if interface == "zcr_extended_keyboard_v1" {
                (
                    HOST_EXTENDED_KEYBOARD,
                    zcr_extended_keyboard_v1::EVT_PEEK_KEY,
                )
            } else {
                (HOST_KEYBOARD, wl_keyboard::EVT_KEY)
            };
            dispatch_raw_event(
                &mut handler,
                &mut ctx,
                interface,
                release.build_message(sender_id, event),
            );
        }
        let mut post_release_confirm = MessageBuilder::new();
        post_release_confirm.write_u32(1);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            post_release_confirm.build_message(
                HOST_EXTENDED_TEXT_INPUT,
                zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
            ),
        );
        assert!(ctx.host_to_client_queue.is_empty());

        let mut second_backspace_press = MessageBuilder::new();
        second_backspace_press.write_u32(810);
        second_backspace_press.write_u32(1_800);
        second_backspace_press.write_u32(KEY_BACKSPACE);
        second_backspace_press.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            second_backspace_press.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );
        let mut second_backspace_confirm = MessageBuilder::new();
        second_backspace_confirm.write_u32(1);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            second_backspace_confirm.build_message(
                HOST_EXTENDED_TEXT_INPUT,
                zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
            ),
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            3,
            "the second Backspace hold must start a fresh repeat generation"
        );
    }

    #[test]
    fn raw_proxy_dispatch_retires_destroyed_text_input_activation_barrier() {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
        use crate::protocols::wayland::{wl_callback, wl_display};

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        {
            let state = ctx.text_inputs.get_mut(&GUEST_TEXT_INPUT).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        crate::handler::text_input::update_host_activation(&mut ctx, GUEST_TEXT_INPUT);
        let callback_id = ctx.text_inputs[&GUEST_TEXT_INPUT]
            .draining_callback()
            .expect("deactivation must install a callback barrier");
        let initial_guest_events = ctx.host_to_client_queue.len();

        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            MessageBuilder::new().build_message(GUEST_TEXT_INPUT, zwp_text_input_v3::REQ_DESTROY),
        )
        .is_none());
        assert!(!ctx.text_inputs.contains_key(&GUEST_TEXT_INPUT));
        assert_eq!(
            ctx.host_to_client_queue.len(),
            initial_guest_events + 1,
            "destroy must only acknowledge the guest text-input object"
        );

        let mut stale_preedit = MessageBuilder::new();
        stale_preedit.write_u32(1);
        stale_preedit.write_string("stale");
        stale_preedit.write_string("");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            stale_preedit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_PREEDIT_STRING),
        );
        let mut stale_commit = MessageBuilder::new();
        stale_commit.write_u32(1);
        stale_commit.write_string("stale");
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            stale_commit.build_message(HOST_TEXT_INPUT, zwp_text_input_v1::EVT_COMMIT_STRING),
        );
        assert_eq!(ctx.host_to_client_queue.len(), initial_guest_events + 1);

        let mut done = MessageBuilder::new();
        done.write_u32(0);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_callback",
            done.build_message(callback_id.0, wl_callback::EVT_DONE),
        );
        assert!(
            ctx.internal_callbacks.get(callback_id).is_none(),
            "callback.done must consume the activation barrier"
        );
        assert!(ctx.shadow_table.is_pending_destroy_host_only(callback_id.0));
        assert_eq!(
            ctx.host_to_client_queue.len(),
            initial_guest_events + 1,
            "a host-only callback must not emit guest done/delete_id events"
        );

        let mut delete_id = MessageBuilder::new();
        delete_id.write_u32(callback_id.0);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_display",
            delete_id.build_message(1, wl_display::EVT_DELETE_ID),
        );
        assert!(!ctx.shadow_table.is_pending_destroy_host_only(callback_id.0));
        assert!(ctx.shadow_table.is_host_id_available(callback_id.0));
        assert_eq!(
            ctx.host_to_client_queue.len(),
            initial_guest_events + 1,
            "internal callback delete_id must not be forwarded to the guest"
        );
        assert!(
            ctx.host_to_client_queue.iter().all(|message| {
                sender(message) != 1
                    || opcode(message) != wl_display::EVT_DELETE_ID
                    || u32::from_ne_bytes(message.0[8..12].try_into().unwrap()) != callback_id.0
            }),
            "internal callback teardown must never expose its host-only ID"
        );
    }

    #[test]
    fn raw_ime_trace_drops_old_korean_composition_and_repeat_across_focus_barrier() {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;

        const NEXT_GUEST_SURFACE: u32 = 910;
        const NEXT_HOST_SURFACE: u32 = 911;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        register_raw_surface(&mut ctx, NEXT_GUEST_SURFACE, NEXT_HOST_SURFACE);

        let mut held_space = MessageBuilder::new();
        held_space.write_u32(700);
        held_space.write_u32(1_000);
        held_space.write_u32(KEY_SPACE);
        held_space.write_u32(KEY_PRESSED);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            held_space.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            ),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_preedit(HOST_TEXT_INPUT, 1, "가"),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_commit_string(HOST_TEXT_INPUT, 1, "가 "),
        );
        ctx.host_to_client_queue.clear();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            3,
            "the old focused generation must repeat before the focus boundary"
        );

        ctx.host_to_client_queue.clear();
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_leave(701, HOST_SURFACE),
        )
        .is_some());
        let callback_id = ctx.text_inputs[&GUEST_TEXT_INPUT]
            .draining_callback()
            .expect("focus loss must install a deactivation barrier");
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_enter(702, NEXT_HOST_SURFACE),
        )
        .is_some());
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_ENABLE),
        )
        .is_none());
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_COMMIT),
        )
        .is_none());
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());

        ctx.host_to_client_queue.clear();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_preedit(HOST_TEXT_INPUT, 1, "오래된 조합"),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_commit_string(HOST_TEXT_INPUT, 1, "오래된 확정"),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            raw_confirm_preedit(),
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "old composition and held-key recovery must stay behind the focus barrier"
        );

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_callback",
            raw_callback_done(callback_id.0),
        );
        assert!(
            ctx.internal_callbacks.get(callback_id).is_none(),
            "the old callback must be consumed even after guest ID reuse"
        );
        assert!(ctx.shadow_table.is_pending_destroy_host_only(callback_id.0));
        assert!(ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_preedit(HOST_TEXT_INPUT, 2, "새 조합"),
        );
        assert!(
            ctx.host_to_client_queue.iter().any(|message| {
                sender(message) == GUEST_TEXT_INPUT
                    && opcode(message) == zwp_text_input_v3::EVT_PREEDIT_STRING
            }),
            "the new focused generation must accept composition after the barrier"
        );
        assert!(ctx.host_to_client_queue.iter().all(|message| {
            sender(message) != GUEST_TEXT_INPUT
                || opcode(message) != zwp_text_input_v3::EVT_COMMIT_STRING
        }));
    }

    #[test]
    fn raw_ime_trace_destroy_recreate_ignores_old_callback_generation() {
        use crate::protocols::text_input_unstable_v3::{
            zwp_text_input_manager_v3, zwp_text_input_v3,
        };

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        {
            let state = ctx.text_inputs.get_mut(&GUEST_TEXT_INPUT).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        crate::handler::text_input::update_host_activation(&mut ctx, GUEST_TEXT_INPUT);
        let callback_id = ctx.text_inputs[&GUEST_TEXT_INPUT]
            .draining_callback()
            .expect("deactivation must install the old generation callback");

        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_DESTROY),
        )
        .is_none());
        assert!(!ctx.text_inputs.contains_key(&GUEST_TEXT_INPUT));

        let mut recreate = MessageBuilder::new();
        recreate.write_u32(GUEST_TEXT_INPUT);
        recreate.write_u32(GUEST_SEAT);
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_manager_v3",
            recreate.build_message(
                GUEST_TEXT_INPUT_MANAGER,
                zwp_text_input_manager_v3::REQ_GET_TEXT_INPUT,
            ),
        )
        .is_none());
        let replacement_host_id = ctx.text_inputs[&GUEST_TEXT_INPUT].host_v1_id;
        assert_ne!(replacement_host_id, HOST_TEXT_INPUT);

        for request in [zwp_text_input_v3::REQ_ENABLE, zwp_text_input_v3::REQ_COMMIT] {
            assert!(dispatch_raw_request_result(
                &mut handler,
                &mut ctx,
                "zwp_text_input_v3",
                raw_text_input_request(request),
            )
            .is_none());
        }
        assert!(ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());

        ctx.client_to_host_queue.clear();
        ctx.host_to_client_queue.clear();
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_callback",
            raw_callback_done(callback_id.0),
        );
        assert!(ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());
        assert_eq!(
            ctx.text_inputs[&GUEST_TEXT_INPUT].host_v1_id,
            replacement_host_id
        );
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "the old callback must not reconcile or reactivate the replacement object"
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "the host-only old callback must not leak into the replacement guest lifecycle"
        );

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v1",
            raw_preedit(replacement_host_id, 2, "새 객체"),
        );
        assert!(
            !ctx.host_to_client_queue.is_empty(),
            "the replacement host generation must remain live after the stale callback"
        );
    }

    #[test]
    fn raw_ime_trace_uncommitted_enable_disable_never_crosses_focus_generation() {
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;

        const SECOND_GUEST_SURFACE: u32 = 910;
        const SECOND_HOST_SURFACE: u32 = 911;
        const THIRD_GUEST_SURFACE: u32 = 920;
        const THIRD_HOST_SURFACE: u32 = 921;

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        register_raw_surface(&mut ctx, SECOND_GUEST_SURFACE, SECOND_HOST_SURFACE);
        register_raw_surface(&mut ctx, THIRD_GUEST_SURFACE, THIRD_HOST_SURFACE);

        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_DISABLE),
        )
        .is_none());
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].pending_enabled);
        assert!(ctx.text_inputs[&GUEST_TEXT_INPUT].committed_enabled);

        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_leave(800, HOST_SURFACE),
        )
        .is_some());
        let callback_id = ctx.text_inputs[&GUEST_TEXT_INPUT]
            .draining_callback()
            .expect("focus loss must deactivate the committed old generation");
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_enter(801, SECOND_HOST_SURFACE),
        )
        .is_some());
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "wl_callback",
            raw_callback_done(callback_id.0),
        );
        assert!(
            ctx.internal_callbacks.get(callback_id).is_none(),
            "callback.done must consume the old focus barrier"
        );
        assert!(ctx.shadow_table.is_pending_destroy_host_only(callback_id.0));
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());

        ctx.client_to_host_queue.clear();
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_COMMIT),
        )
        .is_none());
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|message| sender(message) != HOST_TEXT_INPUT || opcode(message) != 0),
            "the uncommitted disable from the old focus must not become a new activation"
        );

        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_ENABLE),
        )
        .is_none());
        assert!(ctx.text_inputs[&GUEST_TEXT_INPUT].pending_enabled);
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].committed_enabled);
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_leave(802, SECOND_HOST_SURFACE),
        )
        .is_some());
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            raw_keyboard_enter(803, THIRD_HOST_SURFACE),
        )
        .is_some());
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].pending_enabled);

        ctx.client_to_host_queue.clear();
        assert!(dispatch_raw_request_result(
            &mut handler,
            &mut ctx,
            "zwp_text_input_v3",
            raw_text_input_request(zwp_text_input_v3::REQ_COMMIT),
        )
        .is_none());
        assert!(!ctx.text_inputs[&GUEST_TEXT_INPUT].host_is_active());
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|message| sender(message) != HOST_TEXT_INPUT || opcode(message) != 0),
            "the uncommitted enable from the previous focus must not cross generations"
        );
    }

    #[test]
    fn raw_proxy_dispatch_keeps_new_generation_after_delayed_release_and_focus_loss() {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
        use crate::protocols::wayland::wl_keyboard;
        use crate::state::{GuestKeyOwner, HostId};

        fn peek_key(serial: u32, time: u32, state: u32) -> Vec<u8> {
            let mut event = MessageBuilder::new();
            event.write_u32(serial);
            event.write_u32(time);
            event.write_u32(KEY_SPACE);
            event.write_u32(state);
            event.build_message(
                HOST_EXTENDED_KEYBOARD,
                zcr_extended_keyboard_v1::EVT_PEEK_KEY,
            )
        }

        fn keyboard_key(serial: u32, time: u32, state: u32) -> Vec<u8> {
            let mut event = MessageBuilder::new();
            event.write_u32(serial);
            event.write_u32(time);
            event.write_u32(KEY_SPACE);
            event.write_u32(state);
            event.build_message(HOST_KEYBOARD, wl_keyboard::EVT_KEY)
        }

        let (mut ctx, mut handler) = setup_raw_ime_dispatch();
        let old_press = u32::MAX - 2;
        let old_release = u32::MAX;
        let new_press = 1;
        let new_release = 3;

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_key(old_press, 100, KEY_PRESSED),
        );
        assert!(
            dispatch_raw_event_result(
                &mut handler,
                &mut ctx,
                "wl_keyboard",
                keyboard_key(old_press, 100, KEY_PRESSED),
            )
            .is_some(),
            "the first physical generation must reach the guest"
        );

        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_key(old_release, 110, KEY_RELEASED),
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_key(new_press, 200, KEY_PRESSED),
        );
        assert!(
            dispatch_raw_event_result(
                &mut handler,
                &mut ctx,
                "wl_keyboard",
                keyboard_key(new_press, 200, KEY_PRESSED),
            )
            .is_some(),
            "the wrapped serial must start a new physical generation"
        );

        assert!(
            dispatch_raw_event_result(
                &mut handler,
                &mut ctx,
                "wl_keyboard",
                keyboard_key(old_release, 110, KEY_RELEASED),
            )
            .is_some(),
            "the delayed release must still balance its retired guest press"
        );
        assert!(ctx
            .key_generations
            .physically_held(HostId(HOST_KEYBOARD), KEY_SPACE));
        assert_eq!(
            ctx.guest_key_owner(HostId(HOST_KEYBOARD), KEY_SPACE),
            Some(GuestKeyOwner::Physical),
            "the old release must not clear the wrapped current generation"
        );

        assert!(
            dispatch_raw_event_result(
                &mut handler,
                &mut ctx,
                "wl_keyboard",
                keyboard_key(2, 210, KEY_REPEATED),
            )
            .is_some(),
            "repeat forwarding requires the current generation to remain owned"
        );
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_key(new_release, 220, KEY_RELEASED),
        );
        assert!(
            dispatch_raw_event_result(
                &mut handler,
                &mut ctx,
                "wl_keyboard",
                keyboard_key(new_release, 220, KEY_RELEASED),
            )
            .is_some(),
            "the current generation must retain its own balancing release"
        );
        assert!(!ctx
            .key_generations
            .physically_held(HostId(HOST_KEYBOARD), KEY_SPACE));
        assert!(ctx
            .guest_key_owner(HostId(HOST_KEYBOARD), KEY_SPACE)
            .is_none());

        let acknowledgements = ctx
            .client_to_host_queue
            .iter()
            .filter(|message| sender(message) == HOST_EXTENDED_KEYBOARD && opcode(message) == 1)
            .map(|message| {
                (
                    u32::from_ne_bytes(message.0[8..12].try_into().unwrap()),
                    u32::from_ne_bytes(message.0[12..16].try_into().unwrap()) != 0,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            acknowledgements,
            vec![
                (old_press, true),
                (new_press, true),
                (old_release, true),
                (2, true),
                (new_release, true),
            ],
            "every physical event must receive exactly one policy-consistent ACK"
        );

        // A held key that loses keyboard focus cannot become a later synthetic
        // IME recovery pair.
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_keyboard_v1",
            peek_key(4, 300, KEY_PRESSED),
        );
        let mut leave = MessageBuilder::new();
        leave.write_u32(5);
        leave.write_u32(HOST_SURFACE);
        assert!(dispatch_raw_event_result(
            &mut handler,
            &mut ctx,
            "wl_keyboard",
            leave.build_message(HOST_KEYBOARD, wl_keyboard::EVT_LEAVE),
        )
        .is_some());
        ctx.host_to_client_queue.clear();

        let mut confirm = MessageBuilder::new();
        confirm.write_u32(1);
        dispatch_raw_event(
            &mut handler,
            &mut ctx,
            "zcr_extended_text_input_v1",
            confirm.build_message(
                HOST_EXTENDED_TEXT_INPUT,
                zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT,
            ),
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "focus loss must prevent a later IME confirmation from recovering the held key"
        );
    }

    #[test]
    fn synthetic_manager_destroy_emits_one_guest_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_id = 20;
        ctx.shadow_table.track_interface_with_version(
            guest_id,
            "zwp_text_input_manager_v3".to_string(),
            1,
        );
        let mut handler = SommelierHandler::new();

        let mut destroy = WireMessage::new(
            guest_id,
            protocols::text_input_unstable_v3::zwp_text_input_manager_v3::REQ_DESTROY,
            &[],
            &[],
        );
        assert_eq!(
            protocols::text_input_unstable_v3::zwp_text_input_manager_v3::dispatch_request(
                &mut destroy,
                &mut handler,
                &mut ctx,
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
            &[],
        );
        assert_eq!(
            protocols::text_input_unstable_v3::zwp_text_input_manager_v3::dispatch_request(
                &mut duplicate_destroy,
                &mut handler,
                &mut ctx,
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
        ctx.shadow_table
            .track_interface_with_version(guest_id, "wl_shm_pool".to_string(), 1);
        let mut handler = SommelierHandler::new();

        let mut destroy = WireMessage::new(
            guest_id,
            protocols::wayland::wl_shm_pool::REQ_DESTROY,
            &[],
            &[],
        );
        assert_eq!(
            protocols::wayland::wl_shm_pool::dispatch_request(&mut destroy, &mut handler, &mut ctx,),
            Ok(None)
        );
        assert_eq!(ctx.shadow_table.get_interface(guest_id), None);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap()),
            guest_id
        );
    }

    #[test]
    fn early_output_cleanup_preserves_incoming_fd_ownership() {
        // A memfd has a unique inode for this ownership assertion; pipe
        // read/write ends share an inode and eventfds share an anonymous-inode
        // inode on Linux.
        let incoming_name = CString::new("sommelier-proxy-incoming-fd-test").unwrap();
        let incoming_fd = memfd_create(incoming_name.as_c_str(), MFdFlags::MFD_CLOEXEC)
            .expect("memfd_create")
            .into_raw_fd();
        let incoming_identity = fd_identity(incoming_fd);
        let mut output = OutgoingMessages::default();
        output.push(&[], &[incoming_fd]);
        close_pending_output_fds(&mut output, &[incoming_fd]);

        // The incoming connection still owns this descriptor; the cleanup
        // helper must not close it before the connection drains/drops.
        assert!(
            any_fd_has_identity(incoming_identity),
            "protected incoming descriptor must remain open"
        );
        assert_eq!(unsafe { libc::close(incoming_fd) }, 0);

        let output_name = CString::new("sommelier-proxy-output-fd-test").unwrap();
        let fd = memfd_create(output_name.as_c_str(), MFdFlags::MFD_CLOEXEC)
            .expect("memfd_create")
            .into_raw_fd();
        let output_identity = fd_identity(fd);
        let mut output = OutgoingMessages::default();
        output.push(&[], &[fd]);
        close_pending_output_fds(&mut output, &[]);
        assert!(
            !any_fd_has_identity(output_identity),
            "unprotected queued descriptor must be closed"
        );
    }

    #[test]
    fn translated_sender_survives_destructor_mapping_removal() {
        let mut shadow_table = ShadowTable::new();
        shadow_table.map_id(47, 68);

        let client_to_host = translated_sender_id(&shadow_table, Direction::ClientToHost, 47);
        let host_to_client = translated_sender_id(&shadow_table, Direction::HostToClient, 68);
        shadow_table.remove_id(47);

        assert_eq!(client_to_host, Some(68));
        assert_eq!(host_to_client, Some(47));
        assert_eq!(shadow_table.get_host_id(47), None);
        assert_eq!(shadow_table.get_guest_id(68), None);
    }

    #[test]
    fn destructor_mapping_survives_until_host_delete_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface_with_version(1, "wl_display".to_string(), 1);
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_region".to_string(), 1);
        let mut handler = SommelierHandler::new();

        let mut destroy =
            WireMessage::new(10, protocols::wayland::wl_region::REQ_DESTROY, &[], &[]);
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
        let mut delete_id = WireMessage::new(
            1,
            protocols::wayland::wl_display::EVT_DELETE_ID,
            &payload,
            &[],
        );
        assert_eq!(
            protocols::wayland::dispatch_event(
                "wl_display",
                &mut delete_id,
                &mut handler,
                &mut ctx
            ),
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
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface_with_version(1, "wl_display".to_string(), 1);
        ctx.shadow_table.map_id(5, 15);
        ctx.shadow_table
            .track_interface_with_version(5, "wl_fixes".to_string(), 1);
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_registry".to_string(), 1);
        let mut handler = SommelierHandler::new();

        let payload = 10u32.to_ne_bytes();
        let mut destroy_registry = WireMessage::new(
            5,
            protocols::wayland::wl_fixes::REQ_DESTROY_REGISTRY,
            &payload,
            &[],
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
        let mut delete_id = WireMessage::new(
            1,
            protocols::wayland::wl_display::EVT_DELETE_ID,
            &payload,
            &[],
        );
        assert_eq!(
            protocols::wayland::dispatch_event(
                "wl_display",
                &mut delete_id,
                &mut handler,
                &mut ctx
            ),
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
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_region".to_string(), 1);
        ctx.shadow_table.mark_pending_destroy(10);
        assert!(pending_host_event_is_stale(&ctx, 20, 0, Some(10)));

        ctx.shadow_table.map_id(11, 21);
        ctx.shadow_table
            .track_interface_with_version(11, "wl_buffer".to_string(), 1);
        assert!(ctx.register_local_buffer(11, 21, test_buffer_state()));
        assert!(ctx.mark_buffer_guest_destroyed(11));
        ctx.shadow_table.retire_guest_object(11);
        assert!(!pending_host_event_is_stale(
            &ctx,
            21,
            protocols::wayland::wl_buffer::EVT_RELEASE,
            Some(11)
        ));

        ctx.shadow_table.map_id(12, 22);
        ctx.shadow_table
            .track_interface_with_version(12, "wl_buffer".to_string(), 1);
        assert!(ctx.register_native_buffer(22, (1, 1), Vec::new()));
        // Match the production native-buffer destroy path: the guest
        // interface is retired while the host-side interface remains
        // available long enough to dispatch wl_buffer.release.
        assert!(ctx.mark_buffer_guest_destroyed(12));
        ctx.shadow_table.retire_guest_object(12);
        assert!(!pending_host_event_is_stale(
            &ctx,
            22,
            protocols::wayland::wl_buffer::EVT_RELEASE,
            Some(12)
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
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_output".to_string(), 3);
        ctx.last_sender_id = 10;
        WlOutputHandler::on_release(&mut crate::handler::output::OutputHandler, &mut ctx);
        for opcode in [EVT_GEOMETRY, EVT_MODE, EVT_DONE, EVT_SCALE] {
            assert!(
                pending_host_event_is_stale(&ctx, 20, opcode, Some(10)),
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
            bo: None,
            dmabuf_fd: None,
            bo_stride: 4,
            dmabuf_plane1_offset: 0,
            dmabuf_plane1_stride: 0,
            dmabuf_sync: false,
            dest_ptr: std::ptr::null_mut(),
            dest_size: 0,
            stale: StalePixels::all(),
        }
    }

    #[test]
    fn untracked_host_event_is_dropped_before_handler_state_mutation() {
        struct Probe {
            called: bool,
        }

        impl protocols::wayland::wl_registry::WlRegistryHandler for Probe {
            fn on_global(
                &mut self,
                _ctx: &mut Context,
                _name: u32,
                _interface: &String,
                _version: u32,
            ) -> crate::wire::Action {
                self.called = true;
                crate::wire::Action::Drop
            }
        }

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut builder = MessageBuilder::new();
        builder.write_u32(7);
        builder.write_string("wl_seat");
        builder.write_u32(1);
        let payload = builder.build_message(77, protocols::wayland::wl_registry::EVT_GLOBAL);
        let mut msg = WireMessage::new(
            77,
            protocols::wayland::wl_registry::EVT_GLOBAL,
            &payload[8..],
            &[],
        );
        let mut probe = Probe { called: false };

        let result =
            protocols::wayland::wl_registry::dispatch_event(&mut msg, &mut probe, &mut ctx);

        assert_eq!(result, Ok(None));
        assert!(
            !probe.called,
            "an untracked host event must not reach a stateful handler"
        );
        assert!(ctx.host_globals.is_empty());
    }

    #[test]
    fn created_surface_keeps_separate_guest_and_host_versions() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_compositor".to_string(), 4);
        ctx.shadow_table.set_host_version(20, 3);
        let mut handler = SommelierHandler::new();
        let payload = 30u32.to_ne_bytes();
        let mut msg = WireMessage::new(
            10,
            protocols::wayland::wl_compositor::REQ_CREATE_SURFACE,
            &payload,
            &[],
        );
        let (forwarded, fds) =
            protocols::wayland::dispatch_request("wl_compositor", &mut msg, &mut handler, &mut ctx)
                .expect("create_surface dispatch")
                .expect("create_surface forwarded");
        assert!(fds.is_empty());
        assert_eq!(crate::test_support::msg_sender(&forwarded), 20);
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
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "xdg_positioner".to_string(), 1);

        let payload = [];
        let mut msg = WireMessage::new(
            10,
            protocols::xdg_shell::xdg_positioner::REQ_SET_REACTIVE,
            &payload,
            &[],
        );
        let mut handler = SommelierHandler::new();
        let result = protocols::xdg_shell::dispatch_request(
            "xdg_positioner",
            &mut msg,
            &mut handler,
            &mut ctx,
        );

        assert_eq!(
            result,
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

        ctx.shadow_table.map_id(sender, host_sender);
        ctx.shadow_table
            .track_interface_with_version(sender, "wl_subcompositor".to_string(), 1);
        ctx.shadow_table.map_id(surface, host_surface);
        ctx.shadow_table
            .track_interface_with_version(surface, "wl_surface".to_string(), 1);
        ctx.shadow_table.map_id(parent, host_parent);
        ctx.shadow_table
            .track_interface_with_version(parent, "wl_surface".to_string(), 1);
        ctx.shadow_table.mark_pending_destroy(surface);

        let mut payload = Vec::new();
        payload.extend_from_slice(&new_subsurface.to_ne_bytes());
        payload.extend_from_slice(&surface.to_ne_bytes());
        payload.extend_from_slice(&parent.to_ne_bytes());
        let mut msg = WireMessage::new(
            sender,
            protocols::wayland::wl_subcompositor::REQ_GET_SUBSURFACE,
            &payload,
            &[],
        );
        let mut handler = SommelierHandler::new();

        assert_eq!(
            protocols::wayland::wl_subcompositor::dispatch_request(
                &mut msg,
                &mut handler,
                &mut ctx,
            ),
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
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table.set_host_version(20, 3);

        let payload = [0u8; 8];
        let mut msg = WireMessage::new(
            20,
            protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE_BOUNDS,
            &payload,
            &[],
        );
        let mut handler = SommelierHandler::new();
        let result =
            protocols::xdg_shell::dispatch_event("xdg_toplevel", &mut msg, &mut handler, &mut ctx);

        assert_eq!(
            result,
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
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_data_source".to_string(), 1);

        let mut builder = MessageBuilder::new();
        builder.write_string(&"x".repeat(65_520));
        let payload = builder.into_payload();
        let mut msg = WireMessage::new(
            10,
            protocols::wayland::wl_data_source::REQ_OFFER,
            &payload,
            &[],
        );
        let mut handler = SommelierHandler::new();

        assert_eq!(
            protocols::wayland::wl_data_source::dispatch_request(&mut msg, &mut handler, &mut ctx,),
            Err(ProtocolError::MessageTooLarge(65_536)),
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
    // KeyboardHandler owns xkb state, which is intentionally !Send. Keep each
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
/// shortcut configuration on each `reload_signal`.
///
/// A failure concerning one client is logged and ends only that client.
/// Must run inside a `LocalSet`, like the clients it spawns.
async fn serve(
    listener: UnixListener,
    config: ClientConfig,
    reload_signal: Option<tokio::signal::unix::Signal>,
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

/// Reload the shortcut configuration on each `signal`.
///
/// A task of its own, so a reload never waits behind the accept loop (its
/// retry pause, or a client's host connection). It runs on the proxy's one
/// thread between other tasks' steps, so it never races a key event.
/// `PlacementConfig::reload_and_log` owns what a reload means, including
/// the logged no-op when there is no file.
async fn reload_on_signal(
    mut signal: tokio::signal::unix::Signal,
    placement: Arc<PlacementConfig>,
) {
    while signal.recv().await.is_some() {
        placement.reload_and_log();
    }
}
