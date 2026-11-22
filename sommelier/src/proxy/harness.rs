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

//! A whole proxy client driven through its two connections, for tests that
//! pin what the proxy sends rather than how it stores it.
//!
//! A [`Session`] plays both peers of a [`Client`]: the guest client and the
//! host compositor. A test hands it messages built with the generated
//! `req::` and `evt::` encoders, it dispatches them as `Client::run` does,
//! and it decodes everything the proxy sends either peer into typed
//! messages ([`AnyRequest`], [`AnyEvent`]). Like a Wayland client library,
//! each peer follows its objects through the messages that create and end
//! them ([`wire_peer`]), so a test names objects by their IDs on the wire,
//! learns host IDs from the forwarded requests and never reads the object
//! table, and each peer holds what the proxy sends it to the protocol: it
//! names only objects the peer has, creates objects under free IDs of the
//! proxy's range and ends them as the protocol allows. Every batch that
//! keeps the session checks that the proxy holds no state for an ended
//! object (`Context::assert_no_orphans`).

use super::{Client, Direction, Parked};
use crate::connection::WaylandConnection;
use crate::protocols::wayland::{wl_compositor, wl_display, wl_registry};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::state::{Context, GuestId, HostId};
use crate::test_support::counting::Counts;
use crate::test_support::wire_peer::{self, Message, Role};
use crate::test_support::TestConnection;
use crate::wire::{Framing, MessageHeader, Outgoing, WireId, WL_DISPLAY_ID};
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// A guest object and the host object the proxy paired it with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Object {
    pub(crate) id: u32,
    pub(crate) host: u32,
}

impl Object {
    /// The guest object's ID, as the proxy keys it.
    pub(crate) const fn guest_id(self) -> GuestId {
        GuestId::for_test(self.id)
    }

    /// The host object's ID, as the proxy keys it.
    pub(crate) const fn host_id(self) -> HostId {
        HostId::for_test(self.host)
    }
}

/// A [`Client`] whose guest and host are played by the test.
pub(crate) struct Session {
    client: Client,
    guest: Peer<AnyEvent>,
    host: Peer<AnyRequest>,
    next_id: u32,
    next_global_name: u32,
    registry: Option<Object>,
    compositor: Option<u32>,
    wm_base: Option<u32>,
    text_input_manager: Option<u32>,
    /// What the proxy cost while it dispatched, since [`Self::proxy_cost`]
    /// last took it.
    proxy_cost: Counts,
    /// The most bytes the proxy receives at once ([`Self::receive_in`]).
    piece: usize,
}

impl Session {
    /// A session whose proxy has a plain test context.
    pub(crate) fn new() -> Self {
        let ctx = TestConnection::default().context();
        let (guest, guest_socket) = UnixStream::pair().expect("a guest socket pair");
        let (host, host_socket) = UnixStream::pair().expect("a host socket pair");
        let connection =
            |socket: UnixStream| WaylandConnection::new(socket.into()).expect("register a socket");
        Self {
            client: Client::new(connection(guest_socket), connection(host_socket), ctx),
            guest: Peer::new(connection(guest), Role::Client),
            host: Peer::new(connection(host), Role::Server),
            next_id: WL_DISPLAY_ID + 1,
            next_global_name: 1,
            registry: None,
            compositor: None,
            wm_base: None,
            text_input_manager: None,
            proxy_cost: Counts::default(),
            piece: 4096,
        }
    }

    /// Record what the proxy handles and sends from now on in `out`, as
    /// `SOMMELIER_TRACE_DIR` records a connection (`crate::trace`).
    /// Each peer keeps the bytes it gets from then on
    /// ([`Self::take_received_bytes`]).
    pub(crate) fn record(&mut self, out: impl std::io::Write + 'static) {
        let facts = crate::trace::Facts::of(&self.client.ctx);
        let recorder = crate::trace::Recorder::new(Box::new(out), &facts);
        self.client.trace = Some(recorder.expect("start a trace"));
        self.guest.bytes = Some(Vec::new());
        self.host.bytes = Some(Vec::new());
    }

    /// The bytes the guest and the host got since the last call, if the
    /// session records ([`Self::record`]).
    pub(crate) fn take_received_bytes(&mut self) -> (Vec<u8>, Vec<u8>) {
        let take = |bytes: &mut Option<Vec<u8>>| bytes.as_mut().map(std::mem::take);
        let guest = take(&mut self.guest.bytes).expect("the session records");
        (
            guest,
            take(&mut self.host.bytes).expect("the session records"),
        )
    }

    /// The proxy's state: `ctx()` reads state no peer can observe, or
    /// injects a fault no peer can cause; each use says which, in a
    /// comment.
    pub(crate) fn ctx(&mut self) -> &mut Context {
        &mut self.client.ctx
    }

    /// The guest sends `requests`, which must not end the session.
    pub(crate) async fn guest(&mut self, requests: impl IntoIterator<Item = Outgoing<u32>>) {
        assert!(
            self.send(Direction::ClientToHost, requests).await,
            "the guest's requests ended the session"
        );
    }

    /// The guest sends `requests`, which must end the session.
    pub(crate) async fn guest_fatal(&mut self, requests: impl IntoIterator<Item = Outgoing<u32>>) {
        assert!(
            !self.send(Direction::ClientToHost, requests).await,
            "the guest's requests did not end the session"
        );
    }

    /// The host sends `events`, which must not end the session.
    pub(crate) async fn host(&mut self, events: impl IntoIterator<Item = Outgoing<u32>>) {
        assert!(
            self.send(Direction::HostToClient, events).await,
            "the host's events ended the session"
        );
    }

    /// The host sends `events`, which must end the session.
    pub(crate) async fn host_fatal(&mut self, events: impl IntoIterator<Item = Outgoing<u32>>) {
        assert!(
            !self.send(Direction::HostToClient, events).await,
            "the host's events did not end the session"
        );
    }

    /// The guest sends `requests`; returns whether the session goes on, for
    /// a driver that reports a session's end rather than asserting it.
    pub(crate) async fn try_guest(
        &mut self,
        requests: impl IntoIterator<Item = Outgoing<u32>>,
    ) -> bool {
        self.send(Direction::ClientToHost, requests).await
    }

    /// The host sends `events`; returns whether the session goes on.
    pub(crate) async fn try_host(
        &mut self,
        events: impl IntoIterator<Item = Outgoing<u32>>,
    ) -> bool {
        self.send(Direction::HostToClient, events).await
    }

