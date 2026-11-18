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
mod host_mapping;
mod id_allocator;
mod input;
mod internal_callbacks;
mod lifecycle;
mod objects;
mod outputs;
mod registries;
mod render;
mod window_placement;
mod xdg_roles;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use tokio::task::JoinSet;

pub(crate) use self::aura_surfaces::AuraSurfaces;
pub(crate) use self::host_mapping::HostMapping;
use self::render::RenderBufferRegistry;
#[cfg(test)]
use self::render::MAX_PENDING_DAMAGE_RECTS;
use crate::allocator::{Allocator, PendingFence};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{Action, Outgoing};
use log::{debug, error, warn};
use xkbcommon::xkb;

#[allow(unused_imports)]
pub(crate) use self::input::{
    reconcile, ActivationStep, ConfirmPreeditPlan, CursorRect, GuestKeyDecision, GuestKeyDelivery,
    GuestKeyEvent, GuestKeyOwner, HostApplied, HostCommitPlan, HostContentType, HostPreeditContext,
    HostPreeditPlan, HostTarget, KeyGenerationRegistry, KeyboardFocus, KeyboardFocusRegistry,
    KeyboardFocusUpdate, PeekKeyProvenance, PointerFocus, PreeditRegionPlan, SeatFocusChange,
    ShownPreedit, TextInputState, V3Pending,
};
pub(crate) use self::internal_callbacks::{InternalCallback, InternalCallbacks};
pub use self::objects::{DeletedHostId, Link, ShadowTable};
pub use self::outputs::OutputState;
pub(crate) use self::outputs::{
    is_valid_transform, GlobalBinding, OutputRegistry, INERT_BIND_INTERFACE,
};
pub(crate) use self::registries::RegistryRecord;
#[cfg(test)]
pub(crate) use self::render::SurfacePending;
pub(crate) use self::render::{
    BufferRect, BufferSpace, BufferState, DamageRegion, Rect, RenderBufferLifecycle,
    RenderBufferUse, ShmPool, StalePixels, SurfaceAttachment, SurfaceCommit, SurfaceMapping,
    SurfaceRect, SurfaceState, ViewportState,
};
#[cfg(test)]
pub(crate) use self::window_placement::ARC_APPLICATION_ID_PREFIX;
pub(crate) use self::window_placement::{
    ArcPlacementState, PendingWindowBoundsConfigure, PendingWindowBoundsSurfaceConfigure,
    RemoteShellManager, RemoteShellPlacementState, WindowPlacementState,
};
pub(crate) use self::xdg_roles::XdgRoles;
use crate::placement_config::PlacementConfig;

/// A Wayland object ID allocated by the **guest** (client) side, as
/// [`ShadowTable`] keys guest objects.
///
/// Within the table, and in state keyed by it, a guest ID cannot stand in
/// for a [`HostId`]. Handlers and the table's query methods pass raw `u32`
/// IDs, so at those call sites the direction is a naming convention
/// (`guest_*`, `host_*`), not a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestId(pub(crate) u32);

/// A Wayland object ID on the **host** connection.
///
/// State keyed by host objects (keyboard focus, key generations, render
/// buffers) uses it, so a guest ID cannot be passed there. Event handlers
/// receive the raw sender as `ctx.last_sender_id`; [`HostId::from_event_sender`]
/// wraps it, and [`ShadowTable::get_guest_id`] translates a host ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
}

/// Why a session ends ([`Context::end_session`]). The guest is told in
/// one `wl_display.error` (`handler::display::session_error`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// Protocol error `code` on guest object `object`: one the proxy found
    /// in a guest request, or the host's own, which becomes the guest's.
    Protocol {
        object: u32,
        code: u32,
        message: String,
    },
    /// The proxy failed, or the host ended the session in a way the guest
    /// cannot be told about: `implementation` on the display.
    Internal,
}

/// What dispatching one message produced besides its forward
/// ([`Context::take_effects`]). The proxy commits it whole, or, when the
/// message ends the session, sends none of it.
pub(crate) struct Effects {
    pub(crate) to_host: Vec<Outgoing>,
    pub(crate) to_guest: Vec<Outgoing>,
    pub(crate) end: Option<SessionEnd>,
    /// The fence the message waits for before it can be dispatched again;
    /// see [`Context::wait_for_fence`].
    pub(crate) wait: Option<FenceWait>,
}

/// A guest GPU write that a message waits for ([`Context::wait_for_fence`]).
#[derive(Debug)]
pub(crate) struct FenceWait {
    pub(crate) fence: PendingFence,
    /// How long the message may wait, counted from the first time it does,
    /// before its session ends.
    pub(crate) timeout: std::time::Duration,
}

/// The fence a test's pipe plane stands in for
/// ([`Context::plane_fences_for_test`]).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum PlaneFenceForTest {
    /// The plane is its own sync_file.
    SyncFile,
    /// The virtio-gpu fallback, which reports a busy buffer and nothing to
    /// wait on.
    Busy,
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

