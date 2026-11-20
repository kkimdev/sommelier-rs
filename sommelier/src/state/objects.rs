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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;

use super::id_allocator::{IdAllocator, WL_MAP_MAX_OBJECTS};
use super::object_data::{Data, ObjectData};
use crate::protocols::Iface;
use crate::wire::{EventId, RequestId, WireId, WL_DISPLAY_ID};
use log::{debug, warn};

/// A Wayland object ID of the **guest**'s, as [`ShadowTable`] keys guest
/// objects and the events the proxy sends name them.
///
/// Only the table makes one: from an ID it allocates, or from a wire value
/// it resolves to a guest object or to a free ID the guest may create an
/// object under. So a guest ID cannot stand in for a [`HostId`], an event
/// cannot name a host object, and a request cannot name a guest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestId(u32);

/// A Wayland object ID on the **host** connection, as [`ShadowTable`] keys
/// host objects and the requests the proxy sends name them. Like a
/// [`GuestId`], only the table makes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(u32);

impl GuestId {
    /// `wl_display`, which a connection starts with.
    pub const DISPLAY: Self = Self(WL_DISPLAY_ID);

    /// Guest ID `id`, for a test that names an object by the ID its guest
    /// gave it.
    #[cfg(test)]
    pub(crate) const fn for_test(id: u32) -> Self {
        Self(id)
    }
}

impl HostId {
    /// `wl_display`, which a connection starts with.
    pub const DISPLAY: Self = Self(WL_DISPLAY_ID);

    /// Host ID `id`, for a test that names an object by the ID its host
    /// saw.
    #[cfg(test)]
    pub(crate) const fn for_test(id: u32) -> Self {
        Self(id)
    }
}

impl WireId for GuestId {
    fn wire(self) -> u32 {
        self.0
    }
}

impl EventId for GuestId {}

impl WireId for HostId {
    fn wire(self) -> u32 {
        self.0
    }
}

impl RequestId for HostId {}

impl fmt::Display for GuestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What a host `wl_display.delete_id` ended
/// ([`ShadowTable::release_deleted_host_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedHostId {
    /// The host may not delete the ID: the proxy does not track it, or it
    /// holds a live object only the proxy uses, which the host never
    /// destroys on its own. Nothing changed.
    Ignored,
    /// Host object `host`, which only the proxy used; no guest object is
    /// owed a `delete_id`.
    HostOnly { host: HostId },
    /// Host object `host`, paired with guest object `guest`, which is now
    /// owed its `delete_id`.
    Paired {
        host: HostId,
        guest: GuestId,
        /// Whether the guest object was live: the host destroyed it on its
        /// own, rather than acknowledging the guest's destructor, so this
        /// is the object's first end.
        was_live: bool,
    },
}

/// What a live guest object stands for on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    /// The host object it is paired with.
    Host(HostId),
    /// Nothing: the proxy implements the object itself.
    Local,
}

impl Link {
    /// The host object, if the guest object has one.
    pub fn host(self) -> Option<HostId> {
        match self {
            Self::Host(host) => Some(host),
            Self::Local => None,
        }
    }
}

/// The sender of a guest request, resolved once, before anything of the
/// message is read ([`ShadowTable::resolve_request`]): a live guest object.
/// Its handler gets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestSender {
    pub guest: GuestId,
    pub iface: Iface,
    /// Its negotiated version.
    pub version: u32,
    /// The host object it is paired with, or `None` for a local object,
    /// which the proxy implements itself.
    pub host: Option<PairedHost>,
}

impl RequestSender {
    /// The host object it is paired with, if any.
    pub fn host_id(&self) -> Option<HostId> {
        Some(self.host?.id)
    }
}

/// The host object a guest object is paired with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairedHost {
    pub id: HostId,
    /// The host object's own version, which a bridged object's may not
    /// share with its guest object.
    pub version: u32,
}

/// The sender of a host event, resolved once, before anything of the
/// message is read ([`ShadowTable::resolve_event`]): a live host object.
/// Its handler gets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventSender {
    pub host: HostId,
    /// Its own interface, which events from it decode by.
    pub iface: Iface,
    /// Its negotiated version.
    pub version: u32,
    /// The guest object it is paired with, or `None` for an object only
    /// the proxy uses.
    pub guest: Option<PairedGuest>,
}

impl EventSender {
    /// The guest object it is paired with, if any.
    pub fn guest_id(&self) -> Option<GuestId> {
        Some(self.guest?.id)
    }
}

/// The guest object a host object is paired with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairedGuest {
    pub id: GuestId,
    /// Whether the guest destroyed it, and it awaits the host's
    /// `delete_id` (see [`ShadowTable::guest_destroyed`]).
    pub destroyed: bool,
}

/// An object argument of a guest request, resolved before its handler runs
/// ([`ShadowTable::request_object`]): a live guest object of the interface
/// the argument names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestObject {
    pub guest: GuestId,
    /// What it stands for on the host.
    pub host: Link,
}

/// An object argument of a host event, resolved before its handler runs
/// ([`ShadowTable::event_object`]): a host object of the interface the
/// argument names, paired with a live guest object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventObject {
    pub host: HostId,
    pub guest: GuestId,
}

/// The interface and negotiated version of an object on one side.
#[derive(Debug, Clone, Copy)]
struct Interface {
    iface: Iface,
    version: u32,
}

impl Interface {
    fn new(iface: Iface, version: u32) -> Self {
        Self { iface, version }
    }
}

/// An object the guest has.
#[derive(Debug)]
enum GuestObject {
    /// An object with no host counterpart, which the proxy implements
    /// ([`ShadowTable::create_local`]), and its data.
    Local(Interface, ObjectData),
    /// An object paired with a host object, which records the pairing too.
    Paired {
        host: HostId,
        interface: Interface,
        state: GuestState,
    },
}

