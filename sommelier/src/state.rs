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

mod aura_surfaces;
mod id_allocator;
mod input;
mod internal_callbacks;
mod objects;
mod outputs;
mod render;
mod window_placement;
mod xdg_roles;

use std::collections::{HashMap, HashSet};
use std::os::unix::io::OwnedFd;
use std::sync::Arc;

pub(crate) use self::aura_surfaces::AuraSurfaces;
use self::render::RenderBufferRegistry;
#[cfg(test)]
use self::render::MAX_PENDING_DAMAGE_RECTS;
use crate::allocator::Allocator;
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::Outgoing;
use log::{error, warn};

#[allow(unused_imports)]
pub(crate) use self::input::{
    ConfirmPreeditPlan, GuestCommitPlan, GuestKeyDecision, GuestKeyDelivery, GuestKeyEvent,
    GuestKeyOwner, HostActivationState, HostCommitPlan, HostPreeditPlan, KeyGenerationRegistry,
    KeyboardFocus, KeyboardFocusRegistry, KeyboardFocusUpdate, PeekKeyProvenance,
    PreeditRegionPlan, SeatFocusChange, TextInputState,
};
pub(crate) use self::internal_callbacks::{InternalCallback, InternalCallbacks};
pub use self::objects::{DeletedHostId, ShadowTable};
pub use self::outputs::OutputState;
pub(crate) use self::outputs::{
    is_valid_transform, transformed_extent, GlobalBinding, OutputRegistry, INERT_BIND_INTERFACE,
};
pub(crate) use self::render::{
    BufferState, DamageRect, DamageRegion, RenderBufferLifecycle, RenderBufferUse, ShmPool,
    StalePixels, SurfaceAttachment, SurfaceCommit, SurfaceState, ViewportState,
};
#[cfg(test)]
pub(crate) use self::window_placement::ARC_APPLICATION_ID_PREFIX;
pub(crate) use self::window_placement::{
    PendingWindowBoundsConfigure, PendingWindowBoundsSurfaceConfigure, WindowPlacementState,
};
pub(crate) use self::xdg_roles::XdgRoles;
use crate::placement_config::PlacementConfig;

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

impl Drop for Context {
    fn drop(&mut self) {
        // Context is also dropped on synchronous protocol-error paths. Tokio
        // JoinHandle::drop detaches a task, so explicitly abort every active
        // clipboard pump instead of allowing it to outlive the connection.
        for pump in self.clipboard_pumps.drain(..) {
            pump.abort();
        }
    }
}

pub struct PendingParam {
    pub fd: OwnedFd,
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

#[derive(Debug, Default)]
pub struct GtkShellState {
    /// Activation token supplied by GTK for windows created through this
    /// shell binding. ChromeOS validates the token before granting focus.
    pub startup_id: Option<String>,
    /// Synthetic gtk_surface1 objects created from this shell binding.
    pub surfaces: HashSet<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GtkSurfaceState {
    pub shell_id: u32,
    pub wl_surface_id: u32,
}

/// Work a request handler defers until the proxy has forwarded the request.
///
/// A handler runs before the generated dispatcher maps the request's new
/// IDs and encodes it, and the request can still be rejected after the
/// handler returns. Work that must only happen for a forwarded request, or
/// that needs a new ID's host mapping, is recorded with
/// [`Context::defer_until_forwarded`]. The proxy loop takes it after every
/// message and applies it only when the message was forwarded. Requests it
/// queues follow the forwarded bytes and the requests the handler queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostDispatch {
    /// `xdg_surface.get_toplevel` created `xdg_toplevel` from `xdg_surface`.
    XdgToplevelCreated { xdg_surface: u32, xdg_toplevel: u32 },
}

/// The proxy's internal host `zaura_shell` binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostAuraShell {
    pub(crate) id: u32,
    /// The registry name of the global it was bound from, so its
    /// `global_remove` releases exactly this binding.
    pub(crate) global_name: u32,
    /// The bound version, clamped by the `zaura_shell` bind in
    /// `handler/registry.rs` (at least `ZAURA_SHELL_MIN_VERSION`). Every
    /// Aura child created from this binding takes it.
    pub(crate) version: u32,
}

