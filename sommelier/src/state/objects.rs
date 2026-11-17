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

//! The objects of one connection on each side of the proxy, and how the
//! two sides pair up.

use std::collections::HashMap;

use super::id_allocator::{IdAllocator, WL_MAP_MAX_OBJECTS};
use super::{GuestId, HostId};
use crate::protocols::wayland::wl_display;
use crate::wire::WL_DISPLAY_ID;
use log::warn;

/// What a host `wl_display.delete_id` ended
/// ([`ShadowTable::release_deleted_host_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedHostId {
    /// The host may not delete the ID: the proxy does not track it, or it
    /// holds a live object only the proxy uses, which the host never
    /// destroys on its own. Nothing changed.
    Ignored,
    /// A host-only object; no guest object is owed a `delete_id`.
    HostOnly,
    /// An object paired with this guest object, now owed its `delete_id`.
    Paired(u32),
}

/// What a live guest object stands for on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The host object it is paired with.
    Host(u32),
    /// Nothing: the proxy implements the object itself.
    Local,
}

/// The interface and negotiated version of an object on one side.
#[derive(Debug)]
struct Interface {
    name: String,
    version: u32,
}

impl Interface {
    fn new(name: &str, version: u32) -> Self {
        Self {
            name: name.to_string(),
            version,
        }
    }
}

