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
//! each peer learns an object's interface from the message that creates
//! it, so a test names objects by their IDs on the wire, learns host IDs
//! from the forwarded requests and never reads the object table. Every
//! batch that keeps the session checks that the proxy holds no state for an
//! ended object (`Context::assert_no_orphans`).

use super::{Client, Direction, ParkedRequest};
use crate::connection::WaylandConnection;
use crate::protocols::wayland::{wl_compositor, wl_display, wl_registry};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::state::Context;
use crate::test_support::decode;
use crate::wire::{Framing, MessageHeader, Outgoing, ProtocolError, WireMessage, WL_DISPLAY_ID};
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// A guest object and the host object the proxy paired it with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Object {
    pub(crate) id: u32,
    pub(crate) host: u32,
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
}

impl Session {
    /// A session whose proxy has a plain test context.
    pub(crate) fn new() -> Self {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (guest, guest_socket) = UnixStream::pair().expect("a guest socket pair");
        let (host, host_socket) = UnixStream::pair().expect("a host socket pair");
        let connection =
            |socket: UnixStream| WaylandConnection::new(socket.into()).expect("register a socket");
        Self {
            client: Client::new(connection(guest_socket), connection(host_socket), ctx),
            guest: Peer::new(connection(guest)),
            host: Peer::new(connection(host)),
            next_id: WL_DISPLAY_ID + 1,
            next_global_name: 1,
            registry: None,
            compositor: None,
        }
    }

    /// The proxy's state: `ctx()` reads state no peer can observe, or
    /// injects a fault no peer can cause; each use says which, in a
    /// comment.
    pub(crate) fn ctx(&mut self) -> &mut Context {
        &mut self.client.ctx
    }

    /// The guest sends `requests`, which must not end the session.
    pub(crate) async fn guest(&mut self, requests: impl IntoIterator<Item = Outgoing>) {
        assert!(
            self.send(Direction::ClientToHost, requests).await,
            "the guest's requests ended the session"
        );
    }

    /// The guest sends `requests`, which must end the session.
    pub(crate) async fn guest_fatal(&mut self, requests: impl IntoIterator<Item = Outgoing>) {
        assert!(
            !self.send(Direction::ClientToHost, requests).await,
            "the guest's requests did not end the session"
        );
    }

    /// The host sends `events`, which must not end the session.
    pub(crate) async fn host(&mut self, events: impl IntoIterator<Item = Outgoing>) {
        assert!(
            self.send(Direction::HostToClient, events).await,
            "the host's events ended the session"
        );
    }

    /// The host sends `events`, which must end the session.
    pub(crate) async fn host_fatal(&mut self, events: impl IntoIterator<Item = Outgoing>) {
        assert!(
            !self.send(Direction::HostToClient, events).await,
            "the host's events did not end the session"
        );
    }

    /// Make every later send to the guest fail, as when its connection
    /// breaks. Closing the guest's end would not do: a process another test
    /// spawns may hold a copy of it.
    pub(crate) fn break_guest_connection(&self) {
        let socket = self.client.client_conn.as_raw_fd();
        assert_eq!(unsafe { libc::shutdown(socket, libc::SHUT_WR) }, 0);
    }

    /// Whether a guest request waits for a fence, holding back the guest's
    /// stream.
    pub(crate) fn parked(&self) -> bool {
        self.client.parked.is_some()
    }

