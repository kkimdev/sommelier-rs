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
//!   outlives the guest object: a render buffer the host still shows.
//!
//! What a hook sends joins the current message's effects. A guest
//! destructor runs it before the transition, while the table still knows
//! the object; a `delete_id` runs it after, so a hook gets the object's
//! interface and IDs and never looks up its own entry.

use super::{Context, DeletedHostId, GuestId, HostId, Link, ObjectData};
use crate::handler::{
    compositor, data_control, keyboard, linux_dmabuf, output, pointer, registry, text_input,
    viewport, xdg_shell,
};
use crate::protocols::wayland::wl_display;
use crate::protocols::Iface;
use crate::wire::WireId;

impl Context {
    /// The guest destroyed guest object `guest`; see
    /// [`super::ShadowTable::guest_destroyed`].
    pub(crate) fn guest_destroyed(&mut self, guest: GuestId) {
        self.forget_live_paired(guest);
        self.shadow_table.guest_destroyed(guest);
    }

    /// The guest destroyed guest object `guest` while the host still uses
    /// its host object; see [`super::ShadowTable::guest_destroyed_in_use`].
    pub(crate) fn guest_destroyed_in_use(&mut self, guest: GuestId) {
        self.forget_live_paired(guest);
        self.shadow_table.guest_destroyed_in_use(guest);
    }

    /// Guest object `guest` ended, and its host object stays; see
    /// [`super::ShadowTable::guest_ended_host_kept`].
    pub(crate) fn guest_ended_host_kept(&mut self, guest: GuestId) -> bool {
        self.forget_live_paired(guest);
        self.shadow_table.guest_ended_host_kept(guest)
    }

    /// Guest object `guest` ended with its host object; see
    /// [`super::ShadowTable::guest_ended_host_destroyed`].
    pub(crate) fn guest_ended_host_destroyed(&mut self, guest: GuestId) {
        self.forget_live_paired(guest);
        self.shadow_table.guest_ended_host_destroyed(guest);
    }

    /// Local guest object `guest` ended: its destructor completed in the
    /// proxy, which queues its `delete_id`, as its host would for a host
    /// object, unless the proxy created it under a server ID, which
    /// libwayland frees without one; see
    /// [`super::ShadowTable::local_destroyed`].
    ///
    /// # Returns
    ///
    /// `false`, changing nothing, unless `guest` is a live local object.
    pub(crate) fn local_destroyed(&mut self, guest: GuestId) -> bool {
        if self.shadow_table.link(guest) != Some(Link::Local) {
            return false;
        }
        self.forget(guest, None);
        let server_id = self.shadow_table.is_guest_server_id(guest);
        let ended = self.shadow_table.local_destroyed(guest);
        debug_assert!(ended, "a live local object ends");
        if !server_id {
            self.queue_delete_id(guest);
        }
        true
    }

    /// Queue the `wl_display.delete_id` that ends guest object `guest`, for
    /// an object whose host object cannot send it: one the proxy
    /// implements, or one backed by a host object without a destructor.
    /// The caller ends the object in the [`super::ShadowTable`] first.
    pub(crate) fn queue_delete_id(&mut self, guest: GuestId) {
        // `delete_id` names the ended object as a `uint`, not as an object.
        self.send_guest(wl_display::evt::delete_id(GuestId::DISPLAY, guest.wire()));
    }

    /// Guest object `guest` never reached the host; see
    /// [`super::ShadowTable::abandon`].
    pub(crate) fn abandon(&mut self, guest: GuestId) {
        self.forget_live_paired(guest);
        self.shadow_table.abandon(guest);
    }

    /// Apply the host's `wl_display.delete_id` for `host_id`, a wire value;
    /// see [`super::ShadowTable::release_deleted_host_id`].
    pub(crate) fn release_deleted_host_id(&mut self, host_id: u32) -> DeletedHostId {
        let guest = self.shadow_table.guest_named_by_host(host_id);
        let interface = guest.and_then(|guest| self.shadow_table.get_interface(guest));
        let (deleted, data) = self.shadow_table.release_deleted_host_id(host_id);
        match deleted {
            DeletedHostId::Ignored => {}
            DeletedHostId::HostOnly { host } => forget_host_id(self, host),
            DeletedHostId::Paired {
                host,
                guest,
                was_live,
            } => {
                // A live guest object ends here, so the table hands back
                // the data it kept; one the guest already destroyed kept
                // none, and an ignored `delete_id` never took any.
                if let (true, Some(interface)) = (was_live, interface) {
                    forget_object(self, interface, guest, Some(host), true, data);
                }
                forget_host_id(self, host);
            }
        }
        deleted
    }

