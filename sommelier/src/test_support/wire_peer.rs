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

//! One end of a Wayland connection as a test plays it: the objects that end
//! has, tracked as libwayland tracks them, and the rules every message it
//! receives must keep.
//!
//! A [`Peer`] plays a client or a server. It learns from each message it
//! sends, without judging it: a test may send anything, malformed or not,
//! to see what the proxy makes of it. It checks each message it receives,
//! which the proxy sent, against what it has sent and received so far:
//!
//! - the sender, and every object argument, is an object the peer has,
//!   of the declared interface, and one the sending side has not ended:
//!   a message can cross the other side's destroy of an object, never its
//!   sender's own; a null object only where the argument may be null;
//! - the message is no newer than its sender's version, which an object
//!   takes from the bind that created it, or else from its parent;
//! - a new object takes an ID from its creator's range (the server's from
//!   `0xff000000`) that is free on both sides, and no further than one past
//!   the highest ID that range has used, as libwayland's object map
//!   requires;
//! - a `wl_display.delete_id` names, once, a client object that a
//!   destructor ended, a frame callback whose surface has ended, as a
//!   compositor ends those with their surface, or another live object
//!   the server may end on its own ([`Peer::server_may_end`]: none by
//!   default); its ID is free again once the client has destroyed its
//!   object too;
//! - for a server that hands each message to the client as it sends it
//!   ([`Peer::in_step`]), no request comes from, or names, an object after
//!   the server ended it.
//!
//! The proxy is the client of the host and the server of the guest, so a
//! test's guest is a [`Role::Client`] peer and its host a [`Role::Server`]
//! one. The two peers of `proxy::harness::Session`, and the host display
//! the DST and the fuzzer share (`sim::display`), each hold one.

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::OwnedFd;

use crate::protocols::wayland::{wl_display, wl_fixes, wl_registry, wl_surface};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::wire::{Framing, MessageHeader, Outgoing, ProtocolError, WireMessage, WL_DISPLAY_ID};

/// The first ID of the range a server allocates (`WL_SERVER_ID_START`).
pub(crate) const SERVER_ID_START: u32 = 0xff00_0000;

/// Which end of the connection a peer plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Sends requests and receives events.
    Client,
    /// Sends events and receives requests.
    Server,
}

/// Where an object is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    Live,
    /// A destructor ended it: a request the client sent, or an event the
    /// server sent, as `by` says. The side that ended it sends nothing more
    /// from it, nor names it; the other side's messages can cross the
    /// destructor. A client object's ID is free once its `delete_id`
    /// comes, a server object's at once.
    Destroyed {
        by: Role,
    },
    /// The server ended it on its own and sent its `delete_id`. The client
    /// can send requests that crossed it; the server can send nothing
    /// more. Its ID is free.
    Deleted,
}

#[derive(Clone, Debug)]
struct Object {
    interface: String,
    version: u32,
    life: Life,
    /// Which object this is, of all those the peer has had: an ID can
    /// name several in turn.
    serial: u64,
    /// For a frame callback, its surface and the surface's serial.
    frame_of: Option<(u32, u64)>,
}

/// Which live objects, by ID and interface, a server may end on its own
/// ([`Peer::server_may_end`]).
pub(crate) type MayEnd = Box<dyn Fn(u32, &str) -> bool + Send>;

/// One end of a connection; see the module documentation.
pub(crate) struct Peer {
    role: Role,
    objects: BTreeMap<u32, Object>,
    /// How many IDs of each range, client and server, the map spans: the
    /// next fresh ID's index.
    spans: [u32; 2],
    /// Which live object, by ID and interface, the server may end on its
    /// own, with a `delete_id` the client did not ask for.
    server_may_end: MayEnd,
    /// What this peer sent that is not a whole message yet, and the
    /// descriptors sent with it, which the messages take in order.
    unsent: Vec<u8>,
    unsent_fds: VecDeque<OwnedFd>,
    /// The stream this peer sends can no longer be framed.
    unframed: bool,
    /// The client reads what this server sends before it sends anything
    /// more ([`Peer::in_step`]).
    in_step: bool,
    /// The serial of the next object.
    serials: u64,
}

