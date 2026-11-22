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

//! One message through the proxy, without I/O: [`dispatch`] resolves a
//! framed message's sender, decodes and handles it, and commits what it
//! produced to the output pending for each peer, as one transaction
//! (DESIGN "Message effects"). `Client::handle_msgs` is the shell around
//! it: it frames each connection's bytes, records traces, parks a request
//! that waits for a fence, and sends.

use std::collections::VecDeque;
use std::os::fd::{AsFd, OwnedFd};

use super::Direction;
use crate::connection::OutgoingMessages;
use crate::handler::Handlers;
use crate::protocols::wayland::wl_keyboard;
use crate::protocols::{self, AnyEvent, Iface};
use crate::state::{Context, EventSender, GuestId, HostId, PostDispatch, SessionEnd, Wait};
use crate::warn_once::WarnOnce;
use crate::wire::{MessageHeader, Outgoing, ProtocolError, ToGuest, ToHost, WireMessage};

/// One framed message from `direction`'s source: its header, its payload
/// after the header, and the descriptors its connection has queued, which
/// it takes from the front as it decodes.
pub(super) struct Frame<'a> {
    pub(super) direction: Direction,
    pub(super) header: MessageHeader,
    pub(super) payload: &'a [u8],
    pub(super) fds: &'a mut VecDeque<OwnedFd>,
}

/// The output pending for each peer, which a handled message appends to.
pub(super) struct Outbox<'a> {
    pub(super) to_host: &'a mut OutgoingMessages<HostId>,
    pub(super) to_guest: &'a mut OutgoingMessages<GuestId>,
}

/// What dispatching one message did.
pub(super) enum Dispatched {
    /// It was handled: its forward and its output are queued.
    Handled,
    /// It ends the session, and queued nothing of its own.
    Ends(SessionEnd),
    /// It waits, for a guest GPU fence or the guest's globals, having
    /// changed nothing, and is to be dispatched again
    /// (`Context::wait_for_fence`, `Context::wait_for_globals`).
    Waits(Wait),
}

/// What [`dispatch`] reports of one message.
pub(super) struct Report {
    pub(super) dispatched: Dispatched,
    /// How many of the queued descriptors the message took.
    pub(super) fds: usize,
    /// Whether the message could not be read while descriptors stay
    /// queued that it may own (`WaylandConnection::ambiguous_untracked_fd`).
    pub(super) ambiguous_fds: bool,
    /// The text of a host keymap the message carried, when asked for.
    pub(super) keymap: Option<Vec<u8>>,
}

/// The forward of a message, to the peer its direction goes to.
enum Forward {
    /// A guest request's, for the host.
    Host(ToHost),
    /// A host event's, for the guest.
    Guest(ToGuest),
}

/// What the proxy read of a host event.
enum HostRead {
    /// Its sender is no live host object the proxy tracks.
    Untracked,
    /// The event, whole.
    Event(EventSender, AnyEvent),
    /// An event the generated code cannot read: an unknown opcode, or
    /// arguments that do not decode whole.
    Unreadable(EventSender, ProtocolError),
}