    /// Forget guest object `guest` if it is live and paired, the only
    /// objects the paired transitions end: a local object keeps its state
    /// until it ends itself.
    fn forget_live_paired(&mut self, guest: GuestId) {
        if let Some(Link::Host(host)) = self.shadow_table.link(guest) {
            self.forget(guest, Some(host));
        }
    }

    /// Forget live guest object `guest`, paired with `host` or local.
    fn forget(&mut self, guest: GuestId, host: Option<HostId>) {
        if let Some(interface) = self.shadow_table.get_interface(guest) {
            let data = self.shadow_table.take_data(guest);
            forget_object(self, interface, guest, host, false, data);
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
/// `data` is what the object's table entry kept
/// ([`super::object_data`]), which ends here: dropped, or handed to its
/// owner. The match names every interface, so a new one must say whether
/// its objects have state of their own outside their entry. A
/// `wl_buffer`'s state is its render buffer, keyed by host ID
/// ([`forget_host_id`]); an inert `wl_output`, a local one, has none.
fn forget_object(
    ctx: &mut Context,
    interface: Iface,
    guest: GuestId,
    host: Option<HostId>,
    host_deleted: bool,
    data: ObjectData,
) {
    // The data whose end does more than drop it, and the data that ends
    // with its entry.
    match data {
        ObjectData::DmabufFactory(factory) => linux_dmabuf::forget_factory(ctx, factory),
        ObjectData::DmabufFeedback(feedback) => linux_dmabuf::forget_feedback(ctx, guest, feedback),
        ObjectData::Viewport(viewport) => viewport::forget_viewport(ctx, viewport),
        ObjectData::DataControlSource(source) => data_control::forget_source(ctx, source),
        ObjectData::None
        | ObjectData::Shm(_)
        | ObjectData::ShmPool(_)
        | ObjectData::Params(_)
        | ObjectData::GtkShell(_)
        | ObjectData::GtkSurface(_)
        | ObjectData::DataControlDevice(_)
        | ObjectData::DataControlOffer(_)
        | ObjectData::HostOfferTypes(_)
        | ObjectData::SecurityContext(_) => {}
    }
    match (interface, host) {
        (Iface::WlRegistry, Some(host)) => registry::forget_registry(ctx, host),
        (Iface::WlOutput, Some(host)) => output::forget_output(ctx, host),
        (Iface::WlKeyboard, Some(host)) => keyboard::forget_keyboard(ctx, host),
        (Iface::WlPointer, Some(host)) => pointer::forget_pointer(ctx, host),
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
        (Iface::XdgSurface, _) => xdg_shell::forget_xdg_surface(ctx, guest),
        (Iface::XdgToplevel, _) => xdg_shell::forget_xdg_toplevel(ctx, guest),
        (Iface::ZwpTextInputV3, _) => text_input::forget_text_input(ctx, guest, host_deleted),
        (Iface::WlDataOffer, Some(host)) => data_control::forget_host_offer(ctx, host),
        // No state of their own outside their entry, or none that ends
        // here: GTK shells and surfaces, which have no destructor, last
        // the connection.
        (
            Iface::WlDisplay
            | Iface::WlShm
            | Iface::WlShmPool
            | Iface::ZwpLinuxDmabufV1
            | Iface::ZwpLinuxDmabufFeedbackV1
            | Iface::ZwpLinuxBufferParamsV1
            | Iface::WpViewport
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
            | Iface::ZauraPopup
            | Iface::ExtDataControlManagerV1
            | Iface::ExtDataControlDeviceV1
            | Iface::ExtDataControlSourceV1
            | Iface::ExtDataControlOfferV1
            | Iface::WpSecurityContextManagerV1
            | Iface::WpSecurityContextV1,
            _,
        ) => {}
    }
}

/// Forget the state of host ID `host`, which the host released: what
/// outlived its guest object.
fn forget_host_id(ctx: &mut Context, host: HostId) {
    ctx.render_buffers.remove(host);
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
        let guest = |what: &str, id: GuestId, interface: Iface| {
            assert!(
                table.live_object(id, interface).is_some(),
                "{what} outlived guest {interface} {id}"
            );
        };
        let host = |what: &str, id: HostId, interface: Iface| {
            assert!(
                table
                    .get_guest_id(id)
                    .is_some_and(|paired| table.live_object(paired, interface).is_some()),
                "{what} outlived host {interface} {id}"
            );
        };
        let host_only = |what: &str, id: HostId, interface: Iface| {
            assert!(
                table.get_host_interface(id) == Some(interface) && table.get_guest_id(id).is_none(),
                "{what} outlived host-only {interface} {id}"
            );
        };

        // Surfaces, their roles and what they show.
        for &surface in self.surfaces.keys() {
            guest("wl_surface state", surface, Iface::WlSurface);
        }
        // A viewport's or GTK surface's link to its wl_surface ends with
        // that surface.
        for (_, viewport) in table.data_of::<crate::handler::viewport::Viewport>() {
            if let Some(surface) = viewport.surface_for_test() {
                guest("viewport link", surface, Iface::WlSurface);
            }
        }
        for (_, gtk_surface) in table.data_of::<crate::handler::gtk_shell::GtkSurface>() {
            if let Some(surface) = gtk_surface.wl_surface_for_test() {
                guest("GTK surface", surface, Iface::WlSurface);
            }
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

        // The selections the connection carries for data-control sources.
        for source in self.data_control.carried_for_test() {
            host_only("carried selection", source, Iface::WlDataSource);
        }
        if let Some(offer) = self.data_control.selection_offer_for_test() {
            host("host clipboard", offer, Iface::WlDataOffer);
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
            host("keyboard record", keyboard, Iface::WlKeyboard);
            assert_eq!(
                table.get_guest_id(keyboard),
                Some(record.guest),
                "keyboard record {} names another guest keyboard",
                keyboard
            );
            if let Some(extended) = record.extended {
                host_only("extended keyboard", extended, Iface::ZcrExtendedKeyboardV1);
            }
        }
        for keyboard in self.keys.key_state_keyboards() {
            host("key generations", keyboard, Iface::WlKeyboard);
        }
        // A seat reference names the guest wl_seat that its keyboard or
        // text input was created from, and outlives that seat, whose ID the
        // guest may then reuse for any object (INVARIANTS "Object
        // lifetimes"). So a keyboard's seat and a text input's seat are not
        // checked against the table, and every copy of a keyboard's seat
        // must name it.
        let keyboard_seat = |keyboard: HostId| self.keys.keyboards().seat(keyboard);
        for (keyboard, focus) in self.keys.focus().focuses_for_test() {
            host("keyboard focus", keyboard, Iface::WlKeyboard);
            guest("keyboard focus", focus.guest_surface, Iface::WlSurface);
            assert_eq!(
                keyboard_seat(keyboard),
                Some(focus.guest_seat),
                "keyboard focus names a seat other than keyboard {}'s",
                keyboard
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
                keyboard
            );
        }
        if let Some((text_input, keyboard, host_keyboard)) = self.keys.recovery_objects() {
            guest("pending recovery", text_input, Iface::ZwpTextInputV3);
            guest("pending recovery", keyboard, Iface::WlKeyboard);
            host("pending recovery", host_keyboard, Iface::WlKeyboard);
        }
        for (pointer, surface) in self.pointer_focus.focuses_for_test() {
            host("pointer focus", pointer, Iface::WlPointer);
            guest("pointer focus", surface, Iface::WlSurface);
        }
        for pointer in self.wheel_steps.pointers_for_test() {
            host("wheel steps", pointer, Iface::WlPointer);
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
        for (buffer, _) in self.render_buffer_lifecycles() {
            assert!(
                !table.is_host_id_available(buffer),
                "render buffer outlived host ID {}",
                buffer
            );
        }
        linux_dmabuf::assert_no_orphans(self);

        // Registries.
        self.globals.assert_no_orphans(table);
    }
}
