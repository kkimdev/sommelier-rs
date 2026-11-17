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
use super::{GuestId, HostId, INERT_BIND_INTERFACE};
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

/// The interface and negotiated version of a guest object.
#[derive(Debug)]
struct Interface {
    name: String,
    version: u32,
}

/// An object the guest has.
#[derive(Debug)]
enum GuestObject {
    /// An object with no host counterpart, which the proxy implements
    /// ([`ShadowTable::is_local_only_guest_object`]). A handler that tracks
    /// a new object before its dispatcher pairs it (`wl_seat.get_keyboard`)
    /// makes one for the rest of that dispatch.
    Local(Interface),
    /// An object paired with a host object, which records the pairing too.
    Paired {
        host: HostId,
        /// `None` until the interface is tracked, and once
        /// [`ShadowTable::retire_guest_object`] has moved it to the host
        /// object.
        interface: Option<Interface>,
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
    fn interface(&self) -> Option<&Interface> {
        match self {
            Self::Local(interface) => Some(interface),
            Self::Paired { interface, .. } => interface.as_ref(),
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
    /// `interface` and `version` are the host object's own, when recorded:
    /// an interface that differs from the guest's (a v3 text input backed
    /// by v1), or the guest's once [`ShadowTable::retire_guest_object`] has
    /// moved it here. A missing version falls back to the guest object's,
    /// and an event may name the object by either interface.
    Paired {
        guest: GuestId,
        interface: Option<String>,
        version: Option<u32>,
    },
    /// A live object that only the proxy uses.
    HostOnly {
        interface: String,
        version: Option<u32>,
    },
    /// An object whose destructor is queued, until the host's `delete_id`.
    AwaitingDeleteId,
    /// An object without a destructor that nothing uses any more. Nothing
    /// frees its ID but a `delete_id` the host sends anyway.
    ReservedForConnection,
}

impl HostObject {
    fn guest(&self) -> Option<GuestId> {
        match self {
            Self::Paired { guest, .. } => Some(*guest),
            _ => None,
        }
    }

    /// The object's own interface.
    fn interface(&self) -> Option<&String> {
        match self {
            Self::Paired { interface, .. } => interface.as_ref(),
            Self::HostOnly { interface, .. } => Some(interface),
            Self::AwaitingDeleteId | Self::ReservedForConnection => None,
        }
    }

    /// The object's own version.
    fn version(&self) -> Option<u32> {
        match self {
            Self::Paired { version, .. } | Self::HostOnly { version, .. } => *version,
            Self::AwaitingDeleteId | Self::ReservedForConnection => None,
        }
    }

    /// Whether the object can still send events.
    fn is_live(&self) -> bool {
        matches!(self, Self::Paired { .. } | Self::HostOnly { .. })
    }
}

/// The objects of one connection on each side of the proxy, and how they
/// pair up.
///
/// Each side has one entry per object, and a pairing is recorded in both
/// entries. Guest and host IDs are independent: only [`Self::map_id`] pairs
/// two objects, and every change that ends a pairing changes both entries.
pub struct ShadowTable {
    /// Every object the guest has.
    guests: HashMap<GuestId, GuestObject>,
    /// Every host ID in use: [`Self::allocate_host_id`] skips these, and a
    /// host `new_id` may not name one.
    hosts: HashMap<HostId, HostObject>,
    /// The IDs this proxy allocates for its objects on the host connection.
    /// An ID returns here only through [`Self::release_deleted_host_id`].
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
    /// The version of an object registered without a negotiated version.
    ///
    /// Test fixtures and legacy internal registrations that do not carry a
    /// negotiated version remain permissive. Production-created objects are
    /// registered with their actual version by the generated dispatcher.
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
    /// host's `wl_display.delete_id`, and no table entry names it.
    ///
    /// # Panics
    ///
    /// When the client holds `WL_MAP_MAX_OBJECTS` live objects, which the
    /// host would refuse anyway. Allocation runs only in a client's own
    /// task, and Tokio confines a task's panic to it, so this ends that
    /// client alone; unwinding drops its connections and descriptors.
    pub fn allocate_host_id(&mut self) -> u32 {
        loop {
            let Some(id) = self.host_ids.next_candidate() else {
                log::error!("Ending the client: its host object IDs are exhausted");
                panic!("host Wayland object IDs exhausted");
            };
            if !self.hosts.contains_key(&HostId(id)) {
                return id;
            }
            // Only an object registered under an ID this allocator did not
            // hand out gets here: a host `new_id` from the client range, or
            // a test fixture. Skipping keeps two objects off one ID; the
            // warning keeps the cause visible.
            warn!("Skipping host ID {id}, which is registered but was never allocated");
        }
    }

    /// Allocate an ID in Wayland's server-generated ID range.
    ///
    /// A host compositor is allowed to choose a different raw ID than the
    /// guest-side proxy can expose. Returning the host ID verbatim can collide
    /// with a guest-created object and makes later requests use the wrong
    /// namespace. Allocate from the guest's own server range instead, and
    /// reserve the chosen ID in the normal guest maps at the call site.
    ///
    /// # Panics
    ///
    /// Like [`Self::allocate_host_id`], when the range is exhausted; that
    /// ends only the client whose task allocates.
    pub fn allocate_guest_server_id(&mut self) -> u32 {
        loop {
            let Some(id) = self.guest_server_ids.next_candidate() else {
                log::error!("Ending the client: its guest server IDs are exhausted");
                panic!("guest Wayland server IDs exhausted");
            };
            if !self.guests.contains_key(&GuestId(id)) {
                return id;
            }
            // As in `allocate_host_id`: only an ID registered without being
            // allocated (a test fixture) gets here.
            warn!("Skipping guest server ID {id}, which is registered but was never allocated");
        }
    }

    /// Pair the hard-coded IDs of a test fixture as one object of
    /// `interface`, with no negotiated version, and keep both allocators
    /// from handing those IDs out.
    ///
    /// # Panics
    ///
    /// When either ID is already in use.
    #[cfg(test)]
    pub(crate) fn insert_paired_for_test(&mut self, guest_id: u32, host_id: u32, interface: &str) {
        assert!(
            self.get_host_id(guest_id).is_none()
                && self.get_interface(guest_id).is_none()
                && self.guest_server_ids.reserve(guest_id),
            "fixture guest ID {guest_id} is in use"
        );
        assert!(
            self.is_host_id_available(host_id) && self.host_ids.reserve(host_id),
            "fixture host ID {host_id} is in use"
        );
        self.map_id(guest_id, host_id);
        self.track_interface(guest_id, interface.to_string());
    }

    /// Pair guest object `guest_id` with host object `host_id`, creating
    /// either entry that does not exist.
    ///
    /// Production pairs only unused IDs; test fixtures also remap paired
    /// ones. The guest object's old host object then keeps its own
    /// interface, without a version, as a host-only object, or ends if it
    /// has none; the host object's old guest object ends.
    pub fn map_id(&mut self, guest_id: u32, host_id: u32) {
        let (guest, host) = (GuestId(guest_id), HostId(host_id));
        if let Some(old_host) = self.host_id_of(guest).filter(|&old_host| old_host != host) {
            if let Some(HostObject::Paired {
                interface: Some(interface),
                ..
            }) = self.remove_paired_host(old_host, guest)
            {
                let host_only = HostObject::HostOnly {
                    interface,
                    version: None,
                };
                self.hosts.insert(old_host, host_only);
            }
        }

        let (interface, version) = match self.hosts.remove(&host) {
            Some(HostObject::Paired {
                guest: old_guest,
                interface,
                version,
            }) => {
                if old_guest != guest {
                    self.forget_guest_object(old_guest);
                }
                (interface, version)
            }
            Some(HostObject::HostOnly { interface, version }) => (Some(interface), version),
            reserved => {
                debug_assert!(reserved.is_none(), "pairing reserved host ID {host_id}");
                (None, None)
            }
        };
        self.hosts.insert(
            host,
            HostObject::Paired {
                guest,
                interface,
                version,
            },
        );

        let (interface, state) = match self.guests.remove(&guest) {
            Some(GuestObject::Paired {
                interface, state, ..
            }) => (interface, state),
            Some(GuestObject::Local(interface)) => (Some(interface), GuestState::Live),
            None => (None, GuestState::Live),
        };
        self.guests.insert(
            guest,
            GuestObject::Paired {
                host,
                interface,
                state,
            },
        );
    }

    pub fn get_host_id(&self, guest_id: u32) -> Option<u32> {
        self.host_id_of(GuestId(guest_id)).map(|host| host.0)
    }

    /// Check that a guest object has the interface required by a request
    /// argument. Wayland object IDs are not interchangeable just because they
    /// happen to be mapped; forwarding a `wl_surface` where a `wl_seat` is
    /// expected would otherwise send a type-invalid request to the host.
    pub fn guest_object_matches(&self, guest_id: u32, interface: &str) -> bool {
        self.get_interface(guest_id)
            .is_some_and(|actual| actual == interface)
    }

    /// Whether the proxy implements guest object `guest_id` itself, with no
    /// host counterpart.
    ///
    /// Its handlers consume the requests locally (the SHM shim, the v3
    /// text-input manager, and an inert `wl_output` whose bind crossed its
    /// global's removal, with its `zxdg_output_v1`), so the generated
    /// dispatcher lets them reach the handler while it rejects every other
    /// unmapped sender before it can mutate state.
    pub fn is_local_only_guest_object(&self, guest_id: u32) -> bool {
        let Some(GuestObject::Local(interface)) = self.guests.get(&GuestId(guest_id)) else {
            return false;
        };
        matches!(
            interface.name.as_str(),
            "wl_shm"
                | "wl_shm_pool"
                // An inert output and the xdg_output created for it.
                | INERT_BIND_INTERFACE
                | "zxdg_output_v1"
                | "zwp_text_input_manager_v3"
                | "zwp_linux_dmabuf_feedback_v1"
                | "gtk_shell1"
                | "gtk_surface1"
        )
    }

    /// Check the interface of a host object carried by an event: its own,
    /// or that of the guest object it is paired with.
    pub fn host_object_matches(&self, host_id: u32, interface: &str) -> bool {
        self.get_host_interface(host_id)
            .is_some_and(|actual| actual == interface)
            || self
                .get_guest_id(host_id)
                .is_some_and(|guest_id| self.guest_object_matches(guest_id, interface))
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

    /// Return whether `guest_id` belongs to the range reserved for objects
    /// created by the server.
    ///
    /// The client destroys these objects but never receives `delete_id` for
    /// them: the server, not the client, owns their raw ID lifecycle. Their
    /// proxy mappings must therefore be released when the destructor is
    /// forwarded instead of waiting for an acknowledgement that cannot arrive.
    pub fn is_guest_server_id(&self, guest_id: u32) -> bool {
        guest_id >= Self::GUEST_SERVER_ID_START
    }

    /// Return whether a raw host object ID can be accepted from a
    /// server-generated `new_id` event.
    ///
    /// Host IDs share one namespace across guest-paired objects and internal
    /// proxy objects. A reused ID would otherwise overwrite the pairing and
    /// route subsequent host events to the wrong guest object.
    pub fn is_host_id_available(&self, host_id: u32) -> bool {
        host_id > 1 && !self.hosts.contains_key(&HostId(host_id))
    }

    pub fn get_guest_id(&self, host_id: u32) -> Option<u32> {
        self.guest_id_of(HostId(host_id)).map(|guest| guest.0)
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
        self.hosts
            .get(&HostId(host_id))
            .is_some_and(HostObject::is_live)
    }

    /// Mark a forwarded destructor as pending host deletion. Retain the
    /// interface metadata so a subsequent request reaches the generated
    /// dispatcher and is rejected explicitly, while retaining the pairing
    /// until the host emits `wl_display.delete_id`.
    pub fn mark_pending_destroy(&mut self, guest_id: u32) {
        if let Some(GuestObject::Paired { state, .. }) = self.guests.get_mut(&GuestId(guest_id)) {
            *state = GuestState::DestroyPending;
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
    pub(super) fn mark_pending_destroy_host(&mut self, host_id: u32) -> bool {
        let host = HostId(host_id);
        if !matches!(self.hosts.get(&host), Some(HostObject::HostOnly { .. })) {
            return false;
        }
        if self.host_ids.contains(host_id) {
            self.hosts.insert(host, HostObject::AwaitingDeleteId);
        } else {
            self.hosts.remove(&host);
        }
        true
    }

    #[cfg(test)]
    pub fn is_pending_destroy_host_only(&self, host_id: u32) -> bool {
        matches!(
            self.hosts.get(&HostId(host_id)),
            Some(HostObject::AwaitingDeleteId)
        )
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

    /// Record the interface of guest object `guest_id`, keeping its version
    /// if it has one.
    pub fn track_interface(&mut self, guest_id: u32, interface: String) {
        let version = self
            .guests
            .get(&GuestId(guest_id))
            .and_then(GuestObject::interface)
            .map_or(Self::UNKNOWN_OBJECT_VERSION, |known| known.version);
        self.track_interface_with_version(guest_id, interface, version);
    }

    /// Record the interface and version of guest object `guest_id`.
    pub fn track_interface_with_version(&mut self, guest_id: u32, interface: String, version: u32) {
        let tracked = Interface {
            name: interface,
            version,
        };
        match self.guests.get_mut(&GuestId(guest_id)) {
            Some(GuestObject::Local(interface)) => *interface = tracked,
            Some(GuestObject::Paired { interface, .. }) => *interface = Some(tracked),
            None => {
                self.guests
                    .insert(GuestId(guest_id), GuestObject::Local(tracked));
            }
        }
    }

    /// Record the host object's own interface, keeping its version if it
    /// has one. An ID with no live object becomes a host-only object.
    #[cfg(test)]
    pub fn track_host_interface(&mut self, host_id: u32, interface: String) {
        self.set_host_interface(HostId(host_id), interface, None);
    }

    /// Record the host object's own interface and version. An ID with no
    /// live object becomes a host-only object.
    pub fn track_host_interface_with_version(
        &mut self,
        host_id: u32,
        interface: String,
        version: u32,
    ) {
        self.set_host_interface(HostId(host_id), interface, Some(version));
    }

    /// Record `host`'s own interface, and `version` or, when `None`, the
    /// version it has (else the unknown one).
    fn set_host_interface(&mut self, host: HostId, interface: String, version: Option<u32>) {
        let new_version =
            |own: Option<u32>| Some(version.or(own).unwrap_or(Self::UNKNOWN_OBJECT_VERSION));
        match self.hosts.get_mut(&host) {
            Some(HostObject::Paired {
                interface: own,
                version: own_version,
                ..
            }) => {
                *own = Some(interface);
                *own_version = new_version(*own_version);
            }
            Some(HostObject::HostOnly {
                interface: own,
                version: own_version,
            }) => {
                *own = interface;
                *own_version = new_version(*own_version);
            }
            reserved => {
                debug_assert!(reserved.is_none(), "tracking reserved host ID {host:?}");
                let version = new_version(None);
                self.hosts
                    .insert(host, HostObject::HostOnly { interface, version });
            }
        }
    }

    /// Set the negotiated version of a live host object, usually one just
    /// paired with a guest object.
    pub fn set_host_version(&mut self, host_id: u32, version: u32) {
        let object = self.hosts.get_mut(&HostId(host_id));
        debug_assert!(
            object.as_deref().is_some_and(HostObject::is_live),
            "no live host object {host_id} to version"
        );
        if let Some(
            HostObject::Paired { version: own, .. } | HostObject::HostOnly { version: own, .. },
        ) = object
        {
            *own = Some(version);
        }
    }

    /// The guest object's version, or else its host object's.
    pub fn guest_object_version(&self, guest_id: u32) -> Option<u32> {
        let object = self.guests.get(&GuestId(guest_id))?;
        object
            .interface()
            .map(|interface| interface.version)
            .or_else(|| self.hosts.get(&object.host()?)?.version())
    }

    /// The host object's own version, or else its guest object's.
    pub fn host_object_version(&self, host_id: u32) -> Option<u32> {
        let object = self.hosts.get(&HostId(host_id))?;
        object.version().or_else(|| {
            let guest = self.guests.get(&object.guest()?)?;
            guest.interface().map(|interface| interface.version)
        })
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

    /// Whether `version` is the unknown version: the object was registered
    /// without a negotiated one.
    pub(crate) fn is_unknown_version(version: u32) -> bool {
        version == Self::UNKNOWN_OBJECT_VERSION
    }

    /// Forget active dispatch metadata while reserving the host ID until
    /// connection teardown. Use when a protocol has no destructor request but
    /// its host-side proxy may still exist after the guest-facing global is
    /// removed.
    ///
    /// A paired object is not a binding of the proxy's own, so it is left
    /// alone; the host's `delete_id` cannot give a binding's ID to one (see
    /// [`Self::release_deleted_host_id`]).
    pub fn retire_host_interface(&mut self, host_id: u32) {
        let host = HostId(host_id);
        if let Some(guest) = self.guest_id_of(host) {
            warn!(
                "Not retiring host object {host_id}, which guest object {} uses",
                guest.0
            );
            return;
        }
        self.hosts.insert(host, HostObject::ReservedForConnection);
    }

    pub fn get_interface(&self, guest_id: u32) -> Option<&String> {
        let object = self.guests.get(&GuestId(guest_id))?;
        object.interface().map(|interface| &interface.name)
    }

    /// The host object's own interface (see [`HostObject::Paired`]).
    pub fn get_host_interface(&self, host_id: u32) -> Option<&String> {
        self.hosts.get(&HostId(host_id))?.interface()
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

    /// End guest object `guest_id` and the host object paired with it.
    pub fn remove_id(&mut self, guest_id: u32) {
        let guest = GuestId(guest_id);
        if let Some(host) = self
            .forget_guest_object(guest)
            .and_then(|object| object.host())
        {
            self.remove_paired_host(host, guest);
        }
    }

    /// Remove only the guest-side half of a mapping.
    ///
    /// Some host protocols (notably `zwp_text_input_v1`) have no wire-level
    /// destructor. The guest-facing object can be destroyed while the
    /// host-side proxy remains alive until the connection closes. Keeping the
    /// host object as a host-only object prevents a later allocation from
    /// reusing that ID and routing stale host events to a new object.
    pub fn remove_guest_mapping(&mut self, guest_id: u32) {
        let guest = GuestId(guest_id);
        let Some(object) = self.forget_guest_object(guest) else {
            return;
        };
        let Some(host) = object.host() else {
            return;
        };
        let Some(HostObject::Paired {
            interface, version, ..
        }) = self.remove_paired_host(host, guest)
        else {
            return;
        };
        // Production callers normally register the real host interface
        // separately (a guest v3 text input is backed by a host v1
        // object). If that registration is absent, retain the guest
        // interface as a conservative reservation instead of allowing
        // the host ID to be reused while queued host traffic is in flight.
        let guest_interface = object.interface().map(|interface| interface.name.clone());
        if let Some(interface) = interface.or(guest_interface) {
            self.hosts
                .insert(host, HostObject::HostOnly { interface, version });
        }
    }

    /// Mark a guest object as destroyed while retaining both sides of its
    /// mapping for a delayed host event (for example wl_buffer.release).
    ///
    /// Requests carrying this guest ID will fail the interface validation,
    /// while events from the still-live host object can continue to resolve
    /// back to the retired guest ID until [`Self::remove_id`] is called.
    /// Preserve the interface metadata on the host side as well: proxy
    /// dispatch needs an interface name before it can invoke the event
    /// handler, and the guest metadata is intentionally removed so a
    /// destroyed object cannot accept another request.
    pub fn retire_guest_object(&mut self, guest_id: u32) {
        let guest = GuestId(guest_id);
        let Some(object) = self.guests.get_mut(&guest) else {
            return;
        };
        let GuestObject::Paired {
            host,
            interface,
            state,
        } = object
        else {
            // Nothing can name an unpaired object again: it ends here.
            self.forget_guest_object(guest);
            return;
        };
        let Some(interface) = interface.take() else {
            return;
        };
        *state = GuestState::DestroyPending;
        if let Some(HostObject::Paired {
            interface: own,
            version,
            ..
        }) = self.hosts.get_mut(host)
        {
            own.get_or_insert(interface.name);
            version.get_or_insert(Self::UNKNOWN_OBJECT_VERSION);
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
            self.host_ids.contains(host_id) && !matches!(object, HostObject::HostOnly { .. })
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
}

impl Default for ShadowTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_ids_stay_within_libwayland_servers_client_range() {
        let mut table = ShadowTable::new();
        assert_eq!(table.allocate_host_id(), 2, "0 is null and 1 is wl_display");
        // libwayland-server accepts a client ID equal to WL_MAP_MAX_OBJECTS
        // and refuses the next one.
        table.host_ids = IdAllocator::new(WL_MAP_MAX_OBJECTS..ShadowTable::CLIENT_IDS.end);
        assert_eq!(table.allocate_host_id(), 0x00f0_0000);
        let exhausted =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| table.allocate_host_id()));
        assert!(
            exhausted.is_err(),
            "the allocator must not hand out an ID libwayland-server refuses"
        );
    }

    /// Regression: allocate_host_id must not re-issue the ID of a host-only
    /// object, such as the internally bound `zcr_keyboard_extension_v1`,
    /// which has no guest counterpart. An allocator blind to these IDs could
    /// return one in use and route messages to the wrong handler.
    #[test]
    fn allocate_host_id_skips_host_interfaces_entries() {
        let mut table = ShadowTable::new();
        // Fresh IDs start at 2.
        table.track_host_interface(2, "zcr_keyboard_extension_v1".to_string());
        table.track_host_interface(3, "zwp_linux_dmabuf_v1".to_string());

        let id = table.allocate_host_id();
        assert_eq!(id, 4, "allocator must skip host-only objects, got {id}");
    }

    #[test]
    fn allocate_host_id_skips_retired_host_interfaces() {
        let mut table = ShadowTable::new();
        table.retire_host_interface(2);
        assert_eq!(
            table.allocate_host_id(),
            3,
            "IDs whose host proxy has no destructor must remain reserved"
        );
    }

    #[test]
    fn host_generated_ids_reject_retired_host_interfaces() {
        let mut table = ShadowTable::new();
        table.retire_host_interface(2);
        assert!(
            !table.is_host_id_available(2),
            "host-generated objects must not reuse retired proxy IDs"
        );
    }

    #[test]
    fn allocate_guest_server_id_uses_wayland_server_range_and_skips_reserved_ids() {
        let mut table = ShadowTable::new();
        let first = table.allocate_guest_server_id();
        assert_eq!(first, ShadowTable::GUEST_SERVER_ID_START);

        // Both normal mappings and interface-only registrations reserve guest
        // IDs. A generated host event must not overwrite either one.
        table.map_id(first, 200);
        let second = table.allocate_guest_server_id();
        table.track_interface(second, "wl_data_offer".to_string());
        let third = table.allocate_guest_server_id();

        assert_eq!(second, first + 1);
        assert_eq!(third, first + 2);
        assert!(third >= ShadowTable::GUEST_SERVER_ID_START);
    }

    #[test]
    fn guest_server_ids_are_reused_and_stay_below_the_client_map_limit() {
        let mut table = ShadowTable::new();
        let first = table.allocate_guest_server_id();
        table.map_id(first, ShadowTable::GUEST_SERVER_ID_START + 40);
        table.track_interface(first, "wl_buffer".to_string());
        table.remove_id(first);
        assert_eq!(
            table.allocate_guest_server_id(),
            first,
            "a destroyed server-created object frees its guest ID"
        );

        // libwayland-client accepts the server index WL_MAP_MAX_OBJECTS
        // itself and refuses the next one.
        table.guest_server_ids =
            IdAllocator::new(ShadowTable::SERVER_IDS.end - 1..ShadowTable::SERVER_IDS.end);
        assert_eq!(table.allocate_guest_server_id(), 0xfff0_0000);
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
        let host_object = ShadowTable::GUEST_SERVER_ID_START + 40;

        // The guest half goes while the host object stays reserved.
        let guest = table.allocate_guest_server_id();
        table.map_id(guest, host_object);
        table.track_interface(guest, "zwp_text_input_v3".to_string());
        table.remove_guest_mapping(guest);
        assert_eq!(table.allocate_guest_server_id(), guest);

        // A new guest object takes over the host object.
        table.map_id(guest, host_object);
        let replacement = table.allocate_guest_server_id();
        table.map_id(replacement, host_object);
        assert_eq!(table.get_host_id(guest), None);
        assert_eq!(table.allocate_guest_server_id(), guest);

        // Retiring keeps the ID until the mapping is removed, and frees it
        // once.
        table.track_interface(replacement, "wl_buffer".to_string());
        table.retire_guest_object(replacement);
        let next = table.allocate_guest_server_id();
        assert_ne!(next, replacement, "a retired object still owns its ID");
        table.remove_id(replacement);
        table.remove_id(replacement);
        assert_eq!(table.allocate_guest_server_id(), replacement);
        assert_eq!(table.allocate_guest_server_id(), next + 1);
    }

    #[test]
    fn guest_id_availability_rejects_reserved_and_reused_ids() {
        let mut table = ShadowTable::new();
        assert!(!table.is_guest_id_available(0));
        assert!(!table.is_guest_id_available(1));
        assert!(table.is_guest_id_available(2));
        assert!(!table.is_guest_id_available(ShadowTable::GUEST_SERVER_ID_START));

        table.map_id(2, 20);
        assert!(!table.is_guest_id_available(2));
        table.remove_id(2);
        table.track_interface(3, "wl_surface".to_string());
        assert!(!table.is_guest_id_available(3));
    }

    #[test]
    fn host_id_availability_rejects_reserved_and_reused_ids() {
        let mut table = ShadowTable::new();
        assert!(!table.is_host_id_available(0));
        assert!(!table.is_host_id_available(1));
        assert!(table.is_host_id_available(2));

        table.map_id(20, 30);
        assert!(!table.is_host_id_available(30));
        table.track_host_interface(31, "wl_shm".to_string());
        assert!(!table.is_host_id_available(31));
    }

    #[test]
    fn event_sender_is_known_for_paired_and_internal_objects_only() {
        let mut table = ShadowTable::new();
        assert!(!table.is_event_sender_known(90));

        table.map_id(10, 20);
        assert!(table.is_event_sender_known(20));
        assert!(!table.is_event_sender_known(10));

        table.track_host_interface(30, "wl_shm".to_string());
        assert!(table.is_event_sender_known(30));
        assert!(!table.is_event_sender_known(31));
    }

    #[test]
    fn local_only_guest_objects_are_explicitly_allowlisted() {
        let mut table = ShadowTable::new();
        table.track_interface(20, "wl_shm".to_string());
        table.track_interface(21, "zwp_text_input_manager_v3".to_string());
        table.track_interface(22, "wl_shm_pool".to_string());
        table.track_interface(23, "wl_surface".to_string());
        table.track_interface(25, INERT_BIND_INTERFACE.to_string());
        table.track_interface(26, "zxdg_output_v1".to_string());

        assert!(table.is_local_only_guest_object(20));
        assert!(table.is_local_only_guest_object(21));
        assert!(table.is_local_only_guest_object(22));
        assert!(!table.is_local_only_guest_object(23));
        assert!(!table.is_local_only_guest_object(24));
        assert!(table.is_local_only_guest_object(25), "an inert output");
        assert!(table.is_local_only_guest_object(26), "its xdg_output");
    }

    #[test]
    fn object_versions_are_tracked_on_both_sides_of_a_mapping() {
        let mut table = ShadowTable::new();
        table.map_id(20, 30);
        table.track_interface_with_version(20, "wl_surface".to_string(), 4);
        table.set_host_version(30, 4);

        assert_eq!(table.guest_object_version(20), Some(4));
        assert_eq!(table.host_object_version(30), Some(4));
        assert_eq!(table.guest_object_version(99), None);

        table.remove_id(20);
        assert_eq!(table.guest_object_version(20), None);
        assert_eq!(table.host_object_version(30), None);
    }

    #[test]
    fn map_id_replaces_an_existing_host_mapping_without_leaving_a_dangling_guest() {
        let mut table = ShadowTable::new();
        table.map_id(10, 20);
        table.track_interface(10, "wl_surface".to_string());
        table.map_id(11, 20);

        assert_eq!(table.get_guest_id(20), Some(11));
        assert_eq!(table.get_host_id(10), None);
        assert_eq!(table.get_interface(10), None);
        assert_eq!(table.get_host_id(11), Some(20));
    }

    #[test]
    fn remove_guest_mapping_reserves_host_id_without_host_registration() {
        let mut table = ShadowTable::new();
        table.map_id(20, 40);
        table.track_interface(20, "zwp_text_input_v3".to_string());

        table.remove_guest_mapping(20);

        assert_eq!(table.get_host_id(20), None);
        assert_eq!(
            table.get_host_interface(40),
            Some(&"zwp_text_input_v3".to_string())
        );
        assert_ne!(
            table.allocate_host_id(),
            40,
            "a host object without a guest destructor must stay reserved"
        );
    }

    #[test]
    fn retired_guest_object_keeps_host_event_interface_metadata() {
        let mut table = ShadowTable::new();
        table.map_id(20, 40);
        table.track_interface_with_version(20, "wl_buffer".to_string(), 1);
        table.set_host_version(40, 1);

        table.retire_guest_object(20);

        assert_eq!(table.get_interface(20), None);
        assert_eq!(
            table.get_host_interface(40),
            Some(&"wl_buffer".to_string()),
            "a delayed wl_buffer.release must still be dispatchable after guest destroy"
        );
        assert!(table.is_event_sender_known(40));
        assert_eq!(table.host_object_version(40), Some(1));
        assert!(table.is_pending_destroy_guest(20));
    }

    #[test]
    fn host_only_destructor_reservation_survives_until_delete_id() {
        let mut table = ShadowTable::new();
        table.track_host_interface_with_version(40, "zcr_extended_keyboard_v1".to_string(), 2);

        // The destroy request has been queued, but the host has not processed
        // it yet. The dispatch metadata is retired immediately while the
        // numeric ID remains unavailable for reuse.
        table.mark_pending_destroy_host(40);
        assert!(table.is_pending_destroy_host_only(40));
        assert!(!table.is_event_sender_known(40));
        assert!(!table.is_host_id_available(40));

        // The host acknowledgement is the only point at which the reservation
        // can be released.
        assert_eq!(table.release_deleted_host_id(40), DeletedHostId::HostOnly);
        assert!(!table.is_pending_destroy_host_only(40));
        assert!(table.is_host_id_available(40));
    }

    #[test]
    fn destroyed_host_allocated_object_frees_its_id_at_once() {
        // The host allocates IDs from the server range and sends no
        // delete_id for them; it may reuse one for its next new object.
        let host_buffer = ShadowTable::GUEST_SERVER_ID_START + 5;
        let mut table = ShadowTable::new();
        table.track_host_interface(host_buffer, "wl_buffer".to_string());

        assert!(table.mark_pending_destroy_host(host_buffer));
        assert!(!table.is_event_sender_known(host_buffer));
        assert!(
            table.is_host_id_available(host_buffer),
            "the host's next server-created object may reuse the ID"
        );
    }

    #[test]
    fn host_ids_are_reused_only_after_delete_id() {
        let mut table = ShadowTable::new();
        let first = table.allocate_host_id();
        table.map_id(10, first);
        table.track_interface(10, "wl_surface".to_string());
        table.mark_pending_destroy(10);

        let second = table.allocate_host_id();
        assert_ne!(second, first, "an ID awaiting delete_id must not be reused");
        table.map_id(11, second);

        assert_eq!(
            table.release_deleted_host_id(first),
            DeletedHostId::Paired(10)
        );
        assert_eq!(table.get_host_id(10), None);
        assert_eq!(table.get_interface(10), None);
        assert_eq!(
            table.allocate_host_id(),
            first,
            "a deleted ID is reused first"
        );
        assert_eq!(
            table.release_deleted_host_id(first + 100),
            DeletedHostId::Ignored,
            "a delete_id for an ID never allocated changes nothing"
        );
        assert_eq!(table.allocate_host_id(), second + 1);
    }

    /// The host never destroys a binding of the proxy's own, so a
    /// `delete_id` for one must not free its ID: the ID would go to a new
    /// object while the binding still holds it, and the binding's later
    /// `global_remove` would retire the new object.
    #[test]
    fn a_delete_id_for_a_live_host_only_object_changes_nothing() {
        let mut table = ShadowTable::new();
        // An internal binding without a destructor, such as the host wl_shm.
        let binding = table.allocate_host_id();
        table.track_host_interface_with_version(binding, "wl_shm".to_string(), 1);

        assert_eq!(
            table.release_deleted_host_id(binding),
            DeletedHostId::Ignored
        );
        assert!(table.is_event_sender_known(binding));

        // The binding's global goes, and with it the binding only.
        let next = table.allocate_host_id();
        assert_ne!(next, binding);
        table.map_id(11, next);
        table.retire_host_interface(binding);
        assert!(!table.is_event_sender_known(binding));
        assert_eq!(table.get_host_id(11), Some(next));

        // Retiring a paired object is refused, not fatal.
        table.retire_host_interface(next);
        assert_eq!(table.get_host_id(11), Some(next));
        assert!(table.is_event_sender_known(next));
    }

    #[test]
    fn fixture_ids_are_never_handed_out() {
        let mut table = ShadowTable::new();
        let server_guest = ShadowTable::GUEST_SERVER_ID_START + 1;
        table.insert_paired_for_test(10, 3, "wl_surface");
        table.insert_paired_for_test(server_guest, 4, "wl_data_offer");
        assert_eq!(table.get_host_id(10), Some(3));
        assert_eq!(
            table.get_interface(server_guest),
            Some(&"wl_data_offer".to_string())
        );

        // The allocators skip the fixture IDs themselves.
        assert_eq!(table.host_ids.next_candidate(), Some(2));
        assert_eq!(table.host_ids.next_candidate(), Some(5));
        assert_eq!(
            table.guest_server_ids.next_candidate(),
            Some(ShadowTable::GUEST_SERVER_ID_START)
        );
        assert_eq!(
            table.guest_server_ids.next_candidate(),
            Some(server_guest + 1)
        );
    }

    /// A guest `zwp_text_input_v3` is backed by a host `zwp_text_input_v1`,
    /// which has no destructor.
    #[test]
    fn a_paired_host_object_keeps_its_own_interface() {
        let mut table = ShadowTable::new();
        table.map_id(20, 40);
        table.track_interface_with_version(20, "zwp_text_input_v3".to_string(), 1);
        table.track_host_interface_with_version(40, "zwp_text_input_v1".to_string(), 1);

        assert_eq!(
            table.get_interface(20),
            Some(&"zwp_text_input_v3".to_string())
        );
        assert_eq!(
            table.get_host_interface(40),
            Some(&"zwp_text_input_v1".to_string())
        );
        assert!(table.host_object_matches(40, "zwp_text_input_v1"));
        assert!(
            table.host_object_matches(40, "zwp_text_input_v3"),
            "a paired host object also matches its guest's interface"
        );

        // Once the guest object goes, the host object keeps its own
        // interface, routes its events and stays reserved.
        table.remove_guest_mapping(20);
        assert_eq!(
            table.get_host_interface(40),
            Some(&"zwp_text_input_v1".to_string())
        );
        assert!(!table.host_object_matches(40, "zwp_text_input_v3"));
        assert!(table.is_event_sender_known(40));
        assert!(!table.is_host_id_available(40));
    }

    #[test]
    fn retiring_a_host_object_awaiting_delete_id_keeps_its_id_reserved() {
        let mut table = ShadowTable::new();
        let host_id = table.allocate_host_id();
        table.track_host_interface(host_id, "zaura_output".to_string());
        assert!(table.mark_pending_destroy_host(host_id));

        table.retire_host_interface(host_id);
        assert!(!table.is_pending_destroy_host_only(host_id));
        assert!(!table.is_event_sender_known(host_id));
        assert!(!table.is_host_id_available(host_id));
        assert_ne!(table.allocate_host_id(), host_id);

        // The host's delete_id still frees it.
        assert_eq!(
            table.release_deleted_host_id(host_id),
            DeletedHostId::HostOnly
        );
        assert!(table.is_host_id_available(host_id));
    }

    #[test]
    fn a_remapped_guest_leaves_its_old_host_object_only_its_own_interface() {
        let mut table = ShadowTable::new();
        table.map_id(20, 40);
        table.track_host_interface_with_version(40, "zwp_text_input_v1".to_string(), 3);
        table.map_id(21, 41);
        table.set_host_version(41, 3);

        table.map_id(20, 42);
        table.map_id(21, 43);

        assert_eq!(table.get_host_id(20), Some(42));
        assert_eq!(table.get_guest_id(40), None);
        assert_eq!(
            table.get_host_interface(40),
            Some(&"zwp_text_input_v1".to_string()),
            "an object with its own interface stays a host-only object"
        );
        assert_eq!(table.host_object_version(40), None);
        assert!(table.is_event_sender_known(40));
        assert_eq!(table.get_guest_id(41), None);
        assert_eq!(table.host_object_version(41), None);
        assert!(
            table.is_host_id_available(41),
            "an object without its own interface is forgotten"
        );
    }
}
