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

use std::collections::{HashMap, HashSet};

use super::id_allocator::{IdAllocator, WL_MAP_MAX_OBJECTS};
use super::{GuestId, HostId, INERT_BIND_INTERFACE};
use log::warn;

/// What a host `wl_display.delete_id` ended
/// ([`ShadowTable::release_deleted_host_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedHostId {
    /// An ID the proxy does not track; nothing changed.
    Untracked,
    /// A host-only object; no guest object is owed a `delete_id`.
    HostOnly,
    /// An object paired with this guest object, now owed its `delete_id`.
    Paired(u32),
}

#[allow(dead_code)]
pub struct ShadowTable {
    guest_to_host: HashMap<u32, u32>,
    host_to_guest: HashMap<u32, u32>,
    interfaces: HashMap<u32, String>,
    host_interfaces: HashMap<u32, String>,
    guest_versions: HashMap<u32, u32>,
    host_versions: HashMap<u32, u32>,
    retired_host_ids: HashSet<u32>,
    /// Host-only objects whose destructor was queued but whose host
    /// `wl_display.delete_id` acknowledgement has not arrived yet.
    pending_destroy_host_ids: HashSet<u32>,
    /// Guest objects whose destructor request has reached the host but whose
    /// host-side `wl_display.delete_id` has not arrived yet.
    pending_destroy_guest_ids: HashSet<u32>,
    /// The IDs this proxy allocates for its objects on the host connection.
    /// An ID returns here only through [`Self::release_deleted_host_id`].
    host_ids: IdAllocator,
    /// Wayland reserves the upper 8-bit ID range for IDs allocated by the
    /// server and sent to the client in an event. Host and guest object
    /// namespaces are independent, so host-generated `new_id` values must be
    /// translated to a fresh ID in this range before they reach the guest.
    /// An ID returns here when [`Self::remove_id`] forgets its object.
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
            guest_to_host: HashMap::new(),
            host_to_guest: HashMap::new(),
            interfaces: HashMap::new(),
            host_interfaces: HashMap::new(),
            guest_versions: HashMap::new(),
            host_versions: HashMap::new(),
            retired_host_ids: HashSet::new(),
            pending_destroy_host_ids: HashSet::new(),
            pending_destroy_guest_ids: HashSet::new(),
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
            if !self.is_host_id_registered(id) {
                return id;
            }
            // Only an object registered under an ID this allocator did not
            // hand out gets here: a host `new_id` from the client range, or
            // a test fixture. Skipping keeps two objects off one ID; the
            // warning keeps the cause visible.
            warn!("Skipping host ID {id}, which is registered but was never allocated");
        }
    }

    /// Apply the host's `wl_display.delete_id` for `host_id`.
    ///
    /// The host has destroyed the object and forgotten its ID, so no later
    /// event can name it. Forget every mapping of the ID and return it to
    /// the allocator. As in libwayland-client, this is the only way an ID
    /// this proxy allocated becomes reusable: an ID is never reused while
    /// its `delete_id` is outstanding, and objects without a destructor keep
    /// theirs for the connection.
    ///
    /// A `delete_id` for an ID this proxy did not allocate, or no longer
    /// tracks, changes nothing and returns [`DeletedHostId::Untracked`].
    pub fn release_deleted_host_id(&mut self, host_id: u32) -> DeletedHostId {
        if !self.host_ids.contains(host_id) || !self.is_host_id_registered(host_id) {
            warn!("Ignoring wl_display.delete_id for untracked host ID {host_id}");
            return DeletedHostId::Untracked;
        }
        let guest_id = self.host_to_guest.get(&host_id).copied();
        if let Some(guest_id) = guest_id {
            self.remove_id(guest_id);
        }
        self.host_to_guest.remove(&host_id);
        self.host_interfaces.remove(&host_id);
        self.host_versions.remove(&host_id);
        self.retired_host_ids.remove(&host_id);
        self.pending_destroy_host_ids.remove(&host_id);
        self.host_ids.release(host_id);
        guest_id.map_or(DeletedHostId::HostOnly, DeletedHostId::Paired)
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
            if !self.guest_to_host.contains_key(&id) && !self.interfaces.contains_key(&id) {
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

    pub fn map_id(&mut self, guest_id: u32, host_id: u32) {
        if let Some(old_host_id) = self.guest_to_host.insert(guest_id, host_id) {
            if old_host_id != host_id && self.host_to_guest.get(&old_host_id) == Some(&guest_id) {
                self.host_to_guest.remove(&old_host_id);
                self.host_versions.remove(&old_host_id);
            }
        }
        if let Some(old_guest_id) = self.host_to_guest.insert(host_id, guest_id) {
            if old_guest_id != guest_id && self.guest_to_host.get(&old_guest_id) == Some(&host_id) {
                self.guest_to_host.remove(&old_guest_id);
                self.forget_guest_object(old_guest_id, true);
            }
        }
    }

    pub fn get_host_id(&self, guest_id: u32) -> Option<u32> {
        self.guest_to_host.get(&guest_id).cloned()
    }

    /// Check that a guest object has the interface required by a request
    /// argument. Wayland object IDs are not interchangeable just because they
    /// happen to be mapped; forwarding a `wl_surface` where a `wl_seat` is
    /// expected would otherwise send a type-invalid request to the host.
    pub fn guest_object_matches(&self, guest_id: u32, interface: &str) -> bool {
        self.interfaces
            .get(&guest_id)
            .is_some_and(|actual| actual == interface)
    }

    /// Synthetic guest objects are intentionally not paired with a host
    /// object. Their handlers consume requests locally (such as the SHM shim,
    /// the v3 text-input manager, and an inert `wl_output` whose bind crossed
    /// its global's removal, with its `zxdg_output_v1`), so the generated dispatcher must let those
    /// requests reach the handler while rejecting every other unmapped
    /// sender before it can mutate state.
    pub fn is_local_only_guest_object(&self, guest_id: u32) -> bool {
        matches!(
            self.interfaces.get(&guest_id).map(String::as_str),
            Some(
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
        )
    }

    /// Check the interface of a host object carried by an event. Host-created
    /// objects are recorded in `host_interfaces`, while objects paired with a
    /// guest object use the guest-side interface metadata.
    pub fn host_object_matches(&self, host_id: u32, interface: &str) -> bool {
        self.host_interfaces
            .get(&host_id)
            .is_some_and(|actual| actual == interface)
            || self
                .host_to_guest
                .get(&host_id)
                .and_then(|guest_id| self.interfaces.get(guest_id))
                .is_some_and(|actual| actual == interface)
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
            && !self.guest_to_host.contains_key(&guest_id)
            && !self.interfaces.contains_key(&guest_id)
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
    /// proxy objects. A reused ID would otherwise overwrite the reverse map
    /// and route subsequent host events to the wrong guest object.
    pub fn is_host_id_available(&self, host_id: u32) -> bool {
        host_id > 1 && !self.is_host_id_registered(host_id)
    }

    /// Whether any table entry names `host_id`: a paired or host-only
    /// object, or a reservation awaiting `delete_id` or teardown.
    fn is_host_id_registered(&self, host_id: u32) -> bool {
        self.host_to_guest.contains_key(&host_id)
            || self.host_interfaces.contains_key(&host_id)
            || self.retired_host_ids.contains(&host_id)
            || self.pending_destroy_host_ids.contains(&host_id)
    }

    pub fn get_guest_id(&self, host_id: u32) -> Option<u32> {
        self.host_to_guest.get(&host_id).cloned()
    }

    /// Return whether a host event sender belongs to an object that this
    /// connection is currently tracking.
    ///
    /// Paired guest objects live in `host_to_guest`; objects that Sommelier
    /// binds internally (for example the host `wl_shm` and keyboard-extension
    /// objects) live in `host_interfaces`. An event from neither namespace is
    /// stale or malformed and must not reach a handler, because handlers may
    /// update proxy state before the generated forwarding code can discover
    /// that the sender has no guest representation.
    pub fn is_event_sender_known(&self, host_id: u32) -> bool {
        self.host_to_guest.contains_key(&host_id) || self.host_interfaces.contains_key(&host_id)
    }

    /// Mark a forwarded destructor as pending host deletion. Retain the
    /// interface metadata so a subsequent request reaches the generated
    /// dispatcher and is rejected explicitly, while retaining both numeric
    /// maps until the host emits `wl_display.delete_id`.
    pub fn mark_pending_destroy(&mut self, guest_id: u32) {
        if self.guest_to_host.contains_key(&guest_id) {
            self.pending_destroy_guest_ids.insert(guest_id);
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
    pub(super) fn mark_pending_destroy_host(&mut self, host_id: u32) -> bool {
        if self.host_to_guest.contains_key(&host_id) || !self.host_interfaces.contains_key(&host_id)
        {
            return false;
        }
        self.host_interfaces.remove(&host_id);
        self.host_versions.remove(&host_id);
        self.retired_host_ids.remove(&host_id);
        !self.host_ids.contains(host_id) || self.pending_destroy_host_ids.insert(host_id)
    }

    #[allow(dead_code)]
    pub fn is_pending_destroy_host_only(&self, host_id: u32) -> bool {
        self.pending_destroy_host_ids.contains(&host_id)
    }

    pub fn is_pending_destroy_guest(&self, guest_id: u32) -> bool {
        self.pending_destroy_guest_ids.contains(&guest_id)
    }

    pub fn is_pending_destroy_host(&self, host_id: u32) -> bool {
        self.host_to_guest
            .get(&host_id)
            .is_some_and(|guest_id| self.is_pending_destroy_guest(*guest_id))
    }

    pub fn track_interface(&mut self, guest_id: u32, interface: String) {
        self.interfaces.insert(guest_id, interface);
        self.guest_versions
            .entry(guest_id)
            .or_insert(Self::UNKNOWN_OBJECT_VERSION);
    }

    pub fn track_interface_with_version(&mut self, guest_id: u32, interface: String, version: u32) {
        self.interfaces.insert(guest_id, interface);
        self.guest_versions.insert(guest_id, version);
    }

    #[allow(dead_code)]
    pub fn track_host_interface(&mut self, host_id: u32, interface: String) {
        self.host_interfaces.insert(host_id, interface);
        self.host_versions
            .entry(host_id)
            .or_insert(Self::UNKNOWN_OBJECT_VERSION);
    }

    pub fn track_host_interface_with_version(
        &mut self,
        host_id: u32,
        interface: String,
        version: u32,
    ) {
        self.host_interfaces.insert(host_id, interface);
        self.host_versions.insert(host_id, version);
    }

    /// Set the negotiated version for a host object that is already paired
    /// with a guest object. Keeping this separate from `host_interfaces`
    /// avoids treating every paired object as an internal host-only object.
    pub fn set_host_version(&mut self, host_id: u32, version: u32) {
        self.host_versions.insert(host_id, version);
    }

    pub fn guest_object_version(&self, guest_id: u32) -> Option<u32> {
        self.guest_versions.get(&guest_id).copied().or_else(|| {
            self.guest_to_host
                .get(&guest_id)
                .and_then(|host_id| self.host_versions.get(host_id).copied())
        })
    }

    pub fn host_object_version(&self, host_id: u32) -> Option<u32> {
        self.host_versions.get(&host_id).copied().or_else(|| {
            self.host_to_guest
                .get(&host_id)
                .and_then(|guest_id| self.guest_versions.get(guest_id).copied())
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
    pub fn retire_host_interface(&mut self, host_id: u32) {
        self.host_interfaces.remove(&host_id);
        self.host_versions.remove(&host_id);
        self.pending_destroy_host_ids.remove(&host_id);
        self.retired_host_ids.insert(host_id);
    }

    pub fn get_interface(&self, guest_id: u32) -> Option<&String> {
        self.interfaces.get(&guest_id)
    }

    pub fn get_host_interface(&self, host_id: u32) -> Option<&String> {
        self.host_interfaces.get(&host_id)
    }

    /// Typed lookup: translate a guest-allocated object ID to its host counterpart.
    /// Use in **client→host request** handlers where `ctx.last_sender_id` is a guest ID.
    pub fn host_id_of(&self, guest: GuestId) -> Option<HostId> {
        self.guest_to_host.get(&guest.0).map(|&h| HostId(h))
    }

    /// Typed lookup: translate a host-allocated object ID to its guest counterpart.
    /// Use in **host→client event** handlers where `ctx.last_sender_id` is a host ID.
    pub fn guest_id_of(&self, host: HostId) -> Option<GuestId> {
        self.host_to_guest.get(&host.0).map(|&g| GuestId(g))
    }

    pub fn remove_id(&mut self, guest_id: u32) {
        let host_id = self.guest_to_host.remove(&guest_id);
        if let Some(host_id) = host_id {
            self.host_to_guest.remove(&host_id);
            self.host_interfaces.remove(&host_id);
            self.host_versions.remove(&host_id);
        }
        self.forget_guest_object(guest_id, host_id.is_some());
    }

    /// Forget the guest side of `guest_id`, whose mapping to a host object
    /// the caller has just removed (`was_mapped`) or never had.
    ///
    /// The one place a guest object ends: its metadata goes, and an ID from
    /// the server range returns to the allocator. A server-created object
    /// gets no `delete_id`; libwayland-client frees its ID as soon as either
    /// side destroys it, which is when the proxy drops its mapping. Nothing
    /// is freed for an ID that named no object, so no ID is freed twice.
    ///
    /// # Returns
    ///
    /// The object's interface, if it still had one.
    fn forget_guest_object(&mut self, guest_id: u32, was_mapped: bool) -> Option<String> {
        debug_assert!(!self.guest_to_host.contains_key(&guest_id));
        self.pending_destroy_guest_ids.remove(&guest_id);
        self.guest_versions.remove(&guest_id);
        let interface = self.interfaces.remove(&guest_id);
        if was_mapped || interface.is_some() {
            self.guest_server_ids.release(guest_id);
        }
        interface
    }

    /// Remove only the guest-side half of a mapping.
    ///
    /// Some host protocols (notably `zwp_text_input_v1`) have no wire-level
    /// destructor. The guest-facing object can be destroyed while the
    /// host-side proxy remains alive until the connection closes. Keeping the
    /// host interface reservation prevents a later allocation from reusing
    /// that ID and routing stale host events to a new object.
    pub fn remove_guest_mapping(&mut self, guest_id: u32) {
        let host_id = self.guest_to_host.remove(&guest_id);
        let guest_interface = self.forget_guest_object(guest_id, host_id.is_some());
        if let Some(host_id) = host_id {
            if self.host_to_guest.get(&host_id) == Some(&guest_id) {
                self.host_to_guest.remove(&host_id);
            }
            // Production callers normally register the real host interface
            // separately (a guest v3 text input is backed by a host v1
            // object). If that registration is absent, retain the guest
            // interface as a conservative reservation instead of allowing
            // the host ID to be reused while queued host traffic is in flight.
            if let (std::collections::hash_map::Entry::Vacant(entry), Some(interface)) =
                (self.host_interfaces.entry(host_id), guest_interface)
            {
                entry.insert(interface);
            }
        }
    }

    /// Mark a guest object as destroyed while retaining both sides of its
    /// mapping for a delayed host event (for example wl_buffer.release).
    ///
    /// Requests carrying this guest ID will fail the interface validation,
    /// while events from the still-live host object can continue to resolve
    /// back to the retired guest ID until [`remove_id`] is called. Preserve the
    /// interface metadata on the host side as well: proxy dispatch needs an
    /// interface name before it can invoke the event handler, and the guest
    /// metadata is intentionally removed so a destroyed object cannot accept
    /// another request.
    pub fn retire_guest_object(&mut self, guest_id: u32) {
        let Some(&host_id) = self.guest_to_host.get(&guest_id) else {
            // Nothing can name an unmapped object again: it ends here.
            self.forget_guest_object(guest_id, false);
            return;
        };
        let interface = self.interfaces.remove(&guest_id);
        self.guest_versions.remove(&guest_id);
        if let Some(interface) = interface {
            self.pending_destroy_guest_ids.insert(guest_id);
            self.host_interfaces.entry(host_id).or_insert(interface);
            self.host_versions
                .entry(host_id)
                .or_insert(Self::UNKNOWN_OBJECT_VERSION);
        }
    }

    #[allow(dead_code)]
    pub fn find_by_interface(&self, interface_name: &str) -> Vec<u32> {
        self.interfaces
            .iter()
            .filter_map(|(id, name)| {
                if name == interface_name {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect()
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

    /// Regression: allocate_host_id must not re-issue IDs already registered in
    /// `host_interfaces` (used for internally-bound objects like
    /// `zcr_keyboard_extension_v1` that have no guest-side counterpart and are
    /// therefore absent from `host_to_guest`).
    ///
    /// Without the `host_interfaces` check the allocator was blind to these IDs
    /// and could return an ID that is already in use, causing the dispatcher to
    /// route messages to the wrong handler.
    #[test]
    fn allocate_host_id_skips_host_interfaces_entries() {
        let mut table = ShadowTable::new();
        // Fresh IDs start at 2.
        // Register IDs 2 and 3 as host-tracked interfaces (no guest pair).
        table.track_host_interface(2, "zcr_keyboard_extension_v1".to_string());
        table.track_host_interface(3, "zwp_linux_dmabuf_v1".to_string());

        // The allocator must skip 2 and 3 (in host_interfaces) and return 4.
        let id = table.allocate_host_id();
        assert_eq!(
            id, 4,
            "allocator must skip IDs registered in host_interfaces, got {}",
            id
        );
        assert!(
            !table.host_interfaces.contains_key(&id) || id == 4,
            "returned ID must not be in host_interfaces"
        );
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
            DeletedHostId::Untracked,
            "a delete_id for an ID never allocated changes nothing"
        );
        assert_eq!(table.allocate_host_id(), second + 1);
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
