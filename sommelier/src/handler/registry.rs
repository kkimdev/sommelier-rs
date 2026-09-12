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

use crate::handler::display::queue_protocol_error;
use crate::handler::shm::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, register_guest_shm,
};
use crate::protocols::aura_shell::zaura_shell::REQ_RELEASE as ZAURA_SHELL_RELEASE;
use crate::protocols::fractional_scale_v1::ALLOWED_INTERFACES as FRACTIONAL_SCALE_ALLOWED;
use crate::protocols::gtk::ALLOWED_INTERFACES as GTK_ALLOWED;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1::REQ_DESTROY as DMABUF_DESTROY;
use crate::protocols::linux_dmabuf_v1::ALLOWED_INTERFACES as DMABUF_ALLOWED;
use crate::protocols::text_input_unstable_v3::ALLOWED_INTERFACES as TEXT_INPUT_ALLOWED;
use crate::protocols::viewporter::ALLOWED_INTERFACES as VIEWPORTER_ALLOWED;
use crate::protocols::wayland::ALLOWED_INTERFACES as WL_ALLOWED;
use crate::protocols::wayland::{wl_display, wl_fixes, wl_registry};
use crate::protocols::xdg_decoration_unstable_v1::ALLOWED_INTERFACES as XDG_DECORATION_ALLOWED;
use crate::protocols::xdg_shell::ALLOWED_INTERFACES as XDG_ALLOWED;
use crate::state::{Context, GtkShellState, HostGlobal, HostId, PendingDmabufGlobal};
use crate::wire::{Action, MessageBuilder};
use log::error;

// The guest-facing proxy only implements the pre-feedback dmabuf requests
// plus the v4 feedback objects. Do not advertise v5: v5 adds tranche events
// and semantics that are not translated by this proxy.
const LINUX_DMABUF_VERSION: u32 = 4;
// Legacy format/modifier events are needed to synthesize the v4 feedback
// object exposed to the guest.  `create_immed` is available since v2 and
// explicit modifier events since v3, so bind at v3 when the host supports it.
const LINUX_DMABUF_CAPABILITY_VERSION: u32 = 3;
const TEXT_INPUT_MANAGER_VERSION: u32 = 1;
const TEXT_INPUT_EXTENSION_VERSION: u32 = 11;
const WL_COMPOSITOR_VERSION: u32 = 4;
const XDG_SHELL_VERSION: u32 = 3;
const LINUX_DMABUF_CLIENT_VERSION: u32 = 4;

/// Version ceilings used by ChromiumOS Sommelier's client-facing globals.
///
/// The generated protocol code can decode newer XML messages, but that does
/// not mean the proxy implements the semantics of every newer version. In
/// particular, the C++ proxy caps `wl_seat` at v5 and `wl_output` at v3; it
/// also exposes the compositor only up to the version for which its surface
/// damage handling is implemented. Keep the guest advertisement aligned with
/// those ceilings so clients do not start using requests/events that this
/// proxy merely forwards without the required coordinate/focus handling.
fn advertised_global_version(interface: &str, host_version: u32) -> u32 {
    let cap = match interface {
        // ChromiumOS implements wl_surface.damage_buffer in terms of the
        // legacy damage request and therefore exposes compositor v4 when the
        // host supports it. It does not expose v5/v6 surface semantics.
        "wl_compositor" => WL_COMPOSITOR_VERSION,
        "wl_output" => 3,
        "wl_seat" => 5,
        "wl_data_device_manager" => 3,
        "xdg_wm_base" => XDG_SHELL_VERSION,
        "zwp_linux_dmabuf_v1" => LINUX_DMABUF_CLIENT_VERSION,
        _ => host_version,
    };
    host_version.min(cap)
}

fn keyboard_extension_version(host_version: u32) -> u32 {
    host_version.min(2)
}

fn should_bind_internal_dmabuf(gpu_accel: bool, virtwl_supports_dmabuf: bool) -> bool {
    gpu_accel || virtwl_supports_dmabuf
}

fn next_global_generation(ctx: &mut Context) -> u64 {
    let generation = ctx.next_global_generation.max(1);
    ctx.next_global_generation = generation.wrapping_add(1).max(1);
    generation
}

fn note_registry_global(ctx: &mut Context, name: u32) -> bool {
    let registry_id = ctx.last_sender_id;
    let current_generation = ctx.global_generations.get(&name).copied();
    let previous_generation = ctx
        .registry_global_generations
        .get(&registry_id)
        .and_then(|names| names.get(&name))
        .copied();
    let already_seen = ctx
        .registry_global_names
        .get(&registry_id)
        .is_some_and(|names| names.contains(&name));

    if already_seen && previous_generation == current_generation {
        return false;
    }

    let registry_removed = ctx
        .registry_global_removed
        .get(&registry_id)
        .is_some_and(|names| names.contains(&name));
    let generation = match (current_generation, registry_removed, previous_generation) {
        (None, _, _) => {
            let new_generation = next_global_generation(ctx);
            ctx.global_generations.insert(name, new_generation);
            new_generation
        }
        (Some(current), true, Some(previous)) if previous == current => {
            // This registry removed the current generation and is now seeing
            // the replacement first. Advance the shared generation; other
            // registries that still report the old generation will adopt this
            // generation when their replacement event arrives.
            let new_generation = next_global_generation(ctx);
            ctx.global_generations.insert(name, new_generation);
            new_generation
        }
        (Some(current), _, _) => current,
    };
    ctx.registry_global_names
        .entry(registry_id)
        .or_default()
        .insert(name);
    if let Some(names) = ctx.registry_global_removed.get_mut(&registry_id) {
        names.remove(&name);
    }
    ctx.registry_global_generations
        .entry(registry_id)
        .or_default()
        .insert(name, generation);
    true
}

fn queue_synthetic_global(ctx: &mut Context, name: u32, interface: &str, version: u32) -> bool {
    queue_synthetic_global_for_registry(ctx, ctx.last_sender_id, name, interface, version)
}

fn queue_synthetic_global_for_registry(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    interface: &str,
    version: u32,
) -> bool {
    let registry_guest_id = ctx
        .shadow_table
        .get_guest_id(registry_host_id)
        .unwrap_or(registry_host_id);
    let mut builder = MessageBuilder::new();
    builder.write_u32(name);
    builder.write_string(interface);
    builder.write_u32(version);
    match builder.try_build_message(registry_guest_id, wl_registry::EVT_GLOBAL) {
        Ok(message) => {
            ctx.host_to_client_queue.push((message, Vec::new()));
            true
        }
        Err(error) => {
            log::warn!(
                "Dropping oversized synthetic global {}:{}: {}",
                name,
                interface,
                error
            );
            false
        }
    }
}

fn defer_synthetic_dmabuf_global(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    version: u32,
    generation: u64,
) {
    let pending = PendingDmabufGlobal {
        registry_host_id,
        name,
        version,
        generation,
    };
    if !ctx.pending_dmabuf_globals.contains(&pending) {
        ctx.pending_dmabuf_globals.push(pending);
    }
    ctx.registry_global_visibility
        .entry(registry_host_id)
        .or_default()
        .insert(name, false);
}

pub(crate) fn publish_pending_dmabuf_globals(ctx: &mut Context, generation: u64) {
    let can_publish = crate::handler::linux_dmabuf::has_synthetic_feedback_formats(ctx, generation);
    let mut matching = Vec::new();
    ctx.pending_dmabuf_globals.retain(|pending| {
        if pending.generation == generation {
            matching.push(pending.clone());
            false
        } else {
            true
        }
    });

    if !can_publish {
        log::warn!(
            "Hiding linux-dmabuf generation {} because the host advertised no supported formats",
            generation
        );
        return;
    }

    for pending in matching {
        let is_current = ctx
            .registry_global_names
            .get(&pending.registry_host_id)
            .is_some_and(|names| names.contains(&pending.name))
            && ctx
                .registry_global_generations
                .get(&pending.registry_host_id)
                .and_then(|generations| generations.get(&pending.name))
                .is_some_and(|&current| current == generation)
            && !ctx
                .registry_global_removed
                .get(&pending.registry_host_id)
                .is_some_and(|names| names.contains(&pending.name))
            && ctx.global_generations.get(&pending.name) == Some(&generation);
        if !is_current {
            continue;
        }

        ctx.host_globals.insert(
            pending.name,
            HostGlobal {
                interface: "zwp_linux_dmabuf_v1".to_string(),
                version: pending.version,
            },
        );
        let visible = queue_synthetic_global_for_registry(
            ctx,
            pending.registry_host_id,
            pending.name,
            "zwp_linux_dmabuf_v1",
            pending.version,
        );
        ctx.registry_global_visibility
            .entry(pending.registry_host_id)
            .or_default()
            .insert(pending.name, visible);
        if !visible {
            ctx.host_globals.remove(&pending.name);
        }
    }
}

fn queue_internal_bind(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    interface: &str,
    version: u32,
    host_id: u32,
) -> bool {
    let mut builder = MessageBuilder::new();
    builder.write_u32(name);
    builder.write_string(interface);
    builder.write_u32(version);
    builder.write_u32(host_id);
    match builder.try_build_message(registry_host_id, wl_registry::REQ_BIND) {
        Ok(message) => {
            ctx.client_to_host_queue.push((message, Vec::new()));
            true
        }
        Err(error) => {
            log::warn!(
                "Dropping oversized internal bind {}:{}: {}",
                name,
                interface,
                error
            );
            false
        }
    }
}

