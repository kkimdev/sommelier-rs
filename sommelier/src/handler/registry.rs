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

use crate::handler::callback::queue_internal_sync;
use crate::handler::linux_dmabuf::GUEST_DMABUF_VERSION;
use crate::handler::shm::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, register_guest_shm,
};
use crate::handler::Handlers;
use crate::protocols::aura_shell::zaura_shell;
use crate::protocols::fractional_scale_v1::ALLOWED_INTERFACES as FRACTIONAL_SCALE_ALLOWED;
use crate::protocols::gtk::{gtk_shell1, ALLOWED_INTERFACES as GTK_ALLOWED};
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::linux_dmabuf_v1::{
    zwp_linux_dmabuf_v1, ALLOWED_INTERFACES as DMABUF_ALLOWED,
};
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::text_input_unstable_v3::{
    zwp_text_input_manager_v3, ALLOWED_INTERFACES as TEXT_INPUT_ALLOWED,
};
use crate::protocols::viewporter::ALLOWED_INTERFACES as VIEWPORTER_ALLOWED;
use crate::protocols::wayland::ALLOWED_INTERFACES as WL_ALLOWED;
use crate::protocols::wayland::{
    wl_compositor, wl_display, wl_fixes, wl_output, wl_registry, wl_seat, wl_shm, wl_surface,
};
use crate::protocols::xdg_decoration_unstable_v1::ALLOWED_INTERFACES as XDG_DECORATION_ALLOWED;
use crate::protocols::xdg_output_unstable_v1::ALLOWED_INTERFACES as XDG_OUTPUT_ALLOWED;
use crate::protocols::xdg_shell::{xdg_wm_base, ALLOWED_INTERFACES as XDG_ALLOWED};
use crate::protocols::Iface;
use crate::state::{
    Context, GlobalBinding, GtkShellState, HostAuraShell, HostGlobal, HostId, InternalCallback,
    PendingDmabufGlobal, INERT_BIND_INTERFACE,
};
use crate::wire::{queue_or_drop, Action};
use log::error;

/// The newest `zwp_linux_dmabuf_v1` the proxy binds on the host. This is a
/// different fact from the guest version: the legacy format and modifier
/// events, which are deprecated from the feedback version on, are what the
/// proxy synthesizes the guest's feedback object from. `create_immed` is
/// available since v2 and explicit modifier events since v3, so bind at v3
/// when the host supports it.
const LINUX_DMABUF_CAPABILITY_VERSION: u32 = zwp_linux_dmabuf_v1::EVT_MODIFIER_SINCE;
// The host binding has no feedback requests, so no host feedback object
// exists (see `ZwpLinuxDmabufFeedbackV1Handler for LinuxDmabufHandler`).
const _: () =
    assert!(LINUX_DMABUF_CAPABILITY_VERSION < zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK_SINCE);
/// The newest host `zwp_text_input_manager_v1` the proxy binds: the vendored
/// XML's. The generated code knows no message added after it, so a newer
/// binding could deliver events whose unknown opcodes end the session.
const TEXT_INPUT_MANAGER_VERSION: u32 =
    crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1::VERSION;
/// The newest host `zcr_text_input_extension_v1` the proxy binds, for the
/// same reason.
const TEXT_INPUT_EXTENSION_VERSION: u32 =
    crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1::VERSION;
/// The `wl_compositor` ceiling, at which a host of
/// [`WL_COMPOSITOR_HOST_VERSION`] or newer is offered, as ChromiumOS
/// Sommelier offers it with `--support-damage-buffer`: v4 adds only
/// `wl_surface.damage_buffer`, which the proxy records and sends the host
/// as `wl_surface.damage` at commit. The proxy does not implement the v5
/// and v6 surface semantics: it reads attach's offset, which v5 replaced
/// with `wl_surface.offset`, and the generated version check rejects the
/// latter, so raising the ceiling needs that handled first.
const WL_COMPOSITOR_VERSION: u32 = 4;
const _: () = assert!(WL_COMPOSITOR_VERSION < wl_surface::REQ_OFFSET_SINCE);
/// v3 provides buffer scale/transform; damage_buffer (v4) is emulated locally.
const WL_COMPOSITOR_HOST_VERSION: u32 = 3;
/// The `xdg_wm_base` ceiling, ChromiumOS Sommelier's
/// (`SL_XDG_SHELL_MAX_VERSION` in `sommelier-xdg-shell.h`). The vendored
/// XML is newer: v4 to v7 add configure bounds, window manager
/// capabilities, and the suspended and constrained states, which the proxy
/// has not been checked against.
const XDG_SHELL_VERSION: u32 = 3;
/// The `wl_shm` version the proxy offers the guest and binds on the host.
/// The proxy implements the guest's `wl_shm` itself, as a v1 compatibility
/// shim (`handler/shm/`), so a newer host or XML version is not offered.
const WL_SHM_VERSION: u32 = 1;
/// The `wl_output` ceiling; [`forwarded_global_version`] says why. The
/// vendored XML is newer: v4 adds the `name` and `description` events.
const WL_OUTPUT_VERSION: u32 = 3;
/// The `wl_seat` ceiling; [`forwarded_global_version`] says why. The
/// vendored XML is newer: v6 to v10 add touch shape and orientation,
/// high-resolution scroll and scroll-direction events, and, at v10, the
/// `repeated` key state.
pub(crate) const WL_SEAT_VERSION: u32 = 5;
/// The newest `zcr_keyboard_extension_v1` the proxy binds: the vendored
/// XML's. The proxy handles all of it, including the v2 `peek_key` event
/// that reports keys the IME consumed.
const KEYBOARD_EXTENSION_VERSION: u32 =
    crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::VERSION;
/// The oldest `zaura_shell` this proxy binds, the v6+ contract ChromiumOS
/// Sommelier uses. Every Aura child takes its shell's version, so this
/// floor is what lets the proxy create Aura surfaces and outputs, and send
/// `set_application_id` and `set_startup_id`, without a per-object version
/// check. The assertions keep that true if the vendored protocol changes.
const ZAURA_SHELL_MIN_VERSION: u32 = 6;
const _: () = assert!(
    crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_OUTPUT_SINCE <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);
const _: () = assert!(
    crate::protocols::aura_shell::zaura_surface::REQ_SET_STARTUP_ID_SINCE
        <= ZAURA_SHELL_MIN_VERSION
);
/// The newest `zaura_shell` this proxy binds: the interface version of the
/// vendored `third_party/protocols/aura-shell.xml`. The generated code knows
/// no request or event added after it, so binding a newer host shell would
/// let Exo send Aura events whose unknown opcodes end the client's session.
const ZAURA_SHELL_MAX_VERSION: u32 = crate::protocols::aura_shell::zaura_shell::VERSION;

/// The protocols whose host globals the proxy forwards, apart from
/// `zxdg_decoration_manager_v1`, which only `--xdg-decoration` forwards.
/// The globals it binds internally or replaces are handled before this.
const FORWARDED_PROTOCOLS: [&[&str]; 7] = [
    WL_ALLOWED,
    XDG_ALLOWED,
    DMABUF_ALLOWED,
    VIEWPORTER_ALLOWED,
    TEXT_INPUT_ALLOWED,
    FRACTIONAL_SCALE_ALLOWED,
    XDG_OUTPUT_ALLOWED,
];

/// Whether the proxy forwards a host global of `interface` to the guest.
fn forwards_host_global(ctx: &Context, interface: &str) -> bool {
    FORWARDED_PROTOCOLS
        .iter()
        .any(|interfaces| interfaces.contains(&interface))
        || (XDG_DECORATION_ALLOWED.contains(&interface) && ctx.settings().xdg_decoration)
}

/// The version the guest is offered of a host global of `interface` that
/// the host advertises at `host_version`, or `None` if the proxy does not
/// forward it.
///
/// The offer is never newer than the vendored XML: the generated code knows
/// no message added after it, so a newer global would let the guest send
/// requests whose unknown opcodes end its session.
///
/// Some interfaces have a lower ceiling, as in ChromiumOS Sommelier's
/// client-facing globals. The generated code can decode newer XML
/// messages, but that does not mean the proxy implements the semantics of
/// every newer version. In particular, the C++ proxy caps `wl_seat` at v5
/// and `wl_output` at v3; it also exposes the compositor only up to the
/// version for which its surface damage handling is implemented. Keep the
/// guest advertisement aligned with those ceilings so clients do not start
/// using requests/events that this proxy merely forwards without the
/// required coordinate/focus handling.
fn forwarded_global_version(ctx: &Context, interface: &str, host_version: u32) -> Option<u32> {
    if !forwards_host_global(ctx, interface) {
        return None;
    }
    let xml_version = Iface::from_name(interface)?.version();
    // ChromiumOS implements wl_surface.damage_buffer in terms of the legacy
    // damage request, so a v3 host compositor is offered as v4. Hosts below
    // v3 retain their original version; the proxy does not expose v5/v6
    // surface semantics.
    let host_version =
        if interface == wl_compositor::NAME && host_version >= WL_COMPOSITOR_HOST_VERSION {
            WL_COMPOSITOR_VERSION
        } else {
            host_version
        };
    let ceiling = match interface {
        wl_compositor::NAME => WL_COMPOSITOR_VERSION,
        wl_output::NAME => WL_OUTPUT_VERSION,
        wl_seat::NAME => WL_SEAT_VERSION,
        xdg_wm_base::NAME => XDG_SHELL_VERSION,
        _ => xml_version,
    };
    Some(host_version.min(ceiling).min(xml_version))
}

fn keyboard_extension_version(host_version: u32) -> u32 {
    host_version.min(KEYBOARD_EXTENSION_VERSION)
}

fn should_bind_internal_dmabuf(gpu_accel: bool, virtwl_supports_dmabuf: bool) -> bool {
    gpu_accel || virtwl_supports_dmabuf
}

/// Whether a registry still lists global `name` at `generation`: it has not
/// seen that advertisement's `global_remove` yet.
pub(crate) fn global_still_seen(ctx: &Context, name: u32, generation: Option<u64>) -> bool {
    registry_listing(ctx, name, generation).is_some()
}

/// The lowest host registry that still lists global `name` at
/// `generation`, if any (see [`global_still_seen`]).
fn registry_listing(ctx: &Context, name: u32, generation: Option<u64>) -> Option<u32> {
    ctx.registries
        .iter()
        .filter(|(_, record)| {
            record
                .listed_generation(name)
                .is_some_and(|listed| Some(listed) == generation)
        })
        .map(|(&registry, _)| registry)
        .min()
}

/// Whether the host removed global `name` at `generation`, its current
/// advertisement, as a registry has seen.
fn global_removal_seen(ctx: &Context, name: u32, generation: u64) -> bool {
    ctx.removed_global_generations.get(&name) == Some(&generation)
        && ctx.global_generations.get(&name) == Some(&generation)
}

/// Retire global `name`, whose current advertisement the host removed,
/// once no registry still lists it ([`global_still_seen`]): reset the
/// internal binding it backed, and drop its connection-wide metadata,
/// whose absence refuses new binds.
///
/// `wl_registry.global_remove` invalidates only the *global name*. Any
/// object that was already bound remains valid until its own destructor
/// (the Wayland core protocol explicitly requires this), so an internal
/// object keeps its ID and singleton state reserved, and queued host events
/// cannot be routed to a later object that happens to reuse the ID.
fn retire_unseen_global(ctx: &mut Context, name: u32) {
    ctx.removed_global_generations.remove(&name);
    reset_internal_binding_for_global(ctx, name);
    crate::handler::shm::retire_guest_shms(ctx, name);
    ctx.singleton_candidates.remove(&name);
    ctx.host_globals.remove(&name);
    if ensure_all_bound(ctx).is_err() {
        // The failed bind ended the session in `ctx`, which is all its
        // result says; the message's handler cannot do more.
        debug_assert!(ctx.is_fatal());
    }
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
        .registries
        .get(&registry_id)
        .and_then(|record| record.generation(name));
    let already_seen = ctx
        .registries
        .get(&registry_id)
        .is_some_and(|record| record.lists(name));

    if already_seen && previous_generation == current_generation {
        return false;
    }

    let registry_removed = ctx
        .registries
        .get(&registry_id)
        .is_some_and(|record| record.is_removed(name));
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
    ctx.registries
        .entry(registry_id)
        .or_default()
        .list(name, generation);
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
    queue_or_drop(
        &mut ctx.host_to_client_queue,
        wl_registry::evt::global(registry_guest_id, name, interface, version),
        format_args!("synthetic global {name}:{interface}"),
    )
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
    set_registry_global_visibility(ctx, registry_host_id, name, false);
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
            .registries
            .get(&pending.registry_host_id)
            .and_then(|record| record.listed_generation(pending.name))
            == Some(generation)
            && ctx.global_generations.get(&pending.name) == Some(&generation);
        if !is_current {
            continue;
        }

        ctx.host_globals.insert(
            pending.name,
            HostGlobal {
                interface: zwp_linux_dmabuf_v1::NAME.to_string(),
                version: pending.version,
            },
        );
        let visible = queue_synthetic_global_for_registry(
            ctx,
            pending.registry_host_id,
            pending.name,
            zwp_linux_dmabuf_v1::NAME,
            pending.version,
        );
        set_registry_global_visibility(ctx, pending.registry_host_id, pending.name, visible);
        if !visible {
            ctx.host_globals.remove(&pending.name);
        }
    }
}