/// An object the guest has.
#[derive(Debug)]
enum GuestObject {
    /// An object with no host counterpart, which the proxy implements
    /// ([`ShadowTable::create_local`]).
    Local(Interface),
    /// An object paired with a host object, which records the pairing too.
    Paired {
        host: HostId,
        interface: Interface,
        state: GuestState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestState {
    Live,
    /// The guest destroyed the object. It rejects requests, but stays paired
    /// until the host's `delete_id` (or the event that ends it), so late
    /// events still resolve.
    DestroyPending,
}

impl GuestObject {
    fn interface(&self) -> &Interface {
        match self {
            Self::Local(interface) | Self::Paired { interface, .. } => interface,
        }
    }

    fn host(&self) -> Option<HostId> {
        match self {
            Self::Local(_) => None,
            Self::Paired { host, .. } => Some(*host),
        }
    }
}

/// What holds a host ID. An ID is in use exactly while it has one.
#[derive(Debug)]
enum HostObject {
    /// An object paired with a guest object, which records the pairing too.
    /// Its interface is the guest object's, except for a bridged object
    /// ([`ShadowTable::create_bridged`]).
    Paired {
        guest: GuestId,
        interface: Interface,
    },
    /// A live object that only the proxy uses.
    HostOnly(Interface),
    /// An object the proxy no longer uses, whose ID stays in use until the
    /// host's `delete_id`: one whose destructor is queued, or one without a
    /// destructor, which keeps its ID for the connection unless the host
    /// sends a `delete_id` anyway.
    Retired,
}

impl HostObject {
    fn guest(&self) -> Option<GuestId> {
        match self {
            Self::Paired { guest, .. } => Some(*guest),
            _ => None,
        }
    }

    /// The interface of a live object.
    fn interface(&self) -> Option<&Interface> {
        match self {
            Self::Paired { interface, .. } | Self::HostOnly(interface) => Some(interface),
            Self::Retired => None,
        }
    }
}

/// The objects of one connection on each side of the proxy, and how they
/// pair up.
///
/// Each side has one entry per object, with the interface and version that
/// side knows it by, and a pairing is recorded in both entries. Guest and
/// host IDs are independent: only the constructors pair two objects, each
/// with IDs nothing holds, and every transition that ends a pairing changes
/// both entries.
pub struct ShadowTable {
    /// Every object the guest has.
    guests: HashMap<GuestId, GuestObject>,
    /// Every host ID in use. A host `new_id` may not name one.
    hosts: HashMap<HostId, HostObject>,
    /// The IDs this proxy allocates for its objects on the host connection.
    /// An ID returns here only through [`Self::release_deleted_host_id`] or
    /// [`Self::abandon`].
    host_ids: IdAllocator,
    /// Wayland reserves the upper 8-bit ID range for IDs allocated by the
    /// server and sent to the client in an event. Host and guest object
    /// namespaces are independent, so host-generated `new_id` values must be
    /// translated to a fresh ID in this range before they reach the guest.
    /// An ID returns here when its guest object ends.
    guest_server_ids: IdAllocator,
}

impl ShadowTable {
    const GUEST_SERVER_ID_START: u32 = 0xff00_0000;
    /// The IDs a Wayland client allocates: 0 is null, 1 is `wl_display`,
    /// and libwayland-server accepts up to `WL_MAP_MAX_OBJECTS` inclusive.
    const CLIENT_IDS: std::ops::Range<u32> = 2..WL_MAP_MAX_OBJECTS + 1;
    /// The server IDs a libwayland client accepts: index (offset from
    /// [`Self::GUEST_SERVER_ID_START`]) up to `WL_MAP_MAX_OBJECTS` inclusive.
    const SERVER_IDS: std::ops::Range<u32> =
        Self::GUEST_SERVER_ID_START..Self::GUEST_SERVER_ID_START + WL_MAP_MAX_OBJECTS + 1;
    /// The version of an object created without a negotiated version: a
    /// test fixture, or a child of one. It passes every version check.
    const UNKNOWN_OBJECT_VERSION: u32 = u32::MAX;

    pub fn new() -> Self {
        Self {
            guests: HashMap::new(),
            hosts: HashMap::new(),
            host_ids: IdAllocator::new(Self::CLIENT_IDS),
            guest_server_ids: IdAllocator::new(Self::SERVER_IDS),
        }
    }

    /// Allocate an ID for a new proxy object on the host connection.
    ///
    /// The ID is free on the host: either never used, or released by the
    /// host's `wl_display.delete_id`. No entry holds it: the allocator hands
    /// out each ID once until it is released, and the host creates objects
    /// only with server IDs ([`Self::accepts_host_new_id`]).
    ///
    /// # Panics
    ///
    /// When the client holds `WL_MAP_MAX_OBJECTS` live objects, which the
    /// host would refuse anyway. Allocation runs only in a client's own
    /// task, and Tokio confines a task's panic to it, so this ends that
    /// client alone; unwinding drops its connections and descriptors.
    fn allocate_host_id(&mut self) -> HostId {
        let Some(id) = self.host_ids.next_candidate() else {
            log::error!("Ending the client: its host object IDs are exhausted");
            panic!("host Wayland object IDs exhausted");
        };
        debug_assert!(
            !self.hosts.contains_key(&HostId(id)),
            "host ID {id} is in use"
        );
        HostId(id)
    }

    /// Allocate an ID in Wayland's server-generated ID range for a guest
    /// object that the host created.
    ///
    /// # Panics
    ///
    /// Like [`Self::allocate_host_id`], when the range is exhausted; that
    /// ends only the client whose task allocates.
    fn allocate_guest_server_id(&mut self) -> GuestId {
        let Some(id) = self.guest_server_ids.next_candidate() else {
            log::error!("Ending the client: its guest server IDs are exhausted");
            panic!("guest Wayland server IDs exhausted");
        };
        debug_assert!(
            !self.guests.contains_key(&GuestId(id)),
            "guest server ID {id} is in use"
        );
        GuestId(id)
    }

    // Creation. Each constructor creates an object with every fact the
    // table keeps about it, under IDs that nothing holds.

    /// Create `wl_display`, the object a connection starts with, under the
    /// ID it has on both sides.
    pub fn create_display(&mut self) {
        let interface = || Interface::new("wl_display", wl_display::VERSION);
        self.insert_pair(
            GuestId(WL_DISPLAY_ID),
            interface(),
            HostId(WL_DISPLAY_ID),
            interface(),
        );
    }

    /// Create guest object `guest_id` and a host object for it, paired, each
    /// with `interface` and its own version.
    ///
    /// # Returns
    ///
    /// The host object's ID, newly allocated.
    pub fn create_paired(
        &mut self,
        guest_id: u32,
        interface: &str,
        version: u32,
        host_version: u32,
    ) -> u32 {
        self.create_bridged(guest_id, interface, version, interface, host_version)
    }

    /// Like [`Self::create_paired`], for a guest object backed by a host
    /// object of another interface: a guest `zwp_text_input_v3` backed by a
    /// host `zwp_text_input_v1`.
    pub fn create_bridged(
        &mut self,
        guest_id: u32,
        interface: &str,
        version: u32,
        host_interface: &str,
        host_version: u32,
    ) -> u32 {
        let host = self.allocate_host_id();
        self.insert_pair(
            GuestId(guest_id),
            Interface::new(interface, version),
            host,
            Interface::new(host_interface, host_version),
        );
        host.0
    }

    /// Create a guest object for host object `host_id`, which a host event
    /// just created, with a fresh ID from the guest's server range; both
    /// take `interface` and `version`.
    ///
    /// # Returns
    ///
    /// The guest object's ID.
    pub fn adopt_host_created(&mut self, host_id: u32, interface: &str, version: u32) -> u32 {
        let guest = self.allocate_guest_server_id();
        let interface = || Interface::new(interface, version);
        self.insert_pair(guest, interface(), HostId(host_id), interface());
        guest.0
    }

    /// Create guest object `guest_id`, which the proxy implements itself:
    /// it has no host object, and its handlers consume its requests (the
    /// SHM shim, the v3 text-input manager, or an inert `wl_output` whose
    /// bind crossed its global's removal).
    pub fn create_local(&mut self, guest_id: u32, interface: &str, version: u32) {
        let local = GuestObject::Local(Interface::new(interface, version));
        let previous = self.guests.insert(GuestId(guest_id), local);
        debug_assert!(previous.is_none(), "guest ID {guest_id} is in use");
    }

    /// Create a host object that only the proxy uses.
    ///
    /// # Returns
    ///
    /// Its ID, newly allocated.
    pub fn create_host_only(&mut self, interface: &str, version: u32) -> u32 {
        let host = self.allocate_host_id();
        let object = HostObject::HostOnly(Interface::new(interface, version));
        self.hosts.insert(host, object);
        host.0
    }

    /// Record a new pairing of two unused IDs.
    fn insert_pair(&mut self, guest: GuestId, interface: Interface, host: HostId, own: Interface) {
        let host_object = HostObject::Paired {
            guest,
            interface: own,
        };
        let guest_object = GuestObject::Paired {
            host,
            interface,
            state: GuestState::Live,
        };
        let previous = (
            self.hosts.insert(host, host_object),
            self.guests.insert(guest, guest_object),
        );
        debug_assert!(
            matches!(previous, (None, None)),
            "pairing guest ID {} or host ID {} in use",
            guest.0,
            host.0
        );
    }

    // Generated dispatch. Each message resolves its sender and object
    // arguments once, which validates and translates them together.

    /// Resolve the sender of a guest request: a live guest object.
    ///
    /// `None` when `guest_id` names no object, or one the guest destroyed.
    pub fn request_sender(&self, guest_id: u32) -> Option<Link> {
        match self.guests.get(&GuestId(guest_id))? {
            GuestObject::Local(_) => Some(Link::Local),
            GuestObject::Paired {
                host,
                state: GuestState::Live,
                ..
            } => Some(Link::Host(host.0)),
            GuestObject::Paired {
                state: GuestState::DestroyPending,
                ..
            } => None,
        }
    }

    /// Resolve an object argument of a guest request, which must be a live
    /// guest object of `interface`: Wayland object IDs are not
    /// interchangeable, and forwarding a `wl_surface` where a `wl_seat` is
    /// expected would send a type-invalid request to the host.
    ///
    /// `None` when the request may not name `guest_id` there.
    pub fn request_object(&self, guest_id: u32, interface: &str) -> Option<Link> {
        self.request_sender(guest_id).filter(|_| {
            self.get_interface(guest_id)
                .is_some_and(|own| own == interface)
        })
    }

    /// Resolve an object argument of a host event, which must be a host
    /// object of `interface` paired with a live guest object.
    ///
    /// # Returns
    ///
    /// The guest object's ID, or `None` when the event may not reach the
    /// guest naming `host_id` there.
    pub fn event_object(&self, host_id: u32, interface: &str) -> Option<u32> {
        let HostObject::Paired {
            guest,
            interface: own,
        } = self.hosts.get(&HostId(host_id))?
        else {
            return None;
        };
        (own.name == interface && !self.is_pending_destroy_guest(guest.0)).then_some(guest.0)
    }

    /// Return whether a guest request may allocate `guest_id`.
    ///
    /// Client-created IDs must be non-zero, must not reuse an existing object,
    /// and must stay below Wayland's server-generated ID range. Keeping this
    /// check in the shadow table lets generated protocol dispatchers validate
    /// every `new_id` request before a handler can mutate state.
    pub fn is_guest_id_available(&self, guest_id: u32) -> bool {
        guest_id > 1
            && guest_id < Self::GUEST_SERVER_ID_START
            && !self.guests.contains_key(&GuestId(guest_id))
    }

    /// Return whether a host event may create an object with ID `host_id`:
    /// an ID from the server range that no object holds.
    ///
    /// libwayland-server allocates every object it creates from the server
    /// range. An ID from the client range is one this proxy allocates, so a
    /// host that sent one could later collide with the proxy's own objects.
    pub fn accepts_host_new_id(&self, host_id: u32) -> bool {
        host_id >= Self::GUEST_SERVER_ID_START && !self.hosts.contains_key(&HostId(host_id))
    }

    /// Return whether a host event sender belongs to an object that this
    /// connection is currently tracking: a paired object, or one that
    /// Sommelier binds internally (for example the host `wl_shm` and
    /// keyboard-extension objects).
    ///
    /// An event from any other sender is stale or malformed and must not
    /// reach a handler, because handlers may update proxy state before the
    /// generated forwarding code can discover that the sender has no guest
    /// representation.
    pub fn is_event_sender_known(&self, host_id: u32) -> bool {
        self.get_host_interface(host_id).is_some()
    }

    // Queries.

    pub fn get_host_id(&self, guest_id: u32) -> Option<u32> {
        self.host_id_of(GuestId(guest_id)).map(|host| host.0)
    }

    pub fn get_guest_id(&self, host_id: u32) -> Option<u32> {
        self.guest_id_of(HostId(host_id)).map(|guest| guest.0)
    }

    /// Typed lookup: translate a guest-allocated object ID to its host counterpart.
    /// Use in **client→host request** handlers where `ctx.last_sender_id` is a guest ID.
    pub fn host_id_of(&self, guest: GuestId) -> Option<HostId> {
        self.guests.get(&guest)?.host()
    }

    /// Typed lookup: translate a host-allocated object ID to its guest counterpart.
    /// Use in **host→client event** handlers where `ctx.last_sender_id` is a host ID.
    pub fn guest_id_of(&self, host: HostId) -> Option<GuestId> {
        self.hosts.get(&host)?.guest()
    }

    /// The guest object's interface.
    pub fn get_interface(&self, guest_id: u32) -> Option<&str> {
        Some(&self.guests.get(&GuestId(guest_id))?.interface().name)
    }

    /// The interface of a live host object, which events from it are
    /// dispatched by.
    pub fn get_host_interface(&self, host_id: u32) -> Option<&str> {
        Some(&self.hosts.get(&HostId(host_id))?.interface()?.name)
    }

    /// Return whether `guest_id` belongs to the range reserved for objects
    /// created by the server.
    ///
    /// The client destroys these objects but never receives `delete_id` for
    /// them: the server, not the client, owns their raw ID lifecycle.
    pub fn is_guest_server_id(&self, guest_id: u32) -> bool {
        guest_id >= Self::GUEST_SERVER_ID_START
    }

    pub fn is_pending_destroy_guest(&self, guest_id: u32) -> bool {
        matches!(
            self.guests.get(&GuestId(guest_id)),
            Some(GuestObject::Paired {
                state: GuestState::DestroyPending,
                ..
            })
        )
    }

    pub fn is_pending_destroy_host(&self, host_id: u32) -> bool {
        self.get_guest_id(host_id)
            .is_some_and(|guest_id| self.is_pending_destroy_guest(guest_id))
    }

    /// The guest object's version.
    pub fn guest_object_version(&self, guest_id: u32) -> Option<u32> {
        Some(self.guests.get(&GuestId(guest_id))?.interface().version)
    }

    /// The live host object's version.
    pub fn host_object_version(&self, host_id: u32) -> Option<u32> {
        Some(self.hosts.get(&HostId(host_id))?.interface()?.version)
    }

    /// The guest object's version, or the unknown version when none is
    /// recorded. A child object created from it takes this version.
    pub(crate) fn guest_object_version_or_unknown(&self, guest_id: u32) -> u32 {
        self.guest_object_version(guest_id)
            .unwrap_or(Self::UNKNOWN_OBJECT_VERSION)
    }

    /// The host object's version, or the unknown version when none is
    /// recorded. A child object created from it takes this version.
    pub(crate) fn host_object_version_or_unknown(&self, host_id: u32) -> u32 {
        self.host_object_version(host_id)
            .unwrap_or(Self::UNKNOWN_OBJECT_VERSION)
    }

    /// Whether `version` is the unknown version: the object was created
    /// without a negotiated one.
    pub(crate) fn is_unknown_version(version: u32) -> bool {
        version == Self::UNKNOWN_OBJECT_VERSION
    }

    // Lifecycle. Each transition is named after what happened and changes
    // both sides of a pairing at once.

    /// The guest destroyed paired guest object `guest_id`, and its
    /// destructor went to the host object.
    ///
    /// An object the guest created stays paired until the host's
    /// `delete_id`, rejecting requests, so late events still resolve and the
    /// `delete_id` reaches the guest. An object the server created gets no
    /// `delete_id`: libwayland frees its ID on both sides as soon as either
    /// side destroys it, so both entries end now.
    pub fn guest_destroyed(&mut self, guest_id: u32) {
        if !self.is_guest_server_id(guest_id) {
            self.guest_destroyed_in_use(guest_id);
            return;
        }
        let guest = GuestId(guest_id);
        if let Some(host) = self
            .forget_guest_object(guest)
            .and_then(|object| object.host())
        {
            self.remove_paired_host(host, guest);
        }
    }

    /// The guest destroyed paired guest object `guest_id` while its host
    /// object is still in use, as a buffer the host still shows: the proxy
    /// destroys the host object later, through [`Self::guest_destroyed`].
    ///
    /// Until then the guest object rejects requests, but stays paired so
    /// the host object's events still reach its handler.
    pub fn guest_destroyed_in_use(&mut self, guest_id: u32) {
        if let Some(GuestObject::Paired { state, .. }) = self.guests.get_mut(&GuestId(guest_id)) {
            *state = GuestState::DestroyPending;
        }
    }

    /// Guest object `guest_id` ended without its destructor reaching its
    /// host object, which has none (a v3 text input backed by a host v1
    /// one): the host object stays in use as a host-only object, so its ID
    /// is not reused while its events are in flight.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest_id` is paired.
    pub fn guest_ended_host_kept(&mut self, guest_id: u32) -> bool {
        let Some(host) = self.end_paired_guest(GuestId(guest_id)) else {
            return false;
        };
        if let Some(HostObject::Paired { interface, .. }) = self.hosts.remove(&host) {
            self.hosts.insert(host, HostObject::HostOnly(interface));
        }
        true
    }

    /// Paired guest object `guest_id` ended, and so did its host object:
    /// by a terminal event (`wl_callback.done`) or a destructor the proxy
    /// queued. The host ID stays reserved until the host's `delete_id`,
    /// which then reaches no guest object.
    pub fn guest_ended_host_destroyed(&mut self, guest_id: u32) {
        let host = self.end_paired_guest(GuestId(guest_id));
        debug_assert!(host.is_some(), "guest {guest_id} is not paired");
        if let Some(host) = host {
            self.retire_destroyed_host(host);
        }
    }

    /// Local guest object `guest_id` ended: its destructor completed in the
    /// proxy.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest_id` is a local object.
    pub fn local_destroyed(&mut self, guest_id: u32) -> bool {
        let guest = GuestId(guest_id);
        if !matches!(self.guests.get(&guest), Some(GuestObject::Local(_))) {
            return false;
        }
        self.forget_guest_object(guest);
        true
    }

    /// Forget paired guest object `guest_id` and its host object, which
    /// never reached the host: nothing on either side knows the IDs, so
    /// both are free at once.
    pub fn abandon(&mut self, guest_id: u32) {
        let guest = GuestId(guest_id);
        let host = self
            .forget_guest_object(guest)
            .and_then(|object| object.host());
        debug_assert!(host.is_some(), "guest {guest_id} is not paired");
        if let Some(host) = host {
            self.remove_paired_host(host, guest);
            self.host_ids.release(host.0);
        }
    }

    /// Retire a host-only object after its destructor request has been queued.
    ///
    /// The dispatch metadata is removed immediately so stale events cannot
    /// reach a handler. An ID this proxy allocated remains reserved until the
    /// host acknowledges the destructor with `wl_display.delete_id`. An ID
    /// the host allocated gets no `delete_id` (libwayland frees server IDs
    /// as soon as the destructor is processed and may hand them to the next
    /// server-created object at once), so nothing is kept for it.
    ///
    /// Handlers go through
    /// [`Context::retire_destroyed_host_object`](super::Context::retire_destroyed_host_object), which
    /// owns what a refusal means.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `host_id` is a live host-only
    /// object.
    pub(super) fn host_destroy_queued(&mut self, host_id: u32) -> bool {
        let host = HostId(host_id);
        if !matches!(self.hosts.get(&host), Some(HostObject::HostOnly(_))) {
            return false;
        }
        self.retire_destroyed_host(host);
        true
    }

    /// Retire host-only object `host_id`, which has no destructor, once its
    /// guest object or global is gone: stale events stop reaching it, and
    /// its ID stays reserved for the connection.
    ///
    /// Only a live host-only object is retired. A paired object is not a
    /// binding of the proxy's own, and an ID with no entry has nothing to
    /// keep reserved: reserving it would let it collide with the
    /// allocator's next object. Either is refused with a warning.
    pub fn retire_host(&mut self, host_id: u32) {
        match self.hosts.get_mut(&HostId(host_id)) {
            Some(object @ HostObject::HostOnly(_)) => *object = HostObject::Retired,
            Some(HostObject::Retired) => {}
            Some(HostObject::Paired { guest, .. }) => warn!(
                "Not retiring host object {host_id}, which guest object {} uses",
                guest.0
            ),
            None => warn!("Not retiring host object {host_id}, which is untracked"),
        }
    }

    /// Apply the host's `wl_display.delete_id` for `host_id`.
    ///
    /// The host has destroyed the object and forgotten its ID, so no later
    /// event can name it. End the object and the guest object paired with
    /// it, and return the ID to the allocator. As in libwayland-client, this
    /// is the only way an ID this proxy allocated becomes reusable: an ID is
    /// never reused while its `delete_id` is outstanding, and objects without
    /// a destructor keep theirs for the connection.
    ///
    /// A server may destroy a client's object on its own, as a compositor
    /// does with the pending frame callbacks of a destroyed surface, so a
    /// `delete_id` ends a paired object whether or not the guest destroyed
    /// it. A live object only the proxy uses (a binding, an Aura child, an
    /// internal callback, host-only params) is one the host never destroys
    /// on its own: a `delete_id` for one, or for an ID this proxy did not
    /// allocate or no longer tracks, changes nothing and returns
    /// [`DeletedHostId::Ignored`]. Freeing a live object's ID would hand it
    /// to a new object while the old one still holds it.
    pub fn release_deleted_host_id(&mut self, host_id: u32) -> DeletedHostId {
        let host = HostId(host_id);
        let deletable = self.hosts.get(&host).filter(|object| {
            self.host_ids.contains(host_id) && !matches!(object, HostObject::HostOnly(_))
        });
        let Some(object) = deletable else {
            warn!("Ignoring wl_display.delete_id for host ID {host_id}, which is untracked or a live host-only object");
            return DeletedHostId::Ignored;
        };
        let guest = object.guest();
        self.hosts.remove(&host);
        self.host_ids.release(host_id);
        let Some(guest) = guest else {
            return DeletedHostId::HostOnly;
        };
        let paired = self
            .forget_guest_object(guest)
            .and_then(|object| object.host());
        debug_assert_eq!(paired, Some(host), "guest {guest:?} was not paired");
        DeletedHostId::Paired(guest.0)
    }

    /// End guest object `guest`: the one place a guest object ends.
    ///
    /// An ID from the server range returns to the allocator. A
    /// server-created object gets no `delete_id`; libwayland-client frees its
    /// ID as soon as either side destroys it, which is when the proxy drops
    /// its mapping. Nothing is freed for an ID that named no object, so no
    /// ID is freed twice.
    fn forget_guest_object(&mut self, guest: GuestId) -> Option<GuestObject> {
        let object = self.guests.remove(&guest)?;
        self.guest_server_ids.release(guest.0);
        Some(object)
    }

    /// End paired guest object `guest`, leaving its host object to the
    /// caller, and return that object's ID.
    fn end_paired_guest(&mut self, guest: GuestId) -> Option<HostId> {
        let GuestObject::Paired { host, .. } = self.guests.get(&guest)? else {
            return None;
        };
        let host = *host;
        self.forget_guest_object(guest);
        Some(host)
    }

    /// Remove `host`, the host object paired with `guest`, whose entry the
    /// caller is ending or has ended.
    fn remove_paired_host(&mut self, host: HostId, guest: GuestId) -> Option<HostObject> {
        let object = self.hosts.remove(&host);
        debug_assert_eq!(
            object.as_ref().and_then(HostObject::guest),
            Some(guest),
            "host {host:?} was not paired"
        );
        object
    }

    /// End host object `host`, whose destructor is queued: an ID this proxy
    /// allocated awaits the host's `delete_id`, and one the host allocated
    /// is free at once.
    fn retire_destroyed_host(&mut self, host: HostId) {
        if self.host_ids.contains(host.0) {
            self.hosts.insert(host, HostObject::Retired);
        } else {
            self.hosts.remove(&host);
        }
    }

    // Test fixtures.

    /// Pair the hard-coded IDs of a test fixture as one object of
    /// `interface`, with no negotiated version, and keep both allocators
    /// from handing those IDs out.
    ///
    /// # Panics
    ///
    /// When either ID is already in use.
    #[cfg(test)]
    pub(crate) fn insert_paired_for_test(&mut self, guest_id: u32, host_id: u32, interface: &str) {
        self.insert_bridged_for_test(guest_id, host_id, interface, interface);
    }

    /// Like [`Self::insert_paired_for_test`], with `version` on both sides.
    #[cfg(test)]
    pub(crate) fn insert_paired_with_version_for_test(
        &mut self,
        guest_id: u32,
        host_id: u32,
        interface: &str,
        version: u32,
    ) {
        self.reserve_for_test(guest_id, host_id);
        let interface = || Interface::new(interface, version);
        self.insert_pair(GuestId(guest_id), interface(), HostId(host_id), interface());
    }

    /// Like [`Self::insert_paired_for_test`], for a guest object of
    /// `interface` backed by a host object of `host_interface`.
    #[cfg(test)]
    pub(crate) fn insert_bridged_for_test(
        &mut self,
        guest_id: u32,
        host_id: u32,
        interface: &str,
        host_interface: &str,
    ) {
        self.reserve_for_test(guest_id, host_id);
        self.insert_pair(
            GuestId(guest_id),
            Interface::new(interface, Self::UNKNOWN_OBJECT_VERSION),
            HostId(host_id),
            Interface::new(host_interface, Self::UNKNOWN_OBJECT_VERSION),
        );
    }

    /// Create a host-only object under the hard-coded ID of a test fixture,
    /// and keep the allocator from handing it out.
    ///
    /// # Panics
    ///
    /// When the ID is already in use.
    #[cfg(test)]
    pub(crate) fn insert_host_only_for_test(
        &mut self,
        host_id: u32,
        interface: &str,
        version: u32,
    ) {
        self.reserve_host_id_for_test(host_id);
        let object = HostObject::HostOnly(Interface::new(interface, version));
        self.hosts.insert(HostId(host_id), object);
    }

    /// Change the negotiated version of guest object `guest_id`, for a test
    /// of version-dependent behavior on an object a fixture created.
    #[cfg(test)]
    pub(crate) fn set_guest_version_for_test(&mut self, guest_id: u32, version: u32) {
        let object = self.guests.get_mut(&GuestId(guest_id));
        let interface = match object.expect("a guest object") {
            GuestObject::Local(interface) | GuestObject::Paired { interface, .. } => interface,
        };
        interface.version = version;
    }

    #[cfg(test)]
    fn reserve_for_test(&mut self, guest_id: u32, host_id: u32) {
        assert!(
            !self.guests.contains_key(&GuestId(guest_id))
                && self.guest_server_ids.reserve(guest_id),
            "fixture guest ID {guest_id} is in use"
        );
        self.reserve_host_id_for_test(host_id);
    }

    #[cfg(test)]
    fn reserve_host_id_for_test(&mut self, host_id: u32) {
        assert!(
            !self.hosts.contains_key(&HostId(host_id)) && self.host_ids.reserve(host_id),
            "fixture host ID {host_id} is in use"
        );
    }

    /// Return whether no object holds host ID `host_id`.
    #[cfg(test)]
    pub fn is_host_id_available(&self, host_id: u32) -> bool {
        host_id > 1 && !self.hosts.contains_key(&HostId(host_id))
    }

    /// Return whether host ID `host_id` holds a retired object.
    #[cfg(test)]
    pub fn is_retired_host(&self, host_id: u32) -> bool {
        matches!(self.hosts.get(&HostId(host_id)), Some(HostObject::Retired))
    }
}

impl Default for ShadowTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: u32 = ShadowTable::GUEST_SERVER_ID_START;

    #[test]
    fn host_ids_stay_within_libwayland_servers_client_range() {
        let mut table = ShadowTable::new();
        assert_eq!(
            table.allocate_host_id(),
            HostId(2),
            "0 is null and 1 is wl_display"
        );
        // libwayland-server accepts a client ID equal to WL_MAP_MAX_OBJECTS
        // and refuses the next one.
        table.host_ids = IdAllocator::new(WL_MAP_MAX_OBJECTS..ShadowTable::CLIENT_IDS.end);
        assert_eq!(table.allocate_host_id(), HostId(0x00f0_0000));
        let exhausted =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| table.allocate_host_id()));
        assert!(
            exhausted.is_err(),
            "the allocator must not hand out an ID libwayland-server refuses"
        );
    }

    /// A host ID stays in use while any entry holds it: a live host-only
    /// object (such as the internally bound `zcr_keyboard_extension_v1`) or
    /// one retired for the connection.
    #[test]
    fn host_ids_in_use_are_not_handed_out() {
        let mut table = ShadowTable::new();
        let binding = table.create_host_only("zcr_keyboard_extension_v1", 2);
        let retired = table.create_host_only("wl_shm", 1);
        table.retire_host(retired);

        assert!(!table.is_host_id_available(binding));
        assert!(!table.is_host_id_available(retired));
        let next = table.allocate_host_id().0;
        assert!(next != binding && next != retired);
    }

    #[test]
    fn host_created_objects_get_fresh_guest_server_ids() {
        let mut table = ShadowTable::new();
        let first = table.adopt_host_created(SERVER + 40, "wl_data_offer", 3);
        let second = table.adopt_host_created(SERVER + 41, "wl_data_offer", 3);

        assert_eq!((first, second), (SERVER, SERVER + 1));
        assert_eq!(table.get_host_id(second), Some(SERVER + 41));
        assert_eq!(table.guest_object_version(second), Some(3));
        assert_eq!(table.host_object_version(SERVER + 41), Some(3));
    }

    #[test]
    fn guest_server_ids_are_reused_and_stay_below_the_client_map_limit() {
        let mut table = ShadowTable::new();
        let first = table.adopt_host_created(SERVER + 40, "wl_buffer", 1);
        table.guest_destroyed(first);
        assert_eq!(
            table.adopt_host_created(SERVER + 40, "wl_buffer", 1),
            first,
            "a destroyed server-created object frees its guest ID"
        );

        // libwayland-client accepts the server index WL_MAP_MAX_OBJECTS
        // itself and refuses the next one.
        table.guest_server_ids =
            IdAllocator::new(ShadowTable::SERVER_IDS.end - 1..ShadowTable::SERVER_IDS.end);
        assert_eq!(table.allocate_guest_server_id(), GuestId(0xfff0_0000));
        let exhausted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            table.allocate_guest_server_id()
        }));
        assert!(
            exhausted.is_err(),
            "libwayland-client refuses server IDs past 0xff000000 + WL_MAP_MAX_OBJECTS"
        );
    }

    #[test]
    fn every_end_of_a_server_created_guest_object_frees_its_id() {
        let mut table = ShadowTable::new();

        // The guest half goes while the host object stays.
        let guest = table.adopt_host_created(SERVER + 40, "zwp_text_input_v3", 1);
        assert!(table.guest_ended_host_kept(guest));
        assert_eq!(table.adopt_host_created(SERVER + 41, "wl_buffer", 1), guest);

        // The guest destroys a buffer the host still uses: it keeps its ID
        // until the proxy destroys the host buffer, and frees it once.
        table.guest_destroyed_in_use(guest);
        let next = table.adopt_host_created(SERVER + 42, "wl_buffer", 1);
        assert_ne!(next, guest, "a retired object still owns its ID");
        table.guest_destroyed(guest);
        table.guest_destroyed(guest);
        assert_eq!(table.adopt_host_created(SERVER + 43, "wl_buffer", 1), guest);
        assert_eq!(
            table.adopt_host_created(SERVER + 44, "wl_buffer", 1),
            next + 1
        );
    }

    #[test]
    fn guest_id_availability_rejects_reserved_and_reused_ids() {
        let mut table = ShadowTable::new();
        assert!(!table.is_guest_id_available(0));
        assert!(!table.is_guest_id_available(1));
        assert!(table.is_guest_id_available(2));
        assert!(!table.is_guest_id_available(SERVER));

        table.create_paired(2, "wl_surface", 1, 1);
        assert!(!table.is_guest_id_available(2));
        table.create_local(3, "wl_shm", 1);
        assert!(!table.is_guest_id_available(3));
    }

    #[test]
    fn host_new_ids_must_be_fresh_server_ids() {
        let mut table = ShadowTable::new();
        assert!(table.accepts_host_new_id(SERVER));
        assert!(!table.accepts_host_new_id(0));
        assert!(
            !table.accepts_host_new_id(30),
            "the client range is the proxy's"
        );

        let guest = table.adopt_host_created(SERVER, "wl_data_offer", 3);
        assert!(!table.accepts_host_new_id(SERVER));
        table.guest_destroyed(guest);
        assert!(table.accepts_host_new_id(SERVER));
    }

    #[test]
    fn event_sender_is_known_for_paired_and_internal_objects_only() {
        let mut table = ShadowTable::new();
        assert!(!table.is_event_sender_known(90));

        let paired = table.create_paired(10, "wl_surface", 1, 1);
        assert!(table.is_event_sender_known(paired));
        assert!(!table.is_event_sender_known(10));

        let binding = table.create_host_only("wl_shm", 1);
        assert!(table.is_event_sender_known(binding));
        assert!(table.host_destroy_queued(binding));
        assert!(!table.is_event_sender_known(binding));
    }

    /// Only a live paired or local object can send a request, and an object
    /// argument must also have the interface the request expects.
    #[test]
    fn requests_resolve_live_objects_of_the_expected_interface() {
        let mut table = ShadowTable::new();
        table.create_local(20, "wl_shm", 1);
        let host = table.create_paired(21, "wl_surface", 4, 4);

        assert_eq!(table.request_sender(20), Some(Link::Local));
        assert_eq!(table.request_sender(21), Some(Link::Host(host)));
        assert_eq!(table.request_sender(22), None);
        assert_eq!(
            table.request_object(21, "wl_surface"),
            Some(Link::Host(host))
        );
        assert_eq!(table.request_object(21, "wl_seat"), None);

        table.guest_destroyed(21);
        assert_eq!(table.request_sender(21), None);
        assert_eq!(table.request_object(21, "wl_surface"), None);
        assert!(table.local_destroyed(20));
        assert_eq!(table.request_sender(20), None);
        assert!(!table.local_destroyed(20), "an object ends once");
    }

    #[test]
    fn events_name_paired_objects_whose_guest_object_is_live() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(21, "wl_surface", 4, 4);
        let binding = table.create_host_only("wl_shm", 1);

        assert_eq!(table.event_object(host, "wl_surface"), Some(21));
        assert_eq!(table.event_object(host, "wl_output"), None);
        assert_eq!(
            table.event_object(binding, "wl_shm"),
            None,
            "no guest object"
        );

        table.guest_destroyed(21);
        assert_eq!(table.event_object(host, "wl_surface"), None);
    }

    #[test]
    fn object_versions_are_tracked_on_both_sides_of_a_pairing() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(20, "wl_surface", 4, 3);

        assert_eq!(table.guest_object_version(20), Some(4));
        assert_eq!(table.host_object_version(host), Some(3));
        assert_eq!(table.guest_object_version(99), None);

        table.guest_destroyed(20);
        table.release_deleted_host_id(host);
        assert_eq!(table.guest_object_version(20), None);
        assert_eq!(table.host_object_version(host), None);
    }

    #[test]
    fn a_guest_ended_with_its_host_kept_leaves_a_reserved_host_object() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(20, "wl_callback", 1, 1);

        assert!(table.guest_ended_host_kept(20));
        assert_eq!(table.get_host_id(20), None);
        assert_eq!(table.get_host_interface(host), Some("wl_callback"));
        assert_ne!(
            table.allocate_host_id(),
            HostId(host),
            "a host object without a guest destructor must stay reserved"
        );
        assert!(!table.guest_ended_host_kept(20), "the guest is gone");
    }

    #[test]
    fn a_guest_ended_with_its_host_destroyed_awaits_delete_id() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(20, "wl_callback", 1, 1);

        table.guest_ended_host_destroyed(20);
        assert!(table.is_guest_id_available(20));
        assert!(table.is_retired_host(host));
        assert!(!table.is_event_sender_known(host));
        assert_eq!(table.release_deleted_host_id(host), DeletedHostId::HostOnly);
        assert!(table.is_host_id_available(host));
    }

    #[test]
    fn a_guest_destroyed_in_use_still_routes_host_events() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(20, "wl_buffer", 1, 1);

        table.guest_destroyed_in_use(20);

        assert_eq!(
            table.get_host_interface(host),
            Some("wl_buffer"),
            "a delayed wl_buffer.release must still be dispatchable after guest destroy"
        );
        assert!(table.is_event_sender_known(host));
        assert_eq!(table.host_object_version(host), Some(1));
        assert!(table.is_pending_destroy_guest(20));
        assert_eq!(table.request_sender(20), None);
    }

    #[test]
    fn host_only_destructor_reservation_survives_until_delete_id() {
        let mut table = ShadowTable::new();
        table.insert_host_only_for_test(40, "zcr_extended_keyboard_v1", 2);

        // The destroy request has been queued, but the host has not processed
        // it yet. The dispatch metadata is retired immediately while the
        // numeric ID remains unavailable for reuse.
        assert!(table.host_destroy_queued(40));
        assert!(table.is_retired_host(40));
        assert!(!table.is_event_sender_known(40));
        assert!(!table.is_host_id_available(40));
        assert!(!table.host_destroy_queued(40), "it is retired once");

        // The host acknowledgement is the only point at which the reservation
        // can be released.
        assert_eq!(table.release_deleted_host_id(40), DeletedHostId::HostOnly);
        assert!(!table.is_retired_host(40));
        assert!(table.is_host_id_available(40));
    }

    #[test]
    fn destroyed_host_allocated_object_frees_its_id_at_once() {
        // The host allocates IDs from the server range and sends no
        // delete_id for them; it may reuse one for its next new object.
        let host_buffer = SERVER + 5;
        let mut table = ShadowTable::new();
        table.insert_host_only_for_test(host_buffer, "wl_buffer", 1);

        assert!(table.host_destroy_queued(host_buffer));
        assert!(!table.is_event_sender_known(host_buffer));
        assert!(
            table.accepts_host_new_id(host_buffer),
            "the host's next server-created object may reuse the ID"
        );
    }

    #[test]
    fn host_ids_are_reused_only_after_delete_id() {
        let mut table = ShadowTable::new();
        let first = table.create_paired(10, "wl_surface", 1, 1);
        table.guest_destroyed(10);

        let second = table.create_paired(11, "wl_surface", 1, 1);
        assert_ne!(second, first, "an ID awaiting delete_id must not be reused");

        assert_eq!(
            table.release_deleted_host_id(first),
            DeletedHostId::Paired(10)
        );
        assert_eq!(table.get_host_id(10), None);
        assert_eq!(table.get_interface(10), None);
        assert_eq!(
            table.allocate_host_id(),
            HostId(first),
            "a deleted ID is reused first"
        );
        assert_eq!(
            table.release_deleted_host_id(first + 100),
            DeletedHostId::Ignored,
            "a delete_id for an ID never allocated changes nothing"
        );
        assert_eq!(table.allocate_host_id(), HostId(second + 1));
    }

    /// The host never destroys a binding of the proxy's own, so a
    /// `delete_id` for one must not free its ID: the ID would go to a new
    /// object while the binding still holds it, and the binding's later
    /// `global_remove` would retire the new object.
    #[test]
    fn a_delete_id_for_a_live_host_only_object_changes_nothing() {
        let mut table = ShadowTable::new();
        // An internal binding without a destructor, such as the host wl_shm.
        let binding = table.create_host_only("wl_shm", 1);

        assert_eq!(
            table.release_deleted_host_id(binding),
            DeletedHostId::Ignored
        );
        assert!(table.is_event_sender_known(binding));

        // The binding's global goes, and with it the binding only.
        let next = table.create_paired(11, "wl_surface", 1, 1);
        assert_ne!(next, binding);
        table.retire_host(binding);
        assert!(!table.is_event_sender_known(binding));
        assert_eq!(table.get_host_id(11), Some(next));

        // Retiring a paired object is refused, not fatal.
        table.retire_host(next);
        assert_eq!(table.get_host_id(11), Some(next));
        assert!(table.is_event_sender_known(next));
    }

    /// Retiring an ID that holds no object must not reserve it, or the
    /// allocator's next object would collide with the reservation.
    #[test]
    fn retiring_an_untracked_host_id_changes_nothing() {
        let mut table = ShadowTable::new();
        table.retire_host(2);
        assert!(table.is_host_id_available(2));
        assert_eq!(table.create_host_only("wl_shm", 1), 2);
    }

    /// An object whose creation never reached the host frees its host ID
    /// along with its guest ID, so the ID space stays dense.
    #[test]
    fn an_abandoned_object_frees_both_ids() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(10, "wl_output", 3, 3);

        table.abandon(10);
        assert!(table.is_guest_id_available(10));
        assert!(table.is_host_id_available(host));
        assert_eq!(
            table.allocate_host_id(),
            HostId(host),
            "the host ID is reused"
        );
    }

    #[test]
    fn fixture_ids_are_never_handed_out() {
        let mut table = ShadowTable::new();
        let server_guest = SERVER + 1;
        table.insert_paired_for_test(10, 3, "wl_surface");
        table.insert_paired_for_test(server_guest, 4, "wl_data_offer");
        assert_eq!(table.get_host_id(10), Some(3));
        assert_eq!(table.get_interface(server_guest), Some("wl_data_offer"));

        // The allocators skip the fixture IDs themselves.
        assert_eq!(table.host_ids.next_candidate(), Some(2));
        assert_eq!(table.host_ids.next_candidate(), Some(5));
        assert_eq!(table.guest_server_ids.next_candidate(), Some(SERVER));
        assert_eq!(
            table.guest_server_ids.next_candidate(),
            Some(server_guest + 1)
        );
    }

    /// A guest `zwp_text_input_v3` is backed by a host `zwp_text_input_v1`,
    /// which has no destructor.
    #[test]
    fn a_bridged_host_object_keeps_its_own_interface() {
        let mut table = ShadowTable::new();
        let host = table.create_bridged(20, "zwp_text_input_v3", 1, "zwp_text_input_v1", 1);

        assert_eq!(table.get_interface(20), Some("zwp_text_input_v3"));
        assert_eq!(table.get_host_interface(host), Some("zwp_text_input_v1"));
        assert_eq!(table.event_object(host, "zwp_text_input_v1"), Some(20));
        assert_eq!(table.event_object(host, "zwp_text_input_v3"), None);

        // Once the guest object goes, the host object keeps its own
        // interface, routes its events and stays reserved.
        assert!(table.guest_ended_host_kept(20));
        assert_eq!(table.get_host_interface(host), Some("zwp_text_input_v1"));
        assert!(table.is_event_sender_known(host));
        assert!(!table.is_host_id_available(host));
    }

    #[test]
    fn retiring_a_host_object_awaiting_delete_id_keeps_its_id_reserved() {
        let mut table = ShadowTable::new();
        let host_id = table.create_host_only("zaura_output", 38);
        assert!(table.host_destroy_queued(host_id));

        table.retire_host(host_id);
        assert!(table.is_retired_host(host_id));
        assert!(!table.is_event_sender_known(host_id));
        assert!(!table.is_host_id_available(host_id));
        assert_ne!(table.allocate_host_id(), HostId(host_id));

        // The host's delete_id still frees it.
        assert_eq!(
            table.release_deleted_host_id(host_id),
            DeletedHostId::HostOnly
        );
        assert!(table.is_host_id_available(host_id));
    }
}
