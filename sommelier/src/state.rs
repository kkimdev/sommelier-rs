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

use std::collections::{HashMap, HashSet};
use std::os::unix::io::{OwnedFd, RawFd};
use std::sync::{Arc, RwLock};

use crate::allocator::Allocator;
use crate::virtwl_channel::VirtWaylandChannel;
use log::warn;

/// A Wayland object ID allocated by the **guest** (client) side.
///
/// Request handlers (client→host) receive guest IDs. Use [`ShadowTable::host_id_of`]
/// to translate to the corresponding host ID. Passing a `GuestId` where a `HostId`
/// is expected (or vice versa) is a **compile error**.
///
/// The inner field is intentionally `pub(crate)` so that arbitrary `GuestId(host_id)`
/// constructions cannot be made from outside this crate, preserving the type invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuestId(pub(crate) u32);

impl GuestId {
    /// Wrap the raw sender ID from a **client→host request** handler.
    /// Only call this in handlers where `ctx.last_sender_id` is a guest ID.
    #[inline]
    pub(crate) fn from_request_sender(ctx: &Context) -> Self {
        Self(ctx.last_sender_id)
    }
}

/// A Wayland object ID allocated by the **host** compositor side.
///
/// Event handlers (host→client) receive host IDs. Use [`ShadowTable::guest_id_of`]
/// to translate to the corresponding guest ID. Passing a `HostId` where a `GuestId`
/// is expected (or vice versa) is a **compile error**.
///
/// The inner field is intentionally `pub(crate)` so that arbitrary `HostId(guest_id)`
/// constructions cannot be made from outside this crate, preserving the type invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostId(pub(crate) u32);

/// Compare Wayland serials across wrapping `u32` space.
pub(crate) fn serial_is_after(candidate: u32, previous: u32) -> bool {
    let distance = candidate.wrapping_sub(previous);
    distance != 0 && distance < (1 << 31)
}

impl HostId {
    /// Wrap the raw sender ID from a **host→client event** handler.
    /// Only call this in handlers where `ctx.last_sender_id` is a host ID.
    #[inline]
    pub(crate) fn from_event_sender(ctx: &Context) -> Self {
        Self(ctx.last_sender_id)
    }

    /// Wrap an ID freshly returned by [`ShadowTable::allocate_host_id`].
    ///
    /// Using this constructor (rather than the bare `HostId(raw)` tuple syntax)
    /// makes allocation sites auditable: a code search for `from_allocated` finds
    /// every place a new host-side object is created.
    #[inline]
    pub(crate) fn from_allocated(id: u32) -> Self {
        Self(id)
    }
}

/// Metadata for one global advertised by the host compositor.
///
/// Global names are unique, but interface names are not: a compositor may
/// advertise multiple `wl_seat` or `wl_output` globals at the same time.
/// Keeping the numeric name as the map key prevents a later global from
/// overwriting the bind target for an earlier one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGlobal {
    /// Interface implemented by the global.
    pub interface: String,
    /// Maximum version exposed to the guest for this global.
    pub version: u32,
}

/// A synthetic linux-dmabuf v4 global waiting for legacy host capability
/// discovery to complete.
///
/// The host registry object is retained because each guest registry needs its
/// own `global` event. The generation prevents a delayed capability callback
/// from publishing a replacement or already-removed global.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDmabufGlobal {
    pub registry_host_id: u32,
    pub name: u32,
    pub version: u32,
    pub generation: u64,
}

// Each internally-bound interface stores its host ID in a dedicated
// `ctx.host_*_id` field, and we register it with `track_host_interface`
// so proxy.rs can dispatch inbound host events without any magic number hackery.

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
    next_host_id: u32,
    /// Wayland reserves the upper 8-bit ID range for IDs allocated by the
    /// server and sent to the client in an event. Host and guest object
    /// namespaces are independent, so host-generated `new_id` values must be
    /// translated to a fresh ID in this range before they reach the guest.
    next_guest_server_id: u32,
}