    /// Make every later send to the guest fail, as when its connection
    /// breaks. Closing the guest's end would not do: a process another test
    /// spawns may hold a copy of it.
    pub(crate) fn break_guest_connection(&self) {
        let socket = self
            .client
            .client_conn
            .unix_socket_for_test()
            .expect("the guest connection is a Unix socket")
            .as_raw_fd();
        assert_eq!(unsafe { libc::shutdown(socket, libc::SHUT_WR) }, 0);
    }

    /// Whether a guest request waits, for a fence or the guest's globals,
    /// holding back the guest's stream.
    pub(crate) fn parked(&self) -> bool {
        self.client.parked.is_some()
    }

    /// Whether a guest request waits for a fence: only such a request is
    /// due on its own ([`Self::resume`]); a host batch releases one that
    /// waits for the globals.
    fn waits_for_fence(&self) -> bool {
        matches!(self.client.parked, Some(Parked::Fence(_)))
    }

    /// Wait until the guest request that waits for a fence is due, as
    /// `Client::run` does, and dispatch the guest's stream from it, which
    /// must not end the session.
    pub(crate) async fn resume(&mut self) {
        assert!(self.waits_for_fence(), "no guest request waits for a fence");
        Parked::due(self.client.parked.as_ref()).await;
        self.guest([]).await;
    }

    /// What the proxy itself cost on this thread while it dispatched the
    /// messages either peer sent, since the last call: not the test's
    /// encoding of them, nor the peers' decoding of what the proxy sent.
    pub(crate) fn proxy_cost(&mut self) -> Counts {
        std::mem::take(&mut self.proxy_cost)
    }

    /// What the proxy sent the host since the last call, with the sender of
    /// each request.
    pub(crate) fn host_received(&mut self) -> Vec<(u32, AnyRequest)> {
        std::mem::take(&mut self.host.received)
    }

    /// What the proxy sent the guest since the last call, with the sender
    /// of each event.
    pub(crate) fn guest_received(&mut self) -> Vec<(u32, AnyEvent)> {
        std::mem::take(&mut self.guest.received)
    }

    /// What the proxy sent the guest since the last call before the
    /// `wl_display.error` that ended the session, and that error's object
    /// and code. The error must be the guest's one error and its last
    /// event, and carry a diagnostic.
    pub(crate) fn guest_received_and_error(&mut self) -> (Vec<(u32, AnyEvent)>, (u32, u32)) {
        let (events, error, _) = self.guest_received_and_diagnosed_error();
        (events, error)
    }

    /// [`Self::guest_received_and_error`], with the error's diagnostic.
    pub(crate) fn guest_received_and_diagnosed_error(
        &mut self,
    ) -> (Vec<(u32, AnyEvent)>, (u32, u32), String) {
        assert!(self.client.ended, "the session goes on");
        let mut events = self.guest_received();
        let Some((
            WL_DISPLAY_ID,
            AnyEvent::WlDisplay(wl_display::Event::Error {
                object_id,
                code,
                message,
            }),
        )) = events.pop()
        else {
            panic!("the guest's last event is not an error: {events:?}");
        };
        assert!(!message.is_empty(), "an error carries a diagnostic");
        assert!(
            !events.iter().any(|(_, event)| matches!(
                event,
                AnyEvent::WlDisplay(wl_display::Event::Error { .. })
            )),
            "the guest got more than one error: {events:?}"
        );
        (events, (object_id, code), message)
    }

    /// A guest object ID no object has had.
    pub(crate) fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Pair guest object `id` with a new host object, of `interface` at
    /// `version`, as the proxy pairs an object that no peer here can have
    /// it create, such as a native dma-buf buffer: the proxy and both peers
    /// have it from now on. Returns the host object's ID.
    pub(crate) fn pair_for_test(&mut self, id: u32, interface: &'static str, version: u32) -> u32 {
        let shadow_table = &mut self.client.ctx.shadow_table;
        let iface = crate::protocols::Iface::from_name(interface).expect("a vendored interface");
        let host = shadow_table
            .create_paired(GuestId::for_test(id), iface, version, version)
            .wire();
        self.guest.wire.adopt(id, interface, version);
        self.host.wire.adopt(host, interface, version);
        host
    }

    /// Have the proxy receive what each peer sends `piece` bytes at a time,
    /// rather than a whole receive's worth (4096), so that a message
    /// reaches it in pieces.
    pub(crate) fn receive_in(&mut self, piece: usize) {
        assert!(piece > 0, "a piece of at least a byte");
        self.piece = piece;
    }

    /// Let the proxy end on its own the guest objects for which
    /// `may_end(id, interface)` holds, as it does when the host ends their
    /// host objects. By default it may end only the frame callbacks of a
    /// surface that ended (`wire_peer::Peer::server_may_end`).
    pub(crate) fn host_may_end(&mut self, may_end: impl Fn(u32, &str) -> bool + Send + 'static) {
        self.guest.wire.server_may_end(may_end);
    }

    /// `event`, which the host sends, decoded as the proxy will read it:
    /// by the host objects the proxy has told the host about, and the ones
    /// the host's earlier events created.
    ///
    /// # Panics
    ///
    /// If it does not decode so.
    pub(crate) fn decode_host_event(&self, outgoing: &Outgoing<u32>) -> (u32, AnyEvent) {
        let (message, fds) = (outgoing.bytes(), outgoing.fds());
        let mut fds = fds
            .iter()
            .map(|fd| fd.try_clone().expect("duplicate a descriptor"))
            .collect();
        let decoded = self.host.wire.decode(message, &mut fds);
        decoded.unwrap_or_else(|error| panic!("the host sends {message:?}: {error}"))
    }

    /// The guest sends `request`, which creates one object; returns the host
    /// ID of the object its forward creates. The forward is not kept for
    /// [`Self::host_received`].
    pub(crate) async fn create(&mut self, request: Outgoing<u32>) -> u32 {
        let (_, interface) = self
            .guest
            .created::<AnyRequest>(&request)
            .expect("the request creates an object");
        let earlier = self.host.received.len();
        self.guest([request]).await;
        let forwarded = self.host.received.split_off(earlier);
        let [(_, forward)] = &forwarded[..] else {
            panic!("the request was not forwarded alone: {forwarded:?}");
        };
        let (host, host_interface) = forward.new_object().expect("the forward creates an object");
        assert_eq!(
            host_interface, interface,
            "the forward creates another interface"
        );
        host
    }