fn queue_dmabuf_capability_barrier(ctx: &mut Context, generation: u64) -> bool {
    let callback_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table
        .track_host_interface_with_version(callback_id, "wl_callback".to_string(), 1);

    let mut builder = MessageBuilder::new();
    builder.write_u32(callback_id);
    match builder.try_build_message(1, wl_display::REQ_SYNC) {
        Ok(message) => {
            ctx.client_to_host_queue.push((message, Vec::new()));
            ctx.dmabuf_capability_callbacks
                .insert(callback_id, generation);
            true
        }
        Err(error) => {
            log::warn!(
                "Unable to encode linux-dmabuf capability barrier for generation {}: {}",
                generation,
                error
            );
            ctx.shadow_table.remove_host_interface(callback_id);
            false
        }
    }
}

fn queue_gtk_shell_capability_barrier(ctx: &mut Context, gtk_shell_id: u32) -> bool {
    let callback_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table
        .track_host_interface_with_version(callback_id, "wl_callback".to_string(), 1);
    let mut builder = MessageBuilder::new();
    builder.write_u32(callback_id);
    match builder.try_build_message(1, wl_display::REQ_SYNC) {
        Ok(message) => {
            ctx.client_to_host_queue.push((message, Vec::new()));
            ctx.gtk_shell_capability_callbacks
                .insert(callback_id, gtk_shell_id);
            true
        }
        Err(error) => {
            log::warn!(
                "Unable to encode GTK shell capability barrier for object {}: {}",
                gtk_shell_id,
                error
            );
            ctx.shadow_table.remove_host_interface(callback_id);
            false
        }
    }
}

fn internal_binding_matches(ctx: &Context, name: u32) -> bool {
    ctx.host_dmabuf_global_name == Some(name)
        || ctx.host_shm_global_name == Some(name)
        || ctx.host_text_input_manager_v1_global_name == Some(name)
        || ctx.host_text_input_extension_v1_global_name == Some(name)
        || ctx.host_keyboard_extension_global_name == Some(name)
        || ctx.host_zaura_shell_global_name == Some(name)
}

/// Drop proxy state associated with one host global generation.
///
/// ChromiumOS destroys the internally bound proxy when its host registry
/// global disappears. Keeping the old object registered would let a
/// re-advertised global reuse stale capability/state and route events to an
/// object that no longer represents the current host global.
fn reset_internal_binding_for_global(ctx: &mut Context, name: u32) {
    if ctx.host_dmabuf_global_name == Some(name) {
        let generation = ctx.host_dmabuf_generation.take();
        if let Some(host_id) = ctx.host_dmabuf_id.take() {
            // A registry global removal invalidates the name, not objects
            // already bound from it. Explicitly destroy our hidden binding so
            // the host does not retain a proxy until connection teardown.
            let message = MessageBuilder::new().build_message(host_id, DMABUF_DESTROY);
            ctx.client_to_host_queue.push((message, Vec::new()));
            ctx.shadow_table.mark_pending_destroy_host(host_id);
        }
        ctx.host_dmabuf_global_name = None;
        ctx.supported_formats.clear();
        clear_host_shm_dmabuf_formats(ctx);
        if let Some(generation) = generation {
            crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
        }
    }
    if ctx.host_shm_global_name == Some(name) {
        if let Some(host_id) = ctx.host_shm_id.take() {
            ctx.shadow_table.retire_host_interface(host_id);
        }
        // A synthetic wl_shm object is bound locally, but its capability
        // source is the internal host binding above. Once that binding's
        // global disappears, requests from already-bound guest objects must
        // be ignored rather than being serviced by a replacement binding.
        ctx.stale_shm_guest_objects
            .extend(ctx.shm_guest_formats.keys().copied());
        ctx.stale_shm_pools.extend(ctx.pools.keys().copied());
        ctx.host_shm_global_name = None;
        clear_host_shm_wl_formats(ctx);
    }
    if ctx.host_text_input_manager_v1_global_name == Some(name) {
        // The v1 manager has no wire-level destructor.  `global_remove`
        // invalidates only the advertisement; already-bound synthetic v3
        // managers still use the host proxy to create text-input children.
        // Keep its ID and dispatch metadata reserved until connection teardown.
        ctx.host_text_input_manager_v1_global_name = None;
    }
    if ctx.host_text_input_extension_v1_global_name == Some(name) {
        // As with the v1 manager, this extension manager has no destructor.
        // Existing text-input resources may request extended children after
        // the global disappears, so retain the bound host object.
        ctx.host_text_input_extension_v1_global_name = None;
    }
    if ctx.host_keyboard_extension_global_name == Some(name) {
        if let Some(host_id) = ctx.host_keyboard_extension_id.take() {
            ctx.shadow_table.retire_host_interface(host_id.0);
        }
        ctx.host_keyboard_extension_global_name = None;
        // `global_remove` invalidates only the manager global name. It does
        // not destroy `zcr_extended_keyboard_v1` children that were already
        // created with get_extended_keyboard; those children have their own
        // destructor and remain usable until the corresponding wl_keyboard
        // is released. Keep both maps and the child dispatch registrations so
        // queued peek_key events remain routable after the manager disappears.
    }
    if ctx.host_zaura_shell_global_name == Some(name) {
        let shell_version = ctx.host_zaura_shell_version;
        let shell_id = ctx.host_zaura_shell_id.take();
        ctx.host_zaura_shell_global_name = None;
        ctx.host_zaura_shell_version = 0;
        // `global_remove` invalidates only the advertised global name. Already
        // created `zaura_surface` children have their own protocol lifetime
        // and remain usable until their owning wl_surface is destroyed. Keep
        // their mappings and per-object version metadata so an existing
        // surface can still receive set_application_id after the manager
        // global disappears. This also avoids sending a release for a child
        // that the host never asked us to destroy.
        //
        // Release only the internally bound manager, matching ChromiumOS'
        // registry remover. New children cannot be created until a replacement
        // shell global is advertised and rebound.
        if let Some(host_id) = shell_id {
            if shell_version >= 38 {
                let message = MessageBuilder::new().build_message(host_id, ZAURA_SHELL_RELEASE);
                ctx.client_to_host_queue.push((message, Vec::new()));
                ctx.shadow_table.mark_pending_destroy_host(host_id);
            } else {
                ctx.shadow_table.retire_host_interface(host_id);
            }
        }
    }
    ctx.hidden_host_globals.remove(&name);
}

/// Remove advertisement metadata left by the previous generation of a
/// re-used numeric global name.
///
/// A Wayland global name identifies the current advertisement, not a
/// permanent interface slot. Once this registry has observed the old
/// generation's removal and receives a replacement, any visible/hidden record
/// for the old interface must be discarded before the replacement is handled.
/// Bound child objects remain in their own maps; only connection-wide global
/// advertisement classification is replaced here.
fn replace_global_metadata_for_generation(ctx: &mut Context, name: u32) {
    ctx.host_globals.remove(&name);
    ctx.hidden_host_globals.remove(&name);
    ctx.removed_host_globals.remove(&name);
}

fn record_registry_global_visibility(ctx: &mut Context, name: u32, visible: bool) {
    ctx.registry_global_visibility
        .entry(ctx.last_sender_id)
        .or_default()
        .insert(name, visible);
}

fn can_synthesize_dmabuf_feedback(ctx: &Context, version: u32) -> bool {
    if version < LINUX_DMABUF_CAPABILITY_VERSION {
        return false;
    }
    if ctx.allocator.is_some() {
        return true;
    }
    #[cfg(test)]
    {
        ctx.synthetic_feedback_available_for_test
    }
    #[cfg(not(test))]
    {
        false
    }
}

fn guest_bind_is_allowed(
    ctx: &Context,
    name: u32,
    interface: &str,
    requested_version: u32,
) -> bool {
    let interface_is_supported = WL_ALLOWED.contains(&interface)
        || XDG_ALLOWED.contains(&interface)
        || DMABUF_ALLOWED.contains(&interface)
        || VIEWPORTER_ALLOWED.contains(&interface)
        || TEXT_INPUT_ALLOWED.contains(&interface)
        || GTK_ALLOWED.contains(&interface)
        || (XDG_DECORATION_ALLOWED.contains(&interface) && ctx.xdg_decoration)
        || FRACTIONAL_SCALE_ALLOWED.contains(&interface)
        || interface == "wl_data_device_manager";
    if !interface_is_supported {
        return false;
    }

    let Some(global) = ctx.host_globals.get(&name) else {
        return false;
    };
    if ctx.removed_host_globals.contains(&name) {
        return false;
    }

    // A global is advertised independently on each wl_registry object. Once
    // one registry has observed global_remove, that registry must reject a
    // bind even while another registry is still receiving the old generation.
    // Client→host requests carry the guest registry ID, whereas lifecycle
    // tracking is keyed by the paired host registry ID.
    if let Some(registry_host_id) = ctx
        .shadow_table
        .get_host_id(ctx.last_sender_id)
        .or(Some(ctx.last_sender_id))
    {
        if let Some(names) = ctx.registry_global_names.get(&registry_host_id) {
            if !names.contains(&name) {
                return false;
            }
            let registry_generation = ctx
                .registry_global_generations
                .get(&registry_host_id)
                .and_then(|generations| generations.get(&name))
                .copied();
            if registry_generation != ctx.global_generations.get(&name).copied() {
                return false;
            }
        }
    }

    // The v3 manager is a synthetic global backed by the host v1 manager.
    // on_global records the synthetic interface under the same numeric name.
    // Checking the advertised version here also prevents a client from
    // forwarding a bind for a protocol version that the proxy did not expose.
    global.interface == interface && requested_version > 0 && requested_version <= global.version
}

pub struct RegistryHandler;