impl ShadowTable {
    const GUEST_SERVER_ID_START: u32 = 0xff00_0000;
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
            // Start at 2 to mimic standard Wayland client behavior.
            // ID 1 is reserved for wl_display.
            next_host_id: 2,
            next_guest_server_id: Self::GUEST_SERVER_ID_START,
        }
    }

    pub fn allocate_host_id(&mut self) -> u32 {
        // In the common case (next_host_id points to a free slot) this returns
        // on the first iteration. The u64 loop bound caps worst-case work at
        // u32::MAX iterations to prevent an infinite spin when the ID space is
        // nearly full, and triggers a panic instead of looping forever.
        //
        // We use u64 for the loop variable to avoid an overflow when constructing
        // the `RangeInclusive<u32>`: writing `0u32..=u32::MAX` would require
        // computing `u32::MAX + 1` for the exclusive upper bound, which wraps
        // to 0 and produces an empty range on platforms where range iteration
        // checks `start > end`. Casting to u64 sidesteps this entirely.
        for _ in 0u64..=(u32::MAX as u64) {
            // `id` is the candidate we are testing this iteration.
            let id = self.next_host_id;
            // Advance the counter; .max(2) handles the u32::MAX → 0 → 2 wrap
            // in one step, keeping 0 (null) and 1 (wl_display) permanently skipped.
            self.next_host_id = self.next_host_id.wrapping_add(1).max(2);
            // Accept only IDs ≥ 2 that are not already assigned in either map.
            // `host_to_guest` tracks guest↔host paired objects; `host_interfaces`
            // tracks internally-bound objects (keyboard extension, dmabuf, etc.)
            // that are registered via `track_host_interface` without a guest pair.
            // Both maps must be checked, otherwise a freshly-allocated ID could
            // collide with an already-registered internal object.
            //
            // (The pre-advance id could be 0 or 1 if next_host_id was initialised
            // to those values externally, e.g. in tests.)
            if id >= 2
                && !self.host_to_guest.contains_key(&id)
                && !self.host_interfaces.contains_key(&id)
                && !self.retired_host_ids.contains(&id)
                && !self.pending_destroy_host_ids.contains(&id)
            {
                return id;
            }
        }
        log::error!("sommelier: host Wayland object ID space exhausted");
        panic!("sommelier: host Wayland object ID space exhausted — this should never happen");
    }

    /// Allocate an ID in Wayland's server-generated ID range.
    ///
    /// A host compositor is allowed to choose a different raw ID than the
    /// guest-side proxy can expose. Returning the host ID verbatim can collide
    /// with a guest-created object and makes later requests use the wrong
    /// namespace. Keep a separate wrapping cursor for the server range and
    /// reserve the chosen ID in the normal guest maps at the call site.
    pub fn allocate_guest_server_id(&mut self) -> u32 {
        // The range is exactly 0x0100_0000 IDs
        // (0xff00_0000..=u32::MAX). Use a u64 loop counter so the inclusive
        // bound itself cannot overflow.
        for _ in 0u64..=0x00ff_ffff {
            let id = self.next_guest_server_id;
            self.next_guest_server_id = if id == u32::MAX {
                Self::GUEST_SERVER_ID_START
            } else {
                id + 1
            };

            if id >= Self::GUEST_SERVER_ID_START
                && !self.guest_to_host.contains_key(&id)
                && !self.interfaces.contains_key(&id)
            {
                return id;
            }
        }
        log::error!("sommelier: guest Wayland server ID space exhausted");
        panic!("sommelier: guest Wayland server ID space exhausted — this should never happen");
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
                self.interfaces.remove(&old_guest_id);
                self.guest_versions.remove(&old_guest_id);
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
    /// object. Their handlers consume requests locally (currently the SHM
    /// shim and the v3 text-input manager), so the generated dispatcher must
    /// let those requests reach the handler while rejecting every other
    /// unmapped sender before it can mutate state.
    pub fn is_local_only_guest_object(&self, guest_id: u32) -> bool {
        matches!(
            self.interfaces.get(&guest_id).map(String::as_str),
            Some(
                "wl_shm"
                    | "wl_shm_pool"
                    | "zwp_text_input_manager_v3"
                    | "zwp_linux_dmabuf_feedback_v1"
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
        host_id > 1
            && !self.host_to_guest.contains_key(&host_id)
            && !self.host_interfaces.contains_key(&host_id)
            && !self.retired_host_ids.contains(&host_id)
            && !self.pending_destroy_host_ids.contains(&host_id)
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
    /// reach a handler, but the numeric ID remains reserved until the host
    /// acknowledges the destructor with `wl_display.delete_id`.
    pub fn mark_pending_destroy_host(&mut self, host_id: u32) -> bool {
        if self.host_to_guest.contains_key(&host_id) || !self.host_interfaces.contains_key(&host_id)
        {
            return false;
        }
        self.host_interfaces.remove(&host_id);
        self.host_versions.remove(&host_id);
        self.retired_host_ids.remove(&host_id);
        self.pending_destroy_host_ids.insert(host_id)
    }

    #[allow(dead_code)]
    pub fn is_pending_destroy_host_only(&self, host_id: u32) -> bool {
        self.pending_destroy_host_ids.contains(&host_id)
    }

    /// Consume a `wl_display.delete_id` for a host-only object.
    ///
    /// Host-only resources have no guest object ID to expose, so the display
    /// handler uses this result to consume the acknowledgement without
    /// forwarding an invalid `delete_id(0)` event to the guest.
    pub fn consume_host_delete_id(&mut self, host_id: u32) -> bool {
        self.pending_destroy_host_ids.remove(&host_id)
    }

    pub fn is_pending_destroy_guest(&self, guest_id: u32) -> bool {
        self.pending_destroy_guest_ids.contains(&guest_id)
    }

    /// Clear a guest destructor marker after the guest-side mapping has been
    /// removed while a host-only reservation remains for a delayed event.
    ///
    /// This is narrower than [`remove_id`]: orphaned asynchronous
    /// linux-dmabuf params can acknowledge their guest `delete_id` before
    /// emitting `created`/`failed`, so their host interface metadata must stay
    /// registered until that final event is consumed.
    pub fn clear_pending_destroy_guest(&mut self, guest_id: u32) {
        self.pending_destroy_guest_ids.remove(&guest_id);
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

    /// Remove a host-side interface registration.
    ///
    /// Call this when a host object is destroyed (e.g. `zcr_extended_keyboard_v1.destroy`)
    /// to prevent stale events for the recycled ID from being dispatched.
    ///
    /// # Pipelining note
    /// The `destroy` request and this registration removal are applied immediately
    /// on the client side, but the host compositor processes them asynchronously.
    /// Events for `host_id` that were already queued by the host (e.g. `peek_key`
    /// in protocol v2+) may arrive after the destroy is sent. Those events will
    /// be silently dropped by the dispatcher once the registration is gone, which
    /// is the correct behavior. Exo does not send events after processing `destroy`.
    pub fn remove_host_interface(&mut self, host_id: u32) {
        self.host_interfaces.remove(&host_id);
        self.host_versions.remove(&host_id);
        self.retired_host_ids.remove(&host_id);
        self.pending_destroy_host_ids.remove(&host_id);
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
        self.pending_destroy_guest_ids.remove(&guest_id);
        if let Some(host_id) = self.guest_to_host.remove(&guest_id) {
            self.host_to_guest.remove(&host_id);
            self.host_interfaces.remove(&host_id);
            self.host_versions.remove(&host_id);
        }
        self.interfaces.remove(&guest_id);
        self.guest_versions.remove(&guest_id);
    }

    /// Remove only the guest-side half of a mapping.
    ///
    /// Some host protocols (notably `zwp_text_input_v1`) have no wire-level
    /// destructor. The guest-facing object can be destroyed while the
    /// host-side proxy remains alive until the connection closes. Keeping the
    /// host interface reservation prevents a later allocation from reusing
    /// that ID and routing stale host events to a new object.
    pub fn remove_guest_mapping(&mut self, guest_id: u32) {
        let guest_interface = self.interfaces.remove(&guest_id);
        self.guest_versions.remove(&guest_id);
        if let Some(host_id) = self.guest_to_host.remove(&guest_id) {
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

    /// Complete a server-destroyed paired object while reserving its host ID.
    ///
    /// Some event-only objects, notably `wl_callback`, cease to exist when the
    /// host sends their terminal event. The guest mapping must disappear
    /// immediately, but the host numeric ID remains unavailable until the
    /// compositor's later `wl_display.delete_id`. Combining both transitions
    /// prevents callers from accidentally opening an ID-reuse window between
    /// removing the pair and installing the host-only reservation.
    pub fn retire_server_destroyed_object(&mut self, guest_id: u32) -> Option<HostId> {
        let host_id = *self.guest_to_host.get(&guest_id)?;
        self.remove_guest_mapping(guest_id);
        self.mark_pending_destroy_host(host_id)
            .then_some(HostId(host_id))
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
        let interface = self.interfaces.remove(&guest_id);
        self.guest_versions.remove(&guest_id);
        if let (Some(host_id), Some(interface)) = (self.guest_to_host.get(&guest_id), interface) {
            self.pending_destroy_guest_ids.insert(guest_id);
            self.host_interfaces.entry(*host_id).or_insert(interface);
            self.host_versions
                .entry(*host_id)
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

impl Drop for Context {
    fn drop(&mut self) {
        // Raw FDs in these structures are owned by the proxy until the
        // corresponding queued message is flushed. A protocol error or a
        // disconnect can drop Context before proxy::handle_msgs drains them,
        // so close every remaining descriptor here. De-duplicate defensively:
        // a failed/partially handled request must never turn an accidental
        // duplicate entry into a close of a subsequently reused descriptor.
        let mut owned_fds = HashSet::new();
        for (_, fds) in self.client_to_host_queue.drain(..) {
            owned_fds.extend(fds);
        }
        for (_, fds) in self.host_to_client_queue.drain(..) {
            owned_fds.extend(fds);
        }
        for params in self.pending_params.drain().map(|(_, params)| params) {
            owned_fds.extend(params.into_iter().map(|param| param.fd));
        }

        for fd in owned_fds {
            if fd >= 0 {
                let _ = nix::unistd::close(fd);
            }
        }

        // Context is also dropped on synchronous protocol-error paths. Tokio
        // JoinHandle::drop detaches a task, so explicitly abort every active
        // clipboard pump instead of allowing it to outlive the connection.
        for pump in self.clipboard_pumps.drain(..) {
            pump.abort();
        }
    }
}

pub struct PoolInner {
    pub client_ptr: *mut libc::c_void,
    pub size: usize,
}

unsafe impl Send for PoolInner {}
unsafe impl Sync for PoolInner {}

pub struct PoolState {
    pub client_fd: RawFd,
    pub inner: RwLock<PoolInner>,
}

impl Drop for PoolState {
    fn drop(&mut self) {
        // `Drop` must release the mapping even when a worker panicked while
        // holding the lock. `RwLock::write()` returns an error for a poisoned
        // lock; treating that error as "nothing to clean up" leaks the entire
        // SHM pool until process exit. `get_mut()` is safe here because `&mut
        // self` proves that no other thread can access the lock during drop,
        // and `PoisonError::get_mut()` still exposes the protected value.
        let inner = match self.inner.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        unsafe {
            if !inner.client_ptr.is_null() && inner.client_ptr != libc::MAP_FAILED {
                libc::munmap(inner.client_ptr, inner.size);
                inner.client_ptr = std::ptr::null_mut();
            }
        }
        unsafe {
            if self.client_fd >= 0 {
                libc::close(self.client_fd);
            }
        }
    }
}

pub struct BufferState {
    pub pool: Arc<PoolState>,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: u32,
    pub format: u32,
    #[allow(dead_code)]
    pub bo: Option<gbm::BufferObject<()>>,
    #[allow(dead_code)]
    pub dmabuf_fd: Option<OwnedFd>,
    pub bo_stride: u32,
    /// Destination offset of the second plane in the mapped output buffer.
    /// Zero means the format is single-plane; for NV12 this is the host
    /// allocator's returned plane-1 offset relative to plane 0.
    pub dmabuf_plane1_offset: usize,
    /// Destination stride of the second plane.  Kept separate from
    /// `bo_stride` because host dma-buf allocators may pad planes
    /// independently.
    pub dmabuf_plane1_stride: usize,
    /// Whether the output descriptor requires VirtWL dma-buf begin/end
    /// synchronization around CPU writes.
    pub dmabuf_sync: bool,
    pub dest_ptr: *mut u8,
    pub dest_size: usize,
    /// Newly allocated host storage is uninitialized. The first committed
    /// frame must therefore copy the complete guest buffer even if the client
    /// omitted an explicit damage request.
    pub needs_full_copy: bool,
}

unsafe impl Send for BufferState {}
unsafe impl Sync for BufferState {}

impl Drop for BufferState {
    fn drop(&mut self) {
        if !self.dest_ptr.is_null() && self.dest_ptr as *mut libc::c_void != libc::MAP_FAILED {
            unsafe {
                libc::munmap(self.dest_ptr as *mut libc::c_void, self.dest_size);
                self.dest_ptr = std::ptr::null_mut();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The crop/scale state associated with a `wp_viewport`.
///
/// `wl_fixed_t` values are kept in their raw signed 24.8 representation so
/// damage conversion does not lose fractional source coordinates.  Viewport
/// state is double-buffered by the wl_surface commit, not by the viewport
/// object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportState {
    pub source: Option<(i32, i32, i32, i32)>,
    pub destination: Option<(i32, i32)>,
}

impl ViewportState {
    pub const fn new() -> Self {
        Self {
            source: None,
            destination: None,
        }
    }

    pub const fn is_identity(self) -> bool {
        self.source.is_none() && self.destination.is_none()
    }
}

impl Default for ViewportState {
    fn default() -> Self {
        Self::new()
    }
}

impl DamageRect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

const MAX_PENDING_DAMAGE_RECTS: usize = 256;

/// Bounded damage accumulated for one surface commit.
///
/// A client may send arbitrarily many damage requests before committing.
/// Keeping every rectangle makes later coalescing quadratic and permits
/// unbounded memory growth. Once the exact set reaches its cap, transition to
/// an explicit full-damage state. Full damage is deliberately not encoded as
/// a magic rectangle: every consumer must handle the same authoritative
/// variant, so host damage and local copies cannot disagree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DamageRegion {
    #[default]
    Empty,
    Rects(Vec<DamageRect>),
    Full,
}

impl DamageRegion {
    pub fn push(&mut self, rect: DamageRect) {
        if matches!(self, Self::Full) || rect.width <= 0 || rect.height <= 0 {
            return;
        }
        match self {
            Self::Empty => *self = Self::Rects(vec![rect]),
            Self::Rects(rects) if rects.len() >= MAX_PENDING_DAMAGE_RECTS => {
                *self = Self::Full;
            }
            Self::Rects(rects) => rects.push(rect),
            Self::Full => unreachable!("full damage returned above"),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn rects(&self) -> &[DamageRect] {
        match self {
            Self::Rects(rects) => rects,
            Self::Empty | Self::Full => &[],
        }
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl From<Vec<DamageRect>> for DamageRegion {
    fn from(rects: Vec<DamageRect>) -> Self {
        let mut region = Self::default();
        for rect in rects {
            region.push(rect);
        }
        region
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SurfaceContentSnapshot {
    buffer_id: Option<u32>,
    dimensions: Option<(i32, i32)>,
}

impl SurfaceContentSnapshot {
    fn from_buffer(buffer_id: u32) -> Self {
        Self {
            buffer_id: Some(buffer_id),
            dimensions: None,
        }
    }

    fn buffer_id(&self) -> Option<u32> {
        self.buffer_id
    }

    fn dimensions(&self) -> Option<(i32, i32)> {
        self.dimensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceState {
    /// Immutable metadata for the committed surface contents plus an optional
    /// live wl_buffer object reference. The reference may disappear after
    /// wl_buffer.destroy while the dimensions remain valid for damage mapping.
    current_content: Option<SurfaceContentSnapshot>,
    /// `Some(None)` represents an explicit `attach(NULL)`, while `None`
    /// means that this commit has no attach request at all.
    pub pending_buffer_id: Option<Option<u32>>,
    /// Damage expressed in surface-local coordinates. It can only be copied
    /// directly when the current buffer has the default transform and no
    /// viewport; otherwise the compositor falls back to a complete copy.
    pub pending_surface_damage: DamageRegion,
    /// Damage expressed in buffer pixel coordinates.
    pub pending_buffer_damage: DamageRegion,
    /// Buffer scale/transform are double-buffered by wl_surface. Keeping the
    /// state here lets the commit path decide whether a damage rectangle can
    /// be mapped safely.
    pub pending_buffer_scale: Option<i32>,
    pub current_buffer_scale: i32,
    pub pending_buffer_transform: Option<i32>,
    pub current_buffer_transform: i32,
    /// `wl_surface.offset` is also double-buffered. The SHM bridge does not
    /// currently transform surface damage through a non-zero offset, so the
    /// commit path conservatively performs a complete copy in that case.
    pub pending_offset: Option<(i32, i32)>,
    /// For wl_surface versions before 5, attach(x, y) carries the pending
    /// buffer offset. Keep it separate from the v5+ offset request so a
    /// zero-valued attach does not overwrite a real `wl_surface.offset`
    /// request that appeared earlier in the same state batch.
    pub pending_attach_offset: Option<(i32, i32)>,
    /// A viewport object exists for this surface. The object's state is
    /// tracked separately because an unset viewport is an identity mapping.
    pub viewport: Option<ViewportState>,
    /// Pending viewport state applied by the next surface commit. `Some(None)`
    /// represents destruction of the viewport object; `None` means unchanged.
    pub pending_viewport: Option<Option<ViewportState>>,
}

/// One atomically prepared `wl_surface.commit`.
///
/// Preparing a commit consumes every pending double-buffered field and applies
/// it to [`SurfaceState`]. The complete pre-commit snapshot is retained so any
/// validation or buffer-copy failure can restore the exact prior state. This
/// makes rollback automatically cover fields added to `SurfaceState` later,
/// instead of relying on a parallel list of manually restored fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceCommit {
    previous: SurfaceState,
    pub state: SurfaceState,
    /// `Some(None)` is an explicit `attach(NULL)`; `None` means no attach was
    /// included in this commit.
    pub attachment: Option<Option<u32>>,
    pub surface_damage: DamageRegion,
    pub buffer_damage: DamageRegion,
    /// One-shot placement of the pending attachment relative to the previous
    /// surface contents. wl_surface.offset replaces legacy attach(x, y); it is
    /// consumed by this commit and is not persistent surface state.
    pub buffer_offset: (i32, i32),
}

impl SurfaceCommit {
    pub fn has_buffer_attach(&self) -> bool {
        matches!(self.attachment, Some(Some(_)))
    }

    pub fn attached_buffer_id(&self) -> Option<u32> {
        self.attachment.flatten()
    }

    pub fn attachment_transition(&self) -> Option<(Option<u32>, Option<u32>)> {
        self.attachment
            .map(|next| (self.previous.current_buffer_id(), next))
    }

    pub fn has_full_damage(&self) -> bool {
        self.surface_damage.is_full() || self.buffer_damage.is_full()
    }

    pub fn uses_full_mapping(&self) -> bool {
        self.state.current_buffer_scale != 1
            || self.state.current_buffer_transform != 0
            || (self.has_buffer_attach() && self.buffer_offset != (0, 0))
            || self
                .state
                .viewport
                .is_some_and(|viewport| !viewport.is_identity())
    }

    pub fn has_invalid_fractional_viewport(&self) -> bool {
        let Some(viewport) = self.state.viewport else {
            return false;
        };
        let Some((_, _, width, height)) = viewport.source else {
            return false;
        };
        viewport.destination.is_none() && (width % 256 != 0 || height % 256 != 0)
    }

    pub fn rollback(self, surface: &mut SurfaceState) {
        *surface = self.previous;
    }
}

impl SurfaceState {
    pub(crate) fn current_buffer_id(&self) -> Option<u32> {
        self.current_content
            .as_ref()
            .and_then(SurfaceContentSnapshot::buffer_id)
    }

    pub(crate) fn current_buffer_dimensions(&self) -> Option<(i32, i32)> {
        self.current_content
            .as_ref()
            .and_then(SurfaceContentSnapshot::dimensions)
    }

    pub(crate) fn set_current_buffer_dimensions(
        &mut self,
        buffer_id: u32,
        dimensions: Option<(i32, i32)>,
    ) -> bool {
        if let Some(content) = self.current_content.as_mut() {
            if content.buffer_id == Some(buffer_id) {
                content.dimensions = dimensions;
                return true;
            }
        }
        false
    }

    pub(crate) fn clear_current_buffer_reference(&mut self, buffer_id: u32) {
        let remove_content = self.current_content.as_mut().is_some_and(|content| {
            if content.buffer_id != Some(buffer_id) {
                return false;
            }
            content.buffer_id = None;
            content.dimensions.is_none()
        });
        if remove_content {
            self.current_content = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_current_buffer_for_test(
        &mut self,
        buffer_id: Option<u32>,
        dimensions: Option<(i32, i32)>,
    ) {
        self.current_content = buffer_id
            .map(|buffer_id| SurfaceContentSnapshot {
                buffer_id: Some(buffer_id),
                dimensions,
            })
            .or_else(|| {
                dimensions.map(|dimensions| SurfaceContentSnapshot {
                    buffer_id: None,
                    dimensions: Some(dimensions),
                })
            });
    }

    /// Apply and consume all state pending for the next surface commit.
    pub fn prepare_commit(&mut self) -> SurfaceCommit {
        let previous = self.clone();
        let attachment = self.pending_buffer_id.take();
        if let Some(buffer_id) = attachment {
            self.current_content = buffer_id.map(SurfaceContentSnapshot::from_buffer);
        }
        if let Some(scale) = self.pending_buffer_scale.take() {
            self.current_buffer_scale = scale;
        }
        if let Some(transform) = self.pending_buffer_transform.take() {
            self.current_buffer_transform = transform;
        }
        let buffer_offset = self
            .pending_offset
            .take()
            .or_else(|| self.pending_attach_offset.take())
            .unwrap_or((0, 0));
        // Consume a legacy attach offset even when an explicit offset wins.
        self.pending_attach_offset = None;
        if let Some(viewport) = self.pending_viewport.take() {
            self.viewport = viewport;
        }
        let surface_damage = self.pending_surface_damage.take();
        let buffer_damage = self.pending_buffer_damage.take();

        SurfaceCommit {
            previous,
            state: self.clone(),
            attachment,
            surface_damage,
            buffer_damage,
            buffer_offset,
        }
    }
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            current_content: None,
            pending_buffer_id: None,
            pending_surface_damage: DamageRegion::default(),
            pending_buffer_damage: DamageRegion::default(),
            pending_buffer_scale: None,
            current_buffer_scale: 1,
            pending_buffer_transform: None,
            current_buffer_transform: 0,
            pending_offset: None,
            pending_attach_offset: None,
            viewport: None,
            pending_viewport: None,
        }
    }
}

pub struct PendingParam {
    pub fd: RawFd,
    pub plane_idx: u32,
    pub offset: u32,
    pub stride: u32,
    pub modifier_hi: u32,
    pub modifier_lo: u32,
}

/// Resources owned by one asynchronous linux-dmabuf create generation.
///
/// Host params IDs identify generations even after the guest destroys and
/// reuses its numeric object ID. Keeping the dimensions and synchronization
/// descriptor together under that host ID prevents a late result from an old
/// generation from consuming resources owned by its replacement.
pub struct PendingNativeCreate {
    pub guest_params_id: u32,
    pub dimensions: (i32, i32),
    pub sync_fds: Vec<OwnedFd>,
}

pub struct TextInputState {
    pub host_v1_id: u32,
    pub host_ext_id: Option<u32>,
    pub guest_seat: u32,
    pub active_surface: Option<u32>,
    /// State requested by the guest since its most recent v3 commit.
    pub pending_enabled: bool,
    /// State applied by the most recent v3 commit.
    pub committed_enabled: bool,
    /// Whether an enable/disable request is waiting for the next commit.
    pub enabled_dirty: bool,
    /// Double-buffered v3 surrounding-text state.
    pub pending_surrounding_text: Option<(String, i32, i32)>,
    pub committed_surrounding_text: Option<(String, i32, i32)>,
    pub surrounding_text_dirty: bool,
    pub content_hint: u32,
    pub content_purpose: u32,
    /// Content type last committed to the host. `None` forces the next v3
    /// transaction to replay the pending type after an enable/disable reset.
    pub committed_content_type: Option<(u32, u32)>,
    pub content_type_dirty: bool,
    pub cursor_rect: Option<(i32, i32, i32, i32)>,
    pub cursor_rect_dirty: bool,
    pub text_change_cause: u32,
    pub current_preedit: String,
    /// Number of v3 commit requests received from this guest object.
    ///
    /// The v3 protocol requires every `done` event to use this counter as its
    /// serial. The same value is sent to the v1 host in `commit_state`, but
    /// Exo assigns its own serials to v1 events, so event translation must use
    /// this local counter rather than trusting the host event serial.
    pub guest_commit_serial: u32,
    /// Cursor/selection metadata accumulated before the next v1 preedit event.
    pub pending_preedit_cursor: Option<i32>,
    pub pending_preedit_selection: Option<(u32, u32)>,
    /// Edits accumulated by v1 and applied atomically by the following
    /// `commit_string` event.
    pub pending_deletes: Vec<(u32, u32)>,
    pub pending_cursor_position: Option<(i32, i32)>,
    /// The proxy has entered the IME-consumed Backspace repeat path. This can
    /// be armed by physical key state or by a non-empty-to-empty preedit
    /// transition when Exo consumes the key event entirely.
    pub empty_preedit_repeat_active: bool,
    pub host_activated: bool,
}

/// Provenance for one physical key generation observed through ChromeOS
/// `peek_key`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeekKeyProvenance {
    pub serial: u32,
    pub time: u32,
    pub sequence: u64,
    /// The generation may recover an IME-consumed repeat. Host accelerators
    /// permanently clear this bit for the lifetime of the generation.
    pub eligible: bool,
}

/// Causal watermark for the newest physical generation in one seat/focus
/// domain.
///
/// Recording the owning keyboard lets keyboard teardown retire an otherwise
/// stale watermark atomically with its key generations. Without the owner, a
/// newer generation from a destroyed keyboard can permanently make an older
/// still-held key on another keyboard ineligible for IME repeat recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeekWatermark {
    keyboard: HostId,
    sequence: u64,
}

/// Exclusive guest-side ownership of one evdev key generation.
///
/// A key can have at most one owner: either a real `wl_keyboard` press is
/// awaiting its release, a text-input keysym press is awaiting its release, or
/// IME recovery already emitted a balanced pair and later host events must be
/// suppressed. Encoding these states as one enum prevents the parallel-set
/// inconsistencies that previously caused duplicate and stuck keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyOwner {
    Physical,
    TextInputKeysym,
    /// A balanced synthetic pair was delivered. This completed-generation
    /// tombstone suppresses delayed duplicate channels and permits repeat
    /// recovery without leaving an open guest press.
    ImeRecovery,
}

/// Normalized guest-delivery input from every host key channel.
///
/// Handlers decode protocol-specific fields and provide policy facts, while
/// the key-generation registry alone decides ownership, forwarding, and
/// wl_keyboard ACK semantics. The sum type prevents impossible combinations
/// such as attaching a host ACK to a text-input keysym.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyEvent {
    PhysicalPress {
        repeated: bool,
        host_accelerator: bool,
        ime_repeat_active: bool,
    },
    PhysicalRelease,
    TextInputPress {
        serial: u32,
    },
    TextInputRepeat,
    TextInputRelease {
        serial: u32,
    },
    RecoverIme,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyDelivery {
    Drop,
    Forward,
    EmitBalancedPair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestKeyDecision {
    pub delivery: GuestKeyDelivery,
    /// Present only for real wl_keyboard events.
    pub ack_handled: Option<bool>,
    /// The event closes the current generation's repeat session. Retired
    /// delayed releases deliberately leave a newer session untouched.
    pub ends_repeat: bool,
}

/// A guest press from a retired physical generation whose release channel has
/// not arrived yet.
///
/// Physical releases can be matched exactly to the preceding peek release.
/// Text-input releases only carry their own serial, so they are matched by
/// wrap-aware ordering against the press serial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetiredGuestRelease {
    owner: GuestKeyOwner,
    press_serial: Option<u32>,
    release_serial: Option<u32>,
    next_press_serial: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PhysicalKeyState {
    #[default]
    Unseen,
    Held,
    Released,
}

/// All mutable state for one keyboard/key generation.
///
/// A generation is retained after physical release only while a delayed guest
/// delivery channel still needs its ownership or suppression tombstone. This
/// keeps physical state, ChromeOS peek provenance, IME repeat cancellation,
/// and guest delivery decisions under one authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyGeneration {
    id: u64,
    physical_state: PhysicalKeyState,
    peek: Option<PeekKeyProvenance>,
    /// Serial of the generation's initial peek press. Retained after physical
    /// release while another tombstone keeps the generation alive so the
    /// corresponding delayed wl_keyboard press can still be recognized.
    peek_press_serial: Option<u32>,
    /// Serial of the first physical release observed from either the extended
    /// peek channel or the regular wl_keyboard channel.
    physical_release_serial: Option<u32>,
    backspace_repeat_cancelled: bool,
    guest_owner: Option<GuestKeyOwner>,
    guest_press_serial: Option<u32>,
    host_accelerator_suppressed: bool,
}

impl KeyGeneration {
    fn new(id: u64) -> Self {
        Self {
            id,
            physical_state: PhysicalKeyState::Unseen,
            peek: None,
            peek_press_serial: None,
            physical_release_serial: None,
            backspace_repeat_cancelled: false,
            guest_owner: None,
            guest_press_serial: None,
            host_accelerator_suppressed: false,
        }
    }

    fn is_unreferenced(self) -> bool {
        self.physical_state != PhysicalKeyState::Held
            && self.peek.is_none()
            && !self.backspace_repeat_cancelled
            && self.guest_owner.is_none()
            && !self.host_accelerator_suppressed
    }
}

/// Canonical state machine for every physical keyboard/key generation.
#[derive(Default)]
pub struct KeyGenerationRegistry {
    next_generation: u64,
    entries: HashMap<HostId, HashMap<u32, KeyGeneration>>,
    retired_guest_releases: HashMap<(HostId, u32), Vec<RetiredGuestRelease>>,
    latest_peek_sequences: HashMap<(u32, Option<u32>), PeekWatermark>,
}

impl KeyGenerationRegistry {
    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.next_generation
    }

    fn ensure_generation(&mut self, keyboard: HostId, key: u32) -> &mut KeyGeneration {
        let needs_entry = !self
            .entries
            .get(&keyboard)
            .is_some_and(|keys| keys.contains_key(&key));
        if needs_entry {
            let id = self.allocate_generation();
            self.entries
                .entry(keyboard)
                .or_default()
                .insert(key, KeyGeneration::new(id));
        }
        self.entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .expect("key generation was inserted")
    }

    fn prune_key(&mut self, keyboard: HostId, key: u32) {
        let remove = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.is_unreferenced());
        if remove {
            if let Some(keys) = self.entries.get_mut(&keyboard) {
                keys.remove(&key);
            }
        }
        if self
            .entries
            .get(&keyboard)
            .is_some_and(|keys| keys.is_empty())
        {
            self.entries.remove(&keyboard);
        }
    }

    pub(crate) fn clear_keyboard(&mut self, keyboard: HostId) {
        self.entries.remove(&keyboard);
        self.retired_guest_releases
            .retain(|(pending_keyboard, _), _| *pending_keyboard != keyboard);
        self.latest_peek_sequences
            .retain(|_, watermark| watermark.keyboard != keyboard);
    }

    pub(crate) fn record_latest_peek(
        &mut self,
        guest_seat: u32,
        focused_surface: Option<u32>,
        keyboard: HostId,
        sequence: u64,
    ) {
        self.latest_peek_sequences.insert(
            (guest_seat, focused_surface),
            PeekWatermark { keyboard, sequence },
        );
    }

    pub(crate) fn latest_peek_sequence(
        &self,
        guest_seat: u32,
        focused_surface: Option<u32>,
    ) -> Option<u64> {
        self.latest_peek_sequences
            .get(&(guest_seat, focused_surface))
            .map(|watermark| watermark.sequence)
    }

    pub(crate) fn clear_peek_watermarks_for_seat(&mut self, guest_seat: u32) {
        self.latest_peek_sequences
            .retain(|(seat, _), _| *seat != guest_seat);
    }

    pub(crate) fn clear_peek_watermarks_for_surface(&mut self, guest_surface: u32) {
        self.latest_peek_sequences
            .retain(|(_, surface), _| *surface != Some(guest_surface));
    }

    pub(crate) fn physically_held(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Held)
    }

    pub(crate) fn physical_released(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Released)
    }

    pub(crate) fn peek_press_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.peek_press_serial)
    }

    pub(crate) fn physical_release_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.physical_release_serial)
    }

    pub(crate) fn take_pending_physical_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        self.take_retired_guest_release(keyboard, key, |release| {
            release.owner == GuestKeyOwner::Physical && release.release_serial == Some(serial)
        })
    }

    pub(crate) fn take_pending_text_input_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        let current_press_serial = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| {
                generation
                    .guest_press_serial
                    .or(generation.peek_press_serial)
            });
        self.take_retired_guest_release(keyboard, key, |release| {
            release.owner == GuestKeyOwner::TextInputKeysym
                && release
                    .press_serial
                    .is_some_and(|press_serial| serial_is_after(serial, press_serial))
                && release
                    .next_press_serial
                    .or(current_press_serial)
                    .is_none_or(|current_serial| !serial_is_after(serial, current_serial))
        })
    }

    fn take_retired_guest_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        predicate: impl Fn(&RetiredGuestRelease) -> bool,
    ) -> bool {
        let map_key = (keyboard, key);
        let Some(releases) = self.retired_guest_releases.get_mut(&map_key) else {
            return false;
        };
        let Some(index) = releases.iter().position(predicate) else {
            return false;
        };
        releases.remove(index);
        if releases.is_empty() {
            self.retired_guest_releases.remove(&map_key);
        }
        true
    }

    pub(crate) fn any_physically_held(&self, keyboard: HostId) -> bool {
        self.entries.get(&keyboard).is_some_and(|keys| {
            keys.values()
                .any(|generation| generation.physical_state == PhysicalKeyState::Held)
        })
    }

    /// Observe one physical state notification from either keyboard channel.
    ///
    /// Both channels describe the same hardware generation, so a release from
    /// either one closes physical state. Guest-delivery ownership remains until
    /// its corresponding channel consumes the release.
    #[cfg(test)]
    pub(crate) fn observe_physical_state(&mut self, keyboard: HostId, key: u32, state: u32) {
        self.observe_physical_event(keyboard, key, state, None);
    }

    pub(crate) fn observe_physical_event(
        &mut self,
        keyboard: HostId,
        key: u32,
        state: u32,
        serial: Option<u32>,
    ) {
        match state {
            1 => self.observe_physical_press(keyboard, key, None),
            2 => {
                // A repeat is evidence about an existing physical generation,
                // never permission to invent one after a missing press.
                if let Some(generation) = self
                    .entries
                    .get_mut(&keyboard)
                    .and_then(|keys| keys.get_mut(&key))
                    .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    generation.physical_state = PhysicalKeyState::Held;
                }
            }
            0 => {
                if let Some(generation) = self
                    .entries
                    .get_mut(&keyboard)
                    .and_then(|keys| keys.get_mut(&key))
                {
                    generation.physical_state = PhysicalKeyState::Released;
                    if let Some(serial) = serial {
                        generation.physical_release_serial = Some(serial);
                    }
                    generation.backspace_repeat_cancelled = false;
                }
                // Match the previous physical/peek lifecycle: once no key on
                // this keyboard remains held, old peek provenance is no longer
                // a candidate. Delivery tombstones may still survive.
                if !self.any_physically_held(keyboard) {
                    let keys = self
                        .entries
                        .get(&keyboard)
                        .map(|keys| keys.keys().copied().collect::<Vec<_>>())
                        .unwrap_or_default();
                    if let Some(generations) = self.entries.get_mut(&keyboard) {
                        for generation in generations.values_mut() {
                            generation.peek = None;
                        }
                    }
                    for key in keys {
                        self.prune_key(keyboard, key);
                    }
                }
                self.prune_key(keyboard, key);
            }
            _ => {}
        }
    }

    fn observe_physical_press(
        &mut self,
        keyboard: HostId,
        key: u32,
        next_press_serial: Option<u32>,
    ) {
        self.retire_released_generation(keyboard, key, next_press_serial);
        self.ensure_generation(keyboard, key).physical_state = PhysicalKeyState::Held;
    }

    fn retire_released_generation(
        &mut self,
        keyboard: HostId,
        key: u32,
        next_press_serial: Option<u32>,
    ) -> bool {
        let Some(generation) = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .copied()
            .filter(|generation| generation.physical_state == PhysicalKeyState::Released)
        else {
            return false;
        };
        let retired_release = match generation.guest_owner {
            Some(GuestKeyOwner::Physical) => {
                generation
                    .physical_release_serial
                    .map(|release_serial| RetiredGuestRelease {
                        owner: GuestKeyOwner::Physical,
                        press_serial: generation.guest_press_serial,
                        release_serial: Some(release_serial),
                        next_press_serial,
                    })
            }
            Some(GuestKeyOwner::TextInputKeysym) => {
                generation
                    .guest_press_serial
                    .map(|press_serial| RetiredGuestRelease {
                        owner: GuestKeyOwner::TextInputKeysym,
                        press_serial: Some(press_serial),
                        release_serial: None,
                        next_press_serial,
                    })
            }
            Some(GuestKeyOwner::ImeRecovery) | None => None,
        };
        if let Some(retired_release) = retired_release {
            self.retired_guest_releases
                .entry((keyboard, key))
                .or_default()
                .push(retired_release);
        }
        if let Some(keys) = self.entries.get_mut(&keyboard) {
            keys.remove(&key);
        }
        true
    }

    pub(crate) fn install_enter_snapshot<I>(&mut self, keyboard: HostId, keys: I)
    where
        I: IntoIterator<Item = u32>,
    {
        self.clear_keyboard(keyboard);
        for key in keys {
            let generation = self.ensure_generation(keyboard, key);
            generation.physical_state = PhysicalKeyState::Held;
            generation.guest_owner = Some(GuestKeyOwner::Physical);
        }
    }

    pub(crate) fn peek(&self, keyboard: HostId, key: u32) -> Option<PeekKeyProvenance> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.peek)
    }

    pub(crate) fn peek_keys(
        &self,
        keyboard: HostId,
    ) -> impl Iterator<Item = (u32, PeekKeyProvenance)> + '_ {
        self.entries
            .get(&keyboard)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter_map(|(&key, generation)| generation.peek.map(|peek| (key, peek)))
    }

    pub(crate) fn observe_peek_press(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
        time: u32,
        eligible: bool,
    ) -> u64 {
        if !self.physically_held(keyboard, key) {
            // Unseen means a synthetic text-input channel arrived first and
            // this physical observation belongs to that same generation.
            // Released is the only unambiguous boundary for a new generation.
            self.observe_physical_press(keyboard, key, Some(serial));
        }
        let generation = self.ensure_generation(keyboard, key);
        let sequence = generation.id;
        generation.peek_press_serial = Some(serial);
        generation.peek = Some(PeekKeyProvenance {
            serial,
            time,
            sequence,
            eligible,
        });
        sequence
    }

    pub(crate) fn refresh_peek(&mut self, keyboard: HostId, key: u32, serial: u32, time: u32) {
        if let Some(generation) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
        {
            generation.peek_press_serial = Some(serial);
            if let Some(peek) = generation.peek.as_mut() {
                peek.serial = serial;
                peek.time = time;
            }
        }
    }

    pub(crate) fn observe_peek_release(&mut self, keyboard: HostId, key: u32, serial: u32) {
        self.observe_physical_event(keyboard, key, 0, Some(serial));
    }

    pub(crate) fn invalidate_peek(&mut self, keyboard: HostId, key: u32) {
        if let Some(peek) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .and_then(|generation| generation.peek.as_mut())
        {
            peek.eligible = false;
        }
    }

    pub(crate) fn cancel_backspace_repeat(&mut self, keyboard: HostId, backspace: u32) {
        if self.physically_held(keyboard, backspace) {
            self.ensure_generation(keyboard, backspace)
                .backspace_repeat_cancelled = true;
        }
    }

    pub(crate) fn backspace_repeat_cancelled(&self, keyboard: HostId, backspace: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&backspace))
            .is_some_and(|generation| generation.backspace_repeat_cancelled)
    }

    pub(crate) fn guest_owner(&self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.guest_owner)
    }

    pub(crate) fn guest_press_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.guest_press_serial)
    }

    pub(crate) fn claim_guest_owner(
        &mut self,
        keyboard: HostId,
        key: u32,
        owner: GuestKeyOwner,
    ) -> bool {
        let generation = self.ensure_generation(keyboard, key);
        if generation.guest_owner.is_some() {
            return false;
        }
        generation.guest_owner = Some(owner);
        true
    }

    pub(crate) fn claim_text_input_owner(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        let (starts_new, released) = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .map_or((false, false), |generation| {
                let newer_than_guest_press = generation
                    .guest_press_serial
                    .is_none_or(|press_serial| serial_is_after(serial, press_serial));
                let released = generation.physical_state == PhysicalKeyState::Released;
                let after_release_boundary = generation
                    .physical_release_serial
                    .is_some_and(|release_serial| serial_is_after(serial, release_serial));
                let owner_can_start_next = match generation.guest_owner {
                    Some(GuestKeyOwner::TextInputKeysym) => released,
                    Some(GuestKeyOwner::ImeRecovery) => {
                        generation.physical_state == PhysicalKeyState::Unseen || released
                    }
                    Some(GuestKeyOwner::Physical) | None => false,
                };
                (
                    owner_can_start_next
                        && newer_than_guest_press
                        && (!released || after_release_boundary),
                    released,
                )
            });
        if starts_new {
            if released {
                let retired = self.retire_released_generation(keyboard, key, Some(serial));
                debug_assert!(retired, "released generation was checked above");
            } else if let Some(keys) = self.entries.get_mut(&keyboard) {
                keys.remove(&key);
            }
        }
        let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym);
        if claimed {
            self.ensure_generation(keyboard, key).guest_press_serial = Some(serial);
        }
        claimed
    }

    pub(crate) fn take_guest_owner(&mut self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        let owner = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .and_then(|generation| {
                let owner = generation.guest_owner.take();
                if owner.is_some() {
                    generation.guest_press_serial = None;
                }
                owner
            });
        self.prune_key(keyboard, key);
        owner
    }

    #[cfg(test)]
    pub(crate) fn take_guest_owner_if(
        &mut self,
        keyboard: HostId,
        key: u32,
        expected: GuestKeyOwner,
    ) -> bool {
        let matches = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.guest_owner == Some(expected));
        if !matches {
            return false;
        }
        self.take_guest_owner(keyboard, key) == Some(expected)
    }

    pub(crate) fn complete_guest_owner_if(
        &mut self,
        keyboard: HostId,
        key: u32,
        expected: GuestKeyOwner,
    ) -> bool {
        let Some(generation) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .filter(|generation| generation.guest_owner == Some(expected))
        else {
            return false;
        };
        generation.guest_owner = Some(GuestKeyOwner::ImeRecovery);
        true
    }

    pub(crate) fn suppress_host_accelerator(&mut self, keyboard: HostId, key: u32) {
        self.ensure_generation(keyboard, key)
            .host_accelerator_suppressed = true;
    }

    pub(crate) fn take_host_accelerator_suppression(&mut self, keyboard: HostId, key: u32) -> bool {
        let suppressed = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .is_some_and(|generation| std::mem::take(&mut generation.host_accelerator_suppressed));
        self.prune_key(keyboard, key);
        suppressed
    }

    pub(crate) fn host_accelerator_suppressed(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.host_accelerator_suppressed)
    }

    pub(crate) fn clear_accelerator_suppressions(&mut self, keyboard: HostId) {
        let keys = self
            .entries
            .get(&keyboard)
            .map(|keys| keys.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(generations) = self.entries.get_mut(&keyboard) {
            for generation in generations.values_mut() {
                generation.host_accelerator_suppressed = false;
            }
        }
        for key in keys {
            self.prune_key(keyboard, key);
        }
    }

    /// Apply the only guest-delivery ownership transition for one key event.
    ///
    /// Physical state and peek provenance are observed separately because they
    /// can arrive even when no guest event is emitted. This reducer owns the
    /// mutually exclusive delivery channels and returns all information the
    /// protocol handlers need to encode their result.
    pub(crate) fn transition_guest_key(
        &mut self,
        keyboard: HostId,
        key: u32,
        event: GuestKeyEvent,
    ) -> GuestKeyDecision {
        match event {
            GuestKeyEvent::PhysicalPress {
                repeated,
                host_accelerator,
                ime_repeat_active,
            } => {
                let owner = self.guest_owner(keyboard, key);
                let forwarded = matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                );
                let ime_recovered = owner == Some(GuestKeyOwner::ImeRecovery);
                let suppress_for_ime = ime_recovered || (ime_repeat_active && !forwarded);
                if suppress_for_ime && owner.is_none() {
                    let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::ImeRecovery);
                    debug_assert!(claimed, "guest owner was checked above");
                }

                let accelerator_was_suppressed = self.host_accelerator_suppressed(keyboard, key);
                if accelerator_was_suppressed || host_accelerator {
                    if !forwarded {
                        self.suppress_host_accelerator(keyboard, key);
                    }
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(false),
                        ends_repeat: false,
                    };
                }
                if suppress_for_ime {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(false),
                        ends_repeat: false,
                    };
                }
                if forwarded && !repeated {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(true),
                        ends_repeat: false,
                    };
                }
                if repeated {
                    return GuestKeyDecision {
                        delivery: if forwarded {
                            GuestKeyDelivery::Forward
                        } else {
                            GuestKeyDelivery::Drop
                        },
                        ack_handled: Some(forwarded),
                        ends_repeat: false,
                    };
                }

                let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::Physical);
                debug_assert!(claimed, "new physical delivery had no guest owner");
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::Forward,
                    ack_handled: Some(true),
                    ends_repeat: false,
                }
            }
            GuestKeyEvent::PhysicalRelease => {
                let accelerator_suppressed = self.take_host_accelerator_suppression(keyboard, key);
                let owner = self.guest_owner(keyboard, key);
                let ime_recovered = owner == Some(GuestKeyOwner::ImeRecovery);
                let forwarded = matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                );
                if !ime_recovered {
                    self.take_guest_owner(keyboard, key);
                }
                GuestKeyDecision {
                    delivery: if forwarded && !accelerator_suppressed && !ime_recovered {
                        GuestKeyDelivery::Forward
                    } else {
                        GuestKeyDelivery::Drop
                    },
                    ack_handled: Some(forwarded),
                    ends_repeat: true,
                }
            }
            GuestKeyEvent::TextInputPress { serial } => {
                let accelerator_generation = self.host_accelerator_suppressed(keyboard, key)
                    || (self.physically_held(keyboard, key)
                        && self
                            .peek(keyboard, key)
                            .is_some_and(|press| !press.eligible));
                if accelerator_generation
                    || self.guest_owner(keyboard, key) == Some(GuestKeyOwner::Physical)
                    || !self.claim_text_input_owner(keyboard, key, serial)
                {
                    GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: None,
                        ends_repeat: false,
                    }
                } else {
                    GuestKeyDecision {
                        delivery: GuestKeyDelivery::Forward,
                        ack_handled: None,
                        ends_repeat: false,
                    }
                }
            }
            GuestKeyEvent::TextInputRepeat => GuestKeyDecision {
                delivery: if matches!(
                    self.guest_owner(keyboard, key),
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                ) {
                    GuestKeyDelivery::Forward
                } else {
                    GuestKeyDelivery::Drop
                },
                ack_handled: None,
                ends_repeat: false,
            },
            GuestKeyEvent::TextInputRelease { serial } => {
                let retired = self.take_pending_text_input_release(keyboard, key, serial);
                let current = if retired {
                    true
                } else {
                    let serial_is_current = self
                        .guest_press_serial(keyboard, key)
                        .is_none_or(|press_serial| serial_is_after(serial, press_serial));
                    serial_is_current
                        && self.complete_guest_owner_if(
                            keyboard,
                            key,
                            GuestKeyOwner::TextInputKeysym,
                        )
                };
                GuestKeyDecision {
                    delivery: if current {
                        GuestKeyDelivery::Forward
                    } else {
                        GuestKeyDelivery::Drop
                    },
                    ack_handled: None,
                    ends_repeat: current && !retired,
                }
            }
            GuestKeyEvent::RecoverIme => {
                let owner = self.guest_owner(keyboard, key);
                if matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                ) {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: None,
                        ends_repeat: false,
                    };
                }
                if owner.is_none() {
                    let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::ImeRecovery);
                    debug_assert!(claimed, "guest owner was checked above");
                }
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::EmitBalancedPair,
                    ack_handled: None,
                    ends_repeat: false,
                }
            }
        }
    }
}