/// The most clipboard transfers one connection may have in flight.
///
/// A paste fetches one MIME type and a drop a few, so the cap only bounds
/// what a misbehaving client can pin: a task and two descriptors each.
pub(crate) const MAX_CLIPBOARD_PUMPS: usize = 16;

/// The clipboard transfers of one connection.
///
/// Each pump owns its two descriptors until its transfer ends. The pumps
/// belong to the connection: stopping the set, or dropping it with the
/// [`Context`] on any path, aborts every unfinished one.
#[derive(Default)]
pub(crate) struct ClipboardPumps {
    pumps: JoinSet<()>,
    /// Whether the cap has refused a pump yet. Only the first refusal
    /// warns, so a client that keeps asking cannot flood the log.
    refused: bool,
}

impl ClipboardPumps {
    /// Room for one more pump, after forgetting the ones that have
    /// finished, or `None` while [`MAX_CLIPBOARD_PUMPS`] are in flight.
    pub(crate) fn slot(&mut self) -> Option<PumpSlot<'_>> {
        self.reap();
        if self.pumps.len() < MAX_CLIPBOARD_PUMPS {
            return Some(PumpSlot(&mut self.pumps));
        }
        if std::mem::replace(&mut self.refused, true) {
            debug!("Refusing a clipboard transfer: {MAX_CLIPBOARD_PUMPS} are in flight");
        } else {
            warn!(
                "Refusing a clipboard transfer: {MAX_CLIPBOARD_PUMPS} are in flight; \
                 later refusals on this connection log at debug level"
            );
        }
        None
    }

    /// Abort every pump and wait until each has dropped its descriptors.
    pub(crate) async fn stop(&mut self) {
        self.pumps.shutdown().await;
    }

    /// Forget the pumps that have finished, reporting any that panicked.
    fn reap(&mut self) {
        while let Some(result) = self.pumps.try_join_next() {
            if let Err(e) = result {
                error!("A clipboard pump failed: {}", e);
            }
        }
    }
}

/// Room for one more clipboard pump, from [`ClipboardPumps::slot`].
pub(crate) struct PumpSlot<'a>(&'a mut JoinSet<()>);

impl PumpSlot<'_> {
    pub(crate) fn start(self, pump: impl Future<Output = ()> + Send + 'static) {
        self.0.spawn(pump);
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
    pub surfaces: BTreeSet<u32>,
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
/// message and applies it only when the message is forwarded: it has a
/// forward and did not end the session. What it queues joins the message's
/// effects, after what the handler queued.
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

/// A synthetic guest `wl_shm` object.
#[derive(Debug, Default)]
pub struct GuestShm {
    /// The host `wl_shm` global the guest bound it from. Its requests are
    /// ignored once that name retires (`Context::stale_shm_guest_objects`),
    /// whichever global the proxy itself has bound.
    pub global_name: u32,
    /// The formats already sent to it, so a host capability event that
    /// arrives after the bind is not sent twice.
    pub formats: BTreeSet<u32>,
}

/// The modifier and group state a host `wl_keyboard.modifiers` event
/// carries, as XKB masks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KeyboardModifiers {
    pub(crate) depressed: u32,
    pub(crate) latched: u32,
    pub(crate) locked: u32,
    pub(crate) group: u32,
}