    /// The host advertises global `interface` at `version` on the guest's
    /// registry, which the guest creates on first use; returns the global's
    /// name. What the proxy sends either peer is kept.
    pub(crate) async fn advertise(&mut self, interface: &str, version: u32) -> u32 {
        let registry = self.registry().await;
        let name = self.next_global_name;
        self.next_global_name += 1;
        self.host([wl_registry::evt::global(registry.host, name, interface, version).unwrap()])
            .await;
        name
    }

    /// The host advertises global `interface` at `version`, and the guest
    /// binds it as the proxy advertises it, possibly under another
    /// interface; returns the guest's object. The guest gets only events
    /// of the new object, which are not kept. What the proxy sends the host
    /// is not kept either: its own reactions to the global, and the bind,
    /// unless the proxy implements the global.
    pub(crate) async fn bind(&mut self, interface: &str, version: u32) -> u32 {
        self.bind_global(interface, version).await.0
    }

    /// [`Self::bind`] for a global the proxy forwards: returns the guest's
    /// object and the host object its bind creates.
    pub(crate) async fn bind_paired(&mut self, interface: &str, version: u32) -> Object {
        let (id, host) = self.bind_global(interface, version).await;
        let host = host.expect("the proxy forwards the bind");
        Object { id, host }
    }

    /// [`Self::bind`], which also returns the host object the forward of
    /// the guest's bind creates, if the proxy forwards it.
    async fn bind_global(&mut self, interface: &str, version: u32) -> (u32, Option<u32>) {
        let earlier = (self.guest.received.len(), self.host.received.len());
        let name = self.advertise(interface, version).await;
        let advertised = self.guest.received.split_off(earlier.0);
        let [(
            _,
            AnyEvent::WlRegistry(wl_registry::Event::Global {
                name: advertised_name,
                interface,
                version,
            }),
        )] = &advertised[..]
        else {
            panic!("the guest was not advertised one global: {advertised:?}");
        };
        assert_eq!(*advertised_name, name);
        let registry = self.registry.expect("advertising created the registry");
        let id = self.new_id();
        let advertised = self.host.received.len();
        self.guest([wl_registry::req::bind(registry.id, name, interface, *version, id).unwrap()])
            .await;
        let forward = self.host.received[advertised..]
            .iter()
            .find_map(|request| match request {
                (_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })) => Some(id.2),
                _ => None,
            });
        let host = self.host.received.split_off(earlier.1);
        for (sender, request) in &host {
            if let AnyRequest::WlRegistry(wl_registry::Request::Bind {
                name: bound_name, ..
            }) = request
            {
                assert!(
                    *sender == registry.host && *bound_name == name,
                    "the host got a bind of another global: {host:?}"
                );
            }
        }
        let events = self.guest.received.split_off(earlier.0);
        assert!(
            events.iter().all(|(sender, _)| *sender == id),
            "the guest got events of other objects: {events:?}"
        );
        (id, forward)
    }

    /// A new `wl_surface`, from a `wl_compositor` the guest binds on first
    /// use.
    pub(crate) async fn surface(&mut self) -> Object {
        let compositor = match self.compositor {
            Some(compositor) => compositor,
            None => {
                let compositor = self.bind("wl_compositor", wl_compositor::VERSION).await;
                *self.compositor.insert(compositor)
            }
        };
        let id = self.new_id();
        let host = self
            .create(wl_compositor::req::create_surface(compositor, id))
            .await;
        Object { id, host }
    }

    /// A new `xdg_toplevel` of a new surface, from an `xdg_wm_base` the
    /// guest binds on first use. What the proxy sends the host besides the
    /// forwards that create the objects is not kept.
    pub(crate) async fn toplevel(&mut self) -> Toplevel {
        use crate::protocols::xdg_shell::{xdg_surface, xdg_wm_base};
        let wm_base = match self.wm_base {
            Some(wm_base) => wm_base,
            None => {
                let wm_base = self.bind("xdg_wm_base", xdg_wm_base::VERSION).await;
                *self.wm_base.insert(wm_base)
            }
        };
        let surface = self.surface().await;
        let id = self.new_id();
        let host = self
            .create(xdg_wm_base::req::get_xdg_surface(wm_base, id, surface.id))
            .await;
        let xdg_surface = Object { id, host };
        let id = self.new_id();
        let earlier = self.host.received.len();
        self.guest([xdg_surface::req::get_toplevel(xdg_surface.id, id)])
            .await;
        let forwarded = self.host.received.split_off(earlier);
        let host = forwarded
            .iter()
            .find_map(|(sender, request)| match request {
                AnyRequest::XdgSurface(xdg_surface::Request::GetToplevel { id })
                    if *sender == xdg_surface.host =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the host got no toplevel: {forwarded:?}"));
        Toplevel {
            surface,
            xdg_surface,
            toplevel: Object { id, host },
        }
    }

    /// The host advertises `zaura_shell`, which the proxy binds for itself
    /// and advertises to the guest as `gtk_shell1`; returns the host
    /// `zaura_shell`. Neither peer's messages are kept.
    pub(crate) async fn aura_shell(&mut self) -> u32 {
        use crate::protocols::aura_shell::zaura_shell;
        self.aura_shell_at(zaura_shell::VERSION).await
    }

    /// [`Self::aura_shell`] at `version`, for a test of what an older
    /// shell gives.
    pub(crate) async fn aura_shell_at(&mut self, version: u32) -> u32 {
        use crate::protocols::aura_shell::zaura_shell;
        self.registry().await;
        self.advertise(zaura_shell::NAME, version).await;
        let host = self.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] = &host[..] else {
            panic!("the proxy did not bind zaura_shell: {host:?}");
        };
        assert_eq!(id.1, version, "the proxy binds the shell it is offered");
        let advertised = self.guest_received();
        assert!(
            matches!(
                &advertised[..],
                [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }))]
                    if interface == "gtk_shell1"
            ),
            "the guest was not advertised gtk_shell1: {advertised:?}"
        );
        id.2
    }

    /// The guest's `wl_registry`, which it creates on first use.
    pub(crate) async fn registry(&mut self) -> Object {
        if let Some(registry) = self.registry {
            return registry;
        }
        let registry = self.get_registry().await;
        *self.registry.insert(registry)
    }

    /// A `wl_registry` the guest creates, whose globals the host advertises
    /// later. A proxy that may withhold a global follows the forwarded
    /// `get_registry` with a sync (`registry::await_globals`), which the
    /// host answers at once, as it has no global yet.
    async fn get_registry(&mut self) -> Object {
        use crate::protocols::wayland::wl_callback;
        let id = self.new_id();
        let earlier = self.host.received.len();
        self.guest([wl_display::req::get_registry(WL_DISPLAY_ID, id)])
            .await;
        let forwarded = self.host.received.split_off(earlier);
        let host = match forwarded[..] {
            [(
                WL_DISPLAY_ID,
                AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry }),
            )] => registry,
            [(
                WL_DISPLAY_ID,
                AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry }),
            ), (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] => {
                self.host([
                    wl_callback::evt::done(callback, 0),
                    wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
                ])
                .await;
                registry
            }
            _ => panic!("the host got {forwarded:?}, not a get_registry"),
        };
        Object { id, host }
    }

    /// Send `messages` over their sender's socket, which the proxy
    /// receives as `Client::run` does, and dispatch them, up to a guest
    /// request that waits for a fence ([`Self::resume`]) or the guest's
    /// globals; returns whether the session goes on. A host batch that
    /// settles the globals a guest request waits for is followed by the
    /// guest's stream, as `Client::run` dispatches it next. It otherwise
    /// dispatches one direction and then sends everything, so it never
    /// interleaves directions, and cannot show when a batch's output leaves
    /// relative to the other peer's batches; the tests of that dispatch a
    /// batch at a time ([`Self::batch`]).
    async fn send(
        &mut self,
        direction: Direction,
        messages: impl IntoIterator<Item = Outgoing<u32>>,
    ) -> bool {
        for message in messages {
            let peer = match direction {
                Direction::ClientToHost => &mut self.guest.wire,
                Direction::HostToClient => &mut self.host.wire,
            };
            peer.send(&message);
            if !self.transmit(direction, message) {
                // The proxy refuses what it received, as `Client::run`
                // ends the session on a failed receive.
                self.client.ended = true;
                return false;
            }
        }
        let before = Counts::now();
        let mut direction = direction;
        let goes_on = loop {
            let goes_on = self.client.handle_msgs(direction).await;
            let parked = self.client.parked.is_some();
            if !goes_on || (direction == Direction::ClientToHost && parked) {
                break goes_on;
            }
            if self.client.connection(direction).has_complete_message() {
                continue;
            }
            // Only a request a host batch released is left in the guest's
            // stream.
            if parked || !self.client.client_conn.has_complete_message() {
                break goes_on;
            }
            direction = Direction::ClientToHost;
        };
        // Nothing more is buffered, so the proxy sends what it holds.
        let goes_on = self.client.flush().await && goes_on;
        self.proxy_cost += Counts::now() - before;
        self.guest.receive();
        self.host.receive();
        goes_on
    }
}

