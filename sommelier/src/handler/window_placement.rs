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

//! Window-placement mechanics: the Aura objects, requests, and configure
//! ordering behind compositor-owned placement shortcuts.
//!
//! The keyboard handler decides that a chord is a placement shortcut; this
//! module moves the focused window. It owns the Aura toplevel that carries
//! `zaura_toplevel.set_window_bounds`, the surface's ARC policy identity
//! (see `sommelier/docs/ARC_TASK_AND_SESSION_IDS.md`), the host-stream
//! barrier that orders each bounds request, the configures held behind that
//! barrier, and the translation of `zaura_toplevel.configure` into the
//! guest's `xdg_toplevel.configure`. Its per-connection state lives in
//! [`crate::state::WindowPlacementState`]; protocol handlers translate wire
//! events and delegate here, one call per event:
//!
//! | Caller | Entry point |
//! | :--- | :--- |
//! | consumed placement chord (`wl_keyboard.key`) | [`place_focused`] |
//! | `xdg_surface.get_toplevel`, after dispatch | [`attach_toplevel`] |
//! | `zaura_shell` bound late | [`attach_recorded_toplevels`] |
//! | XDG role teardown | [`release_toplevel`] |
//! | XDG and GTK application IDs | [`set_aura_application_id`] |
//! | `wl_callback.done` | [`complete_barrier`] |
//! | `xdg_surface.configure` | [`hold_surface_configure`] |
//! | `xdg_toplevel.configure` | [`hold_toplevel_configure`] |
//! | `zaura_toplevel.configure` | [`translate_aura_configure`] |
//!
//! The host Aura surface is shared ([`crate::state::AuraSurfaces`]), and
//! `crate::handler::aura_objects` creates and releases it and the placement
//! Aura toplevel.

use crate::handler::aura_objects::{
    self, aura_object_version, ensure_host_zaura_surface, AuraChild,
};
use crate::handler::callback::queue_internal_sync;
use crate::protocols::aura_shell::zaura_output::EVT_INSETS_SINCE;
use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE;
use crate::protocols::aura_shell::zaura_surface::{self, REQ_UNSET_SNAP_SINCE};
use crate::protocols::aura_shell::zaura_toplevel::{
    self, REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE, REQ_SET_WINDOW_BOUNDS_SINCE,
};
use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel};
use crate::state::{Context, HostId, InternalCallback};
use crate::window_layout::{Rect, WindowLayoutAction};
use crate::wire::queue_or_drop;
use std::sync::atomic::{AtomicBool, Ordering};

/// The `zaura_shell` version placement needs.
///
/// Placement creates an Aura toplevel only to send
/// `set_supports_screen_coordinates` and `set_window_bounds`, and it
/// measures the work area with `zaura_output.insets`, without which the
/// work area would include the shelf. Every Aura child takes its shell's
/// version, so placement requires the newest of the messages it uses; the
/// assertion below keeps that true if the vendored protocol changes.
const PLACEMENT_SHELL_SINCE: u32 = EVT_INSETS_SINCE;
const _: () = assert!(
    REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_WINDOW_BOUNDS_SINCE <= PLACEMENT_SHELL_SINCE
);

/// Whether [`warn_if_shell_cannot_place`] has warned. Every client
/// connection binds the same host shell, so one warning per process says
/// it all.
static OLD_SHELL_WARNED: AtomicBool = AtomicBool::new(false);

/// Warn, once per process, that placement is disabled because the host
/// `zaura_shell` is older than [`PLACEMENT_SHELL_SINCE`]. The registry calls
/// this when it binds the shell while the ARC bounds policy is active.
///
/// # Returns
///
/// Whether this call logged the warning.
pub(crate) fn warn_if_shell_cannot_place(shell_version: u32) -> bool {
    if shell_version >= PLACEMENT_SHELL_SINCE || OLD_SHELL_WARNED.swap(true, Ordering::Relaxed) {
        return false;
    }
    log::warn!(
        "Window placement is disabled: zaura_shell v{} lacks zaura_output.insets (v{}), \
         so the work area would include the shelf",
        shell_version,
        PLACEMENT_SHELL_SINCE
    );
    true
}

// ── Placing the focused window ────────────────────────────────────────────

/// Why a consumed placement chord did not move the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementSkip {
    /// The keyboard focus is not on an xdg_toplevel.
    NoFocusedToplevel,
    /// No output with complete metadata can host the surface (see
    /// `OutputRegistry::for_surface`), including when the client never
    /// bound a `wl_output`.
    NoUsableOutput,
    /// The rectangle has an empty extent, an edge that rounds onto the work
    /// area border, or overflowing coordinates on this output.
    RectNotRepresentable,
    /// The host Aura toplevel cannot be attached: no ARC policy identity
    /// could be allocated, or zaura_shell is missing or too old.
    NoAuraToplevel,
    /// The host Aura surface is unavailable (no zaura_shell).
    NoAuraSurface,
}

impl PlacementSkip {
    /// Log level for a skip. A rect the output cannot represent, or an Aura
    /// toplevel that cannot be attached (old or missing zaura_shell, or ARC
    /// identities exhausted), persists until the configuration or the proxy
    /// changes, so it is logged at info. Other skips depend on transient
    /// focus or output state and stay at debug.
    pub(crate) fn log_level(self) -> log::Level {
        match self {
            Self::RectNotRepresentable | Self::NoAuraToplevel => log::Level::Info,
            Self::NoFocusedToplevel | Self::NoUsableOutput | Self::NoAuraSurface => {
                log::Level::Debug
            }
        }
    }
}

/// The xdg_toplevel a keyboard focuses, with the IDs placement needs.
struct FocusedToplevel {
    xdg_toplevel_guest_id: u32,
    xdg_toplevel_host_id: u32,
    wl_surface_guest_id: u32,
}

/// Resolve the xdg_toplevel that `host_keyboard_id` focuses, if any.
fn focused_toplevel(ctx: &Context, host_keyboard_id: HostId) -> Option<FocusedToplevel> {
    let guest_keyboard_id = ctx.shadow_table.get_guest_id(host_keyboard_id.0)?;
    let guest_seat_id = *ctx.keyboard_to_seat.get(&guest_keyboard_id)?;
    let wl_surface_guest_id = ctx.keyboard_focus.surface_for_seat(guest_seat_id)?;
    if !ctx
        .keyboard_focus
        .keyboard_owns_surface(host_keyboard_id, wl_surface_guest_id)
    {
        return None;
    }
    let xdg_toplevel_guest_id = ctx.xdg_roles.toplevel_on_wl_surface(wl_surface_guest_id)?;
    // `record_xdg_toplevel` records only mapped toplevels, and every path
    // that unmaps one removes its record first, so the lookup cannot miss.
    let xdg_toplevel_host_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id);
    debug_assert!(
        xdg_toplevel_host_id.is_some(),
        "recorded xdg_toplevel {xdg_toplevel_guest_id} has no host object"
    );
    Some(FocusedToplevel {
        xdg_toplevel_guest_id,
        xdg_toplevel_host_id: xdg_toplevel_host_id?,
        wl_surface_guest_id,
    })
}