#[derive(Debug)]
enum GuestState {
    /// The object is live, and keeps its data.
    Live(ObjectData),
    /// The guest destroyed the object. It rejects requests, but stays paired
    /// until the host's `delete_id` (or the event that ends it), so late
    /// events still resolve.
    DestroyPending,
}

impl GuestObject {
    fn interface(&self) -> &Interface {
        match self {
            Self::Local(interface, _) | Self::Paired { interface, .. } => interface,
        }
    }

    /// The data of a live object.
    fn data(&self) -> Option<&ObjectData> {
        match self {
            Self::Local(_, data)
            | Self::Paired {
                state: GuestState::Live(data),
                ..
            } => Some(data),
            Self::Paired {
                state: GuestState::DestroyPending,
                ..
            } => None,
        }
    }

    /// [`Self::data`], mutably.
    fn data_mut(&mut self) -> Option<&mut ObjectData> {
        match self {
            Self::Local(_, data)
            | Self::Paired {
                state: GuestState::Live(data),
                ..
            } => Some(data),
            Self::Paired {
                state: GuestState::DestroyPending,
                ..
            } => None,
        }
    }

    fn host(&self) -> Option<HostId> {
        match self {
            Self::Local(..) => None,
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
    guests: BTreeMap<GuestId, GuestObject>,
    /// Every host ID in use. A host `new_id` may not name one.
    hosts: BTreeMap<HostId, HostObject>,
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
    /// Whether [`Self::event_object`] has warned that the host named an
    /// object of another interface; later ones log at debug level.
    warned_wrong_interface: Cell<bool>,
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

    pub fn new() -> Self {
        Self {
            guests: BTreeMap::new(),
            hosts: BTreeMap::new(),
            host_ids: IdAllocator::new(Self::CLIENT_IDS),
            guest_server_ids: IdAllocator::new(Self::SERVER_IDS),
            warned_wrong_interface: Cell::new(false),
        }
    }

    /// Allocate an ID for a new proxy object on the host connection.
    ///
    /// The ID is free on the host: either never used, or released by the
    /// host's `wl_display.delete_id`. No entry holds it: the allocator hands
    /// out each ID once until it is released, and the host creates objects
    /// only with server IDs ([`Self::host_new_id`]).
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
        let interface = || Interface::new(Iface::WlDisplay, Iface::WlDisplay.version());
        self.insert_pair(GuestId::DISPLAY, interface(), HostId::DISPLAY, interface());
    }

    /// Create guest object `guest` and a host object for it, paired, each
    /// with `interface` and its own version.
    ///
    /// # Returns
    ///
    /// The host object's ID, newly allocated.
    pub fn create_paired(
        &mut self,
        guest: GuestId,
        interface: Iface,
        version: u32,
        host_version: u32,
    ) -> HostId {
        self.create_bridged(guest, interface, version, interface, host_version)
    }

    /// Like [`Self::create_paired`], for a guest object backed by a host
    /// object of another interface: a guest `zwp_text_input_v3` backed by a
    /// host `zwp_text_input_v1`.
    pub fn create_bridged(
        &mut self,
        guest: GuestId,
        interface: Iface,
        version: u32,
        host_interface: Iface,
        host_version: u32,
    ) -> HostId {
        let host = self.allocate_host_id();
        self.insert_pair(
            guest,
            Interface::new(interface, version),
            host,
            Interface::new(host_interface, host_version),
        );
        host
    }

    /// Create a guest object for host object `host`, which a host event
    /// just created, with a fresh ID from the guest's server range; both
    /// take `interface` and `version`.
    ///
    /// # Returns
    ///
    /// The guest object's ID.
    pub fn adopt_host_created(&mut self, host: HostId, interface: Iface, version: u32) -> GuestId {
        let guest = self.allocate_guest_server_id();
        let interface = || Interface::new(interface, version);
        self.insert_pair(guest, interface(), host, interface());
        guest
    }

    /// Create guest object `guest`, which the proxy implements itself: it
    /// has no host object, and its handlers consume its requests (the SHM
    /// shim, the v3 text-input manager, or an inert `wl_output` whose bind
    /// crossed its global's removal).
    pub fn create_local(&mut self, guest: GuestId, interface: Iface, version: u32) {
        let local = GuestObject::Local(Interface::new(interface, version), ObjectData::None);
        let previous = self.guests.insert(guest, local);
        debug_assert!(previous.is_none(), "guest ID {guest} is in use");
    }

    /// [`Self::create_local`], with the data the object keeps until it
    /// ends, of its interface.
    pub(crate) fn create_local_with<T: Data>(
        &mut self,
        guest: GuestId,
        interface: Iface,
        version: u32,
        data: T,
    ) {
        debug_assert_eq!(interface, T::IFACE, "guest {guest} keeps another's data");
        let local = GuestObject::Local(Interface::new(interface, version), data.into());
        let previous = self.guests.insert(guest, local);
        debug_assert!(previous.is_none(), "guest ID {guest} is in use");
    }

    /// Create a host object that only the proxy uses.
    ///
    /// # Returns
    ///
    /// Its ID, newly allocated.
    pub fn create_host_only(&mut self, interface: Iface, version: u32) -> HostId {
        let host = self.allocate_host_id();
        let object = HostObject::HostOnly(Interface::new(interface, version));
        self.hosts.insert(host, object);
        host
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
            state: GuestState::Live(ObjectData::None),
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
    // arguments once, which validates, translates and types them together.
    // These are the only ways a wire value becomes an ID.

    /// Resolve the sender of a guest request, a live guest object, with
    /// every fact dispatch needs of it.
    ///
    /// `None` when `guest_id` names no object, or one the guest destroyed.
    pub fn resolve_request(&self, guest_id: u32) -> Option<RequestSender> {
        let guest = GuestId(guest_id);
        let object = self.guests.get(&guest)?;
        let Interface { iface, version } = *object.interface();
        let host = match object {
            GuestObject::Local(..) => None,
            GuestObject::Paired {
                host,
                state: GuestState::Live(_),
                ..
            } => {
                let version = self.hosts.get(host)?.interface()?.version;
                Some(PairedHost { id: *host, version })
            }
            GuestObject::Paired {
                state: GuestState::DestroyPending,
                ..
            } => return None,
        };
        Some(RequestSender {
            guest,
            iface,
            version,
            host,
        })
    }

    /// Resolve the sender of a host event, a live host object, with every
    /// fact dispatch needs of it.
    ///
    /// `None` when `host_id` names no object, or a retired one.
    pub fn resolve_event(&self, host_id: u32) -> Option<EventSender> {
        let host = HostId(host_id);
        let object = self.hosts.get(&host)?;
        let Interface { iface, version } = *object.interface()?;
        let guest = object.guest().map(|guest| PairedGuest {
            id: guest,
            destroyed: self.is_pending_destroy_guest(guest),
        });
        Some(EventSender {
            host,
            iface,
            version,
            guest,
        })
    }

    /// Resolve an object argument of a guest request, which must be a live
    /// guest object of `interface`: Wayland object IDs are not
    /// interchangeable, and forwarding a `wl_surface` where a `wl_seat` is
    /// expected would send a type-invalid request to the host.
    ///
    /// `None` when the request may not name `guest_id` there.
    pub fn request_object(&self, guest_id: u32, interface: Iface) -> Option<RequestObject> {
        let guest = GuestId(guest_id);
        let host = self.live_object(guest, interface)?;
        Some(RequestObject { guest, host })
    }

    /// Resolve an object argument of host event `event` (such as
    /// `wl_surface.enter`), which must be a host object of `interface`
    /// paired with a live guest object.
    ///
    /// An object the guest has destroyed is a race the event lost. An
    /// object of another interface is the host breaking the protocol: the
    /// first on a connection warns, later ones log at debug level.
    ///
    /// # Returns
    ///
    /// The object, or `None` when the event may not reach the guest naming
    /// `host_id` there.
    pub fn event_object(&self, host_id: u32, interface: Iface, event: &str) -> Option<EventObject> {
        let host = HostId(host_id);
        let HostObject::Paired {
            guest,
            interface: own,
        } = self.hosts.get(&host)?
        else {
            return None;
        };
        if own.iface != interface {
            if self.warned_wrong_interface.replace(true) {
                debug!(
                    "Dropping host event {event}: it names {} {host_id} as a {interface}",
                    own.iface
                );
            } else {
                warn!(
                    "Dropping host event {event}: it names {} {host_id} as a {interface}, which breaks the protocol",
                    own.iface
                );
            }
            return None;
        }
        (!self.is_pending_destroy_guest(*guest)).then_some(EventObject {
            host,
            guest: *guest,
        })
    }

    /// The guest object paired with the host object that a host event names
    /// as `host_id` in an argument of no interface, which may name any
    /// object (`wl_display.error`'s).
    pub fn guest_named_by_host(&self, host_id: u32) -> Option<GuestId> {
        self.hosts.get(&HostId(host_id))?.guest()
    }

    /// Resolve the `new_id` of a guest request: an ID the guest may create
    /// an object under.
    ///
    /// Client-created IDs must be in [`Self::CLIENT_IDS`], as
    /// libwayland-server requires, and must not reuse an existing object.
    /// Keeping this check in the shadow table lets generated protocol
    /// dispatchers validate every `new_id` request before a handler can
    /// mutate state.
    ///
    /// `None` when the request may not create `guest_id`.
    pub fn guest_new_id(&self, guest_id: u32) -> Option<GuestId> {
        let guest = GuestId(guest_id);
        (Self::CLIENT_IDS.contains(&guest_id) && !self.guests.contains_key(&guest)).then_some(guest)
    }

    /// Resolve the `new_id` of a host event: an ID from [`Self::SERVER_IDS`],
    /// which libwayland-client accepts, that no object holds.
    ///
    /// libwayland-server allocates every object it creates from the server
    /// range. An ID from the client range is one this proxy allocates, so a
    /// host that sent one could later collide with the proxy's own objects.
    ///
    /// `None` when the event may not create `host_id`.
    pub fn host_new_id(&self, host_id: u32) -> Option<HostId> {
        let host = HostId(host_id);
        (Self::SERVER_IDS.contains(&host_id) && !self.hosts.contains_key(&host)).then_some(host)
    }

    /// What live guest object `guest` of `interface` stands for on the host.
    ///
    /// `None` when `guest` names no live object of `interface`.
    pub fn live_object(&self, guest: GuestId, interface: Iface) -> Option<Link> {
        self.link(guest)
            .filter(|_| self.get_interface(guest) == Some(interface))
    }

    /// What live guest object `guest` stands for on the host.
    ///
    /// `None` when `guest` names no object, or one the guest destroyed.
    pub fn link(&self, guest: GuestId) -> Option<Link> {
        match self.guests.get(&guest)? {
            GuestObject::Local(..) => Some(Link::Local),
            GuestObject::Paired {
                host,
                state: GuestState::Live(_),
                ..
            } => Some(Link::Host(*host)),
            GuestObject::Paired {
                state: GuestState::DestroyPending,
                ..
            } => None,
        }
    }

    /// Return whether guest object `guest` is one the proxy implements
    /// itself ([`Self::create_local`]).
    pub fn is_local(&self, guest: GuestId) -> bool {
        matches!(self.guests.get(&guest), Some(GuestObject::Local(..)))
    }

    // Per-object data (`state::object_data`).

    /// The data of kind `T` of live guest object `guest`, if it keeps it.
    pub(crate) fn data<T: Data>(&self, guest: GuestId) -> Option<&T> {
        T::of(self.guests.get(&guest)?.data()?)
    }

    /// [`Self::data`], mutably.
    pub(crate) fn data_mut<T: Data>(&mut self, guest: GuestId) -> Option<&mut T> {
        T::of_mut(self.guests.get_mut(&guest)?.data_mut()?)
    }

    /// Every live guest object that keeps data of kind `T`, in ID order.
    pub(crate) fn data_of<T: Data + 'static>(&self) -> impl Iterator<Item = (GuestId, &T)> {
        self.guests
            .iter()
            .filter_map(|(&guest, object)| Some((guest, T::of(object.data()?)?)))
    }

    /// [`Self::data_of`], mutably.
    pub(crate) fn data_of_mut<T: Data + 'static>(
        &mut self,
    ) -> impl Iterator<Item = (GuestId, &mut T)> {
        self.guests
            .iter_mut()
            .filter_map(|(&guest, object)| Some((guest, T::of_mut(object.data_mut()?)?)))
    }

