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

//! How per-object state ends with its object.
//!
//! An object can end in several ways: the guest's destructor, a terminal
//! event, an object that never reached the host, or a host
//! `wl_display.delete_id`, which the host may send for a live object of the
//! guest's (INVARIANTS "Object lifetimes"). Whatever the way, the
//! [`Context`] wrapper of the [`super::ShadowTable`] transition runs the
//! one hook for the state the object leaves behind:
//!
//! - [`forget_object`] runs exactly once per object, at its first end,
//!   such as a guest destructor, or a host `delete_id` while the guest
//!   object was live. A later `delete_id` acknowledging the destructor
//!   forgets nothing again.
//! - [`forget_host_id`] runs when the host ID is released, for state that
//!   outlives the guest object: a render buffer the host still shows. It
//!   also drops an asynchronous dma-buf create, as a backstop: forgetting
//!   its params at their end already did.
//!
//! What a hook sends joins the current message's effects. A guest
//! destructor runs it before the transition, while the table still knows
//! the object; a `delete_id` runs it after, so a hook gets the object's
//! interface and IDs and never looks up its own entry.

use super::{Context, DeletedHostId, HostId, Link};
use crate::handler::{
    compositor, keyboard, linux_dmabuf, output, pointer, registry, shm, text_input,
};
use crate::protocols::wayland::wl_display;
use crate::protocols::Iface;
use crate::wire::WL_DISPLAY_ID;

impl Context {
    /// The guest destroyed guest object `guest_id`; see
    /// [`super::ShadowTable::guest_destroyed`].
    pub(crate) fn guest_destroyed(&mut self, guest_id: u32) {
        self.forget_live_paired(guest_id);
        self.shadow_table.guest_destroyed(guest_id);
    }

    /// The guest destroyed guest object `guest_id` while the host still uses
    /// its host object; see [`super::ShadowTable::guest_destroyed_in_use`].
    pub(crate) fn guest_destroyed_in_use(&mut self, guest_id: u32) {
        self.forget_live_paired(guest_id);
        self.shadow_table.guest_destroyed_in_use(guest_id);
    }

    /// Guest object `guest_id` ended, and its host object stays; see
    /// [`super::ShadowTable::guest_ended_host_kept`].
    pub(crate) fn guest_ended_host_kept(&mut self, guest_id: u32) -> bool {
        self.forget_live_paired(guest_id);
        self.shadow_table.guest_ended_host_kept(guest_id)
    }

    /// Guest object `guest_id` ended with its host object; see
    /// [`super::ShadowTable::guest_ended_host_destroyed`].
    pub(crate) fn guest_ended_host_destroyed(&mut self, guest_id: u32) {
        self.forget_live_paired(guest_id);
        self.shadow_table.guest_ended_host_destroyed(guest_id);
    }