pub struct Context {
    pub shadow_table: ShadowTable,
    pub pools: HashMap<u32, ShmPool>,
    render_buffers: RenderBufferRegistry,
    /// Asynchronous linux-dmabuf creates keyed by host params generation.
    pub pending_native_creates: HashMap<HostId, PendingNativeCreate>,
    pub surfaces: HashMap<u32, SurfaceState>,
    pub text_inputs: HashMap<u32, TextInputState>,
    pub keyboard_to_seat: HashMap<u32, u32>,
    pub keyboard_focus: KeyboardFocusRegistry,
    pub last_sender_id: u32,
    /// Pending messages to send from client→host (e.g. ack_key, bind requests).
    pub client_to_host_queue: Vec<Outgoing>,
    /// Pending messages to send from host→client (e.g. synthetic wl_shm.format).
    pub host_to_client_queue: Vec<Outgoing>,
    /// Whether the session ends after this batch; see
    /// [`Self::fail_session`].
    fatal_protocol_error: bool,
    /// Whether a `wl_display.error` has been queued for the guest; see
    /// [`Self::note_protocol_error`].
    protocol_error_queued: bool,
    /// Work deferred by the handler of the message being dispatched; see
    /// [`PostDispatch`].
    post_dispatch: Vec<PostDispatch>,
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
    /// binding disappeared. They remain destroyable, but must not resize or
    /// create buffers through a replacement host binding.
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
    /// The process-wide placement configuration: host accelerators, the
    /// ARC bounds policy, and the placement bindings. Shared, never copied:
    /// a reloaded binding generation is visible to the next lookup.
    placement: Arc<PlacementConfig>,
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
    /// removed name represents a new advertisement generation; a bind of a
    /// removed name crossed its removal on the wire. Each name keeps the
    /// version the guest saw if it can be bound inert (see
    /// `INERT_BIND_INTERFACE`), and `None` otherwise.
    pub registry_global_removed: HashMap<u32, HashMap<u32, Option<u32>>>,
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
    /// Host-only wl_callback barriers the proxy issued for itself, and what
    /// each one waits for.
    pub(crate) internal_callbacks: InternalCallbacks,
    /// Guest-facing v4 globals withheld until the internal v3 binding has
    /// delivered its complete legacy format/modifier capability set.
    pub pending_dmabuf_globals: Vec<PendingDmabufGlobal>,
    /// Guest dmabuf factory ID to the host-global generation from which it was
    /// bound. Global removal does not invalidate an already-bound factory.
    pub dmabuf_guest_generations: HashMap<u32, u64>,
    pub gpu_accel: bool,
    pub xdg_decoration: bool,
    /// The internal host zaura_shell binding (not exposed to the guest), if
    /// one is bound.
    pub(crate) host_zaura_shell: Option<HostAuraShell>,
    /// VM identifier for ChromeOS guest_os app ID formatting, resolved once
    /// in `main` (see [`vm_identifier_from_environment`]).
    pub vm_identifier: String,
    /// This connection's ARC bounds policy state (see
    /// [`WindowPlacementState`] for what it owns). Built from `placement`.
    pub(crate) window_placement: WindowPlacementState,
    /// This connection's bound host outputs (see [`OutputRegistry`] for what
    /// it owns).
    pub(crate) outputs: OutputRegistry,
    /// Synthetic GTK shell bindings and their activation token state.
    pub gtk_shells: HashMap<u32, GtkShellState>,
    /// Synthetic GTK surfaces associated with guest wl_surface objects.
    pub gtk_surfaces: HashMap<u32, GtkSurfaceState>,
    /// The host Aura surface of each host wl_surface (see [`AuraSurfaces`]).
    pub(crate) aura_surfaces: AuraSurfaces,
    /// The guest's xdg_surface and xdg_toplevel links to their wl_surface.
    pub(crate) xdg_roles: XdgRoles,
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
    /// Defer `effect` until the message being dispatched is forwarded; it is
    /// discarded if the message is dropped or rejected.
    pub(crate) fn defer_until_forwarded(&mut self, effect: PostDispatch) {
        self.post_dispatch.push(effect);
    }

    /// End the session once the current batch is handled: the proxy
    /// flushes the queued events, the guest's `wl_display.error` among
    /// them, and then closes it. A session never recovers.
    pub(crate) fn fail_session(&mut self) {
        self.fatal_protocol_error = true;
    }

