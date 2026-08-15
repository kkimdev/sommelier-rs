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
    /// Guest object ID that owns this state while the buffer is live. Retired
    /// buffers keep this value after the guest object has been destroyed so
    /// surface references can still be resolved for damage-only commits.
    pub guest_buffer_id: u32,
    pub pool: Arc<PoolState>,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: u32,
    pub format: u32,
    #[allow(dead_code)]
    pub host_buffer_id: u32,
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
    /// The host compositor has sent wl_buffer.release while this guest
    /// object is still alive. A later guest destroy can drop local backing
    /// storage immediately when this is set.
    pub host_released: bool,
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
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

#[derive(Clone)]
pub struct SurfaceState {
    /// Buffer selected by the most recent committed attach. A commit that
    /// changes only damage still uses this buffer.
    pub current_buffer_id: Option<u32>,
    /// `Some(None)` represents an explicit `attach(NULL)`, while `None`
    /// means that this commit has no attach request at all.
    pub pending_buffer_id: Option<Option<u32>>,
    /// Damage expressed in surface-local coordinates. It can only be copied
    /// directly when the current buffer has the default transform and no
    /// viewport; otherwise the compositor falls back to a complete copy.
    pub pending_surface_damage: Vec<DamageRect>,
    /// Damage expressed in buffer pixel coordinates.
    pub pending_buffer_damage: Vec<DamageRect>,
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
    pub current_offset: (i32, i32),
    /// A viewport object exists for this surface. The object's state is
    /// tracked separately because an unset viewport is an identity mapping.
    pub viewport: Option<ViewportState>,
    /// Pending viewport state applied by the next surface commit. `Some(None)`
    /// represents destruction of the viewport object; `None` means unchanged.
    pub pending_viewport: Option<Option<ViewportState>>,
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            current_buffer_id: None,
            pending_buffer_id: None,
            pending_surface_damage: Vec::new(),
            pending_buffer_damage: Vec::new(),
            pending_buffer_scale: None,
            current_buffer_scale: 1,
            pending_buffer_transform: None,
            current_buffer_transform: 0,
            pending_offset: None,
            pending_attach_offset: None,
            current_offset: (0, 0),
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

pub struct Context {
    pub shadow_table: ShadowTable,
    pub pools: HashMap<u32, Arc<PoolState>>,
    pub buffers: HashMap<u32, BufferState>,
    /// Buffers whose guest wl_buffer object was destroyed while a surface
    /// could still reference them. They remain mapped until the host releases
    /// the buffer, at which point the host object and backing storage can be
    /// retired safely.
    pub retired_buffers: HashMap<u32, BufferState>,
    /// Guest buffer IDs that have been sent to the host in a committed
    /// wl_surface state. A destroyed buffer with this marker retains its
    /// local SHM backing until the surface switches away from it.
    pub submitted_buffers: HashSet<u32>,
    /// Native linux-dmabuf buffers have no local SHM mapping, but their host
    /// wl_buffer still must remain alive after the guest object is destroyed
    /// until the compositor sends wl_buffer.release.
    pub deferred_host_buffers: HashMap<u32, u32>,
    /// Host release markers for native linux-dmabuf buffers that have no
    /// BufferState entry. A released buffer may be destroyed even while a
    /// surface still retains it as its current content.
    pub released_host_buffers: HashSet<u32>,
    /// Dimensions of guest-created linux-dmabuf buffers. Native buffers do
    /// not need a local SHM `BufferState`, but the compositor bridge still
    /// needs their dimensions to translate `damage_buffer` and full damage
    /// rectangles correctly.
    pub native_buffer_sizes: HashMap<u32, (i32, i32)>,
    /// Dimensions waiting for the host's asynchronous linux-dmabuf `created`
    /// event. The key is the guest params object ID; the event handler moves
    /// the value to `native_buffer_sizes` under the host-created buffer ID.
    pub pending_native_buffer_sizes: HashMap<u32, (i32, i32)>,
    /// Async linux-dmabuf params objects whose guest destructor was sent
    /// before the host emitted `created`/`failed`. The key is the host params
    /// ID, which may outlive the guest mapping when the host acknowledges the
    /// params destructor first.
    pub orphaned_dmabuf_params: HashMap<u32, u32>,
    pub surfaces: HashMap<u32, SurfaceState>,
    pub text_inputs: HashMap<u32, TextInputState>,
    pub keyboard_to_seat: HashMap<u32, u32>,
    pub active_surface_for_seat: HashMap<u32, u32>,
    /// Current guest surface entered by each host keyboard. A seat can expose
    /// multiple wl_keyboard objects; a leave from one object must not clear
    /// the seat focus while another object is still entered on the same
    /// surface.
    pub keyboard_active_surfaces: HashMap<HostId, u32>,
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
    /// Physical keys currently held for each host keyboard. Both normal
    /// `wl_keyboard.key` and ChromeOS `peek_key` events update this map, so
    /// IME-consumed keys remain observable without leaking state across seats.
    pub keyboard_pressed_keys: HashMap<HostId, HashSet<u32>>,
    /// Host keyboards whose held Backspace repeat was cancelled by a newer
    /// non-Backspace press. Physical key state remains intact until release,
    /// while empty IME confirmations must not rearm the cancelled repeat.
    pub keyboard_backspace_repeat_cancelled: HashSet<HostId>,
    /// Most recent compositor-relative event time for each host keyboard.
    /// Synthetic compatibility events must use this same time domain.
    pub keyboard_event_times: HashMap<HostId, u32>,
    /// Exact compositor serial/time of the held Backspace generation observed
    /// through ChromeOS `peek_key`.
    pub keyboard_backspace_events: HashMap<HostId, (u32, u32)>,
    /// Backspace key releases that must be consumed because a synthetic press
    /// and release pair was already delivered to the guest.
    pub keyboard_ime_suppressed_keys: HashMap<HostId, HashSet<u32>>,
    /// Keys whose physical press was forwarded to the guest and therefore
    /// still require a real release event.
    pub keyboard_forwarded_keys: HashMap<HostId, HashSet<u32>>,
    /// Keys that were synthesized from a text-input-v1 `keysym` event.
    ///
    /// `keyboard_forwarded_keys` also contains ordinary physical presses, so
    /// it cannot by itself tell whether a later keysym release belongs to a
    /// synthetic press or is a duplicate of a real keyboard event. Keeping
    /// this source marker prevents either path from stealing the other's
    /// release.
    pub keyboard_keysym_forwarded_keys: HashMap<HostId, HashSet<u32>>,
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
    /// One retained duplicate of a native dma-buf plane, keyed by the host
    /// `wl_buffer` ID. It is used to wait for guest GPU writes immediately
    /// before each host surface commit and is dropped after host delete_id.
    pub native_buffer_sync_fds: HashMap<u32, OwnedFd>,
    /// Native dma-buf sync descriptors waiting for an asynchronous
    /// linux-dmabuf `created` event, keyed by guest params ID.
    pub pending_native_sync_fds: HashMap<u32, OwnedFd>,
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

impl Context {
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
            buffers: HashMap::new(),
            retired_buffers: HashMap::new(),
            submitted_buffers: HashSet::new(),
            deferred_host_buffers: HashMap::new(),
            released_host_buffers: HashSet::new(),
            native_buffer_sizes: HashMap::new(),
            pending_native_buffer_sizes: HashMap::new(),
            orphaned_dmabuf_params: HashMap::new(),
            surfaces: HashMap::new(),
            text_inputs: HashMap::new(),
            keyboard_to_seat: HashMap::new(),
            active_surface_for_seat: HashMap::new(),
            keyboard_active_surfaces: HashMap::new(),
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
            keyboard_pressed_keys: HashMap::new(),
            keyboard_backspace_repeat_cancelled: HashSet::new(),
            keyboard_event_times: HashMap::new(),
            keyboard_backspace_events: HashMap::new(),
            keyboard_ime_suppressed_keys: HashMap::new(),
            keyboard_forwarded_keys: HashMap::new(),
            keyboard_keysym_forwarded_keys: HashMap::new(),
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
            native_buffer_sync_fds: HashMap::new(),
            pending_native_sync_fds: HashMap::new(),
            feedback_index_maps: HashMap::new(),
            synthetic_feedback_objects: HashMap::new(),
            synthetic_feedback_refresh_pending: HashSet::new(),
            dmabuf_capabilities: HashMap::new(),
            host_dmabuf_generation: None,
            dmabuf_capability_callbacks: HashMap::new(),
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

    fn errno_reset() {
        unsafe {
            *libc::__errno_location() = 0;
        }
    }

    fn errno_value() -> i32 {
        unsafe { *libc::__errno_location() }
    }
}