pub struct Context {
    pub shadow_table: ShadowTable,
    pub pools: BTreeMap<u32, ShmPool>,
    render_buffers: RenderBufferRegistry,
    /// Asynchronous linux-dmabuf creates keyed by host params generation.
    pub pending_native_creates: BTreeMap<HostId, PendingNativeCreate>,
    pub surfaces: BTreeMap<u32, SurfaceState>,
    /// Every guest `zwp_text_input_v3`, ordered by ID: a focus change
    /// reaches them in that order (INVARIANTS "Nothing the proxy sends
    /// depends on hash order").
    pub text_inputs: BTreeMap<u32, TextInputState>,
    pub keyboard_to_seat: BTreeMap<u32, u32>,
    pub keyboard_focus: KeyboardFocusRegistry,
    pub pointer_focus: PointerFocus,
    pub last_sender_id: u32,
    /// Requests the message being dispatched sends the host besides its
    /// forward (an ack_key or a bind, say); part of its [`Effects`].
    pub client_to_host_queue: Vec<Outgoing>,
    /// Events the message being dispatched sends the guest besides its
    /// forward (a synthetic wl_shm.format, say); part of its [`Effects`].
    pub host_to_client_queue: Vec<Outgoing>,
    /// Why the message being dispatched ends the session, if it does; see
    /// [`Self::end_session`].
    session_end: Option<SessionEnd>,
    /// Work deferred by the handler of the message being dispatched; see
    /// [`PostDispatch`].
    post_dispatch: Vec<PostDispatch>,
    /// The fence the message being dispatched waits for, if it does; see
    /// [`Self::wait_for_fence`].
    fence_wait: Option<FenceWait>,
    pub allocator: Option<Allocator>,
    /// Test-only capability override for exercising synthetic dma-buf
    /// feedback lifecycle without depending on the runner's `/dev/dri`.
    #[cfg(test)]
    pub synthetic_feedback_available_for_test: bool,
    /// Test-only: what each native buffer plane stands in for. No test can
    /// make a dma-buf whose fence is pending, so a pipe stands in, pending
    /// until it is readable (see `handler::compositor::plane_fence`).
    #[cfg(test)]
    pub plane_fences_for_test: Option<PlaneFenceForTest>,
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
    /// advertises them. Only a VirtWL session offers all of them to the
    /// guest; without VirtWL only `host_wl_shm_formats` count.
    pub host_shm_formats: BTreeSet<u32>,
    /// Optional formats learned from the internal host wl_shm binding.
    /// Keeping the source separate lets a dmabuf global disappear without
    /// invalidating a format that the host's real wl_shm object still offers.
    pub host_wl_shm_formats: BTreeSet<u32>,
    /// Optional formats learned from the internal host linux-dmabuf binding.
    /// These are removed when that binding's global is withdrawn.
    pub host_dmabuf_shm_formats: BTreeSet<u32>,
    /// Every synthetic guest wl_shm object, ordered by ID: a host format
    /// reaches them in that order.
    pub guest_shms: BTreeMap<u32, GuestShm>,
    /// Synthetic guest wl_shm objects whose own global, the name each was
    /// bound from, has retired. Wayland keeps an already-bound global
    /// object valid for teardown, but requests sent to it after its
    /// global's removal are ignored, even while the proxy has bound a
    /// replacement; objects bound from a live name are unaffected.
    pub stale_shm_guest_objects: BTreeSet<u32>,
    /// Synthetic wl_shm_pool children of a stale guest wl_shm, created before
    /// its global retired or after it. They remain destroyable, but must not
    /// resize or create buffers through a replacement host binding.
    pub stale_shm_pools: BTreeSet<u32>,
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
    pub keyboard_to_extended_keyboard: BTreeMap<HostId, HostId>,
    /// Reverse lookup for keyboard-extension v2 events. `peek_key` is sent by
    /// the extended-keyboard object, while the physical-key state belongs to
    /// the corresponding host `wl_keyboard`.
    pub extended_keyboard_to_keyboard: BTreeMap<HostId, HostId>,
    /// Physical, peek, repeat-recovery, and guest-delivery state for every
    /// keyboard/key generation.
    pub key_generations: KeyGenerationRegistry,
    /// Evdev keycodes that the active XKB keymap marks as repeatable.
    pub keyboard_repeatable_keys: BTreeMap<HostId, BTreeSet<u32>>,
    /// Parsed keymap for each host keyboard. Keymap updates and failures are
    /// scoped to their sender so one seat cannot invalidate another seat.
    pub(crate) keyboard_keymaps: BTreeMap<HostId, xkb::Keymap>,
    /// XKB state for each host keyboard. A client can bind keyboards from
    /// multiple seats, whose modifier and layout-group state must not leak.
    /// It makes `Context` `!Send` and `!Sync`: `xkb::State` uses non-atomic
    /// interior mutation, so a connection stays on its one thread.
    pub(crate) keyboard_xkb_states: BTreeMap<HostId, xkb::State>,
    /// The modifiers each host keyboard last sent, kept until the keyboard
    /// ends. Exo sends them only when they change, before `enter` rather
    /// than after it, and not again with a new keymap, so focus changes and
    /// keymaps must not reset them.
    pub(crate) keyboard_modifiers: BTreeMap<HostId, KeyboardModifiers>,
    /// Effective keysym → evdev keycode mappings from each host keyboard's
    /// negotiated XKB keymap. Text-input-v1 `keysym` events do not carry a
    /// physical keycode, so the IME bridge uses this per-keyboard map when it
    /// synthesizes a wl_keyboard event.
    pub keyboard_keysym_to_keycode: BTreeMap<HostId, BTreeMap<u32, u32>>,
    /// The process-wide placement configuration: host accelerators, the
    /// ARC bounds policy, and the placement bindings. Shared, never copied:
    /// a reloaded binding generation is visible to the next lookup.
    placement: Arc<PlacementConfig>,
    pub supported_formats: BTreeSet<u32>,
    /// Host globals visible to the guest, keyed by their unique numeric name.
    pub host_globals: BTreeMap<u32, HostGlobal>,
    /// Every host global the proxy may bind as a singleton, by name, with
    /// the host's interface and version, while a registry lists it; see
    /// `handler::registry::Singleton`.
    pub(crate) singleton_candidates: BTreeMap<u32, HostGlobal>,
    /// Host globals consumed internally by the proxy and therefore never
    /// advertised to the guest. Keeping their names lets global_remove clean
    /// the corresponding host-only object without leaking a removal event.
    pub hidden_host_globals: BTreeMap<u32, String>,
    /// What each host registry has advertised, by host registry ID (see
    /// [`RegistryRecord`]).
    pub(crate) registries: BTreeMap<u32, RegistryRecord>,
    pub global_generations: BTreeMap<u32, u64>,
    pub next_global_generation: u64,
    /// The generation of each global whose current advertisement the host
    /// removed, from the first registry's `global_remove` until the global
    /// is retired or advertised anew. It outlives the registries that saw
    /// the removal, which a guest may destroy before the last registry that
    /// lists the global sees it.
    pub removed_global_generations: BTreeMap<u32, u64>,
    pub pending_params: BTreeMap<u32, Vec<PendingParam>>,
    /// Guest linux-dmabuf params objects consumed by create/create_immed.
    ///
    /// The protocol object remains alive after consumption so subsequent
    /// add/create requests can report `already_used` instead of being
    /// confused with an object that was never tracked.
    pub used_dmabuf_params: BTreeSet<u32>,
    /// Guest feedback objects synthesized locally from the host's legacy
    /// linux-dmabuf format/modifier events, keyed by guest feedback ID. The
    /// generation keeps an object created from an old, still-bound factory
    /// isolated from a replacement global's capabilities. Ordered, because
    /// completed capabilities reach the objects in that order.
    pub synthetic_feedback_objects: BTreeMap<u32, u64>,
    /// Avoid queuing one complete feedback sequence for every legacy
    /// format/modifier event. A host can advertise hundreds of pairs in a
    /// single roundtrip; sending one memfd per event can overflow the
    /// receiver's SCM_RIGHTS batch and desynchronize the Wayland FD stream.
    /// The proxy clears this set after each dispatch batch.
    pub synthetic_feedback_refresh_pending: BTreeSet<u32>,
    /// Capability snapshots for every host dmabuf global generation referenced
    /// by a live guest object. A completed snapshot is immutable except for a
    /// defensive refresh if a non-conforming host sends a late unique pair.
    pub dmabuf_capabilities: BTreeMap<u64, DmabufCapabilityState>,
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
    pub dmabuf_guest_generations: BTreeMap<u32, u64>,
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
    pub gtk_shells: BTreeMap<u32, GtkShellState>,
    /// Synthetic GTK surfaces associated with guest wl_surface objects.
    pub gtk_surfaces: BTreeMap<u32, GtkSurfaceState>,
    /// The host Aura surface of each host wl_surface (see [`AuraSurfaces`]).
    pub(crate) aura_surfaces: AuraSurfaces,
    /// The guest's xdg_surface and xdg_toplevel links to their wl_surface.
    pub(crate) xdg_roles: XdgRoles,
    /// Tracks wp_viewport objects back to their associated wl_surface so
    /// destroying a viewport restores the default damage coordinate mapping.
    pub viewport_to_wl_surface: BTreeMap<u32, u32>,
    /// The connection's clipboard transfers.
    pub(crate) clipboard_pumps: ClipboardPumps,
}

