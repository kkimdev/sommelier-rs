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
    use crate::protocols::aura_shell::zaura_shell;
    use crate::protocols::aura_shell::zaura_surface::REQ_RELEASE as REQ_RELEASE_AURA_SURFACE;
    use crate::protocols::aura_shell::zaura_toplevel::{
        ZauraToplevelHandler, REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL,
        REQ_RELEASE_SINCE as REQ_RELEASE_AURA_TOPLEVEL_SINCE,
    };
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::protocols::wayland::wl_surface::{self, WlSurfaceHandler};
    use crate::protocols::xdg_shell::xdg_surface::XdgSurfaceHandler;
    use crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::state::{OutputState, ARC_APPLICATION_ID_PREFIX};
    use crate::test_support::{decode, msg_sender, queued_headers};
    use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};
    use std::collections::VecDeque;

    /// A guest object and the host object it is paired with.
    #[derive(Clone, Copy)]
    struct Paired {
        guest: u32,
        host: u32,
    }

    const SURFACE: Paired = Paired {
        guest: 100,
        host: 200,
    };
    const XDG_SURFACE: Paired = Paired {
        guest: 110,
        host: 210,
    };
    const TOPLEVEL: Paired = Paired {
        guest: 120,
        host: 220,
    };
    /// The host `zaura_shell`, at the vendored version.
    const SHELL: u32 = 300;
    /// Host keyboard 5 (guest 10, on seat 11).
    const KEYBOARD: Paired = Paired { guest: 10, host: 5 };
    /// A 3840x2160 output at 1x.
    const OUTPUT: u32 = 25;

    /// Give `ctx` wl_surface [`SURFACE`] with the toplevel role:
    /// [`XDG_SURFACE`] and its [`TOPLEVEL`].
    fn add_toplevel(ctx: &mut Context) {
        for (object, interface) in [
            (SURFACE, "wl_surface"),
            (XDG_SURFACE, "xdg_surface"),
            (TOPLEVEL, "xdg_toplevel"),
        ] {
            ctx.shadow_table
                .insert_paired_for_test(object.guest, object.host, interface);
        }
        ctx.xdg_roles
            .add_toplevel_role_for_test(XDG_SURFACE.guest, SURFACE.guest, TOPLEVEL.guest);
    }

    /// A context under `placement` with [`add_toplevel`] and [`SHELL`].
    fn toplevel_ctx_with(placement: PlacementConfig) -> Context {
        let mut ctx = Context::with_placement_for_test(placement);
        add_toplevel(&mut ctx);
        ctx.bind_aura_shell_for_test(SHELL);
        ctx
    }

    /// [`toplevel_ctx_with`] the default placement policy.
    fn toplevel_ctx() -> Context {
        toplevel_ctx_with(PlacementConfig::for_test(Vec::new()))
    }

    /// Focus [`KEYBOARD`] on [`SURFACE`], which is all focus resolution
    /// reads.
    fn focus(ctx: &mut Context) {
        let seat = 11;
        ctx.shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        ctx.keyboard_to_seat.insert(KEYBOARD.guest, seat);
        ctx.keyboard_focus
            .set_for_test(HostId(KEYBOARD.host), seat, SURFACE.guest, SURFACE.host);
    }

    /// An output of `mode` at 1x, at `origin_x`.
    fn output(origin_x: i32, mode: (i32, i32)) -> OutputState {
        OutputState {
            origin_x,
            mode_width: mode.0,
            mode_height: mode.1,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        }
    }

    /// [`toplevel_ctx_with`] the ARC bounds policy, focused through
    /// [`KEYBOARD`], on [`OUTPUT`].
    fn focused_ctx() -> Context {
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        focus(&mut ctx);
        ctx.outputs.bind_for_test(OUTPUT, output(0, (3840, 2160)));
        ctx
    }

    /// Place the focused toplevel with `action`; returns what it sends the
    /// host.
    fn place(ctx: &mut Context, action: WindowLayoutAction) -> Vec<(u32, AnyRequest)> {
        ctx.client_to_host_queue.clear();
        assert_eq!(place_focused(ctx, HostId(KEYBOARD.host), action), Ok(()));
        host_requests(ctx)
    }

    fn window_bounds(x: i32, y: i32, width: i32, height: i32, output: u32) -> AnyRequest {
        zaura_toplevel::Request::SetWindowBounds {
            x,
            y,
            width,
            height,
            output,
        }
        .into()
    }

    /// Each message of `queue`, decoded as the interface `interface_of` its
    /// sender has. Panics if a message leaves a descriptor unread.
    fn decode_queue<'a, M>(
        queue: &[Outgoing],
        interface_of: impl Fn(u32) -> Option<&'a str>,
        from_wire: fn(&str, &mut crate::wire::WireMessage) -> Result<M, crate::wire::ProtocolError>,
    ) -> Vec<(u32, M)> {
        queue
            .iter()
            .map(|(message, fds)| {
                let interface = interface_of(msg_sender(message)).expect("a live sender");
                let mut fds: VecDeque<_> = fds
                    .iter()
                    .map(|fd| fd.try_clone().expect("duplicate a queued descriptor"))
                    .collect();
                let decoded = decode(message, &mut fds, |msg| from_wire(interface, msg))
                    .expect("a whole message");
                assert!(
                    fds.is_empty(),
                    "a queued message carries unread descriptors"
                );
                decoded
            })
            .collect()
    }

    /// What `ctx` queued for the host.
    fn host_requests(ctx: &Context) -> Vec<(u32, AnyRequest)> {
        let table = &ctx.shadow_table;
        // The fixtures pair neither the display nor the shell.
        let interface_of = |id| match id {
            WL_DISPLAY_ID => Some("wl_display"),
            SHELL => Some("zaura_shell"),
            id => table.get_host_interface(id),
        };
        decode_queue(
            &ctx.client_to_host_queue,
            interface_of,
            AnyRequest::from_wire,
        )
    }

    /// What `ctx` queued for the guest.
    fn guest_events(ctx: &Context) -> Vec<(u32, AnyEvent)> {
        let table = &ctx.shadow_table;
        decode_queue(
            &ctx.host_to_client_queue,
            |id| table.get_interface(id),
            AnyEvent::from_wire,
        )
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

    /// The host's `zaura_toplevel.configure` of `aura_toplevel`.
    fn aura_configure(ctx: &mut Context, aura_toplevel: u32, size: (i32, i32), states: &[u8]) {
        ctx.last_sender_id = aura_toplevel;
        let action = ZauraToplevelHandler::on_configure(
            &mut CompositorHandler,
            ctx,
            10,
            20,
            size.0,
            size.1,
            states,
        );
        assert_eq!(action, Action::Drop);
    }

    /// The guest's `xdg_toplevel.configure` of [`TOPLEVEL`].
    fn toplevel_configure(size: (i32, i32), states: &[u32]) -> (u32, AnyEvent) {
        let configure = xdg_toplevel::Event::Configure {
            width: size.0,
            height: size.1,
            states: encode_states(states),
        };
        (TOPLEVEL.guest, configure.into())
    }

    fn encode_states(states: &[u32]) -> Vec<u8> {
        states
            .iter()
            .flat_map(|state| state.to_ne_bytes())
            .collect()
    }

    /// Give [`TOPLEVEL`] the v38 Aura toplevel `aura_toplevel`.
    fn remember_aura_toplevel(ctx: &mut Context, aura_toplevel: u32) {
        ctx.window_placement
            .remember_aura_toplevel(TOPLEVEL.guest, aura_toplevel);
        ctx.shadow_table
            .insert_host_only_for_test(aura_toplevel, "zaura_toplevel", 38);
    }

    #[test]
    fn grid_bounds_use_two_by_two_work_area_cells() {
        use WindowLayoutAction::{
            Bottom, BottomLeft, BottomRight, Fullscreen, Left, Right, Top, TopLeft, TopRight,
        };
        for (action, bounds) in [
            (TopLeft, Rect::new(0, 0, 1920, 1080)),
            (Top, Rect::new(0, 0, 3840, 1080)),
            (TopRight, Rect::new(1920, 0, 1920, 1080)),
            (Left, Rect::new(0, 0, 1920, 2160)),
            (Fullscreen, Rect::new(0, 0, 3840, 2160)),
            (BottomLeft, Rect::new(0, 1080, 1920, 1080)),
            (Bottom, Rect::new(0, 1080, 3840, 1080)),
            (BottomRight, Rect::new(1920, 1080, 1920, 1080)),
            (Right, Rect::new(1920, 0, 1920, 2160)),
        ] {
            assert_eq!(
                bounds_for_action(action, output(0, (3840, 2160))),
                Some(bounds),
                "{action:?}"
            );
        }
    }

    #[test]
    fn grid_bounds_reject_coordinate_overflow() {
        use WindowLayoutAction::{BottomRight, Fullscreen, Right, TopLeft, TopRight};
        let far_right = output(i32::MAX, (4, 4));
        for action in [TopRight, BottomRight, Right] {
            assert_eq!(bounds_for_action(action, far_right), None, "{action:?}");
        }
        assert_eq!(
            bounds_for_action(TopLeft, output(0, (1, 1))),
            None,
            "split layouts must never emit zero-sized bounds"
        );
        assert_eq!(
            bounds_for_action(Fullscreen, output(0, (1, 1))),
            Some(Rect::new(0, 0, 1, 1))
        );
    }

    /// The first placement creates the Aura toplevel (in screen
    /// coordinates) and surface and sets the ARC identity; each placement
    /// then resets the window state and sends direct bounds, never a snap
    /// request, followed by its barrier.
    #[test]
    fn place_focused_queues_bounds_after_state_reset() {
        use WindowLayoutAction::{Fullscreen, Left, Right};
        // Each sequence of placements, with the x and width of its bounds.
        for placements in [
            &[(Fullscreen, 0, 3840)][..],
            &[(Left, 0, 1920), (Right, 1920, 1920)],
        ] {
            let mut ctx = focused_ctx();
            for (placement, &(action, x, width)) in placements.iter().enumerate() {
                let sent = place(&mut ctx, action);
                let aura_toplevel = ctx
                    .window_placement
                    .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest)
                    .expect("an Aura toplevel");
                let aura_surface = ctx
                    .aura_surfaces
                    .get(SURFACE.host)
                    .expect("an Aura surface");
                let application_id = ctx.window_placement.arc_application_id(SURFACE.guest);
                assert!(application_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with(ARC_APPLICATION_ID_PREFIX)));
                let mut expected: Vec<(u32, AnyRequest)> = Vec::new();
                if placement == 0 {
                    expected.extend([
                        (
                            SHELL,
                            zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                                id: aura_toplevel,
                                toplevel: TOPLEVEL.host,
                            }
                            .into(),
                        ),
                        (
                            aura_toplevel,
                            zaura_toplevel::Request::SetSupportsScreenCoordinates {}.into(),
                        ),
                        (
                            SHELL,
                            zaura_shell::Request::GetAuraSurface {
                                id: aura_surface,
                                surface: SURFACE.host,
                            }
                            .into(),
                        ),
                        (
                            aura_surface,
                            zaura_surface::Request::SetApplicationId { application_id }.into(),
                        ),
                    ]);
                }
                let callback = ctx
                    .window_placement
                    .active_barrier_for_toplevel(aura_toplevel)
                    .expect("a barrier");
                expected.extend([
                    (
                        TOPLEVEL.host,
                        xdg_toplevel::Request::UnsetFullscreen {}.into(),
                    ),
                    (
                        TOPLEVEL.host,
                        xdg_toplevel::Request::UnsetMaximized {}.into(),
                    ),
                    (aura_surface, zaura_surface::Request::UnsetSnap {}.into()),
                    (aura_toplevel, window_bounds(x, 0, width, 2160, OUTPUT)),
                    (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into()),
                ]);
                assert_eq!(sent, expected, "{action:?}");
            }
        }
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
            let mut ctx = focused_ctx();
            let aura_surface = 0x7000;
            ctx.shadow_table.insert_host_only_for_test(
                aura_surface,
                "zaura_surface",
                surface_version,
            );
            ctx.aura_surfaces.remember(SURFACE.host, aura_surface);

            let sent = place(&mut ctx, WindowLayoutAction::TopLeft);
            let unset_snap: (u32, AnyRequest) =
                (aura_surface, zaura_surface::Request::UnsetSnap {}.into());
            assert_eq!(
                sent.contains(&unset_snap),
                expect_unset_snap,
                "zaura_surface v{surface_version}"
            );
            let aura_toplevel = ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest)
                .expect("Aura toplevel");
            assert!(
                sent.contains(&(aura_toplevel, window_bounds(0, 0, 1920, 1080, OUTPUT))),
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
        let mut ctx = context();
        add_toplevel(&mut ctx);
        focus(&mut ctx);
        let panel = dock(&mut ctx, 1, 10, 30, PANEL);
        let external = dock(&mut ctx, 1, 11, 31, EXTERNAL);
        // The bounds request placing the toplevel on the right half.
        let place_right = |ctx: &mut Context| {
            let sent = place(ctx, WindowLayoutAction::Right);
            let aura_toplevel = ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest)
                .expect("an Aura toplevel");
            sent.into_iter()
                .find(|(sender, request)| {
                    *sender == aura_toplevel
                        && matches!(
                            request,
                            AnyRequest::ZauraToplevel(
                                zaura_toplevel::Request::SetWindowBounds { .. }
                            )
                        )
                })
                .expect("window bounds request")
                .1
        };

        move_surface(&mut ctx, SURFACE.host, None, external);
        assert_eq!(
            place_right(&mut ctx),
            window_bounds(-960, 0, 960, 1080, external)
        );
        move_surface(&mut ctx, SURFACE.host, Some(external), panel);
        assert_eq!(
            place_right(&mut ctx),
            window_bounds(640, 0, 640, 752, panel)
        );

        // Rotated to portrait (Exo's ROTATE_90 is `transform.270`), the
        // monitor's left edge moves to x = -1080.
        let rotated = Display {
            origin: (-1080, 0),
            transform: 3,
            ..EXTERNAL
        };
        send_metrics(&mut ctx, external, rotated);
        move_surface(&mut ctx, SURFACE.host, Some(panel), external);
        assert_eq!(
            place_right(&mut ctx),
            window_bounds(-540, 0, 540, 1920, external)
        );
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
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let aura_surface =
            ensure_host_zaura_surface(&mut ctx, SURFACE.guest).expect("Aura surface");
        assert!(apply_arc_identity(&mut ctx, SURFACE.guest, aura_surface));
        assert!(ctx
            .window_placement
            .arc_application_id_applied(SURFACE.guest));
        let application_id = ctx.window_placement.arc_application_id(SURFACE.guest);
        let sent = host_requests(&ctx);
        assert_eq!(
            sent,
            [
                (
                    SHELL,
                    zaura_shell::Request::GetAuraSurface {
                        id: aura_surface,
                        surface: SURFACE.host,
                    }
                    .into()
                ),
                (
                    aura_surface,
                    zaura_surface::Request::SetApplicationId { application_id }.into()
                ),
            ]
        );

        assert!(apply_arc_identity(&mut ctx, SURFACE.guest, aura_surface));
        assert_eq!(
            host_requests(&ctx),
            sent,
            "repeated placement must not duplicate ARC metadata"
        );
    }

    /// A bounds barrier holds the Aura toplevel's configure, and the
    /// `xdg_surface.configure` that ends its sequence if one arrived, until
    /// its `done`, which sends the guest the toplevel's configure first.
    #[test]
    fn bounds_barrier_holds_configures_until_done_and_flushes_toplevel_first() {
        for surface_serial in [None, Some(77)] {
            let mut ctx = toplevel_ctx();
            let (aura_toplevel, callback) = (650, 651);
            remember_aura_toplevel(&mut ctx, aura_toplevel);
            ctx.shadow_table
                .insert_host_only_for_test(callback, "wl_callback", 1);
            register_window_bounds_barrier_for_test(&mut ctx, callback, aura_toplevel);
            if let Some(serial) = surface_serial {
                ctx.last_sender_id = XDG_SURFACE.host;
                assert_eq!(
                    XdgSurfaceHandler::on_configure(&mut CompositorHandler, &mut ctx, serial),
                    Action::Drop
                );
            }
            aura_configure(&mut ctx, aura_toplevel, (800, 600), &[]);
            assert!(ctx.host_to_client_queue.is_empty(), "{surface_serial:?}");
            assert!(ctx.window_placement.has_held_configure(aura_toplevel));

            done(&mut ctx, callback);
            assert!(!ctx.window_placement.has_active_barrier(aura_toplevel));
            let mut expected = vec![toplevel_configure((800, 600), &[])];
            expected.extend(surface_serial.map(|serial| {
                let configure = xdg_surface::Event::Configure { serial };
                (XDG_SURFACE.guest, configure.into())
            }));
            assert_eq!(guest_events(&ctx), expected);
        }
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
            let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
            ctx.host_zaura_shell
                .as_mut()
                .expect("bound zaura_shell")
                .version = shell_version;
            attach_toplevel(&mut ctx, TOPLEVEL.guest, SURFACE.guest);
            let attached = ctx
                .window_placement
                .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest);
            assert_eq!(attached.is_some(), expect_toplevel, "v{shell_version}");
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
        let mut ctx = toplevel_ctx();
        ctx.host_zaura_shell
            .as_mut()
            .expect("bound zaura_shell")
            .version = REQ_RELEASE_AURA_TOPLEVEL_SINCE - 1;
        let aura_id = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("aura toplevel");
        ctx.client_to_host_queue.clear();

        release_toplevel(&mut ctx, TOPLEVEL.guest);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest),
            None
        );
        assert_eq!(ctx.shadow_table.get_host_interface(aura_id), None);
        assert!(ctx.shadow_table.is_retired_host(aura_id));
        assert!(!ctx.shadow_table.is_host_id_available(aura_id));
    }

    /// A malformed client that gives one wl_surface two xdg_surfaces ends
    /// only the destroyed xdg_surface's toplevel role.
    #[test]
    fn destroying_one_of_two_xdg_surfaces_ends_only_its_toplevel() {
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let (second_xdg_surface, second_toplevel) = (130, 140);
        ctx.shadow_table
            .insert_paired_for_test(second_xdg_surface, 230, "xdg_surface");
        ctx.shadow_table
            .insert_paired_for_test(second_toplevel, 240, "xdg_toplevel");
        ctx.xdg_roles.add_toplevel_role_for_test(
            second_xdg_surface,
            SURFACE.guest,
            second_toplevel,
        );
        let first_aura = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("toplevel");
        let second_aura = ensure_zaura_toplevel(&mut ctx, second_toplevel).expect("toplevel");
        ctx.client_to_host_queue.clear();

        ctx.guest_destroyed(XDG_SURFACE.guest);
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(first_aura, REQ_RELEASE_AURA_TOPLEVEL)]
        );
        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(TOPLEVEL.guest), None);
        assert_eq!(
            ctx.xdg_roles.wl_surface_of_toplevel(second_toplevel),
            Some(SURFACE.guest)
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
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let aura_toplevel = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("toplevel");
        let aura_surface = ensure_host_zaura_surface(&mut ctx, SURFACE.guest).expect("surface");
        ctx.client_to_host_queue.clear();

        ctx.last_sender_id = SURFACE.guest;
        assert_eq!(
            WlSurfaceHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [
                (aura_toplevel, REQ_RELEASE_AURA_TOPLEVEL),
                (aura_surface, REQ_RELEASE_AURA_SURFACE),
                (SURFACE.host, wl_surface::REQ_DESTROY),
            ]
        );
    }

    #[test]
    fn xdg_toplevel_destroy_reserves_aura_id_until_host_delete_id() {
        let mut ctx = toplevel_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = TOPLEVEL.guest;

        assert_eq!(
            XdgToplevelHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement
                .aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest),
            None
        );
        // Child-first: the Aura extension is released before its xdg role.
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [
                (aura_id, REQ_RELEASE_AURA_TOPLEVEL),
                (TOPLEVEL.host, xdg_toplevel::REQ_DESTROY),
            ]
        );
        assert!(ctx.shadow_table.is_retired_host(aura_id));
        assert!(!ctx.shadow_table.is_host_id_available(aura_id));

        ctx.last_sender_id = WL_DISPLAY_ID;
        assert_eq!(DisplayHandler.on_delete_id(&mut ctx, aura_id), Action::Drop);
        assert!(!ctx.shadow_table.is_retired_host(aura_id));
        assert!(ctx.shadow_table.is_host_id_available(aura_id));
    }

    #[test]
    fn window_bounds_barrier_is_queued_after_placement_request() {
        let mut ctx = toplevel_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("aura toplevel");
        ctx.client_to_host_queue.clear();

        queue_window_bounds_barrier(&mut ctx, aura_id);
        let callback = ctx
            .window_placement
            .active_barrier_for_toplevel(aura_id)
            .expect("an active barrier");
        assert_eq!(
            host_requests(&ctx),
            [(WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())]
        );
        assert_eq!(
            ctx.internal_callbacks.get(HostId(callback)),
            Some(InternalCallback::WindowBoundsBarrier {
                zaura_toplevel: aura_id
            })
        );
        assert_eq!(
            ctx.shadow_table.get_host_interface(callback),
            Some("wl_callback")
        );
    }

    /// An Aura configure reaches the guest as an `xdg_toplevel.configure` of
    /// the same size, without the Aura-only states (immersive 100,
    /// minimized 101) and the states the guest's version predates.
    #[test]
    fn aura_configure_is_translated_to_guest_xdg_configure() {
        let host_states = [1, 5, 8, 9, 10, 13, 14];
        for (guest_version, states, expected) in [
            (6, &[1, 2, 3, 4][..], &[1, 2, 3, 4][..]),
            // fullscreen, immersive, minimized and activated.
            (6, &[2, 100, 101, 4], &[2, 4]),
            (1, &host_states, &[1]),
            (2, &host_states, &[1, 5, 8]),
            (6, &host_states, &[1, 5, 8, 9]),
            (7, &host_states, &[1, 5, 8, 9, 10, 13]),
        ] {
            let mut ctx = toplevel_ctx();
            ctx.shadow_table
                .set_guest_version_for_test(TOPLEVEL.guest, guest_version);
            remember_aura_toplevel(&mut ctx, 77);
            aura_configure(&mut ctx, 77, (1920, 1080), &encode_states(states));
            assert_eq!(
                guest_events(&ctx),
                [toplevel_configure((1920, 1080), expected)],
                "v{guest_version} {states:?}"
            );
        }
    }

    #[test]
    fn aura_configure_drops_truncated_state_entry() {
        assert_eq!(guest_xdg_toplevel_states(&[2, 0, 0, 0, 4, 0], 6).len(), 4);
    }

    #[test]
    fn held_aura_configure_is_filtered_when_barrier_completes() {
        let mut ctx = toplevel_ctx();
        ctx.shadow_table
            .set_guest_version_for_test(TOPLEVEL.guest, 1);
        let aura_id = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("aura toplevel");
        queue_window_bounds_barrier(&mut ctx, aura_id);
        let callback = ctx
            .window_placement
            .active_barrier_for_toplevel(aura_id)
            .expect("active barrier");
        aura_configure(
            &mut ctx,
            aura_id,
            (800, 600),
            &encode_states(&[2, 5, 100, 101]),
        );
        assert!(ctx.host_to_client_queue.is_empty());

        done(&mut ctx, callback);
        assert_eq!(guest_events(&ctx), [toplevel_configure((800, 600), &[2])]);
    }

    #[test]
    fn aura_origin_change_is_dropped_with_or_without_a_bounds_barrier() {
        let mut ctx = toplevel_ctx();
        let aura_id = ensure_zaura_toplevel(&mut ctx, TOPLEVEL.guest).expect("aura toplevel");
        ctx.client_to_host_queue.clear();
        let origin_change = |ctx: &mut Context| {
            ctx.last_sender_id = aura_id;
            ZauraToplevelHandler::on_origin_change(&mut CompositorHandler, ctx, 123, 456)
        };

        assert_eq!(origin_change(&mut ctx), Action::Drop);
        queue_window_bounds_barrier(&mut ctx, aura_id);
        assert_eq!(origin_change(&mut ctx), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }
}