impl Peer {
    /// A peer that plays `role`, with only `wl_display`.
    pub(crate) fn new(role: Role) -> Self {
        let display = Object {
            interface: wl_display::NAME.to_owned(),
            version: 1,
            life: Life::Live,
            serial: 0,
            frame_of: None,
        };
        Self {
            role,
            objects: BTreeMap::from([(WL_DISPLAY_ID, display)]),
            // 0 is null and 1 the display.
            spans: [WL_DISPLAY_ID + 1, 0],
            server_may_end: Box::new(|_, _| false),
            serials: 1,
            unsent: Vec::new(),
            unsent_fds: VecDeque::new(),
            unframed: false,
            in_step: false,
        }
    }

    /// A server that learns what it sends just as the client reads it, as
    /// a test that hands the proxy each message does: no request can cross
    /// its `delete_id`s, so the client may not send or name an object once
    /// the server has ended it. (A client in step gains nothing: a server
    /// may name an object after the client destroyed it, as in a `leave`
    /// of a destroyed surface.)
    pub(crate) fn in_step(self) -> Self {
        Self {
            in_step: true,
            ..self
        }
    }

    /// The end of the connection the peer plays.
    pub(crate) fn role(&self) -> Role {
        self.role
    }

    /// Let the server end on its own the live objects for which
    /// `may_end(id, interface)` holds, besides the frame callbacks of a
    /// surface that ended, as a host ends objects it shares with the
    /// proxy. By default it may end no other.
    pub(crate) fn server_may_end(&mut self, may_end: impl Fn(u32, &str) -> bool + Send + 'static) {
        self.server_may_end = Box::new(may_end);
    }

    /// Have `id`, a live object of `interface` at `version` that no
    /// message created: one a test gave the proxy directly.
    pub(crate) fn adopt(&mut self, id: u32, interface: &str, version: u32) {
        let range = usize::from(id >= SERVER_ID_START);
        let index = id - [0, SERVER_ID_START][range];
        self.spans[range] = self.spans[range].max(index + 1);
        let object = Object {
            interface: interface.to_owned(),
            version,
            life: Life::Live,
            serial: self.serials,
            frame_of: None,
        };
        self.serials += 1;
        self.objects.insert(id, object);
    }

    /// The interface of `id`, if the peer has it, live or not.
    pub(crate) fn interface(&self, id: u32) -> Option<&str> {
        self.objects
            .get(&id)
            .map(|object| object.interface.as_str())
    }

    /// The interface of `id`, if it is a live object.
    pub(crate) fn live_interface(&self, id: u32) -> Option<&str> {
        let object = self.objects.get(&id)?;
        (object.life == Life::Live).then_some(object.interface.as_str())
    }

    /// Whether the server ended `id` on its own, and the client has not
    /// used its ID again.
    pub(crate) fn deleted(&self, id: u32) -> bool {
        self.objects
            .get(&id)
            .is_some_and(|object| object.life == Life::Deleted)
    }

    /// The live objects of `interface`, in ID order.
    pub(crate) fn live(&self, interface: &str) -> Vec<u32> {
        let live = |object: &Object| object.life == Life::Live && object.interface == interface;
        self.objects
            .iter()
            .filter(|(_, object)| live(object))
            .map(|(&id, _)| id)
            .collect()
    }

    /// A free server ID for an object this server creates: the lowest one
    /// free, else the next fresh one.
    pub(crate) fn free_server_id(&self) -> u32 {
        (SERVER_ID_START..SERVER_ID_START + self.spans[1])
            .find(|&id| self.reusable(id))
            .unwrap_or(SERVER_ID_START + self.spans[1])
    }