pub(crate) struct LocalBufferCopyResources<'a> {
    pub channel: Option<&'a Arc<VirtWaylandChannel>>,
    pub buffer: &'a mut BufferState,
}

impl Context {
    /// Defer `effect` until the message being dispatched is forwarded; it is
    /// discarded if the message is dropped or rejected.
    pub(crate) fn defer_until_forwarded(&mut self, effect: PostDispatch) {
        self.post_dispatch.push(effect);
    }

    /// End the session with the message being dispatched, which then sends
    /// nothing but the guest's one `wl_display.error` (see [`Effects`]). A
    /// session never recovers.
    ///
    /// libwayland-server posts one error per client and ignores later ones
    /// (`wl_resource_post_error_vargs`), so the first error the guest can be
    /// given wins, and [`SessionEnd::Internal`] only ends a session that has
    /// none.
    pub(crate) fn end_session(&mut self, end: SessionEnd) {
        if matches!(self.session_end, None | Some(SessionEnd::Internal)) {
            self.session_end = Some(end);
        }
    }

    /// Reject the message being dispatched with protocol error `code` on
    /// guest object `object` (see [`Self::end_session`]). A handler returns
    /// the [`Action::Drop`] this returns.
    #[must_use = "a handler returns the Action::Drop"]
    pub(crate) fn reject(&mut self, object: u32, code: u32, message: impl Into<String>) -> Action {
        self.end_session(SessionEnd::Protocol {
            object,
            code,
            message: message.into(),
        });
        Action::Drop
    }

    /// End the session because the proxy failed to handle the message being
    /// dispatched, and log `why` (see [`Self::end_session`]). A handler
    /// returns the [`Action::Drop`] this returns.
    #[must_use = "a handler returns the Action::Drop"]
    pub(crate) fn fail(&mut self, why: impl std::fmt::Display) -> Action {
        error!("{why}");
        self.end_session(SessionEnd::Internal);
        Action::Drop
    }

    /// Have the message being dispatched wait for a guest GPU write: the
    /// proxy holds it and every later message from its peer back, and
    /// dispatches it again once the fence may have signalled. A handler
    /// returns the [`Action::Drop`] this returns, before it changes
    /// anything, so that dispatching the message again is like the first
    /// time. Only a request without descriptor arguments may wait, as its
    /// descriptors would be gone by then.
    #[must_use = "a handler returns the Action::Drop"]
    pub(crate) fn wait_for_fence(&mut self, wait: FenceWait) -> Action {
        self.fence_wait = Some(wait);
        Action::Drop
    }