/// Dispatch `frame` to its handler and commit what it produced to `out`:
/// its forward, then its output in the same direction and back toward its
/// source, unless it ends the session or waits for a fence. With
/// `record_keymap`, the report carries the text of a host keymap.
pub(super) fn dispatch(
    ctx: &mut Context,
    frame: Frame<'_>,
    out: &mut Outbox<'_>,
    record_keymap: bool,
) -> Report {
    let Frame {
        direction,
        header,
        payload,
        fds,
    } = frame;
    let MessageHeader {
        sender_id,
        opcode,
        len,
    } = header;
    let queued_fds = fds.len();
    let mut keymap = None;
    let mut ambiguous_fds = false;
    // The message takes its descriptors from the front of the queue;
    // dropping it unforwarded closes them.
    let msg = |fds| WireMessage::new(sender_id, opcode, payload).with_fds(fds);
    let result = match direction {
        Direction::ClientToHost => match ctx.shadow_table.resolve_request(sender_id) {
            Some(sender) => {
                log::trace!("[{direction:?}] {}:{opcode} (len={len})", sender.iface);
                let mut msg = msg(&mut *fds);
                let forward = protocols::dispatch_request::<Handlers>(sender, &mut msg, ctx);
                whole(&msg, sender.iface, forward).map(|forward| forward.map(Forward::Host))
            }
            // A guest request is only valid when its sender is a live
            // object owned by this connection. Unlike a stale host event
            // (which may have raced object teardown), silently dropping an
            // unknown client request makes the client believe the request
            // was accepted and can desynchronize protocol state.
            None => Err(ProtocolError::InvalidSender(sender_id)),
        },
        Direction::HostToClient => {
            let read = match ctx.shadow_table.resolve_event(sender_id) {
                None => HostRead::Untracked,
                Some(sender) => {
                    log::trace!("[{direction:?}] {}:{opcode} (len={len})", sender.iface);
                    // Each host event is decoded once, whole, for
                    // everything that reads it.
                    let mut msg = msg(&mut *fds);
                    let event = AnyEvent::decode(sender.iface, &mut msg);
                    match msg.check_consumed(event) {
                        Ok(event) => HostRead::Event(sender, event),
                        Err(error) => HostRead::Unreadable(sender, error),
                    }
                }
            };
            if let (
                true,
                HostRead::Event(
                    _,
                    AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { fd, size, .. }),
                ),
            ) = (record_keymap, &read)
            {
                keymap = Some(crate::keymap::bytes(fd.as_fd(), *size).unwrap_or_default());
            }
            match read {
                // One that does not decode ends the session before anything
                // reads it, unless only the proxy uses its object (below).
                HostRead::Unreadable(sender, error) if sender.guest.is_some() => Err(error),
                read => {
                    let event = match &read {
                        HostRead::Event(sender, event) => Some((sender, event)),
                        _ => None,
                    };
                    // A destructor has already been forwarded for a stale
                    // event's object: it reaches no handler and not the
                    // guest, unless its interface's owner says it ends what
                    // the destroy left.
                    let stale = event.is_some_and(|(sender, event)| {
                        sender.guest.is_some_and(|guest| guest.destroyed)
                            && !crate::handler::outlives_destroy(ctx, sender, event)
                    });
                    // An empty IME confirmation's recovery of a held key
                    // waits for the host's next event, which may show that
                    // it was Exo's confirmation before a new press. What it
                    // sends the guest precedes this event's own forward.
                    crate::handler::keyboard::before_host_event(ctx, event, stale);
                    {
                        let decided = ctx.take_effects();
                        debug_assert!(decided.end.is_none() && decided.wait.is_none());
                        commit(out, None, decided.to_host, decided.to_guest);
                    }
                    match read {
                        HostRead::Event(..) if stale => Ok(None),
                        HostRead::Event(sender, event) => {
                            let forward = protocols::dispatch_event::<Handlers>(
                                sender,
                                event,
                                len - MessageHeader::LEN,
                                ctx,
                            );
                            match forward {
                                Err(error @ ProtocolError::UnsupportedVersion { .. })
                                    if sender.guest.is_none() =>
                                {
                                    unreadable_internal_event(sender, opcode, &error);
                                    Ok(None)
                                }
                                forward => forward.map(|forward| forward.map(Forward::Guest)),
                            }
                        }
                        read => {
                            if let HostRead::Unreadable(sender, error) = &read {
                                unreadable_internal_event(*sender, opcode, error);
                            } else {
                                log::debug!(
                                    "[{direction:?}] untracked host id {sender_id} opcode {opcode} (len={len})"
                                );
                            }
                            // SCM_RIGHTS descriptors form an ordered stream
                            // independent of byte-buffer boundaries. A
                            // descriptor received alongside a complete
                            // message the proxy cannot read may belong to a
                            // later message whose bytes are still partial in
                            // this buffer. Do not terminate the connection
                            // merely because the unread message cannot tell
                            // us how many descriptors it consumes; leave the
                            // descriptor queued for the next complete message
                            // and let the connection close it if the peer
                            // goes away before that message arrives.
                            ambiguous_fds = !fds.is_empty();
                            Ok(None)
                        }
                    }
                }
            }
        }
    };

    let forward = match result {
        Ok(forward) => forward,
        Err(e) => {
            let interface = match direction {
                Direction::ClientToHost => ctx
                    .shadow_table
                    .resolve_request(sender_id)
                    .map(|sender| sender.iface),
                Direction::HostToClient => ctx
                    .shadow_table
                    .resolve_event(sender_id)
                    .map(|sender| sender.iface),
            }
            .map_or("unknown", Iface::name);
            log::error!("Protocol error: {e} (direction={direction:?}, id={sender_id}, interface={interface}, opcode={opcode})");
            ctx.end_session(rejection(direction, &e));
            None
        }
    };
    let after_forward = ctx.take_post_dispatch();
    // Only a guest request waits, and only one that takes no
    // descriptors and has no forward, output or deferred work: its
    // descriptors would be gone when it is dispatched again, and the rest
    // would be lost. A host event that waited would spin the loop. Any
    // other wait is a proxy bug, which ends the session before its deferred
    // work changes anything.
    if ctx.waits() && !ctx.is_fatal() {
        let broken = [
            (direction != Direction::ClientToHost, "it is a host event"),
            (fds.len() != queued_fds, "it took descriptors"),
            (forward.is_some(), "it has a forward"),
            (!after_forward.is_empty(), "it deferred work"),
            (ctx.sends_anything(), "it queued output"),
        ]
        .into_iter()
        .find_map(|(broken, why)| broken.then_some(why));
        if let Some(why) = broken {
            log::error!(
                "A {direction:?} message from object {sender_id} waits, \
                 but {why}; ending the session"
            );
            ctx.end_session(SessionEnd::internal());
        }
    }
    // Work deferred until the message is forwarded runs only if it is,
    // which a message that ends the session never is.
    if forward.is_some() && !ctx.is_fatal() {
        for effect in after_forward {
            apply_post_dispatch(ctx, effect);
        }
    }
    // A message's effects are committed whole or not at all: one that ends
    // the session sends nothing, and dropping its forward and effects
    // closes their descriptors.
    let effects = ctx.take_effects();
    let dispatched = match (effects.end, effects.wait) {
        (Some(end), _) => Dispatched::Ends(end),
        // A message that waits changed nothing.
        (None, Some(wait)) => Dispatched::Waits(wait),
        (None, None) => {
            commit(out, forward, effects.to_host, effects.to_guest);
            Dispatched::Handled
        }
    };
    Report {
        dispatched,
        fds: queued_fds - fds.len(),
        ambiguous_fds,
        keymap,
    }
}