impl wl_registry::WlRegistryHandler for RegistryHandler {
    fn on_global(
        &mut self,
        ctx: &mut Context,
        name: u32,
        interface: &String,
        version: u32,
    ) -> Action {
        let registry_id = ctx.last_sender_id;
        let previous_generation = ctx.global_generations.get(&name).copied();
        let registry_previous_generation = ctx
            .registry_global_generations
            .get(&registry_id)
            .and_then(|names| names.get(&name))
            .copied();
        let registry_removed = ctx
            .registry_global_removed
            .get(&registry_id)
            .is_some_and(|names| names.contains(&name));
        if version == 0 {
            log::warn!(
                "Ignoring malformed Wayland global {}:{} with version 0",
                name,
                interface
            );
            return Action::Drop;
        }
        if !note_registry_global(ctx, name) {
            log::warn!(
                "Ignoring duplicate Wayland global name {} in registry {} (interface {})",
                name,
                ctx.last_sender_id,
                interface
            );
            return Action::Drop;
        }
        let generation_changed = previous_generation
            .zip(ctx.global_generations.get(&name).copied())
            .is_some_and(|(previous, current)| previous != current);
        let starts_new_generation = generation_changed
            && registry_removed
            && registry_previous_generation == previous_generation;
        if generation_changed {
            if starts_new_generation && internal_binding_matches(ctx, name) {
                reset_internal_binding_for_global(ctx, name);
            }
            replace_global_metadata_for_generation(ctx, name);
        }
        // A global that disappeared may later be re-advertised with the same
        // numeric name. Its metadata is retained while other registry objects
        // receive global_remove, so clear the tombstone when this is a new
        // advertisement.
        ctx.removed_host_globals.remove(&name);

        if interface == "zaura_shell" && version < 6 {
            // Sommelier's aura integration requires set_application_id (v5)
            // and only binds the shell when the host provides the v6+
            // contract used by ChromiumOS.
            log::debug!(
                "Ignoring zaura_shell v{}; minimum supported version is 6",
                version
            );
            record_registry_global_visibility(ctx, name, false);
            return Action::Drop;
        }

        if interface == "zwp_linux_dmabuf_v1" {
            let generation = ctx
                .global_generations
                .get(&name)
                .copied()
                .unwrap_or_default();
            let virtwl_supports_dmabuf = ctx
                .virtwayland_channel
                .as_ref()
                .is_some_and(|channel| channel.supports_dmabuf());
            if !should_bind_internal_dmabuf(ctx.gpu_accel, virtwl_supports_dmabuf) {
                record_registry_global_visibility(ctx, name, false);
                return Action::Drop;
            }
            // A v3 host still has enough legacy format/modifier information
            // for our synthetic v4 feedback object. Advertise v4 to the
            // guest while clamping only the internal host bind to v3.
            let can_synthesize_feedback = can_synthesize_dmabuf_feedback(ctx, version);
            let client_version = if can_synthesize_feedback {
                LINUX_DMABUF_VERSION
            } else {
                if version >= LINUX_DMABUF_CAPABILITY_VERSION && ctx.allocator.is_none() {
                    log::warn!(
                        "GBM allocator unavailable; exposing linux-dmabuf v3 without synthetic \
                         device feedback"
                    );
                }
                version.min(LINUX_DMABUF_CAPABILITY_VERSION)
            };
            if ctx.host_dmabuf_id.is_some() {
                // One host dmabuf object is shared for capability discovery.
                // A second guest registry still needs the client-facing
                // global event when the GPU path is enabled.
                if ctx.gpu_accel {
                    let capability_ready = ctx
                        .dmabuf_capabilities
                        .get(&generation)
                        .is_some_and(|capabilities| capabilities.ready);
                    if can_synthesize_feedback && !capability_ready {
                        defer_synthetic_dmabuf_global(
                            ctx,
                            ctx.last_sender_id,
                            name,
                            client_version,
                            generation,
                        );
                    } else if !can_synthesize_feedback
                        || crate::handler::linux_dmabuf::has_synthetic_feedback_formats(
                            ctx, generation,
                        )
                    {
                        ctx.host_globals.insert(
                            name,
                            HostGlobal {
                                interface: interface.clone(),
                                version: client_version,
                            },
                        );
                        let visible = queue_synthetic_global(ctx, name, interface, client_version);
                        record_registry_global_visibility(ctx, name, visible);
                    } else {
                        record_registry_global_visibility(ctx, name, false);
                    }
                } else {
                    ctx.hidden_host_globals.insert(name, interface.clone());
                    record_registry_global_visibility(ctx, name, false);
                }
                return Action::Drop;
            }
            // Bind at v3 when available so the proxy can collect legacy
            // format/modifier pairs and synthesize the v4 feedback object.
            // v2 still keeps create_immed available on older hosts.
            let host_bind_version = client_version.min(LINUX_DMABUF_CAPABILITY_VERSION);
            let host_id = ctx.shadow_table.allocate_host_id();
            ctx.host_dmabuf_id = Some(host_id);
            ctx.host_dmabuf_global_name = Some(name);
            ctx.host_dmabuf_generation = Some(generation);
            ctx.dmabuf_capabilities.entry(generation).or_default();
            // Register for event dispatch: the host compositor sends format/modifier
            // events to the dmabuf factory object after we bind it.
            ctx.shadow_table.track_host_interface_with_version(
                host_id,
                "zwp_linux_dmabuf_v1".to_string(),
                host_bind_version,
            );

            if ctx.gpu_accel {
                if can_synthesize_feedback {
                    defer_synthetic_dmabuf_global(
                        ctx,
                        ctx.last_sender_id,
                        name,
                        client_version,
                        generation,
                    );
                } else {
                    ctx.host_globals.insert(
                        name,
                        HostGlobal {
                            interface: interface.clone(),
                            version: client_version,
                        },
                    );
                    let visible = queue_synthetic_global(ctx, name, interface, client_version);
                    record_registry_global_visibility(ctx, name, visible);
                }
            } else {
                // Virtwl uses this object only to learn the formats that can
                // be advertised by the synthetic wl_shm global. Keep the
                // linux-dmabuf global hidden from the guest, matching
                // ChromiumOS's separate --enable-linux-dmabuf switch.
                ctx.hidden_host_globals.insert(name, interface.clone());
                record_registry_global_visibility(ctx, name, false);
            }

            // 2. Bind internally
            let registry_host_id = ctx.last_sender_id;
            let bound = queue_internal_bind(
                ctx,
                registry_host_id,
                name,
                interface,
                host_bind_version,
                host_id,
            );
            if bound && can_synthesize_feedback {
                if !queue_dmabuf_capability_barrier(ctx, generation) {
                    ctx.fatal_protocol_error = true;
                }
            } else if !bound {
                ctx.fatal_protocol_error = true;
            }

            // Drop the original global event so we don't send the host's
            // uncapped advertisement.
            return Action::Drop;
        } else if interface == "zwp_text_input_manager_v1" {
            let bound_version = version.min(TEXT_INPUT_MANAGER_VERSION);
            if ctx.host_text_input_manager_v1_id.is_some() {
                ctx.host_globals.insert(
                    name,
                    HostGlobal {
                        interface: "zwp_text_input_manager_v3".to_string(),
                        version: 1,
                    },
                );
                let visible = queue_synthetic_global(ctx, name, "zwp_text_input_manager_v3", 1);
                record_registry_global_visibility(ctx, name, visible);
                return Action::Drop;
            }
            let host_id = ctx.shadow_table.allocate_host_id();
            ctx.host_text_input_manager_v1_id = Some(host_id);
            ctx.host_text_input_manager_v1_global_name = Some(name);
            // This object is internal (there is no guest-side pair), but it
            // still occupies a host object ID and must be reserved so a later
            // allocation cannot collide with it.
            ctx.shadow_table.track_host_interface_with_version(
                host_id,
                interface.clone(),
                bound_version,
            );

            let client_version = 1;
            ctx.host_globals.insert(
                name,
                HostGlobal {
                    interface: "zwp_text_input_manager_v3".to_string(),
                    version: client_version,
                },
            );
            let visible =
                queue_synthetic_global(ctx, name, "zwp_text_input_manager_v3", client_version);
            record_registry_global_visibility(ctx, name, visible);

            // 2. Bind internally
            let registry_host_id = ctx.last_sender_id;
            queue_internal_bind(
                ctx,
                registry_host_id,
                name,
                interface,
                bound_version,
                host_id,
            );

            return Action::Drop;
        } else if interface == "zcr_text_input_extension_v1" {
            if ctx.host_text_input_extension_v1_id.is_some() {
                ctx.hidden_host_globals.insert(name, interface.clone());
                record_registry_global_visibility(ctx, name, false);
                return Action::Drop;
            }
            let bound_version = version.min(TEXT_INPUT_EXTENSION_VERSION);
            ctx.hidden_host_globals.insert(name, interface.clone());
            record_registry_global_visibility(ctx, name, false);
            let host_id = ctx.shadow_table.allocate_host_id();
            ctx.host_text_input_extension_v1_id = Some(host_id);
            ctx.host_text_input_extension_v1_global_name = Some(name);
            // Reserve internal host-only objects in the allocator even though
            // the factory does not emit events addressed to this object.
            ctx.shadow_table.track_host_interface_with_version(
                host_id,
                interface.clone(),
                bound_version,
            );

            // 2. Bind internally
            let registry_host_id = ctx.last_sender_id;
            queue_internal_bind(
                ctx,
                registry_host_id,
                name,
                interface,
                bound_version,
                host_id,
            );

            return Action::Drop;
        } else if interface == "zcr_keyboard_extension_v1" {
            if ctx.host_keyboard_extension_id.is_some() {
                ctx.hidden_host_globals.insert(name, interface.clone());
                record_registry_global_visibility(ctx, name, false);
                return Action::Drop;
            }
            ctx.hidden_host_globals.insert(name, interface.clone());
            record_registry_global_visibility(ctx, name, false);
            // Bind zcr_keyboard_extension_v1 internally. This is a ChromeOS-
            // specific protocol that enables the ack-key mechanism for
            // controlling host accelerator processing. Version 2 additionally
            // reports physical keys consumed by IME via peek_key.
            let host_id = ctx.shadow_table.allocate_host_id();
            ctx.host_keyboard_extension_id = Some(HostId::from_allocated(host_id));
            ctx.host_keyboard_extension_global_name = Some(name);
            let bound_version = keyboard_extension_version(version);
            // Reserve this internal host-only factory object. Its ID is also
            // used as the sender for get_extended_keyboard requests.
            ctx.shadow_table.track_host_interface_with_version(
                host_id,
                interface.clone(),
                bound_version,
            );

            let registry_host_id = ctx.last_sender_id;
            queue_internal_bind(
                ctx,
                registry_host_id,
                name,
                interface,
                bound_version,
                host_id,
            );
            log::debug!(
                "Bound zcr_keyboard_extension_v1 v{} (host_id={})",
                bound_version,
                host_id
            );

            return Action::Drop;
        } else if interface == "zaura_shell" {
            if ctx.host_zaura_shell_id.is_some() {
                // The host compositor sends the same global list to every
                // wl_registry object. Sommelier has one internal aura shell
                // binding per connection, so a later registry must not bind
                // another host object or overwrite the shared routing state.
                ctx.host_globals.insert(
                    name,
                    HostGlobal {
                        interface: "gtk_shell1".to_string(),
                        version: 1,
                    },
                );
                let visible = queue_synthetic_global(ctx, name, "gtk_shell1", 1);
                record_registry_global_visibility(ctx, name, visible);
                return Action::Drop;
            }
            // Bind zaura_shell internally for ChromeOS shelf integration.
            // We use this to create zaura_surface objects and set application
            // IDs and GTK startup IDs. The host-specific global is replaced
            // at the guest boundary with gtk_shell1, using the same numeric
            // registry name so its global_remove lifecycle remains paired.
            let host_id = ctx.shadow_table.allocate_host_id();
            let bound_version = std::cmp::min(version, 38);
            ctx.host_zaura_shell_id = Some(host_id);
            ctx.host_zaura_shell_global_name = Some(name);
            ctx.host_zaura_shell_version = bound_version;
            ctx.shadow_table.track_host_interface_with_version(
                host_id,
                "zaura_shell".to_string(),
                bound_version,
            );
            ctx.host_globals.insert(
                name,
                HostGlobal {
                    interface: "gtk_shell1".to_string(),
                    version: 1,
                },
            );
            let visible = queue_synthetic_global(ctx, name, "gtk_shell1", 1);
            record_registry_global_visibility(ctx, name, visible);

            let registry_host_id = ctx.last_sender_id;
            queue_internal_bind(
                ctx,
                registry_host_id,
                name,
                interface,
                bound_version,
                host_id,
            );
            log::debug!("Bound zaura_shell internally (host_id={})", host_id);

            return Action::Drop;
        } else if interface == "wl_shm" {
            if ctx.host_shm_id.is_some() {
                ctx.host_globals.insert(
                    name,
                    HostGlobal {
                        interface: interface.clone(),
                        version: 1,
                    },
                );
                let visible = queue_synthetic_global(ctx, name, interface, 1);
                record_registry_global_visibility(ctx, name, visible);
                return Action::Drop;
            }
            let host_id = ctx.shadow_table.allocate_host_id();
            ctx.host_shm_id = Some(host_id);
            ctx.host_shm_global_name = Some(name);
            ctx.host_globals.insert(
                name,
                HostGlobal {
                    interface: interface.clone(),
                    version: 1,
                },
            );
            // wl_shm is emulated: on_bind drops the guest request and sends synthetic
            // format events, so the host never sends wl_shm events to us. It is
            // nevertheless a live host object and its ID must be reserved.
            ctx.shadow_table
                .track_host_interface_with_version(host_id, interface.clone(), 1);

            // Bind to wl_shm
            let registry_host_id = ctx.last_sender_id;
            queue_internal_bind(ctx, registry_host_id, name, interface, 1, host_id);

            // The SHM implementation is a v1 compatibility shim. Advertise
            // the capped version explicitly instead of forwarding a host v2
            // global: otherwise clients bind v2 and are rejected by
            // `guest_bind_is_allowed` even though v1 is fully supported.
            let visible = queue_synthetic_global(ctx, name, interface, 1);
            record_registry_global_visibility(ctx, name, visible);

            return Action::Drop;
        }

        let is_allowed = WL_ALLOWED.contains(&interface.as_str())
            || XDG_ALLOWED.contains(&interface.as_str())
            || DMABUF_ALLOWED.contains(&interface.as_str())
            || VIEWPORTER_ALLOWED.contains(&interface.as_str())
            || TEXT_INPUT_ALLOWED.contains(&interface.as_str())
            || (XDG_DECORATION_ALLOWED.contains(&interface.as_str()) && ctx.xdg_decoration)
            || FRACTIONAL_SCALE_ALLOWED.contains(&interface.as_str())
            || interface == "wl_data_device_manager";

        if !is_allowed {
            record_registry_global_visibility(ctx, name, false);
            return Action::Drop;
        }
        let advertised_version = advertised_global_version(interface, version);
        ctx.host_globals.insert(
            name,
            HostGlobal {
                interface: interface.clone(),
                version: advertised_version,
            },
        );
        if advertised_version != version {
            let visible = queue_synthetic_global(ctx, name, interface, advertised_version);
            record_registry_global_visibility(ctx, name, visible);
            return Action::Drop;
        }
        record_registry_global_visibility(ctx, name, true);
        Action::Forward
    }