    /// Give live guest object `guest`, of `T`'s interface, its data, which
    /// it keeps until it ends.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest` is a live object of `T`'s
    /// interface that keeps no data yet.
    #[must_use = "a refused attach means the proxy's bookkeeping diverged"]
    pub(crate) fn attach<T: Data>(&mut self, guest: GuestId, data: T) -> bool {
        self.attach_data(guest, data.into())
    }

    /// Drop the data of guest object `guest`, for a test that injects a
    /// fault no peer can cause: the proxy loses an object's state.
    #[cfg(test)]
    pub(crate) fn drop_data_for_test(&mut self, guest: GuestId) {
        self.take_data(guest);
    }

    /// [`Self::attach`] data of any kind, which must be of `guest`'s
    /// interface.
    pub(crate) fn attach_data(&mut self, guest: GuestId, data: ObjectData) -> bool {
        let Some(object) = self.guests.get_mut(&guest) else {
            return false;
        };
        if data.iface() != Some(object.interface().iface) {
            return false;
        }
        match object.data_mut() {
            Some(slot @ ObjectData::None) => {
                *slot = data;
                true
            }
            _ => false,
        }
    }

    /// Take the data of guest object `guest`, which is ending, for its
    /// owner to forget (`state::lifecycle`).
    pub(super) fn take_data(&mut self, guest: GuestId) -> ObjectData {
        self.guests
            .get_mut(&guest)
            .and_then(GuestObject::data_mut)
            .map(std::mem::take)
            .unwrap_or_default()
    }

