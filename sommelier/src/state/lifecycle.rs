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
use crate::handler::{compositor, keyboard, linux_dmabuf, output, registry, shm, text_input};

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

    /// Local guest object `guest_id` ended; see
    /// [`super::ShadowTable::local_destroyed`].
    pub(crate) fn local_destroyed(&mut self, guest_id: u32) -> bool {
        if self.shadow_table.request_sender(guest_id) != Some(Link::Local) {
            return false;
        }
        self.forget(guest_id, None);
        self.shadow_table.local_destroyed(guest_id)
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
            .and_then(|guest| self.shadow_table.get_interface(guest))
            .map(str::to_owned);
        let deleted = self.shadow_table.release_deleted_host_id(host_id);
        if let (
            DeletedHostId::Paired {
                guest,
                was_live: true,
            },
            Some(interface),
        ) = (deleted, interface)
        {
            forget_object(self, &interface, guest, Some(host_id), true);
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
            let interface = interface.to_owned();
            forget_object(self, &interface, guest_id, host_id, false);
        }
    }
}

/// Forget the state of guest object `guest`, of `interface`, at its first
/// end, through the owner of each interface's state. `host` is its host
/// object; a local object has none. `host_deleted` says the host deleted
/// that object, so nothing may be sent to it.
///
/// Every interface with per-object state has an arm. A `wl_buffer`'s state
/// is its render buffer, keyed by host ID ([`forget_host_id`]), and an
/// inert `wl_output` has none.
fn forget_object(
    ctx: &mut Context,
    interface: &str,
    guest: u32,
    host: Option<u32>,
    host_deleted: bool,
) {
    match (interface, host) {
        ("wl_registry", Some(host)) => registry::forget_registry(ctx, host),
        ("wl_output", Some(host)) => output::forget_output(ctx, host),
        ("wl_keyboard", Some(host)) => keyboard::forget_keyboard(ctx, guest, HostId(host)),
        ("wl_surface", Some(host)) => compositor::forget_surface(ctx, guest, host),
        ("wp_viewport", _) => compositor::forget_viewport(ctx, guest),
        ("xdg_surface", _) => compositor::forget_xdg_surface(ctx, guest),
        ("xdg_toplevel", _) => compositor::forget_xdg_toplevel(ctx, guest),
        ("wl_shm", _) => shm::forget_shm(ctx, guest),
        ("wl_shm_pool", _) => shm::forget_pool(ctx, guest),
        ("zwp_linux_dmabuf_v1", _) => linux_dmabuf::forget_factory(ctx, guest),
        ("zwp_linux_dmabuf_feedback_v1", _) => linux_dmabuf::forget_feedback(ctx, guest),
        ("zwp_linux_buffer_params_v1", _) => linux_dmabuf::forget_params(ctx, guest, host),
        ("zwp_text_input_v3", _) => text_input::forget_text_input(ctx, guest, host_deleted),
        _ => {}
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
        let guest = |what: &str, id: u32, interface: &str| {
            assert!(
                table.request_object(id, interface).is_some(),
                "{what} outlived guest {interface} {id}"
            );
        };
        let host = |what: &str, id: u32, interface: &str| {
            assert!(
                table
                    .get_guest_id(id)
                    .is_some_and(|paired| table.request_object(paired, interface).is_some()),
                "{what} outlived host {interface} {id}"
            );
        };
        let host_only = |what: &str, id: u32, interface: &str| {
            assert!(
                table.get_host_interface(id) == Some(interface) && table.get_guest_id(id).is_none(),
                "{what} outlived host-only {interface} {id}"
            );
        };

        // Surfaces, their roles and what they show.
        for &surface in self.surfaces.keys() {
            guest("wl_surface state", surface, "wl_surface");
        }
        for (&viewport, &surface) in &self.viewport_to_wl_surface {
            guest("viewport link", viewport, "wp_viewport");
            guest("viewport link", surface, "wl_surface");
        }
        for (&gtk_shell, state) in &self.gtk_shells {
            guest("GTK shell", gtk_shell, "gtk_shell1");
            for &gtk_surface in &state.surfaces {
                guest("GTK shell surface", gtk_surface, "gtk_surface1");
            }
        }
        for (&gtk_surface, state) in &self.gtk_surfaces {
            guest("GTK surface", gtk_surface, "gtk_surface1");
            guest("GTK surface", state.wl_surface_id, "wl_surface");
        }
        for (xdg_surface, wl_surface, toplevel) in self.xdg_roles.roles_for_test() {
            guest("xdg role", xdg_surface, "xdg_surface");
            guest("xdg role", wl_surface, "wl_surface");
            if let Some(toplevel) = toplevel {
                guest("xdg role", toplevel, "xdg_toplevel");
            }
        }
        for surface in self.aura_surfaces.wl_surfaces_for_test() {
            host("Aura surface", surface, "wl_surface");
        }
        for surface in self.render_buffers.copy_surfaces_for_test() {
            guest("buffer copy", surface, "wl_surface");
        }
        let outputs = self.outputs.live_outputs();
        for &output in &outputs {
            host("output record", output, "wl_output");
        }
        for (surface, output) in self.outputs.surface_outputs_for_test() {
            host("surface output", surface, "wl_surface");
            assert!(
                outputs.contains(&output),
                "surface output outlived output {output}"
            );
        }

        // Window placement.
        for surface in self.window_placement.arc_surfaces_for_test() {
            guest("ARC identity", surface, "wl_surface");
        }
        let mut aura_toplevels = std::collections::HashSet::new();
        for (toplevel, aura_toplevel) in self.window_placement.aura_toplevels_for_test() {
            guest("Aura toplevel", toplevel, "xdg_toplevel");
            host_only("Aura toplevel", aura_toplevel, "zaura_toplevel");
            aura_toplevels.insert(aura_toplevel);
        }
        for aura_toplevel in self.window_placement.barrier_toplevels_for_test() {
            assert!(
                aura_toplevels.contains(&aura_toplevel),
                "placement barrier outlived Aura toplevel {aura_toplevel}"
            );
        }

        // Keyboards, their focus and keys, and text input.
        for &keyboard in self.keyboard_to_seat.keys() {
            guest("keyboard seat", keyboard, "wl_keyboard");
        }
        let host_keyboards = self
            .keyboard_to_extended_keyboard
            .keys()
            .chain(self.extended_keyboard_to_keyboard.values())
            .chain(self.keyboard_repeatable_keys.keys())
            .chain(self.keyboard_keysym_to_keycode.keys())
            .chain(self.keyboard_keymaps.keys())
            .chain(self.keyboard_xkb_states.keys())
            .chain(self.keyboard_modifiers.keys())
            .copied()
            .chain(self.key_generations.keyboards_for_test());
        for keyboard in host_keyboards {
            host("keyboard state", keyboard.0, "wl_keyboard");
        }
        // A seat reference names the guest wl_seat that its keyboard or
        // text input was created from, and outlives that seat, whose ID the
        // guest may then reuse for any object (INVARIANTS "Object
        // lifetimes"). So `keyboard_to_seat` and a text input's seat are
        // not checked against the table, and every copy of a keyboard's
        // seat must name it.
        let keyboard_seat = |keyboard: HostId| {
            table
                .get_guest_id(keyboard.0)
                .and_then(|keyboard| self.keyboard_to_seat.get(&keyboard))
                .copied()
        };
        for (keyboard, focus) in self.keyboard_focus.focuses_for_test() {
            host("keyboard focus", keyboard.0, "wl_keyboard");
            guest("keyboard focus", focus.guest_surface, "wl_surface");
            assert_eq!(
                keyboard_seat(keyboard),
                Some(focus.guest_seat),
                "keyboard focus names a seat other than keyboard {}'s",
                keyboard.0
            );
        }
        for (seat, surface, keyboard) in self.key_generations.peek_watermarks_for_test() {
            if let Some(surface) = surface {
                guest("peek watermark", surface, "wl_surface");
            }
            assert_eq!(
                keyboard_seat(keyboard),
                Some(seat),
                "peek watermark names a seat other than keyboard {}'s",
                keyboard.0
            );
        }
        for (&text_input, state) in &self.text_inputs {
            guest("text input state", text_input, "zwp_text_input_v3");
            if let Some(surface) = state.active_surface {
                guest("text input focus", surface, "wl_surface");
            }
        }

        // SHM and dma-buf.
        for &pool in self.pools.keys().chain(&self.stale_shm_pools) {
            guest("SHM pool", pool, "wl_shm_pool");
        }
        for &shm in self
            .shm_guest_formats
            .keys()
            .chain(&self.stale_shm_guest_objects)
        {
            guest("wl_shm state", shm, "wl_shm");
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
                table.event_object(params.0, "zwp_linux_buffer_params_v1"),
                Some(create.guest_params_id),
                "asynchronous dma-buf create outlived host params {}",
                params.0
            );
        }
        for &params in self.pending_params.keys().chain(&self.used_dmabuf_params) {
            guest("dma-buf params", params, "zwp_linux_buffer_params_v1");
        }
        for &factory in self.dmabuf_guest_generations.keys() {
            guest("dma-buf factory", factory, "zwp_linux_dmabuf_v1");
        }
        for &feedback in self.synthetic_feedback_objects.keys() {
            guest("dma-buf feedback", feedback, "zwp_linux_dmabuf_feedback_v1");
        }
        for &generation in self.dmabuf_capabilities.keys() {
            assert!(
                linux_dmabuf::capability_generation_held(self, generation),
                "dma-buf capabilities outlived every hold on generation {generation}"
            );
        }

        // Registries.
        let registries = self
            .registry_global_names
            .keys()
            .chain(self.registry_global_removed.keys())
            .chain(self.registry_global_generations.keys())
            .chain(self.registry_global_visibility.keys())
            .chain(
                self.pending_dmabuf_globals
                    .iter()
                    .map(|pending| &pending.registry_host_id),
            );
        for &registry in registries {
            host("registry globals", registry, "wl_registry");
        }
        // A removal on record is of the global's current generation, and
        // waits for a registry that still lists it; a global the proxy
        // drops is in neither `host_globals` nor `hidden_host_globals`.
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
