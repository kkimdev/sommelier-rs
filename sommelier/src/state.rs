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
mod singletons;
mod window_placement;
mod xdg_roles;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use tokio::task::JoinSet;

pub(crate) use self::aura_surfaces::AuraSurfaces;
pub(crate) use self::host_mapping::{HostMapping, Rows};
use self::render::RenderBufferRegistry;
#[cfg(test)]
use self::render::MAX_PENDING_DAMAGE_RECTS;
use crate::allocator::{PendingFence, VirtGpuDevice};
use crate::virtwl_channel::VirtWaylandChannel;
use crate::wire::{Action, ProtocolError, ToGuest, ToHost};
use log::{debug, error, warn};

pub(crate) use self::input::PointerFocus;
pub(crate) use self::internal_callbacks::{InternalCallback, InternalCallbacks};
pub use self::objects::{
    DeletedHostId, EventObject, EventSender, GuestId, HostId, Link, RequestObject, RequestSender,
    ShadowTable,
};
pub use self::outputs::OutputState;
pub(crate) use self::outputs::{is_valid_transform, OutputRegistry, INERT_BIND_INTERFACE};
pub(crate) use self::registries::RegistryRecord;
#[cfg(test)]
pub(crate) use self::render::SurfacePending;
pub(crate) use self::render::{
    BufferRect, BufferSpace, BufferState, DamageRegion, Rect, RenderBufferLifecycle,
    RenderBufferUse, ShmPool, StalePixels, SurfaceAttachment, SurfaceCommit, SurfaceMapping,
    SurfaceRect, SurfaceState, ViewportState,
};
pub(crate) use self::singletons::{Binding, Singleton, Singletons};
pub(crate) use self::window_placement::{
    Placement, PlacementState, PlacementStep, Point, WindowPlacement,
};
pub(crate) use self::xdg_roles::XdgRoles;
pub(crate) use crate::keymap::KeyboardModifiers;
use crate::placement_config::PlacementConfig;
use crate::settings::Settings;
use std::rc::Rc;

/// The message that `message` encoded, or `None` after logging why it was
/// dropped (see [`Context::send_host_or_drop`]).
fn sent_or_dropped<M>(
    message: Result<M, ProtocolError>,
    what: impl std::fmt::Display,
) -> Option<M> {
    message
        .inspect_err(|error| log::warn!("Dropping {what}: {error}"))
        .ok()
}