    /// Decode `message`, sent by an object this peer has, by its sender's
    /// interface.
    pub(crate) fn decode<M: Message>(
        &self,
        message: &[u8],
        fds: &mut VecDeque<OwnedFd>,
    ) -> Result<(u32, M), ProtocolError> {
        super::decode(message, fds, |msg| {
            let object = self
                .objects
                .get(&msg.sender_id)
                .ok_or(ProtocolError::InvalidSender(msg.sender_id))?;
            M::from_wire(&object.interface, msg)
        })
    }

    /// Learn from `bytes` and their descriptors, which this peer sends and
    /// may hold any part of a message, or several: each message they
    /// complete creates and ends what it does. A message that does not
    /// decode is skipped, and a stream that cannot be framed is not read
    /// further.
    pub(crate) fn send(&mut self, outgoing: &Outgoing<u32>) {
        let (bytes, fds) = (outgoing.bytes(), outgoing.fds());
        self.unsent.extend_from_slice(bytes);
        let fds = fds
            .iter()
            .map(|fd| fd.try_clone().expect("duplicate a sent descriptor"));
        self.unsent_fds.extend(fds);
        self.learn_unsent();
    }

    fn learn_unsent(&mut self) {
        let mut offset = 0;
        while !self.unframed {
            let len = match MessageHeader::frame(&self.unsent[offset..]) {
                Framing::Complete(header) => header.len,
                Framing::Partial => break,
                Framing::Invalid(_) => {
                    self.unframed = true;
                    break;
                }
            };
            let message = self.unsent[offset..offset + len].to_vec();
            let mut fds = std::mem::take(&mut self.unsent_fds);
            match self.role {
                Role::Client => {
                    if let Ok((sender, request)) = self.decode::<AnyRequest>(&message, &mut fds) {
                        let _ = self.request(sender, &request);
                    }
                }
                Role::Server => {
                    if let Ok((sender, event)) = self.decode::<AnyEvent>(&message, &mut fds) {
                        let _ = self.event(sender, &event);
                    }
                }
            }
            self.unsent_fds = fds;
            offset += len;
        }
        self.unsent.drain(..offset);
    }

    /// Decode `message`, which the other side sent this peer, check it and
    /// learn from it.
    ///
    /// # Errors
    ///
    /// What breaks a rule (see the module documentation), with the
    /// message: the other side's fault, not the peer's.
    pub(crate) fn receive<M: Message>(
        &mut self,
        message: &[u8],
        fds: &mut VecDeque<OwnedFd>,
    ) -> Result<(u32, M), String> {
        let (sender, decoded) = self
            .decode::<M>(message, fds)
            .map_err(|error| format!("{message:?} does not decode: {error}"))?;
        decoded
            .apply(sender, self)
            .map_err(|violation| format!("{sender} {decoded:?}: {violation}"))?;
        Ok((sender, decoded))
    }