fn queue_internal_bind(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    interface: Iface,
    version: u32,
    host_id: u32,
) -> bool {
    queue_or_drop(
        &mut ctx.client_to_host_queue,
        wl_registry::req::bind(registry_host_id, name, interface.name(), version, host_id),
        format_args!("internal bind {name}:{interface}"),
    )
}

/// Bind host global `name` as `host_id` for the proxy's own use.
///
/// # Returns
///
/// The session's end if the bind cannot be encoded, which a static
/// interface name never causes.
fn bind_internally(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    interface: Iface,
    version: u32,
    host_id: u32,
) -> Result<(), Action> {
    if queue_internal_bind(ctx, registry_host_id, name, interface, version, host_id) {
        Ok(())
    } else {
        Err(ctx.fail(format_args!("Failed to bind host {interface} internally")))
    }
}

/// A host global the proxy binds once per connection, for its own use.
///
/// Every advertisement of one is a candidate
/// (`Context::singleton_candidates`), and [`ensure_bound`] binds one
/// whenever the proxy has none. A host may advertise one under a new name
/// before it removes the old one; the new name is then bound when the old
/// one retires, so the proxy keeps the capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Singleton {
    Dmabuf,
    KeyboardExtension,
    AuraShell,
    Shm,
}

impl Singleton {
    const ALL: [Self; 4] = [
        Self::Dmabuf,
        Self::KeyboardExtension,
        Self::AuraShell,
        Self::Shm,
    ];

    fn interface(self) -> Iface {
        match self {
            Self::Dmabuf => Iface::ZwpLinuxDmabufV1,
            Self::KeyboardExtension => Iface::ZcrKeyboardExtensionV1,
            Self::AuraShell => Iface::ZauraShell,
            Self::Shm => Iface::WlShm,
        }
    }

    /// The name of the global the proxy bound, if it has a binding.
    fn bound_name(self, ctx: &Context) -> Option<u32> {
        match self {
            Self::Dmabuf => ctx.host_dmabuf_global_name,
            Self::KeyboardExtension => ctx.host_keyboard_extension_global_name,
            Self::AuraShell => ctx.host_zaura_shell.map(|shell| shell.global_name),
            Self::Shm => ctx.host_shm_global_name,
        }
    }

    /// Bind host global `name`, which the host advertises at `version`,
    /// through host registry `registry_host_id`, as the one binding.
    ///
    /// # Returns
    ///
    /// The session's end if the bind cannot be encoded.
    fn bind(
        self,
        ctx: &mut Context,
        registry_host_id: u32,
        name: u32,
        version: u32,
    ) -> Result<(), Action> {
        let interface = self.interface();
        match self {
            Self::Dmabuf => {
                let generation = ctx
                    .global_generations
                    .get(&name)
                    .copied()
                    .unwrap_or_default();
                // Bind at v3 when available so the proxy can collect legacy
                // format/modifier pairs and synthesize the v4 feedback
                // object. v2 still keeps create_immed available on older
                // hosts. The host compositor sends format/modifier events to
                // the dmabuf factory object after we bind it.
                let bound_version = version.min(LINUX_DMABUF_CAPABILITY_VERSION);
                let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
                ctx.host_dmabuf_id = Some(host_id);
                ctx.host_dmabuf_global_name = Some(name);
                ctx.host_dmabuf_generation = Some(generation);
                ctx.dmabuf_capabilities.entry(generation).or_default();
                bind_internally(
                    ctx,
                    registry_host_id,
                    name,
                    interface,
                    bound_version,
                    host_id,
                )?;
                if can_synthesize_dmabuf_feedback(ctx, version) {
                    queue_internal_sync(ctx, InternalCallback::DmabufCapabilities { generation });
                }
            }
            Self::KeyboardExtension => {
                // A ChromeOS protocol that enables the ack-key mechanism for
                // controlling host accelerator processing. Version 2
                // additionally reports physical keys consumed by IME via
                // peek_key. The factory's ID is also the sender of
                // get_extended_keyboard requests.
                let bound_version = keyboard_extension_version(version);
                let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
                ctx.host_keyboard_extension_id = Some(HostId(host_id));
                ctx.host_keyboard_extension_global_name = Some(name);
                bind_internally(
                    ctx,
                    registry_host_id,
                    name,
                    interface,
                    bound_version,
                    host_id,
                )?;
                log::debug!("Bound {interface} v{bound_version} (host_id={host_id})");
            }
            Self::AuraShell => {
                // For ChromeOS shelf integration: the proxy creates
                // zaura_surface objects from it to set application IDs and
                // GTK startup IDs.
                let bound_version = version.min(ZAURA_SHELL_MAX_VERSION);
                let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
                ctx.host_zaura_shell = Some(HostAuraShell {
                    id: host_id,
                    global_name: name,
                    version: bound_version,
                });
                bind_internally(
                    ctx,
                    registry_host_id,
                    name,
                    interface,
                    bound_version,
                    host_id,
                )?;
                // Placement retries what a guest set up before the shell.
                crate::handler::window_placement::shell_bound(ctx, bound_version);
                log::debug!("Bound {interface} internally (host_id={host_id})");
            }
            Self::Shm => {
                // wl_shm is emulated: on_bind drops the guest request and
                // sends synthetic format events. The host's format events
                // reach this binding, which records them (`shm::pool`).
                let host_id = ctx.shadow_table.create_host_only(interface, WL_SHM_VERSION);
                ctx.host_shm_id = Some(host_id);
                ctx.host_shm_global_name = Some(name);
                bind_internally(
                    ctx,
                    registry_host_id,
                    name,
                    interface,
                    WL_SHM_VERSION,
                    host_id,
                )?;
            }
        }
        Ok(())
    }
}

/// Record host global `name` of `singleton`, which the host advertises at
/// `version`, as a candidate binding.
fn add_candidate(ctx: &mut Context, singleton: Singleton, name: u32, version: u32) {
    ctx.singleton_candidates.insert(
        name,
        HostGlobal {
            interface: singleton.interface().name().to_string(),
            version,
        },
    );
}

/// Bind `singleton` if the proxy has no binding of it: the lowest candidate
/// that a registry still lists and whose removal no registry has seen,
/// through the lowest registry that lists it.
///
/// # Returns
///
/// The session's end if the bind cannot be encoded.
fn ensure_bound(ctx: &mut Context, singleton: Singleton) -> Result<(), Action> {
    if singleton.bound_name(ctx).is_some() {
        return Ok(());
    }
    let candidate = ctx
        .singleton_candidates
        .iter()
        .filter(|(_, global)| global.interface == singleton.interface().name())
        .filter_map(|(&name, global)| {
            let generation = *ctx.global_generations.get(&name)?;
            if global_removal_seen(ctx, name, generation) {
                return None;
            }
            let registry_host_id = registry_listing(ctx, name, Some(generation))?;
            Some((name, registry_host_id, global.version))
        })
        .min();
    match candidate {
        Some((name, registry_host_id, version)) => {
            singleton.bind(ctx, registry_host_id, name, version)
        }
        None => Ok(()),
    }
}

/// [`ensure_bound`] for every singleton, after a binding may have retired.
fn ensure_all_bound(ctx: &mut Context) -> Result<(), Action> {
    for singleton in Singleton::ALL {
        ensure_bound(ctx, singleton)?;
    }
    Ok(())
}

fn internal_binding_matches(ctx: &Context, name: u32) -> bool {
    ctx.host_dmabuf_global_name == Some(name)
        || ctx.host_shm_global_name == Some(name)
        || ctx.host_keyboard_extension_global_name == Some(name)
        || ctx
            .host_zaura_shell
            .is_some_and(|shell| shell.global_name == name)
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
            ctx.client_to_host_queue
                .push(zwp_linux_dmabuf_v1::req::destroy(host_id));
            ctx.retire_destroyed_host_object(host_id);
        }
        ctx.host_dmabuf_global_name = None;
        clear_host_shm_dmabuf_formats(ctx);
        if let Some(generation) = generation {
            crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
        }
    }
    if ctx.host_shm_global_name == Some(name) {
        if let Some(host_id) = ctx.host_shm_id.take() {
            ctx.shadow_table.retire_host(host_id);
        }
        ctx.host_shm_global_name = None;
        clear_host_shm_wl_formats(ctx);
    }
    // The text-input managers have no destructor, and text inputs still
    // create children from them after their globals go, so they are never
    // reset or rebound (see `text_input_manager_global`).
    if ctx.host_keyboard_extension_global_name == Some(name) {
        if let Some(host_id) = ctx.host_keyboard_extension_id.take() {
            ctx.shadow_table.retire_host(host_id.0);
        }
        ctx.host_keyboard_extension_global_name = None;
        // `global_remove` invalidates only the manager global name. It does
        // not destroy `zcr_extended_keyboard_v1` children that were already
        // created with get_extended_keyboard; those children have their own
        // destructor and remain usable until the corresponding wl_keyboard
        // is released. Keep both maps and the child dispatch registrations so
        // queued peek_key events remain routable after the manager disappears.
    }
    if let Some(shell) = ctx
        .host_zaura_shell
        .take_if(|shell| shell.global_name == name)
    {
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
        crate::handler::aura_objects::release_shell(ctx, shell);
    }
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
    ctx.removed_global_generations.remove(&name);
    ctx.singleton_candidates.remove(&name);
    ctx.host_globals.remove(&name);
}

/// Advertise host global `name` to the guest on the sending registry as
/// `interface` at `version`, in place of the host's advertisement.
fn advertise_synthetic_global(ctx: &mut Context, name: u32, interface: &str, version: u32) {
    ctx.host_globals.insert(
        name,
        HostGlobal {
            interface: interface.to_string(),
            version,
        },
    );
    let visible = queue_synthetic_global(ctx, name, interface, version);
    record_registry_global_visibility(ctx, name, visible);
}

/// Keep host global `name` of `interface`, which the proxy uses itself,
/// from the guest.
fn hide_global(ctx: &mut Context, name: u32) {
    record_registry_global_visibility(ctx, name, false);
}

fn record_registry_global_visibility(ctx: &mut Context, name: u32, visible: bool) {
    set_registry_global_visibility(ctx, ctx.last_sender_id, name, visible);
}

/// Host registry `registry_host_id`'s current advertisement of global
/// `name` was `visible` to the guest, or not. Only a registry that lists
/// the name has an advertisement to classify.
fn set_registry_global_visibility(
    ctx: &mut Context,
    registry_host_id: u32,
    name: u32,
    visible: bool,
) {
    if let Some(record) = ctx.registries.get_mut(&registry_host_id) {
        record.set_visibility(name, visible);
    }
}