    /// Wait until the parked guest request is due, as `Client::run` does,
    /// and dispatch the guest's stream from it, which must not end the
    /// session.
    pub(crate) async fn resume(&mut self) {
        assert!(self.parked(), "no guest request waits for a fence");
        ParkedRequest::due(self.client.parked.as_ref()).await;
        self.guest([]).await;
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

    /// The guest sends `request`, which creates one object; returns the host
    /// ID of the object its forward creates. The forward is not kept for
    /// [`Self::host_received`].
    pub(crate) async fn create(&mut self, request: Outgoing) -> u32 {
        let (_, interface) = self
            .guest
            .objects
            .learn::<AnyRequest>(&request)
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

    /// The guest's `wl_registry`, which it creates on first use.
    pub(crate) async fn registry(&mut self) -> Object {
        if let Some(registry) = self.registry {
            return registry;
        }
        let id = self.new_id();
        let host = self
            .create(wl_display::req::get_registry(WL_DISPLAY_ID, id))
            .await;
        *self.registry.insert(Object { id, host })
    }

    /// Queue `messages` on the proxy's connection to their sender and
    /// dispatch them as `Client::run` does, up to a guest request that
    /// waits for a fence ([`Self::resume`]); returns whether the session
    /// goes on.
    async fn send(
        &mut self,
        direction: Direction,
        messages: impl IntoIterator<Item = Outgoing>,
    ) -> bool {
        for message in messages {
            match direction {
                Direction::ClientToHost => self.guest.objects.learn::<AnyRequest>(&message),
                Direction::HostToClient => self.host.objects.learn::<AnyEvent>(&message),
            };
            let (bytes, fds) = message;
            let connection = self.client.connection_mut(direction);
            connection.read_buf.extend(bytes);
            connection.read_fds.extend(fds);
        }
        loop {
            let goes_on = self.client.handle_msgs(direction).await;
            self.guest.receive();
            self.host.receive();
            let parked = direction == Direction::ClientToHost && self.client.parked.is_some();
            if !goes_on || parked || !self.client.connection(direction).has_complete_message() {
                return goes_on;
            }
        }
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

/// A peer of the proxy: its end of the connection, the objects it knows
/// and what it received, decoded.
struct Peer<M> {
    connection: WaylandConnection,
    objects: Objects,
    received: Vec<(u32, M)>,
}

impl<M: Message> Peer<M> {
    fn new(connection: WaylandConnection) -> Self {
        Self {
            connection,
            objects: Objects(HashMap::from([(WL_DISPLAY_ID, "wl_display".to_string())])),
            received: Vec::new(),
        }
    }

    /// Decode every whole message the proxy has sent. Each must decode, as
    /// sent by an object the peer knows.
    fn receive(&mut self) {
        while self
            .connection
            .try_recv()
            .expect("receive from the proxy")
            .is_some_and(|bytes| bytes > 0)
        {}
        let connection = &mut self.connection;
        let mut offset = 0;
        while let Framing::Complete(MessageHeader { len, .. }) =
            MessageHeader::frame(&connection.read_buf[offset..])
        {
            let message = &connection.read_buf[offset..offset + len];
            let decoded = self
                .objects
                .decode(message, &mut connection.read_fds)
                .unwrap_or_else(|error| panic!("the proxy sent {message:?}: {error}"));
            self.received.push(decoded);
            offset += len;
        }
        connection.read_buf.drain(..offset);
    }
}

/// The interface of each object a peer knows, by ID.
struct Objects(HashMap<u32, String>);

impl Objects {
    /// Decode `message`, sent by an object this peer knows, and learn the
    /// object it creates.
    fn decode<M: Message>(
        &mut self,
        message: &[u8],
        fds: &mut VecDeque<OwnedFd>,
    ) -> Result<(u32, M), ProtocolError> {
        let decoded = decode(message, fds, |msg| {
            let interface = self
                .0
                .get(&msg.sender_id)
                .ok_or(ProtocolError::InvalidSender(msg.sender_id))?;
            M::from_wire(interface, msg)
        })?;
        if let Some((id, interface)) = decoded.1.new_object() {
            self.0.insert(id, interface.to_string());
        }
        Ok(decoded)
    }

    /// Learn the object `message`, which this peer sends, creates, and
    /// return its ID and interface. A malformed message creates none; the
    /// proxy reports it.
    fn learn<M: Message>(&mut self, (message, fds): &Outgoing) -> Option<(u32, String)> {
        let mut fds = fds
            .iter()
            .map(|fd| fd.try_clone().expect("duplicate a descriptor"))
            .collect();
        let (_, decoded) = self.decode::<M>(message, &mut fds).ok()?;
        decoded
            .new_object()
            .map(|(id, interface)| (id, interface.to_string()))
    }
}

/// A message of any interface, as [`Objects`] decodes it.
trait Message: Sized {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError>;
    fn new_object(&self) -> Option<(u32, &str)>;
}

impl Message for AnyRequest {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
        AnyRequest::from_wire(interface, msg)
    }

    fn new_object(&self) -> Option<(u32, &str)> {
        AnyRequest::new_object(self)
    }
}

impl Message for AnyEvent {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
        AnyEvent::from_wire(interface, msg)
    }

    fn new_object(&self) -> Option<(u32, &str)> {
        AnyEvent::new_object(self)
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
}