    /// A request `sender` sent: one the client sends, or one the server
    /// receives and checks.
    ///
    /// # Returns
    ///
    /// The client object the request destroyed, if any, whose `delete_id`
    /// the server owes.
    ///
    /// # Errors
    ///
    /// For a server, what breaks a rule.
    pub(crate) fn request(
        &mut self,
        sender: u32,
        request: &AnyRequest,
    ) -> Result<Option<u32>, String> {
        let received = self.role == Role::Server;
        let version = match request {
            AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }) => Some(id.1),
            _ => None,
        };
        let new_object = request.new_object();
        let new_object = new_object.map(|(id, interface)| (id, interface, version));
        let shape = Shape {
            sender,
            since: request.since(),
            objects: request.object_args(),
            new_object,
        };
        let mut checks = Checks::new(received);
        self.follow(&shape, Role::Client, &mut checks);
        let mut owed = None;
        if request.is_destructor() {
            owed = self.destroy(sender, Role::Client);
        }
        match *request {
            // `wl_fixes.destroy_registry` destroys the registry it names.
            AnyRequest::WlFixes(wl_fixes::Request::DestroyRegistry { registry }) => {
                owed = self.destroy(registry, Role::Client);
            }
            AnyRequest::WlSurface(wl_surface::Request::Frame { callback }) => {
                let surface = self.objects.get(&sender).map(|surface| surface.serial);
                if let Some(callback) = self.objects.get_mut(&callback) {
                    callback.frame_of = surface.map(|serial| (sender, serial));
                }
            }
            _ => {}
        }
        checks.result().map(|()| owed)
    }

    /// An event `sender` sent: one the server sends, or one the client
    /// receives and checks.
    ///
    /// # Errors
    ///
    /// For a client, what breaks a rule.
    pub(crate) fn event(&mut self, sender: u32, event: &AnyEvent) -> Result<(), String> {
        let received = self.role == Role::Client;
        let shape = Shape {
            sender,
            since: event.since(),
            objects: event.object_args(),
            new_object: event
                .new_object()
                .map(|(id, interface)| (id, interface, None)),
        };
        let mut checks = Checks::new(received);
        self.follow(&shape, Role::Server, &mut checks);
        if event.is_destructor() {
            self.destroy(sender, Role::Server);
        }
        if let AnyEvent::WlDisplay(wl_display::Event::DeleteId { id }) = *event {
            self.delete(id, &mut checks);
        }
        checks.result()
    }

    /// Check `shape`, which `from` sent, and create its new object.
    fn follow(&mut self, shape: &Shape<'_>, from: Role, checks: &mut Checks) {
        let Some(sender) = self.objects.get(&shape.sender).cloned() else {
            return checks.fail(format!("its sender {} is no object", shape.sender));
        };
        checks.check(!self.ended_by(&sender, from), || {
            format!(
                "its sender {} ({}) is {:?}",
                shape.sender, sender.interface, sender.life
            )
        });
        checks.check(shape.since <= sender.version, || {
            format!("{} has version {}", sender.interface, sender.version)
        });
        for &(id, declared, nullable) in &shape.objects {
            if id == 0 {
                checks.check(nullable, || "it names null for an object".to_owned());
                continue;
            }
            let Some(object) = self.objects.get(&id) else {
                checks.fail(format!("it names {id}, which is no object"));
                continue;
            };
            checks.check(!self.ended_by(object, from), || {
                format!(
                    "it names {id} ({}), which is {:?}",
                    object.interface, object.life
                )
            });
            checks.check(
                declared.is_none_or(|declared| declared == object.interface),
                || format!("it names {id}, a {}, as a {declared:?}", object.interface),
            );
        }
        let Some((id, interface, version)) = shape.new_object else {
            return;
        };
        let range = usize::from(id >= SERVER_ID_START);
        let creator = [Role::Client, Role::Server][range];
        let in_range = creator == from && id > WL_DISPLAY_ID;
        checks.check(in_range, || {
            format!("it creates {id:#x}, outside its sender's range")
        });
        let reusable = self.reusable(id);
        checks.check(reusable, || {
            format!("it creates {id:#x}, which is still in use")
        });
        // What a peer sends under an ID it cannot use creates nothing: the
        // other side refuses it.
        if !(in_range && reusable) {
            return;
        }
        let index = id - [0, SERVER_ID_START][range];
        checks.check(index <= self.spans[range], || {
            format!("it creates {id:#x}, past the next fresh ID")
        });
        self.spans[range] = self.spans[range].max(index + 1);
        let object = Object {
            interface: interface.to_owned(),
            version: version.unwrap_or(sender.version),
            life: Life::Live,
            serial: self.serials,
            frame_of: None,
        };
        self.serials += 1;
        self.objects.insert(id, object);
    }

    /// Whether the side that sends `from` has ended `object`, so that it
    /// can no longer send it or name it.
    fn ended_by(&self, object: &Object, from: Role) -> bool {
        match object.life {
            Life::Live => false,
            // A client in step has read the server's destructor.
            Life::Destroyed { by } => from == by || (self.in_step && from == Role::Client),
            Life::Deleted => from == Role::Server || self.in_step,
        }
    }

    /// Whether `id` is free for a new object of its range.
    fn reusable(&self, id: u32) -> bool {
        match self.objects.get(&id).map(|object| object.life) {
            None | Some(Life::Deleted) => true,
            Some(Life::Live) => false,
            // A client object's ID waits for its `delete_id`.
            Some(Life::Destroyed { .. }) => id >= SERVER_ID_START,
        }
    }

    /// A destructor `by` one side ends `id`; returns it if it is a client
    /// object whose `delete_id` is now owed.
    fn destroy(&mut self, id: u32, by: Role) -> Option<u32> {
        let object = self.objects.get_mut(&id)?;
        match object.life {
            Life::Live => {
                object.life = Life::Destroyed { by };
                (id < SERVER_ID_START).then_some(id)
            }
            // Its `delete_id` crossed the destructor.
            Life::Deleted => {
                self.objects.remove(&id);
                None
            }
            Life::Destroyed { .. } => None,
        }
    }

    /// The server's `delete_id` of `id`.
    fn delete(&mut self, id: u32, checks: &mut Checks) {
        let Some(object) = self.objects.get_mut(&id) else {
            return checks.fail(format!("{id} is no object"));
        };
        if id >= SERVER_ID_START {
            return checks.fail(format!("{id:#x} is a server object, which gets none"));
        }
        match object.life {
            Life::Destroyed { .. } => {
                self.objects.remove(&id);
            }
            Life::Live => {
                let interface = object.interface.clone();
                let frame_of = object.frame_of;
                object.life = Life::Deleted;
                // A compositor ends a surface's frame callbacks with it.
                let surface_ended = frame_of.is_some_and(|(surface, serial)| {
                    let surface = self.objects.get(&surface);
                    surface.is_none_or(|surface| {
                        surface.serial != serial || surface.life != Life::Live
                    })
                });
                let may_end = surface_ended || (self.server_may_end)(id, &interface);
                checks.check(may_end, || {
                    format!("{id} ({interface}) is live, and the server may not end it")
                });
            }
            Life::Deleted => checks.fail(format!("{id} ({}) got one already", object.interface)),
        }
    }
}