/// Handle `delivery`, which another connection handed this one through
/// the clipboard hub, as a transaction like a message's: commit what it
/// produced to `out`, or nothing if it ends the session, which it returns.
pub(super) fn deliver(
    ctx: &mut Context,
    delivery: crate::handler::data_control::Delivery,
    out: &mut Outbox<'_>,
) -> Option<SessionEnd> {
    crate::handler::data_control::deliver(ctx, delivery);
    let effects = ctx.take_effects();
    debug_assert!(effects.wait.is_none(), "a delivery never waits");
    if effects.end.is_none() {
        commit(out, None, effects.to_host, effects.to_guest);
    }
    effects.end
}

/// Queue `forward`, then `to_host` and `to_guest`, each for its peer. The
/// generated dispatcher encoded the forward under the sender it translated
/// before the handler ran, so a handler that retires the sender's mapping
/// cannot change it.
fn commit(
    out: &mut Outbox<'_>,
    forward: Option<Forward>,
    to_host: impl Iterator<Item = ToHost>,
    to_guest: impl Iterator<Item = ToGuest>,
) {
    match forward {
        Some(Forward::Host(message)) => out.to_host.push(translated(message)),
        Some(Forward::Guest(message)) => out.to_guest.push(translated(message)),
        None => {}
    }
    for message in to_host {
        out.to_host.push(message);
    }
    for message in to_guest {
        out.to_guest.push(message);
    }
}

/// Log forward `message`, which the dispatcher translated.
fn translated<I>(message: Outgoing<I>) -> Outgoing<I> {
    log::debug!(
        "  -> translated ({} bytes, {} fds)",
        message.bytes().len(),
        message.fds().len()
    );
    message
}

/// Whether an event from an object only the proxy uses has failed to
/// decode yet. Only the first warns.
pub(super) static UNREADABLE_INTERNAL_EVENT: WarnOnce = WarnOnce::new();

