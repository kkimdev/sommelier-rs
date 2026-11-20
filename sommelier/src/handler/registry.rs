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
use crate::protocols::gtk::gtk_shell1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::wayland::{wl_display, wl_fixes, wl_registry, wl_shm, wl_surface};
use crate::protocols::Iface;
use crate::settings::Settings;
use crate::state::{
    Binding, Context, GlobalBinding, GtkShellState, HostGlobal, InternalCallback,
    PendingDmabufGlobal, Singleton, INERT_BIND_INTERFACE,
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
/// The `wl_output` ceiling; [`offered_version`] says why. The
/// vendored XML is newer: v4 adds the `name` and `description` events.
const WL_OUTPUT_VERSION: u32 = 3;
/// The `wl_seat` ceiling; [`offered_version`] says why. The
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

/// What the proxy does with a host global, by its interface (see
/// [`policy`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Policy {
    /// Forward it to the guest while `enabled` says so, offered at the
    /// host's version capped at `ceiling` ([`offered_version`]), and hide
    /// it otherwise.
    Forward {
        ceiling: u32,
        enabled: fn(&Settings) -> bool,
    },
    /// Keep it from the guest. An interface that is no global, which no
    /// compositor advertises, is hidden too.
    Hide,
    /// Bind it for the proxy's own use; its handler decides what the guest
    /// sees in its place.
    BindInternal(Internal),
    /// Keep it from the guest, which binds the proxy's own global of this
    /// interface instead, advertised for another host global.
    Synthetic,
}

/// A host global the proxy binds for its own use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Internal {
    /// Bound once per connection, and bound anew when the binding's global
    /// retires (`Context::singletons`).
    Singleton(Singleton),
    /// The first `zwp_text_input_manager_v1`, kept for the connection: the
    /// guest is offered a `zwp_text_input_manager_v3` under its name.
    TextInputManager,
    /// The first `zcr_text_input_extension_v1`, kept for the connection.
    TextInputExtension,
}

/// A global forwarded whatever the settings, offered at most at `ceiling`.
const fn forward(ceiling: u32) -> Policy {
    Policy::Forward {
        ceiling,
        enabled: |_| true,
    }
}

/// What the proxy does with a host global of `iface`: the one table of
/// the globals it knows. Its `match` names every interface, so a new
/// interface needs a row, and a forwarded global needs nothing more.
///
/// A forwarded global's ceiling is its generated `VERSION` unless a named
/// constant above documents why it deliberately differs; no ceiling is
/// above the XML (test: `every_interface_has_one_global_policy`).
pub(crate) const fn policy(iface: Iface) -> Policy {
    match iface {
        Iface::WlCompositor => forward(WL_COMPOSITOR_VERSION),
        Iface::WlSubcompositor => forward(Iface::WlSubcompositor.version()),
        Iface::WlDataDeviceManager => forward(Iface::WlDataDeviceManager.version()),
        Iface::WlShell => forward(Iface::WlShell.version()),
        Iface::WlSeat => forward(WL_SEAT_VERSION),
        Iface::WlOutput => forward(WL_OUTPUT_VERSION),
        Iface::WlFixes => forward(Iface::WlFixes.version()),
        Iface::XdgWmBase => forward(XDG_SHELL_VERSION),
        Iface::WpViewporter => forward(Iface::WpViewporter.version()),
        Iface::WpFractionalScaleManagerV1 => forward(Iface::WpFractionalScaleManagerV1.version()),
        Iface::ZxdgOutputManagerV1 => forward(Iface::ZxdgOutputManagerV1.version()),
        // A host's own v3 manager is offered too; the proxy serves the
        // guest's bind of any (`on_bind`).
        Iface::ZwpTextInputManagerV3 => forward(Iface::ZwpTextInputManagerV3.version()),
        Iface::ZxdgDecorationManagerV1 => Policy::Forward {
            ceiling: Iface::ZxdgDecorationManagerV1.version(),
            enabled: |settings| settings.xdg_decoration,
        },
        Iface::WlShm => Policy::BindInternal(Internal::Singleton(Singleton::Shm)),
        Iface::ZwpLinuxDmabufV1 => Policy::BindInternal(Internal::Singleton(Singleton::Dmabuf)),
        Iface::ZcrKeyboardExtensionV1 => {
            Policy::BindInternal(Internal::Singleton(Singleton::KeyboardExtension))
        }
        Iface::ZauraShell => Policy::BindInternal(Internal::Singleton(Singleton::AuraShell)),
        Iface::ZwpTextInputManagerV1 => Policy::BindInternal(Internal::TextInputManager),
        Iface::ZcrTextInputExtensionV1 => Policy::BindInternal(Internal::TextInputExtension),
        // Advertised by the proxy for `zaura_shell`.
        Iface::GtkShell1 => Policy::Synthetic,
        // No globals.
        Iface::WlDisplay
        | Iface::WlRegistry
        | Iface::WlCallback
        | Iface::WlShmPool
        | Iface::WlBuffer
        | Iface::WlDataOffer
        | Iface::WlDataSource
        | Iface::WlDataDevice
        | Iface::WlShellSurface
        | Iface::WlSurface
        | Iface::WlPointer
        | Iface::WlKeyboard
        | Iface::WlTouch
        | Iface::WlRegion
        | Iface::WlSubsurface
        | Iface::XdgPositioner
        | Iface::XdgSurface
        | Iface::XdgToplevel
        | Iface::XdgPopup
        | Iface::ZwpLinuxBufferParamsV1
        | Iface::ZwpLinuxDmabufFeedbackV1
        | Iface::WpViewport
        | Iface::ZwpTextInputV3
        | Iface::ZwpTextInputV1
        | Iface::ZcrExtendedTextInputV1
        | Iface::ZxdgToplevelDecorationV1
        | Iface::WpFractionalScaleV1
        | Iface::ZxdgOutputV1
        | Iface::ZcrExtendedKeyboardV1
        | Iface::GtkSurface1
        | Iface::ZauraSurface
        | Iface::ZauraOutput
        | Iface::ZauraToplevel
        | Iface::ZauraPopup => Policy::Hide,
    }
}