    /// Local guest object `guest_id` ended: its destructor completed in the
    /// proxy, which queues its `delete_id`, as its host would for a host
    /// object; see [`super::ShadowTable::local_destroyed`].
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest_id` is a live local object.
    pub(crate) fn local_destroyed(&mut self, guest_id: u32) -> bool {
        if self.shadow_table.request_sender(guest_id) != Some(Link::Local) {
            return false;
        }
        self.forget(guest_id, None);
        let ended = self.shadow_table.local_destroyed(guest_id);
        debug_assert!(ended, "a live local object ends");
        self.queue_delete_id(guest_id);
        true
    }

    /// Queue the `wl_display.delete_id` that ends guest object `guest_id`,
    /// for an object whose host object cannot send it: one the proxy
    /// implements, or one backed by a host object without a destructor.
    /// The caller ends the object in the [`super::ShadowTable`] first.
    pub(crate) fn queue_delete_id(&mut self, guest_id: u32) {
        self.host_to_client_queue
            .push(wl_display::evt::delete_id(WL_DISPLAY_ID, guest_id));
    }

    /// Guest object `guest_id` never reached the host; see
    /// [`super::ShadowTable::abandon`].
    pub(crate) fn abandon(&mut self, guest_id: u32) {
        self.forget_live_paired(guest_id);
        self.shadow_table.abandon(guest_id);
    }

    /// Apply the host's `wl_display.delete_id` for `host_id`; see
    /// [`super::ShadowTable::release_deleted_host_id`].
    pub(crate) fn release_deleted_host_id(&mut self, host_id: u32) -> DeletedHostId {
        let interface = self
            .shadow_table
            .get_guest_id(host_id)
            .and_then(|guest| self.shadow_table.get_interface(guest));
        let deleted = self.shadow_table.release_deleted_host_id(host_id);
        if let (
            DeletedHostId::Paired {
                guest,
                was_live: true,
            },
            Some(interface),
        ) = (deleted, interface)
        {
            forget_object(self, interface, guest, Some(host_id), true);
        }
        if deleted != DeletedHostId::Ignored {
            forget_host_id(self, HostId(host_id));
        }
        deleted
    }

    /// Forget guest object `guest_id` if it is live and paired, the only
    /// objects the paired transitions end: a local object keeps its state
    /// until it ends itself.
    fn forget_live_paired(&mut self, guest_id: u32) {
        if let Some(Link::Host(host_id)) = self.shadow_table.request_sender(guest_id) {
            self.forget(guest_id, Some(host_id));
        }
    }

    /// Forget live guest object `guest_id`, paired with `host_id` or local.
    fn forget(&mut self, guest_id: u32, host_id: Option<u32>) {
        if let Some(interface) = self.shadow_table.get_interface(guest_id) {
            forget_object(self, interface, guest_id, host_id, false);
        }
    }
}

/// Forget the state of guest object `guest`, of `interface`, at its first
/// end, through the owner of each interface's state. `host` is its host
/// object; a local object has none. `host_deleted` says the host deleted
/// that object, so nothing may be sent to it. Only the text-input arm reads
/// it, since only a text input sends to its own host object as it ends (a
/// `deactivate`); the other arms send only to other objects, such as Aura
/// children or an extended keyboard, which the host's delete leaves live.
///
/// The match names every interface, so a new one must say whether its
/// objects have state of their own. A `wl_buffer`'s state is its render
/// buffer, keyed by host ID ([`forget_host_id`]); an inert `wl_output`, a
/// local one, has none.
fn forget_object(
    ctx: &mut Context,
    interface: Iface,
    guest: u32,
    host: Option<u32>,
    host_deleted: bool,
) {
    match (interface, host) {
        (Iface::WlRegistry, Some(host)) => registry::forget_registry(ctx, host),
        (Iface::WlOutput, Some(host)) => output::forget_output(ctx, host),
        (Iface::WlKeyboard, Some(host)) => keyboard::forget_keyboard(ctx, HostId(host)),
        (Iface::WlPointer, Some(host)) => pointer::forget_pointer(ctx, HostId(host)),
        (Iface::WlSurface, Some(host)) => compositor::forget_surface(ctx, guest, host),
        // Local objects of these interfaces have no state.
        (
            Iface::WlRegistry
            | Iface::WlOutput
            | Iface::WlKeyboard
            | Iface::WlPointer
            | Iface::WlSurface,
            None,
        ) => {}
        (Iface::WpViewport, _) => compositor::forget_viewport(ctx, guest),
        (Iface::XdgSurface, _) => compositor::forget_xdg_surface(ctx, guest),
        (Iface::XdgToplevel, _) => compositor::forget_xdg_toplevel(ctx, guest),
        (Iface::WlShm, _) => shm::forget_shm(ctx, guest),
        (Iface::WlShmPool, _) => shm::forget_pool(ctx, guest),
        (Iface::ZwpLinuxDmabufV1, _) => linux_dmabuf::forget_factory(ctx, guest),
        (Iface::ZwpLinuxDmabufFeedbackV1, _) => linux_dmabuf::forget_feedback(ctx, guest),
        (Iface::ZwpLinuxBufferParamsV1, _) => linux_dmabuf::forget_params(ctx, guest, host),
        (Iface::ZwpTextInputV3, _) => text_input::forget_text_input(ctx, guest, host_deleted),
        // No state of their own, or none that ends here: a GTK surface's
        // ends with its `wl_surface` (`compositor::forget_surface`), and a
        // GTK shell, which the guest cannot destroy, lasts the connection.
        (
            Iface::WlDisplay
            | Iface::WlCallback
            | Iface::WlCompositor
            | Iface::WlBuffer
            | Iface::WlDataOffer
            | Iface::WlDataSource
            | Iface::WlDataDevice
            | Iface::WlDataDeviceManager
            | Iface::WlShell
            | Iface::WlShellSurface
            | Iface::WlSeat
            | Iface::WlTouch
            | Iface::WlRegion
            | Iface::WlSubcompositor
            | Iface::WlSubsurface
            | Iface::WlFixes
            | Iface::XdgWmBase
            | Iface::XdgPositioner
            | Iface::XdgPopup
            | Iface::WpViewporter
            | Iface::ZwpTextInputManagerV3
            | Iface::ZwpTextInputV1
            | Iface::ZwpTextInputManagerV1
            | Iface::ZcrTextInputExtensionV1
            | Iface::ZcrExtendedTextInputV1
            | Iface::ZxdgDecorationManagerV1
            | Iface::ZxdgToplevelDecorationV1
            | Iface::WpFractionalScaleManagerV1
            | Iface::WpFractionalScaleV1
            | Iface::ZxdgOutputManagerV1
            | Iface::ZxdgOutputV1
            | Iface::ZcrKeyboardExtensionV1
            | Iface::ZcrExtendedKeyboardV1
            | Iface::GtkShell1
            | Iface::GtkSurface1
            | Iface::ZauraShell
            | Iface::ZauraSurface
            | Iface::ZauraOutput
            | Iface::ZauraToplevel
            | Iface::ZauraPopup,
            _,
        ) => {}
    }
}

/// Forget the state of host ID `host`, which the host released: what
/// outlived its guest object, and an asynchronous dma-buf create, which
/// `forget_params` should have dropped already.
fn forget_host_id(ctx: &mut Context, host: HostId) {
    ctx.render_buffers.remove(host);
    ctx.pending_native_creates.remove(&host);
}

#[cfg(test)]
impl Context {
    /// Panic if per-object state outlived its object: every key of a map
    /// that holds state for an object, and every object such state names,
    /// must be a live object of the right interface in the
    /// [`super::ShadowTable`].
    ///
    /// A guest object must be live (local, or paired and not destroyed). A
    /// host object must be paired with one, or, if only the proxy uses it,
    /// be live itself. A render buffer, which the host may still show after
    /// its guest object ends, only needs its host ID to be in use, retired
    /// included. A seat reference outlives its seat, so only its copies
    /// are checked, against their keyboard's.
    pub(crate) fn assert_no_orphans(&self) {
        let table = &self.shadow_table;
        let guest = |what: &str, id: u32, interface: Iface| {
            assert!(
                table.request_object(id, interface).is_some(),
                "{what} outlived guest {interface} {id}"
            );
        };
        let host = |what: &str, id: u32, interface: Iface| {
            assert!(
                table
                    .get_guest_id(id)
                    .is_some_and(|paired| table.request_object(paired, interface).is_some()),
                "{what} outlived host {interface} {id}"
            );
        };
        let host_only = |what: &str, id: u32, interface: Iface| {
            assert!(
                table.get_host_interface(id) == Some(interface) && table.get_guest_id(id).is_none(),
                "{what} outlived host-only {interface} {id}"
            );
        };

        // Surfaces, their roles and what they show.
        for &surface in self.surfaces.keys() {
            guest("wl_surface state", surface, Iface::WlSurface);
        }
        for (&viewport, &surface) in &self.viewport_to_wl_surface {
            guest("viewport link", viewport, Iface::WpViewport);
            guest("viewport link", surface, Iface::WlSurface);
        }
        for &gtk_shell in self.gtk_shells.keys() {
            guest("GTK shell", gtk_shell, Iface::GtkShell1);
        }
        for (&gtk_surface, state) in &self.gtk_surfaces {
            guest("GTK surface", gtk_surface, Iface::GtkSurface1);
            guest("GTK surface", state.wl_surface_id, Iface::WlSurface);
        }
        for (xdg_surface, wl_surface, toplevel) in self.xdg_roles.roles_for_test() {
            guest("xdg role", xdg_surface, Iface::XdgSurface);
            guest("xdg role", wl_surface, Iface::WlSurface);
            if let Some(toplevel) = toplevel {
                guest("xdg role", toplevel, Iface::XdgToplevel);
            }
        }
        for surface in self.aura_surfaces.wl_surfaces_for_test() {
            host("Aura surface", surface, Iface::WlSurface);
        }
        for surface in self.render_buffers.copy_surfaces_for_test() {
            guest("buffer copy", surface, Iface::WlSurface);
        }
        let outputs = self.outputs.live_outputs();
        for &output in &outputs {
            host("output record", output, Iface::WlOutput);
        }
        for (surface, output) in self.outputs.surface_outputs_for_test() {
            host("surface output", surface, Iface::WlSurface);
            assert!(
                outputs.contains(&output),
                "surface output outlived output {output}"
            );
        }

        // Window placement.
        if let Some(placement) = self.placement() {
            placement.assert_no_orphans(&guest, &host_only);
        }

        // Keyboards, their focus and keys, and text input.
        // A keyboard's record holds everything of the keyboard itself, so
        // its host object and the guest one paired with it are all there is
        // to check; its extended keyboard is host-only.
        for (keyboard, record) in self.keys.keyboards().iter() {
            host("keyboard record", keyboard.0, Iface::WlKeyboard);
            assert_eq!(
                table.get_guest_id(keyboard.0),
                Some(record.guest),
                "keyboard record {} names another guest keyboard",
                keyboard.0
            );
            if let Some(extended) = record.extended {
                host_only(
                    "extended keyboard",
                    extended.0,
                    Iface::ZcrExtendedKeyboardV1,
                );
            }
        }
        for keyboard in self.keys.key_state_keyboards() {
            host("key generations", keyboard.0, Iface::WlKeyboard);
        }
        // A seat reference names the guest wl_seat that its keyboard or
        // text input was created from, and outlives that seat, whose ID the
        // guest may then reuse for any object (INVARIANTS "Object
        // lifetimes"). So a keyboard's seat and a text input's seat are not
        // checked against the table, and every copy of a keyboard's seat
        // must name it.
        let keyboard_seat = |keyboard: HostId| self.keys.keyboards().seat(keyboard);
        for (keyboard, focus) in self.keyboard_focus.focuses_for_test() {
            host("keyboard focus", keyboard.0, Iface::WlKeyboard);
            guest("keyboard focus", focus.guest_surface, Iface::WlSurface);
            assert_eq!(
                keyboard_seat(keyboard),
                Some(focus.guest_seat),
                "keyboard focus names a seat other than keyboard {}'s",
                keyboard.0
            );
        }
        for (seat, surface, keyboard) in self.keys.peek_watermarks() {
            if let Some(surface) = surface {
                guest("peek watermark", surface, Iface::WlSurface);
            }
            assert_eq!(
                keyboard_seat(keyboard),
                Some(seat),
                "peek watermark names a seat other than keyboard {}'s",
                keyboard.0
            );
        }
        if let Some((text_input, keyboard, host_keyboard)) = self.keys.recovery_objects() {
            guest("pending recovery", text_input, Iface::ZwpTextInputV3);
            guest("pending recovery", keyboard, Iface::WlKeyboard);
            host("pending recovery", host_keyboard.0, Iface::WlKeyboard);
        }
        for (pointer, surface) in self.pointer_focus.focuses_for_test() {
            host("pointer focus", pointer.0, Iface::WlPointer);
            guest("pointer focus", surface, Iface::WlSurface);
        }
        for pointer in self.wheel_steps.pointers_for_test() {
            host("wheel steps", pointer.0, Iface::WlPointer);
        }
        for (text_input, t) in self.ime.iter() {
            guest("text input state", text_input.0, Iface::ZwpTextInputV3);
            // I5: a drain or a fence that outlived its callback would drop
            // the field's composition events for good.
            for token in t.barriers() {
                assert!(
                    self.internal_callbacks
                        .awaits(super::InternalCallback::TextInput { token })
                        .is_some(),
                    "text input {} keeps barrier {token:?}, whose callback is not pending",
                    text_input.0
                );
            }
            if let Some(surface) = t.focused_surface() {
                guest("text input focus", surface.0, Iface::WlSurface);
            }
        }
        self.ime
            .assert_invariants(&crate::handler::text_input::CtxEnv::of(self));

        // SHM and dma-buf.
        for &pool in self.pools.keys().chain(&self.stale_shm_pools) {
            guest("SHM pool", pool, Iface::WlShmPool);
        }
        for &shm in self.guest_shms.keys().chain(&self.stale_shm_guest_objects) {
            guest("wl_shm state", shm, Iface::WlShm);
        }
        for (buffer, _) in self.render_buffer_lifecycles() {
            assert!(
                !table.is_host_id_available(buffer.0),
                "render buffer outlived host ID {}",
                buffer.0
            );
        }
        for (&params, create) in &self.pending_native_creates {
            assert_eq!(
                table.event_object(
                    params.0,
                    Iface::ZwpLinuxBufferParamsV1,
                    "zwp_linux_buffer_params_v1.created"
                ),
                Some(create.guest_params_id),
                "asynchronous dma-buf create outlived host params {}",
                params.0
            );
        }
        for &params in self.pending_params.keys().chain(&self.used_dmabuf_params) {
            guest("dma-buf params", params, Iface::ZwpLinuxBufferParamsV1);
        }
        for &factory in self.dmabuf_guest_generations.keys() {
            guest("dma-buf factory", factory, Iface::ZwpLinuxDmabufV1);
        }
        for &feedback in self.synthetic_feedback_objects.keys() {
            guest(
                "dma-buf feedback",
                feedback,
                Iface::ZwpLinuxDmabufFeedbackV1,
            );
        }
        for &generation in self.dmabuf_capabilities.keys() {
            assert!(
                linux_dmabuf::capability_generation_held(self, generation),
                "dma-buf capabilities outlived every hold on generation {generation}"
            );
        }

        // Registries.
        let registries = self.registries.keys().chain(
            self.pending_dmabuf_globals
                .iter()
                .map(|pending| &pending.registry_host_id),
        );
        for &registry in registries {
            host("registry globals", registry, Iface::WlRegistry);
        }
        // A singleton candidate is a name a registry still lists.
        for &name in self.singleton_candidates.keys() {
            assert!(
                registry::global_still_seen(
                    self,
                    name,
                    self.global_generations.get(&name).copied()
                ),
                "singleton candidate {name} outlived every registry that lists it"
            );
        }
        // A removal on record is of the global's current generation, and
        // waits for a registry that still lists it; a global the proxy
        // drops is in no registry's record.
        for (&name, &generation) in &self.removed_global_generations {
            assert_eq!(
                self.global_generations.get(&name),
                Some(&generation),
                "removal of global {name} outlived its generation {generation}"
            );
            assert!(
                registry::global_still_seen(self, name, Some(generation)),
                "removal of global {name} outlived every registry that lists it"
            );
        }
    }
}