/// What a message does to the objects, besides ending them.
struct Shape<'m> {
    sender: u32,
    since: u32,
    objects: Vec<(u32, Option<&'static str>, bool)>,
    /// The object it creates, its interface, and its version if it says.
    new_object: Option<(u32, &'m str, Option<u32>)>,
}

/// The rules a message breaks, if the peer received it.
struct Checks {
    received: bool,
    broken: Vec<String>,
}

impl Checks {
    fn new(received: bool) -> Self {
        Self {
            received,
            broken: Vec::new(),
        }
    }

    fn check(&mut self, holds: bool, rule: impl FnOnce() -> String) {
        if !holds && self.received {
            self.broken.push(rule());
        }
    }

    fn fail(&mut self, rule: String) {
        self.check(false, || rule);
    }

    fn result(self) -> Result<(), String> {
        match self.broken.is_empty() {
            true => Ok(()),
            false => Err(self.broken.join("; ")),
        }
    }
}

/// A message of any interface, as a [`Peer`] decodes and follows it.
pub(crate) trait Message: Sized + std::fmt::Debug {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError>;

    /// The ID and interface of the object it creates.
    fn new_object(&self) -> Option<(u32, &str)>;

    /// Follow the message, which `sender` sent, on `peer`.
    fn apply(&self, sender: u32, peer: &mut Peer) -> Result<(), String>;
}

impl Message for AnyRequest {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
        AnyRequest::from_wire(interface, msg)
    }

    fn new_object(&self) -> Option<(u32, &str)> {
        AnyRequest::new_object(self)
    }

    fn apply(&self, sender: u32, peer: &mut Peer) -> Result<(), String> {
        peer.request(sender, self).map(drop)
    }
}

impl Message for AnyEvent {
    fn from_wire(interface: &str, msg: &mut WireMessage) -> Result<Self, ProtocolError> {
        AnyEvent::from_wire(interface, msg)
    }