fn can_synthesize_dmabuf_feedback(ctx: &Context, version: u32) -> bool {
    if version < LINUX_DMABUF_CAPABILITY_VERSION {
        return false;
    }
    if ctx.virtgpu().is_some() {
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

/// How the proxy serves an admitted guest bind.
#[derive(Debug, PartialEq, Eq)]
enum BindAdmission {
    /// Bind the host global of this advertisement generation. Bound
    /// objects record the generation so that a delayed `global_remove` of
    /// an older advertisement cannot retire a binding of its replacement.
    Live { generation: u64 },
    /// The registry advertised the global and then removed it, and the
    /// bind crossed the removal on the wire. libwayland-server makes this
    /// race harmless, so the proxy creates an inert guest object instead
    /// of ending the session.
    Inert,
}

/// Decide whether the guest may bind global `name` as `interface` at
/// `requested_version` through the registry that sent the request.
///
/// # Returns
///
/// How to serve the bind, or `None` to reject it as a protocol error. A
/// registry that has seen the name must agree with the connection-wide
/// generation (checked below), so that generation is the one bound.
fn admit_guest_bind(
    ctx: &Context,
    name: u32,
    interface: &str,
    requested_version: u32,
) -> Option<BindAdmission> {
    // gtk_shell1 is never a host global: the proxy advertises it for the
    // host zaura_shell.
    if !forwards_host_global(ctx, interface) && !GTK_ALLOWED.contains(&interface) {
        return None;
    }

    // A global is advertised independently on each wl_registry object.
    // Client→host requests carry the guest registry ID, whereas lifecycle
    // tracking is keyed by the paired host registry ID.
    let registry_host_id = ctx
        .shadow_table
        .get_host_id(ctx.last_sender_id)
        .unwrap_or(ctx.last_sender_id);
    // Once one registry has observed global_remove, a bind through it never
    // reaches the host, even while another registry is still receiving the
    // old generation. A bind through a registry that has not seen it yet is
    // forwarded below: that relies on the host keeping a removed global
    // bindable until wl_global_destroy, as Exo does by deferring it after
    // wl_global_remove. A host that destroyed it at once would answer the
    // bind with INVALID_OBJECT, as it would a native client's.
    if let Some(removal) = ctx
        .registries
        .get(&registry_host_id)
        .and_then(|record| record.removal(name))
    {
        let inert = interface == INERT_BIND_INTERFACE
            && removal
                .inert_bind_version
                .is_some_and(|version| (1..=version).contains(&requested_version));
        return inert.then_some(BindAdmission::Inert);
    }

    let global = ctx.host_globals.get(&name)?;
    if let Some(record) = ctx.registries.get(&registry_host_id) {
        if !record.lists(name) {
            return None;
        }
        if record.generation(name) != ctx.global_generations.get(&name).copied() {
            return None;
        }
    }

    // The v3 manager is a synthetic global backed by the host v1 manager.
    // on_global records the synthetic interface under the same numeric name.
    // Checking the advertised version here also prevents a client from
    // forwarding a bind for a protocol version that the proxy did not expose.
    let version_is_advertised = global.interface == interface
        && requested_version > 0
        && requested_version <= global.version;
    if !version_is_advertised {
        return None;
    }
    // `note_registry_global` records a generation before any name enters
    // `host_globals`, and generations are never removed, so this lookup
    // cannot fail for an advertised name.
    let generation = *ctx.global_generations.get(&name)?;
    Some(BindAdmission::Live { generation })
}

/// Record the sending registry's advertisement of host global `name`,
/// and, if it starts a new generation of the name, replace the old one's
/// metadata.
///
/// # Returns
///
/// The action for the event if the proxy handles it no further: `Drop`
/// for a malformed or duplicate advertisement, or the session's end.
fn accept_global(
    ctx: &mut Context,
    name: u32,
    interface: &str,
    version: u32,
) -> Result<(), Action> {
    let registry_id = ctx.last_sender_id;
    let previous_generation = ctx.global_generations.get(&name).copied();
    let registry_previous_generation = ctx
        .registries
        .get(&registry_id)
        .and_then(|record| record.generation(name));
    let registry_removed = ctx
        .registries
        .get(&registry_id)
        .is_some_and(|record| record.is_removed(name));
    if version == 0 {
        log::warn!(
            "Ignoring malformed Wayland global {}:{} with version 0",
            name,
            interface
        );
        return Err(Action::Drop);
    }
    if !note_registry_global(ctx, name) {
        log::warn!(
            "Ignoring duplicate Wayland global name {} in registry {} (interface {})",
            name,
            ctx.last_sender_id,
            interface
        );
        return Err(Action::Drop);
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
        // The old generation of the name has retired.
        crate::handler::shm::retire_guest_shms(ctx, name);
        replace_global_metadata_for_generation(ctx, name);
        // The name may now be another interface's, so a singleton
        // whose binding it was needs another candidate.
        ensure_all_bound(ctx)?;
    }
    Ok(())
}

/// The action for a host global event the proxy does not forward: `Drop`,
/// unless binding a singleton for it ended the session.
fn dropped(bound: Result<(), Action>) -> Action {
    bound.err().unwrap_or(Action::Drop)
}

/// A host `zwp_linux_dmabuf_v1` global: a candidate for the proxy's own
/// binding and, with GPU access, the guest's dma-buf global.
fn dmabuf_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    let interface = zwp_linux_dmabuf_v1::NAME;
    let generation = ctx
        .global_generations
        .get(&name)
        .copied()
        .unwrap_or_default();
    let virtwl_supports_dmabuf = ctx
        .virtwayland_channel()
        .is_some_and(|channel| channel.supports_dmabuf());
    if !should_bind_internal_dmabuf(ctx.settings().gpu_accel, virtwl_supports_dmabuf) {
        record_registry_global_visibility(ctx, name, false);
        return Action::Drop;
    }
    // A v3 host still has enough legacy format/modifier information
    // for our synthetic v4 feedback object. Advertise v4 to the
    // guest while clamping only the internal host bind to v3.
    let can_synthesize_feedback = can_synthesize_dmabuf_feedback(ctx, version);
    let client_version = if can_synthesize_feedback {
        GUEST_DMABUF_VERSION
    } else {
        if version >= LINUX_DMABUF_CAPABILITY_VERSION && ctx.virtgpu().is_none() {
            log::warn!(
                "No virtio-gpu render node; exposing linux-dmabuf v3 without synthetic \
                 device feedback"
            );
        }
        version.min(LINUX_DMABUF_CAPABILITY_VERSION)
    };
    add_candidate(ctx, Singleton::Dmabuf, name, version);
    if ctx.settings().gpu_accel {
        // One host dmabuf object is shared for capability discovery,
        // and a guest global for it waits until the binding of its
        // own generation has the capabilities.
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
            || crate::handler::linux_dmabuf::has_synthetic_feedback_formats(ctx, generation)
        {
            advertise_synthetic_global(ctx, name, interface, client_version);
        } else {
            record_registry_global_visibility(ctx, name, false);
        }
    } else {
        // Virtwl uses this object only to learn the formats that can
        // be advertised by the synthetic wl_shm global. Keep the
        // linux-dmabuf global hidden from the guest, matching
        // ChromiumOS's separate --enable-linux-dmabuf switch.
        hide_global(ctx, name);
    }
    // The host's own advertisement is uncapped, so it is never forwarded.
    dropped(ensure_bound(ctx, Singleton::Dmabuf))
}

/// A host `zwp_text_input_manager_v1` global: the guest is offered a
/// synthetic `zwp_text_input_manager_v3` under its name, and the proxy
/// binds the first one for itself.
fn text_input_manager_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    advertise_synthetic_global(
        ctx,
        name,
        zwp_text_input_manager_v3::NAME,
        zwp_text_input_manager_v3::VERSION,
    );
    if ctx.host_text_input_manager_v1_id.is_some() {
        return Action::Drop;
    }
    let interface = Iface::ZwpTextInputManagerV1;
    let bound_version = version.min(TEXT_INPUT_MANAGER_VERSION);
    // This object is internal: there is no guest-side pair.
    let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
    ctx.host_text_input_manager_v1_id = Some(host_id);
    let registry_host_id = ctx.last_sender_id;
    dropped(bind_internally(
        ctx,
        registry_host_id,
        name,
        interface,
        bound_version,
        host_id,
    ))
}

/// A host `zcr_text_input_extension_v1` global, hidden from the guest; the
/// proxy binds the first one for itself.
fn text_input_extension_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    let interface = Iface::ZcrTextInputExtensionV1;
    hide_global(ctx, name);
    if ctx.host_text_input_extension_v1_id.is_some() {
        return Action::Drop;
    }
    let bound_version = version.min(TEXT_INPUT_EXTENSION_VERSION);
    let host_id = ctx.shadow_table.create_host_only(interface, bound_version);
    ctx.host_text_input_extension_v1_id = Some(host_id);
    let registry_host_id = ctx.last_sender_id;
    dropped(bind_internally(
        ctx,
        registry_host_id,
        name,
        interface,
        bound_version,
        host_id,
    ))
}

/// A host `zcr_keyboard_extension_v1` global, hidden from the guest and a
/// candidate for the proxy's own binding.
fn keyboard_extension_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    hide_global(ctx, name);
    add_candidate(ctx, Singleton::KeyboardExtension, name, version);
    dropped(ensure_bound(ctx, Singleton::KeyboardExtension))
}

/// A host `zaura_shell` global: the guest is offered `gtk_shell1` under
/// its name, and it is a candidate for the proxy's own binding. One older
/// than [`ZAURA_SHELL_MIN_VERSION`] is ignored.
fn aura_shell_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    if version < ZAURA_SHELL_MIN_VERSION {
        log::debug!(
            "Ignoring zaura_shell v{}; minimum supported version is {}",
            version,
            ZAURA_SHELL_MIN_VERSION
        );
        record_registry_global_visibility(ctx, name, false);
        return Action::Drop;
    }
    // The host-specific global is replaced at the guest boundary
    // with gtk_shell1, using the same numeric registry name so its
    // global_remove lifecycle remains paired.
    advertise_synthetic_global(ctx, name, gtk_shell1::NAME, gtk_shell1::VERSION);
    // The host compositor sends the same global list to every
    // wl_registry object, and the proxy has one internal aura shell
    // binding per connection.
    add_candidate(ctx, Singleton::AuraShell, name, version);
    dropped(ensure_bound(ctx, Singleton::AuraShell))
}

/// A host `wl_shm` global: the guest is offered the proxy's own `wl_shm`
/// under its name, and it is a candidate for the proxy's own binding.
fn shm_global(ctx: &mut Context, name: u32, version: u32) -> Action {
    // Advertise the capped version explicitly instead of forwarding
    // a host v2 global: otherwise clients bind v2 and are rejected
    // by `admit_guest_bind`.
    advertise_synthetic_global(ctx, name, wl_shm::NAME, WL_SHM_VERSION);
    add_candidate(ctx, Singleton::Shm, name, version);
    dropped(ensure_bound(ctx, Singleton::Shm))
}

/// Any other host global: forwarded at the version the proxy offers, if
/// it offers one (see [`forwarded_global_version`]), and hidden otherwise.
fn forwarded_global(ctx: &mut Context, name: u32, interface: &str, version: u32) -> Action {
    let Some(advertised_version) = forwarded_global_version(ctx, interface, version) else {
        if interface.contains(char::REPLACEMENT_CHARACTER) {
            log::debug!("Hiding global {name}, whose interface name {interface:?} is not UTF-8");
        }
        record_registry_global_visibility(ctx, name, false);
        return Action::Drop;
    };
    if advertised_version != version {
        advertise_synthetic_global(ctx, name, interface, advertised_version);
        return Action::Drop;
    }
    ctx.host_globals.insert(
        name,
        HostGlobal {
            interface: interface.to_string(),
            version: advertised_version,
        },
    );
    record_registry_global_visibility(ctx, name, true);
    Action::Forward
}

impl wl_registry::WlRegistryHandler for Handlers {
    /// A host global: the proxy binds it for itself, advertises a global
    /// of its own in its place, forwards it, or hides it, by its
    /// interface.
    fn on_global(ctx: &mut Context, name: u32, interface: &String, version: u32) -> Action {
        if let Err(action) = accept_global(ctx, name, interface, version) {
            return action;
        }
        match interface.as_str() {
            zwp_linux_dmabuf_v1::NAME => dmabuf_global(ctx, name, version),
            zwp_text_input_manager_v1::NAME => text_input_manager_global(ctx, name, version),
            zcr_text_input_extension_v1::NAME => text_input_extension_global(ctx, name, version),
            zcr_keyboard_extension_v1::NAME => keyboard_extension_global(ctx, name, version),
            zaura_shell::NAME => aura_shell_global(ctx, name, version),
            wl_shm::NAME => shm_global(ctx, name, version),
            _ => forwarded_global(ctx, name, interface, version),
        }
    }