/// The transport between each peer and the proxy.
impl Session {
    /// Write `bytes` to the socket of the peer that sends in `direction`,
    /// `fds` with their first byte, a piece at a time
    /// ([`Self::receive_in`]), and have the proxy receive each piece, as
    /// `Client::run` does, so that it reads them through its own
    /// transport. Returns `false` if the proxy refuses what it receives.
    fn transmit(&mut self, direction: Direction, outgoing: Outgoing<u32>) -> bool {
        let (bytes, fds) = outgoing.into_parts();
        let mut sent = 0;
        while sent < bytes.len() {
            let end = bytes.len().min(sent + self.piece);
            let fds: &[OwnedFd] = if sent == 0 { &fds } else { &[] };
            sent += match direction {
                Direction::ClientToHost => self.guest.write(&bytes[sent..end], fds),
                Direction::HostToClient => self.host.write(&bytes[sent..end], fds),
            };
            let connection = self.client.connection_mut(direction);
            loop {
                match connection.try_recv() {
                    Ok(Some(bytes)) if bytes > 0 => {}
                    Ok(_) => break,
                    Err(_) => return false,
                }
            }
        }
        // The kernel holds its own references to the descriptors it carries.
        drop(fds);
        true
    }
}

/// Sessions with a configured proxy, and keyboards.
impl Session {
    /// A session whose proxy has the test context `ctx`, for a test that
    /// configures what the process would read at startup, such as
    /// accelerators.
    pub(crate) fn with_context(mut ctx: Context) -> Self {
        let mut s = Self::new();
        ctx.shadow_table.create_display();
        s.client.ctx = ctx;
        s
    }

    /// A `wl_keyboard` the guest gets from a `wl_seat` it binds, with the
    /// [`default_keymap`] the host sends it, which the guest gets; returns
    /// the seat and the keyboard.
    pub(crate) async fn keyboard(&mut self) -> (u32, Object) {
        use crate::protocols::wayland::wl_seat;
        let seat = self.bind("wl_seat", wl_seat::VERSION).await;
        (seat, self.keyboard_of(seat).await)
    }

    /// [`Self::keyboard`] of `seat`, which the guest has bound.
    pub(crate) async fn keyboard_of(&mut self, seat: u32) -> Object {
        use crate::protocols::wayland::wl_seat;
        let id = self.new_id();
        let host = self.create(wl_seat::req::get_keyboard(seat, id)).await;
        let keyboard = Object { id, host };
        self.keymap(keyboard).await;
        keyboard
    }

    /// A `wl_pointer` the guest gets from `seat`, which the host's pointer
    /// focus then enters on `surface`; the guest gets the `enter`.
    pub(crate) async fn pointer_on(&mut self, seat: u32, surface: Object) -> Object {
        use crate::protocols::wayland::{wl_pointer, wl_seat};
        let id = self.new_id();
        let host = self.create(wl_seat::req::get_pointer(seat, id)).await;
        self.host([wl_pointer::evt::enter(host, 1, surface.host, 0, 0)])
            .await;
        let enter = wl_pointer::Event::Enter {
            serial: 1,
            surface: surface.id,
            surface_x: 0,
            surface_y: 0,
        };
        assert_eq!(self.guest_received(), [(id, enter.into())]);
        Object { id, host }
    }

    /// A `wl_touch` the guest gets from `seat`.
    pub(crate) async fn touch(&mut self, seat: u32) -> Object {
        use crate::protocols::wayland::wl_seat;
        let id = self.new_id();
        let host = self.create(wl_seat::req::get_touch(seat, id)).await;
        Object { id, host }
    }

    /// The host sends `keyboard` the [`default_keymap`], which the guest
    /// gets.
    pub(crate) async fn keymap(&mut self, keyboard: Object) {
        use crate::protocols::wayland::wl_keyboard;
        let keymap = default_keymap();
        let size = keymap.len() as u32;
        self.keymap_of(keyboard, wl_keyboard::KEYMAP_FORMAT_XKB_V1, &keymap, size)
            .await;
    }