/// Drop an event that `sender`, an object only the proxy uses (its own
/// `zaura_shell` binding, say), sent with `opcode`, and that the proxy
/// cannot read (`error`): it reaches no guest object, so the guest's
/// session goes on, as it did when the proxy skipped the events of its
/// own Aura objects unread.
fn unreadable_internal_event(sender: EventSender, opcode: u16, error: &ProtocolError) {
    log::log!(
        UNREADABLE_INTERNAL_EVENT.level(),
        "Dropping event {opcode} of host {} {}, which only the proxy uses: {error}",
        sender.iface,
        sender.host
    );
}

/// `decoded`, the result of decoding `msg` from an object of `iface`,
/// unless it left trailing bytes (see [`WireMessage::check_consumed`]),
/// which are logged.
fn whole<T>(
    msg: &WireMessage,
    iface: Iface,
    decoded: Result<T, ProtocolError>,
) -> Result<T, ProtocolError> {
    let decoded = msg.check_consumed(decoded);
    if decoded.as_ref().err() == Some(&ProtocolError::TrailingData) {
        log::error!(
            "Protocol message left {} trailing payload byte(s): interface={iface}, opcode={}",
            msg.payload.len() - msg.offset,
            msg.opcode
        );
    }
    decoded
}

/// How a message the proxy cannot frame, decode or dispatch ends the
/// session.
pub(super) fn rejection(direction: Direction, error: &ProtocolError) -> SessionEnd {
    match direction {
        // Tell the guest why, as libwayland-server does.
        Direction::ClientToHost => crate::handler::display::request_error(error),
        // The host broke the protocol; the guest did nothing wrong.
        Direction::HostToClient => SessionEnd::internal(),
    }
}

/// Apply the work a handler deferred until its message was forwarded (see
/// [`PostDispatch`]).
fn apply_post_dispatch(ctx: &mut Context, effect: PostDispatch) {
    match effect {
        PostDispatch::Attach { object, data } => {
            if !ctx.shadow_table.attach_data(object, data) {
                let _ = ctx.fail(format_args!("Guest object {object} cannot keep its state"));
            }
        }
        PostDispatch::XdgToplevelCreated {
            xdg_surface,
            xdg_toplevel,
        } => crate::handler::compositor::record_xdg_toplevel(ctx, xdg_surface, xdg_toplevel),
        PostDispatch::KeyboardCreated { keyboard, seat } => {
            crate::handler::keyboard::keyboard_created(ctx, keyboard, seat)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_display;
    use crate::wire::WL_DISPLAY_ID;

    /// Dispatch the guest's `message` through the core alone, with no
    /// connection, into `out`.
    fn dispatch_guest(ctx: &mut Context, message: Outgoing<u32>, out: &mut Outbox<'_>) -> Report {
        let bytes = message.bytes();
        let frame = Frame {
            direction: Direction::ClientToHost,
            header: MessageHeader::parse(bytes).expect("a whole message"),
            payload: &bytes[MessageHeader::LEN..],
            fds: &mut VecDeque::new(),
        };
        dispatch(ctx, frame, out, false)
    }

    /// The core handles a framed message without I/O: a guest's sync is
    /// forwarded to the host, and a request from an object the guest does
    /// not have ends the session and queues nothing.
    #[test]
    fn a_frame_dispatches_without_a_connection() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table.create_display();
        let (mut to_host, mut to_guest) = Default::default();
        let mut out = Outbox {
            to_host: &mut to_host,
            to_guest: &mut to_guest,
        };

        let sync = wl_display::req::sync(WL_DISPLAY_ID, 2);
        let report = dispatch_guest(&mut ctx, sync, &mut out);
        assert!(matches!(report.dispatched, Dispatched::Handled));
        assert_eq!((out.to_host.len(), out.to_guest.len()), (1, 0));

        let stray = wl_display::req::sync(7, 3);
        let report = dispatch_guest(&mut ctx, stray, &mut out);
        assert!(matches!(
            report.dispatched,
            Dispatched::Ends(SessionEnd::Protocol {
                object: WL_DISPLAY_ID,
                code: wl_display::ERROR_INVALID_OBJECT,
                ..
            })
        ));
        assert_eq!((out.to_host.len(), out.to_guest.len()), (1, 0));
    }
}