/// Leave fullscreen, maximized and snapped state before new bounds apply.
///
/// The Aura surface may predate `unset_snap`: it keeps the version of the
/// `zaura_shell` generation that created it, even after a newer shell is
/// bound. Such a surface cannot be snapped by this proxy, and sending it
/// the request would be a protocol error, so it is skipped.
fn clear_window_state(ctx: &mut Context, host_xdg_toplevel_id: u32, zaura_surface_id: u32) {
    ctx.client_to_host_queue.extend([
        xdg_toplevel::req::unset_fullscreen(host_xdg_toplevel_id),
        xdg_toplevel::req::unset_maximized(host_xdg_toplevel_id),
    ]);
    if aura_object_version(ctx, zaura_surface_id) >= REQ_UNSET_SNAP_SINCE {
        ctx.client_to_host_queue
            .push(zaura_surface::req::unset_snap(zaura_surface_id));
    }
}

/// Resolve the screen bounds for a placement action on an output.
///
/// The action's normalized rectangle is mapped into the output work area
/// by [`crate::window_layout::NormalizedRect::to_work_area_bounds`], which
/// owns the rounding rule. Returns `None` when the output has no usable
/// work area or the rectangle cannot be represented in it.
fn bounds_for_action(
    action: WindowLayoutAction,
    output: crate::state::OutputState,
) -> Option<Rect> {
    action.rect().to_work_area_bounds(output.work_area()?)
}

/// Place the focused toplevel for one consumed placement chord.
///
/// Resolves the toplevel that `host_keyboard_id` focuses and the output it
/// has entered, maps `action` into that output's work area, and queues the
/// state reset, `zaura_toplevel.set_window_bounds`, and the barrier that
/// holds configures until the host has applied the bounds.
///
/// # Arguments
///
/// * `host_keyboard_id` - the keyboard whose chord was consumed.
/// * `action` - the placement bound to the chord.
///
/// # Errors
///
/// A [`PlacementSkip`] names why the window was not moved. The chord stays
/// consumed either way (see `KeyboardHandler::on_key`); a skip queues no
/// placement request.
pub(crate) fn place_focused(
    ctx: &mut Context,
    host_keyboard_id: HostId,
    action: WindowLayoutAction,
) -> Result<(), PlacementSkip> {
    debug_assert!(
        ctx.window_placement.uses_arc_bounds(),
        "classify_key yields placement actions only while the ARC bounds policy is active"
    );
    let FocusedToplevel {
        xdg_toplevel_guest_id: guest_xdg_toplevel_id,
        xdg_toplevel_host_id: host_xdg_toplevel_id,
        wl_surface_guest_id: guest_wl_surface_id,
    } = focused_toplevel(ctx, host_keyboard_id).ok_or(PlacementSkip::NoFocusedToplevel)?;
    let (output_host_id, output) = ctx
        .shadow_table
        .get_host_id(guest_wl_surface_id)
        .and_then(|host_surface_id| ctx.outputs.for_surface(host_surface_id))
        .ok_or(PlacementSkip::NoUsableOutput)?;
    let bounds = bounds_for_action(action, output).ok_or(PlacementSkip::RectNotRepresentable)?;
    let zaura_toplevel_id =
        attach_placement_toplevel(ctx, guest_xdg_toplevel_id, guest_wl_surface_id)
            .ok_or(PlacementSkip::NoAuraToplevel)?;
    let zaura_surface_id =
        ensure_host_zaura_surface(ctx, guest_wl_surface_id).ok_or(PlacementSkip::NoAuraSurface)?;
    // Every check that can skip placement has run, so a skip never leaves
    // part of the batch below in the host stream. Setup requests queued
    // above (the Aura objects) stay valid for the next attempt.
    //
    // `attach_placement_toplevel` allocated the identity, and every Aura
    // surface supports `set_application_id` (the registry binds no older
    // `zaura_shell`), so applying it cannot fail here.
    let identity_applied = apply_arc_identity(ctx, guest_wl_surface_id, zaura_surface_id);
    debug_assert!(
        identity_applied,
        "an attached placement toplevel's surface always takes its ARC identity"
    );

    clear_window_state(ctx, host_xdg_toplevel_id, zaura_surface_id);
    ctx.client_to_host_queue
        .push(zaura_toplevel::req::set_window_bounds(
            zaura_toplevel_id,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            output_host_id,
        ));
    queue_window_bounds_barrier(ctx, zaura_toplevel_id);
    log::info!(
        "window layout {:?}: xdg_toplevel={} zaura_toplevel={} bounds=({}, {}, {}, {}) output={}",
        action,
        guest_xdg_toplevel_id,
        zaura_toplevel_id,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        output_host_id
    );
    Ok(())
}

// ── ARC policy identity and the Aura toplevel ─────────────────────────────

/// Send the application ID the guest set (through XDG or GTK) to the
/// surface's host Aura surface, creating that surface if needed. Nothing is
/// sent while no `zaura_shell` is bound.
///
/// `zaura_surface.set_application_id` is also what Exo checks before it
/// accepts `zaura_toplevel.set_window_bounds`. While the ARC bounds policy
/// is active, the surface's ARC identity therefore replaces the guest's ID
/// (see [`apply_arc_identity`]); otherwise the guest's ID is sent in the
/// Guest OS namespace. The Aura surface needs no version check: the
/// registry binds no `zaura_shell` older than `set_application_id`.
///
/// # Arguments
///
/// * `native_application_id` - the guest's ID in the Guest OS namespace
///   (`compositor::native_wayland_app_id`). An ID too long for one message
///   is dropped with a warning.
pub(crate) fn set_aura_application_id(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
    native_application_id: &str,
) {
    let Some(zaura_surface_host_id) = ensure_host_zaura_surface(ctx, wl_surface_guest_id) else {
        return;
    };
    if ctx.window_placement.uses_arc_bounds() {
        log::debug!(
            "Not sending application ID {} to zaura_surface (host_id={}): the ARC bounds policy replaces it",
            native_application_id,
            zaura_surface_host_id
        );
        if !apply_arc_identity(ctx, wl_surface_guest_id, zaura_surface_host_id) {
            log::warn!(
                "Unable to apply ARC policy identity for wl_surface {}",
                wl_surface_guest_id
            );
        }
        return;
    }
    if queue_or_drop(
        &mut ctx.client_to_host_queue,
        zaura_surface::req::set_application_id(zaura_surface_host_id, Some(native_application_id)),
        format_args!("the application ID of zaura_surface {zaura_surface_host_id}"),
    ) {
        log::debug!(
            "Set application ID to {} on zaura_surface (host_id={})",
            native_application_id,
            zaura_surface_host_id
        );
    }
}

/// Queue the surface's ARC policy identity to its host Aura surface, once.
///
/// While the ARC bounds policy is active, this identity replaces every
/// application ID the guest sets on the Aura surface, because Exo
/// authorizes `set_window_bounds` only for it. An XDG client may omit
/// `xdg_toplevel.set_app_id`, so placement also applies it before the first
/// bounds request. The applied set keeps repeated calls from emitting
/// duplicate metadata requests.
///
/// # Returns
///
/// `true` when the identity is (or already was) queued; `false` when no
/// identity can be allocated (the block is exhausted).
fn apply_arc_identity(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
    zaura_surface_host_id: u32,
) -> bool {
    if ctx
        .window_placement
        .arc_application_id_applied(wl_surface_guest_id)
    {
        return true;
    }
    let Some(application_id) = ctx.window_placement.arc_application_id(wl_surface_guest_id) else {
        return false;
    };
    // The identity is proxy-generated and at most
    // `ARC_APPLICATION_ID_PREFIX` plus ten digits, so it always fits.
    let message =
        zaura_surface::req::set_application_id(zaura_surface_host_id, Some(&application_id))
            .expect("an ARC identity fits a message");
    log::debug!(
        "Set ARC policy application ID {} on zaura_surface (host_id={})",
        application_id,
        zaura_surface_host_id
    );
    ctx.client_to_host_queue.push(message);
    ctx.window_placement
        .mark_arc_application_id_applied(wl_surface_guest_id);
    true
}