/// Compare Wayland serials across wrapping `u32` space.
pub(crate) fn serial_is_after(candidate: u32, previous: u32) -> bool {
    let distance = candidate.wrapping_sub(previous);
    distance != 0 && distance < (1 << 31)
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

impl SessionEnd {
    /// The proxy failed ([`Self::Internal`]). Every internal end is made
    /// here, and logged under [`INTERNAL_END_TARGET`], so that a test can
    /// tell it from a rejection of the guest even when a later protocol
    /// error replaces it ([`Context::end_session`]) or the guest has gone.
    pub(crate) fn internal() -> Self {
        error!(target: INTERNAL_END_TARGET, "the session ended internally");
        Self::Internal
    }
}

/// The log target of [`SessionEnd::internal`].
pub(crate) const INTERNAL_END_TARGET: &str = "sommelier::internal_end";

/// What dispatching one message produced besides its forward
/// ([`Context::take_effects`]). The proxy commits it whole, or, when the
/// message ends the session, sends none of it. The output drains the
/// context's queues, which keep their capacity for the next message;
/// dropping what it has not yielded closes its descriptors.
pub(crate) struct Effects<'a> {
    pub(crate) to_host: std::vec::Drain<'a, ToHost>,
    pub(crate) to_guest: std::vec::Drain<'a, ToGuest>,
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
/// own `global` event; the registry's removal of the global drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDmabufGlobal {
    pub registry_host_id: HostId,
    pub name: u32,
    pub version: u32,
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
    pub guest_params_id: GuestId,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GtkSurfaceState {
    pub shell_id: GuestId,
    pub wl_surface_id: GuestId,
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
    XdgToplevelCreated {
        xdg_surface: GuestId,
        xdg_toplevel: GuestId,
    },
    /// `wl_seat.get_keyboard` created guest keyboard `keyboard` of guest
    /// seat `seat`, whose host keyboard its forward mapped.
    KeyboardCreated { keyboard: GuestId, seat: GuestId },
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

pub struct Context {
    pub shadow_table: ShadowTable,
    pub pools: BTreeMap<GuestId, ShmPool>,
    render_buffers: RenderBufferRegistry,
    /// Asynchronous linux-dmabuf creates keyed by host params generation.
    pub pending_native_creates: BTreeMap<HostId, PendingNativeCreate>,
    pub surfaces: BTreeMap<GuestId, SurfaceState>,
    /// Every guest `zwp_text_input_v3`, ordered by ID: a focus change
    /// reaches them in that order (INVARIANTS "Nothing the proxy sends
    /// depends on hash order").
    pub ime: crate::ime::ImeCore,
    /// The keyboard core: every host keyboard, one record each, and its
    /// keys.
    pub(crate) keys: crate::keys::KeyboardCore,
    pub pointer_focus: PointerFocus,
    /// Wheel steps from touchpad scrolling, which the client gets if
    /// [`crate::client_kind`] chooses it.
    pub(crate) wheel_steps: crate::handler::pointer::WheelSteps,
    /// Requests the message being dispatched sends the host besides its
    /// forward (an ack_key or a bind, say); part of its [`Effects`].
    to_host: Vec<ToHost>,
    /// Events the message being dispatched sends the guest besides its
    /// forward (a synthetic wl_shm.format, say); part of its [`Effects`].
    to_guest: Vec<ToGuest>,
    /// Why the message being dispatched ends the session, if it does; see
    /// [`Self::end_session`].
    session_end: Option<SessionEnd>,
    /// Work deferred by the handler of the message being dispatched; see
    /// [`PostDispatch`].
    post_dispatch: Vec<PostDispatch>,
    /// The fence the message being dispatched waits for, if it does; see
    /// [`Self::wait_for_fence`].
    fence_wait: Option<FenceWait>,
    /// The process's settings, shared by every connection.
    settings: Rc<Settings>,
    /// The process's virtio-gpu render node, shared by every client.
    virtgpu: Option<Arc<VirtGpuDevice>>,
    /// Test-only capability override for exercising synthetic dma-buf
    /// feedback lifecycle without depending on the runner's `/dev/dri`.
    #[cfg(test)]
    pub synthetic_feedback_available_for_test: bool,
    /// Test-only: what each native buffer plane stands in for. No test can
    /// make a dma-buf whose fence is pending, so a pipe stands in, pending
    /// until it is readable (see `handler::compositor::plane_fence`).
    #[cfg(test)]
    pub plane_fences_for_test: Option<PlaneFenceForTest>,
    /// This connection's VirtWL channel to the host, if it has one.
    virtwayland_channel: Option<Arc<VirtWaylandChannel>>,
    /// Optional formats learned from the internal host wl_shm binding.
    /// Keeping the source separate lets a dmabuf global disappear without
    /// invalidating a format that the host's real wl_shm object still offers.
    pub host_wl_shm_formats: BTreeSet<u32>,
    /// Optional formats learned from the internal host linux-dmabuf binding.
    /// These are removed when that binding's global is withdrawn.
    pub host_dmabuf_shm_formats: BTreeSet<u32>,
    /// Every synthetic guest wl_shm object, ordered by ID: a host format
    /// reaches them in that order.
    pub guest_shms: BTreeMap<GuestId, GuestShm>,
    /// Synthetic guest wl_shm objects whose own global, the name each was
    /// bound from, has retired. Wayland keeps an already-bound global
    /// object valid for teardown, but requests sent to it after its
    /// global's removal are ignored, even while the proxy has bound a
    /// replacement; objects bound from a live name are unaffected.
    pub stale_shm_guest_objects: BTreeSet<GuestId>,
    /// Synthetic wl_shm_pool children of a stale guest wl_shm, created before
    /// its global retired or after it. They remain destroyable, but must not
    /// resize or create buffers through a replacement host binding.
    pub stale_shm_pools: BTreeSet<GuestId>,
    /// The host's globals and the proxy's bindings of them.
    pub(crate) globals: crate::handler::registry::Globals,
    pub pending_params: BTreeMap<GuestId, Vec<PendingParam>>,
    /// Guest linux-dmabuf params objects consumed by create/create_immed.
    ///
    /// The protocol object remains alive after consumption so subsequent
    /// add/create requests can report `already_used` instead of being
    /// confused with an object that was never tracked.
    pub used_dmabuf_params: BTreeSet<GuestId>,
    /// Guest feedback objects synthesized locally from the host's legacy
    /// linux-dmabuf format/modifier events, keyed by guest feedback ID,
    /// with the host global whose capabilities they carry, so an object
    /// created from an old, still-bound factory stays isolated from a
    /// replacement global's capabilities. Ordered, because completed
    /// capabilities reach the objects in that order.
    pub synthetic_feedback_objects: BTreeMap<GuestId, u32>,
    /// Avoid queuing one complete feedback sequence for every legacy
    /// format/modifier event. A host can advertise hundreds of pairs in a
    /// single roundtrip; sending one memfd per event can overflow the
    /// receiver's SCM_RIGHTS batch and desynchronize the Wayland FD stream.
    /// The proxy clears this set after each dispatch batch.
    pub synthetic_feedback_refresh_pending: BTreeSet<GuestId>,
    /// Capability snapshots for every host dmabuf global, by name, that
    /// anything holds (`handler::linux_dmabuf::capabilities_held`). A
    /// completed snapshot is immutable except for a defensive refresh if a
    /// non-conforming host sends a late unique pair.
    pub dmabuf_capabilities: BTreeMap<u32, DmabufCapabilityState>,
    /// Host-only wl_callback barriers the proxy issued for itself, and what
    /// each one waits for.
    pub(crate) internal_callbacks: InternalCallbacks,
    /// Guest-facing v4 globals withheld until the internal v3 binding has
    /// delivered its complete legacy format/modifier capability set.
    pub pending_dmabuf_globals: Vec<PendingDmabufGlobal>,
    /// Guest dmabuf factory ID to the name of the host global it was bound
    /// from. Global removal does not invalidate an already-bound factory.
    pub dmabuf_factories: BTreeMap<GuestId, u32>,
    /// This connection's window placement, while it is on (see
    /// [`WindowPlacement`] for what it owns); decided as it connects.
    window_placement: Option<WindowPlacement>,
    /// This connection's bound host outputs (see [`OutputRegistry`] for what
    /// it owns).
    pub(crate) outputs: OutputRegistry,
    /// Synthetic GTK shell bindings and their activation token state.
    pub gtk_shells: BTreeMap<GuestId, GtkShellState>,
    /// Synthetic GTK surfaces associated with guest wl_surface objects.
    pub gtk_surfaces: BTreeMap<GuestId, GtkSurfaceState>,
    /// The host Aura surface of each host wl_surface (see [`AuraSurfaces`]).
    pub(crate) aura_surfaces: AuraSurfaces,
    /// The guest's xdg_surface and xdg_toplevel links to their wl_surface.
    pub(crate) xdg_roles: XdgRoles,
    /// Tracks wp_viewport objects back to their associated wl_surface so
    /// destroying a viewport restores the default damage coordinate mapping.
    pub viewport_to_wl_surface: BTreeMap<GuestId, GuestId>,
    /// The connection's clipboard transfers.
    pub(crate) clipboard_pumps: ClipboardPumps,
}

pub(crate) struct LocalBufferCopyResources<'a> {
    pub channel: Option<&'a Arc<VirtWaylandChannel>>,
    pub buffer: &'a mut BufferState,
}

impl Context {
    /// Send `request` to the host after the message being dispatched (see
    /// [`Effects`]).
    pub(crate) fn send_host(&mut self, request: ToHost) {
        self.to_host.push(request);
    }

    /// Send `event` to the guest after the message being dispatched (see
    /// [`Effects`]).
    pub(crate) fn send_guest(&mut self, event: ToGuest) {
        self.to_guest.push(event);
    }

    /// [`Self::send_host`] each of `requests`, in order.
    pub(crate) fn send_host_all(&mut self, requests: impl IntoIterator<Item = ToHost>) {
        self.to_host.extend(requests);
    }

    /// [`Self::send_guest`] each of `events`, in order.
    pub(crate) fn send_guest_all(&mut self, events: impl IntoIterator<Item = ToGuest>) {
        self.to_guest.extend(events);
    }

    /// [`Self::send_host`] a request that may not have been encoded, or log
    /// and drop it.
    ///
    /// Only the encoder of a message with a string or array fails, when the
    /// data does not fit [`crate::wire::MessageHeader::MAX_LEN`]. That data
    /// can come from the host or a guest, so dropping the one message keeps
    /// the session alive instead of ending it.
    ///
    /// # Arguments
    ///
    /// * `what` - names the message in the log, such as
    ///   `format_args!("startup ID for zaura_surface {id}")`.
    ///
    /// # Returns
    ///
    /// Whether the message was queued.
    pub(crate) fn send_host_or_drop(
        &mut self,
        request: Result<ToHost, ProtocolError>,
        what: impl std::fmt::Display,
    ) -> bool {
        sent_or_dropped(request, what)
            .map(|request| self.send_host(request))
            .is_some()
    }

    /// [`Self::send_guest`] an event that may not have been encoded, or log
    /// and drop it, as [`Self::send_host_or_drop`] does.
    pub(crate) fn send_guest_or_drop(
        &mut self,
        event: Result<ToGuest, ProtocolError>,
        what: impl std::fmt::Display,
    ) -> bool {
        sent_or_dropped(event, what)
            .map(|event| self.send_guest(event))
            .is_some()
    }

    /// Whether the message being dispatched sends anything besides its
    /// forward.
    pub(crate) fn sends_anything(&self) -> bool {
        !self.to_host.is_empty() || !self.to_guest.is_empty()
    }

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
    pub(crate) fn reject(
        &mut self,
        object: GuestId,
        code: u32,
        message: impl Into<String>,
    ) -> Action {
        self.end_session(SessionEnd::Protocol {
            object: crate::wire::WireId::wire(object),
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
        self.end_session(SessionEnd::internal());
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
    pub(crate) fn take_effects(&mut self) -> Effects<'_> {
        Effects {
            to_host: self.to_host.drain(..),
            to_guest: self.to_guest.drain(..),
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

    /// The guest events the message being dispatched sends so far, for a
    /// test that dispatches through a handler directly.
    #[cfg(test)]
    pub(crate) fn guest_queue_for_test(&self) -> &[ToGuest] {
        &self.to_guest
    }

    /// The host requests the message being dispatched sends so far, for a
    /// test that dispatches through a handler directly.
    #[cfg(test)]
    pub(crate) fn host_queue_for_test(&self) -> &[ToHost] {
        &self.to_host
    }

    /// Forget what the messages dispatched so far send, for a test that
    /// dispatches through a handler directly.
    #[cfg(test)]
    pub(crate) fn clear_queues_for_test(&mut self) {
        self.to_guest.clear();
        self.to_host.clear();
    }

    /// Bind host object `id` as the internal `zaura_shell` at the vendored
    /// protocol's version, as the registry would.
    #[cfg(test)]
    pub(crate) fn bind_aura_shell_for_test(&mut self, id: HostId) {
        self.globals.bind_for_test(Singleton::AuraShell, id);
    }

    /// Take the work deferred while dispatching the current message.
    pub(crate) fn take_post_dispatch(&mut self) -> Vec<PostDispatch> {
        std::mem::take(&mut self.post_dispatch)
    }

    pub(crate) fn render_buffer_host_id(&self, guest_buffer_id: GuestId) -> Option<HostId> {
        self.shadow_table
            .get_host_id(guest_buffer_id)
            .filter(|host_id| self.render_buffers.contains(*host_id))
    }

    pub(crate) fn register_local_buffer(
        &mut self,
        guest_buffer_id: GuestId,
        host_buffer_id: HostId,
        backing: BufferState,
    ) -> bool {
        debug_assert_eq!(
            self.shadow_table.get_host_id(guest_buffer_id),
            Some(host_buffer_id)
        );
        self.render_buffers.register_local(host_buffer_id, backing)
    }

    pub(crate) fn register_native_buffer(
        &mut self,
        host_buffer_id: HostId,
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    ) -> bool {
        self.render_buffers
            .register_native(host_buffer_id, size, sync_fds)
    }

    pub(crate) fn local_buffer(&self, guest_buffer_id: GuestId) -> Option<&BufferState> {
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

    /// Whether any buffer the guest destroyed waits to be retired.
    pub(crate) fn has_guest_destroyed_buffers(&self) -> bool {
        self.render_buffers.has_guest_destroyed()
    }

    #[cfg(test)]
    pub(crate) fn render_buffer_count(&self) -> usize {
        self.render_buffers.len()
    }

    /// Create the state of guest `wl_surface` `surface`, as its
    /// `wl_compositor.create_surface` arrives. The state ends with the
    /// surface (see `forget_surface`).
    pub(crate) fn create_surface(&mut self, surface: GuestId) {
        let previous = self.surfaces.insert(surface, SurfaceState::default());
        debug_assert!(previous.is_none(), "wl_surface {surface} created twice");
    }

    /// The state of the live guest `wl_surface` `surface`, or `None` if the
    /// proxy has lost it, which [`Self::missing_surface_state`] ends the
    /// session for.
    pub(crate) fn surface_mut(&mut self, surface: GuestId) -> Option<&mut SurfaceState> {
        self.surfaces.get_mut(&surface)
    }

    /// The state of `surface`, created if a test sets it up without a
    /// `create_surface`.
    #[cfg(test)]
    pub(crate) fn surface_for_test(&mut self, surface: GuestId) -> &mut SurfaceState {
        self.surfaces.entry(surface).or_default()
    }

    /// End the session because live `wl_surface` `surface` has no state, an
    /// internal divergence: every surface gets its state when it is
    /// created. A handler returns the [`Action::Drop`] this returns.
    #[must_use = "a handler returns the Action::Drop"]
    pub(crate) fn missing_surface_state(&mut self, surface: GuestId) -> Action {
        self.fail(format_args!("wl_surface {surface} has no state"))
    }

    /// Stale what `commit` of `surface`, which shows its buffer through
    /// `mapping`, changed in the host copies of the surface's local buffers.
    /// See [`StalePixels`].
    pub(crate) fn stale_surface_pixels(
        &mut self,
        surface: GuestId,
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
    pub(crate) fn forget_surface_buffer_copies(&mut self, surface: GuestId) {
        self.render_buffers.forget_surface(surface);
    }

    pub(crate) fn buffer_dimensions(&self, guest_buffer_id: GuestId) -> Option<(i32, i32)> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.buffer_dimensions_for_host(host_id)
    }

    pub(crate) fn buffer_dimensions_for_host(&self, host_id: HostId) -> Option<(i32, i32)> {
        self.render_buffers.dimensions(host_id)
    }

    pub(crate) fn native_buffer_sync_fds(&self, guest_buffer_id: GuestId) -> Option<&[OwnedFd]> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.render_buffers.native_sync_fds(host_id)
    }

    pub(crate) fn native_buffer_uses_implicit_sync(&self, guest_buffer_id: GuestId) -> bool {
        self.render_buffer_host_id(guest_buffer_id)
            .is_some_and(|host_id| self.render_buffers.uses_implicit_sync_fallback(host_id))
    }

    pub(crate) fn enable_native_buffer_implicit_sync(&mut self, guest_buffer_id: GuestId) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.enable_implicit_sync_fallback(host_id)
    }

    pub(crate) fn host_buffer_use(&self, guest_buffer_id: GuestId) -> Option<RenderBufferUse> {
        let host_id = self.render_buffer_host_id(guest_buffer_id)?;
        self.render_buffers.lifecycle(host_id)?.use_state().cloned()
    }

    #[cfg(test)]
    pub(crate) fn mark_buffer_submitted(&mut self, guest_buffer_id: GuestId) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.submit(host_id)
    }

    pub(crate) fn mark_buffer_released(&mut self, guest_buffer_id: GuestId) -> bool {
        let Some(host_id) = self.render_buffer_host_id(guest_buffer_id) else {
            return false;
        };
        self.render_buffers.release(host_id)
    }

    pub(crate) fn finish_surface_destroy_use(
        &mut self,
        guest_buffer_id: GuestId,
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
        previous_guest_buffer: Option<GuestId>,
        next_guest_buffer: Option<GuestId>,
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

    pub(crate) fn mark_buffer_guest_destroyed(&mut self, guest_buffer_id: GuestId) -> bool {
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
    pub(crate) fn retire_destroyed_host_object(&mut self, host_id: HostId) {
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
        guest_buffer_id: GuestId,
        host_buffer_id: HostId,
    ) -> bool {
        if self.shadow_table.get_host_id(guest_buffer_id) != Some(host_buffer_id) {
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

    pub(crate) fn buffer_is_submitted(&self, guest_buffer_id: GuestId) -> bool {
        self.host_buffer_use(guest_buffer_id)
            .is_some_and(|use_state| use_state.is_awaiting_release())
    }

    pub(crate) fn buffer_is_released(&self, guest_buffer_id: GuestId) -> bool {
        self.host_buffer_use(guest_buffer_id) == Some(RenderBufferUse::Released)
    }

    pub(crate) fn host_buffer_is_guest_destroyed(&self, host_buffer_id: HostId) -> bool {
        self.render_buffers
            .lifecycle(host_buffer_id)
            .is_some_and(|lifecycle| lifecycle.is_guest_destroyed())
    }

    /// Create the state of one client connection.
    ///
    /// # Arguments
    ///
    /// * `settings` - the process's settings.
    /// * `facts` - what the connection learned as it was set up. It has
    ///   placement exactly while the shared bindings are valid now, so a
    ///   client that connects after the bindings file appeared gets
    ///   placement, and one that connects after it went does not.
    pub(crate) fn new(settings: Rc<Settings>, facts: ConnectionFacts) -> Self {
        let ConnectionFacts {
            virtwl,
            virtgpu,
            wheel,
            placement,
        } = facts;
        Self {
            shadow_table: ShadowTable::new(),
            pools: BTreeMap::new(),
            render_buffers: RenderBufferRegistry::default(),
            pending_native_creates: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            ime: crate::ime::ImeCore::default(),
            keys: crate::keys::KeyboardCore::new(settings.host_accelerators.clone()),
            pointer_focus: PointerFocus::default(),
            wheel_steps: crate::handler::pointer::WheelSteps::new(wheel),
            to_host: Vec::new(),
            to_guest: Vec::new(),
            session_end: None,
            post_dispatch: Vec::new(),
            fence_wait: None,
            settings,
            virtgpu,
            #[cfg(test)]
            synthetic_feedback_available_for_test: false,
            #[cfg(test)]
            plane_fences_for_test: None,
            virtwayland_channel: virtwl,
            host_wl_shm_formats: BTreeSet::new(),
            host_dmabuf_shm_formats: BTreeSet::new(),
            guest_shms: BTreeMap::new(),
            stale_shm_guest_objects: BTreeSet::new(),
            stale_shm_pools: BTreeSet::new(),
            globals: Default::default(),
            pending_params: BTreeMap::new(),
            used_dmabuf_params: BTreeSet::new(),
            synthetic_feedback_objects: BTreeMap::new(),
            synthetic_feedback_refresh_pending: BTreeSet::new(),
            dmabuf_capabilities: BTreeMap::new(),
            internal_callbacks: InternalCallbacks::default(),
            pending_dmabuf_globals: Vec::new(),
            dmabuf_factories: BTreeMap::new(),
            window_placement: WindowPlacement::connect(placement),
            outputs: OutputRegistry::default(),
            gtk_shells: BTreeMap::new(),
            gtk_surfaces: BTreeMap::new(),
            aura_surfaces: AuraSurfaces::default(),
            xdg_roles: XdgRoles::default(),
            viewport_to_wl_surface: BTreeMap::new(),
            clipboard_pumps: ClipboardPumps::default(),
        }
    }

    /// A test's connection: [`Settings::for_test`] with the given switches
    /// and host accelerators, and [`ConnectionFacts::for_test`].
    #[cfg(test)]
    pub fn new_for_test(
        gpu_accel: bool,
        xdg_decoration: bool,
        accelerators: Vec<crate::accelerator::Accelerator>,
    ) -> Self {
        let settings = Settings {
            gpu_accel,
            xdg_decoration,
            host_accelerators: accelerators.into(),
            ..Settings::for_test()
        };
        Self::new(Rc::new(settings), ConnectionFacts::for_test())
    }

    /// A test's connection with the placement configuration `placement`,
    /// for example `PlacementConfig::active_for_test`.
    #[cfg(test)]
    pub(crate) fn with_placement_for_test(placement: PlacementConfig) -> Self {
        Self::for_test(Settings::for_test(), placement)
    }

    /// A test's connection with `settings` and the placement
    /// configuration `placement`.
    #[cfg(test)]
    pub(crate) fn for_test(settings: Settings, placement: PlacementConfig) -> Self {
        Self::new(
            Rc::new(settings),
            ConnectionFacts {
                placement,
                ..ConnectionFacts::for_test()
            },
        )
    }

    /// The process's settings.
    pub(crate) fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The process's virtio-gpu render node, if it has opened.
    pub(crate) fn virtgpu(&self) -> Option<&VirtGpuDevice> {
        self.virtgpu.as_deref()
    }

    /// This connection's VirtWL channel to the host, if it has one.
    pub(crate) fn virtwayland_channel(&self) -> Option<&Arc<VirtWaylandChannel>> {
        self.virtwayland_channel.as_ref()
    }

    /// Step the keyboard core with `input`, reading the facts it does not
    /// own from this context ([`crate::handler::keyboard::CtxEnv`]).
    pub(crate) fn step_keys(&mut self, input: crate::keys::KeyInput) -> crate::keys::KeyEffects {
        let env = crate::handler::keyboard::CtxEnv::new(self.window_placement.as_ref(), &self.ime);
        self.keys.step(input, &env)
    }

    /// Where shortcut binding `binding` places the focused window, while
    /// placement is on.
    pub(crate) fn placement_rect(
        &self,
        binding: crate::accelerator::BindingId,
    ) -> Option<crate::window_layout::NormalizedRect> {
        self.window_placement.as_ref()?.rect(binding)
    }

    /// Where the placement bound to `chord` puts the focused window.
    #[cfg(test)]
    pub(crate) fn placement_rect_for(
        &self,
        chord: &crate::accelerator::Accelerator,
    ) -> Option<crate::window_layout::NormalizedRect> {
        let binding = self.window_placement.as_ref()?.binding_for(chord)?;
        self.placement_rect(binding)
    }

    /// This connection's placement state, while placement is on for it.
    pub(crate) fn placement(&self) -> Option<&PlacementState> {
        Some(&self.window_placement.as_ref()?.state)
    }

    /// [`placement`](Self::placement), mutably.
    pub(crate) fn placement_mut(&mut self) -> Option<&mut PlacementState> {
        Some(&mut self.window_placement.as_mut()?.state)
    }
}

/// What one connection learned as it was set up, besides the process's
/// [`Settings`]: everything [`Context::new`] needs that differs between
/// connections.
pub(crate) struct ConnectionFacts {
    /// Its VirtWL channel to the host, when the host is a VirtWL device.
    pub(crate) virtwl: Option<Arc<VirtWaylandChannel>>,
    /// The process's virtio-gpu render node, if it has opened.
    pub(crate) virtgpu: Option<Arc<VirtGpuDevice>>,
    /// Whether its client gets wheel steps from touchpad scrolling.
    pub(crate) wheel: crate::client_kind::WheelClient,
    /// The process's window placement configuration.
    pub(crate) placement: PlacementConfig,
}

#[cfg(test)]
impl ConnectionFacts {
    /// A test connection's: no VirtWL channel or render node, no wheel
    /// steps, and placement off.
    pub(crate) fn for_test() -> Self {
        Self {
            virtwl: None,
            virtgpu: None,
            wheel: crate::client_kind::WheelClient::No,
            placement: PlacementConfig::for_test(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::{wl_keyboard, wl_shm};
    use crate::protocols::Iface;
    use std::os::fd::OwnedFd;

    fn alt_q() -> crate::accelerator::Accelerator {
        crate::accelerator::parse_accelerator("<Alt>q").unwrap()
    }

    /// Placement is decided per connection when it starts: a connection
    /// that started while the file was missing stays off when the file
    /// appears, and the next connection is on. One that started on keeps
    /// placement when the file goes, with no bindings.
    #[test]
    fn a_connection_takes_placement_from_the_file_when_it_starts() {
        let dir = crate::test_support::TempDir::new("context-placement-file");
        let path = dir.path().join("window-placement.toml");
        let (mut file, placement) = crate::placement_config::BindingsFile::loaded_for_test(&path);
        let before = Context::with_placement_for_test(placement.clone());
        assert!(before.placement().is_none());

        dir.write(
            "window-placement.toml",
            b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\naction = \"left\"\n",
        );
        file.reload();
        let after = Context::with_placement_for_test(placement.clone());
        assert!(after.placement().is_some());
        let left = crate::window_layout::NormalizedRect::preset("left");
        assert_eq!(after.placement_rect_for(&alt_q()), left);
        assert_eq!(
            before.placement_rect_for(&alt_q()),
            None,
            "a connection that is off binds nothing"
        );

        std::fs::remove_file(&path).unwrap();
        file.reload();
        assert_eq!(after.placement_rect_for(&alt_q()), None);
        assert!(after.placement().is_some());
        assert!(Context::with_placement_for_test(placement)
            .placement()
            .is_none());
    }

    #[test]
    fn retiring_a_destroyed_host_object_requires_a_live_host_only_object() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let host_id = ctx.shadow_table.create_host_only(Iface::WlShmPool, 1);
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.shadow_table.is_retired_host(host_id));
        assert!(!ctx.is_fatal());

        // A second retirement means the bookkeeping diverged.
        ctx.retire_destroyed_host_object(host_id);
        assert!(ctx.is_fatal());
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

        let mut pumps = ClipboardPumps::default();
        pumps.slot().unwrap().start(async move {
            let pump = crate::handler::data_device::pump(src.into(), dst.into());
            pump.await.expect("the pump runs until it is stopped");
        });
        // One chunk crosses, so the pump is running, then it waits for the
        // next one, which the host never writes.
        std::io::Write::write_all(&mut host, b"first").unwrap();
        let mut first = [0; 5];
        guest.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"first");

        pumps.stop().await;

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
        let mut pumps = ClipboardPumps::default();
        let mut releases = Vec::new();
        for _ in 0..MAX_CLIPBOARD_PUMPS {
            let (release, released) = tokio::sync::oneshot::channel::<()>();
            let slot = pumps.slot().expect("room under the cap");
            slot.start(async {
                let _ = released.await;
            });
            releases.push(release);
        }
        assert!(pumps.slot().is_none(), "no pump beyond the cap");

        drop(releases.pop());
        // Every ready task runs before this one resumes.
        tokio::task::yield_now().await;
        assert!(pumps.slot().is_some());
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
        let (guest, host) = (GuestId::for_test(5), HostId::for_test(5));
        ctx.send_host(wl_shm::req::create_pool(host, host, first, 4));
        for keymap in [second, third] {
            ctx.send_guest(wl_keyboard::evt::keymap(guest, 1, keymap, 0));
        }
        ctx.pending_params.insert(
            GuestId::for_test(7),
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
                attachment: SurfaceAttachment::Attach(GuestId::for_test(2)),
                surface_damage: vec![SurfaceRect::new(1, 2, 3, 4)].into(),
                buffer_damage: vec![BufferRect::new(5, 6, 7, 8)].into(),
                buffer_scale: Some(2),
                buffer_transform: Some(3),
                attach_offset: Some((11, 12)),
                viewport: Some(Some(ViewportState {
                    source: Some((0, 0, 256, 256)),
                    destination: Some((20, 30)),
                })),
            },
            logged_viewport_rule: false,
            new_host_copies: Default::default(),
        };
        surface.set_current_buffer_for_test(Some(GuestId::for_test(1)), Some((10, 20)));

        let commit = surface.prepare_commit();

        assert_eq!(
            commit.attachment,
            SurfaceAttachment::Attach(GuestId::for_test(2))
        );
        assert_eq!(commit.attached_buffer_id(), Some(GuestId::for_test(2)));
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
        assert_eq!(commit.buffer_offset, (11, 12));
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
                attach_offset: Some((4, 5)),
                viewport: Some(None),
            },
            logged_viewport_rule: false,
            new_host_copies: Default::default(),
        };
        surface.set_current_buffer_for_test(Some(GuestId::for_test(1)), Some((10, 20)));
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
        surface.set_current_buffer_for_test(Some(GuestId::for_test(1)), Some((100, 50)));

        let commit = surface.prepare_commit();

        assert_eq!(commit.attachment, SurfaceAttachment::Detach);
        assert_eq!(surface.current_buffer_id(), None);
        assert_eq!(surface.current_buffer_dimensions(), None);
    }

    #[test]
    fn content_snapshot_ignores_mismatched_destroy_generations() {
        let mut surface = SurfaceState::default();
        surface.set_current_buffer_for_test(Some(GuestId::for_test(7)), Some((100, 50)));

        surface.clear_buffer_references(GuestId::for_test(8));
        assert_eq!(surface.current_buffer_id(), Some(GuestId::for_test(7)));
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));

        surface.clear_buffer_references(GuestId::for_test(7));
        assert_eq!(surface.current_buffer_id(), None);
        assert_eq!(surface.current_buffer_dimensions(), Some((100, 50)));
        assert!(
            !surface.set_current_buffer_dimensions(GuestId::for_test(7), Some((1, 1))),
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
        let buffer = GuestId::for_test(20);
        let host_buffer = HostId::for_test(40);
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, Iface::WlBuffer);
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
            ctx.render_buffers.lifecycle(host_buffer),
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
        let buffer = GuestId::for_test(20);
        let host_buffer = HostId::for_test(40);
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, Iface::WlBuffer);
        assert!(ctx.register_native_buffer(host_buffer, (1, 1), Vec::new()));

        assert!(ctx.finalize_surface_attachment(None, Some(buffer)));
        assert!(ctx.finalize_surface_attachment(Some(buffer), Some(buffer)));
        assert_eq!(ctx.host_buffer_use(buffer), Some(awaiting_release(false)));
    }

    #[test]
    fn release_clears_the_detached_latch_before_a_new_submit() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let buffer = GuestId::for_test(20);
        let host_buffer = HostId::for_test(40);
        ctx.shadow_table
            .insert_paired_for_test(buffer, host_buffer, Iface::WlBuffer);
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
        let old = GuestId::for_test(20);
        let old_host = HostId::for_test(40);
        let new = GuestId::for_test(21);
        let new_host = HostId::for_test(41);
        for (guest, host) in [(old, old_host), (new, new_host)] {
            ctx.shadow_table
                .insert_paired_for_test(guest, host, Iface::WlBuffer);
            assert!(ctx.register_native_buffer(host, (1, 1), Vec::new()));
        }
        assert!(ctx.mark_buffer_submitted(old));
        assert!(ctx.render_buffers.mark_host_destroy_queued(new_host));

        assert!(!ctx.finalize_surface_attachment(Some(old), Some(new)));
        assert_eq!(ctx.host_buffer_use(old), Some(awaiting_release(false)));
        assert_eq!(
            ctx.render_buffers.lifecycle(new_host),
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
        let host_buffer = HostId::for_test(40);

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

    /// Whether a destroyed buffer waits to be retired follows every way an
    /// entry enters and leaves `GuestDestroyed`, so retiring returns early
    /// only when nothing waits.
    #[test]
    fn render_buffer_registry_knows_whether_a_destroyed_buffer_waits() {
        let (first, second) = (HostId::for_test(40), HostId::for_test(41));
        let mut registry = RenderBufferRegistry::default();
        assert!(registry.register_native(first, (1, 1), Vec::new()));
        assert!(registry.register_native(second, (1, 1), Vec::new()));
        assert!(!registry.has_guest_destroyed());

        assert!(registry.mark_guest_destroyed(first));
        assert!(!registry.mark_guest_destroyed(first));
        assert!(registry.has_guest_destroyed());
        assert!(registry.mark_host_destroy_queued(first));
        assert!(!registry.has_guest_destroyed());

        assert!(registry.mark_guest_destroyed(second));
        assert!(registry.has_guest_destroyed());
        assert!(registry.remove(second));
        assert!(!registry.has_guest_destroyed());
        assert!(registry.remove(first));
        assert!(!registry.has_guest_destroyed());
    }

    #[test]
    fn render_buffer_registry_rejects_duplicate_host_generation_without_mutation() {
        let host_id = HostId::for_test(40);
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
        let previous_guest = GuestId::for_test(20);
        let previous_host = HostId::for_test(40);
        let next_guest = GuestId::for_test(21);
        let next_host = HostId::for_test(41);
        let unregistered_guest = GuestId::for_test(22);
        let unregistered_host = HostId::for_test(42);
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        for (guest, host) in [
            (previous_guest, previous_host),
            (next_guest, next_host),
            (unregistered_guest, unregistered_host),
        ] {
            ctx.shadow_table
                .insert_paired_for_test(guest, host, Iface::WlBuffer);
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
        let host_id = HostId::for_test(40);

        for mut encoded in 0..sequence_count {
            let mut registry = RenderBufferRegistry::default();
            assert!(registry.register_native(host_id, (1, 1), Vec::new()));
            let mut ownership = ModelOwnership::GuestAlive;
            let mut use_state = RenderBufferUse::NeverSubmitted;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();

                let actual_changed = match operation {
                    Operation::Submit => registry.submit(host_id),
                    Operation::Release => registry.release(host_id),
                    Operation::Detach => registry.detach(host_id),
                    Operation::LastSurfaceDestroy => registry.end_last_surface_use(host_id, false),
                    Operation::GuestDestroy => registry.mark_guest_destroyed(host_id),
                    Operation::HostDestroy => registry.mark_host_destroy_queued(host_id),
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
                assert_eq!(registry.lifecycle(host_id), Some(expected));
                assert_eq!(
                    registry.dimensions(host_id).is_some(),
                    ownership != ModelOwnership::HostDestroyQueued
                );
            }
        }
    }
}