    /// The host sends `keyboard` a keymap of `format` and `size` in a file
    /// holding `bytes`, which the guest gets whether or not the proxy can
    /// use it.
    pub(crate) async fn keymap_of(
        &mut self,
        keyboard: Object,
        format: u32,
        bytes: &[u8],
        size: u32,
    ) {
        use crate::protocols::wayland::wl_keyboard;
        // The memfd's offset stays at its end, as a host may leave it.
        let file = crate::test_support::shm_pool_file(bytes);
        let fd = OwnedFd::from(file.try_clone().expect("duplicate the keymap file"));
        self.host([wl_keyboard::evt::keymap(keyboard.host, format, fd, size)])
            .await;
        let fd = OwnedFd::from(file);
        let keymap = wl_keyboard::Event::Keymap { format, fd, size };
        assert_eq!(self.guest_received(), [(keyboard.id, keymap.into())]);
    }
}

/// A guest `xdg_toplevel`, its `xdg_surface` and `wl_surface`, each with
/// the host object the proxy paired it with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Toplevel {
    pub(crate) surface: Object,
    pub(crate) xdg_surface: Object,
    pub(crate) toplevel: Object,
}

/// A guest text input and the host objects the proxy bridges it to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextInput {
    pub(crate) id: u32,
    /// The host `zwp_text_input_v1`.
    pub(crate) host: u32,
    /// The host `zcr_extended_text_input_v1`.
    pub(crate) ext: u32,
}

/// Text inputs, which the proxy bridges to the host's
/// `zwp_text_input_v1` and its extension.
impl Session {
    /// A new `zwp_text_input_v3` of `seat`, from a manager the guest binds
    /// on first use, when the host also advertises the newest text-input
    /// extension, which the proxy binds for itself. What the guest gets
    /// for it is kept: an `enter` if the seat's keyboard focus is on a
    /// surface.
    pub(crate) async fn text_input(&mut self, seat: u32) -> TextInput {
        use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1::{
            self, Request as Extension,
        };
        use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1::{
            self, Request as Manager,
        };
        use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
        let manager = match self.text_input_manager {
            Some(manager) => manager,
            None => {
                let version = zwp_text_input_manager_v1::VERSION;
                let manager = self.bind("zwp_text_input_manager_v1", version).await;
                let earlier = self.host.received.len();
                let version = zcr_text_input_extension_v1::VERSION;
                self.advertise(zcr_text_input_extension_v1::NAME, version)
                    .await;
                self.host.received.truncate(earlier);
                *self.text_input_manager.insert(manager)
            }
        };
        let id = self.new_id();
        let earlier = self.host.received.len();
        self.guest([zwp_text_input_manager_v3::req::get_text_input(
            manager, id, seat,
        )])
        .await;
        let forwarded = self.host.received.split_off(earlier);
        let [(_, AnyRequest::ZwpTextInputManagerV1(Manager::CreateTextInput { id: host })), (
            _,
            AnyRequest::ZcrTextInputExtensionV1(Extension::GetExtendedTextInput {
                id: ext,
                text_input,
            }),
        )] = forwarded[..]
        else {
            panic!("the host got no text input: {forwarded:?}");
        };
        assert_eq!(text_input, host);
        TextInput { id, host, ext }
    }

    /// The guest enables `text_input`, focused, with its commit `serial`,
    /// and the host activates it; the guest gets the commit's `done`. What
    /// the proxy sends the host besides the activation is not kept.
    pub(crate) async fn enable(&mut self, text_input: TextInput, serial: u32) {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::Request as V1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
        let id = text_input.id;
        let earlier = self.host.received.len();
        self.guest([
            zwp_text_input_v3::req::enable(id),
            zwp_text_input_v3::req::commit(id),
        ])
        .await;
        let forwarded = self.host.received.split_off(earlier);
        assert!(
            forwarded.iter().any(|request| matches!(
                request,
                (sender, AnyRequest::ZwpTextInputV1(V1::Activate { .. }))
                    if *sender == text_input.host
            )),
            "the host did not activate the text input: {forwarded:?}"
        );
        let done = zwp_text_input_v3::Event::Done { serial };
        assert_eq!(self.guest_received(), [(id, done.into())]);
    }
}

/// The default XKB keymap as a host sends it: its text and the NUL that
/// the size in `wl_keyboard.keymap` counts.
pub(crate) fn default_keymap() -> Vec<u8> {
    use xkbcommon::xkb;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
    let keymap = xkb::Keymap::new_from_names(&context, "", "", "", "", None, flags)
        .expect("the default keymap");
    let mut text = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    text.push(0);
    text
}

/// Globals the proxy binds for itself, and native buffers the host
/// creates.
impl Session {
    /// The host advertises global `interface` at `version`, which the
    /// proxy binds for itself; returns the global's name and the host
    /// object the proxy binds. What the guest gets for the global is kept,
    /// and so is what the proxy sends the host besides the bind.
    pub(crate) async fn advertise_internal(&mut self, interface: &str, version: u32) -> (u32, u32) {
        self.registry().await;
        let earlier = self.host.received.len();
        let name = self.advertise(interface, version).await;
        let bind = self.host.received[earlier..]
            .iter()
            .position(|(_, request)| {
                matches!(
                    request,
                    AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })
                        if id.0 == interface
                )
            })
            .unwrap_or_else(|| {
                let host = &self.host.received[earlier..];
                panic!("the proxy did not bind {interface}: {host:?}")
            });
        let (_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })) =
            self.host.received.remove(earlier + bind)
        else {
            unreachable!("the request found is a bind");
        };
        (name, id.2)
    }

    /// A new `width`x`height` XRGB8888 native `wl_buffer` that the host
    /// creates under `host`, its next server ID, for the guest's
    /// asynchronous `create` through its linux-dmabuf `dmabuf`, whose one
    /// plane is `plane` with rows of `4 * width` bytes. The guest gets the
    /// `created` event, and then destroys the params. What the proxy sends
    /// the host for it is not kept.
    pub(crate) async fn created_buffer(
        &mut self,
        dmabuf: u32,
        plane: OwnedFd,
        width: i32,
        height: i32,
        host: u32,
    ) -> Object {
        use crate::protocols::linux_dmabuf_v1::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
        const XRGB8888: u32 = 0x3432_5258;
        let earlier = self.host.received.len();
        let params = self.new_id();
        let host_params = self
            .create(zwp_linux_dmabuf_v1::req::create_params(dmabuf, params))
            .await;
        let stride = 4 * u32::try_from(width).expect("a width of at least 0");
        self.guest([
            zwp_linux_buffer_params_v1::req::add(params, plane, 0, 0, stride, 0, 0),
            zwp_linux_buffer_params_v1::req::create(params, width, height, XRGB8888, 0),
        ])
        .await;
        self.host([zwp_linux_buffer_params_v1::evt::created(host_params, host)])
            .await;
        let received = self.guest_received();
        let [(
            sender,
            AnyEvent::ZwpLinuxBufferParamsV1(zwp_linux_buffer_params_v1::Event::Created { buffer }),
        )] = received[..]
        else {
            panic!("the guest got no buffer: {received:?}");
        };
        assert_eq!(sender, params, "another params got the buffer");
        self.guest([zwp_linux_buffer_params_v1::req::destroy(params)])
            .await;
        self.host.received.truncate(earlier);
        Object { id: buffer, host }
    }
}