/// Create or reuse the internal Aura toplevel associated with a guest
/// `xdg_toplevel`. The object is needed for screen-coordinate bounds requests.
/// Private so that every caller goes through the policy and identity gate
/// in [`attach_placement_toplevel`].
fn ensure_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) -> Option<u32> {
    if let Some(existing_id) = ctx
        .window_placement
        .aura_toplevel_for_xdg_toplevel(xdg_toplevel_guest_id)
    {
        return Some(existing_id);
    }

    let xdg_toplevel_host_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id)?;
    if ctx
        .host_zaura_shell
        .is_none_or(|shell| shell.version < PLACEMENT_SHELL_SINCE)
    {
        return None;
    }
    let zaura_toplevel_host_id =
        aura_objects::create(ctx, AuraChild::TOPLEVEL, xdg_toplevel_host_id)?;
    ctx.client_to_host_queue
        .push(zaura_toplevel::req::set_supports_screen_coordinates(
            zaura_toplevel_host_id,
        ));
    ctx.window_placement
        .remember_aura_toplevel(xdg_toplevel_guest_id, zaura_toplevel_host_id);
    Some(zaura_toplevel_host_id)
}

/// Attach the host Aura toplevel that placement uses for an XDG toplevel.
///
/// Placement needs a `zaura_toplevel` only while the ARC bounds policy is
/// active, and Exo authorizes bounds requests only for a surface carrying an
/// ARC policy identity. The identity is allocated first so exhaustion fails
/// closed: no Aura child is created that placement could never use. Every
/// path that creates a placement toplevel ([`attach_toplevel`],
/// [`attach_recorded_toplevels`], and placement itself) goes through here.
///
/// # Returns
///
/// The host `zaura_toplevel` ID, or `None` when the policy is inactive, the
/// identity is unavailable, or the host has no usable `zaura_shell` yet.
fn attach_placement_toplevel(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    if !ctx.window_placement.uses_arc_bounds() {
        return None;
    }
    if ctx
        .window_placement
        .arc_application_id(wl_surface_guest_id)
        .is_none()
    {
        log::warn!(
            "Unable to allocate ARC policy identity for wl_surface {}",
            wl_surface_guest_id
        );
        return None;
    }
    ensure_zaura_toplevel(ctx, xdg_toplevel_guest_id)
}

/// Attach the placement Aura toplevel to a newly created XDG toplevel.
///
/// Called once the generated dispatcher has accepted and mapped
/// `xdg_surface.get_toplevel`. A toplevel that cannot attach yet needs no
/// handling here: a missing `zaura_shell` is retried by
/// [`attach_recorded_toplevels`], and placement attaches lazily.
pub(crate) fn attach_toplevel(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
) {
    // `None` is the documented "not yet" outcome above, not an error.
    let _ = attach_placement_toplevel(ctx, xdg_toplevel_guest_id, wl_surface_guest_id);
}

/// Attach every recorded XDG toplevel after `zaura_shell` is bound.
///
/// A guest may create toplevels before the host registry delivers
/// `zaura_shell`; their guest-side associations are already recorded, so
/// the late shell can attach the missing Aura children through the same
/// identity gate as `get_toplevel`. A toplevel that still cannot attach is
/// retried lazily by placement.
pub(crate) fn attach_recorded_toplevels(ctx: &mut Context) {
    for (xdg_toplevel_guest_id, wl_surface_guest_id) in ctx.xdg_roles.toplevels() {
        attach_toplevel(ctx, xdg_toplevel_guest_id, wl_surface_guest_id);
    }
}

/// Release the placement Aura toplevel of a guest XDG toplevel.
///
/// Called whenever the XDG role ends (see `XdgRoles`' remove methods). Does
/// nothing when the toplevel has no Aura toplevel. A barrier still in flight
/// stays pending until its `done` but no longer gates anything.
pub(crate) fn release_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    if let Some(zaura_toplevel_host_id) = ctx
        .window_placement
        .take_aura_toplevel(xdg_toplevel_guest_id)
    {
        aura_objects::release(ctx, AuraChild::TOPLEVEL, zaura_toplevel_host_id);
    }
}

// ── Bounds barrier and configures ─────────────────────────────────────────

/// Queue a host `wl_display.sync` immediately after a
/// `zaura_toplevel.set_window_bounds` request.
///
/// Exo can emit a configure for the old bounds before it has processed the
/// placement request. The callback is an ordered host-stream barrier: once
/// `done` arrives, all events generated by requests before the sync have
/// already been delivered. Keep the callback host ID reserved through its
/// subsequent `wl_display.delete_id`, just like the other internal barriers.
fn queue_window_bounds_barrier(ctx: &mut Context, zaura_toplevel_host_id: u32) {
    // A new shortcut supersedes the previous barrier for this toplevel. The
    // old callback remains pending until its terminal event so its host ID
    // cannot be recycled while Exo still has the object alive.
    let callback = queue_internal_sync(
        ctx,
        InternalCallback::WindowBoundsBarrier {
            zaura_toplevel: zaura_toplevel_host_id,
        },
    );
    ctx.window_placement
        .register_barrier(callback.0, zaura_toplevel_host_id);
}

/// Make `callback_host_id`, already tracked as a host-only `wl_callback`,
/// the newest bounds barrier of `zaura_toplevel_host_id`, as
/// `queue_window_bounds_barrier` does without queueing the request.
#[cfg(test)]
pub(crate) fn register_window_bounds_barrier_for_test(
    ctx: &mut Context,
    callback_host_id: u32,
    zaura_toplevel_host_id: u32,
) {
    ctx.internal_callbacks.register(
        HostId(callback_host_id),
        InternalCallback::WindowBoundsBarrier {
            zaura_toplevel: zaura_toplevel_host_id,
        },
    );
    ctx.window_placement
        .register_barrier(callback_host_id, zaura_toplevel_host_id);
}

/// Keep only the XDG states the guest's `xdg_toplevel` version can receive.
///
/// `zaura_toplevel.configure` carries XDG states plus Aura-only supplements
/// (`immersive` = 100, `minimized` = 101) that are not part of xdg-shell, and
/// it is not filtered by the guest's negotiated `xdg_toplevel` version. The
/// array is a sequence of native-endian u32 values; a trailing partial entry
/// is malformed and dropped.
fn guest_xdg_toplevel_states(states: &[u8], guest_version: u32) -> Vec<u8> {
    let (entries, _malformed_tail) = states.as_chunks::<4>();
    entries
        .iter()
        .filter(|entry| {
            let state = u32::from_ne_bytes(**entry);
            xdg_toplevel::state_since(state).is_some_and(|since| since <= guest_version)
        })
        .flatten()
        .copied()
        .collect()
}