    /// Whether [`Self::fail_session`] has ended the session.
    pub(crate) fn is_fatal(&self) -> bool {
        self.fatal_protocol_error
    }

    /// Record that a `wl_display.error` is being queued for the guest, and
    /// return whether it is the first. libwayland-server posts one error per
    /// client and ignores later ones (`wl_resource_post_error_vargs`), so
    /// only the first is sent.
    pub(crate) fn note_protocol_error(&mut self) -> bool {
        !std::mem::replace(&mut self.protocol_error_queued, true)
    }

    /// Forget a fatal error, so a test can go on with the same context.
    #[cfg(test)]
    pub(crate) fn clear_protocol_error_for_test(&mut self) {
        self.fatal_protocol_error = false;
        self.protocol_error_queued = false;
    }

    /// Whether work deferred by a dispatched message has not been taken.
    pub(crate) fn has_post_dispatch(&self) -> bool {
        !self.post_dispatch.is_empty()
    }

    /// Bind host object `id` as the internal `zaura_shell` at the vendored
    /// protocol's version, as the registry would.
    #[cfg(test)]
    pub(crate) fn bind_aura_shell_for_test(&mut self, id: u32) {
        self.host_zaura_shell = Some(HostAuraShell {
            id,
            global_name: id,
            version: crate::protocols::aura_shell::zaura_shell::VERSION,
        });
    }

    /// Take the work deferred while dispatching the current message.
    pub(crate) fn take_post_dispatch(&mut self) -> Vec<PostDispatch> {
        std::mem::take(&mut self.post_dispatch)
    }

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

    /// Stale what `commit` of `surface` changed in the host copies of the
    /// surface's local buffers. See [`StalePixels`].
    pub(crate) fn stale_surface_pixels(&mut self, surface: u32, commit: &SurfaceCommit) {
        // A buffer attached together with a new mapping was last copied for
        // the old one, while the client's damage is relative to the new one:
        // then no host copy relates to the new contents pixel for pixel.
        let relatable = !(commit.has_buffer_attach() && commit.changes_buffer_mapping());
        let shown = commit
            .state
            .current_buffer_id()
            .filter(|_| relatable)
            .and_then(|buffer| self.render_buffer_host_id(buffer));
        self.render_buffers.stale_surface_pixels(
            surface,
            shown,
            &commit.buffer_copy_damage(),
            commit.changes_buffer_mapping(),
        );
    }