    // Queries.

    /// The host object paired with guest object `guest`.
    pub fn get_host_id(&self, guest: GuestId) -> Option<HostId> {
        self.guests.get(&guest)?.host()
    }

    /// The guest object paired with host object `host`.
    pub fn get_guest_id(&self, host: HostId) -> Option<GuestId> {
        self.hosts.get(&host)?.guest()
    }

    /// The guest object's interface.
    pub fn get_interface(&self, guest: GuestId) -> Option<Iface> {
        Some(self.guests.get(&guest)?.interface().iface)
    }

    /// The interface of a live host object, which events from it are
    /// dispatched by.
    pub fn get_host_interface(&self, host: HostId) -> Option<Iface> {
        Some(self.hosts.get(&host)?.interface()?.iface)
    }

    /// Return whether `guest` belongs to the range reserved for objects
    /// created by the server.
    ///
    /// The client destroys these objects but never receives `delete_id` for
    /// them: the server, not the client, owns their raw ID lifecycle.
    pub fn is_guest_server_id(&self, guest: GuestId) -> bool {
        guest.0 >= Self::GUEST_SERVER_ID_START
    }

    pub fn is_pending_destroy_guest(&self, guest: GuestId) -> bool {
        matches!(
            self.guests.get(&guest),
            Some(GuestObject::Paired {
                state: GuestState::DestroyPending,
                ..
            })
        )
    }

    /// The guest object's version.
    pub fn guest_object_version(&self, guest: GuestId) -> Option<u32> {
        Some(self.guests.get(&guest)?.interface().version)
    }

    /// The live host object's version.
    pub fn host_object_version(&self, host: HostId) -> Option<u32> {
        Some(self.hosts.get(&host)?.interface()?.version)
    }

    // Lifecycle. Each transition is named after what happened and changes
    // both sides of a pairing at once. Those that end an object are visible
    // only in `state`: everything else ends one through its `Context`
    // wrapper (`state::lifecycle`), which forgets the object's state.

    /// The guest destroyed paired guest object `guest`, and its destructor
    /// went to the host object.
    ///
    /// An object the guest created stays paired until the host's
    /// `delete_id`, rejecting requests, so late events still resolve and the
    /// `delete_id` reaches the guest. An object the server created gets no
    /// `delete_id`: libwayland frees its ID on both sides as soon as either
    /// side destroys it, so both entries end now.
    pub(super) fn guest_destroyed(&mut self, guest: GuestId) {
        if !self.is_guest_server_id(guest) {
            self.guest_destroyed_in_use(guest);
            return;
        }
        if let Some(host) = self.end_paired_guest(guest) {
            self.remove_paired_host(host, guest);
        }
    }

    /// The guest destroyed paired guest object `guest` while its host object
    /// is still in use, as a buffer the host still shows: the proxy destroys
    /// the host object later, through [`Self::guest_destroyed`].
    ///
    /// Until then the guest object rejects requests, but stays paired so
    /// the host object's events still reach its handler.
    pub(super) fn guest_destroyed_in_use(&mut self, guest: GuestId) {
        if let Some(GuestObject::Paired { state, .. }) = self.guests.get_mut(&guest) {
            debug_assert!(
                matches!(
                    state,
                    GuestState::DestroyPending | GuestState::Live(ObjectData::None)
                ),
                "guest {guest} ends before its data is forgotten"
            );
            *state = GuestState::DestroyPending;
        }
    }