    fn on_global_remove(ctx: &mut Context, name: u32) -> Action {
        let registry_id = ctx.last_sender_id;
        let Some(record) = ctx.registries.get(&registry_id) else {
            return Action::Drop;
        };
        let Some(removed_generation) = record.listed_generation(name) else {
            return Action::Drop;
        };
        let was_visible = record
            .visibility(name)
            .unwrap_or_else(|| ctx.host_globals.contains_key(&name));
        ctx.pending_dmabuf_globals.retain(|pending| {
            !(pending.registry_host_id == registry_id
                && pending.name == name
                && pending.generation == removed_generation)
        });
        // A bound wl_output remains a valid Wayland object after its global
        // advertisement disappears, but it must stop feeding
        // compositor-owned placement, and its host-only zaura_output is
        // released now (see `OutputRegistry::remove_global`). Match the
        // exact advertisement generation so a delayed remove cannot retire a
        // replacement.
        for aura_output in ctx.outputs.remove_global(GlobalBinding {
            name,
            generation: removed_generation,
        }) {
            crate::handler::aura_objects::release(
                ctx,
                crate::handler::aura_objects::AuraChild::OUTPUT,
                aura_output,
            );
        }
        crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, removed_generation);
        // Host registries are independent Wayland objects and their events can
        // be interleaved. A global name may be removed from one registry and
        // re-advertised there before another registry delivers the old remove.
        // Do not retire the connection-wide metadata until every registry
        // has observed the removal; otherwise that stale remove invalidates a
        // newly advertised generation.
        let current_generation = ctx.global_generations.get(&name).copied();
        let removes_current_generation = Some(removed_generation) == current_generation;
        // Keep the version of a removed output the guest saw, so a bind that
        // crossed this removal is told apart from a bind of a name it never
        // saw. `host_globals` describes only the current generation.
        let inert_bind_version = ctx
            .host_globals
            .get(&name)
            .filter(|global| {
                was_visible
                    && removes_current_generation
                    && global.interface == INERT_BIND_INTERFACE
            })
            .map(|global| global.version);
        if let Some(record) = ctx.registries.get_mut(&registry_id) {
            record.remove(name, inert_bind_version);
        }
        if removes_current_generation {
            ctx.removed_global_generations
                .insert(name, removed_generation);
            if !global_still_seen(ctx, name, current_generation) {
                retire_unseen_global(ctx, name);
            }
        }
        // Only globals previously exposed to the guest should generate a
        // matching removal event. Hidden/internal globals were never sent to
        // the guest and must not leak lifecycle notifications.
        if was_visible {
            Action::Forward
        } else {
            Action::Drop
        }
    }

    /// A guest bind, which only this handler knows how to serve: the
    /// generated code forwards nothing for it
    /// (`wayland_codegen::generator::HAND_WRITTEN_REQUESTS`).
    fn on_bind(ctx: &mut Context, name: u32, id: &(String, u32, u32)) {
        // id is (interface, version, new_id)
        let (requested, version, guest_new_id) = id;

        // The object table keeps the interface's generated name. One no
        // vendored protocol defines is never advertised, so it is no global
        // the guest may bind either.
        let admission = Iface::from_name(requested).and_then(|interface| {
            Some((
                interface,
                admit_guest_bind(ctx, name, interface.name(), *version)?,
            ))
        });
        let (interface, generation) = match admission {
            Some((interface, BindAdmission::Live { generation })) => (interface, generation),
            Some((interface, BindAdmission::Inert)) => {
                log::debug!(
                    "Bind of removed global {}:{} v{} crossed its removal; guest object {} is inert",
                    name,
                    interface,
                    version,
                    guest_new_id
                );
                // A local object: it never reaches the host, and its
                // destructor completes in the proxy.
                ctx.shadow_table
                    .create_local(*guest_new_id, interface, *version);
                return;
            }
            None => {
                error!(
                    "Rejecting bind for unadvertised, unsupported, or too-new global {}:{} v{}",
                    name, requested, version
                );
                let _ = ctx.reject(
                    ctx.last_sender_id,
                    wl_display::ERROR_INVALID_OBJECT,
                    format!(
                        "invalid bind for global {} (interface {}, version {})",
                        name, requested, version
                    ),
                );
                return;
            }
        };

        if interface == Iface::WlShm {
            // Do NOT forward wl_shm to host. We emulate it.
            ctx.shadow_table
                .create_local(*guest_new_id, interface, *version);

            // Advertise mandatory formats immediately and optional formats
            // only after the host wl_shm/dmabuf capability event has arrived.
            // This mirrors ChromiumOS Sommelier and prevents a guest from
            // selecting a format that the host cannot import.
            register_guest_shm(ctx, *guest_new_id, name);

            // The host-side wl_shm object was bound when the host global was
            // observed. Binding the synthetic guest global must not issue a
            // second host bind or create a guest→host alias.
            return;
        } else if interface == Iface::ZwpTextInputManagerV3 {
            // This is a synthetic guest-only manager. Every guest manager
            // shares the one internal host v1 manager, so mapping each guest
            // ID to that host ID would overwrite the previous manager's
            // reverse mapping and allow one manager's destroy to unregister
            // the shared host object.
            ctx.shadow_table
                .create_local(*guest_new_id, interface, *version);
            return;
        } else if interface == Iface::GtkShell1 {
            ctx.shadow_table
                .create_local(*guest_new_id, interface, *version);
            ctx.gtk_shells
                .insert(*guest_new_id, GtkShellState::default());
            queue_internal_sync(
                ctx,
                InternalCallback::GtkShellCapabilities {
                    gtk_shell: *guest_new_id,
                },
            );
            return;
        }

        // Every other global is paired with a host binding.
        //
        // The guest-facing dmabuf global is synthesized at v4, while the
        // host object used for params/create may only be v2/v3. Keep the
        // guest metadata at v4 so feedback requests are accepted locally,
        // but clamp the paired host object to v3 so registry.bind does not
        // request an unsupported host version.
        let host_version = if interface == Iface::ZwpLinuxDmabufV1 {
            (*version).min(LINUX_DMABUF_CAPABILITY_VERSION)
        } else if interface == Iface::WlCompositor {
            // Every guest v4 request missing from host v3 is handled locally.
            (*version).min(WL_COMPOSITOR_HOST_VERSION)
        } else {
            *version
        };

        // We need to send the bind request to the host.
        // The sender is the registry object.
        let registry_guest_id = ctx.last_sender_id;
        let Some(registry_host_id) = ctx.shadow_table.get_host_id(registry_guest_id) else {
            error!("Registry not mapped! Guest ID: {}", registry_guest_id);
            return;
        };
        let host_new_id =
            ctx.shadow_table
                .create_paired(*guest_new_id, interface, *version, host_version);
        let bind_version = host_version;
        if !queue_internal_bind(
            ctx,
            registry_host_id,
            name,
            interface,
            bind_version,
            host_new_id,
        ) {
            ctx.abandon(*guest_new_id);
            let _ = ctx.fail(format_args!(
                "Failed to bind host {interface} for the guest"
            ));
            return;
        }
        if interface == Iface::WlOutput {
            // Register the output only once its bind is queued, so the
            // failure paths above have no output state to undo.
            ctx.outputs.bind(
                host_new_id,
                bind_version,
                GlobalBinding { name, generation },
            );
            // Placement's host-only Aura output metadata (scale factors and
            // insets).
            crate::handler::window_placement::output_bound(ctx, host_new_id);
        }
        if interface == Iface::ZwpLinuxDmabufV1 {
            ctx.dmabuf_guest_generations
                .insert(*guest_new_id, generation);
        }
        if interface == Iface::WlSeat {
            // A text input whose seat had no host object can activate now.
            let seat = crate::ime::SeatId(*guest_new_id);
            crate::handler::text_input::ime(ctx, crate::ime::Input::SeatBound(seat));
        }
    }
}

/// Forget host registry `registry_host_id`, which ended (see
/// `crate::state::lifecycle`): what it was advertised, and its hold on
/// capability generations.
///
/// A global whose removal another registry has seen waits for every
/// registry that still lists it (see `on_global_remove`). If this was the
/// last one, it will never see the removal, so the global is retired now.
pub(crate) fn forget_registry(ctx: &mut Context, registry_host_id: u32) {
    let record = ctx.registries.remove(&registry_host_id).unwrap_or_default();
    ctx.pending_dmabuf_globals
        .retain(|pending| pending.registry_host_id != registry_host_id);
    for (name, generation) in record.listed() {
        if global_removal_seen(ctx, name, generation)
            && !global_still_seen(ctx, name, Some(generation))
        {
            retire_unseen_global(ctx, name);
        }
    }
    let generations: std::collections::BTreeSet<_> = record.generations().collect();
    for generation in generations {
        crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
    }
    // A candidate no registry lists any more can never be bound.
    let unlisted: Vec<u32> = ctx
        .singleton_candidates
        .keys()
        .copied()
        .filter(|&name| !global_still_seen(ctx, name, ctx.global_generations.get(&name).copied()))
        .collect();
    for name in unlisted {
        ctx.singleton_candidates.remove(&name);
    }
}

impl wl_fixes::WlFixesHandler for Handlers {
    fn on_destroy_registry(ctx: &mut Context, registry: u32) -> Action {
        // wl_fixes destroys the registry passed as an object argument rather
        // than its own request sender. Reserve that guest/host mapping until
        // wl_display.delete_id acknowledges the host-side destruction.
        ctx.guest_destroyed(registry);
        Action::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::{
        forwarded_global_version, keyboard_extension_version, should_bind_internal_dmabuf,
        Singleton, FORWARDED_PROTOCOLS, KEYBOARD_EXTENSION_VERSION,
        LINUX_DMABUF_CAPABILITY_VERSION, WL_SHM_VERSION, XDG_DECORATION_ALLOWED,
        ZAURA_SHELL_MIN_VERSION,
    };
    use crate::handler::Handlers;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell};
    use crate::protocols::gtk::gtk_shell1;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_fixes::WlFixesHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::{wl_display, wl_registry, wl_shm};
    use crate::protocols::AnyRequest;
    use crate::protocols::Iface;
    use crate::state::{Context, HostGlobal, HostId, InternalCallback};
    use crate::test_support::{decode, decoded, protocol_error};
    use crate::wire::{Action, WL_DISPLAY_ID};

    /// A guest `wl_registry` and the host registry the proxy paired it
    /// with. Their IDs differ, so looking up the wrong one fails.
    #[derive(Clone, Copy, Debug)]
    struct Registry {
        guest: u32,
        host: u32,
    }

    const A: Registry = Registry {
        guest: 10,
        host: 100,
    };

    const B: Registry = Registry {
        guest: 11,
        host: 101,
    };

    impl Registry {
        /// The host advertises global `name` on this registry.
        fn global(self, ctx: &mut Context, name: u32, interface: &str, version: u32) -> Action {
            ctx.last_sender_id = self.host;
            Handlers::on_global(ctx, name, &interface.to_string(), version)
        }

        /// The host removes global `name` from this registry.
        fn remove(self, ctx: &mut Context, name: u32) -> Action {
            ctx.last_sender_id = self.host;
            Handlers::on_global_remove(ctx, name)
        }

        /// The guest binds global `name` as `interface` at `version`, as
        /// its new object `id`.
        fn bind(self, ctx: &mut Context, name: u32, interface: &str, version: u32, id: u32) {
            ctx.last_sender_id = self.guest;
            Handlers::on_bind(ctx, name, &(interface.to_string(), version, id));
        }

        /// The guest destroys this registry through `wl_fixes`.
        fn destroy(self, ctx: &mut Context) -> Action {
            Handlers::on_destroy_registry(ctx, self.guest)
        }
    }

    /// `ctx` with the guest's `registries`.
    fn with_registries(mut ctx: Context, registries: &[Registry]) -> Context {
        for registry in registries {
            ctx.shadow_table
                .insert_paired_for_test(registry.guest, registry.host, "wl_registry");
        }
        ctx
    }

    /// A test context with the guest's `registries`, and no GPU access.
    fn context(registries: &[Registry]) -> Context {
        with_registries(Context::new_for_test(false, false, vec![]), registries)
    }

    /// [`context`] with GPU access, which the proxy needs to bind the
    /// host's linux-dmabuf.
    fn gpu_context(registries: &[Registry]) -> Context {
        with_registries(Context::new_for_test(true, false, vec![]), registries)
    }

    /// A context whose window placement uses Aura, for output and toplevel
    /// tests.
    fn aura_placement_context() -> Context {
        let placement = PlacementConfig::active_for_test(&[]);
        with_registries(Context::with_placement_for_test(placement), &[A])
    }

    /// Take the events queued for the guest, all of them `wl_registry`'s.
    fn guest_globals(ctx: &mut Context) -> Vec<(u32, wl_registry::Event)> {
        let queue = std::mem::take(&mut ctx.host_to_client_queue);
        decoded(&queue, wl_registry::Event::from_wire)
    }

    fn global_event(name: u32, interface: &str, version: u32) -> wl_registry::Event {
        wl_registry::Event::Global {
            name,
            interface: interface.to_string(),
            version,
        }
    }

    /// Take the requests queued for the host, decoded: those of the
    /// display, of registries A and B, and of the objects `others` names
    /// with their interfaces.
    fn host_requests(ctx: &mut Context, others: &[(u32, &str)]) -> Vec<(u32, AnyRequest)> {
        let queue = std::mem::take(&mut ctx.client_to_host_queue);
        queue
            .into_iter()
            .map(|(message, fds)| {
                decode(&message, &mut fds.into(), |msg| {
                    let interface = match msg.sender_id {
                        WL_DISPLAY_ID => "wl_display",
                        sender if sender == A.host || sender == B.host => "wl_registry",
                        sender => others
                            .iter()
                            .find(|(id, _)| *id == sender)
                            .map_or("an unknown interface", |(_, interface)| interface),
                    };
                    AnyRequest::from_wire(interface, msg)
                })
                .expect("a queued request decodes")
            })
            .collect()
    }

    fn bind_request(name: u32, interface: &str, version: u32, id: u32) -> AnyRequest {
        wl_registry::Request::Bind {
            name,
            id: (interface.to_string(), version, id),
        }
        .into()
    }

    /// The host ID of the proxy's binding of `singleton`.
    fn bound_host_id(singleton: Singleton, ctx: &Context) -> Option<u32> {
        match singleton {
            Singleton::Dmabuf => ctx.host_dmabuf_id,
            Singleton::KeyboardExtension => ctx.host_keyboard_extension_id.map(|id| id.0),
            Singleton::AuraShell => ctx.host_zaura_shell.map(|shell| shell.id),
            Singleton::Shm => ctx.host_shm_id,
        }
    }

    /// A format the proxy can offer in synthetic linux-dmabuf feedback:
    /// `XRGB8888` with the linear modifier.
    const XRGB8888_LINEAR: (u32, u64) = (0x3432_5258, 0);

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
    fn wl_fixes_destroy_registry_removes_deferred_dmabuf_advertisements() {
        let mut ctx = gpu_context(&[A]);
        let record = ctx.registries.entry(A.host).or_default();
        record.list(7, 9);
        record.set_visibility(7, false);
        ctx.dmabuf_capabilities.entry(9).or_default();
        ctx.pending_dmabuf_globals
            .push(crate::state::PendingDmabufGlobal {
                registry_host_id: A.host,
                name: 7,
                version: 4,
                generation: 9,
            });

        assert_eq!(A.destroy(&mut ctx), Action::Forward);
        assert!(!ctx.registries.contains_key(&A.host));
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert!(!ctx.dmabuf_capabilities.contains_key(&9));
        assert!(ctx.shadow_table.is_pending_destroy_guest(A.guest));
    }