/// The guest's `gtk_shell1`, which the proxy serves itself in place of the
/// host's `zaura_shell` ([`Session::gtk_shell`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GtkShell {
    pub(crate) id: u32,
    /// The global the guest bound it from: the host's `zaura_shell`, on
    /// [`Session::registry`].
    pub(crate) name: u32,
    /// The host `zaura_shell` the proxy bound for itself.
    pub(crate) shell: u32,
}

/// Registries and their globals: the globals the host advertises, removes
/// and reuses on each of several registries, and the `gtk_shell1` the proxy
/// serves for the host's `zaura_shell`.
impl Session {
    /// The host advertises `zaura_shell` at `version`, which the proxy binds
    /// for itself, and the guest binds the `gtk_shell1` the proxy
    /// advertises in its place, which gets its capabilities once the host
    /// has answered the proxy's sync. Neither peer's messages are kept.
    pub(crate) async fn gtk_shell(&mut self, version: u32) -> GtkShell {
        use crate::protocols::aura_shell::zaura_shell;
        use crate::protocols::gtk::gtk_shell1;
        use crate::protocols::wayland::wl_callback;
        let registry = self.registry().await;
        let name = self.advertise(zaura_shell::NAME, version).await;
        let host = self.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: shell, .. }))] = &host[..]
        else {
            panic!("the proxy did not bind zaura_shell: {host:?}");
        };
        let shell = shell.2;
        let advertised = wl_registry::Event::Global {
            name,
            interface: gtk_shell1::NAME.to_string(),
            version: gtk_shell1::VERSION,
        };
        assert_eq!(self.guest_received(), [(registry.id, advertised.into())]);
        let id = self.new_id();
        self.guest([wl_registry::req::bind(
            registry.id,
            name,
            gtk_shell1::NAME,
            gtk_shell1::VERSION,
            id,
        )
        .unwrap()])
            .await;
        let host = self.host_received();
        let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            host[..]
        else {
            panic!("the proxy did not ask for gtk_shell1's capabilities: {host:?}");
        };
        self.host([
            wl_callback::evt::done(callback, 0),
            wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
        ])
        .await;
        let capabilities = gtk_shell1::Event::Capabilities { capabilities: 0 };
        assert_eq!(self.guest_received(), [(id, capabilities.into())]);
        GtkShell { id, name, shell }
    }

    /// A new `wl_registry` the guest creates, besides [`Self::registry`].
    pub(crate) async fn new_registry(&mut self) -> Object {
        self.get_registry().await
    }

    /// The host advertises global `name` of `interface` at `version` on
    /// `registry`; [`Self::advertise`] never picks `name` after. What the
    /// proxy sends either peer is kept.
    pub(crate) async fn global(
        &mut self,
        registry: Object,
        name: u32,
        interface: &str,
        version: u32,
    ) {
        self.next_global_name = self.next_global_name.max(name + 1);
        self.host([wl_registry::evt::global(registry.host, name, interface, version).unwrap()])
            .await;
    }

    /// The host removes global `name` from `registry`. What the proxy sends
    /// either peer is kept.
    pub(crate) async fn global_remove(&mut self, registry: Object, name: u32) {
        self.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
    }
}

/// Dispatch a batch at a time, and the proxy's own event loop: for the
/// proxy's tests of fair dispatch, of when output leaves, and of the loop
/// itself, which [`Self::send`] cannot show, as it dispatches one
/// direction and then sends everything. What either peer receives is
/// decoded and checked as always.
impl Session {
    /// The peer that sends in `direction` sends `messages`, which the proxy
    /// receives, as `Client::run` does, but does not dispatch yet.
    pub(super) fn queue(
        &mut self,
        direction: Direction,
        messages: impl IntoIterator<Item = Outgoing<u32>>,
    ) {
        for message in messages {
            match direction {
                Direction::ClientToHost => self.guest.wire.send(&message),
                Direction::HostToClient => self.host.wire.send(&message),
            }
            assert!(
                self.transmit(direction, message),
                "the proxy refuses what it receives"
            );
        }
    }

    /// The peer that sends in `direction` sends `messages`, which wait in
    /// its socket: the proxy has not received them yet.
    pub(super) fn write(
        &mut self,
        direction: Direction,
        messages: impl IntoIterator<Item = Outgoing<u32>>,
    ) {
        for message in messages {
            match direction {
                Direction::ClientToHost => self.guest.send(message),
                Direction::HostToClient => self.host.send(message),
            }
        }
    }

    /// The direction `Client::run` dispatches next when it prefers the
    /// host's (`prefer_host`) or the guest's, as it chooses it: it may
    /// receive from the preferred peer. `None` if neither has a whole
    /// message buffered.
    pub(super) fn next_direction(&mut self, prefer_host: bool) -> Option<Direction> {
        self.client
            .next_buffered_direction(prefer_host)
            .expect("both peers are open")
    }

    /// Dispatch one batch of what the proxy has received in `direction`, as
    /// `Client::run` does; returns whether the session goes on. It sends
    /// what a batch sends: what earlier batches left, before a batch in the
    /// other direction or after a few in the same one, and its own full
    /// submissions, but not the rest ([`Self::flush`]).
    pub(super) async fn batch(&mut self, direction: Direction) -> bool {
        let goes_on = self.client.handle_msgs(direction).await;
        self.guest.receive();
        self.host.receive();
        goes_on
    }

    /// Handle what other connections delivered through the clipboard hub,
    /// as `Client::run` does once its mailbox is ready; returns whether the
    /// session goes on.
    pub(crate) async fn deliver(&mut self) -> bool {
        let goes_on = self.client.handle_deliveries().await;
        self.guest.receive();
        self.host.receive();
        goes_on
    }