    /// Whether the message being dispatched waits for a fence (see
    /// [`Self::wait_for_fence`]).
    pub(crate) fn waits_for_fence(&self) -> bool {
        self.fence_wait.is_some()
    }

    /// Take what the message just dispatched produced (see [`Effects`]).
    pub(crate) fn take_effects(&mut self) -> Effects {
        Effects {
            to_host: std::mem::take(&mut self.client_to_host_queue),
            to_guest: std::mem::take(&mut self.host_to_client_queue),
            end: self.session_end.take(),
            wait: self.fence_wait.take(),
        }
    }

    /// Why the message being dispatched ends the session, if it does.
    #[cfg(test)]
    pub(crate) fn session_end(&self) -> Option<&SessionEnd> {
        self.session_end.as_ref()
    }

    /// Whether the message being dispatched ends the session.
    pub(crate) fn is_fatal(&self) -> bool {
        self.session_end.is_some()
    }

    /// Forget a session end, so a test can go on with the same context.
    #[cfg(test)]
    pub(crate) fn clear_session_end_for_test(&mut self) {
        self.session_end = None;
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
        let channel = self.virtwayland_channel.as_ref();
        let buffer = self.render_buffers.local_copy_mut(host_id)?;
        Some(LocalBufferCopyResources { channel, buffer })
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

    /// Create the state of guest `wl_surface` `surface`, as its
    /// `wl_compositor.create_surface` arrives. The state ends with the
    /// surface (see `forget_surface`).
    pub(crate) fn create_surface(&mut self, surface: u32) {
        let previous = self.surfaces.insert(surface, SurfaceState::default());
        debug_assert!(previous.is_none(), "wl_surface {surface} created twice");
    }

    /// The state of the live guest `wl_surface` `surface`, or `None` if the
    /// proxy has lost it, which [`Self::missing_surface_state`] ends the
    /// session for.
    pub(crate) fn surface_mut(&mut self, surface: u32) -> Option<&mut SurfaceState> {
        self.surfaces.get_mut(&surface)
    }

    /// The state of `surface`, created if a test sets it up without a
    /// `create_surface`.
    #[cfg(test)]
    pub(crate) fn surface_for_test(&mut self, surface: u32) -> &mut SurfaceState {
        self.surfaces.entry(surface).or_default()
    }

    /// End the session because live `wl_surface` `surface` has no state, an
    /// internal divergence: every surface gets its state when it is
    /// created. A handler returns the [`Action::Drop`] this returns.
    #[must_use = "a handler returns the Action::Drop"]
    pub(crate) fn missing_surface_state(&mut self, surface: u32) -> Action {
        self.fail(format_args!("wl_surface {surface} has no state"))
    }

    /// Stale what `commit` of `surface`, which shows its buffer through
    /// `mapping`, changed in the host copies of the surface's local buffers.
    /// See [`StalePixels`].
    pub(crate) fn stale_surface_pixels(
        &mut self,
        surface: u32,
        commit: &SurfaceCommit,
        mapping: &SurfaceMapping,
    ) {
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
            &commit.buffer_copy_damage(mapping),
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
    /// `wl_display.delete_id` (see [`ShadowTable::host_destroy_queued`]).
    ///
    /// Every caller tracked the object as host-only and has not retired it,
    /// so a refusal means the proxy's ID bookkeeping diverged, which ends the
    /// session.
    pub(crate) fn retire_destroyed_host_object(&mut self, host_id: u32) {
        if !self.shadow_table.host_destroy_queued(host_id) {
            // Deep in object teardown: the session ends, and the loop
            // drops this message's effects whatever its handler returns.
            let _ = self.fail(format_args!(
                "Host object {host_id} was not a live host-only object when it was retired"
            ));
        }
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

        let render_buffer_ended = if self.shadow_table.is_guest_server_id(guest_buffer_id) {
            self.render_buffers.remove(host_buffer_id)
        } else {
            !self.render_buffers.contains(host_buffer_id)
                || self.render_buffers.mark_host_destroy_queued(host_buffer_id)
        };
        if render_buffer_ended {
            self.guest_destroyed(guest_buffer_id);
        }
        render_buffer_ended
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
        let window_placement = WindowPlacementState::new(placement.backend());
        Self {
            shadow_table: ShadowTable::new(),
            pools: BTreeMap::new(),
            render_buffers: RenderBufferRegistry::default(),
            pending_native_creates: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            text_inputs: BTreeMap::new(),
            keyboard_to_seat: BTreeMap::new(),
            keyboard_focus: KeyboardFocusRegistry::default(),
            pointer_focus: PointerFocus::default(),
            last_sender_id: 0,
            client_to_host_queue: Vec::new(),
            host_to_client_queue: Vec::new(),
            session_end: None,
            post_dispatch: Vec::new(),
            fence_wait: None,
            allocator,
            #[cfg(test)]
            synthetic_feedback_available_for_test: false,
            #[cfg(test)]
            plane_fences_for_test: None,
            virtwayland_channel: None,
            host_dmabuf_id: None,
            host_dmabuf_global_name: None,
            host_shm_id: None,
            host_shm_global_name: None,
            host_text_input_manager_v1_global_name: None,
            host_text_input_extension_v1_global_name: None,
            host_keyboard_extension_global_name: None,
            host_shm_formats: BTreeSet::new(),
            host_wl_shm_formats: BTreeSet::new(),
            host_dmabuf_shm_formats: BTreeSet::new(),
            guest_shms: BTreeMap::new(),
            stale_shm_guest_objects: BTreeSet::new(),
            stale_shm_pools: BTreeSet::new(),
            host_text_input_manager_v1_id: None,
            host_text_input_extension_v1_id: None,
            host_keyboard_extension_id: None,
            keyboard_to_extended_keyboard: BTreeMap::new(),
            extended_keyboard_to_keyboard: BTreeMap::new(),
            key_generations: KeyGenerationRegistry::default(),
            keyboard_repeatable_keys: BTreeMap::new(),
            keyboard_keymaps: BTreeMap::new(),
            keyboard_xkb_states: BTreeMap::new(),
            keyboard_modifiers: BTreeMap::new(),
            keyboard_keysym_to_keycode: BTreeMap::new(),
            placement,
            supported_formats: BTreeSet::new(),
            host_globals: BTreeMap::new(),
            singleton_candidates: BTreeMap::new(),
            hidden_host_globals: BTreeMap::new(),
            registries: BTreeMap::new(),
            global_generations: BTreeMap::new(),
            next_global_generation: 1,
            removed_global_generations: BTreeMap::new(),
            pending_params: BTreeMap::new(),
            used_dmabuf_params: BTreeSet::new(),
            synthetic_feedback_objects: BTreeMap::new(),
            synthetic_feedback_refresh_pending: BTreeSet::new(),
            dmabuf_capabilities: BTreeMap::new(),
            host_dmabuf_generation: None,
            internal_callbacks: InternalCallbacks::default(),
            pending_dmabuf_globals: Vec::new(),
            dmabuf_guest_generations: BTreeMap::new(),
            gpu_accel,
            xdg_decoration,
            host_zaura_shell: None,
            vm_identifier,
            window_placement,
            outputs: OutputRegistry::default(),
            gtk_shells: BTreeMap::new(),
            gtk_surfaces: BTreeMap::new(),
            aura_surfaces: AuraSurfaces::default(),
            xdg_roles: XdgRoles::default(),
            viewport_to_wl_surface: BTreeMap::new(),
            clipboard_pumps: ClipboardPumps::default(),
        }
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
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

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
            ctx.window_placement.arc().is_some(),
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
        assert!(ctx.window_placement.arc().is_none());
    }

    #[test]
    fn retiring_a_destroyed_host_object_requires_a_live_host_only_object() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let host_id = ctx.shadow_table.create_host_only("wl_shm_pool", 1);
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.shadow_table.is_retired_host(host_id));
        assert!(!ctx.is_fatal());

        // A second retirement means the bookkeeping diverged.
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.is_fatal());
    }

    /// The test constructors ignore every `SOMMELIER_*` variable, which a
    /// developer's environment may set (Crostini sets some). The test sets
    /// all of them, in a child process, so a constructor that read one
    /// fails here wherever the test runs. It also checks that it sets every
    /// variable the sources name, so a new one cannot be left out.
    #[test]
    fn test_contexts_read_no_environment() {
        const VARIABLES: &[(&str, &str)] = &[
            ("SOMMELIER_ACCELERATORS", "<Alt>w"),
            // A probe of this node is seen (as an allocator) only where it
            // is a virtio-gpu render node, as in a Crostini container;
            // elsewhere a probe fails unseen.
            ("SOMMELIER_DRM_DEVICE", "/dev/dri/renderD128"),
            ("SOMMELIER_VM_IDENTIFIER", "penguin-vm"),
            ("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1"),
            ("SOMMELIER_WINDOW_PLACEMENT_BACKEND", "arc"),
            ("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS", "<Alt>q=left"),
        ];
        // Variables only tests read.
        const TEST_ONLY: &[&str] = &["SOMMELIER_FUZZ_", "SOMMELIER_TEST_"];
        let named = source_files(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        )))
        .into_iter()
        .flat_map(|path| {
            let source = std::fs::read_to_string(&path).expect("read a source file");
            source
                .split("\"SOMMELIER_")
                .skip(1)
                .filter_map(|rest| {
                    let (name, _) = rest.split_once('"')?;
                    // A whole literal of a variable's name, not a message.
                    (!name.is_empty()
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_uppercase() || byte == b'_'))
                    .then(|| format!("SOMMELIER_{name}"))
                })
                .collect::<Vec<_>>()
        })
        .filter(|name| !TEST_ONLY.iter().any(|prefix| name.starts_with(prefix)))
        .collect::<std::collections::BTreeSet<_>>();
        let set = VARIABLES
            .iter()
            .map(|&(name, _)| name.to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(set, named, "the test sets every SOMMELIER_* variable");

        crate::test_support::in_own_process_with_env(
            "state::tests::test_contexts_read_no_environment",
            VARIABLES,
            test_contexts_read_no_environment_in_child,
        );
    }

    /// Every `.rs` file under `dir`.
    fn source_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .expect("read a source directory")
            .map(|entry| entry.expect("a source directory entry").path())
            .flat_map(|path| {
                if path.is_dir() {
                    source_files(&path)
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    vec![path]
                } else {
                    Vec::new()
                }
            })
            .collect()
    }

    fn test_contexts_read_no_environment_in_child() {
        assert_eq!(
            std::env::var("SOMMELIER_VM_IDENTIFIER").as_deref(),
            Ok("penguin-vm"),
            "the child has the variables"
        );
        let accelerators = vec![alt_q()];
        let ctx = Context::new_for_test(true, false, accelerators);
        assert!(ctx.gpu_accel);
        assert!(ctx.is_host_accelerator(&alt_q()));
        assert_eq!(ctx.placement_action_for(&alt_q()), None);
        assert!(ctx.window_placement.arc().is_none());
        assert_eq!(ctx.vm_identifier, "termina");
        assert!(ctx.allocator.is_none(), "no DRM device is probed");

        let ctx = Context::with_placement_for_test(PlacementConfig::for_test(Vec::new()));
        assert_eq!(ctx.vm_identifier, "termina");
        assert!(ctx.allocator.is_none(), "no DRM device is probed");
    }

    fn awaiting_release(has_detached_use: bool) -> RenderBufferUse {
        RenderBufferUse::AwaitingRelease { has_detached_use }
    }

    /// Stopping the pumps closes their descriptors at once, even while a
    /// pump waits in the middle of a transfer for its peer to write.
    #[tokio::test]
    async fn stop_clipboard_pumps_aborts_and_closes_owned_fds() {
        use crate::test_support::{any_fd_has_identity, fd_identity};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use tokio::io::AsyncReadExt;

        // Each end of a socket pair is a socket of its own, so the pump's
        // ends have identities that the test's ends do not share.
        let (src, mut host) = UnixStream::pair().unwrap();
        let (dst, guest) = UnixStream::pair().unwrap();
        let pump_ends = [src.as_raw_fd(), dst.as_raw_fd()].map(fd_identity);
        guest.set_nonblocking(true).unwrap();
        let mut guest = tokio::net::UnixStream::from_std(guest).unwrap();

        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.clipboard_pumps.slot().unwrap().start(async move {
            let pump = crate::handler::data_device::pump(src.into(), dst.into());
            pump.await.expect("the pump runs until it is stopped");
        });
        // One chunk crosses, so the pump is running, then it waits for the
        // next one, which the host never writes.
        std::io::Write::write_all(&mut host, b"first").unwrap();
        let mut first = [0; 5];
        guest.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"first");

        ctx.clipboard_pumps.stop().await;

        assert!(!pump_ends.into_iter().any(any_fd_has_identity));
        assert_eq!(
            guest.read(&mut first).await.unwrap(),
            0,
            "the guest reads EOF"
        );
    }

    /// A connection runs at most `MAX_CLIPBOARD_PUMPS` pumps, and one that
    /// finishes makes room for another.
    #[tokio::test]
    async fn clipboard_pumps_are_capped_per_connection() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let mut releases = Vec::new();
        for _ in 0..MAX_CLIPBOARD_PUMPS {
            let (release, released) = tokio::sync::oneshot::channel::<()>();
            let slot = ctx.clipboard_pumps.slot().expect("room under the cap");
            slot.start(async {
                let _ = released.await;
            });
            releases.push(release);
        }
        assert!(
            ctx.clipboard_pumps.slot().is_none(),
            "no pump beyond the cap"
        );

        drop(releases.pop());
        // Every ready task runs before this one resumes.
        tokio::task::yield_now().await;
        assert!(ctx.clipboard_pumps.slot().is_some());
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
    fn host_id_raw_round_trip() {
        // Verify the typed constructor round-trips through the same inner u32.
        let mut ctx = Context::new_for_test(false, false, Vec::new());
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
            current_buffer_scale: 1,
            current_buffer_transform: 0,
            viewport: None,
            pending: SurfacePending {
                attachment: SurfaceAttachment::Attach(2),
                surface_damage: vec![SurfaceRect::new(1, 2, 3, 4)].into(),
                buffer_damage: vec![BufferRect::new(5, 6, 7, 8)].into(),
                buffer_scale: Some(2),
                buffer_transform: Some(3),
                offset: Some((9, 10)),
                attach_offset: Some((11, 12)),
                viewport: Some(Some(ViewportState {
                    source: Some((0, 0, 256, 256)),
                    destination: Some((20, 30)),
                })),
            },
            logged_viewport_rule: false,
        };
        surface.set_current_buffer_for_test(Some(1), Some((10, 20)));

        let commit = surface.prepare_commit();

        assert_eq!(commit.attachment, SurfaceAttachment::Attach(2));
        assert_eq!(commit.attached_buffer_id(), Some(2));
        assert!(commit.has_buffer_attach());
        assert!(commit.moves_buffer());
        assert!(!commit.has_invalid_fractional_viewport());
        assert_eq!(
            commit.surface_damage,
            DamageRegion::Rects(vec![SurfaceRect::new(1, 2, 3, 4)])
        );
        assert_eq!(
            commit.buffer_damage,
            DamageRegion::Rects(vec![BufferRect::new(5, 6, 7, 8)])
        );
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
        assert_eq!(surface.pending, SurfacePending::default());

        let next = surface.prepare_commit();
        assert_eq!(next.attachment, SurfaceAttachment::Unchanged);
        assert!(next.surface_damage.is_empty());
        assert!(next.buffer_damage.is_empty());
        assert_eq!(next.buffer_offset, (0, 0));
        assert_eq!(next.state, commit.state);
    }

    #[test]
    fn surface_commit_rollback_restores_the_complete_snapshot() {
        let mut surface = SurfaceState {
            current_content: None,
            current_buffer_scale: 1,
            current_buffer_transform: 0,
            viewport: Some(ViewportState::new()),
            pending: SurfacePending {
                attachment: SurfaceAttachment::Detach,
                surface_damage: vec![SurfaceRect::new(1, 2, 3, 4)].into(),
                buffer_damage: vec![BufferRect::new(5, 6, 7, 8)].into(),
                buffer_scale: Some(2),
                buffer_transform: Some(3),
                offset: Some((4, 5)),
                attach_offset: Some((11, 12)),
                viewport: Some(None),
            },
            logged_viewport_rule: false,
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
        let mut surface = SurfaceState::default();
        surface.pending.attachment = SurfaceAttachment::Detach;
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

        surface.clear_buffer_references(8);
        assert_eq!(surface.current_buffer_id(), Some(7));
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));

        surface.clear_buffer_references(7);
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
            .map(|index| BufferRect::new(index as i32, 0, 1, 1))
            .collect();
        for rect in &exact {
            surface.pending.buffer_damage.push(*rect);
        }

        assert_eq!(surface.pending.buffer_damage, DamageRegion::Rects(exact));

        surface
            .pending
            .buffer_damage
            .push(BufferRect::new(-10, -20, 30, 40));
        assert_eq!(surface.pending.buffer_damage, DamageRegion::Full);

        for _ in 0..MAX_PENDING_DAMAGE_RECTS {
            surface
                .pending
                .buffer_damage
                .push(BufferRect::new(1, 1, 1, 1));
        }
        assert_eq!(
            surface.pending.buffer_damage,
            DamageRegion::Full,
            "a collapsed region must remain bounded"
        );

        let commit = surface.prepare_commit();
        assert_eq!(commit.buffer_damage, DamageRegion::Full);
        assert!(surface.pending.buffer_damage.is_empty());
    }

    #[test]
    fn collapsed_damage_rollback_restores_the_full_pending_region() {
        let mut surface = SurfaceState::default();
        for index in 0..=MAX_PENDING_DAMAGE_RECTS {
            surface
                .pending
                .surface_damage
                .push(SurfaceRect::new(index as i32, 0, 1, 1));
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
        let mut surface = SurfaceState::default();
        surface.pending.viewport = Some(Some(ViewportState {
            source: Some((0, 0, 257, 256)),
            destination: None,
        }));
        assert!(surface.prepare_commit().has_invalid_fractional_viewport());

        surface.pending.viewport = Some(Some(ViewportState {
            source: Some((0, 0, 257, 256)),
            destination: Some((10, 10)),
        }));
        assert!(!surface.prepare_commit().has_invalid_fractional_viewport());
    }

    #[test]
    fn pending_viewport_starts_from_the_pending_else_the_committed_one() {
        let source = Some((0, 0, 256, 256));
        let mut surface = SurfaceState::default();
        surface.pending_viewport_mut().source = source;
        surface.prepare_commit();
        let committed = surface.viewport;

        // Nothing pending: the request edits a copy of the committed viewport.
        surface.pending_viewport_mut().destination = Some((10, 20));
        assert_eq!(surface.viewport, committed);
        assert_eq!(
            surface.pending.viewport,
            Some(Some(ViewportState {
                source,
                destination: Some((10, 20)),
            }))
        );

        // A viewport pending: the request edits it.
        surface.pending_viewport_mut().destination = Some((30, 40));
        assert_eq!(
            surface.prepare_commit().state.viewport,
            Some(ViewportState {
                source,
                destination: Some((30, 40)),
            })
        );

        // A destroyed viewport pending: the request edits a copy of the
        // committed viewport.
        surface.pending.viewport = Some(None);
        surface.pending_viewport_mut().source = None;
        assert_eq!(
            surface.pending.viewport,
            Some(Some(ViewportState {
                source: None,
                destination: Some((30, 40)),
            }))
        );
    }

    #[test]
    fn host_buffer_use_is_an_exclusive_phase() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let buffer = 20;
        let host_buffer = 40;
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, "wl_buffer");
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
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, "wl_buffer");
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
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, "wl_buffer");
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
            ctx.shadow_table
                .insert_paired_for_test(guest, host, "wl_buffer");
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
            ctx.shadow_table
                .insert_paired_for_test(guest, host, "wl_buffer");
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