    fn on_global_remove(&mut self, ctx: &mut Context, name: u32) -> Action {
        let registry_id = ctx.last_sender_id;
        let removed_generation = ctx
            .registry_global_generations
            .get(&registry_id)
            .and_then(|generations| generations.get(&name))
            .copied();
        let was_visible = ctx
            .registry_global_visibility
            .get(&registry_id)
            .and_then(|visibility| visibility.get(&name))
            .copied()
            .unwrap_or_else(|| ctx.host_globals.contains_key(&name));
        let registry_seen = ctx
            .registry_global_names
            .get_mut(&registry_id)
            .is_some_and(|names| names.remove(&name));
        if !registry_seen {
            return Action::Drop;
        }
        ctx.pending_dmabuf_globals.retain(|pending| {
            !(pending.registry_host_id == registry_id
                && pending.name == name
                && Some(pending.generation) == removed_generation)
        });
        if let Some(generation) = removed_generation {
            crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
        }
        if let Some(visibility) = ctx.registry_global_visibility.get_mut(&registry_id) {
            visibility.remove(&name);
        }
        ctx.registry_global_removed
            .entry(registry_id)
            .or_default()
            .insert(name);
        // Host registries are independent Wayland objects and their events can
        // be interleaved. A global name may be removed from one registry and
        // re-advertised there before another registry delivers the old remove.
        // Do not tombstone the connection-wide metadata until every registry
        // has observed the removal; otherwise that stale remove invalidates a
        // newly advertised generation.
        let current_generation = ctx.global_generations.get(&name).copied();
        let removes_current_generation = removed_generation == current_generation;
        let still_seen = ctx.registry_global_names.iter().any(|(registry, names)| {
            names.contains(&name)
                && ctx
                    .registry_global_generations
                    .get(registry)
                    .and_then(|generations| generations.get(&name))
                    .copied()
                    == current_generation
        });
        if removes_current_generation && !still_seen {
            reset_internal_binding_for_global(ctx, name);
        }
        if ctx.hidden_host_globals.contains_key(&name) {
            // `wl_registry.global_remove` invalidates only the *global name*.
            // Any object that was already bound remains valid until its own
            // destructor (the Wayland core protocol explicitly requires this).
            // Keep the internal object's ID and singleton state reserved so
            // queued host events cannot be routed to a later object that
            // happens to reuse the ID.
            if removes_current_generation && !still_seen {
                ctx.removed_host_globals.insert(name);
                ctx.hidden_host_globals.remove(&name);
                ctx.host_globals.remove(&name);
            }
            return if was_visible {
                Action::Forward
            } else {
                Action::Drop
            };
        }
        // Only globals previously exposed to the guest should generate a
        // matching removal event. Hidden/internal globals were never sent to
        // the guest and must not leak lifecycle notifications.
        if ctx.host_globals.contains_key(&name) {
            if removes_current_generation && !still_seen {
                ctx.removed_host_globals.insert(name);
                ctx.host_globals.remove(&name);
            }
            if was_visible {
                Action::Forward
            } else {
                Action::Drop
            }
        } else {
            if was_visible {
                Action::Forward
            } else {
                Action::Drop
            }
        }
    }