    /// Each registry gets its own synthetic global, under the name its host
    /// registry gave it, and every such name is bindable.
    #[test]
    fn synthetic_globals_are_sent_to_each_guest_registry() {
        for (interface, version, advertised) in
            [("wl_shm", 2, "wl_shm"), ("zaura_shell", 38, "gtk_shell1")]
        {
            for name_on_b in [7, 8] {
                let mut ctx = context(&[A, B]);
                assert_eq!(A.global(&mut ctx, 7, interface, version), Action::Drop);
                assert_eq!(
                    B.global(&mut ctx, name_on_b, interface, version),
                    Action::Drop
                );
                assert_eq!(
                    guest_globals(&mut ctx),
                    [
                        (A.guest, global_event(7, advertised, 1)),
                        (B.guest, global_event(name_on_b, advertised, 1)),
                    ],
                    "{interface} as {name_on_b} on B"
                );
                assert_eq!(
                    ctx.host_globals.get(&name_on_b),
                    Some(&HostGlobal {
                        interface: advertised.to_string(),
                        version: 1,
                    }),
                    "{interface} as {name_on_b} on B"
                );

                B.bind(&mut ctx, name_on_b, advertised, 1, 20);
                assert!(!ctx.is_fatal(), "{interface} as {name_on_b} on B");
                assert_eq!(
                    ctx.shadow_table.get_interface(20).map(Iface::name),
                    Some(advertised),
                    "{interface} as {name_on_b} on B"
                );
            }
        }
    }

