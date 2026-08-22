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

mod input;
mod render;
mod window_placement;

use std::collections::{HashMap, HashSet};
use std::os::unix::io::{OwnedFd, RawFd};
use std::sync::Arc;
#[cfg(test)]
use std::sync::RwLock;

use self::render::RenderBufferRegistry;
#[cfg(test)]
use self::render::{DamageRegion, MAX_PENDING_DAMAGE_RECTS};
use crate::allocator::Allocator;
use crate::virtwl_channel::VirtWaylandChannel;
#[cfg(test)]
use crate::window_shortcuts::ShortcutConfig;
#[cfg(test)]
use crate::window_shortcuts::ShortcutConfigHandle;
use log::warn;

#[allow(unused_imports)]
pub(crate) use self::input::{
    ConfirmPreeditPlan, GuestCommitPlan, GuestKeyDecision, GuestKeyDelivery, GuestKeyEvent,
    GuestKeyOwner, HostActivationState, HostCommitPlan, HostPreeditPlan, KeyGenerationRegistry,
    KeyboardFocus, KeyboardFocusRegistry, KeyboardFocusUpdate, PeekKeyProvenance,
    PreeditRegionPlan, SeatFocusChange, TextInputActivationBarrierRegistry, TextInputState,
};
pub(crate) use self::render::{
    BufferState, DamageRect, PoolInner, PoolState, RenderBufferLifecycle, RenderBufferUse,
    SurfaceAttachment, SurfaceCommit, SurfaceState, ViewportState,
};
#[cfg(test)]
pub(crate) use self::window_placement::ShortcutReloadResult;
#[cfg(test)]
pub(crate) use self::window_placement::{
    OutputState, ARC_TASK_APPLICATION_ID_PREFIX, ARC_TASK_ID_POOL_END, ARC_TASK_ID_POOL_START,
};
pub(crate) use self::window_placement::{
    WindowArcIdLifetime, WindowGeometryMethod, WindowHostPolicy, WindowPlacementGeometry,
    WindowPlacementMode, WindowPlacementPlanError, WindowPlacementRuntime,
    WindowPlacementRuntimeHandle, WindowPlacementState,
};

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
    /// Single source of truth for placement backend selection and all
    /// compositor-owned placement state.
    pub(crate) window_placement: WindowPlacementState,
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
        let resolve = |guest_buffer| match guest_buffer {
            Some(guest_id) => self.render_buffer_host_id(guest_id).map(Some),
            None => Some(None),
        };
        let Some(previous) = resolve(previous_guest_buffer) else {
            return false;
        };
        let Some(next) = resolve(next_guest_buffer) else {
            return false;
        };
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
        Self::new_with_placement_runtime(
            gpu_accel,
            xdg_decoration,
            WindowPlacementRuntime::from_environment(),
        )
    }

    pub(crate) fn new_with_placement_runtime(
        gpu_accel: bool,
        xdg_decoration: bool,
        placement_runtime: WindowPlacementRuntimeHandle,
    ) -> Self {
        // Initialize allocator
        let allocator = match Allocator::new() {
            Ok(alloc) => Some(alloc),
            Err(e) => {
                warn!("Failed to initialize GBM allocator: {}", e);
                None
            }
        };

        let window_placement = WindowPlacementState::with_runtime(placement_runtime);

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
            window_placement,
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

    /// Test-only constructor that injects a deterministic host accelerator
    /// policy and nine-grid placement configuration.
    #[cfg(test)]
    pub fn new_for_test(
        gpu_accel: bool,
        xdg_decoration: bool,
        accelerators: Vec<crate::accelerator::Accelerator>,
    ) -> Self {
        let runtime = WindowPlacementRuntime::new(
            WindowPlacementMode::disabled(),
            ShortcutConfigHandle::new(ShortcutConfig::test_nine_grid()),
            None,
            Arc::new(accelerators),
            None,
        );
        Self::new_with_placement_runtime(gpu_accel, xdg_decoration, runtime)
    }
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
    fn output_work_area_converts_scale_and_insets() {
        let output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            scale: 2,
            insets_top: 24,
            insets_left: 8,
            insets_bottom: 48,
            insets_right: 16,
        };
        assert_eq!(output.work_area(), Some((8, 24, 1896, 1008)));
    }

    #[test]
    fn output_work_area_rejects_invalid_dimensions() {
        let output = OutputState {
            mode_width: 100,
            mode_height: 100,
            scale: 2,
            insets_top: 60,
            ..Default::default()
        };
        assert_eq!(output.work_area(), None);
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
            pending_attachment: SurfaceAttachment::Attach(2),
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

        assert_eq!(commit.attachment, SurfaceAttachment::Attach(2));
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
        assert_eq!(surface.pending_attachment, SurfaceAttachment::Unchanged);
        assert!(surface.pending_surface_damage.is_empty());
        assert!(surface.pending_buffer_damage.is_empty());
        assert!(surface.pending_buffer_scale.is_none());
        assert!(surface.pending_buffer_transform.is_none());
        assert!(surface.pending_offset.is_none());
        assert!(surface.pending_attach_offset.is_none());
        assert!(surface.pending_viewport.is_none());

        let next = surface.prepare_commit();
        assert_eq!(next.attachment, SurfaceAttachment::Unchanged);
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
            pending_attachment: SurfaceAttachment::Detach,
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
            pending_attachment: SurfaceAttachment::Detach,
            ..SurfaceState::default()
        };
        surface.set_current_buffer_for_test(Some(1), Some((100, 50)));

        let commit = surface.prepare_commit();

        assert_eq!(commit.attachment, SurfaceAttachment::Detach);
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
    fn surface_attachment_requires_both_exact_render_generations() {
        let previous_guest = 20;
        let previous_host = 40;
        let next_guest = 21;
        let next_host = 41;
        let unregistered_guest = 22;
        let unregistered_host = 42;
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        for (guest, host) in [
            (previous_guest, previous_host),
            (next_guest, next_host),
            (unregistered_guest, unregistered_host),
        ] {
            ctx.shadow_table.map_id(guest, host);
        }
        assert!(ctx.register_native_buffer(previous_host, (1, 1), Vec::new()));
        assert!(ctx.register_native_buffer(next_host, (1, 1), Vec::new()));
        assert!(ctx.mark_buffer_submitted(previous_guest));

        assert!(!ctx.finalize_surface_attachment(Some(previous_guest), Some(unregistered_guest)));
        assert_eq!(
            ctx.host_buffer_use(previous_guest),
            Some(awaiting_release(false)),
            "an unresolved next generation must not detach the previous one"
        );

        assert!(!ctx.finalize_surface_attachment(Some(unregistered_guest), Some(next_guest)));
        assert_eq!(
            ctx.host_buffer_use(next_guest),
            Some(RenderBufferUse::NeverSubmitted),
            "an unresolved previous generation must not submit the next one"
        );
    }

    #[test]
    fn render_buffer_lifecycle_matches_all_short_transition_sequences() {
        #[derive(Clone, Copy, Eq, PartialEq)]
        enum ModelOwnership {
            GuestAlive,
            GuestDestroyed,
            HostDestroyQueued,
        }

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
            let mut ownership = ModelOwnership::GuestAlive;
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
                    (ModelOwnership::HostDestroyQueued, _) => false,
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
                    (ModelOwnership::GuestAlive, Operation::GuestDestroy) => {
                        ownership = ModelOwnership::GuestDestroyed;
                        true
                    }
                    (ModelOwnership::GuestDestroyed, Operation::GuestDestroy) => false,
                    (
                        ModelOwnership::GuestAlive | ModelOwnership::GuestDestroyed,
                        Operation::HostDestroy,
                    ) => {
                        ownership = ModelOwnership::HostDestroyQueued;
                        true
                    }
                };
                let expected = match ownership {
                    ModelOwnership::GuestAlive => {
                        RenderBufferLifecycle::GuestAlive(use_state.clone())
                    }
                    ModelOwnership::GuestDestroyed => {
                        RenderBufferLifecycle::GuestDestroyed(use_state.clone())
                    }
                    ModelOwnership::HostDestroyQueued => RenderBufferLifecycle::HostDestroyQueued,
                };

                assert_eq!(actual_changed, expected_changed);
                assert_eq!(registry.lifecycle(HostId(host_id)), Some(expected));
                assert_eq!(
                    registry.dimensions(HostId(host_id)).is_some(),
                    ownership != ModelOwnership::HostDestroyQueued
                );
            }
        }
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