    /// Send all the batches left for either peer, as `Client::run` does
    /// before it waits; returns whether the session goes on.
    pub(super) async fn flush(&mut self) -> bool {
        let goes_on = self.client.flush().await;
        self.guest.receive();
        self.host.receive();
        goes_on
    }

    /// Wait until the guest request that waits for a fence is due, as
    /// `Client::run` does, without dispatching it.
    pub(super) async fn fence_due(&self) {
        assert!(self.waits_for_fence(), "no guest request waits for a fence");
        Parked::due(self.client.parked.as_ref()).await;
    }

    /// Run the proxy's own event loop, `Client::run`, beside `peers`, which
    /// plays the guest and the host through [`Running`]. Once `peers`
    /// returns, both close their connections, which ends the loop, and the
    /// session with it: an ended stand-in takes the proxy's place. What
    /// the proxy sent that `peers` did not take is kept.
    pub(crate) async fn run(&mut self, peers: impl AsyncFnOnce(&mut Running<'_>)) {
        let (_, guest_socket) = UnixStream::pair().expect("a guest socket pair");
        let (_, host_socket) = UnixStream::pair().expect("a host socket pair");
        let connection =
            |socket: UnixStream| WaylandConnection::new(socket.into()).expect("register a socket");
        let ctx = TestConnection::default().context();
        let stand_in = Client::new(connection(guest_socket), connection(host_socket), ctx);
        let client = std::mem::replace(&mut self.client, stand_in);
        self.client.ended = true;
        let mut running = Running {
            guest: &mut self.guest,
            host: &mut self.host,
        };
        let peers = async {
            peers(&mut running).await;
            running.guest.close();
            running.host.close();
        };
        tokio::join!(client.run(), peers);
        self.guest.receive_last();
        self.host.receive_last();
    }
}

/// Both peers of a session whose proxy runs its own event loop
/// ([`Session::run`]).
pub(crate) struct Running<'a> {
    guest: &'a mut Peer<AnyEvent>,
    host: &'a mut Peer<AnyRequest>,
}

impl Running<'_> {
    /// The host sends `events`.
    pub(crate) fn host(&mut self, events: impl IntoIterator<Item = Outgoing<u32>>) {
        events.into_iter().for_each(|event| self.host.send(event));
    }

    /// Wait until the proxy has sent the guest an event; returns what the
    /// guest has received.
    pub(crate) async fn guest_received(&mut self) -> Vec<(u32, AnyEvent)> {
        self.guest.next().await
    }

    /// Wait until the proxy has sent the host a request; returns what the
    /// host has received.
    pub(crate) async fn host_received(&mut self) -> Vec<(u32, AnyRequest)> {
        self.host.next().await
    }
}

impl Drop for Session {
    /// A test reads all the proxy sent, so that nothing it did not expect
    /// goes unnoticed.
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        assert!(
            self.guest.received.is_empty(),
            "the test did not read what the guest received: {:?}",
            self.guest.received
        );
        assert!(
            self.host.received.is_empty(),
            "the test did not read what the host received: {:?}",
            self.host.received
        );
    }
}

/// A peer of the proxy: its end of the connection, its objects and what
/// it received, decoded.
struct Peer<M> {
    connection: WaylandConnection,
    wire: wire_peer::Peer,
    received: Vec<(u32, M)>,
    /// The bytes it got, while the session records.
    bytes: Option<Vec<u8>>,
}

impl<M: Message> Peer<M> {
    fn new(connection: WaylandConnection, role: Role) -> Self {
        Self {
            connection,
            // The proxy reads each host event as the host sends it.
            wire: match role {
                Role::Client => wire_peer::Peer::new(role),
                Role::Server => wire_peer::Peer::new(role).in_step(),
            },
            received: Vec::new(),
            bytes: None,
        }
    }

    /// Write `bytes`, and `fds` with them, to the proxy in one send;
    /// returns how many bytes it took. A stream socket carries descriptors
    /// only with a byte: with none, the kernel sends nothing, and they are
    /// lost.
    fn write(&self, bytes: &[u8], fds: &[OwnedFd]) -> usize {
        use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
        use std::io::IoSlice;
        let socket = self
            .connection
            .unix_socket_for_test()
            .expect("a peer's connection is a Unix socket")
            .as_raw_fd();
        let raw: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let rights = [ControlMessage::ScmRights(&raw)];
        let cmsgs: &[ControlMessage] = if raw.is_empty() { &[] } else { &rights };
        let iov = [IoSlice::new(bytes)];
        sendmsg::<()>(socket, &iov, cmsgs, MsgFlags::MSG_NOSIGNAL, None).expect("send to the proxy")
    }

    /// Send `message` whole, and learn from it.
    fn send(&mut self, message: Outgoing<u32>) {
        self.wire.send(&message);
        let (bytes, fds) = message.into_parts();
        let mut sent = self.write(&bytes, &fds);
        while sent < bytes.len() {
            sent += self.write(&bytes[sent..], &[]);
        }
    }

    /// Wait until the proxy has sent this peer a message, and take all
    /// it has received.
    async fn next(&mut self) -> Vec<(u32, M)> {
        while self.received.is_empty() {
            let bytes = self
                .connection
                .recv()
                .await
                .expect("receive from the proxy");
            assert_ne!(bytes, 0, "the proxy closed the connection");
            self.receive();
        }
        std::mem::take(&mut self.received)
    }

    /// Close this peer's sending side, as a peer that goes away does.
    fn close(&self) {
        let socket = self
            .connection
            .unix_socket_for_test()
            .expect("a peer's connection is a Unix socket")
            .as_raw_fd();
        assert_eq!(unsafe { libc::shutdown(socket, libc::SHUT_WR) }, 0);
    }

    /// Decode every whole message the proxy has sent. Each must decode, and
    /// keep the protocol ([`wire_peer`]).
    ///
    /// # Panics
    ///
    /// On a message that breaks the protocol, a stream that cannot be
    /// framed, or anything left over once the proxy has sent all it holds:
    /// part of a message, or a descriptor no message took.
    fn receive(&mut self) {
        while self
            .connection
            .try_recv()
            .expect("receive from the proxy")
            .is_some_and(|bytes| bytes > 0)
        {}
        self.decode_received();
    }

    /// [`Self::receive`] once the proxy has closed its end, which resets
    /// the connection if the proxy left something this peer sent unread.
    fn receive_last(&mut self) {
        loop {
            match self.connection.try_recv() {
                Ok(Some(bytes)) if bytes > 0 => {}
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("receive from the proxy: {error}"),
            }
        }
        self.decode_received();
    }