/// One `wl_keyboard` focus generation.
///
/// Keep both object namespaces: the guest surface drives text-input events,
/// while the host surface identifies delayed `wl_keyboard.leave` events even
/// after the guest mapping has been retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardFocus {
    pub guest_seat: u32,
    pub guest_surface: u32,
    pub host_surface: u32,
}

/// One authoritative seat focus transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeatFocusChange {
    pub guest_seat: u32,
    pub previous_surface: Option<u32>,
    pub current_surface: Option<u32>,
}

/// Result of mutating keyboard focus ownership.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct KeyboardFocusUpdate {
    /// Whether the event changed focus or consumed one deliverable retired leave.
    pub accepted: bool,
    /// Seat transitions that must be projected to text-input objects.
    pub seat_changes: Vec<SeatFocusChange>,
    /// Keyboard sessions whose focus-scoped key state is no longer valid.
    pub retired_keyboards: Vec<HostId>,
}

/// Canonical keyboard-to-seat/surface focus registry.
///
/// Every keyboard on one seat must own the same surface. Entering a different
/// surface retires older keyboard generations for that seat instead of
/// retaining a fallback focus that a delayed leave could revive.
#[derive(Default)]
pub struct KeyboardFocusRegistry {
    keyboards: HashMap<HostId, KeyboardFocus>,
    /// Focus generations superseded by a different keyboard resource.
    ///
    /// Their seat focus is no longer authoritative, but the guest resource
    /// received an enter and still needs exactly one matching leave. A newer
    /// enter on the same keyboard supersedes its older generation without a
    /// tombstone because forwarding that delayed leave would clear the newer
    /// resource focus.
    retired_guest_enters: HashMap<(HostId, u32), KeyboardFocus>,
}

impl KeyboardFocusRegistry {
    pub fn surface_for_seat(&self, guest_seat: u32) -> Option<u32> {
        self.keyboards
            .values()
            .find(|focus| focus.guest_seat == guest_seat)
            .map(|focus| focus.guest_surface)
    }

    pub fn focus_for_keyboard(&self, host_keyboard: HostId) -> Option<KeyboardFocus> {
        self.keyboards.get(&host_keyboard).copied()
    }

    pub fn keyboard_owns_surface(&self, host_keyboard: HostId, guest_surface: u32) -> bool {
        self.focus_for_keyboard(host_keyboard)
            .is_some_and(|focus| focus.guest_surface == guest_surface)
    }

    #[cfg(test)]
    pub fn set_for_test(
        &mut self,
        host_keyboard: HostId,
        guest_seat: u32,
        guest_surface: u32,
        host_surface: u32,
    ) {
        let _ = self.enter(
            host_keyboard,
            KeyboardFocus {
                guest_seat,
                guest_surface,
                host_surface,
            },
        );
    }

    pub fn enter(&mut self, host_keyboard: HostId, focus: KeyboardFocus) -> KeyboardFocusUpdate {
        if self.focus_for_keyboard(host_keyboard) == Some(focus) {
            return KeyboardFocusUpdate::default();
        }

        let mut affected_seats = vec![focus.guest_seat];
        if let Some(previous_focus) = self.focus_for_keyboard(host_keyboard) {
            affected_seats.push(previous_focus.guest_seat);
        }
        affected_seats.sort_unstable();
        affected_seats.dedup();
        let previous_surfaces = affected_seats
            .iter()
            .map(|&guest_seat| (guest_seat, self.surface_for_seat(guest_seat)))
            .collect::<Vec<_>>();

        // A newer enter on the same wl_keyboard resource supersedes every
        // older resource generation. Delayed leaves for those generations
        // must not become visible after the newer enter.
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, current)| {
                (keyboard == host_keyboard
                    || (current.guest_seat == focus.guest_seat
                        && current.guest_surface != focus.guest_surface))
                    .then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable_by_key(|keyboard| keyboard.0);
        for keyboard in &retired_keyboards {
            if let Some(retired_focus) = self.keyboards.remove(keyboard) {
                if *keyboard != host_keyboard {
                    self.retired_guest_enters
                        .insert((*keyboard, retired_focus.host_surface), retired_focus);
                }
            }
        }
        self.keyboards.insert(host_keyboard, focus);

        let seat_changes = previous_surfaces
            .into_iter()
            .filter_map(|(guest_seat, previous_surface)| {
                let current_surface = self.surface_for_seat(guest_seat);
                (previous_surface != current_surface).then_some(SeatFocusChange {
                    guest_seat,
                    previous_surface,
                    current_surface,
                })
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards,
        }
    }

    pub fn leave(&mut self, host_keyboard: HostId, host_surface: u32) -> KeyboardFocusUpdate {
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return if self
                .retired_guest_enters
                .remove(&(host_keyboard, host_surface))
                .is_some()
            {
                KeyboardFocusUpdate {
                    accepted: true,
                    ..KeyboardFocusUpdate::default()
                }
            } else {
                KeyboardFocusUpdate::default()
            };
        };
        if focus.host_surface != host_surface {
            return KeyboardFocusUpdate::default();
        }
        self.remove_keyboard(host_keyboard, focus)
    }

    pub fn release(&mut self, host_keyboard: HostId) -> KeyboardFocusUpdate {
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return KeyboardFocusUpdate::default();
        };
        self.remove_keyboard(host_keyboard, focus)
    }

    pub fn destroy_surface(&mut self, guest_surface: u32) -> KeyboardFocusUpdate {
        let mut affected_seats = self
            .keyboards
            .values()
            .filter_map(|focus| (focus.guest_surface == guest_surface).then_some(focus.guest_seat))
            .collect::<Vec<_>>();
        affected_seats.sort_unstable();
        affected_seats.dedup();

        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, focus)| {
                (focus.guest_surface == guest_surface).then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable_by_key(|keyboard| keyboard.0);
        for keyboard in &retired_keyboards {
            self.keyboards.remove(keyboard);
        }
        self.retired_guest_enters
            .retain(|_, focus| focus.guest_surface != guest_surface);

        let seat_changes = affected_seats
            .into_iter()
            .map(|guest_seat| SeatFocusChange {
                guest_seat,
                previous_surface: Some(guest_surface),
                current_surface: self.surface_for_seat(guest_seat),
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: !retired_keyboards.is_empty(),
            seat_changes,
            retired_keyboards,
        }
    }

    fn remove_keyboard(
        &mut self,
        host_keyboard: HostId,
        focus: KeyboardFocus,
    ) -> KeyboardFocusUpdate {
        let previous_surface = self.surface_for_seat(focus.guest_seat);
        self.keyboards.remove(&host_keyboard);
        let current_surface = self.surface_for_seat(focus.guest_seat);
        let seat_changes = (previous_surface != current_surface)
            .then_some(SeatFocusChange {
                guest_seat: focus.guest_seat,
                previous_surface,
                current_surface,
            })
            .into_iter()
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards: vec![host_keyboard],
        }
    }
}