/// The version the guest is offered of a forwarded host global of `iface`
/// that the host advertises at `host_version`, capped at `ceiling`.
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
fn offered_version(iface: Iface, ceiling: u32, host_version: u32) -> u32 {
    // ChromiumOS implements wl_surface.damage_buffer in terms of the legacy
    // damage request, so a v3 host compositor is offered as v4. Hosts below
    // v3 retain their original version; the proxy does not expose v5/v6
    // surface semantics.
    let host_version = if iface == Iface::WlCompositor && host_version >= WL_COMPOSITOR_HOST_VERSION
    {
        WL_COMPOSITOR_VERSION
    } else {
        host_version
    };
    host_version.min(ceiling).min(iface.version())
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

impl Singleton {
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
                ctx.singletons
                    .bind(self, binding(host_id, name, bound_version));
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
                ctx.singletons
                    .bind(self, binding(host_id, name, bound_version));
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
                ctx.singletons
                    .bind(self, binding(host_id, name, bound_version));
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
                ctx.singletons
                    .bind(self, binding(host_id, name, WL_SHM_VERSION));
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

/// The binding of host object `host` from global `name` at `version`.
fn binding(host: u32, name: u32, version: u32) -> Binding {
    Binding {
        host,
        name,
        version,
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
    if ctx.singletons.get(singleton).is_some() {
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

/// Release the binding that host global `name` backs, if one does.
///
/// ChromiumOS destroys the internally bound proxy when its host registry
/// global disappears. Keeping the old object registered would let a
/// re-advertised global reuse stale capability/state and route events to an
/// object that no longer represents the current host global.
///
/// The text-input managers have no destructor, and text inputs still
/// create children from them after their globals go, so they are never
/// reset or rebound (see `text_input_manager_global`).
fn reset_internal_binding_for_global(ctx: &mut Context, name: u32) {
    let Some((singleton, binding)) = ctx.singletons.release_named(name) else {
        return;
    };
    match singleton {
        Singleton::Dmabuf => {
            let generation = ctx.host_dmabuf_generation.take();
            // A registry global removal invalidates the name, not objects
            // already bound from it. Explicitly destroy our hidden binding
            // so the host does not retain a proxy until connection
            // teardown.
            ctx.client_to_host_queue
                .push(zwp_linux_dmabuf_v1::req::destroy(binding.host));
            ctx.retire_destroyed_host_object(binding.host);
            clear_host_shm_dmabuf_formats(ctx);
            if let Some(generation) = generation {
                crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
            }
        }
        Singleton::Shm => {
            ctx.shadow_table.retire_host(binding.host);
            clear_host_shm_wl_formats(ctx);
        }
        Singleton::KeyboardExtension => {
            // `global_remove` invalidates only the manager global name. It
            // does not destroy `zcr_extended_keyboard_v1` children that
            // were already created with get_extended_keyboard; those
            // children have their own destructor and remain usable until
            // the corresponding wl_keyboard is released. Keep both maps and
            // the child dispatch registrations so queued peek_key events
            // remain routable after the manager disappears.
            ctx.shadow_table.retire_host(binding.host);
        }
        Singleton::AuraShell => {
            // `global_remove` invalidates only the advertised global name.
            // Already created `zaura_surface` children have their own
            // protocol lifetime and remain usable until their owning
            // wl_surface is destroyed. Keep their mappings and per-object
            // version metadata so an existing surface can still receive
            // set_application_id after the manager global disappears. This
            // also avoids sending a release for a child that the host never
            // asked us to destroy.
            //
            // Release only the internally bound manager, matching
            // ChromiumOS' registry remover. New children cannot be created
            // until a replacement shell global is advertised and rebound.
            crate::handler::aura_objects::release_shell(ctx, binding);
        }
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
        if starts_new_generation {
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

/// A host global of `iface` the proxy forwards, at the version it offers
/// ([`offered_version`]).
fn forwarded_global(
    ctx: &mut Context,
    name: u32,
    iface: Iface,
    ceiling: u32,
    version: u32,
) -> Action {
    let advertised_version = offered_version(iface, ceiling, version);
    if advertised_version != version {
        advertise_synthetic_global(ctx, name, iface.name(), advertised_version);
        return Action::Drop;
    }
    ctx.host_globals.insert(
        name,
        HostGlobal {
            interface: iface.name().to_string(),
            version: advertised_version,
        },
    );
    record_registry_global_visibility(ctx, name, true);
    Action::Forward
}

/// A host global the proxy keeps from the guest.
fn hidden_global(ctx: &mut Context, name: u32, interface: &str) -> Action {
    if interface.contains(char::REPLACEMENT_CHARACTER) {
        log::debug!("Hiding global {name}, whose interface name {interface:?} is not UTF-8");
    }
    record_registry_global_visibility(ctx, name, false);
    Action::Drop
}

impl wl_registry::WlRegistryHandler for Handlers {
    /// A host global: the proxy binds it for itself, advertises a global
    /// of its own in its place, forwards it, or hides it, by its
    /// interface.
    fn on_global(ctx: &mut Context, name: u32, interface: &String, version: u32) -> Action {
        if let Err(action) = accept_global(ctx, name, interface, version) {
            return action;
        }
        let Some(iface) = Iface::from_name(interface) else {
            return hidden_global(ctx, name, interface);
        };
        match policy(iface) {
            Policy::Forward { ceiling, enabled } if enabled(ctx.settings()) => {
                forwarded_global(ctx, name, iface, ceiling, version)
            }
            Policy::Forward { .. } | Policy::Hide | Policy::Synthetic => {
                hidden_global(ctx, name, interface)
            }
            Policy::BindInternal(Internal::Singleton(singleton)) => match singleton {
                Singleton::Dmabuf => dmabuf_global(ctx, name, version),
                Singleton::KeyboardExtension => keyboard_extension_global(ctx, name, version),
                Singleton::AuraShell => aura_shell_global(ctx, name, version),
                Singleton::Shm => shm_global(ctx, name, version),
            },
            Policy::BindInternal(Internal::TextInputManager) => {
                text_input_manager_global(ctx, name, version)
            }
            Policy::BindInternal(Internal::TextInputExtension) => {
                text_input_extension_global(ctx, name, version)
            }
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
        keyboard_extension_version, offered_version, policy, should_bind_internal_dmabuf, Internal,
        Policy, KEYBOARD_EXTENSION_VERSION, LINUX_DMABUF_CAPABILITY_VERSION, WL_SHM_VERSION,
        ZAURA_SHELL_MIN_VERSION,
    };
    use crate::handler::Handlers;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
    use crate::protocols::gtk::{gtk_shell1, gtk_surface1};
    use crate::protocols::keyboard_extension_unstable_v1::{
        zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
    };
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::{
        wl_callback, wl_display, wl_fixes, wl_keyboard, wl_output, wl_registry, wl_shm, wl_surface,
    };
    use crate::protocols::{AnyEvent, AnyRequest, Iface};
    use crate::proxy::harness::{Object, Session};
    use crate::state::{Context, HostGlobal, Singleton};
    use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};

    /// How many IDs the guest skips before its first object. The proxy
    /// allocates host IDs densely from the bottom, so without the gap a
    /// registry could have the same ID on both sides, and looking up the
    /// wrong one would pass.
    const GUEST_ID_LEAD: u32 = 10;

    /// One of the guest's two registries.
    #[derive(Clone, Copy, Debug)]
    enum Registry {
        A,
        B,
    }
    use Registry::{A, B};

    /// What a table test has the host or the guest do to the registries.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// The host advertises global `name` of `interface` at `version` on
        /// the registry.
        Global(Registry, u32, &'static str, u32),
        /// The host removes global `name` from the registry.
        Remove(Registry, u32),
        /// The guest destroys the registry through `wl_fixes`.
        Destroy(Registry),
    }
    use Step::{Destroy, Global, Remove};

    /// A whole session for registry tests: the guest's registries [`A`]
    /// (the session's [`Session::registry`]) and [`B`], and the
    /// `wl_fixes` it destroys them with, which the host advertised on `A`
    /// as global 1.
    struct Registries {
        s: Session,
        a: Object,
        b: Object,
        fixes: Object,
    }

    impl Registries {
        /// A session without GPU access.
        async fn new() -> Self {
            Self::with_context(Context::new_for_test(false, false, vec![])).await
        }

        /// A session with GPU access, which the proxy needs to offer the
        /// host's linux-dmabuf, and with synthetic dma-buf feedback if
        /// `feedback`.
        async fn gpu(feedback: bool) -> Self {
            let mut ctx = Context::new_for_test(true, false, vec![]);
            // Stands in for a virtio-gpu render node, which the proxy
            // synthesizes feedback for and a test cannot have.
            ctx.synthetic_feedback_available_for_test = feedback;
            Self::with_context(ctx).await
        }

        /// A session whose window placement uses Aura.
        async fn aura_placement() -> Self {
            let placement = PlacementConfig::active_for_test(&[]);
            Self::with_context(Context::with_placement_for_test(placement)).await
        }

        async fn with_context(ctx: Context) -> Self {
            let mut s = Session::with_context(ctx);
            for _ in 0..GUEST_ID_LEAD {
                s.new_id();
            }
            let a = s.registry().await;
            let b = s.new_registry().await;
            let fixes = s.bind_paired(wl_fixes::NAME, wl_fixes::VERSION).await;
            assert_ne!(a.id, a.host);
            Self { s, a, b, fixes }
        }

        fn of(&self, registry: Registry) -> Object {
            match registry {
                A => self.a,
                B => self.b,
            }
        }

        /// The host advertises global `name` of `interface` at `version`
        /// on `registry`.
        async fn global(&mut self, registry: Registry, name: u32, interface: &str, version: u32) {
            let registry = self.of(registry);
            self.s.global(registry, name, interface, version).await;
        }

        /// The host removes global `name` from `registry`.
        async fn remove(&mut self, registry: Registry, name: u32) {
            let registry = self.of(registry);
            self.s.global_remove(registry, name).await;
        }

        /// The guest binds global `name` as `interface` at `version`
        /// through `registry`; returns its new object.
        async fn bind(
            &mut self,
            registry: Object,
            name: u32,
            interface: &str,
            version: u32,
        ) -> u32 {
            let id = self.s.new_id();
            let bind = wl_registry::req::bind(registry.id, name, interface, version, id).unwrap();
            self.s.guest([bind]).await;
            id
        }

        /// [`Self::bind`] of a global the proxy forwards, which the host
        /// gets alone through `registry`, at the same version; returns the
        /// guest's object and the host's.
        async fn bind_paired(
            &mut self,
            registry: Object,
            name: u32,
            interface: &str,
            version: u32,
        ) -> Object {
            let id = self.bind(registry, name, interface, version).await;
            let host = self.s.host_received();
            let [forward] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let bound = bound(forward);
            assert_eq!(
                host,
                [(registry.host, bind_request(name, interface, version, bound))]
            );
            Object { id, host: bound }
        }

        /// [`Self::bind`], which the proxy refuses with an
        /// `INVALID_OBJECT` error on `registry` that ends the session; the
        /// host gets nothing.
        async fn bind_fatal(&mut self, registry: Object, name: u32, interface: &str, version: u32) {
            let id = self.s.new_id();
            let bind = wl_registry::req::bind(registry.id, name, interface, version, id).unwrap();
            self.s.guest_fatal([bind]).await;
            assert_eq!(
                self.s.guest_received_and_error(),
                (vec![], (registry.id, wl_display::ERROR_INVALID_OBJECT))
            );
            assert_eq!(self.s.host_received(), []);
        }

        /// The guest destroys `registry` through `wl_fixes`, which the
        /// proxy forwards, and the host acknowledges it with `delete_id`,
        /// which the guest gets; returns what else the proxy sent the host.
        async fn destroy(&mut self, registry: Registry) -> Vec<(u32, AnyRequest)> {
            let registry = self.of(registry);
            let destroy = wl_fixes::req::destroy_registry(self.fixes.id, registry.id);
            self.s.guest([destroy]).await;
            let mut host = self.s.host_received();
            let forward = |request: &(u32, AnyRequest)| {
                let destroy = wl_fixes::Request::DestroyRegistry {
                    registry: registry.host,
                };
                *request == (self.fixes.host, destroy.into())
            };
            let at = host
                .iter()
                .position(forward)
                .unwrap_or_else(|| panic!("the host got no destroy_registry: {host:?}"));
            host.remove(at);
            self.s
                .host([wl_display::evt::delete_id(WL_DISPLAY_ID, registry.host)])
                .await;
            let deleted = wl_display::Event::DeleteId { id: registry.id };
            assert_eq!(self.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
            host
        }

        /// Run `step`; returns what the proxy sent the host for it. What
        /// the guest gets is kept.
        async fn run(&mut self, step: Step) -> Vec<(u32, AnyRequest)> {
            match step {
                Global(registry, name, interface, version) => {
                    self.global(registry, name, interface, version).await;
                }
                Remove(registry, name) => self.remove(registry, name).await,
                Destroy(registry) => return self.destroy(registry).await,
            }
            self.s.host_received()
        }
    }

    /// A `wl_registry.global` event.
    fn global(name: u32, interface: &str, version: u32) -> AnyEvent {
        wl_registry::Event::Global {
            name,
            interface: interface.to_string(),
            version,
        }
        .into()
    }

    /// A `wl_registry.global_remove` event.
    fn global_remove(name: u32) -> AnyEvent {
        wl_registry::Event::GlobalRemove { name }.into()
    }

    fn bind_request(name: u32, interface: &str, version: u32, id: u32) -> AnyRequest {
        wl_registry::Request::Bind {
            name,
            id: (interface.to_string(), version, id),
        }
        .into()
    }

    /// The host object that `request`, a bind, creates.
    fn bound(request: &(u32, AnyRequest)) -> u32 {
        match request {
            (_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })) => id.2,
            _ => panic!("{request:?} is no bind"),
        }
    }

    /// The `wl_display.sync` of callback `callback`.
    fn sync(callback: u32) -> (u32, AnyRequest) {
        (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())
    }

    /// A format the proxy can offer in synthetic linux-dmabuf feedback:
    /// `XRGB8888` with the linear modifier.
    const XRGB8888_LINEAR: (u32, u64) = (0x3432_5258, 0);

    /// The proxy's binding of host linux-dmabuf global `name` through
    /// `registry`, at v3, which asks for the binding's capabilities with a
    /// sync: `requests`, which are all the host got. Returns the binding
    /// and the sync's callback.
    fn dmabuf_binding(requests: &[(u32, AnyRequest)], registry: Object, name: u32) -> (u32, u32) {
        let [bind, (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            requests
        else {
            panic!("the host got {requests:?}, not a bind and a sync");
        };
        let dmabuf = bound(bind);
        let expected = bind_request(name, zwp_linux_dmabuf_v1::NAME, 3, dmabuf);
        assert_eq!(*bind, (registry.host, expected));
        (dmabuf, *callback)
    }

    /// The host's answer to capability sync `callback` of linux-dmabuf
    /// binding `dmabuf`: its `formats`, the callback's `done`, and the
    /// callback's `delete_id`.
    fn capabilities(dmabuf: u32, callback: u32, formats: &[(u32, u64)]) -> Vec<Outgoing> {
        let formats = formats.iter().map(|&(format, modifier)| {
            let (hi, lo) = ((modifier >> 32) as u32, modifier as u32);
            zwp_linux_dmabuf_v1::evt::modifier(dmabuf, format, hi, lo)
        });
        formats
            .chain([
                wl_callback::evt::done(callback, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
            ])
            .collect()
    }

    /// The guest's `id`, a `wl_shm` or `gtk_shell1` it just bound, is the
    /// proxy's own: the host gets no bind for it, and the guest gets what
    /// the proxy serves it, `wl_shm`'s mandatory formats, or `gtk_shell1`'s
    /// capabilities once the host has answered the proxy's sync.
    async fn assert_served_locally(s: &mut Session, id: u32, interface: &str) {
        match interface {
            wl_shm::NAME => {
                assert_eq!(s.host_received(), [], "no second host bind");
                assert_eq!(
                    s.guest_received(),
                    [
                        (id, wl_shm::Event::Format { format: 0 }.into()),
                        (id, wl_shm::Event::Format { format: 1 }.into()),
                    ],
                    "only the mandatory formats before the host's capabilities"
                );
            }
            gtk_shell1::NAME => {
                let host = s.host_received();
                let [(WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
                    host[..]
                else {
                    panic!("the host got {host:?}, not one sync");
                };
                assert_eq!(s.guest_received(), []);
                s.host([
                    wl_callback::evt::done(callback, 0),
                    wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
                ])
                .await;
                let capabilities = gtk_shell1::Event::Capabilities { capabilities: 0 };
                assert_eq!(s.guest_received(), [(id, capabilities.into())]);
            }
            _ => panic!("the proxy serves no {interface}"),
        }
    }

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

    /// A registry destroyed through `wl_fixes` takes the linux-dmabuf
    /// global it was to be advertised with it: once the capabilities
    /// arrive, no registry gets it. The session's check that no state
    /// outlives its object pins that the proxy drops the deferred
    /// advertisement with the registry.
    #[tokio::test]
    async fn wl_fixes_destroy_registry_removes_deferred_dmabuf_advertisements() {
        let mut r = Registries::gpu(true).await;
        r.global(B, 7, zwp_linux_dmabuf_v1::NAME, 4).await;
        let (dmabuf, callback) = dmabuf_binding(&r.s.host_received(), r.b, 7);
        assert_eq!(r.s.guest_received(), [], "the global waits for its formats");

        assert_eq!(r.destroy(B).await, []);

        let formats = capabilities(dmabuf, callback, &[XRGB8888_LINEAR]);
        r.s.host(formats).await;
        assert_eq!(r.s.guest_received(), []);
        assert_eq!(r.s.host_received(), []);
    }

    /// Each registry gets its own synthetic global, under the name its host
    /// registry gave it, and every such name is bindable.
    #[tokio::test]
    async fn synthetic_globals_are_sent_to_each_guest_registry() {
        for (interface, version, bound_version, advertised) in [
            ("wl_shm", 2, 1, wl_shm::NAME),
            ("zaura_shell", 38, 38, gtk_shell1::NAME),
        ] {
            for name_on_b in [7, 8] {
                let mut r = Registries::new().await;
                r.global(A, 7, interface, version).await;
                r.global(B, name_on_b, interface, version).await;
                assert_eq!(
                    r.s.guest_received(),
                    [
                        (r.a.id, global(7, advertised, 1)),
                        (r.b.id, global(name_on_b, advertised, 1)),
                    ],
                    "{interface} as {name_on_b} on B"
                );
                let host = r.s.host_received();
                let [binding] = &host[..] else {
                    panic!("the host got {host:?}, not one bind");
                };
                let binding = bound(binding);
                assert_eq!(
                    host,
                    [(r.a.host, bind_request(7, interface, bound_version, binding))],
                    "{interface} is bound once"
                );

                let id = r.bind(r.b, name_on_b, advertised, 1).await;
                assert_served_locally(&mut r.s, id, advertised).await;
            }
        }
    }

    /// The proxy serves the `gtk_shell1` it advertises for `zaura_shell`
    /// itself, and sends its capabilities once the host has answered a
    /// sync.
    #[tokio::test]
    async fn a_gtk_shell_bind_is_served_locally() {
        let mut r = Registries::new().await;
        r.global(A, 7, zaura_shell::NAME, 38).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(7, "gtk_shell1", 1))]);
        assert_eq!(r.s.host_received().len(), 1, "the proxy binds the shell");

        let gtk_shell = r.bind(r.a, 7, gtk_shell1::NAME, 1).await;
        assert_served_locally(&mut r.s, gtk_shell, gtk_shell1::NAME).await;
    }

    /// The proxy binds each global it uses itself at the newest version
    /// both it and the host support, and keeps each binding's host ID from
    /// reuse: the host, which checks every new object, gets the guest's
    /// next objects under other IDs.
    #[tokio::test]
    async fn internal_bindings_are_capped_and_reserved_from_host_id_reuse() {
        let mut r = Registries::new().await;
        // Offered at v99, each is bound at the newest version the proxy
        // supports: the vendored XML's, or wl_shm's v1.
        let bindings = [
            (
                11,
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
            ),
            (
                12,
                "zcr_text_input_extension_v1",
                zcr_text_input_extension_v1::VERSION,
            ),
            (
                13,
                "zcr_keyboard_extension_v1",
                zcr_keyboard_extension_v1::VERSION,
            ),
            (14, "wl_shm", 1),
        ];
        for (name, interface, _) in bindings {
            r.global(A, name, interface, 99).await;
        }

        let host = r.s.host_received();
        let ids = Vec::from_iter(host.iter().map(bound));
        let binds = bindings.iter().zip(&ids);
        assert_eq!(
            host,
            Vec::from_iter(binds.map(|(&(name, interface, version), &id)| {
                (r.a.host, bind_request(name, interface, version, id))
            }))
        );
        assert_eq!(
            r.s.guest_received(),
            [
                (
                    r.a.id,
                    global(
                        11,
                        zwp_text_input_manager_v3::NAME,
                        zwp_text_input_manager_v3::VERSION
                    )
                ),
                (r.a.id, global(14, wl_shm::NAME, 1)),
            ]
        );
        let surface = r.s.surface().await;
        assert!(!ids.contains(&surface.host), "{ids:?} stay reserved");

        // Offered at v1, each is bound at v1.
        for (name, interface, _) in bindings {
            let mut r = Registries::new().await;
            r.global(A, name, interface, 1).await;
            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(r.a.host, bind_request(name, interface, 1, binding))],
                "{interface}"
            );
            r.s.guest_received();
        }
    }

    /// Without synthetic feedback the guest gets the host's linux-dmabuf at
    /// once, capped at v3, the version the proxy binds to collect legacy
    /// format modifiers.
    #[tokio::test]
    async fn a_dmabuf_without_synthetic_feedback_is_offered_and_bound_at_v3() {
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        for version in [4, 5, 99] {
            let mut r = Registries::gpu(false).await;
            r.global(A, 10, dmabuf, version).await;

            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind and no sync");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(r.a.host, bind_request(10, dmabuf, 3, binding))],
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), [(r.a.id, global(10, dmabuf, 3))]);
            r.bind_paired(r.a, 10, dmabuf, 3).await;
            r.bind_fatal(r.a, 10, dmabuf, 4).await;
        }
    }

    /// With synthetic feedback the guest's linux-dmabuf global waits for
    /// the capabilities of its binding, and stays hidden if the host offers
    /// no format the proxy supports: its removal does not reach the guest
    /// either.
    #[tokio::test]
    async fn a_synthetic_dmabuf_global_waits_for_its_capabilities() {
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        for (formats, offered) in [(&[XRGB8888_LINEAR][..], Some(4)), (&[], None)] {
            let mut r = Registries::gpu(true).await;
            r.global(A, 10, dmabuf, 4).await;
            let (binding, callback) = dmabuf_binding(&r.s.host_received(), r.a, 10);
            assert_eq!(r.s.guest_received(), []);

            r.s.host(capabilities(binding, callback, formats)).await;
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(offered.map(|version| (r.a.id, global(10, dmabuf, version))))
            );

            r.remove(A, 10).await;
            let destroy = zwp_linux_dmabuf_v1::Request::Destroy {};
            assert_eq!(r.s.host_received(), [(binding, destroy.into())]);
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(offered.map(|_| (r.a.id, global_remove(10))))
            );
        }
    }

    /// A global of version 0 is malformed; the proxy drops it.
    #[tokio::test]
    async fn a_zero_version_global_is_dropped() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_seat", 0).await;
        assert_eq!(r.s.guest_received(), []);
        assert_eq!(r.s.host_received(), []);
        r.bind_fatal(r.a, 10, "wl_seat", 1).await;
    }

    /// A second global under a name a registry lists is a duplicate; the
    /// proxy drops it and keeps the first.
    #[tokio::test]
    async fn a_duplicate_global_name_is_dropped() {
        let mut r = Registries::new().await;
        r.global(A, 11, "wl_seat", 5).await;
        r.global(A, 11, "wl_output", 3).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(11, "wl_seat", 5))]);
        r.bind_paired(r.a, 11, "wl_seat", 5).await;
    }

    /// A removed output is forgotten and its `zaura_output` released at
    /// once: a `wl_output` before v3 has no release request to free them
    /// later. The guest's object itself stays valid until it releases it.
    #[tokio::test]
    async fn global_remove_forgets_the_output_and_releases_its_aura_child() {
        let mut r = Registries::aura_placement().await;
        r.global(A, 11, zaura_shell::NAME, 38).await;
        let host = r.s.host_received();
        let [shell] = &host[..] else {
            panic!("the host got {host:?}, not the shell's bind");
        };
        let shell = bound(shell);
        r.global(A, 10, "wl_output", 2).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(11, "gtk_shell1", 1)),
                (r.a.id, global(10, "wl_output", 2)),
            ]
        );
        let output = r.bind(r.a, 10, "wl_output", 2).await;
        let host = r.s.host_received();
        let [bind, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id, .. }))] =
            &host[..]
        else {
            panic!("the output got no Aura output: {host:?}");
        };
        let (output_host, aura_output) = (bound(bind), *id);
        let get_aura_output = zaura_shell::Request::GetAuraOutput {
            id: aura_output,
            output: output_host,
        };
        assert_eq!(host[1], (shell, get_aura_output.into()));

        r.remove(A, 10).await;
        assert_eq!(
            r.s.host_received(),
            [(aura_output, zaura_output::Request::Release {}.into())]
        );
        assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(10))]);
        // Reads state no peer can observe: placement no longer reads the
        // output.
        assert!(r.s.ctx().outputs.is_empty_for_test());

        r.s.host([wl_output::evt::done(output_host)]).await;
        assert_eq!(
            r.s.guest_received(),
            [(output, wl_output::Event::Done {}.into())],
            "the guest's output stays valid"
        );
    }

    /// A bind through a guest registry with no host object cannot be sent,
    /// so it registers no output. No peer can make such a registry, so this
    /// test builds the proxy's state by hand.
    #[test]
    fn output_bind_through_an_unmapped_registry_registers_no_output() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let (registry, host_registry) = (10, 100);
        ctx.shadow_table
            .insert_paired_for_test(registry, host_registry, "wl_registry");
        ctx.last_sender_id = host_registry;
        let output = "wl_output".to_string();
        assert_eq!(
            Handlers::on_global(&mut ctx, 10, &output, 3),
            Action::Forward
        );

        // The guest's registry 12 has no host object, so the bind cannot be
        // sent.
        ctx.last_sender_id = 12;
        Handlers::on_bind(&mut ctx, 10, &(output, 3, 20));
        assert!(!ctx.is_fatal());
        assert_eq!(ctx.shadow_table.get_host_id(20), None);
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx.outputs.is_empty_for_test());
    }

    /// A bind the proxy cannot serve is a protocol error on the guest's
    /// registry, and reaches no host.
    #[tokio::test]
    async fn an_invalid_bind_is_a_protocol_error() {
        struct Case {
            name: &'static str,
            setup: &'static [Step],
            /// Injects a fault no peer can cause, after `setup`.
            fault: Option<fn(&mut Context)>,
            /// The registry the guest binds through.
            through: Registry,
            bind: (u32, &'static str, u32),
        }
        for case in [
            Case {
                name: "a global of another interface",
                setup: &[Global(A, 7, "wl_compositor", 1)],
                fault: None,
                through: A,
                bind: (7, "wl_seat", 1),
            },
            Case {
                name: "a name never advertised",
                setup: &[],
                fault: None,
                through: A,
                bind: (42, "wl_seat", 5),
            },
            // Defence in depth: `host_globals` only ever holds forwarded
            // interfaces, so this bind fails at the name lookup as well and
            // the forwarding guard cannot be isolated by a test.
            Case {
                name: "an interface the proxy does not forward",
                setup: &[],
                fault: None,
                through: A,
                bind: (42, "zcr_text_input_extension_v1", 1),
            },
            Case {
                name: "a global hidden without GPU access",
                setup: &[Global(A, 42, "zwp_linux_dmabuf_v1", 5)],
                fault: None,
                through: A,
                bind: (42, "zwp_linux_dmabuf_v1", 5),
            },
            Case {
                name: "a version above the advertised one",
                setup: &[Global(A, 10, "wl_seat", 3)],
                fault: None,
                through: A,
                bind: (10, "wl_seat", 4),
            },
            Case {
                // Only an output can be bound inert after its removal (see
                // `bind_crossing_an_output_removal_creates_an_inert_output`).
                // The removal of a name the registry never listed is
                // dropped.
                name: "a removed seat",
                setup: &[Global(A, 10, "wl_seat", 5), Remove(A, 99), Remove(A, 10)],
                fault: None,
                through: A,
                bind: (10, "wl_seat", 5),
            },
            Case {
                // A bind that crossed an output's removal is inert only as
                // the output it was.
                name: "a removed output bound as another interface",
                setup: &[Global(A, 10, "wl_output", 3), Remove(A, 10)],
                fault: None,
                through: A,
                bind: (10, "wl_seat", 3),
            },
            Case {
                name: "a live global the binding registry never listed",
                setup: &[Global(A, 10, "wl_output", 3), Global(B, 11, "wl_seat", 5)],
                fault: None,
                through: B,
                bind: (10, "wl_output", 3),
            },
            Case {
                // Registry B listed nothing, so only the retirement of the
                // global its removal from A completed can refuse the bind.
                name: "a retired global bound through a registry that listed nothing",
                setup: &[Global(A, 10, "wl_seat", 5), Remove(A, 10)],
                fault: None,
                through: B,
                bind: (10, "wl_seat", 5),
            },
            Case {
                // Reachable only through a proxy bookkeeping bug, not a
                // guest error: a name in `host_globals` whose advertisement
                // generation was never recorded. Registry B lists nothing,
                // so only the generation can refuse the bind.
                name: "a global without an advertisement generation",
                setup: &[],
                fault: Some(|ctx| {
                    let output = HostGlobal {
                        interface: "wl_output".to_string(),
                        version: 3,
                    };
                    ctx.host_globals.insert(10, output);
                }),
                through: B,
                bind: (10, "wl_output", 3),
            },
        ] {
            let mut r = Registries::new().await;
            for &step in case.setup {
                r.run(step).await;
            }
            r.s.guest_received();
            if let Some(fault) = case.fault {
                fault(r.s.ctx());
            }

            let (name, interface, version) = case.bind;
            let through = r.of(case.through);
            let id = r.s.new_id();
            let bind = wl_registry::req::bind(through.id, name, interface, version, id).unwrap();
            r.s.guest_fatal([bind]).await;
            assert_eq!(
                r.s.guest_received_and_error(),
                (vec![], (through.id, wl_display::ERROR_INVALID_OBJECT)),
                "{}",
                case.name
            );
            assert_eq!(r.s.host_received(), [], "{}", case.name);
        }
    }

    /// An output bound before the proxy binds `zaura_shell` gets its Aura
    /// output when the shell is bound.
    #[tokio::test]
    async fn late_zaura_shell_binding_attaches_existing_outputs() {
        let mut r = Registries::aura_placement().await;
        r.global(A, 10, "wl_output", 3).await;
        r.s.guest_received();
        // The host gets the bind alone: the Aura output waits for the
        // shell's binding.
        let output = r.bind_paired(r.a, 10, "wl_output", 3).await;

        r.global(A, 11, zaura_shell::NAME, 38).await;
        assert_eq!(
            r.s.guest_received(),
            [(r.a.id, global(11, "gtk_shell1", 1))]
        );
        let host = r.s.host_received();
        let [bind, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id, .. }))] =
            &host[..]
        else {
            panic!("late shell binding must attach every already-bound output: {host:?}");
        };
        let shell = bound(bind);
        let get_aura_output = zaura_shell::Request::GetAuraOutput {
            id: *id,
            output: output.host,
        };
        assert_eq!(
            host,
            [
                (r.a.host, bind_request(11, zaura_shell::NAME, 38, shell)),
                (shell, get_aura_output.into()),
            ]
        );
    }

    /// A toplevel created before zaura_shell is bound gets its Aura
    /// toplevel when the shell is bound.
    #[tokio::test]
    async fn late_zaura_shell_binding_attaches_existing_toplevels() {
        let mut r = Registries::aura_placement().await;
        let toplevel = r.s.toplevel().await;
        assert_eq!(r.s.host_received(), []);

        r.global(A, 11, zaura_shell::NAME, 38).await;
        assert_eq!(
            r.s.guest_received(),
            [(r.a.id, global(11, "gtk_shell1", 1))]
        );
        let host = r.s.host_received();
        let [bind, (
            _,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                id, ..
            }),
        ), _] = &host[..]
        else {
            panic!("late shell binding must attach every recorded toplevel: {host:?}");
        };
        let (shell, aura_toplevel) = (bound(bind), *id);
        let get_aura_toplevel = zaura_shell::Request::GetAuraToplevelForXdgToplevel {
            id: aura_toplevel,
            toplevel: toplevel.toplevel.host,
        };
        assert_eq!(
            host,
            [
                (r.a.host, bind_request(11, zaura_shell::NAME, 38, shell)),
                (shell, get_aura_toplevel.into()),
                (
                    aura_toplevel,
                    zaura_toplevel::Request::SetSupportsScreenCoordinates {}.into()
                ),
            ]
        );
    }

    /// The guest's `wl_shm` is emulated over the proxy's one host binding.
    #[tokio::test]
    async fn synthetic_shm_bind_does_not_create_a_second_host_object() {
        let mut r = Registries::new().await;
        r.global(A, 7, wl_shm::NAME, 1).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(7, wl_shm::NAME, 1))]);
        assert_eq!(r.s.host_received().len(), 1, "the proxy binds wl_shm");

        let shm = r.bind(r.a, 7, wl_shm::NAME, 1).await;
        assert_served_locally(&mut r.s, shm, wl_shm::NAME).await;
    }

    /// Every guest `zwp_text_input_manager_v3` is local: mapping each to
    /// the one host manager would let one's destruction unmap the others'.
    #[tokio::test]
    async fn synthetic_text_input_managers_do_not_alias_shared_host_id() {
        let mut r = Registries::new().await;
        r.global(A, 9, zwp_text_input_manager_v1::NAME, 1).await;
        let extension = zcr_text_input_extension_v1::VERSION;
        r.global(A, 10, zcr_text_input_extension_v1::NAME, extension)
            .await;
        let host = r.s.host_received();
        let [manager, _] = &host[..] else {
            panic!("the host got {host:?}, not the managers' binds");
        };
        let manager = bound(manager);
        r.s.guest_received();
        let manager_v3 = zwp_text_input_manager_v3::NAME;
        let first = r.bind(r.a, 9, manager_v3, 1).await;
        let second = r.bind(r.a, 9, manager_v3, 1).await;
        assert_eq!(r.s.host_received(), [], "the managers are local");

        r.s.guest([zwp_text_input_manager_v3::req::destroy(first)])
            .await;
        assert_eq!(r.s.host_received(), []);
        let deleted = wl_display::Event::DeleteId { id: first };
        assert_eq!(r.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        // The host manager still serves the other one.
        let seat = r.s.bind("wl_seat", 5).await;
        let text_input = r.s.new_id();
        r.s.guest([zwp_text_input_manager_v3::req::get_text_input(
            second, text_input, seat,
        )])
        .await;
        let host = r.s.host_received();
        assert!(
            matches!(
                &host[..],
                [(sender, AnyRequest::ZwpTextInputManagerV1(
                    zwp_text_input_manager_v1::Request::CreateTextInput { .. }
                )), _] if *sender == manager
            ),
            "the host got {host:?}, not a text input of its manager"
        );
    }

    #[tokio::test]
    async fn duplicate_interfaces_keep_each_global_name_bindable() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_seat", 5).await;
        r.global(A, 11, "wl_seat", 5).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(10, "wl_seat", 5)),
                (r.a.id, global(11, "wl_seat", 5)),
            ]
        );

        r.bind_paired(r.a, 10, "wl_seat", 5).await;
        r.bind_paired(r.a, 11, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn removed_text_input_globals_keep_bound_manager_usable() {
        let mut r = Registries::new().await;
        r.global(A, 9, zwp_text_input_manager_v1::NAME, 1).await;
        r.global(A, 10, zcr_text_input_extension_v1::NAME, 11).await;
        let host = r.s.host_received();
        let [manager, extension] = &host[..] else {
            panic!("the host got {host:?}, not the managers' binds");
        };
        let (manager, extension) = (bound(manager), bound(extension));
        r.s.guest_received();

        // The synthetic v3 global is advertised under the host manager's
        // numeric name. Bind a guest manager before removing the host globals.
        let manager_v3 = r.bind(r.a, 9, zwp_text_input_manager_v3::NAME, 1).await;
        r.remove(A, 9).await;
        r.remove(A, 10).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(9))]);
        assert_eq!(r.s.host_received(), []);

        // An already-bound manager remains usable after global_remove. New
        // text-input children must still be created through both retained
        // host-side factories.
        let seat = r.s.bind("wl_seat", 5).await;
        let text_input = r.s.new_id();
        r.s.guest([zwp_text_input_manager_v3::req::get_text_input(
            manager_v3, text_input, seat,
        )])
        .await;
        let host = r.s.host_received();
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

    /// The `zcr_extended_keyboard_v1` of a keyboard outlives its manager's
    /// global: the keyboard's release still destroys it.
    #[tokio::test]
    async fn keyboard_extension_global_remove_keeps_existing_children_alive() {
        let mut r = Registries::new().await;
        r.global(A, 20, zcr_keyboard_extension_v1::NAME, 2).await;
        let host = r.s.host_received();
        let [extension] = &host[..] else {
            panic!("the host got {host:?}, not the extension's bind");
        };
        let extension = bound(extension);
        let (_, keyboard) = r.s.keyboard().await;
        let surface = r.s.surface().await;
        r.s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        assert_eq!(r.s.guest_received().len(), 2, "the enter and its modifiers");
        let host = r.s.host_received();
        let [(
            sender,
            AnyRequest::ZcrKeyboardExtensionV1(
                zcr_keyboard_extension_v1::Request::GetExtendedKeyboard {
                    id: child,
                    keyboard: of,
                },
            ),
        )] = host[..]
        else {
            panic!("the keyboard got no extended keyboard: {host:?}");
        };
        assert_eq!((sender, of), (extension, keyboard.host));

        r.remove(A, 20).await;
        assert_eq!(r.s.host_received(), []);
        assert_eq!(r.s.guest_received(), []);

        r.s.guest([wl_keyboard::req::release(keyboard.id)]).await;
        assert_eq!(
            r.s.host_received(),
            [
                (keyboard.host, wl_keyboard::Request::Release {}.into()),
                (child, zcr_extended_keyboard_v1::Request::Destroy {}.into()),
            ],
            "an existing child must remain mapped after manager removal"
        );
    }

    /// Removing the global a singleton is bound to retires the binding: its
    /// host ID stays reserved, the host object is destroyed if its
    /// interface has a destructor, and the name's next advertisement is
    /// bound afresh.
    #[tokio::test]
    async fn removing_a_bound_singleton_global_retires_its_binding() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises, and the one the proxy binds.
            version: u32,
            bound_version: u32,
            /// The global the guest is advertised for it, which its removal
            /// reaches.
            advertised: Option<(&'static str, u32)>,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
            /// Whether a new binding asks for its capabilities with a sync.
            capability_sync: bool,
        }
        for Case {
            singleton,
            version,
            bound_version,
            advertised,
            destructor,
            capability_sync,
        } in [
            Case {
                singleton: Singleton::Shm,
                version: 1,
                bound_version: 1,
                advertised: Some((wl_shm::NAME, 1)),
                destructor: None,
                capability_sync: false,
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: 2,
                bound_version: 2,
                advertised: None,
                destructor: None,
                capability_sync: false,
            },
            Case {
                singleton: Singleton::AuraShell,
                version: 38,
                bound_version: 38,
                advertised: Some((gtk_shell1::NAME, 1)),
                destructor: Some(zaura_shell::Request::Release {}.into()),
                capability_sync: false,
            },
            // Removed before its capabilities arrived, the global was never
            // shown to the guest.
            Case {
                singleton: Singleton::Dmabuf,
                version: 4,
                bound_version: 3,
                advertised: None,
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
                capability_sync: true,
            },
        ] {
            let interface = singleton.interface().name();
            let mut r = Registries::gpu(true).await;
            let a = r.a;
            let advertise = || {
                Vec::from_iter(
                    advertised.map(|(advertised, version)| (a.id, global(10, advertised, version))),
                )
            };
            // The proxy binds the global as `version`, and asks for its
            // capabilities if it has some; returns the binding.
            let binding = |host: Vec<(u32, AnyRequest)>| {
                let binding = bound(&host[0]);
                let expected = bind_request(10, interface, bound_version, binding);
                let sync = capability_sync.then(|| {
                    let callback = host[1].1.new_object().expect("a callback").0;
                    sync(callback)
                });
                assert_eq!(
                    host,
                    Vec::from_iter([(a.host, expected)].into_iter().chain(sync)),
                    "{interface}"
                );
                binding
            };
            r.global(A, 10, interface, version).await;
            let old = binding(r.s.host_received());
            assert_eq!(r.s.guest_received(), advertise(), "{interface}");

            r.remove(A, 10).await;
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(destructor.map(|request| (old, request))),
                "{interface}"
            );
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(advertised.map(|_| (a.id, global_remove(10)))),
                "{interface}"
            );
            // Reads state no peer can observe: the guest global that waited
            // for the binding's capabilities, which no registry lists now.
            assert!(r.s.ctx().pending_dmabuf_globals.is_empty(), "{interface}");

            r.global(A, 10, interface, version).await;
            let new = binding(r.s.host_received());
            assert_ne!(new, old, "{interface}");
            assert_eq!(r.s.guest_received(), advertise(), "{interface}");
        }
    }

    /// Removing the `zaura_shell` global releases the proxy's binding if
    /// its version has a destructor, and keeps the `zaura_surface`s made
    /// from it until their surfaces are destroyed.
    #[tokio::test]
    async fn global_remove_keeps_aura_children_until_surface_destroy() {
        for (version, released) in [(38, true), (37, false)] {
            let mut r = Registries::new().await;
            r.global(A, 10, zaura_shell::NAME, version).await;
            let host = r.s.host_received();
            let [shell] = &host[..] else {
                panic!("the host got {host:?}, not the shell's bind");
            };
            let shell = bound(shell);
            r.s.guest_received();
            let gtk_shell = r.bind(r.a, 10, gtk_shell1::NAME, 1).await;
            assert_served_locally(&mut r.s, gtk_shell, gtk_shell1::NAME).await;
            let surface = r.s.surface().await;
            let gtk_surface = r.s.new_id();
            r.s.guest([
                gtk_shell1::req::set_startup_id(gtk_shell, Some("launch")).unwrap(),
                gtk_shell1::req::get_gtk_surface(gtk_shell, gtk_surface, surface.id),
            ])
            .await;
            let host = r.s.host_received();
            let [(_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), _] =
                host[..]
            else {
                panic!("the surface got no Aura surface: {host:?}");
            };
            let aura_surface = id;
            let get_aura_surface = zaura_shell::Request::GetAuraSurface {
                id: aura_surface,
                surface: surface.host,
            };
            let startup_id = zaura_surface::Request::SetStartupId {
                startup_id: Some("launch".to_string()),
            };
            assert_eq!(
                host,
                [
                    (shell, get_aura_surface.into()),
                    (aura_surface, startup_id.into())
                ]
            );

            r.remove(A, 10).await;
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(released.then(|| (shell, zaura_shell::Request::Release {}.into()))),
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), [(r.a.id, global_remove(10))]);

            // The Aura surface outlives the global: GTK still names the
            // application on it.
            let application = Some("com.example.Terminal");
            r.s.guest([gtk_surface1::req::set_dbus_properties(
                gtk_surface,
                application,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()])
                .await;
            let host = r.s.host_received();
            assert!(
                matches!(
                    &host[..],
                    [(sender, AnyRequest::ZauraSurface(
                        zaura_surface::Request::SetApplicationId { .. }
                    ))] if *sender == aura_surface
                ),
                "the host got {host:?}, not the application of the Aura surface"
            );

            // Until its surface is destroyed: an Aura surface of v38
            // releases with it.
            r.s.guest([wl_surface::req::destroy(surface.id)]).await;
            let release =
                (released).then(|| (aura_surface, zaura_surface::Request::Release {}.into()));
            assert_eq!(
                r.s.host_received(),
                Vec::from_iter(
                    release
                        .into_iter()
                        .chain([(surface.host, wl_surface::Request::Destroy {}.into())])
                ),
                "v{version}"
            );
            assert_eq!(r.s.guest_received(), []);
        }
    }

    /// A host may advertise a singleton global under a new name before it
    /// removes the old one. The proxy keeps its binding of the old name,
    /// and binds the new one when the old one retires.
    #[tokio::test]
    async fn a_singleton_advertised_anew_is_bound_when_the_old_name_retires() {
        struct Case {
            singleton: Singleton,
            /// The version the host advertises both globals at, and the
            /// one the proxy binds.
            version: u32,
            bound_version: u32,
            /// The global the guest is advertised for each, which its
            /// removal reaches.
            advertised: Option<(&'static str, u32)>,
            /// The request that destroys the old binding, if its interface
            /// has one.
            destructor: Option<AnyRequest>,
        }
        for Case {
            singleton,
            version,
            bound_version,
            advertised,
            destructor,
        } in [
            Case {
                singleton: Singleton::Dmabuf,
                version: LINUX_DMABUF_CAPABILITY_VERSION,
                bound_version: 3,
                advertised: Some((zwp_linux_dmabuf_v1::NAME, 3)),
                destructor: Some(zwp_linux_dmabuf_v1::Request::Destroy {}.into()),
            },
            Case {
                singleton: Singleton::KeyboardExtension,
                version: KEYBOARD_EXTENSION_VERSION,
                bound_version: 2,
                advertised: None,
                destructor: None,
            },
            // Before v38 `zaura_shell` has no destructor.
            Case {
                singleton: Singleton::AuraShell,
                version: ZAURA_SHELL_MIN_VERSION,
                bound_version: 6,
                advertised: Some((gtk_shell1::NAME, 1)),
                destructor: None,
            },
            Case {
                singleton: Singleton::Shm,
                version: WL_SHM_VERSION,
                bound_version: 1,
                advertised: Some((wl_shm::NAME, 1)),
                destructor: None,
            },
        ] {
            let mut r = Registries::gpu(false).await;
            let interface = singleton.interface().name();
            let a = r.a;
            let advertise = |name| {
                Vec::from_iter(
                    advertised
                        .map(|(advertised, version)| (a.id, global(name, advertised, version))),
                )
            };
            r.global(A, 10, interface, version).await;
            assert_eq!(r.s.guest_received(), advertise(10), "{interface}");
            let host = r.s.host_received();
            let [old] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let old = bound(old);
            assert_eq!(
                host,
                [(a.host, bind_request(10, interface, bound_version, old))],
                "{interface} is bound"
            );
            r.global(A, 11, interface, version).await;
            assert_eq!(r.s.guest_received(), advertise(11), "{interface}");
            assert_eq!(r.s.host_received(), [], "{interface} is bound once");

            r.remove(A, 10).await;

            let host = r.s.host_received();
            let new = bound(host.last().expect("a new binding"));
            let expected = destructor.map(|request| (old, request)).into_iter();
            assert_eq!(
                host,
                Vec::from_iter(
                    expected.chain([(a.host, bind_request(11, interface, bound_version, new))])
                ),
                "{interface}"
            );
            assert_eq!(
                r.s.guest_received(),
                Vec::from_iter(advertised.map(|_| (a.id, global_remove(10)))),
                "{interface}"
            );
        }
    }

    /// When the proxy's `wl_shm` binding retires, it binds the lowest name
    /// (possibly the same name at a new generation) that a registry still
    /// lists and whose removal no registry has seen, through the lowest
    /// registry that lists it. A registry lists a name only at the
    /// generation it last saw advertised.
    #[tokio::test]
    async fn a_singleton_binding_moves_to_the_lowest_live_candidate() {
        struct Case {
            name: &'static str,
            /// The `wl_shm` names each registry lists, in the order the
            /// host advertised them. The proxy binds the first.
            listings: &'static [(Registry, &'static [u32])],
            steps: &'static [Step],
            /// The name the proxy has bound after the steps.
            bound: Option<u32>,
            /// The `wl_shm` binds the steps sent, through each registry, of
            /// each name.
            binds: &'static [(Registry, u32)],
            candidates: &'static [u32],
        }
        for case in [
            Case {
                name: "the lowest candidate is bound through the lowest registry",
                listings: &[(B, &[10, 12, 11]), (A, &[10, 12, 11])],
                steps: &[Remove(B, 10), Remove(A, 10)],
                bound: Some(11),
                binds: &[(A, 11)],
                candidates: &[11, 12],
            },
            Case {
                // Before every registry has seen the bound name removed, one
                // that has may see it advertised for another interface.
                name: "a bound name reused by another interface hands the binding on",
                listings: &[(A, &[10, 11]), (B, &[10])],
                steps: &[Remove(A, 10), Global(A, 10, "wl_output", 3)],
                bound: Some(11),
                binds: &[(A, 11)],
                candidates: &[11],
            },
            Case {
                name: "a candidate name reused by another interface is no candidate",
                listings: &[(A, &[10, 11]), (B, &[10, 11])],
                steps: &[
                    Remove(A, 11),
                    Global(A, 11, "wl_output", 3),
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
                steps: &[Remove(B, 10), Global(B, 10, "wl_shm", 1)],
                bound: Some(10),
                binds: &[(B, 10)],
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
                steps: &[Remove(B, 10), Global(B, 10, "wl_shm", 1), Remove(B, 10)],
                bound: None,
                binds: &[(B, 10)],
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
                    Global(B, 10, "wl_shm", 1),
                    Global(A, 10, "wl_shm", 1),
                    Remove(B, 10),
                ],
                bound: Some(10),
                binds: &[(B, 10)],
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
                    Global(A, 10, "wl_shm", 1),
                    Remove(B, 10),
                    Global(B, 10, "wl_shm", 1),
                    Remove(B, 10),
                ],
                bound: Some(10),
                binds: &[(A, 10)],
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
                binds: &[(A, 11)],
                candidates: &[11],
            },
        ] {
            let mut r = Registries::new().await;
            let mut host = Vec::new();
            let listings = case.listings.iter().flat_map(|&(registry, names)| {
                names
                    .iter()
                    .map(move |&name| Global(registry, name, "wl_shm", 1))
            });
            for step in listings.chain(case.steps.iter().copied()) {
                r.s.guest_received();
                host.extend(r.run(step).await);
            }
            r.s.guest_received();

            // The first name listed is bound through its registry, then
            // each the steps bind.
            let (first, names) = case.listings[0];
            let binds = [(first, names[0])]
                .into_iter()
                .chain(case.binds.iter().copied());
            let expected = binds.zip(&host).map(|((registry, name), request)| {
                let bind = bind_request(name, "wl_shm", 1, bound(request));
                (r.of(registry).host, bind)
            });
            assert_eq!(host, Vec::from_iter(expected), "{}", case.name);
            // Reads state no peer can observe: the names a later
            // retirement could bind.
            let mut candidates = Vec::from_iter(r.s.ctx().singleton_candidates.keys().copied());
            candidates.sort_unstable();
            assert_eq!(candidates, case.candidates, "{}", case.name);

            // A new name is bound at once only if no other is.
            r.global(A, 99, "wl_shm", 1).await;
            r.s.guest_received();
            let host = r.s.host_received();
            assert_eq!(host.is_empty(), case.bound.is_some(), "{}", case.name);
            if case.bound.is_none() {
                let expected = bind_request(99, "wl_shm", 1, bound(&host[0]));
                assert_eq!(host, [(r.a.host, expected)], "{}", case.name);
            }
            if let Some(bound) = case.bound {
                let last = case.binds.last().map_or(names[0], |&(_, name)| name);
                assert_eq!(last, bound, "{}", case.name);
            }
        }
    }

    /// The guest global of a replacement linux-dmabuf waits, like any, for
    /// the capabilities of its own binding.
    #[tokio::test]
    async fn a_promoted_dmabuf_publishes_its_deferred_guest_global() {
        let mut r = Registries::gpu(true).await;
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        r.global(A, 10, dmabuf, 4).await;
        let (old, _) = dmabuf_binding(&r.s.host_received(), r.a, 10);
        r.global(A, 11, dmabuf, 4).await;
        assert_eq!(r.s.host_received(), []);

        r.remove(A, 10).await;
        let host = r.s.host_received();
        let destroy = zwp_linux_dmabuf_v1::Request::Destroy {};
        assert_eq!(host[0], (old, destroy.into()));
        let (new, callback) = dmabuf_binding(&host[1..], r.a, 11);
        assert_eq!(r.s.guest_received(), []);

        r.s.host(capabilities(new, callback, &[XRGB8888_LINEAR]))
            .await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(11, dmabuf, 4))]);
    }

    /// libwayland-server lets a bind cross its global's removal on the
    /// wire, as when a dock flaps and the client binds the output it saw
    /// advertised in the same dispatch. Such an output becomes an inert
    /// guest object, of the version bound, whose release completes in the
    /// proxy. A name never advertised to the binding registry stays a
    /// protocol error.
    #[tokio::test]
    async fn bind_crossing_an_output_removal_creates_an_inert_output() {
        let mut r = Registries::new().await;
        r.global(A, 10, "wl_output", 3).await;
        r.remove(A, 10).await;
        r.global(B, 11, "wl_seat", 5).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(10, "wl_output", 3)),
                (r.a.id, global_remove(10)),
                (r.b.id, global(11, "wl_seat", 5)),
            ]
        );

        let output = r.bind(r.a, 10, "wl_output", 3).await;
        assert_eq!(r.s.host_received(), [], "nothing reaches the host");
        r.s.guest([wl_output::req::release(output)]).await;
        assert_eq!(r.s.host_received(), []);
        let deleted = wl_display::Event::DeleteId { id: output };
        assert_eq!(r.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

        // Name 10 is retired, and registry B never saw it.
        r.bind_fatal(r.b, 10, "wl_output", 3).await;
    }

    /// Registries A and B list global 7, a `wl_seat`, whose removal only
    /// registry A has seen.
    async fn a_global_removed_from_one_of_two_registries() -> Registries {
        let mut r = Registries::new().await;
        for registry in [A, B] {
            r.global(registry, 7, "wl_seat", 5).await;
        }
        r.remove(A, 7).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(7, "wl_seat", 5)),
                (r.b.id, global(7, "wl_seat", 5)),
                (r.a.id, global_remove(7)),
            ]
        );
        r
    }

    /// The removal of a global the proxy neither advertises nor binds is
    /// on record too, and waits for every registry that lists it. The
    /// session's check that no state outlives its object pins that the
    /// proxy drops the record once no registry lists the global.
    #[tokio::test]
    async fn the_removal_of_a_dropped_global_waits_for_every_registry() {
        let mut r = Registries::new().await;
        for registry in [A, B] {
            r.global(registry, 7, "zz_unknown_v1", 1).await;
        }
        r.remove(A, 7).await;
        assert_eq!(r.s.guest_received(), []);
        // Reads state no peer can observe: the removal of a global the
        // proxy uses for nothing.
        assert!(r.s.ctx().removed_global_generations.contains_key(&7));

        assert_eq!(r.destroy(B).await, []);
    }

    /// O3: a global removed from one registry waits for every registry that
    /// still lists it. Destroying the last of those through `wl_fixes`
    /// means it never sees the removal, so the global is retired then: a
    /// registry that listed nothing can no longer bind it.
    #[tokio::test]
    async fn destroying_the_last_registry_to_list_a_removed_global_retires_it() {
        let mut r = a_global_removed_from_one_of_two_registries().await;

        assert_eq!(r.destroy(B).await, []);

        let c = r.s.new_registry().await;
        r.bind_fatal(c, 7, "wl_seat", 5).await;
    }

    /// The removal stays on record when the only registry that saw it is
    /// destroyed first, so destroying the last one still retires the
    /// global.
    #[tokio::test]
    async fn a_removal_outlives_the_registry_that_saw_it() {
        let mut r = a_global_removed_from_one_of_two_registries().await;

        assert_eq!(r.destroy(A).await, []);
        // Registry B lists it, so its bind still reaches the host.
        r.bind_paired(r.b, 7, "wl_seat", 5).await;
        assert_eq!(r.destroy(B).await, []);

        let c = r.s.new_registry().await;
        r.bind_fatal(c, 7, "wl_seat", 5).await;
    }

    /// A registry destroyed while a global is still advertised leaves it
    /// to the other registries.
    #[tokio::test]
    async fn destroying_a_registry_keeps_a_global_no_registry_saw_removed() {
        let mut r = Registries::new().await;
        r.global(A, 7, "wl_seat", 5).await;
        r.s.guest_received();

        assert_eq!(r.destroy(A).await, []);

        // Registry B, which has listed nothing yet, may still bind it.
        r.bind_paired(r.b, 7, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn stale_remove_cannot_invalidate_a_re_advertised_global_generation() {
        let mut r = Registries::new().await;
        r.global(A, 7, "wl_seat", 5).await;
        r.global(B, 7, "wl_seat", 5).await;

        // Registry A observes removal and receives a replacement with the
        // same numeric name before registry B delivers the old remove.
        r.remove(A, 7).await;
        r.global(A, 7, "wl_seat", 5).await;

        // B's delayed remove belongs to generation 1. It must be forwarded to
        // B, but must not retire generation 2 advertised to A.
        r.remove(B, 7).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(7, "wl_seat", 5)),
                (r.b.id, global(7, "wl_seat", 5)),
                (r.a.id, global_remove(7)),
                (r.a.id, global(7, "wl_seat", 5)),
                (r.b.id, global_remove(7)),
            ]
        );

        // An old registry remove must not invalidate the replacement.
        r.bind_paired(r.a, 7, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn re_advertised_name_replaces_visible_metadata_before_hidden_global() {
        let mut r = Registries::new().await;
        let keyboard_extension = zcr_keyboard_extension_v1::NAME;
        r.global(A, 7, "wl_seat", 5).await;
        r.remove(A, 7).await;

        // The host reused name 7 for a hidden singleton. The old visible
        // wl_seat metadata must not survive and authorize a stale bind.
        r.global(A, 7, keyboard_extension, 2).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(7, "wl_seat", 5)),
                (r.a.id, global_remove(7))
            ]
        );
        let host = r.s.host_received();
        let [binding] = &host[..] else {
            panic!("the host got {host:?}, not one bind");
        };
        let binding = bound(binding);
        assert_eq!(
            host,
            [(r.a.host, bind_request(7, keyboard_extension, 2, binding))],
            "the replacement hidden global must own the numeric name"
        );

        r.bind_fatal(r.a, 7, "wl_seat", 5).await;
    }

    #[tokio::test]
    async fn re_advertised_hidden_name_replaces_hidden_metadata_before_visible_global() {
        let mut r = Registries::new().await;
        r.global(A, 7, zcr_keyboard_extension_v1::NAME, 2).await;
        assert_eq!(r.s.host_received().len(), 1, "the proxy binds it");
        r.remove(A, 7).await;
        assert_eq!(r.s.guest_received(), []);

        // Name 7 is now a normal visible global. A stale hidden marker would
        // make its later global_remove disappear instead of reaching the guest.
        r.global(A, 7, "wl_seat", 5).await;
        assert_eq!(r.s.guest_received(), [(r.a.id, global(7, "wl_seat", 5))]);
        r.bind_paired(r.a, 7, "wl_seat", 5).await;
        r.remove(A, 7).await;
        assert_eq!(
            r.s.guest_received(),
            [(r.a.id, global_remove(7))],
            "the visible replacement must forward its own removal"
        );
    }

    #[tokio::test]
    async fn delayed_remove_does_not_reset_replacement_internal_binding() {
        let mut r = Registries::new().await;
        let keyboard_extension = zcr_keyboard_extension_v1::NAME;
        for registry in [A, B] {
            r.global(registry, 7, keyboard_extension, 2).await;
        }
        let host = r.s.host_received();
        let [old] = &host[..] else {
            panic!("the host got {host:?}, not one bind");
        };
        let old = bound(old);

        // Registry A sees the host generation disappear and immediately sees
        // the replacement. The old internal object is retired and a fresh
        // binding is created for the new generation.
        r.remove(A, 7).await;
        r.global(A, 7, keyboard_extension, 2).await;
        let host = r.s.host_received();
        let [replacement] = &host[..] else {
            panic!("the host got {host:?}, not one bind");
        };
        let replacement = bound(replacement);
        assert_ne!(replacement, old);
        assert_eq!(
            host,
            [(
                r.a.host,
                bind_request(7, keyboard_extension, 2, replacement)
            )]
        );

        // Registry B's queued remove belongs to generation 1. It must not
        // tear down the generation-2 internal object that is now active:
        // a keyboard's first enter binds its extended keyboard from it.
        r.remove(B, 7).await;
        assert_eq!(r.s.host_received(), []);
        assert_eq!(r.s.guest_received(), []);
        let (_, keyboard) = r.s.keyboard().await;
        let surface = r.s.surface().await;
        r.s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        assert_eq!(r.s.guest_received().len(), 2, "the enter and its modifiers");
        let host = r.s.host_received();
        assert!(
            matches!(
                &host[..],
                [(sender, AnyRequest::ZcrKeyboardExtensionV1(
                    zcr_keyboard_extension_v1::Request::GetExtendedKeyboard { .. }
                ))] if *sender == replacement
            ),
            "the host got {host:?}, not an extended keyboard of the replacement"
        );
    }

    #[tokio::test]
    async fn compositor_guest_version_and_host_bind_are_negotiated_separately() {
        let compositor = "wl_compositor";
        // A global the guest may take as the host offers it is forwarded;
        // the proxy advertises any other at the guest's version itself.
        for (host_version, guest_version, bound_version) in
            [(1, 1, 1), (2, 2, 2), (3, 4, 3), (4, 4, 3), (6, 4, 3)]
        {
            let mut r = Registries::new().await;
            r.global(A, 7, compositor, host_version).await;
            assert_eq!(
                r.s.guest_received(),
                [(r.a.id, global(7, compositor, guest_version))],
                "v{host_version}"
            );

            r.bind(r.a, 7, compositor, guest_version).await;
            let host = r.s.host_received();
            let [binding] = &host[..] else {
                panic!("the host got {host:?}, not one bind");
            };
            let binding = bound(binding);
            assert_eq!(
                host,
                [(
                    r.a.host,
                    bind_request(7, compositor, bound_version, binding)
                )],
                "v{host_version}"
            );

            r.bind_fatal(r.a, 7, compositor, guest_version + 1).await;
        }
    }

    /// The version the guest is offered of a forwarded host `interface`
    /// at `host_version`.
    fn offered(interface: &str, host_version: u32) -> u32 {
        let iface = Iface::from_name(interface).expect("a vendored interface");
        let Policy::Forward { ceiling, .. } = policy(iface) else {
            panic!("{interface} is not forwarded");
        };
        offered_version(iface, ceiling, host_version)
    }

    /// Every interface has one row in the table of globals (`policy`,
    /// whose `match` the compiler holds to every interface): a forwarded
    /// global is never offered above its XML, and each singleton is bound
    /// from a global of its own interface. The singletons the table binds
    /// are those `Singleton::ALL` lists, which the proxy binds anew when
    /// one retires: a variant that is in neither is never constructed,
    /// which the compiler rejects.
    #[test]
    fn every_interface_has_one_global_policy() {
        for iface in Iface::ALL {
            match policy(iface) {
                Policy::Forward { ceiling, .. } => assert!(
                    ceiling <= iface.version(),
                    "{iface} is offered up to v{ceiling}, newer than its XML's v{}",
                    iface.version()
                ),
                Policy::BindInternal(Internal::Singleton(singleton)) => {
                    assert_eq!(singleton.interface(), iface, "{singleton:?}");
                    assert!(
                        Singleton::ALL.contains(&singleton),
                        "{singleton:?} is in Singleton::ALL"
                    );
                }
                Policy::BindInternal(_) | Policy::Hide | Policy::Synthetic => {}
            }
        }
        for singleton in Singleton::ALL {
            assert!(
                matches!(
                    policy(singleton.interface()),
                    Policy::BindInternal(Internal::Singleton(bound)) if bound == singleton
                ),
                "{singleton:?} has its row"
            );
        }
        assert_eq!(offered("wl_seat", u32::MAX), super::WL_SEAT_VERSION);
    }

    #[tokio::test]
    async fn ordinary_globals_are_capped_to_supported_versions() {
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
                super::tests::offered(interface, host_version),
                offered,
                "{interface} v{host_version}"
            );
        }

        let mut r = Registries::new().await;
        r.global(A, 9, "zxdg_output_manager_v1", 3).await;
        r.global(A, 10, "wl_seat", 10).await;
        assert_eq!(
            r.s.guest_received(),
            [
                (r.a.id, global(9, "zxdg_output_manager_v1", 3)),
                (r.a.id, global(10, "wl_seat", 5)),
            ],
            "the guest must receive the capped version"
        );
        r.bind_paired(r.a, 10, "wl_seat", 5).await;
    }
}