    /// Decode every whole message received: see [`Self::receive`].
    fn decode_received(&mut self) {
        let peer = match self.wire.role() {
            Role::Client => "guest",
            Role::Server => "host",
        };
        let connection = &mut self.connection;
        let mut offset = 0;
        loop {
            let len = match MessageHeader::frame(&connection.read_buf[offset..]) {
                Framing::Complete(MessageHeader { len, .. }) => len,
                Framing::Partial => break,
                Framing::Invalid(header) => {
                    panic!("the proxy sent the {peer} a header it cannot frame: {header:?}")
                }
            };
            let message = &connection.read_buf[offset..offset + len];
            if let Some(bytes) = &mut self.bytes {
                bytes.extend_from_slice(message);
            }
            let decoded = self
                .wire
                .receive(message, &mut connection.read_fds)
                .unwrap_or_else(|violation| panic!("the proxy sent the {peer} {violation}"));
            self.received.push(decoded);
            offset += len;
        }
        connection.read_buf.drain(..offset);
        assert!(
            connection.read_buf.is_empty() && connection.read_fds.is_empty(),
            "the proxy left the {peer} part of a message ({} bytes) or descriptors no message \
             took ({})",
            connection.read_buf.len(),
            connection.read_fds.len()
        );
    }

    /// The ID and interface of the object `message`, which this peer is
    /// about to send, creates; `None` for a malformed one, which creates
    /// none.
    fn created<S: Message>(&self, outgoing: &Outgoing<u32>) -> Option<(u32, String)> {
        let (message, fds) = (outgoing.bytes(), outgoing.fds());
        let mut fds = fds
            .iter()
            .map(|fd| fd.try_clone().expect("duplicate a descriptor"))
            .collect::<VecDeque<_>>();
        let (_, decoded) = self.wire.decode::<S>(message, &mut fds).ok()?;
        decoded
            .new_object()
            .map(|(id, interface)| (id, interface.to_string()))
    }
}

/// SHM pools and buffers, which the proxy implements for the guest.
impl Session {
    /// A new `wl_shm_pool` of all of `file`, from the guest's `wl_shm`
    /// `shm`. The pool is the proxy's own, so the host gets nothing.
    pub(crate) async fn shm_pool(&mut self, shm: u32, file: std::fs::File) -> u32 {
        use crate::protocols::wayland::wl_shm;
        let size = file.metadata().expect("the pool file's size").len();
        let pool = self.new_id();
        let size = i32::try_from(size).expect("a pool of at most i32::MAX bytes");
        self.guest([wl_shm::req::create_pool(shm, pool, file.into(), size)])
            .await;
        assert_eq!(self.host_received(), [], "the pool is the proxy's own");
        pool
    }

    /// A new `width`x`height` XRGB8888 native `wl_buffer` that the guest
    /// creates through its linux-dmabuf `dmabuf`, whose one plane is
    /// `plane` with rows of `4 * width` bytes. What the proxy sends the
    /// host for it is not kept.
    pub(crate) async fn native_buffer(
        &mut self,
        dmabuf: u32,
        plane: OwnedFd,
        width: i32,
        height: i32,
    ) -> Object {
        use crate::protocols::linux_dmabuf_v1::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
        const XRGB8888: u32 = 0x3432_5258;
        let params = self.new_id();
        self.create(zwp_linux_dmabuf_v1::req::create_params(dmabuf, params))
            .await;
        let stride = 4 * u32::try_from(width).expect("a width of at least 0");
        self.guest([zwp_linux_buffer_params_v1::req::add(
            params, plane, 0, 0, stride, 0, 0,
        )])
        .await;
        let id = self.new_id();
        self.guest([
            zwp_linux_buffer_params_v1::req::create_immed(params, id, width, height, XRGB8888, 0),
            zwp_linux_buffer_params_v1::req::destroy(params),
        ])
        .await;
        let received = self.host_received();
        let host = received
            .iter()
            .find_map(|(_, request)| match request {
                AnyRequest::ZwpLinuxBufferParamsV1(
                    zwp_linux_buffer_params_v1::Request::CreateImmed { buffer_id, .. },
                ) => Some(*buffer_id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the host got no buffer: {received:?}"));
        Object { id, host }
    }

    /// A new `width`x`height` ARGB8888 `wl_buffer` from the start of
    /// `pool`, with rows of `4 * width` bytes. The host gets a buffer of
    /// its own, from a pool the proxy creates for it; what the proxy sends
    /// the host for it is not kept.
    pub(crate) async fn shm_buffer(&mut self, pool: u32, width: i32, height: i32) -> Object {
        use crate::protocols::wayland::{wl_shm, wl_shm_pool};
        let id = self.new_id();
        let earlier = self.host.received.len();
        self.guest([wl_shm_pool::req::create_buffer(
            pool,
            id,
            0,
            width,
            height,
            4 * width,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;
        let forwarded = self.host.received.split_off(earlier);
        let host = forwarded
            .iter()
            .find_map(|(_, request)| match request {
                AnyRequest::WlShmPool(wl_shm_pool::Request::CreateBuffer { id, .. }) => Some(*id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the host got no buffer: {forwarded:?}"));
        Object { id, host }
    }

    /// [`Self::shm_buffer`], which also returns the file of the pool the
    /// proxy creates for the host's buffer: the host copy, into which the
    /// proxy copies the guest's pixels before a commit shows them.
    pub(crate) async fn shm_buffer_with_host_pool(
        &mut self,
        pool: u32,
        width: i32,
        height: i32,
    ) -> (Object, std::fs::File) {
        use crate::protocols::wayland::{wl_shm, wl_shm_pool};
        let id = self.new_id();
        let earlier = self.host.received.len();
        self.guest([wl_shm_pool::req::create_buffer(
            pool,
            id,
            0,
            width,
            height,
            4 * width,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;
        let forwarded = self.host.received.split_off(earlier);
        let host_pool = forwarded.iter().find_map(|(_, request)| match request {
            AnyRequest::WlShm(wl_shm::Request::CreatePool { fd, .. }) => Some(fd),
            _ => None,
        });
        let host = forwarded.iter().find_map(|(_, request)| match request {
            AnyRequest::WlShmPool(wl_shm_pool::Request::CreateBuffer { id, .. }) => Some(*id),
            _ => None,
        });
        let (Some(host_pool), Some(host)) = (host_pool, host) else {
            panic!("the host got no pool and buffer: {forwarded:?}");
        };
        let host_pool = host_pool.try_clone().expect("duplicate the host pool");
        (Object { id, host }, host_pool.into())
    }
}