    fn on_bind(&mut self, ctx: &mut Context, name: u32, id: &(String, u32, u32)) -> Action {
        // id is (interface, version, new_id)
        let (interface, version, guest_new_id) = id;

        if !guest_bind_is_allowed(ctx, name, interface, *version) {
            error!(
                "Rejecting bind for unadvertised, unsupported, or too-new global {}:{} v{}",
                name, interface, version
            );
            queue_protocol_error(
                ctx,
                ctx.last_sender_id,
                0,
                format!(
                    "invalid bind for global {} (interface {}, version {})",
                    name, interface, version
                ),
            );
            return Action::Drop;
        }

        if interface == "wl_shm" {
            // Do NOT forward wl_shm to host. We emulate it.
            // Just track it so we know this guest ID is wl_shm.
            ctx.shadow_table.track_interface_with_version(
                *guest_new_id,
                "wl_shm".to_string(),
                *version,
            );

            // Advertise mandatory formats immediately and optional formats
            // only after the host wl_shm/dmabuf capability event has arrived.
            // This mirrors ChromiumOS Sommelier and prevents a guest from
            // selecting a format that the host cannot import.
            register_guest_shm(ctx, *guest_new_id);

            // The host-side wl_shm object was bound when the host global was
            // observed. Binding the synthetic guest global must not issue a
            // second host bind or create a guest→host alias.
            return Action::Drop;
        } else if interface == "zwp_text_input_manager_v3" {
            // This is a synthetic guest-only manager. Every guest manager
            // shares the one internal host v1 manager, so mapping each guest
            // ID to that host ID would overwrite the previous manager's
            // reverse mapping and allow one manager's destroy to unregister
            // the shared host object.
            ctx.shadow_table.track_interface_with_version(
                *guest_new_id,
                interface.clone(),
                *version,
            );
            return Action::Drop;
        } else if interface == "gtk_shell1" {
            ctx.shadow_table.track_interface_with_version(
                *guest_new_id,
                interface.clone(),
                *version,
            );
            ctx.gtk_shells
                .insert(*guest_new_id, GtkShellState::default());
            if !queue_gtk_shell_capability_barrier(ctx, *guest_new_id) {
                ctx.fatal_protocol_error = true;
            }
            return Action::Drop;
        }

        // Translation logic for other interfaces
        let host_new_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table.map_id(*guest_new_id, host_new_id);
        ctx.shadow_table
            .track_interface_with_version(*guest_new_id, interface.clone(), *version);
        if interface == "wl_output" {
            ctx.output_host_ids.push(host_new_id);
            ctx.output_states.entry(host_new_id).or_default();
        }
        // The guest-facing dmabuf global is synthesized at v4, while the
        // host object used for params/create may only be v2/v3. Keep the
        // guest metadata at v4 so feedback requests are accepted locally,
        // but clamp the paired host object to v3 so registry.bind does not
        // request an unsupported host version.
        let host_version = if interface == "zwp_linux_dmabuf_v1" {
            (*version).min(LINUX_DMABUF_CAPABILITY_VERSION)
        } else {
            *version
        };
        ctx.shadow_table.set_host_version(host_new_id, host_version);

        // We need to send the bind request to the host.
        // The sender is the registry object.
        let registry_guest_id = ctx.last_sender_id;

        let Some(registry_host_id) = ctx.shadow_table.get_host_id(registry_guest_id) else {
            error!("Registry not mapped! Guest ID: {}", registry_guest_id);
            // Do not leave a guest→host mapping behind when the registry
            // itself has already been destroyed or was never mapped.
            ctx.shadow_table.remove_id(*guest_new_id);
            return Action::Drop;
        };
        let bind_version = if interface == "zwp_linux_dmabuf_v1" {
            host_version
        } else {
            *version
        };
        if !queue_internal_bind(
            ctx,
            registry_host_id,
            name,
            interface,
            bind_version,
            host_new_id,
        ) {
            if interface == "wl_output" {
                ctx.remove_output_state(host_new_id);
            }
            ctx.shadow_table.remove_id(*guest_new_id);
            ctx.fatal_protocol_error = true;
            return Action::Drop;
        }
        if interface == "wl_output" && ctx.window_bounds_as_arc {
            // The Aura output extension carries the logical work-area insets
            // that wl_output itself does not expose. It is host-only; the
            // guest continues to receive the ordinary wl_output events.
            let _ = crate::handler::compositor::ensure_zaura_output(ctx, host_new_id);
        }
        if interface == "zwp_linux_dmabuf_v1" {
            let generation = ctx
                .registry_global_generations
                .get(&registry_host_id)
                .and_then(|generations| generations.get(&name))
                .copied()
                .or_else(|| ctx.global_generations.get(&name).copied())
                .unwrap_or_default();
            ctx.dmabuf_guest_generations
                .insert(*guest_new_id, generation);
        }

        Action::Drop
    }
}