    /// The proxy serves the `gtk_shell1` it advertises for `zaura_shell`
    /// itself, and sends its capabilities once the host has answered a
    /// sync.
    #[test]
    fn a_gtk_shell_bind_is_served_locally() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 7, "zaura_shell", 38), Action::Drop);
        guest_globals(&mut ctx);
        ctx.client_to_host_queue.clear();

        A.bind(&mut ctx, 7, "gtk_shell1", 1, 50);
        assert!(ctx.shadow_table.is_local(50));
        assert!(ctx.gtk_shells.contains_key(&50));
        assert_eq!(guest_globals(&mut ctx), []);
        let requests = host_requests(&mut ctx, &[]);
        let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            requests[..]
        else {
            panic!("the host got {requests:?}, not one sync");
        };
        assert_eq!(
            ctx.internal_callbacks.get(HostId(callback)),
            Some(InternalCallback::GtkShellCapabilities { gtk_shell: 50 })
        );

        ctx.last_sender_id = callback;
        assert_eq!(Handlers::on_done(&mut ctx, 0), Action::Drop);
        assert_eq!(
            decoded(&ctx.host_to_client_queue, gtk_shell1::Event::from_wire),
            [(50, gtk_shell1::Event::Capabilities { capabilities: 0 })]
        );
    }

    /// The proxy binds each global it uses itself at the newest version
    /// both it and the host support, and keeps each binding's host ID from
    /// reuse.
    #[test]
    fn internal_bindings_are_capped_and_reserved_from_host_id_reuse() {
        let mut ctx = context(&[A]);
        // Offered at v99, each is bound at the newest version the proxy
        // supports: the vendored XML's, or wl_shm's v1.
        let bindings = [
            (
                1,
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
                2,
            ),
            (
                2,
                "zcr_text_input_extension_v1",
                zcr_text_input_extension_v1::VERSION,
                3,
            ),
            (
                3,
                "zcr_keyboard_extension_v1",
                zcr_keyboard_extension_v1::VERSION,
                4,
            ),
            (4, "wl_shm", 1, 5),
        ];
        for (name, interface, _, _) in bindings {
            assert_eq!(A.global(&mut ctx, name, interface, 99), Action::Drop);
        }

        assert_eq!(
            host_requests(&mut ctx, &[]),
            bindings.map(|(name, interface, version, id)| {
                (A.host, bind_request(name, interface, version, id))
            })
        );
        assert_eq!(ctx.host_text_input_manager_v1_id, Some(2));
        assert_eq!(ctx.host_text_input_extension_v1_id, Some(3));
        assert_eq!(ctx.host_keyboard_extension_id, Some(HostId(4)));
        assert_eq!(ctx.host_shm_id, Some(5));
        for (_, interface, version, id) in bindings {
            assert_eq!(
                ctx.shadow_table.get_host_interface(id).map(Iface::name),
                Some(interface)
            );
            assert_eq!(ctx.shadow_table.host_object_version(id), Some(version));
        }
        assert_eq!(ctx.shadow_table.create_host_only(Iface::WlCallback, 1), 6);

        // Offered at v1, each is bound at v1.
        for (_, interface, _, _) in bindings {
            let mut ctx = context(&[A]);
            assert_eq!(A.global(&mut ctx, 1, interface, 1), Action::Drop);
            assert_eq!(
                host_requests(&mut ctx, &[]),
                [(A.host, bind_request(1, interface, 1, 2))],
                "{interface}"
            );
            assert_eq!(ctx.shadow_table.host_object_version(2), Some(1));
        }
    }

    /// Without synthetic feedback the guest gets the host's linux-dmabuf at
    /// once, capped at v3, the version the proxy binds to collect legacy
    /// format modifiers.
    #[test]
    fn a_dmabuf_without_synthetic_feedback_is_offered_and_bound_at_v3() {
        let dmabuf = "zwp_linux_dmabuf_v1";
        for version in [4, 5, 99] {
            let mut ctx = gpu_context(&[A]);
            assert_eq!(A.global(&mut ctx, 10, dmabuf, version), Action::Drop);

            let host_id = ctx.host_dmabuf_id.expect("the internal binding");
            assert_eq!(ctx.shadow_table.host_object_version(host_id), Some(3));
            assert_eq!(
                host_requests(&mut ctx, &[]),
                [(A.host, bind_request(10, dmabuf, 3, host_id))],
                "v{version}"
            );
            assert_eq!(
                guest_globals(&mut ctx),
                [(A.guest, global_event(10, dmabuf, 3))]
            );
            assert_eq!(
                ctx.host_globals.get(&10),
                Some(&HostGlobal {
                    interface: dmabuf.to_string(),
                    version: 3,
                })
            );
            assert!(ctx.pending_dmabuf_globals.is_empty());
            assert_eq!(ctx.internal_callbacks.count(), 0);
        }
    }

    /// With synthetic feedback the guest's linux-dmabuf global waits for
    /// the capabilities of its binding, and stays hidden if the host offers
    /// no format the proxy supports.
    #[test]
    fn a_synthetic_dmabuf_global_waits_for_its_capabilities() {
        let dmabuf = "zwp_linux_dmabuf_v1";
        for (formats, offered) in [(&[XRGB8888_LINEAR][..], Some(4)), (&[], None)] {
            let mut ctx = gpu_context(&[A]);
            ctx.synthetic_feedback_available_for_test = true;
            assert_eq!(A.global(&mut ctx, 10, dmabuf, 4), Action::Drop);
            let generation = ctx.global_generations[&10];
            assert!(!ctx.host_globals.contains_key(&10));
            assert_eq!(guest_globals(&mut ctx), []);
            assert_eq!(ctx.pending_dmabuf_globals.len(), 1);

            ctx.dmabuf_capabilities
                .get_mut(&generation)
                .expect("capability generation")
                .format_modifiers
                .extend_from_slice(formats);
            crate::handler::linux_dmabuf::complete_capability_discovery(&mut ctx, generation);

            assert!(ctx.pending_dmabuf_globals.is_empty());
            assert_eq!(
                ctx.host_globals.get(&10),
                offered
                    .map(|version| HostGlobal {
                        interface: dmabuf.to_string(),
                        version,
                    })
                    .as_ref()
            );
            assert_eq!(
                guest_globals(&mut ctx),
                Vec::from_iter(offered.map(|version| (A.guest, global_event(10, dmabuf, version))))
            );
            assert_eq!(
                ctx.registries[&A.host].visibility(10),
                Some(offered.is_some())
            );
        }
    }

    /// A global of version 0 is malformed; the proxy drops it.
    #[test]
    fn a_zero_version_global_is_dropped() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 10, "wl_seat", 0), Action::Drop);
        assert!(!ctx.host_globals.contains_key(&10));
        assert_eq!(guest_globals(&mut ctx), []);
    }

    /// A second global under a name a registry lists is a duplicate; the
    /// proxy drops it and keeps the first.
    #[test]
    fn a_duplicate_global_name_is_dropped() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 11, "wl_seat", 5), Action::Forward);
        assert_eq!(A.global(&mut ctx, 11, "wl_output", 3), Action::Drop);
        assert_eq!(
            ctx.host_globals.get(&11),
            Some(&HostGlobal {
                interface: "wl_seat".to_string(),
                version: 5,
            })
        );
        assert_eq!(guest_globals(&mut ctx), []);
    }

    /// A removed output is forgotten and its `zaura_output` released at
    /// once: a `wl_output` before v3 has no release request to free them
    /// later. The guest's object itself stays valid until it releases it.
    #[test]
    fn global_remove_forgets_the_output_and_releases_its_aura_child() {
        let mut ctx = aura_placement_context();
        assert_eq!(A.global(&mut ctx, 11, "zaura_shell", 38), Action::Drop);
        assert_eq!(A.global(&mut ctx, 10, "wl_output", 2), Action::Forward);
        A.bind(&mut ctx, 10, "wl_output", 2, 20);
        let output = ctx.shadow_table.get_host_id(20).expect("bound output");
        let aura_output = ctx.outputs.aura_output(output).expect("Aura output");
        ctx.client_to_host_queue.clear();

        assert_eq!(A.remove(&mut ctx, 10), Action::Forward);
        assert_eq!(
            host_requests(&mut ctx, &[(aura_output, "zaura_output")]),
            [(aura_output, zaura_output::Request::Release {}.into())]
        );
        assert!(ctx.shadow_table.is_retired_host(aura_output));
        assert!(ctx.outputs.is_empty_for_test());
        assert_eq!(ctx.shadow_table.get_host_id(20), Some(output));
    }

    #[test]
    fn output_bind_through_an_unmapped_registry_registers_no_output() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 10, "wl_output", 3), Action::Forward);

        // The guest's registry 12 has no host object, so the bind cannot be
        // sent.
        let unmapped = Registry {
            guest: 12,
            host: 102,
        };
        unmapped.bind(&mut ctx, 10, "wl_output", 3, 20);
        assert!(!ctx.is_fatal());
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert_eq!(host_requests(&mut ctx, &[]), []);
        assert!(ctx.outputs.is_empty_for_test());
    }

    /// A bind the proxy cannot serve is a protocol error on the guest's
    /// registry, and creates no object.
    #[test]
    fn an_invalid_bind_is_a_protocol_error() {
        struct Case {
            name: &'static str,
            setup: fn(&mut Context),
            /// The registry the guest binds through.
            through: Registry,
            bind: (u32, &'static str, u32),
        }
        for case in [
            Case {
                name: "a global of another interface",
                setup: |ctx| assert_eq!(A.global(ctx, 7, "wl_compositor", 1), Action::Forward),
                through: A,
                bind: (7, "wl_seat", 1),
            },
            Case {
                name: "a name never advertised",
                setup: |_| {},
                through: A,
                bind: (42, "wl_seat", 5),
            },
            // Defence in depth: `host_globals` only ever holds forwarded
            // interfaces, so this bind fails at the name lookup as well and
            // the forwarding guard cannot be isolated by a test.
            Case {
                name: "an interface the proxy does not forward",
                setup: |_| {},
                through: A,
                bind: (42, "zcr_text_input_extension_v1", 1),
            },
            Case {
                name: "a global hidden without GPU access",
                setup: |ctx| assert_eq!(A.global(ctx, 42, "zwp_linux_dmabuf_v1", 5), Action::Drop),
                through: A,
                bind: (42, "zwp_linux_dmabuf_v1", 5),
            },
            Case {
                name: "a version above the advertised one",
                setup: |ctx| assert_eq!(A.global(ctx, 10, "wl_seat", 3), Action::Forward),
                through: A,
                bind: (10, "wl_seat", 4),
            },
            Case {
                // Only an output can be bound inert after its removal (see
                // `bind_crossing_an_output_removal_creates_an_inert_output`).
                // The removal of a name the registry never listed is
                // dropped.
                name: "a removed seat",
                setup: |ctx| {
                    assert_eq!(A.global(ctx, 10, "wl_seat", 5), Action::Forward);
                    assert_eq!(A.remove(ctx, 99), Action::Drop);
                    assert_eq!(A.remove(ctx, 10), Action::Forward);
                },
                through: A,
                bind: (10, "wl_seat", 5),
            },
            Case {
                // A bind that crossed an output's removal is inert only as
                // the output it was.
                name: "a removed output bound as another interface",
                setup: |ctx| {
                    assert_eq!(A.global(ctx, 10, "wl_output", 3), Action::Forward);
                    assert_eq!(A.remove(ctx, 10), Action::Forward);
                },
                through: A,
                bind: (10, "wl_seat", 3),
            },
            Case {
                name: "a live global the binding registry never listed",
                setup: |ctx| {
                    assert_eq!(A.global(ctx, 10, "wl_output", 3), Action::Forward);
                    assert_eq!(B.global(ctx, 11, "wl_seat", 5), Action::Forward);
                },
                through: B,
                bind: (10, "wl_output", 3),
            },
            Case {
                // Registry B listed nothing, so only the retirement of the
                // global its removal from A completed can refuse the bind.
                name: "a retired global bound through a registry that listed nothing",
                setup: |ctx| {
                    assert_eq!(A.global(ctx, 10, "wl_seat", 5), Action::Forward);
                    assert_eq!(A.remove(ctx, 10), Action::Forward);
                },
                through: B,
                bind: (10, "wl_seat", 5),
            },
            Case {
                // Reachable only through a proxy bookkeeping bug, not a
                // guest error: a name in `host_globals` whose advertisement
                // generation was never recorded.
                name: "a global without an advertisement generation",
                setup: |ctx| {
                    let output = HostGlobal {
                        interface: "wl_output".to_string(),
                        version: 3,
                    };
                    ctx.host_globals.insert(10, output);
                },
                through: A,
                bind: (10, "wl_output", 3),
            },
        ] {
            let mut ctx = context(&[A, B]);
            (case.setup)(&mut ctx);
            ctx.client_to_host_queue.clear();

            let (name, interface, version) = case.bind;
            case.through.bind(&mut ctx, name, interface, version, 20);
            assert!(ctx.is_fatal(), "{} must end the session", case.name);
            assert_eq!(
                protocol_error(&ctx),
                (case.through.guest, wl_display::ERROR_INVALID_OBJECT),
                "{}",
                case.name
            );
            assert_eq!(ctx.shadow_table.get_interface(20), None, "{}", case.name);
            assert_eq!(ctx.shadow_table.get_host_id(20), None, "{}", case.name);
            assert_eq!(host_requests(&mut ctx, &[]), [], "{}", case.name);
            assert!(ctx.outputs.is_empty_for_test(), "{}", case.name);
        }
    }

    #[test]
    fn late_zaura_shell_binding_attaches_existing_outputs() {
        let mut ctx = aura_placement_context();
        assert_eq!(A.global(&mut ctx, 10, "wl_output", 3), Action::Forward);
        A.bind(&mut ctx, 10, "wl_output", 3, 20);
        let output = ctx.shadow_table.get_host_id(20).expect("bound output");
        assert_eq!(
            ctx.outputs.aura_output(output),
            None,
            "Aura output must wait for the shell binding"
        );

        assert_eq!(A.global(&mut ctx, 11, "zaura_shell", 38), Action::Drop);
        assert!(
            ctx.outputs.aura_output(output).is_some(),
            "late shell binding must attach every already-bound output"
        );
    }

    /// A toplevel created before zaura_shell is bound gets its Aura
    /// toplevel when the shell is bound.
    #[test]
    fn late_zaura_shell_binding_attaches_existing_toplevels() {
        let placement = PlacementConfig::active_for_test(&[]);
        let mut ctx = with_registries(Context::with_placement_for_test(placement), &[A]);
        ctx.shadow_table
            .insert_paired_for_test(12, 22, "wl_surface");
        ctx.shadow_table
            .insert_paired_for_test(13, 23, "xdg_toplevel");
        ctx.xdg_roles.add_toplevel_role_for_test(13 + 1000, 12, 13);

        assert_eq!(A.global(&mut ctx, 11, "zaura_shell", 38), Action::Drop);
        assert!(
            ctx.placement()
                .and_then(|state| state.aura_toplevel(crate::state::GuestId(13)))
                .is_some(),
            "late shell binding must attach every recorded toplevel"
        );
    }

    /// The guest's `wl_shm` is emulated over the proxy's one host binding.
    #[test]
    fn synthetic_shm_bind_does_not_create_a_second_host_object() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 7, "wl_shm", 1), Action::Drop);
        guest_globals(&mut ctx);
        ctx.client_to_host_queue.clear();

        A.bind(&mut ctx, 7, "wl_shm", 1, 20);
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert_eq!(ctx.shadow_table.get_interface(20), Some(Iface::WlShm));
        assert_eq!(host_requests(&mut ctx, &[]), [], "no second host bind");
        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_shm::Event::from_wire),
            [
                (20, wl_shm::Event::Format { format: 0 }),
                (20, wl_shm::Event::Format { format: 1 }),
            ],
            "only the mandatory formats before the host's capabilities"
        );
    }

    /// Every guest `zwp_text_input_manager_v3` is local: mapping each to
    /// the one host manager would let one's destruction unmap the others'.
    #[test]
    fn synthetic_text_input_managers_do_not_alias_shared_host_id() {
        let mut ctx = context(&[A]);
        assert_eq!(
            A.global(&mut ctx, 9, "zwp_text_input_manager_v1", 1),
            Action::Drop
        );
        let manager = ctx
            .host_text_input_manager_v1_id
            .expect("host manager binding");
        for id in [20, 21] {
            A.bind(&mut ctx, 9, "zwp_text_input_manager_v3", 1, id);
            assert_eq!(ctx.shadow_table.get_host_id(id), None);
        }

        ctx.last_sender_id = 20;
        assert_eq!(
            <Handlers as ZwpTextInputManagerV3Handler>::on_destroy(&mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(manager),
            Some(Iface::ZwpTextInputManagerV1)
        );
        assert!(ctx.shadow_table.get_interface(21).is_some());
    }

    #[test]
    fn duplicate_interfaces_keep_each_global_name_bindable() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 10, "wl_seat", 5), Action::Forward);
        assert_eq!(A.global(&mut ctx, 11, "wl_seat", 5), Action::Forward);

        A.bind(&mut ctx, 10, "wl_seat", 5, 20);
        A.bind(&mut ctx, 11, "wl_seat", 5, 21);
        let host = |id| ctx.shadow_table.get_host_id(id).expect("a host seat");
        let expected = [
            (A.host, bind_request(10, "wl_seat", 5, host(20))),
            (A.host, bind_request(11, "wl_seat", 5, host(21))),
        ];
        assert_eq!(host_requests(&mut ctx, &[]), expected);
    }

    #[test]
    fn removed_text_input_globals_keep_bound_manager_usable() {
        let mut ctx = context(&[A]);
        assert_eq!(
            A.global(&mut ctx, 9, "zwp_text_input_manager_v1", 1),
            Action::Drop
        );
        assert_eq!(
            A.global(&mut ctx, 10, "zcr_text_input_extension_v1", 11),
            Action::Drop
        );
        let manager = ctx
            .host_text_input_manager_v1_id
            .expect("host manager binding");
        let extension = ctx
            .host_text_input_extension_v1_id
            .expect("host extension binding");

        // The synthetic v3 global is advertised under the host manager's
        // numeric name. Bind a guest manager before removing the host globals.
        A.bind(&mut ctx, 9, "zwp_text_input_manager_v3", 1, 20);
        assert_eq!(A.remove(&mut ctx, 9), Action::Forward);
        assert_eq!(A.remove(&mut ctx, 10), Action::Drop);
        assert_eq!(ctx.host_text_input_manager_v1_id, Some(manager));
        assert_eq!(ctx.host_text_input_extension_v1_id, Some(extension));
        ctx.client_to_host_queue.clear();

        // An already-bound manager remains usable after global_remove. New
        // text-input children must still be created through both retained
        // host-side factories.
        ctx.last_sender_id = 20;
        assert_eq!(
            <Handlers as ZwpTextInputManagerV3Handler>::on_get_text_input(&mut ctx, 21, 30),
            Action::Drop
        );
        let host = host_requests(
            &mut ctx,
            &[
                (manager, "zwp_text_input_manager_v1"),
                (extension, "zcr_text_input_extension_v1"),
            ],
        );
        // The host IDs come from the requests that create them, and the
        // extension extends the text input the manager creates.
        let created = |request: &(u32, AnyRequest)| request.1.new_object().map(|(id, _)| id);
        let (Some(host_text_input), Some(host_extended)) = (created(&host[0]), created(&host[1]))
        else {
            panic!("the host got no extended text input: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    manager,
                    zwp_text_input_manager_v1::Request::CreateTextInput {
                        id: host_text_input
                    }
                    .into()
                ),
                (
                    extension,
                    zcr_text_input_extension_v1::Request::GetExtendedTextInput {
                        id: host_extended,
                        text_input: host_text_input,
                    }
                    .into()
                ),
            ]
        );
    }

    #[test]
    fn keyboard_extension_global_remove_keeps_existing_children_alive() {
        let mut ctx = context(&[A]);
        assert_eq!(
            A.global(&mut ctx, 20, "zcr_keyboard_extension_v1", 2),
            Action::Drop
        );
        let manager_id = ctx
            .host_keyboard_extension_id
            .expect("keyboard extension binding");

        // Simulate the child created by get_extended_keyboard for an already
        // bound host wl_keyboard. The child owns its own destructor and must
        // outlive the manager global.
        crate::handler::keyboard::for_test::created(&mut ctx, 41, 40, 42);
        crate::handler::keyboard::ensure_extended_keyboard_bound(&mut ctx, HostId(40));
        let child_id = ctx
            .keys
            .keyboards()
            .extended_for_test(HostId(40))
            .expect("extended keyboard child");
        assert_eq!(
            ctx.shadow_table.get_host_interface(child_id.0),
            Some(Iface::ZcrExtendedKeyboardV1)
        );

        assert_eq!(A.remove(&mut ctx, 20), Action::Drop);
        assert_eq!(ctx.host_keyboard_extension_id, None);
        assert_eq!(
            ctx.shadow_table.get_host_interface(manager_id.0),
            None,
            "global removal retires only the manager dispatch entry"
        );
        assert_eq!(
            ctx.keys.keyboards().extended_for_test(HostId(40)),
            Some(child_id),
            "an existing child must remain mapped after manager removal"
        );
        assert_eq!(
            ctx.keys.keyboards().of_extended(child_id),
            Some(HostId(40)),
            "the reverse child mapping must remain routable"
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(child_id.0),
            Some(Iface::ZcrExtendedKeyboardV1),
            "child dispatch metadata must survive global removal"
        );
    }

    /// Removing the global a singleton is bound to retires the binding: its
    /// host ID stays reserved, the host object is destroyed if its
    /// interface has a destructor, and the name's next advertisement is
    /// bound afresh.
    #[test]
    fn removing_a_bound_singleton_global_retires_its_binding() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises, and the one the proxy binds.
            version: u32,
            bound_version: u32,
            /// What the proxy does with the host's `global_remove`.
            removal: Action,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
            /// Whether a new binding asks for its capabilities with a sync.
            capability_sync: bool,
            /// Whether a guest global waits for the binding's capabilities
            /// when the host removes it.
            pending_guest_global: bool,
        }
        for Case {
            singleton,
            version,
            bound_version,
            removal,
            destructor,
            capability_sync,
            pending_guest_global,
        } in [
            Case {
                singleton: Singleton::Shm,
                version: 1,
                bound_version: 1,
                removal: Action::Forward,
                destructor: None,
                capability_sync: false,
                pending_guest_global: false,
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: 2,
                bound_version: 2,
                removal: Action::Drop,
                destructor: None,
                capability_sync: false,
                pending_guest_global: false,
            },
            Case {
                singleton: Singleton::AuraShell,
                version: 38,
                bound_version: 38,
                removal: Action::Forward,
                destructor: Some(zaura_shell::Request::Release {}.into()),
                capability_sync: false,
                pending_guest_global: false,
            },
            // Removed before its capabilities arrived, the global was never
            // shown to the guest.
            Case {
                singleton: Singleton::Dmabuf,
                version: 4,
                bound_version: 3,
                removal: Action::Drop,
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
                capability_sync: true,
                pending_guest_global: true,
            },
        ] {
            let interface = singleton.interface().name();
            let mut ctx = gpu_context(&[A]);
            ctx.synthetic_feedback_available_for_test = true;
            assert_eq!(A.global(&mut ctx, 10, interface, version), Action::Drop);
            let old = bound_host_id(singleton, &ctx).expect("a binding");
            ctx.client_to_host_queue.clear();
            assert_eq!(
                ctx.pending_dmabuf_globals.len(),
                usize::from(pending_guest_global),
                "{interface}"
            );

            assert_eq!(A.remove(&mut ctx, 10), removal, "{interface}");
            assert_eq!(singleton.bound_name(&ctx), None);
            assert_eq!(bound_host_id(singleton, &ctx), None);
            assert_eq!(ctx.shadow_table.get_host_interface(old), None);
            assert_eq!(
                host_requests(&mut ctx, &[(old, interface)]),
                Vec::from_iter(destructor.map(|request| (old, request))),
                "{interface}"
            );
            assert!(ctx.pending_dmabuf_globals.is_empty(), "{interface}");
            assert_ne!(ctx.shadow_table.create_host_only(Iface::WlCallback, 1), old);

            assert_eq!(A.global(&mut ctx, 10, interface, version), Action::Drop);
            let new = bound_host_id(singleton, &ctx).expect("a new binding");
            assert_ne!(new, old, "{interface}");
            let mut requests = host_requests(&mut ctx, &[]);
            if capability_sync {
                assert!(matches!(
                    requests.pop(),
                    Some((
                        WL_DISPLAY_ID,
                        AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
                    ))
                ));
            }
            assert_eq!(
                requests,
                [(A.host, bind_request(10, interface, bound_version, new))],
                "{interface}"
            );
        }
    }

    /// Removing the `zaura_shell` global releases the proxy's binding if
    /// its version has a destructor, and keeps the `zaura_surface`s made
    /// from it until their surfaces are destroyed.
    #[test]
    fn global_remove_keeps_aura_children_until_surface_destroy() {
        for (version, released) in [(38, true), (37, false)] {
            let mut ctx = context(&[A]);
            assert_eq!(A.global(&mut ctx, 10, "zaura_shell", version), Action::Drop);
            let shell = ctx.host_zaura_shell.expect("aura shell binding").id;
            let surface = ctx.shadow_table.create_host_only(Iface::ZauraSurface, 38);
            ctx.aura_surfaces.remember(50, surface);
            ctx.client_to_host_queue.clear();

            assert_eq!(A.remove(&mut ctx, 10), Action::Forward);
            assert_eq!(ctx.host_zaura_shell, None);
            assert_eq!(ctx.shadow_table.get_host_interface(shell), None);
            assert_eq!(
                host_requests(&mut ctx, &[(shell, "zaura_shell")]),
                Vec::from_iter(released.then(|| (shell, zaura_shell::Request::Release {}.into()))),
                "v{version}"
            );
            assert_eq!(ctx.aura_surfaces.get(50), Some(surface));
            assert_eq!(
                ctx.shadow_table.get_host_interface(surface),
                Some(Iface::ZauraSurface)
            );
            for reserved in [shell, surface] {
                assert_ne!(
                    ctx.shadow_table.create_host_only(Iface::WlCallback, 1),
                    reserved
                );
            }
        }
    }

    /// A host may advertise a singleton global under a new name before it
    /// removes the old one. The proxy keeps its binding of the old name,
    /// and binds the new one when the old one retires.
    #[test]
    fn a_singleton_advertised_anew_is_bound_when_the_old_name_retires() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises both globals at, and the
            /// one the proxy binds.
            version: u32,
            bound_version: u32,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
        }
        for Case {
            singleton,
            version,
            bound_version,
            destructor,
        } in [
            Case {
                singleton: Singleton::Dmabuf,
                version: LINUX_DMABUF_CAPABILITY_VERSION,
                bound_version: 3,
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: KEYBOARD_EXTENSION_VERSION,
                bound_version: 2,
                destructor: None,
            },
            // Before v38 `zaura_shell` has no destructor.
            Case {
                singleton: Singleton::AuraShell,
                version: ZAURA_SHELL_MIN_VERSION,
                bound_version: 6,
                destructor: None,
            },
            Case {
                singleton: Singleton::Shm,
                version: WL_SHM_VERSION,
                bound_version: 1,
                destructor: None,
            },
        ] {
            let mut ctx = gpu_context(&[A]);
            let interface = singleton.interface().name();
            assert_eq!(A.global(&mut ctx, 10, interface, version), Action::Drop);
            let old = bound_host_id(singleton, &ctx).expect("a binding");
            assert_eq!(A.global(&mut ctx, 11, interface, version), Action::Drop);
            assert_eq!(singleton.bound_name(&ctx), Some(10), "{interface}");
            assert_eq!(bound_host_id(singleton, &ctx), Some(old));
            assert_eq!(
                host_requests(&mut ctx, &[]),
                [(A.host, bind_request(10, interface, bound_version, old))],
                "{interface} is bound once"
            );

            A.remove(&mut ctx, 10);

            assert!(!ctx.is_fatal());
            assert_eq!(singleton.bound_name(&ctx), Some(11), "{interface}");
            let new = bound_host_id(singleton, &ctx).expect("a new binding");
            let expected = destructor.map(|request| (old, request)).into_iter();
            assert_eq!(
                host_requests(&mut ctx, &[(old, interface)]),
                Vec::from_iter(
                    expected.chain([(A.host, bind_request(11, interface, bound_version, new))])
                ),
                "{interface}"
            );
        }
    }

    /// A step of [`a_singleton_binding_moves_to_the_lowest_live_candidate`].
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// The registry sees the global's removal.
        Remove(Registry, u32),
        /// The registry sees the name advertised anew, for `wl_output`.
        ReuseAsOutput(Registry, u32),
        /// The registry sees the name advertised anew, for `wl_shm` again.
        ReadvertiseShm(Registry, u32),
        /// The guest destroys the registry through `wl_fixes`.
        Destroy(Registry),
    }

    /// A `wl_shm` bind the proxy sent `through` a registry, of global
    /// `name` as host object `id`.
    #[derive(Clone, Copy, Debug)]
    struct Bind {
        through: Registry,
        name: u32,
        id: u32,
    }

    /// When the proxy's `wl_shm` binding retires, it binds the lowest name
    /// (possibly the same name at a new generation) that a registry still
    /// lists and whose removal no registry has seen, through the lowest
    /// registry that lists it. A registry lists a name only at the
    /// generation it last saw advertised.
    #[test]
    fn a_singleton_binding_moves_to_the_lowest_live_candidate() {
        use Step::{Destroy, ReadvertiseShm, Remove, ReuseAsOutput};
        struct Case {
            name: &'static str,
            /// The `wl_shm` names each registry lists, in the order the
            /// host advertised them.
            listings: &'static [(Registry, &'static [u32])],
            steps: &'static [Step],
            bound: Option<u32>,
            /// The binds the steps sent.
            binds: &'static [Bind],
            candidates: &'static [u32],
        }
        for case in [
            Case {
                name: "the lowest candidate is bound through the lowest registry",
                listings: &[(B, &[10, 12, 11]), (A, &[10, 12, 11])],
                steps: &[Remove(B, 10), Remove(A, 10)],
                bound: Some(11),
                binds: &[Bind {
                    through: A,
                    name: 11,
                    id: 3,
                }],
                candidates: &[11, 12],
            },
            Case {
                // Before every registry has seen the bound name removed, one
                // that has may see it advertised for another interface.
                name: "a bound name reused by another interface hands the binding on",
                listings: &[(A, &[10, 11]), (B, &[10])],
                steps: &[Remove(A, 10), ReuseAsOutput(A, 10)],
                bound: Some(11),
                binds: &[Bind {
                    through: A,
                    name: 11,
                    id: 3,
                }],
                candidates: &[11],
            },
            Case {
                name: "a candidate name reused by another interface is no candidate",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[
                    Remove(A, 11),
                    ReuseAsOutput(A, 11),
                    Remove(A, 10),
                    Remove(B, 10),
                ],
                bound: None,
                binds: &[],
                candidates: &[],
            },
            Case {
                // A compositor may destroy a global as soon as it removes
                // it, so a bind of a name one registry saw removed could
                // end the connection.
                name: "a replacement another registry saw removed is not bound",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[Remove(A, 10), Remove(A, 11), Remove(B, 10)],
                bound: None,
                binds: &[],
                candidates: &[11],
            },
            Case {
                name: "a replacement every registry saw removed is not bound",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[Remove(A, 10), Remove(A, 11), Remove(B, 10), Remove(B, 11)],
                bound: None,
                binds: &[],
                candidates: &[],
            },
            Case {
                name: "a replacement removed first is not bound",
                listings: &[(A, &[10, 11])],
                steps: &[Remove(A, 11), Remove(A, 10)],
                bound: None,
                binds: &[],
                candidates: &[],
            },
            Case {
                // Registry A lists only the old generation of name 10, so
                // it cannot carry the bind of the new one.
                name: "a name advertised anew is bound through a registry that lists it anew",
                listings: &[(A, &[10]), (B, &[10])],
                steps: &[Remove(B, 10), ReadvertiseShm(B, 10)],
                bound: Some(10),
                binds: &[Bind {
                    through: B,
                    name: 10,
                    id: 3,
                }],
                candidates: &[10],
            },
            Case {
                // Registry A still lists name 10, but only its old
                // generation, so the removal of the new one retires it. A
                // registry that lists the current generation keeps it, as
                // in "a binding another registry lists stays" and the two
                // rows below.
                name: "a name advertised anew retires when only its old generation is listed",
                listings: &[(A, &[10]), (B, &[10])],
                steps: &[Remove(B, 10), ReadvertiseShm(B, 10), Remove(B, 10)],
                bound: None,
                binds: &[Bind {
                    through: B,
                    name: 10,
                    id: 3,
                }],
                candidates: &[],
            },
            Case {
                // A registry that still lists the name and sees it again
                // after another registry advanced its generation adopts
                // that generation, so it keeps the name alive.
                name: "a registry listing a name anew adopts its new generation",
                listings: &[(A, &[10]), (B, &[10])],
                steps: &[
                    Remove(B, 10),
                    ReadvertiseShm(B, 10),
                    ReadvertiseShm(A, 10),
                    Remove(B, 10),
                ],
                bound: Some(10),
                binds: &[Bind {
                    through: B,
                    name: 10,
                    id: 3,
                }],
                candidates: &[10],
            },
            Case {
                // Registry B saw the old generation removed, but A already
                // advanced it; B's replacement joins A's generation rather
                // than advancing it again, so A keeps the name alive.
                name: "a lagging registry joins the generation another advanced",
                listings: &[(A, &[10]), (B, &[10])],
                steps: &[
                    Remove(A, 10),
                    ReadvertiseShm(A, 10),
                    Remove(B, 10),
                    ReadvertiseShm(B, 10),
                    Remove(B, 10),
                ],
                bound: Some(10),
                binds: &[Bind {
                    through: A,
                    name: 10,
                    id: 3,
                }],
                candidates: &[10],
            },
            Case {
                name: "a binding another registry lists stays",
                listings: &[(A, &[10, 11]), (B, &[10, 12])],
                steps: &[Remove(A, 10)],
                bound: Some(10),
                binds: &[],
                candidates: &[10, 11, 12],
            },
            Case {
                // Destroying the last registry to list the removed binding
                // retires it; a candidate only that registry listed is
                // forgotten.
                name: "destroying a registry can promote a replacement",
                listings: &[(A, &[10, 11]), (B, &[10, 12])],
                steps: &[Remove(A, 10), Destroy(B)],
                bound: Some(11),
                binds: &[Bind {
                    through: A,
                    name: 11,
                    id: 3,
                }],
                candidates: &[11],
            },
        ] {
            let mut ctx = context(&[A, B]);
            for &(registry, names) in case.listings {
                for &name in names {
                    registry.global(&mut ctx, name, "wl_shm", WL_SHM_VERSION);
                }
            }
            ctx.client_to_host_queue.clear();
            for &step in case.steps {
                match step {
                    Remove(registry, name) => {
                        registry.remove(&mut ctx, name);
                    }
                    ReuseAsOutput(registry, name) => {
                        registry.global(&mut ctx, name, "wl_output", 3);
                    }
                    ReadvertiseShm(registry, name) => {
                        registry.global(&mut ctx, name, "wl_shm", WL_SHM_VERSION);
                    }
                    Destroy(registry) => assert_eq!(registry.destroy(&mut ctx), Action::Forward),
                }
                ctx.assert_no_orphans();
            }

            assert_eq!(Singleton::Shm.bound_name(&ctx), case.bound, "{}", case.name);
            let binds = case.binds.iter().map(|bind| {
                let request = bind_request(bind.name, "wl_shm", WL_SHM_VERSION, bind.id);
                (bind.through.host, request)
            });
            assert_eq!(
                host_requests(&mut ctx, &[]),
                Vec::from_iter(binds),
                "{}",
                case.name
            );
            let mut candidates = Vec::from_iter(ctx.singleton_candidates.keys().copied());
            candidates.sort_unstable();
            assert_eq!(candidates, case.candidates, "{}", case.name);
        }
    }

    /// The guest global of a replacement linux-dmabuf waits, like any, for
    /// the capabilities of its own binding.
    #[test]
    fn a_promoted_dmabuf_publishes_its_deferred_guest_global() {
        let mut ctx = gpu_context(&[A]);
        ctx.synthetic_feedback_available_for_test = true;
        let dmabuf = "zwp_linux_dmabuf_v1";
        assert_eq!(A.global(&mut ctx, 10, dmabuf, 4), Action::Drop);
        assert_eq!(A.global(&mut ctx, 11, dmabuf, 4), Action::Drop);
        let replacement = ctx.global_generations[&11];
        assert_eq!(ctx.pending_dmabuf_globals.len(), 2);

        assert_eq!(A.remove(&mut ctx, 10), Action::Drop);
        assert_eq!(ctx.host_dmabuf_generation, Some(replacement));
        assert!(!ctx.host_globals.contains_key(&11));
        assert_eq!(guest_globals(&mut ctx), []);
        ctx.dmabuf_capabilities
            .get_mut(&replacement)
            .expect("the replacement's capabilities")
            .format_modifiers
            .push(XRGB8888_LINEAR);
        crate::handler::linux_dmabuf::complete_capability_discovery(&mut ctx, replacement);

        assert_eq!(
            ctx.host_globals.get(&11),
            Some(&HostGlobal {
                interface: dmabuf.to_string(),
                version: 4,
            })
        );
        assert!(ctx.pending_dmabuf_globals.is_empty());
        assert_eq!(
            guest_globals(&mut ctx),
            [(A.guest, global_event(11, dmabuf, 4))]
        );
    }

    /// libwayland-server lets a bind cross its global's removal on the
    /// wire, as when a dock flaps and the client binds the output it saw
    /// advertised in the same dispatch. Such an output becomes an inert
    /// guest object. A name never advertised to the binding registry stays
    /// a protocol error.
    #[test]
    fn bind_crossing_an_output_removal_creates_an_inert_output() {
        let mut ctx = context(&[A, B]);
        assert_eq!(A.global(&mut ctx, 10, "wl_output", 3), Action::Forward);
        assert_eq!(A.remove(&mut ctx, 10), Action::Forward);
        assert_eq!(B.global(&mut ctx, 11, "wl_seat", 5), Action::Forward);

        A.bind(&mut ctx, 10, "wl_output", 3, 20);
        assert!(!ctx.is_fatal());
        assert_eq!(host_requests(&mut ctx, &[]), [], "nothing reaches the host");
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert!(ctx
            .shadow_table
            .request_object(20, Iface::WlOutput)
            .is_some());
        assert_eq!(ctx.shadow_table.guest_object_version(20), Some(3));
        assert!(ctx.outputs.is_empty_for_test());

        B.bind(&mut ctx, 10, "wl_output", 3, 21);
        assert_eq!(
            protocol_error(&ctx),
            (B.guest, wl_display::ERROR_INVALID_OBJECT),
            "name 10 is retired, and registry B never saw it"
        );
        assert_eq!(ctx.shadow_table.get_interface(21), None);
    }

    /// Registries A and B list global 7, a `wl_seat`, whose removal only
    /// registry A has seen.
    fn a_global_removed_from_one_of_two_registries() -> Context {
        let mut ctx = context(&[A, B]);
        for registry in [A, B] {
            assert_eq!(registry.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);
        }
        assert_eq!(A.remove(&mut ctx, 7), Action::Forward);
        assert!(
            ctx.host_globals.contains_key(&7),
            "registry B has not seen the removal yet"
        );
        ctx.assert_no_orphans();
        ctx
    }

    /// The removal of a global the proxy neither advertises nor binds is
    /// on record too, and waits for every registry that lists it.
    #[test]
    fn the_removal_of_a_dropped_global_waits_for_every_registry() {
        let mut ctx = context(&[A, B]);
        for registry in [A, B] {
            assert_eq!(
                registry.global(&mut ctx, 7, "zz_unknown_v1", 1),
                Action::Drop
            );
        }
        assert_eq!(A.remove(&mut ctx, 7), Action::Drop);
        assert!(ctx.removed_global_generations.contains_key(&7));
        ctx.assert_no_orphans();

        B.destroy(&mut ctx);
        assert!(ctx.removed_global_generations.is_empty());
        ctx.assert_no_orphans();
    }

    /// O3: a global removed from one registry waits for every registry that
    /// still lists it. Destroying the last of those through `wl_fixes`
    /// means it never sees the removal, so the global is retired then.
    #[test]
    fn destroying_the_last_registry_to_list_a_removed_global_retires_it() {
        let mut ctx = a_global_removed_from_one_of_two_registries();

        assert_eq!(B.destroy(&mut ctx), Action::Forward);

        assert!(!ctx.host_globals.contains_key(&7));
    }

    /// The removal stays on record when the only registry that saw it is
    /// destroyed first, so destroying the last one still retires the
    /// global.
    #[test]
    fn a_removal_outlives_the_registry_that_saw_it() {
        let mut ctx = a_global_removed_from_one_of_two_registries();

        assert_eq!(A.destroy(&mut ctx), Action::Forward);
        assert!(ctx.host_globals.contains_key(&7), "registry B lists it");
        assert_eq!(B.destroy(&mut ctx), Action::Forward);

        assert!(!ctx.host_globals.contains_key(&7));
        assert!(ctx.removed_global_generations.is_empty());
    }

    /// A registry destroyed while a global is still advertised leaves it
    /// to the other registries.
    #[test]
    fn destroying_a_registry_keeps_a_global_no_registry_saw_removed() {
        let mut ctx = context(&[A]);
        assert_eq!(A.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);

        assert_eq!(A.destroy(&mut ctx), Action::Forward);

        assert!(ctx.host_globals.contains_key(&7));
    }

    #[test]
    fn stale_remove_cannot_invalidate_a_re_advertised_global_generation() {
        let mut ctx = context(&[A, B]);
        assert_eq!(A.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);
        assert_eq!(B.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);

        // Registry A observes removal and receives a replacement with the
        // same numeric name before registry B delivers the old remove.
        assert_eq!(A.remove(&mut ctx, 7), Action::Forward);
        assert_eq!(A.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);

        // B's delayed remove belongs to generation 1. It must be forwarded to
        // B, but must not retire generation 2 advertised to A.
        assert_eq!(B.remove(&mut ctx, 7), Action::Forward);
        assert!(
            ctx.host_globals.contains_key(&7),
            "an old registry remove must not invalidate the replacement"
        );
        ctx.assert_no_orphans();

        A.bind(&mut ctx, 7, "wl_seat", 5, 20);
        assert!(ctx.shadow_table.get_host_id(20).is_some());
    }

    #[test]
    fn re_advertised_name_replaces_visible_metadata_before_hidden_global() {
        let mut ctx = context(&[A]);
        let keyboard_extension = "zcr_keyboard_extension_v1";
        assert_eq!(A.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);
        assert_eq!(A.remove(&mut ctx, 7), Action::Forward);

        // The host reused name 7 for a hidden singleton. The old visible
        // wl_seat metadata must not survive and authorize a stale bind.
        assert_eq!(A.global(&mut ctx, 7, keyboard_extension, 2), Action::Drop);
        assert!(
            !ctx.host_globals.contains_key(&7),
            "reused name must not retain the old visible interface metadata"
        );
        assert_eq!(
            ctx.host_keyboard_extension_global_name,
            Some(7),
            "the replacement hidden global must own the numeric name"
        );

        A.bind(&mut ctx, 7, "wl_seat", 5, 20);
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
    }

    #[test]
    fn re_advertised_hidden_name_replaces_hidden_metadata_before_visible_global() {
        let mut ctx = context(&[A]);
        assert_eq!(
            A.global(&mut ctx, 7, "zcr_keyboard_extension_v1", 2),
            Action::Drop
        );
        assert_eq!(A.remove(&mut ctx, 7), Action::Drop);

        // Name 7 is now a normal visible global. A stale hidden marker would
        // make its later global_remove disappear instead of reaching the guest.
        assert_eq!(A.global(&mut ctx, 7, "wl_seat", 5), Action::Forward);
        assert_eq!(
            ctx.host_globals.get(&7),
            Some(&HostGlobal {
                interface: "wl_seat".to_string(),
                version: 5,
            })
        );
        assert_eq!(
            A.remove(&mut ctx, 7),
            Action::Forward,
            "the visible replacement must forward its own removal"
        );
    }

    #[test]
    fn delayed_remove_does_not_reset_replacement_internal_binding() {
        let mut ctx = context(&[A, B]);
        let keyboard_extension = "zcr_keyboard_extension_v1";
        for registry in [A, B] {
            assert_eq!(
                registry.global(&mut ctx, 7, keyboard_extension, 2),
                Action::Drop
            );
        }
        let old_id = ctx
            .host_keyboard_extension_id
            .expect("initial keyboard extension binding");

        // Registry A sees the host generation disappear and immediately sees
        // the replacement. The old internal object is retired and a fresh
        // binding is created for the new generation.
        assert_eq!(A.remove(&mut ctx, 7), Action::Drop);
        assert_eq!(A.global(&mut ctx, 7, keyboard_extension, 2), Action::Drop);
        let replacement_id = ctx
            .host_keyboard_extension_id
            .expect("replacement keyboard extension binding");
        assert_ne!(replacement_id, old_id);
        assert_eq!(
            ctx.shadow_table.get_host_interface(replacement_id.0),
            Some(Iface::ZcrKeyboardExtensionV1)
        );

        // Registry B's queued remove belongs to generation 1. It must not
        // tear down the generation-2 internal object that is now active.
        assert_eq!(B.remove(&mut ctx, 7), Action::Drop);
        assert_eq!(ctx.host_keyboard_extension_id, Some(replacement_id));
        assert_eq!(
            ctx.shadow_table.get_host_interface(replacement_id.0),
            Some(Iface::ZcrKeyboardExtensionV1)
        );
    }

    #[test]
    fn compositor_guest_version_and_host_bind_are_negotiated_separately() {
        let compositor = "wl_compositor";
        // A global the guest may take as the host offers it is forwarded;
        // the proxy advertises any other at the guest's version itself.
        for (host_version, guest_version, bound_version, action) in [
            (1, 1, 1, Action::Forward),
            (2, 2, 2, Action::Forward),
            (3, 4, 3, Action::Drop),
            (4, 4, 3, Action::Forward),
            (6, 4, 3, Action::Drop),
        ] {
            let mut ctx = context(&[A]);
            assert_eq!(
                A.global(&mut ctx, 7, compositor, host_version),
                action,
                "v{host_version}"
            );
            let advertised = (action == Action::Drop)
                .then(|| (A.guest, global_event(7, compositor, guest_version)));
            assert_eq!(guest_globals(&mut ctx), Vec::from_iter(advertised));

            A.bind(&mut ctx, 7, compositor, guest_version, 20);
            assert!(!ctx.is_fatal());
            let host_id = ctx
                .shadow_table
                .get_host_id(20)
                .expect("paired host object");
            assert_eq!(
                ctx.shadow_table.guest_object_version(20),
                Some(guest_version)
            );
            assert_eq!(
                ctx.shadow_table.host_object_version(host_id),
                Some(bound_version)
            );
            assert_eq!(
                host_requests(&mut ctx, &[]),
                [(A.host, bind_request(7, compositor, bound_version, host_id))]
            );

            A.bind(&mut ctx, 7, compositor, guest_version + 1, 21);
            assert_eq!(
                protocol_error(&ctx),
                (A.guest, wl_display::ERROR_INVALID_OBJECT)
            );
            assert_eq!(ctx.shadow_table.get_host_id(21), None);
            assert_eq!(host_requests(&mut ctx, &[]), []);
        }
    }

    #[test]
    fn every_forwarded_global_is_capped_at_its_xml_version() {
        let ctx = Context::new_for_test(false, true, vec![]);
        let forwarded = FORWARDED_PROTOCOLS
            .into_iter()
            .chain([XDG_DECORATION_ALLOWED])
            .flatten();
        for &interface in forwarded {
            let xml_version = Iface::from_name(interface)
                .expect("a forwarded interface comes from the vendored XML")
                .version();
            let offered = forwarded_global_version(&ctx, interface, u32::MAX)
                .expect("the interface is forwarded");
            assert!(
                offered <= xml_version,
                "{interface} is offered at v{offered}, newer than its XML's v{xml_version}"
            );
        }
        assert_eq!(forwarded_global_version(&ctx, "unknown_interface", 1), None);
    }

    #[test]
    fn ordinary_globals_are_capped_to_supported_versions() {
        let mut ctx = context(&[A]);
        for (interface, host_version, offered) in [
            ("wl_compositor", 6, 4),
            ("wl_compositor", 3, 4),
            ("wl_compositor", 2, 2),
            ("wl_output", 4, 3),
            ("wl_seat", 10, 5),
            ("wl_data_device_manager", 99, 3),
            ("xdg_wm_base", 7, 3),
            ("zxdg_output_manager_v1", 4, 3),
        ] {
            assert_eq!(
                forwarded_global_version(&ctx, interface, host_version),
                Some(offered),
                "{interface} v{host_version}"
            );
        }

        assert_eq!(
            A.global(&mut ctx, 9, "zxdg_output_manager_v1", 3),
            Action::Forward
        );
        assert_eq!(A.global(&mut ctx, 10, "wl_seat", 10), Action::Drop);
        assert_eq!(
            ctx.host_globals.get(&10),
            Some(&HostGlobal {
                interface: "wl_seat".to_string(),
                version: 5
            })
        );
        assert_eq!(
            guest_globals(&mut ctx),
            [(A.guest, global_event(10, "wl_seat", 5))],
            "the guest must receive the capped version"
        );
    }
}
