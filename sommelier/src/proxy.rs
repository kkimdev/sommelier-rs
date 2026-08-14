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

use crate::connection::WaylandConnection;
use crate::protocols;
use crate::state::{Context, ShadowTable};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{ProtocolError, WireMessage};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use std::os::fd::BorrowedFd;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};

type DispatchResult = Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError>;

#[derive(Debug, Clone, Copy)]
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

fn checked_message_end(offset: usize, message_len: usize) -> Option<usize> {
    offset.checked_add(message_len)
}

fn valid_message_length(length: usize) -> bool {
    // Wayland stores the total byte length in the upper 16 bits of the
    // second header word. The largest aligned representable length is 65532.
    (8..=0xffff).contains(&length) && length.is_multiple_of(4)
}

fn close_pending_output_fds(out_fds: &mut Vec<RawFd>, still_owned_fds: &[RawFd]) {
    // FDs decoded from an incoming message remain owned by the connection
    // until the read buffer is drained (or the connection is dropped). A
    // handler can also queue newly-created descriptors that are not present in
    // that list. On an early protocol-error return, close only the latter;
    // closing a descriptor in both collections would let a later fd reuse
    // turn the connection drop into a double-close.
    let protected: std::collections::HashSet<_> = still_owned_fds.iter().copied().collect();
    let mut closed = std::collections::HashSet::new();
    for fd in out_fds.drain(..) {
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
        && guest_id.is_some_and(|gid| ctx.retired_buffers.contains_key(&gid));
    ctx.shadow_table.is_pending_destroy_host(sender_id) && !allows_deferred_buffer_release
}

fn set_nonblocking(fd: RawFd) -> nix::Result<()> {
    // The raw descriptor is owned by the caller for the duration of this
    // function. Preserve all existing status flags while adding O_NONBLOCK;
    // F_SETFL replaces, rather than merges, the flag set.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let current = fcntl(borrowed, FcntlArg::F_GETFL)?;
    let updated = OFlag::from_bits_truncate(current) | OFlag::O_NONBLOCK;
    fcntl(borrowed, FcntlArg::F_SETFL(updated))?;
    Ok(())
}

struct SommelierHandler {
    display: crate::handler::display::DisplayHandler,
    registry: crate::handler::registry::RegistryHandler,
    compositor: crate::handler::compositor::CompositorHandler,
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
    fn new(
        client_conn: WaylandConnection,
        host_conn: WaylandConnection,
        gpu_accel: bool,
        xdg_decoration: bool,
    ) -> Self {
        Self {
            client_conn,
            host_conn,
            ctx: Context::new(gpu_accel, xdg_decoration),
            handler: SommelierHandler::new(),
        }
    }

    async fn run(mut self) {
        self.ctx.shadow_table.map_id(1, 1);
        self.ctx
            .shadow_table
            .track_interface(1, "wl_display".to_string());

        loop {
            tokio::select! {
                res = self.client_conn.recv() => {
                    match res {
                        Ok(bytes) => {
                            if bytes == 0 { break; }
                            if !self.handle_msgs(Direction::ClientToHost).await { break; }
                        }
                        Err(_) => break,
                    }
                }
                res = self.host_conn.recv() => {
                    match res {
                        Ok(bytes) => {
                            if bytes == 0 { break; }
                            if !self.handle_msgs(Direction::HostToClient).await { break; }
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

    fn dispatch_request(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        msg: &mut WireMessage,
    ) -> DispatchResult {
        if protocols::wayland::ALLOWED_INTERFACES.contains(&interface) {
            protocols::wayland::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::xdg_shell::ALLOWED_INTERFACES.contains(&interface) {
            protocols::xdg_shell::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::linux_dmabuf_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::linux_dmabuf_v1::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::viewporter::ALLOWED_INTERFACES.contains(&interface) {
            protocols::viewporter::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::text_input_unstable_v3::ALLOWED_INTERFACES.contains(&interface) {
            protocols::text_input_unstable_v3::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::text_input_unstable_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::text_input_unstable_v1::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::text_input_extension_unstable_v1::ALLOWED_INTERFACES
            .contains(&interface)
        {
            protocols::text_input_extension_unstable_v1::dispatch_request(
                interface, msg, handler, ctx,
            )
        } else if protocols::xdg_decoration_unstable_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::xdg_decoration_unstable_v1::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::fractional_scale_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::fractional_scale_v1::dispatch_request(interface, msg, handler, ctx)
        } else if protocols::keyboard_extension_unstable_v1::ALLOWED_INTERFACES.contains(&interface)
        {
            protocols::keyboard_extension_unstable_v1::dispatch_request(
                interface, msg, handler, ctx,
            )
        } else {
            Ok(None)
        }
    }

    fn dispatch_event(
        handler: &mut SommelierHandler,
        ctx: &mut Context,
        interface: &str,
        msg: &mut WireMessage,
    ) -> DispatchResult {
        if protocols::wayland::ALLOWED_INTERFACES.contains(&interface) {
            protocols::wayland::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::xdg_shell::ALLOWED_INTERFACES.contains(&interface) {
            protocols::xdg_shell::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::linux_dmabuf_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::linux_dmabuf_v1::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::viewporter::ALLOWED_INTERFACES.contains(&interface) {
            protocols::viewporter::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::text_input_unstable_v3::ALLOWED_INTERFACES.contains(&interface) {
            protocols::text_input_unstable_v3::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::text_input_unstable_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::text_input_unstable_v1::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::text_input_extension_unstable_v1::ALLOWED_INTERFACES
            .contains(&interface)
        {
            protocols::text_input_extension_unstable_v1::dispatch_event(
                interface, msg, handler, ctx,
            )
        } else if protocols::xdg_decoration_unstable_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::xdg_decoration_unstable_v1::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::fractional_scale_v1::ALLOWED_INTERFACES.contains(&interface) {
            protocols::fractional_scale_v1::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::keyboard_extension_unstable_v1::ALLOWED_INTERFACES.contains(&interface)
        {
            protocols::keyboard_extension_unstable_v1::dispatch_event(interface, msg, handler, ctx)
        } else if protocols::aura_shell::ALLOWED_INTERFACES.contains(&interface) {
            // Silently drop events for internally-bound aura_shell objects.
            // We only use these interfaces to send requests (set_application_id
            // via zaura_surface), never to receive events.
            Ok(None)
        } else {
            Ok(None)
        }
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
        let mut out_buffer = Vec::new();
        let mut out_fds = Vec::new();
        let mut offset: usize = 0;
        let mut fd_offset: usize = 0;
        // A complete untracked message cannot tell us whether descriptors
        // currently queued on the ordered stream belong to it or to a later
        // message. Retain them while the next message is partial (the bytes
        // may still complete the descriptor-owning message), but once another
        // complete message is available the association is irrecoverably
        // ambiguous and the connection must be torn down.
        while let Some(header_end) = offset.checked_add(8) {
            if header_end > conn.read_buf.len() {
                break;
            }
            let sender_end = header_end - 4;
            let sender_id =
                u32::from_ne_bytes(conn.read_buf[offset..sender_end].try_into().unwrap());
            let word2 =
                u32::from_ne_bytes(conn.read_buf[offset + 4..header_end].try_into().unwrap());
            let len = (word2 >> 16) as usize;
            let opcode = (word2 & 0xFFFF) as u16;

            if !valid_message_length(len) {
                log::error!("Invalid message length: {}", len);
                close_pending_output_fds(&mut out_fds, &conn.read_fds);
                return false;
            }
            let Some(message_end) = checked_message_end(offset, len) else {
                log::error!(
                    "Message end overflows usize: offset={}, length={}",
                    offset,
                    len
                );
                close_pending_output_fds(&mut out_fds, &conn.read_fds);
                return false;
            };
            if message_end > conn.read_buf.len() {
                break;
            }

            if conn.ambiguous_untracked_fd {
                log::error!(
                    "Ambiguous SCM_RIGHTS ordering after a complete untracked message; \
                     terminating connection before assigning descriptors to another message"
                );
                close_pending_output_fds(&mut out_fds, &conn.read_fds);
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

            let interface = match direction {
                Direction::ClientToHost => self.ctx.shadow_table.get_interface(sender_id).cloned(),
                Direction::HostToClient => {
                    let host_interface = self.ctx.shadow_table.get_host_interface(sender_id);
                    let guest_interface =
                        guest_id.and_then(|gid| self.ctx.shadow_table.get_interface(gid));
                    if pending_host_event_is_stale(&self.ctx, sender_id, opcode, guest_id) {
                        // A destructor has already been forwarded for this
                        // object. Ignore host events queued before the
                        // compositor processed it; only wl_display.delete_id
                        // is meaningful while the mapping is pending.
                        None
                    } else {
                        host_interface.or(guest_interface).cloned()
                    }
                }
            };

            let mut consumed_fds = 0;
            let result = if let Some(interface) = interface {
                let mut msg =
                    WireMessage::new(sender_id, opcode, &packet[8..], &conn.read_fds[fd_offset..]);

                log::trace!("[{:?}] {}:{} (len={})", direction, interface, opcode, len);

                self.ctx.last_sender_id = sender_id;

                let res = match direction {
                    Direction::ClientToHost => Self::dispatch_request(
                        &mut self.handler,
                        &mut self.ctx,
                        &interface,
                        &mut msg,
                    ),
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
            } else {
                if guest_id.is_some() {
                    log::warn!(
                        "[{:?}] unknown id {} opcode {} (len={})",
                        direction,
                        sender_id,
                        opcode,
                        len
                    );
                    if matches!(direction, Direction::ClientToHost) {
                        // A guest request is only valid when its sender is a
                        // live object owned by this connection. Unlike a
                        // stale host event (which may have raced object
                        // teardown), silently dropping an unknown client
                        // request makes the client believe the request was
                        // accepted and can desynchronize protocol state.
                        log::error!("Rejecting client request from unknown sender {}", sender_id);
                        close_pending_output_fds(&mut out_fds, &conn.read_fds);
                        return false;
                    }
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

            match result {
                Ok(Some((mut data, fds))) => {
                    log::debug!("  -> translated ({} bytes, {} fds)", data.len(), fds.len());

                    // Patch ID in the forwarded message
                    if let Some(gid) = guest_id {
                        if let Some(tid) = target_sender_id {
                            if data.len() >= 4 {
                                data[0..4].copy_from_slice(&tid.to_ne_bytes());
                            }
                        } else {
                            // If we can't map the ID, it might be a new object or error.
                            // For ClientToHost, it's usually sender which should be mapped.
                            // We log a warning but send anyway (might fail on host).
                            log::warn!("Could not map sender ID {} to host ID", gid);
                        }
                    }

                    out_buffer.extend_from_slice(&data);
                    out_fds.extend(fds);
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
                    close_pending_output_fds(&mut out_fds, &conn.read_fds);
                    return false;
                }
            }

            let Some(new_fd_offset) = fd_offset.checked_add(consumed_fds) else {
                log::error!(
                    "Consumed fd offset overflows usize: offset={}, consumed={}",
                    fd_offset,
                    consumed_fds
                );
                close_pending_output_fds(&mut out_fds, &conn.read_fds);
                return false;
            };
            if new_fd_offset > conn.read_fds.len() {
                log::error!(
                    "Protocol consumed more fds than received: consumed={}, received={}",
                    new_fd_offset,
                    conn.read_fds.len()
                );
                close_pending_output_fds(&mut out_fds, &conn.read_fds);
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
                out_buffer.extend_from_slice(&p_data);
                out_fds.extend(p_fds);
            }

            offset = message_end;
            if self.ctx.fatal_protocol_error {
                break;
            }
        }

        let mut success = true;
        if !out_buffer.is_empty() && other_conn.send(&out_buffer, &out_fds).await.is_err() {
            success = false;
        }

        let mut fds_to_close: std::collections::HashSet<std::os::unix::io::RawFd> =
            std::collections::HashSet::new();
        fds_to_close.extend(out_fds.iter().copied());

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
        let mut reverse_out_buf = Vec::new();
        let mut reverse_out_fds = Vec::new();
        for (p_data, p_fds) in reverse_queue.drain(..) {
            log::debug!(
                "  -> adding reverse queued message ({} bytes, {} fds)",
                p_data.len(),
                p_fds.len()
            );
            reverse_out_buf.extend_from_slice(&p_data);
            reverse_out_fds.extend(p_fds);
        }
        if !reverse_out_buf.is_empty()
            && conn.send(&reverse_out_buf, &reverse_out_fds).await.is_err()
        {
            // success=false signals the caller (handle_msgs) to drop this
            // client connection. We still close every original below.
            success = false;
        }
        // FDs are closed unconditionally — we own them and must not leak
        // regardless of whether the send succeeded or was skipped. When
        // sendmsg(SCM_RIGHTS) succeeds it duplicates FDs into the kernel
        // cmsg buffer; the originals are still ours to close. When the send
        // fails or the buffer was empty, we obviously retain ownership.
        fds_to_close.extend(reverse_out_fds.iter().copied());

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
mod tests {
    use super::*;
    use crate::wire::MessageBuilder;
    use std::fs;

    fn assert_fd_released(fd: std::os::unix::io::RawFd, target: &std::path::Path) {
        let current = fs::read_link(format!("/proc/self/fd/{fd}"));
        assert!(
            current.as_ref().map_or(true, |current| current != target),
            "descriptor {fd} still refers to its owned resource: {current:?}"
        );
    }

    #[tokio::test]
    async fn untracked_message_fd_is_retained_when_followup_is_partial() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into_raw_fd()),
            WaylandConnection::new(host_socket.into_raw_fd()),
            false,
            false,
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
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into_raw_fd()),
            WaylandConnection::new(host_socket.into_raw_fd()),
            false,
            false,
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
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into_raw_fd()),
            WaylandConnection::new(host_socket.into_raw_fd()),
            false,
            false,
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

    #[tokio::test]
    async fn unknown_client_sender_is_a_protocol_error() {
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (client_socket, host_socket) =
            UnixStream::pair().expect("test socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(client_socket.into_raw_fd()),
            WaylandConnection::new(host_socket.into_raw_fd()),
            false,
            false,
        );

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
    }

    #[tokio::test]
    async fn invalid_registry_bind_flushes_display_error_before_disconnect() {
        use std::io::Read;
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (guest_proxy, guest_peer) =
            UnixStream::pair().expect("guest socket pair should be created");
        let (host_proxy, host_peer) =
            UnixStream::pair().expect("host socket pair should be created");
        let mut client = Client::new(
            WaylandConnection::new(guest_proxy.into_raw_fd()),
            WaylandConnection::new(host_proxy.into_raw_fd()),
            false,
            false,
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
        assert_eq!(u32::from_ne_bytes(header[0..4].try_into().unwrap()), 1);
        assert_eq!(
            (u32::from_ne_bytes(header[4..8].try_into().unwrap()) & 0xffff) as u16,
            protocols::wayland::wl_display::EVT_ERROR
        );
        assert!(
            (u32::from_ne_bytes(header[4..8].try_into().unwrap()) >> 16) >= 20,
            "display.error must include object, code, and a diagnostic string"
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
            Err(ProtocolError::InvalidObjectId(guest_id))
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
    fn checked_message_end_rejects_offset_overflow() {
        assert_eq!(checked_message_end(usize::MAX - 8, 8), Some(usize::MAX));
        assert_eq!(checked_message_end(usize::MAX, 1), None);
    }

    #[test]
    fn message_lengths_must_be_aligned_and_include_the_header() {
        assert!(!valid_message_length(0));
        assert!(!valid_message_length(7));
        assert!(valid_message_length(8));
        assert!(valid_message_length(12));
        assert!(!valid_message_length(10));
        assert!(valid_message_length(65_532));
        assert!(!valid_message_length(65_536));
    }

    #[test]
    fn early_output_cleanup_preserves_incoming_fd_ownership() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let mut output = vec![pipe_fds[1]];
        close_pending_output_fds(&mut output, &[pipe_fds[1]]);

        // The incoming connection still owns this descriptor; the cleanup
        // helper must not close it before the connection drains/drops.
        assert_ne!(unsafe { libc::fcntl(pipe_fds[1], libc::F_GETFD) }, -1);
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        let mut second_pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(second_pipe.as_mut_ptr()) }, 0);
        let fd = second_pipe[1];
        let mut output = vec![fd];
        close_pending_output_fds(&mut output, &[]);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        unsafe {
            libc::close(second_pipe[0]);
        }
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
        ctx.shadow_table.mark_pending_destroy(11);
        ctx.retired_buffers.insert(11, test_buffer_state());
        assert!(!pending_host_event_is_stale(
            &ctx,
            21,
            protocols::wayland::wl_buffer::EVT_RELEASE,
            Some(11)
        ));
    }

    fn test_buffer_state() -> crate::state::BufferState {
        use crate::state::{BufferState, PoolInner, PoolState};
        use std::sync::{Arc, RwLock};
        BufferState {
            guest_buffer_id: 11,
            pool: Arc::new(PoolState {
                client_fd: -1,
                inner: RwLock::new(PoolInner {
                    client_ptr: std::ptr::null_mut(),
                    size: 0,
                }),
            }),
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: 0,
            host_buffer_id: 21,
            bo: None,
            dmabuf_fd: None,
            bo_stride: 4,
            dest_ptr: std::ptr::null_mut(),
            dest_size: 0,
            needs_full_copy: false,
            host_released: false,
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
        let payload = builder.payload;
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
    wl_seat: seat
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

// Keyboard Extension Protocol (ChromeOS-specific)
protocols::keyboard_extension_unstable_v1::impl_sommelier_delegates!(SommelierHandler, {
    zcr_keyboard_extension_v1: keyboard,
    zcr_extended_keyboard_v1: keyboard
});
impl protocols::keyboard_extension_unstable_v1::ProtocolHandler for SommelierHandler {}

pub async fn run(
    display: &str,
    local_compositor: Option<String>,
    gpu_accel: bool,
    xdg_decoration: bool,
    virtio_wayland: Option<String>,
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

    let listener = UnixListener::bind(display).expect("Failed to bind socket");
    log::info!("Listening on {}", display);

    // KeyboardHandler owns xkb state, which is intentionally !Send. Keep each
    // client task on this executor's thread instead of using `tokio::spawn`
    // (whose Send bound would require an unsound manual Send implementation).
    // `LocalSet::run_until` also makes the ownership requirement visible in
    // this function's future type: callers cannot accidentally move the
    // proxy's client tasks to another executor thread.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        log::info!("New client connected");

                        let client_fd = stream.into_std().unwrap().into_raw_fd();
                        if let Err(e) = set_nonblocking(client_fd) {
                            log::error!("Failed to set non-blocking on client fd: {}", e);
                            let _ = nix::unistd::close(client_fd);
                            continue;
                        }
                        let client_conn = WaylandConnection::new(client_fd);

                        let mut virtwayland_channel_ref = None;

                        let host_conn = {
                            let mut conn = None;

                            if let Some(path) = &virtio_wayland {
                                match VirtWaylandChannel::new(path) {
                                    Ok(channel) => {
                                        let channel_arc = Arc::new(channel);
                                        virtwayland_channel_ref = Some(channel_arc.clone());
                                        conn =
                                            Some(WaylandConnection::new_virtwayland(channel_arc));
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "Failed to open virtio-wayland channel at {}: {}",
                                            path,
                                            e
                                        );
                                    }
                                }
                            }

                            if conn.is_none() {
                                if let Some(path) = local_compositor.as_ref() {
                                    match UnixStream::connect(path).await {
                                        Ok(stream) => {
                                            let host_fd =
                                                stream.into_std().unwrap().into_raw_fd();
                                            if let Err(e) = set_nonblocking(host_fd) {
                                                log::error!(
                                                    "Failed to set non-blocking on host fd: {}",
                                                    e
                                                );
                                                let _ = nix::unistd::close(host_fd);
                                            } else {
                                                conn = Some(WaylandConnection::new(host_fd));
                                            }
                                        }
                                        Err(e) => {
                                            log::error!("Failed to connect to host: {}", e);
                                        }
                                    }
                                } else {
                                    log::error!(
                                        "Local compositor path required if not using virtio-wayland (or if it failed)"
                                    );
                                }
                            }
                            conn
                        };

                        if let Some(host_conn) = host_conn {
                            let mut client =
                                Client::new(client_conn, host_conn, gpu_accel, xdg_decoration);
                            if let Some(channel) = virtwayland_channel_ref {
                                client.ctx.virtwayland_channel = Some(channel);
                            }

                            tokio::task::spawn_local(async move {
                                client.run().await;
                                log::info!("Client disconnected");
                            });
                        } else {
                            log::error!("Failed to establish host connection, closing client");
                        }
                    }
                    Err(e) => {
                        log::error!("Accept error: {}", e);
                    }
                }
            }
        })
        .await;
}