impl wl_fixes::WlFixesHandler for RegistryHandler {
    fn on_destroy_registry(&mut self, ctx: &mut Context, registry: u32) -> Action {
        let registry_host_id = ctx.shadow_table.get_host_id(registry).unwrap_or(registry);
        let generations = ctx
            .registry_global_generations
            .remove(&registry_host_id)
            .into_iter()
            .flat_map(|generations| generations.into_values())
            .collect::<std::collections::HashSet<_>>();
        ctx.registry_global_names.remove(&registry_host_id);
        ctx.registry_global_removed.remove(&registry_host_id);
        ctx.registry_global_visibility.remove(&registry_host_id);
        ctx.pending_dmabuf_globals
            .retain(|pending| pending.registry_host_id != registry_host_id);
        for generation in generations {
            crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
        }

        // wl_fixes destroys the registry passed as an object argument rather
        // than its own request sender. Reserve that guest/host mapping until
        // wl_display.delete_id acknowledges the host-side destruction.
        ctx.shadow_table.mark_pending_destroy(registry);
        Action::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::{
        advertised_global_version, keyboard_extension_version, queue_dmabuf_capability_barrier,
        should_bind_internal_dmabuf, RegistryHandler, TEXT_INPUT_EXTENSION_VERSION,
    };
    use crate::handler::callback::CallbackHandler;
    use crate::handler::linux_dmabuf::LinuxDmabufHandler;
    use crate::protocols::aura_shell::zaura_shell::REQ_RELEASE as ZAURA_SHELL_RELEASE;
    use crate::protocols::aura_shell::{zaura_shell, zaura_surface};
    use crate::protocols::gtk::gtk_shell1;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_fixes::WlFixesHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::state::{Context, HostGlobal, HostId};
    use crate::wire::Action;

    #[test]
    fn keyboard_extension_uses_peek_key_without_exceeding_host_version() {
        assert_eq!(keyboard_extension_version(1), 1);
        assert_eq!(keyboard_extension_version(2), 2);
        assert_eq!(keyboard_extension_version(99), 2);
    }

    #[test]
    fn virtwl_dmabuf_capability_requires_an_internal_bind_without_gpu() {
        assert!(
            should_bind_internal_dmabuf(false, true),
            "a virtwl channel with dmabuf support must be probed and bound even \
             when the guest-facing GPU path is disabled"
        );
    }

    #[test]
    fn dmabuf_capability_barrier_is_a_tracked_host_callback() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert!(queue_dmabuf_capability_barrier(&mut ctx, 9));
        assert_eq!(ctx.client_to_host_queue.len(), 1);

        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()),
            1,
            "wl_display must own the sync request"
        );
        assert_eq!(
            u16::from_ne_bytes(message[4..6].try_into().unwrap()),
            crate::protocols::wayland::wl_display::REQ_SYNC
        );
        let callback_id = u32::from_ne_bytes(message[8..12].try_into().unwrap());
        assert_eq!(ctx.dmabuf_capability_callbacks.get(&callback_id), Some(&9));
        assert_eq!(
            ctx.shadow_table.get_host_interface(callback_id),
            Some(&"wl_callback".to_string())
        );
    }

    #[test]
    fn wl_fixes_destroy_registry_removes_deferred_dmabuf_advertisements() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.registry_global_names.entry(100).or_default().insert(7);
        ctx.registry_global_generations
            .entry(100)
            .or_default()
            .insert(7, 9);
        ctx.registry_global_visibility
            .entry(100)
            .or_default()
            .insert(7, false);
        ctx.dmabuf_capabilities.entry(9).or_default();
        ctx.pending_dmabuf_globals
            .push(crate::state::PendingDmabufGlobal {
                registry_host_id: 100,
                name: 7,
                version: 4,
                generation: 9,
            });

        let mut handler = RegistryHandler;
        assert_eq!(
            WlFixesHandler::on_destroy_registry(&mut handler, &mut ctx, 10),
            Action::Forward
        );
        assert!(!ctx.registry_global_names.contains_key(&100));
        assert!(!ctx.registry_global_generations.contains_key(&100));
        assert!(!ctx.registry_global_visibility.contains_key(&100));
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert!(!ctx.dmabuf_capabilities.contains_key(&9));
        assert!(ctx.shadow_table.is_pending_destroy_guest(10));
    }

    #[test]
    fn synthetic_shm_global_is_sent_to_each_guest_registry() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table.map_id(11, 101);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.shadow_table
            .track_interface(11, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global(&mut ctx, 7, &shm, 2), Action::Drop);
        ctx.host_to_client_queue.clear();

        ctx.last_sender_id = 101;
        assert_eq!(
            handler.on_global(&mut ctx, 7, &shm, 2),
            Action::Drop,
            "the second registry must consume the host event locally"
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "each registry needs its own synthetic wl_shm global event"
        );
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(
            u32::from_ne_bytes(message[0..4].try_into().unwrap()),
            11,
            "the synthetic event must target the second guest registry"
        );
    }

    #[test]
    fn aura_global_exposes_local_gtk_shell_to_every_registry() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table.map_id(11, 101);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_registry".to_string(), 1);
        ctx.shadow_table
            .track_interface_with_version(11, "wl_registry".to_string(), 1);
        let mut handler = RegistryHandler;
        let aura = "zaura_shell".to_string();

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global(&mut ctx, 7, &aura, 38), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let global = &ctx.host_to_client_queue[0].0;
        assert_eq!(u32::from_ne_bytes(global[0..4].try_into().unwrap()), 10);
        let mut wire = crate::wire::WireMessage::new(10, 0, &global[8..], &[]);
        assert_eq!(wire.read_u32().unwrap(), 7);
        assert_eq!(wire.read_string().unwrap(), "gtk_shell1");
        assert_eq!(wire.read_u32().unwrap(), 1);

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 101;
        assert_eq!(handler.on_global(&mut ctx, 7, &aura, 38), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[0..4].try_into().unwrap()),
            11
        );

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 10;
        assert_eq!(
            handler.on_bind(&mut ctx, 7, &("gtk_shell1".to_string(), 1, 50)),
            Action::Drop
        );
        assert!(ctx.shadow_table.is_local_only_guest_object(50));
        assert!(ctx.gtk_shells.contains_key(&50));
        assert!(ctx.host_to_client_queue.is_empty());
        let callback_id = ctx
            .gtk_shell_capability_callbacks
            .iter()
            .find_map(|(&callback_id, &shell_id)| (shell_id == 50).then_some(callback_id))
            .expect("GTK capability callback");
        ctx.last_sender_id = callback_id;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let capabilities = &ctx.host_to_client_queue[0].0;
        assert_eq!(
            u32::from_ne_bytes(capabilities[0..4].try_into().unwrap()),
            50
        );
        assert_eq!(
            u16::from_ne_bytes(capabilities[4..6].try_into().unwrap()),
            gtk_shell1::EVT_CAPABILITIES
        );
        assert_eq!(
            u32::from_ne_bytes(capabilities[8..12].try_into().unwrap()),
            0
        );
    }

    #[test]
    fn re_advertised_synthetic_shm_global_remains_bindable() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table.map_id(11, 101);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.shadow_table
            .track_interface(11, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global(&mut ctx, 7, &shm, 2), Action::Drop);
        ctx.host_to_client_queue.clear();

        // The second registry receives the same synthetic global with a new
        // host global name. It must carry its own metadata: bind validation is
        // keyed by the numeric name, not merely by interface.
        ctx.last_sender_id = 101;
        assert_eq!(handler.on_global(&mut ctx, 8, &shm, 2), Action::Drop);
        assert_eq!(
            ctx.host_globals.get(&8),
            Some(&HostGlobal {
                interface: shm.clone(),
                version: 1,
            }),
            "every visible synthetic global needs bind metadata"
        );

        ctx.last_sender_id = 11;
        let bind = (shm, 1, 20);
        assert_eq!(handler.on_bind(&mut ctx, 8, &bind), Action::Drop);
        assert_eq!(
            ctx.shadow_table.get_interface(20),
            Some(&"wl_shm".to_string()),
            "the second registry's synthetic global must accept a bind"
        );
    }

    #[test]
    fn invalid_bind_queues_a_display_protocol_error() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.host_globals.insert(
            7,
            HostGlobal {
                interface: "wl_compositor".to_string(),
                version: 1,
            },
        );
        ctx.registry_global_names.entry(100).or_default().insert(7);
        ctx.global_generations.insert(7, 1);
        ctx.registry_global_generations
            .entry(100)
            .or_default()
            .insert(7, 1);
        ctx.last_sender_id = 10;

        let mut handler = RegistryHandler;
        let bind = ("wl_seat".to_string(), 1, 20);
        assert_eq!(handler.on_bind(&mut ctx, 7, &bind), Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "invalid wl_registry.bind must report a fatal wl_display.error"
        );
        assert!(ctx.fatal_protocol_error);
    }

    #[test]
    fn internal_bindings_are_capped_to_generated_protocol_versions() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;

        let dmabuf = "zwp_linux_dmabuf_v1".to_string();
        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 99), Action::Drop);
        let dmabuf_id = ctx.host_dmabuf_id.expect("dmabuf binding");
        assert_eq!(ctx.shadow_table.host_object_version(dmabuf_id), Some(3));

        let extension = "zcr_text_input_extension_v1".to_string();
        assert_eq!(
            handler.on_global(&mut ctx, 11, &extension, 99),
            Action::Drop
        );
        let extension_id = ctx
            .host_text_input_extension_v1_id
            .expect("text input extension binding");
        assert_eq!(
            ctx.shadow_table.host_object_version(extension_id),
            Some(TEXT_INPUT_EXTENSION_VERSION)
        );
    }

    #[test]
    fn internal_dmabuf_binding_uses_capability_version_three() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 5), Action::Drop);

        let host_id = ctx.host_dmabuf_id.expect("internal dmabuf binding");
        assert_eq!(
            ctx.shadow_table.host_object_version(host_id),
            Some(3),
            "the internal binding must collect v3 legacy modifiers while the \
             guest-facing global supports feedback v4+"
        );

        let bind = &ctx.client_to_host_queue[0].0;
        let mut wire = crate::wire::WireMessage::new(1, 0, &bind[8..], &[]);
        assert_eq!(wire.read_u32().unwrap(), 10);
        assert_eq!(wire.read_string().unwrap(), dmabuf);
        assert_eq!(
            wire.read_u32().unwrap(),
            3,
            "internal host bind should request v3 for legacy modifier discovery"
        );
        assert_eq!(wire.read_u32().unwrap(), host_id);
    }

    #[test]
    fn gpu_without_allocator_exposes_host_v3_without_feedback_barrier() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.allocator = None;
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 4), Action::Drop);

        assert_eq!(
            ctx.host_globals.get(&10),
            Some(&HostGlobal {
                interface: dmabuf,
                version: 3,
            })
        );
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert!(ctx.dmabuf_capability_callbacks.is_empty());
        assert_eq!(
            ctx.client_to_host_queue.len(),
            1,
            "only the internal v3 bind should be sent without feedback synthesis"
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let global = &ctx.host_to_client_queue[0].0;
        let mut wire = crate::wire::WireMessage::new(1, 0, &global[8..], &[]);
        assert_eq!(wire.read_u32().unwrap(), 10);
        assert_eq!(wire.read_string().unwrap(), "zwp_linux_dmabuf_v1");
        assert_eq!(wire.read_u32().unwrap(), 3);
    }

    #[test]
    fn synthetic_dmabuf_global_waits_for_completed_capabilities() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.synthetic_feedback_available_for_test = true;
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 4), Action::Drop);
        let generation = ctx.global_generations[&10];
        assert!(!ctx.host_globals.contains_key(&10));
        assert!(ctx.host_to_client_queue.is_empty());
        assert_eq!(ctx.pending_dmabuf_globals.len(), 1);

        ctx.dmabuf_capabilities
            .get_mut(&generation)
            .expect("capability generation")
            .format_modifiers
            .push((0x3432_5258, 0));
        LinuxDmabufHandler::complete_capability_discovery(&mut ctx, generation);

        assert_eq!(
            ctx.host_globals.get(&10),
            Some(&HostGlobal {
                interface: dmabuf,
                version: 4,
            })
        );
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(ctx.registry_global_visibility[&1].get(&10), Some(&true));
    }

    #[test]
    fn synthetic_dmabuf_global_stays_hidden_without_supported_formats() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.synthetic_feedback_available_for_test = true;
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 4), Action::Drop);
        let generation = ctx.global_generations[&10];
        LinuxDmabufHandler::complete_capability_discovery(&mut ctx, generation);

        assert!(!ctx.host_globals.contains_key(&10));
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert_eq!(ctx.registry_global_visibility[&1].get(&10), Some(&false));
    }

    #[test]
    fn malformed_zero_version_global_is_not_exposed() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &seat, 0), Action::Drop);
        assert!(!ctx.host_globals.contains_key(&10));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn duplicate_global_name_does_not_overwrite_existing_metadata() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let first = "wl_seat".to_string();
        let second = "wl_output".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &first, 5), Action::Forward);
        assert_eq!(handler.on_global(&mut ctx, 10, &second, 3), Action::Drop);
        assert_eq!(
            ctx.host_globals.get(&10),
            Some(&HostGlobal {
                interface: first,
                version: 5,
            })
        );
    }

    #[test]
    fn internally_bound_objects_are_reserved_from_host_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;

        for (name, (interface, version)) in [
            (100, ("zwp_text_input_manager_v1", 1)),
            (101, ("zcr_text_input_extension_v1", 1)),
            (102, ("zcr_keyboard_extension_v1", 2)),
            (103, ("wl_shm", 1)),
        ] {
            let interface = interface.to_string();
            let _ = handler.on_global(&mut ctx, name, &interface, version);
        }

        assert_eq!(ctx.host_text_input_manager_v1_id, Some(2));
        assert_eq!(ctx.host_text_input_extension_v1_id, Some(3));
        assert_eq!(ctx.host_keyboard_extension_id.map(|id| id.0), Some(4));
        assert_eq!(ctx.host_shm_id, Some(5));
        assert_eq!(ctx.shadow_table.allocate_host_id(), 6);
        for (id, interface) in [
            (2, "zwp_text_input_manager_v1"),
            (3, "zcr_text_input_extension_v1"),
            (4, "zcr_keyboard_extension_v1"),
            (5, "wl_shm"),
        ] {
            assert_eq!(
                ctx.shadow_table.get_host_interface(id),
                Some(&interface.to_string())
            );
        }
    }

    #[test]
    fn synthetic_shm_bind_does_not_create_a_second_host_object() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_globals.insert(
            7,
            HostGlobal {
                interface: "wl_shm".to_string(),
                version: 1,
            },
        );
        ctx.host_shm_id = Some(2);
        ctx.shadow_table
            .track_host_interface(2, "wl_shm".to_string());
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        ctx.last_sender_id = 1;

        let mut handler = RegistryHandler;
        let id = ("wl_shm".to_string(), 1, 20);
        assert_eq!(handler.on_bind(&mut ctx, 7, &id), crate::wire::Action::Drop);

        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert_eq!(
            ctx.shadow_table.get_interface(20),
            Some(&"wl_shm".to_string())
        );
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "synthetic wl_shm bind must not issue a second host bind"
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "only mandatory wl_shm formats are advertised before host capabilities arrive"
        );
    }

    #[test]
    fn synthetic_text_input_managers_do_not_alias_shared_host_id() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_globals.insert(
            9,
            HostGlobal {
                interface: "zwp_text_input_manager_v3".to_string(),
                version: 1,
            },
        );
        ctx.host_text_input_manager_v1_id = Some(2);
        ctx.shadow_table
            .track_host_interface(2, "zwp_text_input_manager_v1".to_string());

        let mut handler = RegistryHandler;
        for guest_id in [20, 21] {
            let id = ("zwp_text_input_manager_v3".to_string(), 1, guest_id);
            assert_eq!(handler.on_bind(&mut ctx, 9, &id), crate::wire::Action::Drop);
            assert_eq!(ctx.shadow_table.get_host_id(guest_id), None);
        }

        ctx.last_sender_id = 20;
        let mut manager_handler = crate::handler::text_input::TextInputManagerV3Handler;
        assert_eq!(
            ZwpTextInputManagerV3Handler::on_destroy(&mut manager_handler, &mut ctx),
            crate::wire::Action::Drop
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(2),
            Some(&"zwp_text_input_manager_v1".to_string())
        );
        assert!(ctx.shadow_table.get_interface(21).is_some());
    }

    #[test]
    fn bind_rejects_unadvertised_interfaces_without_mapping() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let id = ("zcr_text_input_extension_v1".to_string(), 1, 20);

        assert_eq!(
            handler.on_bind(&mut ctx, 42, &id),
            crate::wire::Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert_eq!(ctx.shadow_table.get_interface(20), None);
    }

    #[test]
    fn duplicate_interfaces_keep_each_global_name_bindable() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;

        let seat = "wl_seat".to_string();
        assert_eq!(handler.on_global(&mut ctx, 10, &seat, 5), Action::Forward);
        assert_eq!(handler.on_global(&mut ctx, 11, &seat, 5), Action::Forward);

        let first = ("wl_seat".to_string(), 5, 20);
        let second = ("wl_seat".to_string(), 5, 21);
        assert_eq!(handler.on_bind(&mut ctx, 10, &first), Action::Drop);
        assert_eq!(handler.on_bind(&mut ctx, 11, &second), Action::Drop);
        assert!(ctx.shadow_table.get_host_id(20).is_some());
        assert!(ctx.shadow_table.get_host_id(21).is_some());
    }

    #[test]
    fn duplicate_internal_singletons_do_not_overwrite_the_first_binding() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let first_id = ctx.host_shm_id.expect("first SHM binding");
        let queue_len = ctx.client_to_host_queue.len();

        assert_eq!(handler.on_global(&mut ctx, 11, &shm, 1), Action::Drop);
        assert_eq!(ctx.host_shm_id, Some(first_id));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            queue_len,
            "a duplicate singleton must not issue a second host bind"
        );
    }

    #[test]
    fn removed_text_input_globals_keep_bound_manager_usable() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut registry = RegistryHandler;

        let manager = "zwp_text_input_manager_v1".to_string();
        let extension = "zcr_text_input_extension_v1".to_string();
        assert_eq!(registry.on_global(&mut ctx, 9, &manager, 1), Action::Drop);
        assert_eq!(
            registry.on_global(&mut ctx, 10, &extension, 11),
            Action::Drop
        );
        let manager_id = ctx
            .host_text_input_manager_v1_id
            .expect("host manager binding");
        let extension_id = ctx
            .host_text_input_extension_v1_id
            .expect("host extension binding");

        // The synthetic v3 global is advertised under the host manager's
        // numeric name. Bind a guest manager before removing the host globals.
        let guest_manager = ("zwp_text_input_manager_v3".to_string(), 1, 20);
        assert_eq!(registry.on_bind(&mut ctx, 9, &guest_manager), Action::Drop);

        ctx.last_sender_id = 1;
        assert_eq!(registry.on_global_remove(&mut ctx, 9), Action::Forward);
        assert_eq!(registry.on_global_remove(&mut ctx, 10), Action::Drop);
        assert_eq!(ctx.host_text_input_manager_v1_id, Some(manager_id));
        assert_eq!(ctx.host_text_input_extension_v1_id, Some(extension_id));

        // An already-bound manager remains usable after global_remove. New
        // text-input children must still be created through both retained
        // host-side factories.
        ctx.last_sender_id = 20;
        let mut manager_handler = crate::handler::text_input::TextInputManagerV3Handler;
        assert_eq!(
            ZwpTextInputManagerV3Handler::on_get_text_input(&mut manager_handler, &mut ctx, 21, 30),
            Action::Drop
        );
        assert!(ctx.text_inputs.contains_key(&21));
        assert!(ctx.text_inputs[&21].host_ext_id.is_some());
        assert!(ctx.client_to_host_queue.iter().any(|(message, _)| {
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) == manager_id
        }));
        assert!(ctx.client_to_host_queue.iter().any(|(message, _)| {
            u32::from_ne_bytes(message[0..4].try_into().unwrap()) == extension_id
        }));
    }

    #[test]
    fn global_remove_releases_bound_internal_singleton() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let host_id = ctx.host_shm_id.expect("SHM binding");
        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);
        assert_eq!(
            ctx.host_shm_id, None,
            "global_remove must release the stale singleton binding"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(host_id),
            None,
            "the removed host global must not keep stale dispatch state"
        );

        let keyboard = "zcr_keyboard_extension_v1".to_string();
        assert_eq!(handler.on_global(&mut ctx, 20, &keyboard, 2), Action::Drop);
        let keyboard_id = ctx
            .host_keyboard_extension_id
            .expect("keyboard extension binding")
            .0;
        assert_eq!(handler.on_global_remove(&mut ctx, 20), Action::Drop);
        assert_eq!(ctx.host_keyboard_extension_id.map(|id| id.0), None);
        assert_eq!(ctx.shadow_table.get_host_interface(keyboard_id), None);
    }

    #[test]
    fn keyboard_extension_global_remove_keeps_existing_children_alive() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let keyboard = "zcr_keyboard_extension_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 20, &keyboard, 2), Action::Drop);
        let manager_id = ctx
            .host_keyboard_extension_id
            .expect("keyboard extension binding");

        // Simulate the child created by get_extended_keyboard for an already
        // bound host wl_keyboard. The child owns its own destructor and must
        // outlive the manager global.
        crate::handler::keyboard::KeyboardHandler::ensure_extended_keyboard_bound(
            &mut ctx,
            HostId(40),
        );
        let child_id = *ctx
            .keyboard_to_extended_keyboard
            .get(&HostId(40))
            .expect("extended keyboard child");
        assert_eq!(
            ctx.shadow_table.get_host_interface(child_id.0),
            Some(&"zcr_extended_keyboard_v1".to_string())
        );

        assert_eq!(handler.on_global_remove(&mut ctx, 20), Action::Drop);
        assert_eq!(ctx.host_keyboard_extension_id, None);
        assert_eq!(
            ctx.shadow_table.get_host_interface(manager_id.0),
            None,
            "global removal retires only the manager dispatch entry"
        );
        assert_eq!(
            ctx.keyboard_to_extended_keyboard.get(&HostId(40)),
            Some(&child_id),
            "an existing child must remain mapped after manager removal"
        );
        assert_eq!(
            ctx.extended_keyboard_to_keyboard.get(&child_id),
            Some(&HostId(40)),
            "the reverse child mapping must remain routable"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(child_id.0),
            Some(&"zcr_extended_keyboard_v1".to_string()),
            "child dispatch metadata must survive global removal"
        );
    }

    #[test]
    fn global_remove_releases_internal_singleton_for_re_advertisement() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let first_host_id = ctx.host_shm_id.expect("first SHM binding");
        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);

        assert_eq!(
            ctx.host_shm_id, None,
            "a removed host global must not leave the singleton pointing at a stale object"
        );
        assert_eq!(ctx.host_shm_global_name, None);
        assert_eq!(
            ctx.shadow_table.get_host_interface(first_host_id),
            None,
            "stale internal object registrations must not receive later host events"
        );

        assert_eq!(handler.on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let replacement_host_id = ctx.host_shm_id.expect("replacement SHM binding");
        assert_ne!(
            replacement_host_id, first_host_id,
            "a re-advertised global must bind a fresh host object"
        );
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "the replacement global must issue a new host bind"
        );
    }

    #[test]
    fn removing_shm_global_keeps_old_host_proxy_id_reserved() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shm = "wl_shm".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let old_host_id = ctx.host_shm_id.expect("internal SHM binding");
        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);
        assert_ne!(
            ctx.shadow_table.allocate_host_id(),
            old_host_id,
            "wl_shm has no host-side destructor; its proxy ID must not be recycled"
        );
    }

    #[test]
    fn removing_legacy_aura_global_keeps_shell_and_surface_ids_reserved() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shell = "zaura_shell".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shell, 37), Action::Drop);
        let shell_id = ctx.host_zaura_shell_id.expect("legacy aura shell");
        let surface_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table
            .track_host_interface(surface_id, "zaura_surface".to_string());
        ctx.wl_surface_to_zaura_surface.insert(50, surface_id);

        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);
        assert_ne!(ctx.shadow_table.allocate_host_id(), shell_id);
        assert_ne!(ctx.shadow_table.allocate_host_id(), surface_id);
        assert_eq!(
            ctx.wl_surface_to_zaura_surface.get(&50),
            Some(&surface_id),
            "legacy aura children also survive global removal"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(surface_id),
            Some(&"zaura_surface".to_string()),
            "legacy child dispatch metadata remains live until surface destroy"
        );
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|(message, _)| message.len() < 8
                    || u16::from_ne_bytes(message[4..6].try_into().unwrap())
                        != ZAURA_SHELL_RELEASE),
            "legacy aura shell must not receive an unsupported release request"
        );
    }

    #[test]
    fn global_remove_keeps_aura_children_until_surface_destroy() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let shell = "zaura_shell".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &shell, 38), Action::Drop);
        let shell_id = ctx.host_zaura_shell_id.expect("aura shell binding");
        let surface_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table
            .track_host_interface(surface_id, "zaura_surface".to_string());
        ctx.wl_surface_to_zaura_surface.insert(50, surface_id);

        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);
        assert_eq!(ctx.host_zaura_shell_id, None);
        assert_eq!(ctx.shadow_table.get_host_interface(shell_id), None);
        assert_eq!(
            ctx.wl_surface_to_zaura_surface.get(&50),
            Some(&surface_id),
            "global removal must not destroy an existing aura child"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(surface_id),
            Some(&"zaura_surface".to_string())
        );
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "only the internal bind and manager release should be queued"
        );
        let shell_message = &ctx.client_to_host_queue[1].0;
        assert_eq!(
            u16::from_ne_bytes(shell_message[4..6].try_into().unwrap()),
            zaura_shell::REQ_RELEASE
        );
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(
                    |(message, _)| u16::from_ne_bytes(message[4..6].try_into().unwrap())
                        != zaura_surface::REQ_RELEASE
                ),
            "global removal must not release existing aura children"
        );
    }

    #[test]
    fn global_remove_destroys_internal_dmabuf_factory() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.synthetic_feedback_available_for_test = true;
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 10, &dmabuf, 4), Action::Drop);
        let host_id = ctx.host_dmabuf_id.expect("internal dmabuf binding");
        assert_eq!(
            handler.on_global_remove(&mut ctx, 10),
            Action::Drop,
            "a global removed before capability discovery was never visible"
        );
        assert_eq!(ctx.host_dmabuf_id, None);
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert_eq!(ctx.shadow_table.get_host_interface(host_id), None);
        let destroy = ctx
            .client_to_host_queue
            .last()
            .expect("internal dmabuf destroy request")
            .0
            .clone();
        assert_eq!(
            u16::from_ne_bytes(destroy[4..6].try_into().unwrap()),
            zwp_linux_dmabuf_v1::REQ_DESTROY
        );
        assert_eq!(
            u32::from_ne_bytes(destroy[0..4].try_into().unwrap()),
            host_id
        );
    }

    #[test]
    fn hidden_global_cannot_be_bound_when_gpu_is_disabled() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let dmabuf = "zwp_linux_dmabuf_v1".to_string();

        assert_eq!(handler.on_global(&mut ctx, 42, &dmabuf, 5), Action::Drop);
        let bind = (dmabuf, 5, 20);
        assert_eq!(handler.on_bind(&mut ctx, 42, &bind), Action::Drop);
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
    }

    #[test]
    fn global_remove_forwards_only_visible_globals_and_invalidates_bind() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();
        assert_eq!(handler.on_global(&mut ctx, 10, &seat, 5), Action::Forward);
        assert_eq!(handler.on_global_remove(&mut ctx, 99), Action::Drop);
        assert_eq!(handler.on_global_remove(&mut ctx, 10), Action::Forward);

        let bind = (seat, 5, 20);
        assert_eq!(handler.on_bind(&mut ctx, 10, &bind), Action::Drop);
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
    }

    #[test]
    fn stale_remove_cannot_invalidate_a_re_advertised_global_generation() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table.map_id(11, 101);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.shadow_table
            .track_interface(11, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();

        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global(&mut ctx, 7, &seat, 5), Action::Forward);
        ctx.last_sender_id = 101;
        assert_eq!(handler.on_global(&mut ctx, 7, &seat, 5), Action::Forward);

        // Registry A observes removal and receives a replacement with the
        // same numeric name before registry B delivers the old remove.
        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Forward);
        assert_eq!(handler.on_global(&mut ctx, 7, &seat, 5), Action::Forward);

        // B's delayed remove belongs to generation 1. It must be forwarded to
        // B, but must not tombstone generation 2 advertised to A.
        ctx.last_sender_id = 101;
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Forward);
        assert!(
            !ctx.removed_host_globals.contains(&7),
            "an old registry remove must not invalidate the replacement"
        );

        ctx.last_sender_id = 10;
        let bind = (seat, 5, 20);
        assert_eq!(
            handler.on_bind(&mut ctx, 7, &bind),
            Action::Drop,
            "the replacement global must remain bindable from registry A"
        );
        assert!(ctx.shadow_table.get_host_id(20).is_some());
    }

    #[test]
    fn re_advertised_name_replaces_visible_metadata_before_hidden_global() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();
        let keyboard_extension = "zcr_keyboard_extension_v1".to_string();

        ctx.last_sender_id = 1;
        assert_eq!(handler.on_global(&mut ctx, 7, &seat, 5), Action::Forward);
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Forward);

        // The host reused name 7 for a hidden singleton. The old visible
        // wl_seat metadata must not survive and authorize a stale bind.
        assert_eq!(
            handler.on_global(&mut ctx, 7, &keyboard_extension, 2),
            Action::Drop
        );
        assert!(
            !ctx.host_globals.contains_key(&7),
            "reused name must not retain the old visible interface metadata"
        );
        assert_eq!(
            ctx.hidden_host_globals.get(&7),
            Some(&keyboard_extension),
            "the replacement hidden global must own the numeric name"
        );

        let stale_bind = (seat, 5, 20);
        assert_eq!(
            handler.on_bind(&mut ctx, 7, &stale_bind),
            Action::Drop,
            "a bind for the previous interface must be rejected after name reuse"
        );
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
    }

    #[test]
    fn re_advertised_hidden_name_replaces_hidden_metadata_before_visible_global() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let keyboard_extension = "zcr_keyboard_extension_v1".to_string();
        let seat = "wl_seat".to_string();

        ctx.last_sender_id = 1;
        assert_eq!(
            handler.on_global(&mut ctx, 7, &keyboard_extension, 2),
            Action::Drop
        );
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Drop);

        // Name 7 is now a normal visible global. A stale hidden marker would
        // make its later global_remove disappear instead of reaching the guest.
        assert_eq!(handler.on_global(&mut ctx, 7, &seat, 5), Action::Forward);
        assert!(
            !ctx.hidden_host_globals.contains_key(&7),
            "reused name must not retain hidden classification"
        );
        assert_eq!(
            ctx.host_globals.get(&7),
            Some(&HostGlobal {
                interface: seat.clone(),
                version: 5,
            })
        );
        assert_eq!(
            handler.on_global_remove(&mut ctx, 7),
            Action::Forward,
            "the visible replacement must forward its own removal"
        );
    }

    #[test]
    fn delayed_remove_does_not_reset_replacement_internal_binding() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 100);
        ctx.shadow_table.map_id(11, 101);
        ctx.shadow_table
            .track_interface(10, "wl_registry".to_string());
        ctx.shadow_table
            .track_interface(11, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let keyboard_extension = "zcr_keyboard_extension_v1".to_string();

        ctx.last_sender_id = 100;
        assert_eq!(
            handler.on_global(&mut ctx, 7, &keyboard_extension, 2),
            Action::Drop
        );
        ctx.last_sender_id = 101;
        assert_eq!(
            handler.on_global(&mut ctx, 7, &keyboard_extension, 2),
            Action::Drop
        );
        let old_id = ctx
            .host_keyboard_extension_id
            .expect("initial keyboard extension binding");

        // Registry A sees the host generation disappear and immediately sees
        // the replacement. The old internal object is retired and a fresh
        // binding is created for the new generation.
        ctx.last_sender_id = 100;
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Drop);
        assert_eq!(
            handler.on_global(&mut ctx, 7, &keyboard_extension, 2),
            Action::Drop
        );
        let replacement_id = ctx
            .host_keyboard_extension_id
            .expect("replacement keyboard extension binding");
        assert_ne!(replacement_id, old_id);
        assert_eq!(
            ctx.shadow_table.get_host_interface(replacement_id.0),
            Some(&keyboard_extension)
        );

        // Registry B's queued remove belongs to generation 1. It must not
        // tear down the generation-2 internal object that is now active.
        ctx.last_sender_id = 101;
        assert_eq!(handler.on_global_remove(&mut ctx, 7), Action::Drop);
        assert_eq!(ctx.host_keyboard_extension_id, Some(replacement_id));
        assert_eq!(
            ctx.shadow_table.get_host_interface(replacement_id.0),
            Some(&keyboard_extension)
        );
    }

    #[test]
    fn bind_rejects_version_above_advertised_global() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();
        assert_eq!(handler.on_global(&mut ctx, 10, &seat, 3), Action::Forward);

        let bind = (seat, 4, 20);
        assert_eq!(handler.on_bind(&mut ctx, 10, &bind), Action::Drop);
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
    }

    #[test]
    fn ordinary_globals_are_capped_to_supported_versions() {
        assert_eq!(advertised_global_version("wl_compositor", 6), 4);
        assert_eq!(advertised_global_version("wl_output", 4), 3);
        assert_eq!(advertised_global_version("wl_seat", 10), 5);
        assert_eq!(advertised_global_version("wl_data_device_manager", 99), 3);
        assert_eq!(advertised_global_version("xdg_wm_base", 7), 3);
        assert_eq!(advertised_global_version("zwp_linux_dmabuf_v1", 5), 4);

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.map_id(1, 1);
        ctx.shadow_table
            .track_interface(1, "wl_registry".to_string());
        let mut handler = RegistryHandler;
        let seat = "wl_seat".to_string();
        assert_eq!(handler.on_global(&mut ctx, 10, &seat, 10), Action::Drop);
        assert_eq!(
            ctx.host_globals.get(&10),
            Some(&HostGlobal {
                interface: seat,
                version: 5
            })
        );
        let message = ctx
            .host_to_client_queue
            .pop()
            .expect("capped global event")
            .0;
        assert_eq!(
            u32::from_ne_bytes(message[message.len() - 4..].try_into().unwrap()),
            5,
            "the guest must receive the capped version"
        );
    }
}