/// Host-compositor use phase of one render buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderBufferUse {
    NeverSubmitted,
    AwaitingRelease {
        /// At least one earlier attachment was replaced before the host
        /// compositor emitted `wl_buffer.release`. No surface destructor can
        /// prove that detached use complete; only the release event can.
        has_detached_use: bool,
    },
    Released,
}

impl RenderBufferUse {
    fn awaiting() -> Self {
        Self::AwaitingRelease {
            has_detached_use: false,
        }
    }

    fn is_awaiting_release(&self) -> bool {
        matches!(self, Self::AwaitingRelease { .. })
    }
}

/// Guest/host ownership phase of one render buffer.
///
/// The lifecycle and use phase live in one enum so guest-destroyed backing
/// cannot accidentally remain in a separate "live" map, and a queued host
/// destructor cannot still be represented as compositor-owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderBufferLifecycle {
    GuestAlive(RenderBufferUse),
    GuestDestroyed(RenderBufferUse),
    HostDestroyQueued,
}

impl RenderBufferLifecycle {
    pub fn use_state(&self) -> Option<&RenderBufferUse> {
        match self {
            Self::GuestAlive(use_state) | Self::GuestDestroyed(use_state) => Some(use_state),
            Self::HostDestroyQueued => None,
        }
    }

    pub fn is_guest_destroyed(&self) -> bool {
        matches!(self, Self::GuestDestroyed(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderBufferOwnership {
    GuestAlive,
    GuestDestroyed,
    HostDestroyQueued,
}

/// Storage owned by one host `wl_buffer` generation.
enum RenderBufferBacking {
    /// Guest SHM copied into proxy-owned host storage.
    LocalCopy(BufferState),
    /// Guest-created linux-dmabuf forwarded without a CPU copy.
    Native {
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    },
}

struct RenderBuffer {
    backing: Option<RenderBufferBacking>,
    ownership: RenderBufferOwnership,
    use_state: RenderBufferUse,
    implicit_sync_fallback: bool,
}

/// Canonical host-ID keyed registry for every render buffer.
///
/// A host ID is the Wayland generation identity: it remains unique while late
/// `release` and `delete_id` events are in flight even if the guest numeric ID
/// becomes reusable. All backing, use, and ownership state therefore moves
/// together under this one key.
#[derive(Default)]
struct RenderBufferRegistry {
    entries: HashMap<HostId, RenderBuffer>,
}

impl RenderBufferRegistry {
    fn register_local(&mut self, host_id: HostId, backing: BufferState) -> bool {
        match self.entries.entry(host_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RenderBuffer {
                    backing: Some(RenderBufferBacking::LocalCopy(backing)),
                    ownership: RenderBufferOwnership::GuestAlive,
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    fn register_native(
        &mut self,
        host_id: HostId,
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    ) -> bool {
        match self.entries.entry(host_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RenderBuffer {
                    backing: Some(RenderBufferBacking::Native { size, sync_fds }),
                    ownership: RenderBufferOwnership::GuestAlive,
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    fn contains(&self, host_id: HostId) -> bool {
        self.entries.contains_key(&host_id)
    }

    fn local_copy_mut(&mut self, host_id: HostId) -> Option<&mut BufferState> {
        match self.entries.get_mut(&host_id)?.backing.as_mut()? {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    fn local_copy(&self, host_id: HostId) -> Option<&BufferState> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    fn dimensions(&self, host_id: HostId) -> Option<(i32, i32)> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::LocalCopy(backing) => Some((backing.width, backing.height)),
            RenderBufferBacking::Native { size, .. } => Some(*size),
        }
    }

    fn native_sync_fds(&self, host_id: HostId) -> Option<&[OwnedFd]> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::Native { sync_fds, .. } => Some(sync_fds),
            RenderBufferBacking::LocalCopy(_) => None,
        }
    }

    fn uses_implicit_sync_fallback(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .is_some_and(|buffer| buffer.implicit_sync_fallback)
    }

    fn enable_implicit_sync_fallback(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if !matches!(buffer.backing, Some(RenderBufferBacking::Native { .. })) {
            return false;
        }
        buffer.implicit_sync_fallback = true;
        true
    }

    fn lifecycle(&self, host_id: HostId) -> Option<RenderBufferLifecycle> {
        let buffer = self.entries.get(&host_id)?;
        Some(match buffer.ownership {
            RenderBufferOwnership::GuestAlive => {
                RenderBufferLifecycle::GuestAlive(buffer.use_state.clone())
            }
            RenderBufferOwnership::GuestDestroyed => {
                RenderBufferLifecycle::GuestDestroyed(buffer.use_state.clone())
            }
            RenderBufferOwnership::HostDestroyQueued => RenderBufferLifecycle::HostDestroyQueued,
        })
    }

    fn lifecycles(&self) -> impl Iterator<Item = (HostId, RenderBufferLifecycle)> + '_ {
        self.entries.keys().filter_map(|&host_id| {
            self.lifecycle(host_id)
                .map(|lifecycle| (host_id, lifecycle))
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn remove(&mut self, host_id: HostId) -> bool {
        self.entries.remove(&host_id).is_some()
    }

    fn can_submit(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .is_some_and(|buffer| buffer.ownership != RenderBufferOwnership::HostDestroyQueued)
    }

    fn can_detach(&self, host_id: HostId) -> bool {
        self.entries.get(&host_id).is_some_and(|buffer| {
            buffer.ownership != RenderBufferOwnership::HostDestroyQueued
                && !matches!(buffer.use_state, RenderBufferUse::NeverSubmitted)
        })
    }

    fn submit(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { .. } => {}
            RenderBufferUse::NeverSubmitted | RenderBufferUse::Released => {
                buffer.use_state = RenderBufferUse::awaiting();
            }
        }
        true
    }

    fn release(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued
            || !buffer.use_state.is_awaiting_release()
        {
            return false;
        }
        buffer.use_state = RenderBufferUse::Released;
        true
    }

    fn detach(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                *has_detached_use = true;
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted => false,
        }
    }

    fn end_last_surface_use(&mut self, host_id: HostId, has_other_current: bool) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        if has_other_current {
            return true;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                if !*has_detached_use {
                    buffer.use_state = RenderBufferUse::NeverSubmitted;
                }
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted => false,
        }
    }

    /// Atomically apply one successful `wl_surface` attachment replacement.
    ///
    /// Both generations are validated before either lifecycle changes. This
    /// keeps a failed replacement from latching the old buffer as detached or
    /// beginning a use interval for the new buffer.
    fn finalize_attachment(&mut self, previous: Option<HostId>, next: Option<HostId>) -> bool {
        if previous == next {
            return next.is_none_or(|host_id| self.submit(host_id));
        }
        if previous.is_some_and(|host_id| !self.can_detach(host_id))
            || next.is_some_and(|host_id| !self.can_submit(host_id))
        {
            return false;
        }
        if let Some(host_id) = previous {
            debug_assert!(self.detach(host_id));
        }
        if let Some(host_id) = next {
            debug_assert!(self.submit(host_id));
        }
        true
    }

    fn mark_guest_destroyed(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        match buffer.ownership {
            RenderBufferOwnership::GuestAlive => {
                buffer.ownership = RenderBufferOwnership::GuestDestroyed;
                true
            }
            RenderBufferOwnership::GuestDestroyed | RenderBufferOwnership::HostDestroyQueued => {
                false
            }
        }
    }

    fn mark_host_destroy_queued(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        buffer.backing = None;
        buffer.ownership = RenderBufferOwnership::HostDestroyQueued;
        true
    }
}

#[derive(Debug, Default)]
pub struct DmabufCapabilityState {
    /// Legacy v3 format/modifier pairs collected for one host-global
    /// generation. The vector preserves the host's preference order while the
    /// linux-dmabuf handler rejects duplicate pairs.
    pub format_modifiers: Vec<(u32, u64)>,
    /// Set only after a host wl_display.sync callback proves that every
    /// format/modifier event generated by the internal v3 bind has arrived.
    pub ready: bool,
}

/// Internal `wl_display.sync` barriers that separate host text-input
/// activation generations.
///
/// Host text-input-v1 objects have no destructor and are reused across guest
/// focus changes. A deactivate followed by this barrier proves that every
/// event from the previous activation has been dispatched while
/// `host_activated` is false, before the object may be activated again.
#[derive(Default)]
pub struct TextInputActivationBarrierRegistry {
    by_callback: HashMap<HostId, (u32, u32)>,
    by_text_input_generation: HashMap<(u32, u32), HostId>,
}

impl TextInputActivationBarrierRegistry {
    pub(crate) fn is_pending(&self, guest_text_input: u32, host_v1_id: u32) -> bool {
        self.by_text_input_generation
            .contains_key(&(guest_text_input, host_v1_id))
    }

    pub(crate) fn install(
        &mut self,
        callback: HostId,
        guest_text_input: u32,
        host_v1_id: u32,
    ) -> bool {
        if self.by_callback.contains_key(&callback)
            || self
                .by_text_input_generation
                .contains_key(&(guest_text_input, host_v1_id))
        {
            return false;
        }
        self.by_callback
            .insert(callback, (guest_text_input, host_v1_id));
        self.by_text_input_generation
            .insert((guest_text_input, host_v1_id), callback);
        true
    }

    pub(crate) fn complete(&mut self, callback: HostId) -> Option<(u32, u32)> {
        let generation = self.by_callback.remove(&callback)?;
        if self.by_text_input_generation.get(&generation) == Some(&callback) {
            self.by_text_input_generation.remove(&generation);
        }
        Some(generation)
    }

    #[cfg(test)]
    pub(crate) fn callback_for(&self, guest_text_input: u32, host_v1_id: u32) -> Option<HostId> {
        self.by_text_input_generation
            .get(&(guest_text_input, host_v1_id))
            .copied()
    }
}

pub struct Context {
    pub shadow_table: ShadowTable,
    pub pools: HashMap<u32, Arc<PoolState>>,
    render_buffers: RenderBufferRegistry,
    /// Asynchronous linux-dmabuf creates keyed by host params generation.
    pub pending_native_creates: HashMap<HostId, PendingNativeCreate>,
    /// Async linux-dmabuf params objects whose guest destructor was sent
    /// before the host emitted `created`/`failed`. The key is the host params
    /// ID, which may outlive the guest mapping when the host acknowledges the
    /// params destructor first.
    pub orphaned_dmabuf_params: HashMap<u32, u32>,
    pub surfaces: HashMap<u32, SurfaceState>,
    pub text_inputs: HashMap<u32, TextInputState>,
    pub keyboard_to_seat: HashMap<u32, u32>,
    pub keyboard_focus: KeyboardFocusRegistry,
    pub last_sender_id: u32,
    /// Pending messages to send from client→host (e.g. ack_key, bind requests).
    pub client_to_host_queue: Vec<(Vec<u8>, Vec<RawFd>)>,
    /// Pending messages to send from host→client (e.g. synthetic wl_shm.format).
    pub host_to_client_queue: Vec<(Vec<u8>, Vec<RawFd>)>,
    /// A queued `wl_display.error` must be flushed before the client session
    /// is torn down. This is set by fatal protocol validation paths.
    pub fatal_protocol_error: bool,
    /// Monotonic serial source for synthetic keyboard compatibility events.
    pub synthetic_keyboard_serial: u32,
    pub allocator: Option<Allocator>,
    /// Test-only capability override for exercising synthetic dma-buf
    /// feedback lifecycle without depending on the runner's `/dev/dri`.
    #[cfg(test)]
    pub synthetic_feedback_available_for_test: bool,
    pub virtwayland_channel: Option<Arc<VirtWaylandChannel>>,
    pub host_dmabuf_id: Option<u32>,
    /// Global name that produced the currently bound internal dmabuf object.
    pub host_dmabuf_global_name: Option<u32>,
    pub host_shm_id: Option<u32>,
    /// Global name that produced the currently bound internal wl_shm object.
    pub host_shm_global_name: Option<u32>,
    /// Global names that produced the internal singleton bindings. These
    /// names let registry removal reset exactly the binding that disappeared,
    /// without tearing down a duplicate or a newer global generation.
    pub host_text_input_manager_v1_global_name: Option<u32>,
    pub host_text_input_extension_v1_global_name: Option<u32>,
    pub host_keyboard_extension_global_name: Option<u32>,
    pub host_zaura_shell_global_name: Option<u32>,
    /// Formats observed from the host's SHM or dmabuf capability events that
    /// the SHM bridge can actually copy. ARGB/XRGB are always available per
    /// the wl_shm contract; optional formats are added only after the host
    /// advertises them.
    pub host_shm_formats: HashSet<u32>,
    /// Optional formats learned from the internal host wl_shm binding.
    /// Keeping the source separate lets a dmabuf global disappear without
    /// invalidating a format that the host's real wl_shm object still offers.
    pub host_wl_shm_formats: HashSet<u32>,
    /// Optional formats learned from the internal host linux-dmabuf binding.
    /// These are removed when that binding's global is withdrawn.
    pub host_dmabuf_shm_formats: HashSet<u32>,
    /// Formats already sent to each synthetic guest wl_shm object. Keeping
    /// this per object prevents duplicate format events when host capability
    /// events arrive after a guest bind.
    pub shm_guest_formats: HashMap<u32, HashSet<u32>>,
    /// Synthetic guest wl_shm objects whose host capability binding has been
    /// removed. Wayland keeps an already-bound global object valid for
    /// teardown, but requests sent to it after global removal are ignored.
    /// Keep these IDs reserved and reject create_pool without allowing a
    /// replacement host wl_shm binding to service the old object.
    pub stale_shm_guest_objects: HashSet<u32>,
    /// Synthetic wl_shm_pool children created before the host capability
    /// binding disappeared. They remain destroyable, but must not resize
    /// local mappings or create buffers through a replacement host binding.
    pub stale_shm_pools: HashSet<u32>,
    pub host_text_input_manager_v1_id: Option<u32>,
    pub host_text_input_extension_v1_id: Option<u32>,
    /// Host-side zcr_keyboard_extension_v1 object ID (bound internally on startup).
    pub host_keyboard_extension_id: Option<HostId>,
    /// Maps host keyboard ID → host extended-keyboard ID for `ack_key`.
    ///
    /// Both key and value are [`HostId`]s intentionally — using [`GuestId`] here
    /// by mistake is a **compile error**, preventing the direction bug where a
    /// client→host request handler reads `ctx.last_sender_id` (a guest ID) and
    /// uses it to look up a host-keyed map.
    pub keyboard_to_extended_keyboard: HashMap<HostId, HostId>,
    /// Reverse lookup for keyboard-extension v2 events. `peek_key` is sent by
    /// the extended-keyboard object, while the physical-key state belongs to
    /// the corresponding host `wl_keyboard`.
    pub extended_keyboard_to_keyboard: HashMap<HostId, HostId>,
    /// Physical, peek, repeat-recovery, and guest-delivery state for every
    /// keyboard/key generation.
    pub key_generations: KeyGenerationRegistry,
    /// Evdev keycodes that the active XKB keymap marks as repeatable.
    pub keyboard_repeatable_keys: HashMap<HostId, HashSet<u32>>,
    /// Effective keysym → evdev keycode mappings from each host keyboard's
    /// negotiated XKB keymap. Text-input-v1 `keysym` events do not carry a
    /// physical keycode, so the IME bridge uses this per-keyboard map when it
    /// synthesizes a wl_keyboard event.
    pub keyboard_keysym_to_keycode: HashMap<HostId, HashMap<u32, u32>>,
    /// Parsed SOMMELIER_ACCELERATORS: keys the host should handle.
    pub accelerators: Vec<crate::accelerator::Accelerator>,
    pub supported_formats: HashSet<u32>,
    /// Host globals visible to the guest, keyed by their unique numeric name.
    pub host_globals: HashMap<u32, HostGlobal>,
    /// Host globals consumed internally by the proxy and therefore never
    /// advertised to the guest. Keeping their names lets global_remove clean
    /// the corresponding host-only object without leaking a removal event.
    pub hidden_host_globals: HashMap<u32, String>,
    /// Global names already emitted by each host registry object. A single
    /// Wayland client may create more than one wl_registry; the compositor
    /// sends the complete global list to each one, so this must not be a
    /// connection-wide set keyed only by global name.
    pub registry_global_names: HashMap<u32, HashSet<u32>>,
    /// Names removed from each registry. A subsequent global event for a
    /// removed name represents a new advertisement generation.
    pub registry_global_removed: HashMap<u32, HashSet<u32>>,
    pub registry_global_generations: HashMap<u32, HashMap<u32, u64>>,
    /// Whether each registry's current advertisement for a name was visible
    /// to the guest. This is kept per registry/generation so a delayed
    /// global_remove for an old hidden/visible generation cannot be classified
    /// using replacement metadata.
    pub registry_global_visibility: HashMap<u32, HashMap<u32, bool>>,
    pub global_generations: HashMap<u32, u64>,
    pub next_global_generation: u64,
    /// Globals that have been removed from the host registry. Keep their
    /// metadata until every registry has observed the removal so a second
    /// registry can still receive its own global_remove event, while blocking
    /// new binds in the meantime.
    pub removed_host_globals: HashSet<u32>,
    pub pending_params: HashMap<u32, Vec<PendingParam>>,
    /// Guest linux-dmabuf params objects consumed by create/create_immed.
    ///
    /// The protocol object remains alive after consumption so subsequent
    /// add/create requests can report `already_used` instead of being
    /// confused with an object that was never tracked.
    pub used_dmabuf_params: HashSet<u32>,
    pub feedback_index_maps: HashMap<u32, HashMap<u16, u16>>,
    /// Guest feedback objects synthesized locally from the host's legacy
    /// linux-dmabuf format/modifier events, keyed by guest feedback ID. The
    /// generation keeps an object created from an old, still-bound factory
    /// isolated from a replacement global's capabilities.
    pub synthetic_feedback_objects: HashMap<u32, u64>,
    /// Avoid queuing one complete feedback sequence for every legacy
    /// format/modifier event. A host can advertise hundreds of pairs in a
    /// single roundtrip; sending one memfd per event can overflow the
    /// receiver's SCM_RIGHTS batch and desynchronize the Wayland FD stream.
    /// The proxy clears this set after each dispatch batch.
    pub synthetic_feedback_refresh_pending: HashSet<u32>,
    /// Capability snapshots for every host dmabuf global generation referenced
    /// by a live guest object. A completed snapshot is immutable except for a
    /// defensive refresh if a non-conforming host sends a late unique pair.
    pub dmabuf_capabilities: HashMap<u64, DmabufCapabilityState>,
    /// Generation collected by the current internal host dmabuf binding.
    pub host_dmabuf_generation: Option<u64>,
    /// Host-only wl_callback IDs used as capability-discovery barriers.
    pub dmabuf_capability_callbacks: HashMap<u32, u64>,
    /// Host callback generations that drain stale text-input events before
    /// reactivation of a reused v1 object.
    pub text_input_activation_barriers: TextInputActivationBarrierRegistry,
    /// Guest-facing v4 globals withheld until the internal v3 binding has
    /// delivered its complete legacy format/modifier capability set.
    pub pending_dmabuf_globals: Vec<PendingDmabufGlobal>,
    /// Guest dmabuf factory ID to the host-global generation from which it was
    /// bound. Global removal does not invalidate an already-bound factory.
    pub dmabuf_guest_generations: HashMap<u32, u64>,
    pub gpu_accel: bool,
    pub xdg_decoration: bool,
    /// Host-side zaura_shell object ID (bound internally, not exposed to guest).
    pub host_zaura_shell_id: Option<u32>,
    /// Bound version of zaura_shell (capped at 38 in registry). Used to guard
    /// opcodes that require specific protocol versions.
    pub host_zaura_shell_version: u32,
    /// VM identifier for ChromeOS guest_os app ID formatting (from SOMMELIER_VM_IDENTIFIER).
    pub vm_identifier: String,
    /// Maps host wl_surface ID → host zaura_surface ID for app ID passthrough.
    pub wl_surface_to_zaura_surface: HashMap<u32, u32>,
    /// Tracks xdg_surface → wl_surface associations (guest IDs).
    pub xdg_surface_to_wl_surface: HashMap<u32, u32>,
    /// Tracks xdg_toplevel → wl_surface associations (guest IDs).
    pub xdg_toplevel_to_wl_surface: HashMap<u32, u32>,
    /// Tracks wp_viewport objects back to their associated wl_surface so
    /// destroying a viewport restores the default damage coordinate mapping.
    pub viewport_to_wl_surface: HashMap<u32, u32>,
    /// Clipboard transfer tasks own the VirtWL/read and client/write
    /// descriptors until the transfer reaches EOF. Keep their join handles
    /// with the connection so a client disconnect can cancel the transfer
    /// instead of leaving a detached task and two open descriptors behind.
    pub(crate) clipboard_pumps: Vec<tokio::task::JoinHandle<()>>,
}

pub(crate) struct LocalBufferCopyResources<'a> {
    pub allocator: Option<&'a Allocator>,
    pub channel: Option<&'a Arc<VirtWaylandChannel>>,
    pub buffer: &'a mut BufferState,
}

impl Context {
    pub(crate) fn render_buffer_host_id(&self, guest_buffer_id: u32) -> Option<HostId> {
        self.shadow_table
            .get_host_id(guest_buffer_id)
            .map(HostId)
            .filter(|host_id| self.render_buffers.contains(*host_id))
    }

    pub(crate) fn register_local_buffer(
        &mut self,
        guest_buffer_id: u32,
        host_buffer_id: u32,
        backing: BufferState,
    ) -> bool {
        debug_assert_eq!(
            self.shadow_table.get_host_id(guest_buffer_id),
            Some(host_buffer_id)
        );
        self.render_buffers
            .register_local(HostId(host_buffer_id), backing)
    }

    pub(crate) fn register_native_buffer(
        &mut self,
        host_buffer_id: u32,
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    ) -> bool {
        self.render_buffers
            .register_native(HostId(host_buffer_id), size, sync_fds)
    }

    pub(crate) fn local_buffer(&self, guest_buffer_id: u32) -> Option<&BufferState> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.render_buffers.local_copy(host_id)
    }

    pub(crate) fn local_buffer_copy_resources(
        &mut self,
        host_id: HostId,
    ) -> Option<LocalBufferCopyResources<'_>> {
        let allocator = self.allocator.as_ref();
        let channel = self.virtwayland_channel.as_ref();
        let buffer = self.render_buffers.local_copy_mut(host_id)?;
        Some(LocalBufferCopyResources {
            allocator,
            channel,
            buffer,
        })
    }

    pub(crate) fn render_buffer_lifecycles(
        &self,
    ) -> impl Iterator<Item = (HostId, RenderBufferLifecycle)> + '_ {
        self.render_buffers.lifecycles()
    }

    #[cfg(test)]
    pub(crate) fn render_buffer_lifecycle_for_host(
        &self,
        host_id: HostId,
    ) -> Option<RenderBufferLifecycle> {
        self.render_buffers.lifecycle(host_id)
    }

    #[cfg(test)]
    pub(crate) fn has_render_buffer_host(&self, host_id: HostId) -> bool {
        self.render_buffers.contains(host_id)
    }

    #[cfg(test)]
    pub(crate) fn render_buffer_count(&self) -> usize {
        self.render_buffers.len()
    }

    pub(crate) fn buffer_dimensions(&self, guest_buffer_id: u32) -> Option<(i32, i32)> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.buffer_dimensions_for_host(host_id)
    }

    pub(crate) fn buffer_dimensions_for_host(&self, host_id: HostId) -> Option<(i32, i32)> {
        self.render_buffers.dimensions(host_id)
    }

    pub(crate) fn native_buffer_sync_fds(&self, guest_buffer_id: u32) -> Option<&[OwnedFd]> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.render_buffers.native_sync_fds(host_id)
    }

    pub(crate) fn native_buffer_uses_implicit_sync(&self, guest_buffer_id: u32) -> bool {
        self.render_buffer_host_id(guest_buffer_id)
            .is_some_and(|host_id| self.render_buffers.uses_implicit_sync_fallback(host_id))
    }

    pub(crate) fn enable_native_buffer_implicit_sync(&mut self, guest_buffer_id: u32) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.enable_implicit_sync_fallback(host_id)
    }