    /// Guest object `guest` ended without its destructor reaching its host
    /// object, which has none (a v3 text input backed by a host v1 one): the
    /// host object stays in use as a host-only object, so its ID is not
    /// reused while its events are in flight.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest` is paired.
    pub(super) fn guest_ended_host_kept(&mut self, guest: GuestId) -> bool {
        let Some(host) = self.end_paired_guest(guest) else {
            return false;
        };
        if let Some(HostObject::Paired { interface, .. }) = self.hosts.remove(&host) {
            self.hosts.insert(host, HostObject::HostOnly(interface));
        }
        true
    }

    /// Paired guest object `guest` ended, and so did its host object: by a
    /// terminal event (`wl_callback.done`) or a destructor the proxy queued.
    /// The host ID stays reserved until the host's `delete_id`, which then
    /// reaches no guest object.
    pub(super) fn guest_ended_host_destroyed(&mut self, guest: GuestId) {
        let host = self.end_paired_guest(guest);
        debug_assert!(host.is_some(), "guest {guest} is not paired");
        if let Some(host) = host {
            self.retire_destroyed_host(host);
        }
    }

    /// Local guest object `guest` ended: its destructor completed in the
    /// proxy.
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest` is a local object.
    pub(super) fn local_destroyed(&mut self, guest: GuestId) -> bool {
        if !self.is_local(guest) {
            return false;
        }
        self.forget_guest_object(guest);
        true
    }

    /// Forget paired guest object `guest` and its host object, which never
    /// reached the host: nothing on either side knows the IDs, so both are
    /// free at once.
    pub(super) fn abandon(&mut self, guest: GuestId) {
        let host = self
            .forget_guest_object(guest)
            .and_then(|object| object.host());
        debug_assert!(host.is_some(), "guest {guest} is not paired");
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
    /// `false`, changing nothing, unless `host` is a live host-only object.
    pub(super) fn host_destroy_queued(&mut self, host: HostId) -> bool {
        if !matches!(self.hosts.get(&host), Some(HostObject::HostOnly(_))) {
            return false;
        }
        self.retire_destroyed_host(host);
        true
    }

    /// Retire host-only object `host`, which has no destructor, once its
    /// guest object or global is gone: stale events stop reaching it, and
    /// its ID stays reserved for the connection.
    ///
    /// Only a live host-only object is retired. A paired object is not a
    /// binding of the proxy's own, and an ID with no entry has nothing to
    /// keep reserved: reserving it would let it collide with the
    /// allocator's next object. Either is refused with a warning.
    pub fn retire_host(&mut self, host: HostId) {
        match self.hosts.get_mut(&host) {
            Some(object @ HostObject::HostOnly(_)) => *object = HostObject::Retired,
            Some(HostObject::Retired) => {}
            Some(HostObject::Paired { guest, .. }) => {
                warn!("Not retiring host object {host}, which guest object {guest} uses")
            }
            None => warn!("Not retiring host object {host}, which is untracked"),
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
    pub(super) fn release_deleted_host_id(&mut self, host_id: u32) -> DeletedHostId {
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
            return DeletedHostId::HostOnly { host };
        };
        let was_live = !self.is_pending_destroy_guest(guest);
        let paired = self
            .forget_guest_object(guest)
            .and_then(|object| object.host());
        debug_assert_eq!(paired, Some(host), "guest {guest:?} was not paired");
        DeletedHostId::Paired {
            host,
            guest,
            was_live,
        }
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
    /// `interface`, at the newest version its protocol file defines, and
    /// keep both allocators from handing those IDs out.
    ///
    /// # Panics
    ///
    /// When either ID is already in use.
    #[cfg(test)]
    pub(crate) fn insert_paired_for_test(
        &mut self,
        guest: GuestId,
        host: HostId,
        interface: Iface,
    ) {
        self.insert_paired_with_version_for_test(guest, host, interface, interface.version());
    }

    /// Like [`Self::insert_paired_for_test`], with `version` on both sides.
    #[cfg(test)]
    pub(crate) fn insert_paired_with_version_for_test(
        &mut self,
        guest: GuestId,
        host: HostId,
        interface: Iface,
        version: u32,
    ) {
        self.reserve_for_test(guest, host);
        let interface = || Interface::new(interface, version);
        self.insert_pair(guest, interface(), host, interface());
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
        host: HostId,
        interface: Iface,
        version: u32,
    ) {
        self.reserve_host_id_for_test(host);
        let object = HostObject::HostOnly(Interface::new(interface, version));
        self.hosts.insert(host, object);
    }

    /// Change the negotiated version of guest object `guest_id`, for a test
    /// of version-dependent behavior on an object a fixture created.
    #[cfg(test)]
    pub(crate) fn set_guest_version_for_test(&mut self, guest: GuestId, version: u32) {
        let object = self.guests.get_mut(&guest);
        let interface = match object.expect("a guest object") {
            GuestObject::Local(interface, _) | GuestObject::Paired { interface, .. } => interface,
        };
        interface.version = version;
    }

    #[cfg(test)]
    fn reserve_for_test(&mut self, guest: GuestId, host: HostId) {
        assert!(
            !self.guests.contains_key(&guest) && self.guest_server_ids.reserve(guest.0),
            "fixture guest ID {guest} is in use"
        );
        self.reserve_host_id_for_test(host);
    }

    #[cfg(test)]
    fn reserve_host_id_for_test(&mut self, host: HostId) {
        assert!(
            !self.hosts.contains_key(&host) && self.host_ids.reserve(host.0),
            "fixture host ID {host} is in use"
        );
    }

    /// Return whether no object holds host ID `host`.
    #[cfg(test)]
    pub fn is_host_id_available(&self, host: HostId) -> bool {
        host.0 > 1 && !self.hosts.contains_key(&host)
    }

    /// Return whether host ID `host` holds a retired object.
    #[cfg(test)]
    pub fn is_retired_host(&self, host: HostId) -> bool {
        matches!(self.hosts.get(&host), Some(HostObject::Retired))
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
    /// The event a test resolves an object argument for, as logged.
    const EVENT: &str = "wl_pointer.enter";

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
        let binding = table.create_host_only(Iface::ZcrKeyboardExtensionV1, 2);
        let retired = table.create_host_only(Iface::WlShm, 1);
        table.retire_host(retired);

        assert!(!table.is_host_id_available(binding));
        assert!(!table.is_host_id_available(retired));
        let next = table.allocate_host_id();
        assert!(next != binding && next != retired);
    }

    #[test]
    fn host_created_objects_get_fresh_guest_server_ids() {
        let mut table = ShadowTable::new();
        let first = table.adopt_host_created(HostId(SERVER + 40), Iface::WlDataOffer, 3);
        let second = table.adopt_host_created(HostId(SERVER + 41), Iface::WlDataOffer, 3);

        assert_eq!((first, second), (GuestId(SERVER), GuestId(SERVER + 1)));
        assert_eq!(table.get_host_id(second), Some(HostId(SERVER + 41)));
        assert_eq!(table.guest_object_version(second), Some(3));
        assert_eq!(table.host_object_version(HostId(SERVER + 41)), Some(3));
    }

    #[test]
    fn guest_server_ids_are_reused_and_stay_below_the_client_map_limit() {
        let mut table = ShadowTable::new();
        let first = table.adopt_host_created(HostId(SERVER + 40), Iface::WlBuffer, 1);
        table.guest_destroyed(first);
        assert_eq!(
            table.adopt_host_created(HostId(SERVER + 40), Iface::WlBuffer, 1),
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
        let guest = table.adopt_host_created(HostId(SERVER + 40), Iface::ZwpTextInputV3, 1);
        assert!(table.guest_ended_host_kept(guest));
        assert_eq!(
            table.adopt_host_created(HostId(SERVER + 41), Iface::WlBuffer, 1),
            guest
        );

        // The guest destroys a buffer the host still uses: it keeps its ID
        // until the proxy destroys the host buffer, and frees it once.
        table.guest_destroyed_in_use(guest);
        let next = table.adopt_host_created(HostId(SERVER + 42), Iface::WlBuffer, 1);
        assert_ne!(next, guest, "a retired object still owns its ID");
        table.guest_destroyed(guest);
        table.guest_destroyed(guest);
        assert_eq!(
            table.adopt_host_created(HostId(SERVER + 43), Iface::WlBuffer, 1),
            guest
        );
        assert_eq!(
            table.adopt_host_created(HostId(SERVER + 44), Iface::WlBuffer, 1),
            GuestId(next.0 + 1)
        );
    }

    #[test]
    fn guest_id_availability_rejects_reserved_and_reused_ids() {
        let mut table = ShadowTable::new();
        assert!(!table.guest_new_id(0).is_some());
        assert!(!table.guest_new_id(1).is_some());
        assert!(table.guest_new_id(2).is_some());
        // libwayland-server refuses a client ID past WL_MAP_MAX_OBJECTS.
        assert!(table.guest_new_id(WL_MAP_MAX_OBJECTS).is_some());
        assert!(!table.guest_new_id(WL_MAP_MAX_OBJECTS + 1).is_some());
        assert!(!table.guest_new_id(SERVER - 1).is_some());
        assert!(!table.guest_new_id(SERVER).is_some());

        table.create_paired(GuestId(2), Iface::WlSurface, 1, 1);
        assert!(!table.guest_new_id(2).is_some());
        table.create_local(GuestId(3), Iface::WlShm, 1);
        assert!(!table.guest_new_id(3).is_some());
    }

    #[test]
    fn host_new_ids_must_be_fresh_server_ids() {
        let mut table = ShadowTable::new();
        assert!(table.host_new_id(SERVER).is_some());
        assert!(!table.host_new_id(0).is_some());
        assert!(
            !table.host_new_id(30).is_some(),
            "the client range is the proxy's"
        );
        // libwayland-client refuses a server index past WL_MAP_MAX_OBJECTS.
        assert!(table.host_new_id(SERVER + WL_MAP_MAX_OBJECTS).is_some());
        assert!(!table.host_new_id(SERVER + WL_MAP_MAX_OBJECTS + 1).is_some());
        assert!(!table.host_new_id(u32::MAX).is_some());

        let guest = table.adopt_host_created(HostId(SERVER), Iface::WlDataOffer, 3);
        assert!(!table.host_new_id(SERVER).is_some());
        table.guest_destroyed(guest);
        assert!(table.host_new_id(SERVER).is_some());
    }

    #[test]
    fn event_sender_is_known_for_paired_and_internal_objects_only() {
        let mut table = ShadowTable::new();
        assert!(!table.get_host_interface(HostId(90)).is_some());

        let paired = table.create_paired(GuestId(10), Iface::WlSurface, 1, 1);
        assert!(table.get_host_interface(paired).is_some());
        assert!(!table.get_host_interface(HostId(10)).is_some());

        let binding = table.create_host_only(Iface::WlShm, 1);
        assert!(table.get_host_interface(binding).is_some());
        assert!(table.host_destroy_queued(binding));
        assert!(!table.get_host_interface(binding).is_some());
    }

    /// Only a live paired or local object can send a request, and an object
    /// argument must also have the interface the request expects.
    #[test]
    fn requests_resolve_live_objects_of_the_expected_interface() {
        let mut table = ShadowTable::new();
        table.create_local(GuestId(20), Iface::WlShm, 1);
        let host = table.create_paired(GuestId(21), Iface::WlSurface, 4, 4);

        assert_eq!(table.link(GuestId(20)), Some(Link::Local));
        assert_eq!(table.link(GuestId(21)), Some(Link::Host(host)));
        assert_eq!(table.link(GuestId(22)), None);
        let surface = RequestObject {
            guest: GuestId(21),
            host: Link::Host(host),
        };
        assert_eq!(table.request_object(21, Iface::WlSurface), Some(surface));
        assert_eq!(table.request_object(21, Iface::WlSeat), None);

        table.guest_destroyed(GuestId(21));
        assert_eq!(table.link(GuestId(21)), None);
        assert_eq!(table.request_object(21, Iface::WlSurface), None);
        assert!(table.local_destroyed(GuestId(20)));
        assert_eq!(table.link(GuestId(20)), None);
        assert!(!table.local_destroyed(GuestId(20)), "an object ends once");
    }

    #[test]
    fn events_name_paired_objects_whose_guest_object_is_live() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(21), Iface::WlSurface, 4, 4);
        let binding = table.create_host_only(Iface::WlShm, 1);

        let surface = EventObject {
            host,
            guest: GuestId(21),
        };
        assert_eq!(
            table.event_object(host.0, Iface::WlSurface, EVENT),
            Some(surface)
        );
        assert_eq!(table.event_object(host.0, Iface::WlOutput, EVENT), None);
        assert_eq!(
            table.event_object(binding.0, Iface::WlShm, EVENT),
            None,
            "no guest object"
        );

        table.guest_destroyed(GuestId(21));
        assert_eq!(table.event_object(host.0, Iface::WlSurface, EVENT), None);
    }

    /// A host event that names an object of another interface breaks the
    /// protocol, unlike one that names a stale object, which is a race: the
    /// connection warns of the first.
    #[test]
    fn an_event_object_of_another_interface_is_warned_of() {
        let mut table = ShadowTable::new();
        let stale = table.create_paired(GuestId(21), Iface::WlSurface, 4, 4);
        table.guest_destroyed(GuestId(21));
        let host = table.create_paired(GuestId(22), Iface::WlSurface, 4, 4);

        assert_eq!(table.event_object(stale.0, Iface::WlSurface, EVENT), None);
        assert!(!table.warned_wrong_interface.get());
        assert_eq!(table.event_object(host.0, Iface::WlOutput, EVENT), None);
        assert!(table.warned_wrong_interface.get());
    }

    #[test]
    fn object_versions_are_tracked_on_both_sides_of_a_pairing() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(20), Iface::WlSurface, 4, 3);

        assert_eq!(table.guest_object_version(GuestId(20)), Some(4));
        assert_eq!(table.host_object_version(host), Some(3));
        assert_eq!(table.guest_object_version(GuestId(99)), None);

        table.guest_destroyed(GuestId(20));
        table.release_deleted_host_id(host.0);
        assert_eq!(table.guest_object_version(GuestId(20)), None);
        assert_eq!(table.host_object_version(host), None);
    }

    #[test]
    fn a_guest_ended_with_its_host_kept_leaves_a_reserved_host_object() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(20), Iface::WlCallback, 1, 1);

        assert!(table.guest_ended_host_kept(GuestId(20)));
        assert_eq!(table.get_host_id(GuestId(20)), None);
        assert_eq!(table.get_host_interface(host), Some(Iface::WlCallback));
        assert_ne!(
            table.allocate_host_id(),
            host,
            "a host object without a guest destructor must stay reserved"
        );
        assert!(
            !table.guest_ended_host_kept(GuestId(20)),
            "the guest is gone"
        );
    }

    #[test]
    fn a_guest_ended_with_its_host_destroyed_awaits_delete_id() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(20), Iface::WlCallback, 1, 1);

        table.guest_ended_host_destroyed(GuestId(20));
        assert!(table.guest_new_id(20).is_some());
        assert!(table.is_retired_host(host));
        assert!(!table.get_host_interface(host).is_some());
        assert_eq!(
            table.release_deleted_host_id(host.0),
            DeletedHostId::HostOnly { host }
        );
        assert!(table.is_host_id_available(host));
    }

    #[test]
    fn a_guest_destroyed_in_use_still_routes_host_events() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(20), Iface::WlBuffer, 1, 1);

        table.guest_destroyed_in_use(GuestId(20));

        assert_eq!(
            table.get_host_interface(host),
            Some(Iface::WlBuffer),
            "a delayed wl_buffer.release must still be dispatchable after guest destroy"
        );
        assert!(table.get_host_interface(host).is_some());
        assert_eq!(table.host_object_version(host), Some(1));
        assert!(table.is_pending_destroy_guest(GuestId(20)));
        assert_eq!(table.link(GuestId(20)), None);
    }

    #[test]
    fn host_only_destructor_reservation_survives_until_delete_id() {
        let mut table = ShadowTable::new();
        table.insert_host_only_for_test(HostId(40), Iface::ZcrExtendedKeyboardV1, 2);

        // The destroy request has been queued, but the host has not processed
        // it yet. The dispatch metadata is retired immediately while the
        // numeric ID remains unavailable for reuse.
        assert!(table.host_destroy_queued(HostId(40)));
        assert!(table.is_retired_host(HostId(40)));
        assert!(!table.get_host_interface(HostId(40)).is_some());
        assert!(!table.is_host_id_available(HostId(40)));
        assert!(!table.host_destroy_queued(HostId(40)), "it is retired once");

        // The host acknowledgement is the only point at which the reservation
        // can be released.
        assert_eq!(
            table.release_deleted_host_id(40),
            DeletedHostId::HostOnly { host: HostId(40) }
        );
        assert!(!table.is_retired_host(HostId(40)));
        assert!(table.is_host_id_available(HostId(40)));
    }

    #[test]
    fn destroyed_host_allocated_object_frees_its_id_at_once() {
        // The host allocates IDs from the server range and sends no
        // delete_id for them; it may reuse one for its next new object.
        let mut table = ShadowTable::new();
        let host_buffer = HostId(SERVER + 5);
        table.insert_host_only_for_test(host_buffer, Iface::WlBuffer, 1);

        assert!(table.host_destroy_queued(host_buffer));
        assert!(!table.get_host_interface(host_buffer).is_some());
        assert!(
            table.host_new_id(host_buffer.0).is_some(),
            "the host's next server-created object may reuse the ID"
        );
    }

    #[test]
    fn host_ids_are_reused_only_after_delete_id() {
        let mut table = ShadowTable::new();
        let first = table.create_paired(GuestId(10), Iface::WlSurface, 1, 1);
        table.guest_destroyed(GuestId(10));

        let second = table.create_paired(GuestId(11), Iface::WlSurface, 1, 1);
        assert_ne!(second, first, "an ID awaiting delete_id must not be reused");

        assert_eq!(
            table.release_deleted_host_id(first.0),
            DeletedHostId::Paired {
                host: first,
                guest: GuestId(10),
                was_live: false
            }
        );
        assert_eq!(table.get_host_id(GuestId(10)), None);
        assert_eq!(table.get_interface(GuestId(10)), None);
        assert_eq!(
            table.allocate_host_id(),
            first,
            "a deleted ID is reused first"
        );
        assert_eq!(
            table.release_deleted_host_id(first.0 + 100),
            DeletedHostId::Ignored,
            "a delete_id for an ID never allocated changes nothing"
        );
        assert_eq!(table.allocate_host_id(), HostId(second.0 + 1));
    }

    /// The host never destroys a binding of the proxy's own, so a
    /// `delete_id` for one must not free its ID: the ID would go to a new
    /// object while the binding still holds it, and the binding's later
    /// `global_remove` would retire the new object.
    #[test]
    fn a_delete_id_for_a_live_host_only_object_changes_nothing() {
        let mut table = ShadowTable::new();
        // An internal binding without a destructor, such as the host wl_shm.
        let binding = table.create_host_only(Iface::WlShm, 1);

        assert_eq!(
            table.release_deleted_host_id(binding.0),
            DeletedHostId::Ignored
        );
        assert!(table.get_host_interface(binding).is_some());

        // The binding's global goes, and with it the binding only.
        let next = table.create_paired(GuestId(11), Iface::WlSurface, 1, 1);
        assert_ne!(next, binding);
        table.retire_host(binding);
        assert!(!table.get_host_interface(binding).is_some());
        assert_eq!(table.get_host_id(GuestId(11)), Some(next));

        // Retiring a paired object is refused, not fatal.
        table.retire_host(next);
        assert_eq!(table.get_host_id(GuestId(11)), Some(next));
        assert!(table.get_host_interface(next).is_some());
    }

    /// Retiring an ID that holds no object must not reserve it, or the
    /// allocator's next object would collide with the reservation.
    #[test]
    fn retiring_an_untracked_host_id_changes_nothing() {
        let mut table = ShadowTable::new();
        table.retire_host(HostId(2));
        assert!(table.is_host_id_available(HostId(2)));
        assert_eq!(table.create_host_only(Iface::WlShm, 1), HostId(2));
    }

    /// An object whose creation never reached the host frees its host ID
    /// along with its guest ID, so the ID space stays dense.
    #[test]
    fn an_abandoned_object_frees_both_ids() {
        let mut table = ShadowTable::new();
        let host = table.create_paired(GuestId(10), Iface::WlOutput, 3, 3);

        table.abandon(GuestId(10));
        assert!(table.guest_new_id(10).is_some());
        assert!(table.is_host_id_available(host));
        assert_eq!(table.allocate_host_id(), host, "the host ID is reused");
    }

    #[test]
    fn fixture_ids_are_never_handed_out() {
        let mut table = ShadowTable::new();
        let server_guest = SERVER + 1;
        table.insert_paired_for_test(GuestId(10), HostId(3), Iface::WlSurface);
        table.insert_paired_for_test(GuestId(server_guest), HostId(4), Iface::WlDataOffer);
        assert_eq!(table.get_host_id(GuestId(10)), Some(HostId(3)));
        assert_eq!(
            table.get_interface(GuestId(server_guest)),
            Some(Iface::WlDataOffer)
        );

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
        let host = table.create_bridged(
            GuestId(20),
            Iface::ZwpTextInputV3,
            1,
            Iface::ZwpTextInputV1,
            1,
        );

        assert_eq!(
            table.get_interface(GuestId(20)),
            Some(Iface::ZwpTextInputV3)
        );
        assert_eq!(table.get_host_interface(host), Some(Iface::ZwpTextInputV1));
        let text_input = EventObject {
            host,
            guest: GuestId(20),
        };
        assert_eq!(
            table.event_object(host.0, Iface::ZwpTextInputV1, EVENT),
            Some(text_input)
        );
        assert_eq!(
            table.event_object(host.0, Iface::ZwpTextInputV3, EVENT),
            None
        );

        // Once the guest object goes, the host object keeps its own
        // interface, routes its events and stays reserved.
        assert!(table.guest_ended_host_kept(GuestId(20)));
        assert_eq!(table.get_host_interface(host), Some(Iface::ZwpTextInputV1));
        assert!(table.get_host_interface(host).is_some());
        assert!(!table.is_host_id_available(host));
    }

    #[test]
    fn retiring_a_host_object_awaiting_delete_id_keeps_its_id_reserved() {
        let mut table = ShadowTable::new();
        let host_id = table.create_host_only(Iface::ZauraOutput, 38);
        assert!(table.host_destroy_queued(host_id));

        table.retire_host(host_id);
        assert!(table.is_retired_host(host_id));
        assert!(!table.get_host_interface(host_id).is_some());
        assert!(!table.is_host_id_available(host_id));
        assert_ne!(table.allocate_host_id(), host_id);

        // The host's delete_id still frees it.
        assert_eq!(
            table.release_deleted_host_id(host_id.0),
            DeletedHostId::HostOnly { host: host_id }
        );
        assert!(table.is_host_id_available(host_id));
    }
}