/// Queue a guest `xdg_toplevel.configure` carrying the host's states,
/// filtered to the guest's negotiated version.
fn queue_guest_xdg_configure(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u8],
) {
    // Without a recorded version, assume the baseline so only states every
    // xdg_toplevel client understands are delivered.
    let guest_version = ctx
        .shadow_table
        .guest_object_version(guest_xdg_toplevel_id)
        .unwrap_or(1);
    let states = guest_xdg_toplevel_states(states, guest_version);
    // The states array is host data, so its encoder is fallible. It is a
    // filtered copy of an array that already fit in the host's message, so
    // a failure means corrupted state and is only logged.
    queue_or_drop(
        &mut ctx.host_to_client_queue,
        xdg_toplevel::evt::configure(guest_xdg_toplevel_id, width, height, &states),
        format_args!("the configure of xdg_toplevel {guest_xdg_toplevel_id}"),
    );
}

fn queue_guest_xdg_surface_configure(ctx: &mut Context, guest_xdg_surface_id: u32, serial: u32) {
    ctx.host_to_client_queue
        .push(xdg_surface::evt::configure(guest_xdg_surface_id, serial));
}

/// Handle `wl_callback.done` for a bounds barrier
/// ([`InternalCallback::WindowBoundsBarrier`]), which the callback handler
/// has already retired.
///
/// Flushes the newest configures retained behind it. Older callbacks flush
/// nothing: a newer barrier owns the pending configures and flushes them on
/// its own `done`.
pub(crate) fn complete_barrier(
    ctx: &mut Context,
    callback_host_id: u32,
    zaura_toplevel_host_id: u32,
) {
    let held = ctx
        .window_placement
        .complete_barrier(callback_host_id, zaura_toplevel_host_id);
    if let Some(configure) = held.configure {
        queue_guest_xdg_configure(
            ctx,
            configure.guest_xdg_toplevel_id,
            configure.width,
            configure.height,
            &configure.states,
        );
    }
    if let Some(configure) = held.surface_configure {
        queue_guest_xdg_surface_configure(ctx, configure.guest_xdg_surface_id, configure.serial);
    }
}

/// Hold a host `xdg_surface.configure` behind an active bounds barrier.
///
/// # Returns
///
/// `true` when the configure is held and must not be forwarded now; it is
/// delivered when the barrier completes. `false` when no barrier gates the
/// surface's toplevel and the caller forwards the event.
pub(crate) fn hold_surface_configure(
    ctx: &mut Context,
    guest_xdg_surface_id: u32,
    serial: u32,
) -> bool {
    let Some(guest_xdg_toplevel_id) = ctx.xdg_roles.toplevel_of_xdg_surface(guest_xdg_surface_id)
    else {
        return false;
    };
    let Some(zaura_toplevel_host_id) = ctx
        .window_placement
        .aura_toplevel_for_xdg_toplevel(guest_xdg_toplevel_id)
    else {
        return false;
    };
    ctx.window_placement.hold_surface_configure(
        zaura_toplevel_host_id,
        crate::state::PendingWindowBoundsSurfaceConfigure {
            guest_xdg_surface_id,
            serial,
        },
    )
}

/// Hold a host `xdg_toplevel.configure` behind an active bounds barrier.
///
/// # Returns
///
/// `true` when the configure is held (the newest one replaces any older)
/// and must not be forwarded now; `false` when no barrier gates the
/// toplevel and the caller forwards the event.
pub(crate) fn hold_toplevel_configure(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u8],
) -> bool {
    let Some(zaura_toplevel_host_id) = ctx
        .window_placement
        .aura_toplevel_for_xdg_toplevel(guest_xdg_toplevel_id)
    else {
        return false;
    };
    hold_toplevel_configure_behind_barrier(
        ctx,
        zaura_toplevel_host_id,
        guest_xdg_toplevel_id,
        width,
        height,
        states,
    )
}

/// Hold a toplevel configure, from either the XDG or the Aura toplevel,
/// behind the active barrier of `zaura_toplevel_host_id`.
///
/// # Returns
///
/// `false`, holding nothing, when no barrier gates the toplevel.
fn hold_toplevel_configure_behind_barrier(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    guest_xdg_toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u8],
) -> bool {
    ctx.window_placement.hold_configure(
        zaura_toplevel_host_id,
        crate::state::PendingWindowBoundsConfigure {
            guest_xdg_toplevel_id,
            width,
            height,
            states: states.to_vec(),
        },
    )
}