    fn new_object(&self) -> Option<(u32, &str)> {
        AnyEvent::new_object(self)
    }

    fn apply(&self, sender: u32, peer: &mut Peer) -> Result<(), String> {
        peer.event(sender, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::{
        wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_keyboard, wl_seat,
        wl_surface,
    };

    /// A client that has bound `wl_seat` at `version` as 3, from registry
    /// 2, and got keyboard 4 from it.
    fn client_with_keyboard(version: u32) -> Peer {
        let mut client = Peer::new(Role::Client);
        let sent: [(u32, AnyRequest); 3] = [
            (1, wl_display::Request::GetRegistry { registry: 2 }.into()),
            (
                2,
                wl_registry::Request::Bind {
                    name: 1,
                    id: (wl_seat::NAME.to_owned(), version, 3),
                }
                .into(),
            ),
            (3, wl_seat::Request::GetKeyboard { id: 4 }.into()),
        ];
        for (sender, request) in &sent {
            client.request(*sender, request).expect("a client sends");
        }
        client
    }

    fn key() -> AnyEvent {
        wl_keyboard::Event::Key {
            serial: 1,
            time: 0,
            key: 30,
            state: wl_keyboard::KEY_STATE_PRESSED,
        }
        .into()
    }

    fn delete_id(id: u32) -> AnyEvent {
        wl_display::Event::DeleteId { id }.into()
    }

    /// The review's case: a peer that never forgot an object took events
    /// from it after its `delete_id`.
    #[test]
    fn an_event_may_cross_its_objects_destroy_but_not_its_delete_id() {
        let mut client = client_with_keyboard(7);
        let release = wl_keyboard::Request::Release {}.into();
        assert_eq!(client.request(4, &release), Ok(Some(4)));
        assert_eq!(client.event(4, &key()), Ok(()), "crossed the release");
        assert_eq!(client.event(1, &delete_id(4)), Ok(()));
        let error = client.event(4, &key()).expect_err("from no object");
        assert!(error.contains("sender 4 is no object"), "{error}");
        let error = client.event(1, &delete_id(4)).expect_err("a second one");
        assert!(error.contains("4 is no object"), "{error}");
    }

    #[test]
    fn the_server_ends_a_live_object_once_and_only_as_it_may() {
        let mut client = client_with_keyboard(7);
        client.server_may_end(|_, interface| interface == wl_keyboard::NAME);
        let error = client.event(1, &delete_id(3)).expect_err("a seat");
        assert!(error.contains("may not end"), "{error}");
        assert_eq!(client.event(1, &delete_id(4)), Ok(()));
        assert!(client.deleted(4));
        let error = client.event(4, &key()).expect_err("ended by the server");
        assert!(error.contains("Deleted"), "{error}");
        let error = client.event(1, &delete_id(4)).expect_err("a second one");
        assert!(error.contains("got one already"), "{error}");
        // The client may still send from it, and then use its ID again.
        let release = wl_keyboard::Request::Release {}.into();
        assert_eq!(client.request(4, &release), Ok(None));
        let keyboard = wl_seat::Request::GetKeyboard { id: 4 }.into();
        client.request(3, &keyboard).expect("a client sends");
        assert_eq!(client.event(4, &key()), Ok(()));
    }

    #[test]
    fn a_message_newer_than_its_object_breaks_the_protocol() {
        let mut client = client_with_keyboard(3);
        let repeat_info = wl_keyboard::Event::RepeatInfo {
            rate: 25,
            delay: 600,
        }
        .into();
        let error = client.event(4, &repeat_info).expect_err("since 4");
        assert!(error.contains("wl_keyboard has version 3"), "{error}");
        let mut client = client_with_keyboard(4);
        assert_eq!(client.event(4, &repeat_info), Ok(()));
    }

    #[test]
    fn a_new_object_takes_a_free_id_from_its_creators_range() {
        let mut client = client_with_keyboard(7);
        let bind = wl_registry::Request::Bind {
            name: 2,
            id: (wl_data_device_manager::NAME.to_owned(), 3, 5),
        };
        client.request(2, &bind.into()).expect("a client sends");
        let device = wl_data_device_manager::Request::GetDataDevice { id: 6, seat: 3 };
        client.request(5, &device.into()).expect("a client sends");
        let offer = |id| wl_data_device::Event::DataOffer { id }.into();
        let error = client.event(6, &offer(7)).expect_err("a client ID");
        assert!(error.contains("outside its sender's range"), "{error}");
        let error = client
            .event(6, &offer(SERVER_ID_START + 1))
            .expect_err("a gap");
        assert!(error.contains("past the next fresh ID"), "{error}");
        assert_eq!(client.event(6, &offer(SERVER_ID_START)), Ok(()));
        let error = client
            .event(6, &offer(SERVER_ID_START))
            .expect_err("in use");
        assert!(error.contains("still in use"), "{error}");
        // A server object the client destroys is free at once.
        let destroy = crate::protocols::wayland::wl_data_offer::Request::Destroy {};
        assert_eq!(client.request(SERVER_ID_START, &destroy.into()), Ok(None));
        assert_eq!(client.event(6, &offer(SERVER_ID_START)), Ok(()));
    }

    #[test]
    fn an_object_argument_names_a_live_object_of_its_interface() {
        let mut client = client_with_keyboard(7);
        let enter = |surface| {
            let enter = wl_keyboard::Event::Enter {
                serial: 1,
                surface,
                keys: Vec::new(),
            };
            AnyEvent::from(enter)
        };
        let error = client.event(4, &enter(0)).expect_err("null");
        assert!(error.contains("names null"), "{error}");
        let error = client.event(4, &enter(3)).expect_err("a seat");
        assert!(error.contains("names 3, a wl_seat"), "{error}");
        let error = client.event(4, &enter(9)).expect_err("nothing");
        assert!(error.contains("names 9, which is no object"), "{error}");
    }

    #[test]
    fn a_server_takes_no_request_from_an_object_its_client_destroyed() {
        let mut server = Peer::new(Role::Server);
        let received: [(u32, AnyRequest); 3] = [
            (1, wl_display::Request::GetRegistry { registry: 2 }.into()),
            (
                2,
                wl_registry::Request::Bind {
                    name: 1,
                    id: (wl_compositor::NAME.to_owned(), 6, 3),
                }
                .into(),
            ),
            (3, wl_compositor::Request::CreateSurface { id: 4 }.into()),
        ];
        for (sender, request) in &received {
            assert_eq!(server.request(*sender, request), Ok(None));
        }
        let destroy = wl_surface::Request::Destroy {}.into();
        assert_eq!(server.request(4, &destroy), Ok(Some(4)), "owes a delete_id");
        let commit = wl_surface::Request::Commit {}.into();
        let error = server.request(4, &commit).expect_err("destroyed");
        assert!(error.contains("Destroyed"), "{error}");
        // Its ID waits for the delete_id.
        let surface = wl_compositor::Request::CreateSurface { id: 4 }.into();
        let error = server.request(3, &surface).expect_err("in use");
        assert!(error.contains("still in use"), "{error}");
        server.event(1, &delete_id(4)).expect("a server sends");
        assert_eq!(server.request(3, &surface), Ok(None));
        // A surface the server ends on its own still takes the requests
        // that crossed its delete_id, and its ID is free.
        server.event(1, &delete_id(4)).expect("a server sends");
        assert!(server.deleted(4));
        assert_eq!(server.request(4, &commit), Ok(None));
        let sync = wl_display::Request::Sync { callback: 5 }.into();
        assert_eq!(server.request(1, &sync), Ok(None));
        let error = server.request(1, &wl_display::Request::Sync { callback: 7 }.into());
        assert!(error.expect_err("a gap").contains("past the next fresh ID"));
        let done = wl_callback::Event::Done { callback_data: 0 }.into();
        server.event(5, &done).expect("a server sends");
        assert_eq!(server.request(3, &surface), Ok(None), "after its delete_id");
    }

    #[test]
    fn a_server_in_step_takes_no_request_from_an_object_it_ended() {
        let mut server = Peer::new(Role::Server).in_step();
        let received: [(u32, AnyRequest); 3] = [
            (1, wl_display::Request::GetRegistry { registry: 2 }.into()),
            (
                2,
                wl_registry::Request::Bind {
                    name: 1,
                    id: (wl_compositor::NAME.to_owned(), 6, 3),
                }
                .into(),
            ),
            (3, wl_compositor::Request::CreateSurface { id: 4 }.into()),
        ];
        for (sender, request) in &received {
            assert_eq!(server.request(*sender, request), Ok(None));
        }
        server.event(1, &delete_id(4)).expect("a server sends");
        let commit = wl_surface::Request::Commit {}.into();
        let error = server.request(4, &commit).expect_err("after its delete_id");
        assert!(error.contains("Deleted"), "{error}");
    }

    #[test]
    fn a_peer_learns_from_what_it_sends_in_any_pieces() {
        let mut client = Peer::new(Role::Client);
        let (mut bytes, _) = wl_display::req::get_registry(WL_DISPLAY_ID, 2).into_parts();
        bytes.extend(wl_display::req::sync(WL_DISPLAY_ID, 3).bytes());
        let (first, rest) = bytes.split_at(5);
        client.send(&Outgoing::from_parts(first.to_vec(), Vec::new()));
        assert_eq!(client.live(wl_registry::NAME), []);
        client.send(&Outgoing::from_parts(rest.to_vec(), Vec::new()));
        assert_eq!(client.live(wl_registry::NAME), [2]);
        assert_eq!(client.live(wl_callback::NAME), [3]);
        // A stream that cannot be framed teaches nothing more.
        client.send(&Outgoing::from_parts(vec![0xff; 8], Vec::new()));
        client.send(&wl_display::req::sync(WL_DISPLAY_ID, 4));
        assert_eq!(client.live(wl_callback::NAME), [3]);
    }

    #[test]
    fn a_destructor_event_ends_its_sender_for_the_server() {
        let mut client = Peer::new(Role::Client);
        let sync = wl_display::Request::Sync { callback: 2 }.into();
        client.request(1, &sync).expect("a client sends");
        let done = wl_callback::Event::Done { callback_data: 0 }.into();
        assert_eq!(client.event(2, &done), Ok(()));
        let error = client.event(2, &done).expect_err("a second done");
        assert!(error.contains("Destroyed { by: Server }"), "{error}");
        assert_eq!(client.event(1, &delete_id(2)), Ok(()));
    }

    #[test]
    fn by_default_the_server_ends_only_the_frame_callbacks_of_an_ended_surface() {
        let mut client = Peer::new(Role::Client);
        let sent: [(u32, AnyRequest); 5] = [
            (1, wl_display::Request::GetRegistry { registry: 2 }.into()),
            (
                2,
                wl_registry::Request::Bind {
                    name: 1,
                    id: (wl_compositor::NAME.to_owned(), 6, 3),
                }
                .into(),
            ),
            (3, wl_compositor::Request::CreateSurface { id: 4 }.into()),
            (4, wl_surface::Request::Frame { callback: 5 }.into()),
            (4, wl_surface::Request::Frame { callback: 6 }.into()),
        ];
        for (sender, request) in &sent {
            client.request(*sender, request).expect("a client sends");
        }
        let error = client
            .event(1, &delete_id(5))
            .expect_err("its surface is live");
        assert!(error.contains("may not end"), "{error}");
        let error = client.event(1, &delete_id(4)).expect_err("a live surface");
        assert!(error.contains("may not end"), "{error}");
        let destroy = wl_surface::Request::Destroy {}.into();
        client.request(4, &destroy).expect("a client sends");
        assert_eq!(client.event(1, &delete_id(6)), Ok(()));
    }
}