    /// Drop every host copy's relation to the destroyed `surface`.
    pub(crate) fn forget_surface_buffer_copies(&mut self, surface: u32) {
        self.render_buffers.forget_surface(surface);
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

    /// Retire a host-only object that is being destroyed, by a destructor
    /// just queued or by the host's terminal event, until its
    /// `wl_display.delete_id` (see [`ShadowTable::mark_pending_destroy_host`]).
    ///
    /// Every caller tracked the object as host-only and has not retired it,
    /// so a refusal means the proxy's ID bookkeeping diverged, which ends the
    /// session.
    pub(crate) fn retire_destroyed_host_object(&mut self, host_id: u32) {
        if !self.shadow_table.mark_pending_destroy_host(host_id) {
            error!(
                "Host object {} was not a live host-only object when it was retired",
                host_id
            );
            self.fail_session();
        }
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

    /// Create the state for one client connection.
    ///
    /// # Arguments
    ///
    /// * `gpu_accel`, `xdg_decoration` - the command-line feature switches.
    /// * `vm_identifier` - the VM namespace of application IDs, resolved
    ///   once in `main` (see [`vm_identifier_from_environment`]).
    /// * `placement` - the process-wide placement configuration, resolved
    ///   once in `main`; `Context` never reads placement variables itself.
    pub fn new(
        gpu_accel: bool,
        xdg_decoration: bool,
        vm_identifier: String,
        placement: Arc<PlacementConfig>,
    ) -> Self {
        // `Allocator::new` reads `SOMMELIER_DRM_DEVICE` and probes the DRM
        // render nodes; without one, buffers fall back to VirtWL or SHM.
        let allocator = match Allocator::new() {
            Ok(alloc) => Some(alloc),
            Err(e) => {
                warn!("Failed to initialize GBM allocator: {}", e);
                None
            }
        };
        Self::from_parts(
            gpu_accel,
            xdg_decoration,
            vm_identifier,
            allocator,
            placement,
        )
    }

    /// Build the state from resolved inputs; reads no environment.
    fn from_parts(
        gpu_accel: bool,
        xdg_decoration: bool,
        vm_identifier: String,
        allocator: Option<Allocator>,
        placement: Arc<PlacementConfig>,
    ) -> Self {
        let window_placement = WindowPlacementState::new(placement.arc_id_allocator());
        Self {
            shadow_table: ShadowTable::new(),
            pools: HashMap::new(),
            render_buffers: RenderBufferRegistry::default(),
            pending_native_creates: HashMap::new(),
            surfaces: HashMap::new(),
            text_inputs: HashMap::new(),
            keyboard_to_seat: HashMap::new(),
            keyboard_focus: KeyboardFocusRegistry::default(),
            last_sender_id: 0,
            client_to_host_queue: Vec::new(),
            host_to_client_queue: Vec::new(),
            fatal_protocol_error: false,
            protocol_error_queued: false,
            post_dispatch: Vec::new(),
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
            placement,
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
            internal_callbacks: InternalCallbacks::default(),
            pending_dmabuf_globals: Vec::new(),
            dmabuf_guest_generations: HashMap::new(),
            gpu_accel,
            xdg_decoration,
            host_zaura_shell: None,
            vm_identifier,
            window_placement,
            outputs: OutputRegistry::default(),
            gtk_shells: HashMap::new(),
            gtk_surfaces: HashMap::new(),
            aura_surfaces: AuraSurfaces::default(),
            xdg_roles: XdgRoles::default(),
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

    /// Test-only constructor that reads no environment variable, probes no
    /// DRM device and reserves no ARC task-ID block.
    ///
    /// The ARC bounds policy is off and `accelerators` are the host
    /// accelerators. The VM identifier is the default `termina`, and there
    /// is no GBM allocator.
    #[cfg(test)]
    pub fn new_for_test(
        gpu_accel: bool,
        xdg_decoration: bool,
        accelerators: Vec<crate::accelerator::Accelerator>,
    ) -> Self {
        Self::from_parts(
            gpu_accel,
            xdg_decoration,
            resolve_vm_identifier(None),
            None,
            Arc::new(PlacementConfig::for_test(accelerators)),
        )
    }

    /// Test-only constructor from an explicit placement configuration, for
    /// example `PlacementConfig::arc_bounds_for_test`. Like `new_for_test`,
    /// it reads no environment variable and has no GBM allocator.
    #[cfg(test)]
    pub(crate) fn with_placement_for_test(placement: impl Into<Arc<PlacementConfig>>) -> Self {
        Self::from_parts(
            false,
            false,
            resolve_vm_identifier(None),
            None,
            placement.into(),
        )
    }

    /// Return whether the host keeps this chord (`SOMMELIER_ACCELERATORS`).
    pub(crate) fn is_host_accelerator(&self, chord: &crate::accelerator::Accelerator) -> bool {
        self.placement.is_host_accelerator(chord)
    }

    /// Return the placement action bound to a chord.
    ///
    /// `None` unless the ARC bounds policy is active and the current binding
    /// generation binds the chord. A generation published by a reload is
    /// adopted here, on the next lookup, so every key press sees one complete
    /// generation. Keys already captured as compositor shortcuts are tracked
    /// by key ownership, not by binding, so a dropped binding cannot strand
    /// a held key.
    pub(crate) fn placement_action_for(
        &self,
        chord: &crate::accelerator::Accelerator,
    ) -> Option<crate::window_layout::WindowLayoutAction> {
        self.placement.action_for(chord)
    }
}

/// The variable that names the VM in ChromeOS application IDs.
const VM_IDENTIFIER_VARIABLE: &str = "SOMMELIER_VM_IDENTIFIER";

/// Read the VM namespace of ChromeOS application IDs from
/// [`VM_IDENTIFIER_VARIABLE`] (see [`resolve_vm_identifier`]).
///
/// A value that is not UTF-8 is logged and replaced by the default.
pub(crate) fn vm_identifier_from_environment() -> String {
    let value = std::env::var_os(VM_IDENTIFIER_VARIABLE).and_then(|value| {
        value
            .into_string()
            .inspect_err(|value| {
                warn!(
                    "Ignoring non-UTF-8 {}={:?}; using the default",
                    VM_IDENTIFIER_VARIABLE, value
                );
            })
            .ok()
    });
    resolve_vm_identifier(value)
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

#[cfg(test)]
// Tests build and close raw descriptors to check who owns them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};

    fn alt_q() -> crate::accelerator::Accelerator {
        crate::accelerator::parse_accelerator("<Alt>q").unwrap()
    }

    #[test]
    fn context_adopts_only_valid_reloads_on_the_next_lookup() {
        use crate::window_layout::WindowLayoutAction;
        let dir = crate::test_support::TempDir::new("context-shortcut-follow");
        let path = dir.write(
            "shortcuts.toml",
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n",
        );
        let placement = Arc::new(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1")],
            Some(&path),
            crate::arc_task_ids::ArcIdAllocator::block_for_test(),
        ));
        let ctx = Context::with_placement_for_test(Arc::clone(&placement));
        assert_eq!(
            ctx.placement_action_for(&alt_q()),
            Some(WindowLayoutAction::Left)
        );

        dir.write("shortcuts.toml", b"version = 1\nbindings = 3\n");
        assert!(placement.reload().unwrap().is_err());
        assert_eq!(
            ctx.placement_action_for(&alt_q()),
            Some(WindowLayoutAction::Left),
            "an invalid reload keeps the last known-good bindings"
        );

        dir.write(
            "shortcuts.toml",
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"right\"\n",
        );
        placement.reload().unwrap().unwrap();
        assert_eq!(
            ctx.placement_action_for(&alt_q()),
            Some(WindowLayoutAction::Right),
            "the next lookup adopts the reloaded generation"
        );
        assert!(
            ctx.window_placement.uses_arc_bounds(),
            "reload never changes the gate"
        );
    }

    #[test]
    fn shortcuts_are_ignored_without_an_active_arc_policy() {
        let ctx = Context::with_placement_for_test(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS", "<Alt>q=left")],
            None,
            crate::arc_task_ids::ArcIdAllocator::block_for_test(),
        ));
        assert_eq!(ctx.placement_action_for(&alt_q()), None);
        assert!(!ctx.window_placement.uses_arc_bounds());
    }

    #[test]
    fn retiring_a_destroyed_host_object_requires_a_live_host_only_object() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let host_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table
            .track_host_interface(host_id, "wl_shm_pool".to_string());
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.shadow_table.is_pending_destroy_host_only(host_id));
        assert!(!ctx.is_fatal());

        // A second retirement means the bookkeeping diverged.
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.is_fatal());
    }

    #[test]
    fn test_contexts_read_no_environment() {
        let accelerators = vec![alt_q()];
        let ctx = Context::new_for_test(true, false, accelerators);
        assert!(ctx.gpu_accel);
        assert!(ctx.is_host_accelerator(&alt_q()));
        assert_eq!(ctx.placement_action_for(&alt_q()), None);
        assert!(!ctx.window_placement.uses_arc_bounds());
        assert_eq!(ctx.vm_identifier, "termina");
        assert!(ctx.allocator.is_none(), "no DRM device is probed");

        let ctx = Context::with_placement_for_test(PlacementConfig::for_test(Vec::new()));
        assert_eq!(ctx.vm_identifier, "termina");
        assert!(ctx.allocator.is_none(), "no DRM device is probed");
    }

    fn awaiting_release(has_detached_use: bool) -> RenderBufferUse {
        RenderBufferUse::AwaitingRelease { has_detached_use }
    }

    #[tokio::test]
    async fn stop_clipboard_pumps_aborts_and_closes_owned_fds() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        // Both ends of a pipe share its identity, and a parallel test can
        // reuse either number once the pump closes it.
        let pipe = crate::test_support::fd_identity(pipe_fds[0]);
        let read_owned = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let write_owned = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let pump = tokio::spawn(async move {
            let mut input = tokio::fs::File::from_std(std::fs::File::from(read_owned));
            let mut output = tokio::fs::File::from_std(std::fs::File::from(write_owned));
            let _ = tokio::io::copy(&mut input, &mut output).await;
        });

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.clipboard_pumps.push(pump);
        ctx.stop_clipboard_pumps().await;

        assert!(!crate::test_support::any_fd_has_identity(pipe));
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
        let ctx = Context::new_for_test(false, false, Vec::new());
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
        use crate::test_support::{any_fd_has_identity, fd_identity, shm_pool_file};
        use std::os::fd::AsRawFd;

        // Memfds have identities of their own, which no other test reuses.
        let files: [_; 4] = std::array::from_fn(|_| shm_pool_file(&[]));
        let identities = files.each_ref().map(|file| fd_identity(file.as_raw_fd()));
        let [first, second, third, pending] = files.map(OwnedFd::from);

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.client_to_host_queue.push((Vec::new(), vec![first]));
        ctx.host_to_client_queue
            .push((Vec::new(), vec![second, third]));
        ctx.pending_params.insert(
            7,
            vec![PendingParam {
                fd: pending,
                plane_idx: 0,
                offset: 0,
                stride: 4,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        drop(ctx);

        for identity in identities {
            assert!(
                !any_fd_has_identity(identity),
                "Context::drop must release every descriptor it owns"
            );
        }
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
            Some(RenderBufferUse::ReleaseOwed)
        );
        assert!(!ctx.buffer_is_submitted(buffer));
        assert!(!ctx.buffer_is_released(buffer));
        assert_eq!(
            ctx.render_buffers.lifecycle(HostId(host_buffer)),
            Some(RenderBufferLifecycle::GuestAlive(
                RenderBufferUse::ReleaseOwed
            ))
        );
        assert!(
            ctx.mark_buffer_released(buffer),
            "the release the host owed for the ended use is expected"
        );
        assert_eq!(ctx.host_buffer_use(buffer), Some(RenderBufferUse::Released));
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
            Some(RenderBufferUse::ReleaseOwed)
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
        let host_buffer = HostId(40);

        for mut encoded in 0..sequence_count {
            // Exercise the lifecycle reducer directly rather than building a
            // full Context for every one of the 15,625 short sequences.
            let mut registry = RenderBufferRegistry::default();
            assert!(registry.register_native(host_buffer, (1, 1), Vec::new()));
            let mut model = RenderBufferUse::NeverSubmitted;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                match operation {
                    Operation::Submit => {
                        if !matches!(&model, RenderBufferUse::AwaitingRelease { .. }) {
                            model = awaiting_release(false);
                        }
                        assert!(registry.submit(host_buffer));
                    }
                    Operation::Release => {
                        let expected = model.owes_release();
                        if expected {
                            model = RenderBufferUse::Released;
                        }
                        assert_eq!(registry.release(host_buffer), expected);
                    }
                    Operation::Detach => {
                        let expected = model.may_be_current();
                        if matches!(&model, RenderBufferUse::AwaitingRelease { .. }) {
                            model = awaiting_release(true);
                        }
                        assert_eq!(
                            registry.finalize_attachment(Some(host_buffer), None),
                            expected
                        );
                    }
                    Operation::LastSurfaceDestroy => {
                        let expected = model.may_be_current();
                        if model == awaiting_release(false) {
                            model = RenderBufferUse::ReleaseOwed;
                        }
                        assert_eq!(registry.end_last_surface_use(host_buffer, false), expected);
                    }
                    Operation::OtherSurfaceRemains => {
                        assert!(registry.end_last_surface_use(host_buffer, true));
                    }
                }
                assert_eq!(
                    registry.lifecycle(host_buffer),
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
                        let owed = use_state.owes_release();
                        if owed {
                            use_state = RenderBufferUse::Released;
                        }
                        owed
                    }
                    (_, Operation::Detach) => {
                        let may_be_current = use_state.may_be_current();
                        if use_state.is_awaiting_release() {
                            use_state = awaiting_release(true);
                        }
                        may_be_current
                    }
                    (_, Operation::LastSurfaceDestroy) => {
                        let may_be_current = use_state.may_be_current();
                        if use_state == awaiting_release(false) {
                            use_state = RenderBufferUse::ReleaseOwed;
                        }
                        may_be_current
                    }
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
}