/// Translate a host `zaura_toplevel.configure` into the guest's
/// `xdg_toplevel.configure`.
///
/// The event is held behind an active bounds barrier like an XDG configure;
/// otherwise it is delivered now with its states filtered to the guest's
/// `xdg_toplevel` version. An event for an Aura toplevel that is no longer
/// attached is dropped. `x` and `y` are only logged: `xdg_toplevel` has no
/// origin.
pub(crate) fn translate_aura_configure(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    states: &[u8],
) {
    let Some(guest_xdg_toplevel_id) = ctx
        .window_placement
        .xdg_toplevel_for_aura_toplevel(zaura_toplevel_host_id)
    else {
        return;
    };
    log::debug!(
        "zaura_toplevel.configure host={} guest={} bounds={}x{} origin=({}, {}) barrier_active={}",
        zaura_toplevel_host_id,
        guest_xdg_toplevel_id,
        width,
        height,
        x,
        y,
        ctx.window_placement
            .has_active_barrier(zaura_toplevel_host_id)
    );
    if !hold_toplevel_configure_behind_barrier(
        ctx,
        zaura_toplevel_host_id,
        guest_xdg_toplevel_id,
        width,
        height,
        states,
    ) {
        queue_guest_xdg_configure(ctx, guest_xdg_toplevel_id, width, height, states);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc_task_ids::ArcIdAllocator;
    use crate::handler::compositor::CompositorHandler;
    use crate::handler::display::DisplayHandler;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL;
    use crate::protocols::aura_shell::zaura_surface::{REQ_SET_APPLICATION_ID, REQ_UNSET_SNAP};
    use crate::protocols::aura_shell::zaura_toplevel::REQ_SET_WINDOW_BOUNDS;
    use crate::protocols::aura_shell::zaura_toplevel::{
        REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL,
        REQ_RELEASE_SINCE as REQ_RELEASE_AURA_TOPLEVEL_SINCE,
    };
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::{
        XdgToplevelHandler, REQ_DESTROY as REQ_DESTROY_XDG_TOPLEVEL,
    };
    use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
    use crate::state::ARC_APPLICATION_ID_PREFIX;
    use crate::test_support::{msg_opcode, msg_sender, queued_headers};
    use crate::wire::Action;

    fn message_sender(message: &crate::wire::Outgoing) -> u32 {
        msg_sender(&message.0)
    }

    fn message_opcode(message: &crate::wire::Outgoing) -> u16 {
        msg_opcode(&message.0)
    }

    /// Map guest keyboard `guest_keyboard_id` to `host_keyboard_id` on
    /// `guest_seat`, which is all focus resolution reads.
    fn map_keyboard(
        ctx: &mut Context,
        guest_keyboard_id: u32,
        host_keyboard_id: u32,
        guest_seat: u32,
    ) {
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard_id, guest_seat);
    }

    fn focus_keyboard(
        ctx: &mut Context,
        host_keyboard_id: u32,
        guest_seat: u32,
        guest_surface: u32,
    ) {
        let host_surface = ctx
            .shadow_table
            .get_host_id(guest_surface)
            .unwrap_or(guest_surface);
        ctx.keyboard_focus.set_for_test(
            HostId(host_keyboard_id),
            guest_seat,
            guest_surface,
            host_surface,
        );
    }

    /// Deliver the host `wl_callback.done` of `callback_host_id`.
    fn done(ctx: &mut Context, callback_host_id: u32) {
        use crate::protocols::wayland::wl_callback::WlCallbackHandler;

        ctx.last_sender_id = callback_host_id;
        assert_eq!(
            crate::handler::callback::CallbackHandler.on_done(ctx, 0),
            Action::Drop
        );
    }

    fn setup_ctx() -> (Context, u32, u32, u32) {
        setup_ctx_with(PlacementConfig::for_test(Vec::new()))
    }

    /// An xdg_toplevel (guest 400, host 500) on wl_surface (guest 100, host
    /// 200) through xdg_surface 500, with zaura_shell 300 at version 38.
    fn setup_ctx_with(placement: PlacementConfig) -> (Context, u32, u32, u32) {
        let mut ctx = Context::with_placement_for_test(placement);
        let wl_surface_guest = 100u32;
        let wl_surface_host = 200u32;
        let zaura_shell_host = 300u32;
        let xdg_toplevel_id = 400u32;
        let xdg_surface_id = 500u32;

        ctx.shadow_table
            .insert_paired_for_test(wl_surface_guest, wl_surface_host, "wl_surface");
        ctx.shadow_table.insert_paired_for_test(
            xdg_surface_id,
            xdg_surface_id + 100,
            "xdg_surface",
        );
        ctx.shadow_table.insert_paired_for_test(
            xdg_toplevel_id,
            xdg_toplevel_id + 100,
            "xdg_toplevel",
        );
        ctx.bind_aura_shell_for_test(zaura_shell_host);
        ctx.xdg_roles
            .add_toplevel_role_for_test(xdg_surface_id, wl_surface_guest, xdg_toplevel_id);

        (ctx, xdg_toplevel_id, zaura_shell_host, wl_surface_host)
    }

    #[test]
    fn grid_bounds_use_two_by_two_work_area_cells() {
        let output = crate::state::OutputState {
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        };
        assert_eq!(
            bounds_for_action(WindowLayoutAction::TopLeft, output),
            Some(Rect::new(0, 0, 1920, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Top, output),
            Some(Rect::new(0, 0, 3840, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::TopRight, output),
            Some(Rect::new(1920, 0, 1920, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Left, output),
            Some(Rect::new(0, 0, 1920, 2160))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Fullscreen, output),
            Some(Rect::new(0, 0, 3840, 2160))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::BottomLeft, output),
            Some(Rect::new(0, 1080, 1920, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Bottom, output),
            Some(Rect::new(0, 1080, 3840, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::BottomRight, output),
            Some(Rect::new(1920, 1080, 1920, 1080))
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Right, output),
            Some(Rect::new(1920, 0, 1920, 2160))
        );
    }

    #[test]
    fn grid_bounds_reject_coordinate_overflow() {
        let output = crate::state::OutputState {
            origin_x: i32::MAX,
            mode_width: 4,
            mode_height: 4,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        };

        assert_eq!(
            bounds_for_action(WindowLayoutAction::TopRight, output),
            None
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::BottomRight, output),
            None
        );
        assert_eq!(bounds_for_action(WindowLayoutAction::Right, output), None);

        let tiny_output = crate::state::OutputState {
            mode_width: 1,
            mode_height: 1,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        };
        assert_eq!(
            bounds_for_action(WindowLayoutAction::TopLeft, tiny_output),
            None,
            "split layouts must never emit zero-sized bounds"
        );
        assert_eq!(
            bounds_for_action(WindowLayoutAction::Fullscreen, tiny_output),
            Some(Rect::new(0, 0, 1, 1))
        );
    }

    /// Host keyboard 5 (guest 10, seat 11) focused on wl_surface 12 (host
    /// 22) with xdg_toplevel 13 (host 23), zaura_shell 24 at version 38, and
    /// a 3840x2160 output 25 the ARC bounds policy can place on.
    ///
    /// # Returns
    ///
    /// The context, the host keyboard ID, and the guest xdg_toplevel ID.
    fn focused_toplevel_ctx() -> (Context, u32, u32) {
        let host_keyboard = 5u32;
        let xdg_toplevel = 13u32;
        let mut ctx = Context::with_placement_for_test(PlacementConfig::arc_bounds_for_test(""));
        map_keyboard(&mut ctx, 10, host_keyboard, 11);
        ctx.shadow_table
            .insert_paired_for_test(12, 22, "wl_surface");
        ctx.shadow_table
            .insert_paired_for_test(xdg_toplevel, 23, "xdg_toplevel");
        ctx.xdg_roles
            .add_toplevel_role_for_test(xdg_toplevel + 1000, 12, xdg_toplevel);
        focus_keyboard(&mut ctx, host_keyboard, 11, 12);
        ctx.bind_aura_shell_for_test(24);
        ctx.outputs.bind_for_test(
            25,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                aura_current_scale: Some(1000),
                aura_device_scale_factor: Some(1000),
                ..Default::default()
            },
        );
        (ctx, host_keyboard, xdg_toplevel)
    }

    #[test]
    fn place_focused_queues_bounds_after_state_reset() {
        let (mut ctx, host_keyboard, xdg_toplevel) = focused_toplevel_ctx();

        assert_eq!(
            place_focused(
                &mut ctx,
                HostId(host_keyboard),
                WindowLayoutAction::Fullscreen,
            ),
            Ok(())
        );
        let opcodes: Vec<u16> = ctx
            .client_to_host_queue
            .iter()
            .map(|(message, _)| msg_opcode(message))
            .collect();
        assert_eq!(
            opcodes,
            vec![
                crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
                crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES,
                crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE,
                crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID,
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                crate::protocols::wayland::wl_display::REQ_SYNC,
            ]
        );
        let bounds = ctx
            .client_to_host_queue
            .iter()
            .find(|(message, _)| {
                msg_sender(message)
                    == ctx
                        .window_placement
                        .aura_toplevel_for_xdg_toplevel(xdg_toplevel)
                        .unwrap()
                    && msg_opcode(message) == REQ_SET_WINDOW_BOUNDS
            })
            .expect("window bounds request")
            .0
            .as_slice();
        assert_eq!(i32::from_ne_bytes(bounds[8..12].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(bounds[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(bounds[16..20].try_into().unwrap()), 3840);
        assert_eq!(i32::from_ne_bytes(bounds[20..24].try_into().unwrap()), 2160);
    }

    /// `zaura_surface.unset_snap` exists only from version 16. An Aura
    /// surface created under an older `zaura_shell` generation keeps its
    /// version after the shell is re-advertised at a newer one, so placement
    /// must not send it the request (Exo would reject the unknown method and
    /// disconnect the client). A new enough surface still gets it.
    #[test]
    fn unset_snap_is_sent_only_to_an_aura_surface_that_has_it() {
        for (surface_version, expect_unset_snap) in [
            (REQ_UNSET_SNAP_SINCE - 1, false),
            (REQ_UNSET_SNAP_SINCE, true),
        ] {
            let (mut ctx, host_keyboard, xdg_toplevel) = focused_toplevel_ctx();
            let aura_surface = 0x7000;
            ctx.shadow_table.insert_host_only_for_test(
                aura_surface,
                "zaura_surface",
                surface_version,
            );
            ctx.aura_surfaces.remember(22, aura_surface);

            assert_eq!(
                place_focused(&mut ctx, HostId(host_keyboard), WindowLayoutAction::TopLeft),
                Ok(()),
                "zaura_surface v{surface_version}"
            );
            let sent_unset_snap = ctx.client_to_host_queue.iter().any(|message| {
                message_sender(message) == aura_surface && message_opcode(message) == REQ_UNSET_SNAP
            });
            assert_eq!(
                sent_unset_snap, expect_unset_snap,
                "zaura_surface v{surface_version}"
            );
            let aura_toplevel = ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel)
                .expect("Aura toplevel");
            assert!(
                ctx.client_to_host_queue.iter().any(|message| {
                    message_sender(message) == aura_toplevel
                        && message_opcode(message) == REQ_SET_WINDOW_BOUNDS
                }),
                "the bounds request does not depend on unset_snap"
            );
        }
    }

    /// Placement maps into the work area of the output the focused surface
    /// entered, with that output's own origin, scale and rotation: a 2x
    /// panel, a 1x external monitor left of it (negative coordinates), and
    /// that monitor rotated.
    #[test]
    fn placement_uses_the_entered_outputs_own_geometry() {
        use crate::handler::output::fixture::{
            context, dock, move_surface, send_metrics, Display, EXTERNAL, PANEL,
        };
        let (host_keyboard, host_surface, xdg_toplevel) = (5, 22, 13);
        let mut ctx = context();
        map_keyboard(&mut ctx, 10, host_keyboard, 11);
        ctx.shadow_table
            .insert_paired_for_test(12, host_surface, "wl_surface");
        ctx.shadow_table
            .insert_paired_for_test(xdg_toplevel, 23, "xdg_toplevel");
        ctx.xdg_roles
            .add_toplevel_role_for_test(xdg_toplevel + 1000, 12, xdg_toplevel);
        focus_keyboard(&mut ctx, host_keyboard, 11, 12);
        let panel = dock(&mut ctx, 1, 10, 30, PANEL);
        let external = dock(&mut ctx, 1, 11, 31, EXTERNAL);
        // The right half; `set_window_bounds` is (x, y, width, height, output).
        let place_right = |ctx: &mut Context| {
            ctx.client_to_host_queue.clear();
            let placed = place_focused(ctx, HostId(host_keyboard), WindowLayoutAction::Right);
            assert_eq!(placed, Ok(()));
            let aura_toplevel = ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel);
            let (bounds, _) = ctx
                .client_to_host_queue
                .iter()
                .find(|message| {
                    Some(message_sender(message)) == aura_toplevel
                        && message_opcode(message) == REQ_SET_WINDOW_BOUNDS
                })
                .expect("window bounds request");
            let field = |index: usize| {
                let start = 8 + 4 * index;
                i32::from_ne_bytes(bounds[start..start + 4].try_into().unwrap())
            };
            let output = u32::try_from(field(4)).unwrap();
            (Rect::new(field(0), field(1), field(2), field(3)), output)
        };

        move_surface(&mut ctx, host_surface, None, external);
        assert_eq!(
            place_right(&mut ctx),
            (Rect::new(-960, 0, 960, 1080), external)
        );
        move_surface(&mut ctx, host_surface, Some(external), panel);
        assert_eq!(place_right(&mut ctx), (Rect::new(640, 0, 640, 752), panel));

        // Rotated to portrait (Exo's ROTATE_90 is `transform.270`), the
        // monitor's left edge moves to x = -1080.
        let rotated = Display {
            origin: (-1080, 0),
            transform: 3,
            ..EXTERNAL
        };
        send_metrics(&mut ctx, external, rotated);
        move_surface(&mut ctx, host_surface, Some(panel), external);
        assert_eq!(
            place_right(&mut ctx),
            (Rect::new(-540, 0, 540, 1920), external)
        );
    }

    #[test]
    fn left_and_right_use_direct_bounds_instead_of_snap_requests() {
        let (mut ctx, host_keyboard, xdg_toplevel) = focused_toplevel_ctx();

        for (action, expected_x) in [
            (WindowLayoutAction::Left, 0),
            (WindowLayoutAction::Right, 1920),
        ] {
            ctx.client_to_host_queue.clear();
            assert_eq!(
                place_focused(&mut ctx, HostId(host_keyboard), action),
                Ok(())
            );
            let opcodes: Vec<u16> = ctx
                .client_to_host_queue
                .iter()
                .map(|(message, _)| msg_opcode(message))
                .collect();
            assert!(
                opcodes.contains(&REQ_SET_WINDOW_BOUNDS),
                "{action:?} must use set_window_bounds"
            );
            assert!(
                !opcodes.contains(&crate::protocols::aura_shell::zaura_surface::REQ_SET_SNAP_LEFT),
                "{action:?} must not use set_snap_left"
            );
            assert!(
                !opcodes.contains(&crate::protocols::aura_shell::zaura_surface::REQ_SET_SNAP_RIGHT),
                "{action:?} must not use set_snap_right"
            );
            let bounds = ctx
                .client_to_host_queue
                .iter()
                .find(|(message, _)| {
                    msg_sender(message)
                        == ctx
                            .window_placement
                            .aura_toplevel_for_xdg_toplevel(xdg_toplevel)
                            .unwrap()
                        && msg_opcode(message) == REQ_SET_WINDOW_BOUNDS
                })
                .expect("window bounds request")
                .0
                .as_slice();
            assert_eq!(
                i32::from_ne_bytes(bounds[8..12].try_into().unwrap()),
                expected_x
            );
            assert_eq!(i32::from_ne_bytes(bounds[12..16].try_into().unwrap()), 0);
            assert_eq!(i32::from_ne_bytes(bounds[16..20].try_into().unwrap()), 1920);
            assert_eq!(i32::from_ne_bytes(bounds[20..24].try_into().unwrap()), 2160);
        }
    }

    #[test]
    fn arc_policy_identity_is_unique_and_stable_per_surface() {
        let mut ctx = Context::with_placement_for_test(PlacementConfig::arc_bounds_for_test(""));
        let first = ctx
            .window_placement
            .arc_application_id(10)
            .expect("first identity");
        assert_eq!(
            ctx.window_placement.arc_application_id(10).as_deref(),
            Some(first.as_str())
        );
        let second = ctx
            .window_placement
            .arc_application_id(11)
            .expect("second identity");
        assert_ne!(first, second);
        assert!(first.starts_with(ARC_APPLICATION_ID_PREFIX));
        assert!(second.starts_with(ARC_APPLICATION_ID_PREFIX));
        for identity in [&first, &second] {
            assert!(!identity.starts_with("org.chromium.arc.session."));
            let id = identity
                .strip_prefix(ARC_APPLICATION_ID_PREFIX)
                .expect("task-form ARC prefix")
                .parse::<u32>()
                .expect("numeric ARC task ID");
            assert!((crate::arc_task_ids::ARC_ID_POOL_START
                ..=crate::arc_task_ids::ARC_ID_POOL_END)
                .contains(&id));
        }
    }

    #[test]
    fn arc_policy_identity_fails_closed_when_the_block_is_exhausted() {
        let mut ctx = Context::with_placement_for_test(PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_BOUNDS_AS_ARC", "1")],
            None,
            ArcIdAllocator::exhausted_for_test(),
        ));
        assert!(ctx.window_placement.arc_application_id(10).is_none());
        assert!(!ctx.window_placement.has_arc_application_ids());
    }

    #[test]
    fn arc_policy_identity_is_applied_before_placement_without_app_id() {
        let (mut ctx, _xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) =
            setup_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let zaura_surface_host = ensure_host_zaura_surface(&mut ctx, 100).expect("Aura surface");
        assert!(apply_arc_identity(&mut ctx, 100, zaura_surface_host,));
        assert!(ctx.window_placement.arc_application_id_applied(100));
        let set_application_id_count = ctx
            .client_to_host_queue
            .iter()
            .filter(|(message, _)| {
                msg_sender(message) == zaura_surface_host
                    && msg_opcode(message) == REQ_SET_APPLICATION_ID
            })
            .count();
        assert_eq!(set_application_id_count, 1);

        assert!(apply_arc_identity(&mut ctx, 100, zaura_surface_host,));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .filter(|(message, _)| {
                    msg_sender(message) == zaura_surface_host
                        && msg_opcode(message) == REQ_SET_APPLICATION_ID
                })
                .count(),
            1,
            "repeated placement must not duplicate ARC metadata"
        );
    }

    #[test]
    fn bounds_barrier_holds_configure_until_done() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_toplevel_host = 650;
        let callback_host = 651;
        ctx.window_placement
            .remember_aura_toplevel(xdg_toplevel_id, aura_toplevel_host);
        ctx.shadow_table
            .insert_host_only_for_test(aura_toplevel_host, "zaura_toplevel", 38);
        ctx.shadow_table
            .insert_host_only_for_test(callback_host, "wl_callback", 1);
        register_window_bounds_barrier_for_test(&mut ctx, callback_host, aura_toplevel_host);
        let mut handler = CompositorHandler;
        ctx.last_sender_id = aura_toplevel_host;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut handler,
                &mut ctx,
                0,
                0,
                10,
                20,
                &[],
            ),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.window_placement.has_held_configure(aura_toplevel_host));

        done(&mut ctx, callback_host);
        assert!(!ctx.window_placement.has_active_barrier(aura_toplevel_host));
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            msg_opcode(&ctx.host_to_client_queue[0].0),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
    }

    #[test]
    fn bounds_barrier_flushes_toplevel_before_surface_configure() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_toplevel_host = 700;
        let callback_host = 701;
        let xdg_surface_host = 600;
        ctx.window_placement
            .remember_aura_toplevel(xdg_toplevel_id, aura_toplevel_host);
        ctx.shadow_table
            .insert_host_only_for_test(aura_toplevel_host, "zaura_toplevel", 38);
        ctx.shadow_table
            .insert_host_only_for_test(callback_host, "wl_callback", 1);
        register_window_bounds_barrier_for_test(&mut ctx, callback_host, aura_toplevel_host);
        ctx.last_sender_id = xdg_surface_host;
        assert_eq!(
            XdgSurfaceHandler::on_configure(&mut CompositorHandler, &mut ctx, 77),
            Action::Drop
        );
        ctx.last_sender_id = aura_toplevel_host;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
                800,
                600,
                &[],
            ),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());

        done(&mut ctx, callback_host);
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(
            msg_opcode(&ctx.host_to_client_queue[0].0),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
        assert_eq!(
            msg_opcode(&ctx.host_to_client_queue[1].0),
            crate::protocols::xdg_shell::xdg_surface::EVT_CONFIGURE
        );
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[1].0[8..12].try_into().unwrap()),
            77
        );
    }

    /// A `zaura_shell` without `set_window_bounds` cannot place, and one
    /// without `zaura_output.insets` would measure a work area that
    /// includes the shelf, so neither gets an Aura toplevel; the first
    /// version with both does.
    #[test]
    fn placement_needs_a_shell_with_window_bounds_and_output_insets() {
        for (shell_version, expect_toplevel) in [
            (REQ_SET_WINDOW_BOUNDS_SINCE, false),
            (EVT_INSETS_SINCE - 1, false),
            (EVT_INSETS_SINCE, true),
        ] {
            let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) =
                setup_ctx_with(PlacementConfig::arc_bounds_for_test(""));
            ctx.host_zaura_shell
                .as_mut()
                .expect("bound zaura_shell")
                .version = shell_version;
            attach_toplevel(&mut ctx, xdg_toplevel_id, 100);
            assert_eq!(
                ctx.window_placement
                    .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id)
                    .is_some(),
                expect_toplevel,
                "zaura_shell v{shell_version}"
            );
            assert_eq!(
                ctx.client_to_host_queue.is_empty(),
                !expect_toplevel,
                "zaura_shell v{shell_version}"
            );
        }
    }

    #[test]
    fn an_old_shell_is_reported_once_per_process() {
        OLD_SHELL_WARNED.store(false, Ordering::Relaxed);
        assert!(!warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE));
        assert!(warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE - 1));
        assert!(!warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE - 1));
    }

    /// Before `zaura_toplevel.release` the Aura toplevel has no destructor:
    /// releasing the XDG role retires its ID without a request, and the ID
    /// stays reserved for the connection.
    #[test]
    fn aura_toplevel_without_release_is_retired_silently() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.host_zaura_shell
            .as_mut()
            .expect("bound zaura_shell")
            .version = REQ_RELEASE_AURA_TOPLEVEL_SINCE - 1;
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();

        release_toplevel(&mut ctx, xdg_toplevel_id);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id),
            None
        );
        assert_eq!(ctx.shadow_table.get_host_interface(aura_id), None);
        assert!(ctx.shadow_table.is_retired_host(aura_id));
        assert!(!ctx.shadow_table.is_host_id_available(aura_id));
    }

    #[test]
    fn ensure_zaura_toplevel_requests_screen_coordinates() {
        let (mut ctx, xdg_toplevel_id, zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id),
            Some(aura_id)
        );
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_sender(&ctx.client_to_host_queue[0].0), zaura_shell_host);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[0].0),
            REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue[1].0), aura_id);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES
        );
    }

    /// A malformed client that gives one wl_surface two xdg_surfaces ends
    /// only the destroyed xdg_surface's toplevel role.
    #[test]
    fn destroying_one_of_two_xdg_surfaces_ends_only_its_toplevel() {
        let (mut ctx, first_toplevel, _zaura_shell_host, _wl_surface_host) =
            setup_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let (first_xdg_surface, second_xdg_surface, second_toplevel) = (500, 520, 420);
        ctx.shadow_table
            .insert_paired_for_test(second_xdg_surface, 620, "xdg_surface");
        ctx.shadow_table
            .insert_paired_for_test(second_toplevel, 720, "xdg_toplevel");
        ctx.xdg_roles
            .add_toplevel_role_for_test(second_xdg_surface, 100, second_toplevel);
        let first_aura = ensure_zaura_toplevel(&mut ctx, first_toplevel).expect("toplevel");
        let second_aura = ensure_zaura_toplevel(&mut ctx, second_toplevel).expect("toplevel");
        ctx.client_to_host_queue.clear();

        ctx.guest_destroyed(first_xdg_surface);
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            vec![(first_aura, REQ_RELEASE_AURA_TOPLEVEL)]
        );
        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(first_toplevel), None);
        assert_eq!(
            ctx.xdg_roles.wl_surface_of_toplevel(second_toplevel),
            Some(100)
        );
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(second_toplevel),
            Some(second_aura)
        );
    }

    /// A wl_surface destroyed under a live XDG toplevel ends the toplevel's
    /// role: its Aura toplevel and Aura surface are both released before
    /// the wl_surface destructor.
    #[test]
    fn wl_surface_destroy_releases_its_aura_children_first() {
        use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE as REQ_RELEASE_AURA_SURFACE;
        use crate::protocols::wayland::wl_surface::{WlSurfaceHandler, REQ_DESTROY};

        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, wl_surface_host) =
            setup_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let wl_surface_guest = 100;
        let aura_toplevel = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("toplevel");
        let aura_surface = ensure_host_zaura_surface(&mut ctx, wl_surface_guest).expect("surface");
        ctx.client_to_host_queue.clear();

        ctx.last_sender_id = wl_surface_guest;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        let queued = queued_headers(&ctx.client_to_host_queue);
        assert_eq!(
            queued,
            vec![
                (aura_toplevel, REQ_RELEASE_AURA_TOPLEVEL),
                (aura_surface, REQ_RELEASE_AURA_SURFACE),
                (wl_surface_host, REQ_DESTROY),
            ]
        );
    }

    #[test]
    fn xdg_toplevel_destroy_reserves_aura_id_until_host_delete_id() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = xdg_toplevel_id;

        assert_eq!(
            XdgToplevelHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(xdg_toplevel_id),
            None
        );
        // Child-first: the Aura extension is released before its xdg role.
        let queued = queued_headers(&ctx.client_to_host_queue);
        assert_eq!(
            queued,
            vec![
                (aura_id, REQ_RELEASE_AURA_TOPLEVEL),
                (xdg_toplevel_id + 100, REQ_DESTROY_XDG_TOPLEVEL),
            ]
        );
        assert!(ctx.shadow_table.is_retired_host(aura_id));
        assert!(!ctx.shadow_table.is_host_id_available(aura_id));

        ctx.last_sender_id = 1;
        assert_eq!(DisplayHandler.on_delete_id(&mut ctx, aura_id), Action::Drop);
        assert!(!ctx.shadow_table.is_retired_host(aura_id));
        assert!(ctx.shadow_table.is_host_id_available(aura_id));
    }

    #[test]
    fn window_bounds_barrier_is_queued_after_placement_request() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();

        queue_window_bounds_barrier(&mut ctx, aura_id);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(message), 1);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::wayland::wl_display::REQ_SYNC
        );
        let callback_id = u32::from_ne_bytes(message[8..12].try_into().unwrap());
        assert_eq!(
            ctx.internal_callbacks.get(HostId(callback_id)),
            Some(InternalCallback::WindowBoundsBarrier {
                zaura_toplevel: aura_id
            })
        );
        assert_eq!(
            ctx.window_placement.active_barrier_for_toplevel(aura_id),
            Some(callback_id)
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(callback_id),
            Some("wl_callback")
        );
    }

    #[test]
    fn aura_configure_is_translated_to_guest_xdg_configure() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.window_placement
            .remember_aura_toplevel(xdg_toplevel_id, 77);
        ctx.shadow_table
            .insert_host_only_for_test(77, "zaura_toplevel", 38);
        ctx.shadow_table
            .set_guest_version_for_test(xdg_toplevel_id, 6);
        ctx.last_sender_id = 77;

        let states = encode_states(&[1, 2, 3, 4]);
        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                10,
                20,
                1920,
                1080,
                &states,
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(msg_sender(message), xdg_toplevel_id);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
        let mut expected = Vec::new();
        expected.extend_from_slice(&1920i32.to_ne_bytes());
        expected.extend_from_slice(&1080i32.to_ne_bytes());
        expected.extend_from_slice(&16u32.to_ne_bytes());
        expected.extend_from_slice(&states);
        assert_eq!(&message[8..], expected.as_slice());
    }

    fn encode_states(states: &[u32]) -> Vec<u8> {
        states
            .iter()
            .flat_map(|state| state.to_ne_bytes())
            .collect()
    }

    /// Decode the states array of a guest `xdg_toplevel.configure`.
    fn configure_states(message: &[u8]) -> Vec<u32> {
        let length = u32::from_ne_bytes(message[16..20].try_into().unwrap()) as usize;
        message[20..20 + length]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|entry| u32::from_ne_bytes(*entry))
            .collect()
    }

    fn translated_states(guest_version: u32, states: &[u32]) -> Vec<u32> {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.shadow_table
            .set_guest_version_for_test(xdg_toplevel_id, guest_version);
        ctx.window_placement
            .remember_aura_toplevel(xdg_toplevel_id, 77);
        ctx.shadow_table
            .insert_host_only_for_test(77, "zaura_toplevel", 38);
        ctx.last_sender_id = 77;
        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
                800,
                600,
                &encode_states(states),
            );
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let message = &ctx.host_to_client_queue[0].0;
        assert_eq!(msg_sender(message), xdg_toplevel_id);
        assert_eq!(
            msg_opcode(message),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
        configure_states(message)
    }

    #[test]
    fn aura_configure_drops_aura_only_states() {
        // fullscreen + Aura immersive (100) + Aura minimized (101) + activated.
        assert_eq!(translated_states(6, &[2, 100, 101, 4]), vec![2, 4]);
    }

    #[test]
    fn aura_configure_filters_states_by_guest_xdg_toplevel_version() {
        let host_states = [1, 5, 8, 9, 10, 13, 14];
        assert_eq!(translated_states(1, &host_states), vec![1]);
        assert_eq!(translated_states(2, &host_states), vec![1, 5, 8]);
        assert_eq!(translated_states(6, &host_states), vec![1, 5, 8, 9]);
        assert_eq!(translated_states(7, &host_states), vec![1, 5, 8, 9, 10, 13]);
    }

    #[test]
    fn aura_configure_drops_truncated_state_entry() {
        assert_eq!(guest_xdg_toplevel_states(&[2, 0, 0, 0, 4, 0], 6).len(), 4);
    }

    #[test]
    fn held_aura_configure_is_filtered_when_barrier_completes() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        ctx.shadow_table
            .set_guest_version_for_test(xdg_toplevel_id, 1);
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        queue_window_bounds_barrier(&mut ctx, aura_id);
        let callback_id = ctx
            .window_placement
            .active_barrier_for_toplevel(aura_id)
            .expect("active barrier");
        ctx.last_sender_id = aura_id;
        let action =
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
                800,
                600,
                &encode_states(&[2, 5, 100, 101]),
            );
        assert_eq!(action, Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());

        done(&mut ctx, callback_id);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(configure_states(&ctx.host_to_client_queue[0].0), vec![2]);
    }

    #[test]
    fn aura_origin_change_is_dropped_with_or_without_a_bounds_barrier() {
        let (mut ctx, xdg_toplevel_id, _zaura_shell_host, _wl_surface_host) = setup_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, xdg_toplevel_id).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = aura_id;
        let origin_change = |ctx: &mut Context| {
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                ctx,
                123,
                456,
            )
        };

        assert_eq!(origin_change(&mut ctx), Action::Drop);
        queue_window_bounds_barrier(&mut ctx, aura_id);
        ctx.last_sender_id = aura_id;
        assert_eq!(origin_change(&mut ctx), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }
}