    pub(crate) fn host_buffer_use(&self, guest_buffer_id: u32) -> Option<RenderBufferUse> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.render_buffers.lifecycle(host_id)?.use_state().cloned()
    }

    #[cfg(test)]
    pub(crate) fn mark_buffer_submitted(&mut self, guest_buffer_id: u32) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.submit(host_id)
    }

    pub(crate) fn mark_buffer_released(&mut self, guest_buffer_id: u32) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.release(host_id)
    }

    pub(crate) fn finish_surface_destroy_use(
        &mut self,
        guest_buffer_id: u32,
        has_other_current: bool,
    ) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers
            .end_last_surface_use(host_id, has_other_current)
    }

    pub(crate) fn finalize_surface_attachment(
        &mut self,
        previous_guest_buffer: Option<u32>,
        next_guest_buffer: Option<u32>,
    ) -> bool {
        let previous = previous_guest_buffer.and_then(|id| self.render_buffer_host_id(id));
        let next = next_guest_buffer.and_then(|id| self.render_buffer_host_id(id));
        self.render_buffers.finalize_attachment(previous, next)
    }

    pub(crate) fn mark_buffer_guest_destroyed(&mut self, guest_buffer_id: u32) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.mark_guest_destroyed(host_id)
    }

    pub(crate) fn remove_render_buffer_host(&mut self, host_buffer_id: u32) -> bool {
        self.render_buffers.remove(HostId(host_buffer_id))
    }

    /// Apply the ID-ownership policy after a host `wl_buffer.destroy` has
    /// successfully entered the ordered outgoing stream.
    ///
    /// Client-created IDs must survive until the host's `delete_id` can be
    /// translated back to the guest. Asynchronous dmabuf buffers use
    /// server-created IDs on both sides, so the queued destructor is their
    /// terminal lifecycle edge and the complete generation is removed now.
    pub(crate) fn complete_queued_buffer_destroy(
        &mut self,
        guest_buffer_id: u32,
        host_buffer_id: HostId,
    ) -> bool {
        if self.shadow_table.get_host_id(guest_buffer_id) != Some(host_buffer_id.0) {
            return false;
        }

        if self.shadow_table.is_guest_server_id(guest_buffer_id) {
            if !self.render_buffers.remove(host_buffer_id) {
                return false;
            }
            self.shadow_table.remove_id(guest_buffer_id);
        } else {
            if self.render_buffers.contains(host_buffer_id)
                && !self.render_buffers.mark_host_destroy_queued(host_buffer_id)
            {
                return false;
            }
            self.shadow_table.mark_pending_destroy(guest_buffer_id);
        }
        true
    }

    pub(crate) fn buffer_is_submitted(&self, guest_buffer_id: u32) -> bool {
        self.host_buffer_use(guest_buffer_id)
            .is_some_and(|use_state| use_state.is_awaiting_release())
    }

    pub(crate) fn buffer_is_released(&self, guest_buffer_id: u32) -> bool {
        self.host_buffer_use(guest_buffer_id) == Some(RenderBufferUse::Released)
    }

    pub(crate) fn host_buffer_is_guest_destroyed(&self, host_buffer_id: u32) -> bool {
        self.render_buffers
            .lifecycle(HostId(host_buffer_id))
            .is_some_and(|lifecycle| lifecycle.is_guest_destroyed())
    }

    pub(crate) fn guest_key_owner(
        &self,
        host_keyboard_id: HostId,
        key: u32,
    ) -> Option<GuestKeyOwner> {
        self.key_generations.guest_owner(host_keyboard_id, key)
    }

    pub(crate) fn transition_guest_key(
        &mut self,
        host_keyboard_id: HostId,
        key: u32,
        event: GuestKeyEvent,
    ) -> GuestKeyDecision {
        self.key_generations
            .transition_guest_key(host_keyboard_id, key, event)
    }

    /// Claim delivery ownership for a key that currently has no guest owner.
    ///
    /// Returning `false` leaves the existing owner untouched. Callers can
    /// therefore reject duplicate presses without accidentally changing which
    /// event source must close or suppress the eventual release.
    #[cfg(test)]
    pub(crate) fn claim_guest_key(
        &mut self,
        host_keyboard_id: HostId,
        key: u32,
        owner: GuestKeyOwner,
    ) -> bool {
        self.key_generations
            .claim_guest_owner(host_keyboard_id, key, owner)
    }

    /// Release a key only when the expected source still owns it.
    #[cfg(test)]
    pub(crate) fn take_guest_key_if(
        &mut self,
        host_keyboard_id: HostId,
        key: u32,
        expected: GuestKeyOwner,
    ) -> bool {
        self.key_generations
            .take_guest_owner_if(host_keyboard_id, key, expected)
    }

    pub fn new(gpu_accel: bool, xdg_decoration: bool) -> Self {
        // Initialize allocator
        let allocator = match Allocator::new() {
            Ok(alloc) => Some(alloc),
            Err(e) => {
                warn!("Failed to initialize GBM allocator: {}", e);
                None
            }
        };

        let accelerators_env = std::env::var("SOMMELIER_ACCELERATORS").unwrap_or_default();
        let accelerators = match crate::accelerator::parse_accelerators(&accelerators_env) {
            Ok(list) => list,
            Err(e) => {
                // A malformed accelerator config should not crash the proxy — that
                // would break every app in the container. Degrade to no filtering
                // (all keys forwarded to guest) and log a clear error.
                warn!(
                    "Invalid SOMMELIER_ACCELERATORS '{}': {}. \
                     Accelerator filtering disabled.",
                    accelerators_env, e
                );
                Vec::new()
            }
        };

        Self {
            shadow_table: ShadowTable::new(),
            pools: HashMap::new(),
            render_buffers: RenderBufferRegistry::default(),
            pending_native_creates: HashMap::new(),
            orphaned_dmabuf_params: HashMap::new(),
            surfaces: HashMap::new(),
            text_inputs: HashMap::new(),
            keyboard_to_seat: HashMap::new(),
            keyboard_focus: KeyboardFocusRegistry::default(),
            last_sender_id: 0,
            client_to_host_queue: Vec::new(),
            host_to_client_queue: Vec::new(),
            fatal_protocol_error: false,
            synthetic_keyboard_serial: 0,
            allocator,
            #[cfg(test)]
            synthetic_feedback_available_for_test: false,
            virtwayland_channel: None,
            host_dmabuf_id: None,
            host_dmabuf_global_name: None,
            host_shm_id: None,
            host_shm_global_name: None,
            host_text_input_manager_v1_global_name: None,
            host_text_input_extension_v1_global_name: None,
            host_keyboard_extension_global_name: None,
            host_zaura_shell_global_name: None,
            host_shm_formats: HashSet::new(),
            host_wl_shm_formats: HashSet::new(),
            host_dmabuf_shm_formats: HashSet::new(),
            shm_guest_formats: HashMap::new(),
            stale_shm_guest_objects: HashSet::new(),
            stale_shm_pools: HashSet::new(),
            host_text_input_manager_v1_id: None,
            host_text_input_extension_v1_id: None,
            host_keyboard_extension_id: None,
            keyboard_to_extended_keyboard: HashMap::new(),
            extended_keyboard_to_keyboard: HashMap::new(),
            key_generations: KeyGenerationRegistry::default(),
            keyboard_repeatable_keys: HashMap::new(),
            keyboard_keysym_to_keycode: HashMap::new(),
            accelerators,
            supported_formats: HashSet::new(),
            host_globals: HashMap::new(),
            hidden_host_globals: HashMap::new(),
            registry_global_names: HashMap::new(),
            registry_global_removed: HashMap::new(),
            registry_global_generations: HashMap::new(),
            registry_global_visibility: HashMap::new(),
            global_generations: HashMap::new(),
            next_global_generation: 1,
            removed_host_globals: HashSet::new(),
            pending_params: HashMap::new(),
            used_dmabuf_params: HashSet::new(),
            feedback_index_maps: HashMap::new(),
            synthetic_feedback_objects: HashMap::new(),
            synthetic_feedback_refresh_pending: HashSet::new(),
            dmabuf_capabilities: HashMap::new(),
            host_dmabuf_generation: None,
            dmabuf_capability_callbacks: HashMap::new(),
            text_input_activation_barriers: TextInputActivationBarrierRegistry::default(),
            pending_dmabuf_globals: Vec::new(),
            dmabuf_guest_generations: HashMap::new(),
            gpu_accel,
            xdg_decoration,
            host_zaura_shell_id: None,
            host_zaura_shell_version: 0,
            vm_identifier: resolve_vm_identifier(std::env::var("SOMMELIER_VM_IDENTIFIER").ok()),
            wl_surface_to_zaura_surface: HashMap::new(),
            xdg_surface_to_wl_surface: HashMap::new(),
            xdg_toplevel_to_wl_surface: HashMap::new(),
            viewport_to_wl_surface: HashMap::new(),
            clipboard_pumps: Vec::new(),
        }
    }

    /// Cancel clipboard pumps when the client connection is going away.
    ///
    /// The asynchronous caller should await this method so Tokio runs each
    /// cancelled future's destructor and closes its owned descriptors.
    pub(crate) async fn stop_clipboard_pumps(&mut self) {
        let pumps = std::mem::take(&mut self.clipboard_pumps);
        for pump in pumps {
            pump.abort();
            let _ = pump.await;
        }
    }

    /// Remove completed transfer handles while retaining active pumps.
    pub(crate) fn reap_clipboard_pumps(&mut self) {
        self.clipboard_pumps.retain(|pump| !pump.is_finished());
    }

    /// Test-only constructor that overrides `SOMMELIER_ACCELERATORS` after construction.
    ///
    /// `Context::new` reads `SOMMELIER_ACCELERATORS` from the environment, so
    /// using it in unit tests would make test outcomes depend on whether the
    /// developer's shell has that variable set. This constructor calls `new()`
    /// and then immediately replaces `ctx.accelerators` with the supplied list,
    /// ensuring tests always run against a known accelerator configuration
    /// regardless of the environment.
    #[cfg(test)]
    pub fn new_for_test(
        gpu_accel: bool,
        xdg_decoration: bool,
        accelerators: Vec<crate::accelerator::Accelerator>,
    ) -> Self {
        let mut ctx = Self::new(gpu_accel, xdg_decoration);
        ctx.accelerators = accelerators;
        ctx
    }
}

/// Resolve the VM namespace used in ChromeOS application IDs.
///
/// An exported-but-empty environment variable is equivalent to an unset one.
/// This mirrors ChromiumOS Sommelier's `strlen(vm_id) != 0` fallback and keeps
/// generated IDs valid (`org.chromium.guest_os.termina.wayland.<app-id>`).
fn resolve_vm_identifier(value: Option<String>) -> String {
    value
        .filter(|identifier| !identifier.is_empty())
        .unwrap_or_else(|| "termina".to_string())
}

impl Default for Context {
    fn default() -> Self {
        Self::new(false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::{FromRawFd, OwnedFd};

    fn awaiting_release(has_detached_use: bool) -> RenderBufferUse {
        RenderBufferUse::AwaitingRelease { has_detached_use }
    }

    #[tokio::test]
    async fn stop_clipboard_pumps_aborts_and_closes_owned_fds() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let read_fd = unsafe { libc::fcntl(pipe_fds[0], libc::F_DUPFD_CLOEXEC, 1000) };
        let write_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1001) };
        assert!(read_fd >= 1000);
        assert!(write_fd >= 1001);
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        let read_owned = unsafe { OwnedFd::from_raw_fd(read_fd) };
        let write_owned = unsafe { OwnedFd::from_raw_fd(write_fd) };
        let pump = tokio::spawn(async move {
            let mut input = tokio::fs::File::from_std(std::fs::File::from(read_owned));
            let mut output = tokio::fs::File::from_std(std::fs::File::from(write_owned));
            let _ = tokio::io::copy(&mut input, &mut output).await;
        });

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.clipboard_pumps.push(pump);
        ctx.stop_clipboard_pumps().await;

        assert_eq!(unsafe { libc::fcntl(read_fd, libc::F_GETFD) }, -1);
        assert_eq!(unsafe { libc::fcntl(write_fd, libc::F_GETFD) }, -1);
    }

    #[test]
    fn allocate_host_id_skips_zero_and_one_after_wrap() {
        // Verify that when next_host_id wraps from u32::MAX to 0, the allocator
        // correctly skips the reserved IDs (0 = null, 1 = wl_display) and returns
        // a valid ID ≥ 2 on both the allocation that reads u32::MAX *and* the
        // subsequent allocation that reads the post-wrap value.
        //
        // Walk-through:
        //   iter 0: id = u32::MAX, advance → wrapping_add(1)=0, .max(2)=2
        //           u32::MAX >= 2 and not in map → return u32::MAX  ✓
        //   (next call) iter 0: id = 2, advance → 3
        //                       2 >= 2 and not in map → return 2  ✓
        let mut table = ShadowTable::new();
        table.next_host_id = u32::MAX;

        let id1 = table.allocate_host_id();
        assert!(id1 >= 2, "must never return 0 or 1, got {}", id1);

        // The second allocation happens after the counter has wrapped to 2.
        // It must also return a valid ID and must not collide with id1.
        let id2 = table.allocate_host_id();
        assert!(
            id2 >= 2,
            "post-wrap allocation must skip reserved IDs, got {}",
            id2
        );
        assert_ne!(id1, id2, "successive allocations must return distinct IDs");
    }

    #[test]
    fn allocate_host_id_wraps_correctly() {
        // Verify that starting with next_host_id = 0 (simulating a state where
        // the counter was somehow zeroed) results in the first returned ID being
        // a valid value ≥ 2.
        //
        // Walk-through:
        //   iter 0: id = 0, advance → wrapping_add(1)=1, .max(2)=2
        //           0 < 2 → skip
        //   iter 1: id = 2, advance → 3
        //           2 ≥ 2 and not in map → return 2  ✓
        let mut table = ShadowTable::new();
        table.next_host_id = 0;
        let id = table.allocate_host_id();
        assert!(
            id >= 2,
            "post-zero allocation must skip reserved IDs, got {}",
            id
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
        // next_host_id starts at 2.
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
    fn allocate_guest_server_id_wraps_inside_server_range() {
        let mut table = ShadowTable::new();
        table.next_guest_server_id = u32::MAX;

        let last = table.allocate_guest_server_id();
        let first = table.allocate_guest_server_id();

        assert_eq!(last, u32::MAX);
        assert_eq!(first, ShadowTable::GUEST_SERVER_ID_START);
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

        assert!(table.is_local_only_guest_object(20));
        assert!(table.is_local_only_guest_object(21));
        assert!(table.is_local_only_guest_object(22));
        assert!(!table.is_local_only_guest_object(23));
        assert!(!table.is_local_only_guest_object(24));
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
        table.next_host_id = 40;
        assert_eq!(
            table.allocate_host_id(),
            41,
            "pending host-only IDs must not be reallocated"
        );

        // The host acknowledgement is the only point at which the reservation
        // can be released.
        assert!(table.consume_host_delete_id(40));
        assert!(!table.is_pending_destroy_host_only(40));
        table.next_host_id = 40;
        assert_eq!(
            table.allocate_host_id(),
            40,
            "the acknowledged ID may be reused after delete_id"
        );
        assert!(!table.consume_host_delete_id(40));
    }

    #[test]
    fn empty_or_missing_vm_identifier_defaults_to_termina() {
        assert_eq!(resolve_vm_identifier(None), "termina");
        assert_eq!(resolve_vm_identifier(Some(String::new())), "termina");
        assert_eq!(
            resolve_vm_identifier(Some("penguin".to_string())),
            "penguin"
        );
    }

    #[test]
    fn guest_id_and_host_id_raw_round_trip() {
        // Verify the typed constructors round-trip through the same inner u32.
        let ctx = Context::new(false, false);
        let mut ctx = ctx;
        ctx.last_sender_id = 42;
        let gid = GuestId::from_request_sender(&ctx);
        assert_eq!(gid.0, 42);

        ctx.last_sender_id = 99;
        let hid = HostId::from_event_sender(&ctx);
        assert_eq!(hid.0, 99);
    }

    #[test]
    fn context_drop_closes_queued_and_pending_fds() {
        let mut pipes = [[-1; 2]; 3];
        for pipe in &mut pipes {
            let result = unsafe { libc::pipe(pipe.as_mut_ptr()) };
            assert_eq!(result, 0, "pipe should be created");
        }
        let read_fds: Vec<_> = pipes.iter().map(|pipe| pipe[0]).collect();
        let queued_fd = pipes[0][1];
        let duplicate_queued_fd = pipes[1][1];
        let pending_fd = pipes[2][1];
        let owned_targets = [queued_fd, duplicate_queued_fd, pending_fd]
            .into_iter()
            .map(|fd| {
                fs::read_link(format!("/proc/self/fd/{fd}"))
                    .expect("owned test descriptor should have a procfs target")
            })
            .collect::<Vec<_>>();

        let mut ctx = Context::new_for_test(false, false, vec![]);
        // The same descriptor appearing in two queues is not a normal path,
        // but Drop must remain safe if an error path leaves duplicated
        // bookkeeping behind.
        ctx.client_to_host_queue.push((Vec::new(), vec![queued_fd]));
        ctx.host_to_client_queue
            .push((Vec::new(), vec![queued_fd, duplicate_queued_fd]));
        ctx.pending_params.insert(
            7,
            vec![PendingParam {
                fd: pending_fd,
                plane_idx: 0,
                offset: 0,
                stride: 4,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        drop(ctx);

        for (fd, target) in [queued_fd, duplicate_queued_fd, pending_fd]
            .into_iter()
            .zip(owned_targets)
        {
            let current = fs::read_link(format!("/proc/self/fd/{fd}"));
            assert!(
                current.as_ref().map_or(true, |current| current != &target),
                "Context::drop must release fd {} (current target: {:?})",
                fd,
                current
            );
        }
        for fd in read_fds {
            unsafe {
                libc::close(fd);
            }
        }
    }

    #[test]
    fn poisoned_pool_lock_still_unmaps_pool_memory_on_drop() {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page_size > 0);
        let page_size = page_size as usize;
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapped, libc::MAP_FAILED);

        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let pool = PoolState {
            client_fd: pipe_fds[0],
            inner: RwLock::new(PoolInner {
                client_ptr: mapped,
                size: page_size,
            }),
        };

        // Poison the lock while retaining the mapping in its protected value.
        // PoolState::drop must clean it up instead of treating the poisoned
        // write lock as an empty pool.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = pool.inner.write().expect("lock should initially work");
            panic!("intentional pool lock poison");
        }));

        let mut residency = 0u8;
        assert_eq!(
            unsafe { libc::mincore(mapped, page_size, &mut residency) },
            0,
            "mapping should still exist before PoolState::drop"
        );

        drop(pool);
        unsafe {
            libc::close(pipe_fds[1]);
        }

        errno_reset();
        assert_eq!(
            unsafe { libc::mincore(mapped, page_size, &mut residency) },
            -1,
            "PoolState::drop must unmap a poisoned pool"
        );
        assert_eq!(errno_value(), libc::ENOMEM);
    }

    #[test]
    fn surface_commit_applies_every_pending_field_atomically() {
        let mut surface = SurfaceState {
            current_content: None,
            pending_buffer_id: Some(Some(2)),
            pending_surface_damage: vec![DamageRect::new(1, 2, 3, 4)].into(),
            pending_buffer_damage: vec![DamageRect::new(5, 6, 7, 8)].into(),
            pending_buffer_scale: Some(2),
            current_buffer_scale: 1,
            pending_buffer_transform: Some(3),
            current_buffer_transform: 0,
            pending_offset: Some((9, 10)),
            pending_attach_offset: Some((11, 12)),
            viewport: None,
            pending_viewport: Some(Some(ViewportState {
                source: Some((0, 0, 256, 256)),
                destination: Some((20, 30)),
            })),
        };
        surface.set_current_buffer_for_test(Some(1), Some((10, 20)));

        let commit = surface.prepare_commit();

        assert_eq!(commit.attachment, Some(Some(2)));
        assert_eq!(commit.attached_buffer_id(), Some(2));
        assert!(commit.has_buffer_attach());
        assert!(commit.uses_full_mapping());
        assert!(!commit.has_invalid_fractional_viewport());
        assert_eq!(
            commit.surface_damage,
            DamageRegion::Rects(vec![DamageRect::new(1, 2, 3, 4)])
        );
        assert_eq!(
            commit.buffer_damage,
            DamageRegion::Rects(vec![DamageRect::new(5, 6, 7, 8)])
        );
        assert!(!commit.has_full_damage());
        assert_eq!(
            commit.buffer_offset,
            (9, 10),
            "an explicit offset must win over the legacy attach offset"
        );
        assert_eq!(surface.current_buffer_scale, 2);
        assert_eq!(surface.current_buffer_transform, 3);
        assert_eq!(
            surface.viewport,
            Some(ViewportState {
                source: Some((0, 0, 256, 256)),
                destination: Some((20, 30)),
            })
        );
        assert_eq!(surface, commit.state);
        assert!(surface.pending_buffer_id.is_none());
        assert!(surface.pending_surface_damage.is_empty());
        assert!(surface.pending_buffer_damage.is_empty());
        assert!(surface.pending_buffer_scale.is_none());
        assert!(surface.pending_buffer_transform.is_none());
        assert!(surface.pending_offset.is_none());
        assert!(surface.pending_attach_offset.is_none());
        assert!(surface.pending_viewport.is_none());

        let next = surface.prepare_commit();
        assert!(next.attachment.is_none());
        assert!(next.surface_damage.is_empty());
        assert!(next.buffer_damage.is_empty());
        assert!(!next.has_full_damage());
        assert_eq!(next.buffer_offset, (0, 0));
        assert_eq!(next.state, commit.state);
    }

    #[test]
    fn surface_commit_rollback_restores_the_complete_snapshot() {
        let mut surface = SurfaceState {
            current_content: None,
            pending_buffer_id: Some(None),
            pending_surface_damage: vec![DamageRect::new(1, 2, 3, 4)].into(),
            pending_buffer_damage: vec![DamageRect::new(5, 6, 7, 8)].into(),
            pending_buffer_scale: Some(2),
            current_buffer_scale: 1,
            pending_buffer_transform: Some(3),
            current_buffer_transform: 0,
            pending_offset: Some((4, 5)),
            pending_attach_offset: Some((11, 12)),
            viewport: Some(ViewportState::new()),
            pending_viewport: Some(None),
        };
        surface.set_current_buffer_for_test(Some(1), Some((10, 20)));
        let before = surface.clone();

        let commit = surface.prepare_commit();
        assert_eq!(commit.buffer_offset, (4, 5));
        assert_ne!(surface, before);
        commit.rollback(&mut surface);

        assert_eq!(surface, before);
    }

    #[test]
    fn surface_commit_detach_clears_committed_content_dimensions() {
        let mut surface = SurfaceState {
            pending_buffer_id: Some(None),
            ..SurfaceState::default()
        };
        surface.set_current_buffer_for_test(Some(1), Some((100, 50)));

        let commit = surface.prepare_commit();

        assert_eq!(commit.attachment, Some(None));
        assert_eq!(surface.current_buffer_id(), None);
        assert_eq!(surface.current_buffer_dimensions(), None);
    }

    #[test]
    fn content_snapshot_ignores_mismatched_destroy_generations() {
        let mut surface = SurfaceState::default();
        surface.set_current_buffer_for_test(Some(7), Some((100, 50)));

        surface.clear_current_buffer_reference(8);
        assert_eq!(surface.current_buffer_id(), Some(7));
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));

        surface.clear_current_buffer_reference(7);
        assert_eq!(surface.current_buffer_id(), None);
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));
        assert!(
            !surface.set_current_buffer_dimensions(7, Some((1, 1))),
            "a reused numeric ID must not mutate a dimensions-only old snapshot"
        );
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));
    }

    #[test]
    fn pending_damage_is_exact_until_bounded_then_becomes_full() {
        let mut surface = SurfaceState::default();
        let exact: Vec<_> = (0..MAX_PENDING_DAMAGE_RECTS)
            .map(|index| DamageRect::new(index as i32, 0, 1, 1))
            .collect();
        for rect in &exact {
            surface.pending_buffer_damage.push(*rect);
        }

        assert_eq!(surface.pending_buffer_damage, DamageRegion::Rects(exact));

        surface
            .pending_buffer_damage
            .push(DamageRect::new(-10, -20, 30, 40));
        assert_eq!(surface.pending_buffer_damage, DamageRegion::Full);

        for _ in 0..MAX_PENDING_DAMAGE_RECTS {
            surface
                .pending_buffer_damage
                .push(DamageRect::new(1, 1, 1, 1));
        }
        assert_eq!(
            surface.pending_buffer_damage,
            DamageRegion::Full,
            "a collapsed region must remain bounded"
        );

        let commit = surface.prepare_commit();
        assert_eq!(commit.buffer_damage, DamageRegion::Full);
        assert!(commit.has_full_damage());
        assert!(surface.pending_buffer_damage.is_empty());
    }

    #[test]
    fn collapsed_damage_rollback_restores_the_full_pending_region() {
        let mut surface = SurfaceState::default();
        for index in 0..=MAX_PENDING_DAMAGE_RECTS {
            surface
                .pending_surface_damage
                .push(DamageRect::new(index as i32, 0, 1, 1));
        }
        let pending = surface.clone();

        let commit = surface.prepare_commit();
        assert_eq!(commit.surface_damage, DamageRegion::Full);
        commit.rollback(&mut surface);

        assert_eq!(surface, pending);
        let retry = surface.prepare_commit();
        assert_eq!(retry.surface_damage, DamageRegion::Full);
    }

    #[test]
    fn surface_commit_validates_fractional_viewport_as_one_state() {
        let mut surface = SurfaceState {
            pending_viewport: Some(Some(ViewportState {
                source: Some((0, 0, 257, 256)),
                destination: None,
            })),
            ..SurfaceState::default()
        };
        assert!(surface.prepare_commit().has_invalid_fractional_viewport());

        surface.pending_viewport = Some(Some(ViewportState {
            source: Some((0, 0, 257, 256)),
            destination: Some((10, 10)),
        }));
        assert!(!surface.prepare_commit().has_invalid_fractional_viewport());
    }

    #[test]
    fn host_buffer_use_is_an_exclusive_phase() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let buffer = 20;
        let host_buffer = 40;
        ctx.shadow_table.map_id(buffer, host_buffer);
        assert!(ctx.register_native_buffer(host_buffer, (1, 1), Vec::new()));

        assert_eq!(
            ctx.host_buffer_use(buffer),
            Some(RenderBufferUse::NeverSubmitted)
        );
        assert!(ctx.mark_buffer_submitted(buffer));
        assert_eq!(ctx.host_buffer_use(buffer), Some(awaiting_release(false)));
        assert!(ctx.buffer_is_submitted(buffer));
        assert!(!ctx.buffer_is_released(buffer));

        assert!(ctx.mark_buffer_released(buffer));
        assert_eq!(ctx.host_buffer_use(buffer), Some(RenderBufferUse::Released));
        assert!(!ctx.buffer_is_submitted(buffer));
        assert!(ctx.buffer_is_released(buffer));

        assert!(ctx.mark_buffer_submitted(buffer));
        assert_eq!(
            ctx.host_buffer_use(buffer),
            Some(awaiting_release(false)),
            "a new commit must replace the prior release edge"
        );
        assert!(ctx.finish_surface_destroy_use(buffer, false));
        assert_eq!(
            ctx.host_buffer_use(buffer),
            Some(RenderBufferUse::NeverSubmitted)
        );
        assert_eq!(
            ctx.render_buffers.lifecycle(HostId(host_buffer)),
            Some(RenderBufferLifecycle::GuestAlive(
                RenderBufferUse::NeverSubmitted
            ))
        );
    }

    #[test]
    fn same_buffer_reattach_does_not_create_a_detached_use() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let buffer = 20;
        let host_buffer = 40;
        ctx.shadow_table.map_id(buffer, host_buffer);
        assert!(ctx.register_native_buffer(host_buffer, (1, 1), Vec::new()));

        assert!(ctx.finalize_surface_attachment(None, Some(buffer)));
        assert!(ctx.finalize_surface_attachment(Some(buffer), Some(buffer)));
        assert_eq!(ctx.host_buffer_use(buffer), Some(awaiting_release(false)));
    }

    #[test]
    fn release_clears_the_detached_latch_before_a_new_submit() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let buffer = 20;
        let host_buffer = 40;
        ctx.shadow_table.map_id(buffer, host_buffer);
        assert!(ctx.register_native_buffer(host_buffer, (1, 1), Vec::new()));

        assert!(ctx.finalize_surface_attachment(None, Some(buffer)));
        assert!(ctx.finalize_surface_attachment(Some(buffer), None));
        assert_eq!(ctx.host_buffer_use(buffer), Some(awaiting_release(true)));
        assert!(ctx.mark_buffer_released(buffer));
        assert!(ctx.finalize_surface_attachment(None, Some(buffer)));
        assert_eq!(ctx.host_buffer_use(buffer), Some(awaiting_release(false)));
        assert!(ctx.finish_surface_destroy_use(buffer, false));
        assert_eq!(
            ctx.host_buffer_use(buffer),
            Some(RenderBufferUse::NeverSubmitted)
        );
    }

    #[test]
    fn failed_attachment_replacement_mutates_neither_buffer_generation() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let old = 20;
        let old_host = 40;
        let new = 21;
        let new_host = 41;
        for (guest, host) in [(old, old_host), (new, new_host)] {
            ctx.shadow_table.map_id(guest, host);
            assert!(ctx.register_native_buffer(host, (1, 1), Vec::new()));
        }
        assert!(ctx.mark_buffer_submitted(old));
        assert!(ctx
            .render_buffers
            .mark_host_destroy_queued(HostId(new_host)));

        assert!(!ctx.finalize_surface_attachment(Some(old), Some(new)));
        assert_eq!(ctx.host_buffer_use(old), Some(awaiting_release(false)));
        assert_eq!(
            ctx.render_buffers.lifecycle(HostId(new_host)),
            Some(RenderBufferLifecycle::HostDestroyQueued)
        );
    }

    #[test]
    fn host_buffer_use_matches_all_short_transition_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            Submit,
            Release,
            Detach,
            LastSurfaceDestroy,
            OtherSurfaceRemains,
        }

        let operations = [
            Operation::Submit,
            Operation::Release,
            Operation::Detach,
            Operation::LastSurfaceDestroy,
            Operation::OtherSurfaceRemains,
        ];
        let sequence_len = 6;
        let sequence_count = operations.len().pow(sequence_len);
        let buffer = 20;
        let host_buffer = 40;

        for mut encoded in 0..sequence_count {
            let mut ctx = Context::new_for_test(false, false, Vec::new());
            ctx.shadow_table.map_id(buffer, host_buffer);
            assert!(ctx.register_native_buffer(host_buffer, (1, 1), Vec::new()));
            let mut model = RenderBufferUse::NeverSubmitted;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                match operation {
                    Operation::Submit => {
                        if !matches!(&model, RenderBufferUse::AwaitingRelease { .. }) {
                            model = awaiting_release(false);
                        }
                        assert!(ctx.mark_buffer_submitted(buffer));
                    }
                    Operation::Release => {
                        let expected = matches!(&model, RenderBufferUse::AwaitingRelease { .. });
                        if expected {
                            model = RenderBufferUse::Released;
                        }
                        assert_eq!(ctx.mark_buffer_released(buffer), expected);
                    }
                    Operation::Detach => {
                        let expected = !matches!(&model, RenderBufferUse::NeverSubmitted);
                        if matches!(&model, RenderBufferUse::AwaitingRelease { .. }) {
                            model = awaiting_release(true);
                        }
                        assert_eq!(
                            ctx.finalize_surface_attachment(Some(buffer), None),
                            expected
                        );
                    }
                    Operation::LastSurfaceDestroy => {
                        let expected = !matches!(&model, RenderBufferUse::NeverSubmitted);
                        if model == awaiting_release(false) {
                            model = RenderBufferUse::NeverSubmitted;
                        }
                        assert_eq!(ctx.finish_surface_destroy_use(buffer, false), expected);
                    }
                    Operation::OtherSurfaceRemains => {
                        assert!(ctx.finish_surface_destroy_use(buffer, true));
                    }
                }
                assert_eq!(ctx.host_buffer_use(buffer), Some(model.clone()));
                assert_eq!(
                    ctx.render_buffers.lifecycle(HostId(host_buffer)),
                    Some(RenderBufferLifecycle::GuestAlive(model.clone()))
                );
            }
        }
    }

    #[test]
    fn render_buffer_registry_rejects_duplicate_host_generation_without_mutation() {
        let host_id = HostId(40);
        let mut registry = RenderBufferRegistry::default();

        assert!(registry.register_native(host_id, (16, 8), Vec::new()));
        assert!(!registry.register_native(host_id, (99, 99), Vec::new()));

        assert_eq!(
            registry.lifecycle(host_id),
            Some(RenderBufferLifecycle::GuestAlive(
                RenderBufferUse::NeverSubmitted
            ))
        );
        assert_eq!(registry.dimensions(host_id), Some((16, 8)));
    }

    #[test]
    fn render_buffer_lifecycle_matches_all_short_transition_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            Submit,
            Release,
            Detach,
            LastSurfaceDestroy,
            GuestDestroy,
            HostDestroy,
        }

        let operations = [
            Operation::Submit,
            Operation::Release,
            Operation::Detach,
            Operation::LastSurfaceDestroy,
            Operation::GuestDestroy,
            Operation::HostDestroy,
        ];
        let sequence_len = 6;
        let sequence_count = operations.len().pow(sequence_len);
        let host_id = 40;

        for mut encoded in 0..sequence_count {
            let mut registry = RenderBufferRegistry::default();
            assert!(registry.register_native(HostId(host_id), (1, 1), Vec::new()));
            let mut ownership = RenderBufferOwnership::GuestAlive;
            let mut use_state = RenderBufferUse::NeverSubmitted;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();

                let actual_changed = match operation {
                    Operation::Submit => registry.submit(HostId(host_id)),
                    Operation::Release => registry.release(HostId(host_id)),
                    Operation::Detach => registry.detach(HostId(host_id)),
                    Operation::LastSurfaceDestroy => {
                        registry.end_last_surface_use(HostId(host_id), false)
                    }
                    Operation::GuestDestroy => registry.mark_guest_destroyed(HostId(host_id)),
                    Operation::HostDestroy => registry.mark_host_destroy_queued(HostId(host_id)),
                };
                let expected_changed = match (ownership, operation) {
                    (RenderBufferOwnership::HostDestroyQueued, _) => false,
                    (_, Operation::Submit) => {
                        if !matches!(&use_state, RenderBufferUse::AwaitingRelease { .. }) {
                            use_state = awaiting_release(false);
                        }
                        true
                    }
                    (_, Operation::Release) => {
                        let awaiting =
                            matches!(&use_state, RenderBufferUse::AwaitingRelease { .. });
                        if awaiting {
                            use_state = RenderBufferUse::Released;
                        }
                        awaiting
                    }
                    (_, Operation::Detach) => match &use_state {
                        RenderBufferUse::AwaitingRelease { .. } => {
                            use_state = awaiting_release(true);
                            true
                        }
                        RenderBufferUse::Released => true,
                        RenderBufferUse::NeverSubmitted => false,
                    },
                    (_, Operation::LastSurfaceDestroy) => match &use_state {
                        RenderBufferUse::AwaitingRelease {
                            has_detached_use: false,
                        } => {
                            use_state = RenderBufferUse::NeverSubmitted;
                            true
                        }
                        RenderBufferUse::AwaitingRelease {
                            has_detached_use: true,
                        }
                        | RenderBufferUse::Released => true,
                        RenderBufferUse::NeverSubmitted => false,
                    },
                    (RenderBufferOwnership::GuestAlive, Operation::GuestDestroy) => {
                        ownership = RenderBufferOwnership::GuestDestroyed;
                        true
                    }
                    (RenderBufferOwnership::GuestDestroyed, Operation::GuestDestroy) => false,
                    (
                        RenderBufferOwnership::GuestAlive | RenderBufferOwnership::GuestDestroyed,
                        Operation::HostDestroy,
                    ) => {
                        ownership = RenderBufferOwnership::HostDestroyQueued;
                        true
                    }
                };
                let expected = match ownership {
                    RenderBufferOwnership::GuestAlive => {
                        RenderBufferLifecycle::GuestAlive(use_state.clone())
                    }
                    RenderBufferOwnership::GuestDestroyed => {
                        RenderBufferLifecycle::GuestDestroyed(use_state.clone())
                    }
                    RenderBufferOwnership::HostDestroyQueued => {
                        RenderBufferLifecycle::HostDestroyQueued
                    }
                };

                assert_eq!(actual_changed, expected_changed);
                assert_eq!(registry.lifecycle(HostId(host_id)), Some(expected));
                assert_eq!(
                    registry.dimensions(HostId(host_id)).is_some(),
                    ownership != RenderBufferOwnership::HostDestroyQueued
                );
            }
        }
    }

    #[test]
    fn keyboard_focus_registry_balances_replacement_and_delayed_leave() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };
        let mut registry = KeyboardFocusRegistry::default();

        assert_eq!(
            registry.enter(keyboard_a, focus_a),
            KeyboardFocusUpdate {
                accepted: true,
                seat_changes: vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: None,
                    current_surface: Some(20),
                }],
                retired_keyboards: Vec::new(),
            }
        );
        assert_eq!(
            registry.enter(keyboard_b, focus_b),
            KeyboardFocusUpdate {
                accepted: true,
                seat_changes: vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: Some(21),
                }],
                retired_keyboards: vec![keyboard_a],
            }
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
        assert_eq!(
            registry.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate {
                accepted: true,
                ..KeyboardFocusUpdate::default()
            },
            "a delayed leave must balance the retired guest enter once"
        );
        assert_eq!(
            registry.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a duplicate delayed leave must be rejected"
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
    }

    #[test]
    fn keyboard_focus_registry_rejects_old_leave_after_same_keyboard_reenter() {
        let keyboard = HostId(10);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };
        let mut registry = KeyboardFocusRegistry::default();

        registry.enter(keyboard, focus_a);
        registry.enter(keyboard, focus_b);
        assert_eq!(
            registry.leave(keyboard, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "an old leave on the same resource must not clear its newer enter"
        );
        assert_eq!(registry.focus_for_keyboard(keyboard), Some(focus_b));
    }

    #[test]
    fn keyboard_focus_registry_release_and_destroy_discard_retired_enters() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };

        let mut released = KeyboardFocusRegistry::default();
        released.enter(keyboard_a, focus_a);
        released.enter(keyboard_b, focus_b);
        released.release(keyboard_a);
        assert_eq!(
            released.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a released keyboard resource cannot receive a delayed leave"
        );

        let mut destroyed = KeyboardFocusRegistry::default();
        destroyed.enter(keyboard_a, focus_a);
        destroyed.enter(keyboard_b, focus_b);
        destroyed.destroy_surface(focus_a.guest_surface);
        assert_eq!(
            destroyed.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a destroyed guest surface cannot receive a delayed leave"
        );
    }

    #[test]
    fn keyboard_focus_registry_keeps_shared_surface_until_last_owner() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(keyboard_a, focus);
        registry.enter(keyboard_b, focus);

        let first_leave = registry.leave(keyboard_a, focus.host_surface);
        assert!(first_leave.accepted);
        assert!(first_leave.seat_changes.is_empty());
        assert_eq!(registry.surface_for_seat(1), Some(20));

        let last_leave = registry.leave(keyboard_b, focus.host_surface);
        assert_eq!(
            last_leave.seat_changes,
            vec![SeatFocusChange {
                guest_seat: 1,
                previous_surface: Some(20),
                current_surface: None,
            }]
        );
        assert_eq!(registry.surface_for_seat(1), None);
    }

    #[test]
    fn keyboard_focus_registry_destroys_surface_across_seats_without_fallback() {
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(
            HostId(10),
            KeyboardFocus {
                guest_seat: 1,
                guest_surface: 20,
                host_surface: 200,
            },
        );
        registry.enter(
            HostId(11),
            KeyboardFocus {
                guest_seat: 2,
                guest_surface: 20,
                host_surface: 200,
            },
        );
        registry.enter(
            HostId(12),
            KeyboardFocus {
                guest_seat: 3,
                guest_surface: 30,
                host_surface: 300,
            },
        );

        let update = registry.destroy_surface(20);
        assert_eq!(update.retired_keyboards, vec![HostId(10), HostId(11)]);
        assert_eq!(
            update.seat_changes,
            vec![
                SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: None,
                },
                SeatFocusChange {
                    guest_seat: 2,
                    previous_surface: Some(20),
                    current_surface: None,
                },
            ]
        );
        assert_eq!(registry.surface_for_seat(1), None);
        assert_eq!(registry.surface_for_seat(2), None);
        assert_eq!(registry.surface_for_seat(3), Some(30));
    }

    #[test]
    fn keyboard_focus_registry_preserves_invariants_for_all_short_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            EnterA20,
            EnterA21,
            EnterB20,
            EnterB21,
            LeaveA20,
            LeaveA21,
            LeaveB20,
            LeaveB21,
            ReleaseA,
            ReleaseB,
            Destroy20,
            Destroy21,
        }

        #[derive(Clone, Copy)]
        enum GuestDelivery {
            Enter(HostId, KeyboardFocus),
            Leave(HostId, u32),
            Release(HostId),
            Destroy(u32),
        }

        let operations = [
            Operation::EnterA20,
            Operation::EnterA21,
            Operation::EnterB20,
            Operation::EnterB21,
            Operation::LeaveA20,
            Operation::LeaveA21,
            Operation::LeaveB20,
            Operation::LeaveB21,
            Operation::ReleaseA,
            Operation::ReleaseB,
            Operation::Destroy20,
            Operation::Destroy21,
        ];
        let sequence_len = 5;
        let sequence_count = operations.len().pow(sequence_len);

        for mut encoded in 0..sequence_count {
            let mut registry = KeyboardFocusRegistry::default();
            // Model the focus currently visible to each guest wl_keyboard
            // resource. A newer enter on the same resource supersedes its
            // previous surface; active and retired registry generations must
            // account for this model exactly.
            let mut guest_focus = HashMap::new();
            for _ in 0..sequence_len {
                let (update, delivery) = match operations[encoded % operations.len()] {
                    Operation::EnterA20 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 20,
                            host_surface: 200,
                        };
                        (
                            registry.enter(HostId(10), focus),
                            GuestDelivery::Enter(HostId(10), focus),
                        )
                    }
                    Operation::EnterA21 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 21,
                            host_surface: 201,
                        };
                        (
                            registry.enter(HostId(10), focus),
                            GuestDelivery::Enter(HostId(10), focus),
                        )
                    }
                    Operation::EnterB20 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 20,
                            host_surface: 200,
                        };
                        (
                            registry.enter(HostId(11), focus),
                            GuestDelivery::Enter(HostId(11), focus),
                        )
                    }
                    Operation::EnterB21 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 21,
                            host_surface: 201,
                        };
                        (
                            registry.enter(HostId(11), focus),
                            GuestDelivery::Enter(HostId(11), focus),
                        )
                    }
                    Operation::LeaveA20 => (
                        registry.leave(HostId(10), 200),
                        GuestDelivery::Leave(HostId(10), 200),
                    ),
                    Operation::LeaveA21 => (
                        registry.leave(HostId(10), 201),
                        GuestDelivery::Leave(HostId(10), 201),
                    ),
                    Operation::LeaveB20 => (
                        registry.leave(HostId(11), 200),
                        GuestDelivery::Leave(HostId(11), 200),
                    ),
                    Operation::LeaveB21 => (
                        registry.leave(HostId(11), 201),
                        GuestDelivery::Leave(HostId(11), 201),
                    ),
                    Operation::ReleaseA => (
                        registry.release(HostId(10)),
                        GuestDelivery::Release(HostId(10)),
                    ),
                    Operation::ReleaseB => (
                        registry.release(HostId(11)),
                        GuestDelivery::Release(HostId(11)),
                    ),
                    Operation::Destroy20 => {
                        (registry.destroy_surface(20), GuestDelivery::Destroy(20))
                    }
                    Operation::Destroy21 => {
                        (registry.destroy_surface(21), GuestDelivery::Destroy(21))
                    }
                };
                encoded /= operations.len();

                match delivery {
                    GuestDelivery::Enter(keyboard, focus) => {
                        if update.accepted {
                            guest_focus.insert(keyboard, focus);
                        }
                    }
                    GuestDelivery::Leave(keyboard, host_surface) => {
                        if update.accepted {
                            assert_eq!(
                                guest_focus.get(&keyboard).map(|focus| focus.host_surface),
                                Some(host_surface),
                                "only a guest-visible enter can accept a leave"
                            );
                            guest_focus.remove(&keyboard);
                        }
                    }
                    GuestDelivery::Release(keyboard) => {
                        guest_focus.remove(&keyboard);
                    }
                    GuestDelivery::Destroy(surface) => {
                        guest_focus.retain(|_, focus| focus.guest_surface != surface);
                    }
                }

                for change in &update.seat_changes {
                    assert_ne!(change.previous_surface, change.current_surface);
                    assert_eq!(
                        registry.surface_for_seat(change.guest_seat),
                        change.current_surface
                    );
                }
                for left in registry.keyboards.values() {
                    for right in registry.keyboards.values() {
                        if left.guest_seat == right.guest_seat {
                            assert_eq!(
                                left.guest_surface, right.guest_surface,
                                "one seat must never retain competing surface generations"
                            );
                        }
                    }
                }
                for (&(keyboard, host_surface), retired) in &registry.retired_guest_enters {
                    assert_eq!(retired.host_surface, host_surface);
                    assert!(
                        !registry.keyboards.contains_key(&keyboard),
                        "one keyboard cannot have active and retired guest enters"
                    );
                    assert_eq!(
                        registry
                            .retired_guest_enters
                            .keys()
                            .filter(|(candidate, _)| *candidate == keyboard)
                            .count(),
                        1,
                        "one keyboard can have at most one deliverable retired leave"
                    );
                }
                assert_eq!(
                    guest_focus.len(),
                    registry.keyboards.len() + registry.retired_guest_enters.len(),
                    "every guest-visible focus must be active or await one retired leave"
                );
                for (&keyboard, &focus) in &guest_focus {
                    assert!(
                        registry.focus_for_keyboard(keyboard) == Some(focus)
                            || registry
                                .retired_guest_enters
                                .get(&(keyboard, focus.host_surface))
                                == Some(&focus),
                        "the registry must account for the guest resource's current focus"
                    );
                }
            }
        }
    }

    #[test]
    fn guest_key_owner_is_exclusive_and_source_checked() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let keyboard = HostId(10);
        let key = 57;

        assert!(ctx.claim_guest_key(keyboard, key, GuestKeyOwner::Physical));
        for competing_owner in [GuestKeyOwner::TextInputKeysym, GuestKeyOwner::ImeRecovery] {
            assert!(
                !ctx.claim_guest_key(keyboard, key, competing_owner),
                "a second source must not overwrite the live owner"
            );
        }
        assert_eq!(
            ctx.guest_key_owner(keyboard, key),
            Some(GuestKeyOwner::Physical)
        );
        assert!(!ctx.take_guest_key_if(keyboard, key, GuestKeyOwner::TextInputKeysym));
        assert_eq!(
            ctx.guest_key_owner(keyboard, key),
            Some(GuestKeyOwner::Physical),
            "a mismatched release must not steal another source's key"
        );
        assert!(ctx.take_guest_key_if(keyboard, key, GuestKeyOwner::Physical));
        assert!(ctx.guest_key_owner(keyboard, key).is_none());
    }

    #[test]
    fn guest_key_owner_matches_the_model_for_all_short_transition_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            Claim(GuestKeyOwner),
            Take(GuestKeyOwner),
        }

        let operations = [
            Operation::Claim(GuestKeyOwner::Physical),
            Operation::Claim(GuestKeyOwner::TextInputKeysym),
            Operation::Claim(GuestKeyOwner::ImeRecovery),
            Operation::Take(GuestKeyOwner::Physical),
            Operation::Take(GuestKeyOwner::TextInputKeysym),
            Operation::Take(GuestKeyOwner::ImeRecovery),
        ];
        let sequence_len = 5;
        let sequence_count = operations.len().pow(sequence_len);
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let keyboard = HostId(10);
        let key = 57;

        for mut encoded in 0..sequence_count {
            ctx.key_generations.clear_keyboard(keyboard);
            let mut model = None;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                match operation {
                    Operation::Claim(owner) => {
                        let expected = model.is_none();
                        assert_eq!(ctx.claim_guest_key(keyboard, key, owner), expected);
                        if expected {
                            model = Some(owner);
                        }
                    }
                    Operation::Take(owner) => {
                        let expected = model == Some(owner);
                        assert_eq!(ctx.take_guest_key_if(keyboard, key, owner), expected);
                        if expected {
                            model = None;
                        }
                    }
                }
                assert_eq!(ctx.guest_key_owner(keyboard, key), model);
            }
        }
    }

    #[test]
    fn guest_key_reducer_couples_delivery_ack_and_generation_ownership() {
        let keyboard = HostId(10);
        let physical_key = 30;
        let accelerator_key = 31;
        let keysym_key = 32;
        let mut registry = KeyGenerationRegistry::default();

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: false,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::TextInputPress { serial: 1 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: None,
                ends_repeat: false,
            },
            "a second protocol channel cannot duplicate the physical press"
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: true,
                    host_accelerator: false,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(keyboard, physical_key, GuestKeyEvent::PhysicalRelease,),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: true,
            }
        );

        assert_eq!(
            registry.transition_guest_key(keyboard, physical_key, GuestKeyEvent::RecoverIme,),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::EmitBalancedPair,
                ack_handled: None,
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: false,
                    ime_repeat_active: true,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            },
            "delayed physical delivery cannot duplicate a recovered pair"
        );

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                accelerator_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: true,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                accelerator_key,
                GuestKeyEvent::PhysicalRelease,
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: true,
            },
            "an accelerator release cannot escape without a guest press"
        );

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                keysym_key,
                GuestKeyEvent::TextInputPress { serial: 10 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: None,
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(keyboard, keysym_key, GuestKeyEvent::TextInputRepeat,)
                .delivery,
            GuestKeyDelivery::Forward
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                keysym_key,
                GuestKeyEvent::TextInputRelease { serial: 11 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: None,
                ends_repeat: true,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(
                    keyboard,
                    keysym_key,
                    GuestKeyEvent::TextInputRelease { serial: 12 },
                )
                .delivery,
            GuestKeyDelivery::Drop,
            "each synthetic press has exactly one releasable owner"
        );
    }

    #[test]
    fn guest_key_reducer_prevents_keysym_from_bypassing_accelerator_generation() {
        let keyboard = HostId(10);
        let key = 57;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, key, 10, 100, false);
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                key,
                GuestKeyEvent::TextInputPress { serial: 10 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: None,
                ends_repeat: false,
            },
            "an ineligible held peek generation is already known to be a host accelerator"
        );
        assert_eq!(registry.guest_owner(keyboard, key), None);

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: true,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(keyboard, key, GuestKeyEvent::TextInputPress { serial: 11 },)
                .delivery,
            GuestKeyDelivery::Drop,
            "the text-input channel must also honor explicit accelerator suppression"
        );
        registry.observe_peek_release(keyboard, key, 12);
        assert_eq!(
            registry.transition_guest_key(keyboard, key, GuestKeyEvent::PhysicalRelease),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: true,
            }
        );
        assert_eq!(registry.guest_owner(keyboard, key), None);
        assert_eq!(
            registry
                .transition_guest_key(keyboard, key, GuestKeyEvent::TextInputPress { serial: 13 },)
                .delivery,
            GuestKeyDelivery::Forward,
            "accelerator suppression must not leak into the next released generation"
        );
    }

    #[test]
    fn guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces() {
        #[derive(Clone, Copy)]
        enum Operation {
            PhysicalPress,
            PhysicalRepeat,
            PhysicalRelease,
            TextPress,
            TextRepeat,
            TextRelease,
            Recover,
        }

        let operations = [
            Operation::PhysicalPress,
            Operation::PhysicalRepeat,
            Operation::PhysicalRelease,
            Operation::TextPress,
            Operation::TextRepeat,
            Operation::TextRelease,
            Operation::Recover,
        ];
        let sequence_len = 6;
        let sequence_count = operations.len().pow(sequence_len);
        let keyboard = HostId(10);
        let key = 30;

        for mut encoded in 0..sequence_count {
            let mut registry = KeyGenerationRegistry::default();
            let mut guest_press_open = false;
            let mut serial = 0_u32;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                serial = serial.wrapping_add(1);
                let event = match operation {
                    Operation::PhysicalPress => GuestKeyEvent::PhysicalPress {
                        repeated: false,
                        host_accelerator: false,
                        ime_repeat_active: false,
                    },
                    Operation::PhysicalRepeat => GuestKeyEvent::PhysicalPress {
                        repeated: true,
                        host_accelerator: false,
                        ime_repeat_active: false,
                    },
                    Operation::PhysicalRelease => GuestKeyEvent::PhysicalRelease,
                    Operation::TextPress => GuestKeyEvent::TextInputPress { serial },
                    Operation::TextRepeat => GuestKeyEvent::TextInputRepeat,
                    Operation::TextRelease => GuestKeyEvent::TextInputRelease { serial },
                    Operation::Recover => GuestKeyEvent::RecoverIme,
                };
                let owner_before = registry.guest_owner(keyboard, key);
                let decision = registry.transition_guest_key(keyboard, key, event);

                assert_eq!(
                    decision.ack_handled,
                    match operation {
                        Operation::PhysicalPress => {
                            Some(owner_before != Some(GuestKeyOwner::ImeRecovery))
                        }
                        Operation::PhysicalRepeat | Operation::PhysicalRelease => Some(matches!(
                            owner_before,
                            Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                        )),
                        Operation::TextPress
                        | Operation::TextRepeat
                        | Operation::TextRelease
                        | Operation::Recover => None,
                    },
                    "ACK policy must remain coupled to the generation owner"
                );
                assert_eq!(
                    decision.ends_repeat,
                    match operation {
                        Operation::PhysicalRelease => true,
                        Operation::TextRelease => {
                            owner_before == Some(GuestKeyOwner::TextInputKeysym)
                        }
                        Operation::PhysicalPress
                        | Operation::PhysicalRepeat
                        | Operation::TextPress
                        | Operation::TextRepeat
                        | Operation::Recover => false,
                    },
                    "only the release that closes the current generation may end repeat"
                );
                match (operation, decision.delivery) {
                    (
                        Operation::PhysicalPress | Operation::TextPress,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(!guest_press_open, "a second press cannot be forwarded");
                        guest_press_open = true;
                    }
                    (
                        Operation::PhysicalRepeat | Operation::TextRepeat,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(
                            guest_press_open,
                            "repeat forwarding requires an open guest press"
                        );
                    }
                    (
                        Operation::PhysicalRelease | Operation::TextRelease,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(
                            guest_press_open,
                            "release forwarding requires an open guest press"
                        );
                        guest_press_open = false;
                    }
                    (_, GuestKeyDelivery::EmitBalancedPair) => {
                        assert!(
                            matches!(operation, Operation::Recover),
                            "only IME recovery may emit a balanced pair"
                        );
                        assert!(
                            !guest_press_open,
                            "recovery cannot overlap an owned guest press"
                        );
                    }
                    (_, GuestKeyDelivery::Drop) => {}
                    _ => panic!("delivery kind does not match its normalized input event"),
                }

                assert_eq!(
                    matches!(
                        registry.guest_owner(keyboard, key),
                        Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                    ),
                    guest_press_open,
                    "registry ownership must exactly match the observer's open pair"
                );
            }
        }
    }

    #[test]
    fn key_generation_registry_matches_the_model_for_all_short_transition_sequences() {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        struct ModelGeneration {
            id: u64,
            physical_state: PhysicalKeyState,
            peek: Option<PeekKeyProvenance>,
            peek_press_serial: Option<u32>,
            physical_release_serial: Option<u32>,
            backspace_repeat_cancelled: bool,
            guest_owner: Option<GuestKeyOwner>,
            guest_press_serial: Option<u32>,
            host_accelerator_suppressed: bool,
        }

        impl ModelGeneration {
            fn is_unreferenced(self) -> bool {
                self.physical_state != PhysicalKeyState::Held
                    && self.peek.is_none()
                    && !self.backspace_repeat_cancelled
                    && self.guest_owner.is_none()
                    && !self.host_accelerator_suppressed
            }
        }

        #[derive(Default)]
        struct Model {
            next_generation: u64,
            keys: HashMap<u32, ModelGeneration>,
            retired_guest_releases: HashMap<u32, Vec<RetiredGuestRelease>>,
        }

        impl Model {
            fn ensure(&mut self, key: u32) -> &mut ModelGeneration {
                self.keys.entry(key).or_insert_with(|| {
                    self.next_generation = self.next_generation.wrapping_add(1).max(1);
                    ModelGeneration {
                        id: self.next_generation,
                        ..ModelGeneration::default()
                    }
                })
            }

            fn prune(&mut self) {
                self.keys
                    .retain(|_, generation| !generation.is_unreferenced());
            }

            fn press(&mut self, key: u32, next_press_serial: Option<u32>) {
                self.retire_released(key, next_press_serial);
                self.ensure(key).physical_state = PhysicalKeyState::Held;
            }

            fn retire_released(&mut self, key: u32, next_press_serial: Option<u32>) -> bool {
                let Some(generation) =
                    self.keys.get(&key).copied().filter(|generation| {
                        generation.physical_state == PhysicalKeyState::Released
                    })
                else {
                    return false;
                };
                let retired_release = match generation.guest_owner {
                    Some(GuestKeyOwner::Physical) => {
                        generation.physical_release_serial.map(|release_serial| {
                            RetiredGuestRelease {
                                owner: GuestKeyOwner::Physical,
                                press_serial: generation.guest_press_serial,
                                release_serial: Some(release_serial),
                                next_press_serial,
                            }
                        })
                    }
                    Some(GuestKeyOwner::TextInputKeysym) => {
                        generation
                            .guest_press_serial
                            .map(|press_serial| RetiredGuestRelease {
                                owner: GuestKeyOwner::TextInputKeysym,
                                press_serial: Some(press_serial),
                                release_serial: None,
                                next_press_serial,
                            })
                    }
                    Some(GuestKeyOwner::ImeRecovery) | None => None,
                };
                if let Some(retired_release) = retired_release {
                    self.retired_guest_releases
                        .entry(key)
                        .or_default()
                        .push(retired_release);
                }
                self.keys.remove(&key);
                true
            }

            fn repeat(&mut self, key: u32) {
                if let Some(generation) = self
                    .keys
                    .get_mut(&key)
                    .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    generation.physical_state = PhysicalKeyState::Held;
                }
            }

            fn release(&mut self, key: u32, serial: Option<u32>) {
                if let Some(generation) = self.keys.get_mut(&key) {
                    generation.physical_state = PhysicalKeyState::Released;
                    if let Some(serial) = serial {
                        generation.physical_release_serial = Some(serial);
                    }
                    generation.backspace_repeat_cancelled = false;
                }
                if !self
                    .keys
                    .values()
                    .any(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    for generation in self.keys.values_mut() {
                        generation.peek = None;
                    }
                }
                self.prune();
            }

            fn peek_press(&mut self, key: u32, serial: u32, time: u32, eligible: bool) {
                if !self
                    .keys
                    .get(&key)
                    .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    self.press(key, Some(serial));
                }
                let generation = self.ensure(key);
                generation.peek_press_serial = Some(serial);
                generation.peek = Some(PeekKeyProvenance {
                    serial,
                    time,
                    sequence: generation.id,
                    eligible,
                });
            }

            fn peek_release(&mut self, key: u32, serial: u32) {
                self.release(key, Some(serial));
            }
        }

        #[derive(Clone, Copy)]
        enum Operation {
            Press(u32),
            Repeat(u32),
            Release(u32),
            PeekPress(u32, bool),
            PeekRelease(u32),
            RefreshPeek(u32),
            InvalidatePeek(u32),
            CancelRepeat(u32),
            SuppressAccelerator(u32),
            TakeAcceleratorSuppression(u32),
            ClaimOwner(u32, GuestKeyOwner),
            ClaimTextInput(u32),
            CompleteTextInput(u32),
            TakePendingPhysicalRelease(u32),
            TakePendingTextInputRelease(u32),
            TakeOwner(u32),
            Clear,
        }

        const KEY_A: u32 = 30;
        const KEY_B: u32 = 57;
        let keyboard = HostId(10);
        let operations = [
            Operation::Press(KEY_A),
            Operation::Press(KEY_B),
            Operation::Repeat(KEY_A),
            Operation::Release(KEY_A),
            Operation::Release(KEY_B),
            Operation::PeekPress(KEY_A, true),
            Operation::PeekPress(KEY_B, false),
            Operation::PeekRelease(KEY_A),
            Operation::RefreshPeek(KEY_A),
            Operation::InvalidatePeek(KEY_B),
            Operation::CancelRepeat(KEY_A),
            Operation::SuppressAccelerator(KEY_B),
            Operation::TakeAcceleratorSuppression(KEY_B),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::Physical),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::TextInputKeysym),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::ImeRecovery),
            Operation::ClaimTextInput(KEY_A),
            Operation::CompleteTextInput(KEY_A),
            Operation::TakePendingPhysicalRelease(KEY_A),
            Operation::TakePendingTextInputRelease(KEY_A),
            Operation::TakeOwner(KEY_A),
            Operation::Clear,
        ];
        let sequence_len = 4;
        let sequence_count = operations.len().pow(sequence_len);

        for mut encoded in 0..sequence_count {
            let mut registry = KeyGenerationRegistry::default();
            let mut model = Model::default();

            for step in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                let serial = step + 1;
                let time = step + 101;

                match operation {
                    Operation::Press(key) => {
                        registry.observe_physical_state(keyboard, key, 1);
                        model.press(key, None);
                    }
                    Operation::Repeat(key) => {
                        registry.observe_physical_state(keyboard, key, 2);
                        model.repeat(key);
                    }
                    Operation::Release(key) => {
                        registry.observe_physical_state(keyboard, key, 0);
                        model.release(key, None);
                    }
                    Operation::PeekPress(key, eligible) => {
                        registry.observe_peek_press(keyboard, key, serial, time, eligible);
                        model.peek_press(key, serial, time, eligible);
                    }
                    Operation::PeekRelease(key) => {
                        registry.observe_peek_release(keyboard, key, serial);
                        model.peek_release(key, serial);
                    }
                    Operation::RefreshPeek(key) => {
                        registry.refresh_peek(keyboard, key, serial, time);
                        if let Some(peek) = model.keys.get_mut(&key) {
                            peek.peek_press_serial = Some(serial);
                            if let Some(provenance) = peek.peek.as_mut() {
                                provenance.serial = serial;
                                provenance.time = time;
                            }
                        }
                    }
                    Operation::InvalidatePeek(key) => {
                        registry.invalidate_peek(keyboard, key);
                        if let Some(peek) = model
                            .keys
                            .get_mut(&key)
                            .and_then(|generation| generation.peek.as_mut())
                        {
                            peek.eligible = false;
                        }
                    }
                    Operation::CancelRepeat(key) => {
                        registry.cancel_backspace_repeat(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key).filter(|generation| {
                            generation.physical_state == PhysicalKeyState::Held
                        }) {
                            generation.backspace_repeat_cancelled = true;
                        }
                    }
                    Operation::SuppressAccelerator(key) => {
                        registry.suppress_host_accelerator(keyboard, key);
                        model.ensure(key).host_accelerator_suppressed = true;
                    }
                    Operation::TakeAcceleratorSuppression(key) => {
                        registry.take_host_accelerator_suppression(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key) {
                            generation.host_accelerator_suppressed = false;
                        }
                        model.prune();
                    }
                    Operation::ClaimOwner(key, owner) => {
                        let actual = registry.claim_guest_owner(keyboard, key, owner);
                        let generation = model.ensure(key);
                        let expected = generation.guest_owner.is_none();
                        if expected {
                            generation.guest_owner = Some(owner);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::ClaimTextInput(key) => {
                        let actual = registry.claim_text_input_owner(keyboard, key, serial);
                        let (starts_new, released) =
                            model.keys.get(&key).map_or((false, false), |generation| {
                                let newer_than_guest_press =
                                    generation.guest_press_serial.is_none_or(|press_serial| {
                                        serial_is_after(serial, press_serial)
                                    });
                                let released =
                                    generation.physical_state == PhysicalKeyState::Released;
                                let after_release_boundary =
                                    generation.physical_release_serial.is_some_and(
                                        |release_serial| serial_is_after(serial, release_serial),
                                    );
                                let owner_can_start_next = match generation.guest_owner {
                                    Some(GuestKeyOwner::TextInputKeysym) => released,
                                    Some(GuestKeyOwner::ImeRecovery) => {
                                        generation.physical_state == PhysicalKeyState::Unseen
                                            || released
                                    }
                                    Some(GuestKeyOwner::Physical) | None => false,
                                };
                                (
                                    owner_can_start_next
                                        && newer_than_guest_press
                                        && (!released || after_release_boundary),
                                    released,
                                )
                            });
                        if starts_new {
                            if released {
                                assert!(model.retire_released(key, Some(serial)));
                            } else {
                                model.keys.remove(&key);
                            }
                        }
                        let generation = model.ensure(key);
                        let expected = generation.guest_owner.is_none();
                        if expected {
                            generation.guest_owner = Some(GuestKeyOwner::TextInputKeysym);
                            generation.guest_press_serial = Some(serial);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakePendingTextInputRelease(key) => {
                        let actual =
                            registry.take_pending_text_input_release(keyboard, key, serial);
                        let current_press_serial = model.keys.get(&key).and_then(|generation| {
                            generation
                                .guest_press_serial
                                .or(generation.peek_press_serial)
                        });
                        let releases = model.retired_guest_releases.entry(key).or_default();
                        let pending = releases.iter().position(|release| {
                            release.owner == GuestKeyOwner::TextInputKeysym
                                && release.press_serial.is_some_and(|press_serial| {
                                    serial_is_after(serial, press_serial)
                                })
                                && release
                                    .next_press_serial
                                    .or(current_press_serial)
                                    .is_none_or(|current_serial| {
                                        !serial_is_after(serial, current_serial)
                                    })
                        });
                        let expected = pending.is_some();
                        if let Some(index) = pending {
                            releases.remove(index);
                        }
                        if releases.is_empty() {
                            model.retired_guest_releases.remove(&key);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakePendingPhysicalRelease(key) => {
                        let release_serial = serial.wrapping_sub(2);
                        let actual =
                            registry.take_pending_physical_release(keyboard, key, release_serial);
                        let releases = model.retired_guest_releases.entry(key).or_default();
                        let pending = releases.iter().position(|release| {
                            release.owner == GuestKeyOwner::Physical
                                && release.release_serial == Some(release_serial)
                        });
                        let expected = pending.is_some();
                        if let Some(index) = pending {
                            releases.remove(index);
                        }
                        if releases.is_empty() {
                            model.retired_guest_releases.remove(&key);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::CompleteTextInput(key) => {
                        let actual = registry.complete_guest_owner_if(
                            keyboard,
                            key,
                            GuestKeyOwner::TextInputKeysym,
                        );
                        let expected = model.keys.get(&key).is_some_and(|generation| {
                            generation.guest_owner == Some(GuestKeyOwner::TextInputKeysym)
                        });
                        if expected {
                            model.ensure(key).guest_owner = Some(GuestKeyOwner::ImeRecovery);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakeOwner(key) => {
                        registry.take_guest_owner(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key) {
                            generation.guest_owner = None;
                            generation.guest_press_serial = None;
                        }
                        model.prune();
                    }
                    Operation::Clear => {
                        registry.clear_keyboard(keyboard);
                        model.keys.clear();
                        model.retired_guest_releases.clear();
                    }
                }

                assert_eq!(registry.next_generation, model.next_generation);
                let actual = registry.entries.get(&keyboard);
                assert_eq!(
                    actual.map(HashMap::len).unwrap_or_default(),
                    model.keys.len()
                );
                for (&key, expected) in &model.keys {
                    let actual = &actual.expect("modeled keyboard entry")[&key];
                    assert_eq!(actual.id, expected.id);
                    assert_eq!(actual.physical_state, expected.physical_state);
                    assert_eq!(actual.peek, expected.peek);
                    assert_eq!(actual.peek_press_serial, expected.peek_press_serial);
                    assert_eq!(
                        actual.physical_release_serial,
                        expected.physical_release_serial
                    );
                    assert_eq!(
                        actual.backspace_repeat_cancelled,
                        expected.backspace_repeat_cancelled
                    );
                    assert_eq!(actual.guest_owner, expected.guest_owner);
                    assert_eq!(actual.guest_press_serial, expected.guest_press_serial);
                    assert_eq!(
                        actual.host_accelerator_suppressed,
                        expected.host_accelerator_suppressed
                    );
                }
                assert_eq!(
                    registry
                        .retired_guest_releases
                        .iter()
                        .filter(|((pending_keyboard, _), _)| *pending_keyboard == keyboard)
                        .map(|((_, key), releases)| (*key, releases.clone()))
                        .collect::<HashMap<_, _>>(),
                    model.retired_guest_releases
                );
                assert!(
                    registry
                        .entries
                        .values()
                        .all(|keys| !keys.is_empty()
                            && keys.values().all(|key| !key.is_unreferenced())),
                    "the registry must not retain empty generation tombstones"
                );
            }
        }
    }

    #[test]
    fn repressed_key_replaces_its_released_peek_generation() {
        let keyboard = HostId(10);
        let key_a = 30;
        let key_b = 57;
        let mut registry = KeyGenerationRegistry::default();

        let first_a = registry.observe_peek_press(keyboard, key_a, 1, 10, false);
        let first_b = registry.observe_peek_press(keyboard, key_b, 2, 20, true);
        registry.observe_physical_state(keyboard, key_a, 0);
        assert_eq!(
            registry.peek(keyboard, key_a).map(|peek| peek.sequence),
            Some(first_a),
            "another held key retains the released generation as a causal tombstone"
        );

        let second_a = registry.observe_peek_press(keyboard, key_a, 3, 30, true);
        let peek = registry.peek(keyboard, key_a).unwrap();
        assert!(second_a > first_b);
        assert_ne!(second_a, first_a);
        assert_eq!(peek.sequence, second_a);
        assert!(peek.eligible);
    }

    #[test]
    fn delayed_keyboard_press_preserves_keysym_release_owner() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        assert!(registry.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym));
        registry.observe_physical_state(keyboard, key, 1);

        assert!(registry.physically_held(keyboard, key));
        assert_eq!(
            registry.guest_owner(keyboard, key),
            Some(GuestKeyOwner::TextInputKeysym),
            "the duplicate keyboard channel must not orphan the synthetic press"
        );
    }

    #[test]
    fn delayed_peek_press_preserves_keysym_release_owner() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        assert!(registry.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym));
        registry.observe_peek_press(keyboard, key, 1, 10, true);

        assert!(registry.physically_held(keyboard, key));
        assert_eq!(
            registry.guest_owner(keyboard, key),
            Some(GuestKeyOwner::TextInputKeysym),
            "a delayed peek must not orphan the synthetic press"
        );
    }

    #[test]
    fn retired_releases_match_generation_intervals_and_serial_wrap() {
        let keyboard = HostId(10);
        let text_key = 30;
        let physical_key = 57;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, text_key, 10, 100, true);
        assert!(registry.claim_text_input_owner(keyboard, text_key, 10));
        registry.observe_peek_release(keyboard, text_key, 11);
        registry.observe_peek_press(keyboard, text_key, 20, 200, true);
        assert!(registry.claim_text_input_owner(keyboard, text_key, 20));
        registry.observe_peek_release(keyboard, text_key, 21);
        registry.observe_peek_press(keyboard, text_key, 30, 300, true);

        assert!(
            registry.take_pending_text_input_release(keyboard, text_key, 21),
            "a release must match the retired interval immediately before it"
        );
        assert!(
            registry.take_pending_text_input_release(keyboard, text_key, 11),
            "an older delayed release must remain available after a newer interval closes"
        );
        assert!(
            !registry.take_pending_text_input_release(keyboard, text_key, 21),
            "each retired press must be released exactly once"
        );

        registry.observe_peek_press(keyboard, physical_key, u32::MAX - 1, 400, true);
        assert!(registry.claim_guest_owner(keyboard, physical_key, GuestKeyOwner::Physical));
        registry.observe_peek_release(keyboard, physical_key, u32::MAX);
        registry.observe_peek_press(keyboard, physical_key, 0, 500, true);
        assert!(
            registry.take_pending_physical_release(keyboard, physical_key, u32::MAX),
            "a physical release must survive the next generation across serial wrap"
        );
        assert!(registry.physically_held(keyboard, physical_key));
    }

    #[test]
    fn released_generation_rejects_delayed_press_before_release_boundary() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, key, u32::MAX - 3, 100, true);
        assert!(registry.claim_text_input_owner(keyboard, key, u32::MAX - 2));
        registry.observe_peek_release(keyboard, key, 0);

        assert!(
            !registry.claim_text_input_owner(keyboard, key, u32::MAX - 1),
            "a delayed press from before the physical release must remain in the old generation"
        );
        assert_eq!(
            registry.guest_press_serial(keyboard, key),
            Some(u32::MAX - 2)
        );
        assert!(
            registry.claim_text_input_owner(keyboard, key, 1),
            "a press after the wrapped release boundary must open the next generation"
        );
        assert_eq!(registry.guest_press_serial(keyboard, key), Some(1));

        let raw_key = 57;
        assert!(registry.claim_text_input_owner(keyboard, raw_key, 10));
        registry.observe_physical_event(keyboard, raw_key, 0, Some(20));
        assert!(
            !registry.claim_text_input_owner(keyboard, raw_key, 15),
            "the regular wl_keyboard release must also bound delayed text-input presses"
        );
        assert!(registry.claim_text_input_owner(keyboard, raw_key, 21));
    }

    #[test]
    fn clearing_guest_keys_is_scoped_to_one_keyboard() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        assert!(ctx.claim_guest_key(HostId(10), 57, GuestKeyOwner::ImeRecovery));
        assert!(ctx.claim_guest_key(HostId(11), 57, GuestKeyOwner::TextInputKeysym));
        for keyboard in [HostId(10), HostId(11)] {
            assert!(ctx.key_generations.claim_text_input_owner(keyboard, 30, 1));
            ctx.key_generations.observe_physical_state(keyboard, 30, 0);
            ctx.key_generations.observe_physical_state(keyboard, 30, 1);
        }

        ctx.key_generations.clear_keyboard(HostId(10));

        assert!(ctx.guest_key_owner(HostId(10), 57).is_none());
        assert_eq!(
            ctx.guest_key_owner(HostId(11), 57),
            Some(GuestKeyOwner::TextInputKeysym)
        );
        assert!(
            !ctx.key_generations
                .take_pending_text_input_release(HostId(10), 30, 2),
            "clearing a keyboard must discard its retired releases"
        );
        assert!(
            ctx.key_generations
                .take_pending_text_input_release(HostId(11), 30, 2),
            "clearing one keyboard must preserve another keyboard's retired releases"
        );
    }

    fn errno_reset() {
        unsafe {
            *libc::__errno_location() = 0;
        }
    }

    fn errno_value() -> i32 {
        unsafe { *libc::__errno_location() }
    }
}
