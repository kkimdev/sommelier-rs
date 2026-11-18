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

//! The ARC placement backend: `zaura_toplevel.set_window_bounds` on a
//! surface whose Aura application ID is an ARC policy identity.
//!
//! Exo accepts `set_window_bounds` only from a surface it classifies as an
//! ARC window, so while this backend is active every surface's Aura
//! surface gets a per-surface `org.chromium.arc.<task_id>` identity instead
//! of the guest's application ID (see
//! `sommelier/docs/ARC_TASK_AND_SESSION_IDS.md`). The backend owns the
//! placement Aura toplevel of each XDG toplevel, that identity, the
//! host-stream barrier that orders each bounds request, the configures held
//! behind that barrier, and the translation of `zaura_toplevel.configure`
//! into the guest's `xdg_toplevel.configure`. Its per-connection state is
//! [`crate::state::ArcPlacementState`]; the boundary in
//! `crate::handler::window_placement` calls in here only while it is the
//! connection's backend, so every function may assume it is.

use super::{PlacementSkip, PlacementTarget, PLACEMENT_SHELL_SINCE};
use crate::handler::aura_objects::{
    self, aura_object_version, ensure_host_zaura_surface, AuraChild,
};
use crate::handler::callback::queue_internal_sync;
use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE;
use crate::protocols::aura_shell::zaura_surface::{self, REQ_UNSET_SNAP_SINCE};
use crate::protocols::aura_shell::zaura_toplevel::{
    self, REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE, REQ_SET_WINDOW_BOUNDS_SINCE,
};
use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel};
use crate::state::{ArcPlacementState, Context, InternalCallback};
use crate::wire::queue_or_drop;

// Placement creates an Aura toplevel only to send
// `set_supports_screen_coordinates` and `set_window_bounds`. Every Aura
// child takes its shell's version, so the shell placement requires must
// have both; this keeps that true if the vendored protocol changes.
const _: () = assert!(
    REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_WINDOW_BOUNDS_SINCE <= PLACEMENT_SHELL_SINCE
);

/// The connection's ARC state. The boundary calls this module only while
/// the ARC backend is active.
fn state(ctx: &Context) -> &ArcPlacementState {
    ctx.window_placement
        .arc()
        .expect("the ARC backend is the connection's backend")
}

/// [`state`], mutably.
fn state_mut(ctx: &mut Context) -> &mut ArcPlacementState {
    ctx.window_placement
        .arc_mut()
        .expect("the ARC backend is the connection's backend")
}

// ── Placing the focused window ────────────────────────────────────────────

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

/// Move `target` with `zaura_toplevel.set_window_bounds`.
///
/// Attaches the toplevel's Aura toplevel and applies the surface's ARC
/// identity if they are missing, then queues the state reset, the bounds
/// request, and the barrier that holds configures until the host has
/// applied the bounds.
///
/// # Errors
///
/// [`PlacementSkip::NoAuraToplevel`] or [`PlacementSkip::NoAuraSurface`];
/// a skip queues no placement request.
pub(super) fn apply_bounds(
    ctx: &mut Context,
    target: PlacementTarget,
) -> Result<(), PlacementSkip> {
    let PlacementTarget {
        action,
        xdg_toplevel_guest_id: guest_xdg_toplevel_id,
        wl_surface_guest_id: guest_wl_surface_id,
        output_host_id,
        bounds,
    } = target;
    // `record_xdg_toplevel` records only mapped toplevels, and every path
    // that unmaps one removes its record first, so the lookup cannot miss.
    let host_xdg_toplevel_id = ctx.shadow_table.get_host_id(guest_xdg_toplevel_id);
    debug_assert!(
        host_xdg_toplevel_id.is_some(),
        "recorded xdg_toplevel {guest_xdg_toplevel_id} has no host object"
    );
    let host_xdg_toplevel_id = host_xdg_toplevel_id.ok_or(PlacementSkip::NoFocusedToplevel)?;
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

/// Send the surface's ARC identity to its Aura surface in place of the
/// application ID the guest set (through XDG or GTK).
///
/// `zaura_surface.set_application_id` is also what Exo checks before it
/// accepts `zaura_toplevel.set_window_bounds`, so while the ARC backend is
/// active the surface's ARC identity replaces the guest's ID (see
/// [`apply_arc_identity`]).
pub(super) fn replace_application_id(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
    zaura_surface_host_id: u32,
    native_application_id: &str,
) {
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
    if state(ctx).arc_application_id_applied(wl_surface_guest_id) {
        return true;
    }
    let Some(application_id) = state_mut(ctx).arc_application_id(wl_surface_guest_id) else {
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
    state_mut(ctx).mark_arc_application_id_applied(wl_surface_guest_id);
    true
}

/// Create or reuse the internal Aura toplevel associated with a guest
/// `xdg_toplevel`. The object is needed for screen-coordinate bounds requests.
/// Private so that every caller goes through the policy and identity gate
/// in [`attach_placement_toplevel`].
fn ensure_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) -> Option<u32> {
    if let Some(existing_id) = state(ctx).aura_toplevel_for_xdg_toplevel(xdg_toplevel_guest_id) {
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
    state_mut(ctx).remember_aura_toplevel(xdg_toplevel_guest_id, zaura_toplevel_host_id);
    Some(zaura_toplevel_host_id)
}

/// Attach the host Aura toplevel that placement uses for an XDG toplevel.
///
/// Exo authorizes bounds requests only for a surface carrying an ARC policy
/// identity. The identity is allocated first so exhaustion fails
/// closed: no Aura child is created that placement could never use. Every
/// path that creates a placement toplevel ([`attach_toplevel`],
/// [`attach_recorded_toplevels`], and placement itself) goes through here.
///
/// # Returns
///
/// The host `zaura_toplevel` ID, or `None` when the identity is unavailable
/// or the host has no usable `zaura_shell` yet.
fn attach_placement_toplevel(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    if state_mut(ctx)
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
pub(super) fn attach_toplevel(
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
pub(super) fn attach_recorded_toplevels(ctx: &mut Context) {
    for (xdg_toplevel_guest_id, wl_surface_guest_id) in ctx.xdg_roles.toplevels() {
        attach_toplevel(ctx, xdg_toplevel_guest_id, wl_surface_guest_id);
    }
}

/// Release the placement Aura toplevel of a guest XDG toplevel.
///
/// Called whenever the XDG role ends (see `XdgRoles`' remove methods). Does
/// nothing when the toplevel has no Aura toplevel. A barrier still in flight
/// stays pending until its `done` but no longer gates anything.
pub(super) fn release_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    if let Some(zaura_toplevel_host_id) = state_mut(ctx).take_aura_toplevel(xdg_toplevel_guest_id) {
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
    state_mut(ctx).register_barrier(callback.0, zaura_toplevel_host_id);
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
        crate::state::HostId(callback_host_id),
        InternalCallback::WindowBoundsBarrier {
            zaura_toplevel: zaura_toplevel_host_id,
        },
    );
    state_mut(ctx).register_barrier(callback_host_id, zaura_toplevel_host_id);
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
pub(super) fn complete_barrier(
    ctx: &mut Context,
    callback_host_id: u32,
    zaura_toplevel_host_id: u32,
) {
    let held = state_mut(ctx).complete_barrier(callback_host_id, zaura_toplevel_host_id);
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
pub(super) fn hold_surface_configure(
    ctx: &mut Context,
    guest_xdg_surface_id: u32,
    serial: u32,
) -> bool {
    let Some(guest_xdg_toplevel_id) = ctx.xdg_roles.toplevel_of_xdg_surface(guest_xdg_surface_id)
    else {
        return false;
    };
    let Some(zaura_toplevel_host_id) =
        state(ctx).aura_toplevel_for_xdg_toplevel(guest_xdg_toplevel_id)
    else {
        return false;
    };
    state_mut(ctx).hold_surface_configure(
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
pub(super) fn hold_toplevel_configure(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u8],
) -> bool {
    let Some(zaura_toplevel_host_id) =
        state(ctx).aura_toplevel_for_xdg_toplevel(guest_xdg_toplevel_id)
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
    state_mut(ctx).hold_configure(
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
pub(super) fn translate_aura_configure(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    states: &[u8],
) {
    let Some(guest_xdg_toplevel_id) =
        state(ctx).xdg_toplevel_for_aura_toplevel(zaura_toplevel_host_id)
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
        state(ctx).has_active_barrier(zaura_toplevel_host_id)
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
    use super::super::fixture::*;
    use super::*;
    use crate::arc_task_ids::ArcIdAllocator;
    use crate::handler::compositor::CompositorHandler;
    use crate::handler::display::DisplayHandler;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_output::EVT_INSETS_SINCE;
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
    use crate::protocols::AnyRequest;
    use crate::state::HostId;
    use crate::state::ARC_APPLICATION_ID_PREFIX;
    use crate::test_support::queued_headers;
    use crate::window_layout::WindowLayoutAction;
    use crate::wire::{Action, WL_DISPLAY_ID};

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
                    .arc()
                    .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest))
                    .expect("an Aura toplevel");
                let aura_surface = ctx
                    .aura_surfaces
                    .get(SURFACE.host)
                    .expect("an Aura surface");
                let application_id = ctx
                    .window_placement
                    .arc_mut()
                    .and_then(|arc| arc.arc_application_id(SURFACE.guest));
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
                    .arc()
                    .and_then(|arc| arc.active_barrier_for_toplevel(aura_toplevel))
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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest))
                .expect("Aura toplevel");
            assert!(
                sent.contains(&(aura_toplevel, window_bounds(0, 0, 1920, 1080, OUTPUT))),
                "the bounds request does not depend on unset_snap"
            );
        }
    }

    #[test]
    fn arc_policy_identity_is_unique_and_stable_per_surface() {
        let mut ctx = Context::with_placement_for_test(PlacementConfig::arc_bounds_for_test(""));
        let first = ctx
            .window_placement
            .arc_mut()
            .and_then(|arc| arc.arc_application_id(10))
            .expect("first identity");
        assert_eq!(
            ctx.window_placement
                .arc_mut()
                .and_then(|arc| arc.arc_application_id(10))
                .as_deref(),
            Some(first.as_str())
        );
        let second = ctx
            .window_placement
            .arc_mut()
            .and_then(|arc| arc.arc_application_id(11))
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
        assert!(ctx
            .window_placement
            .arc_mut()
            .and_then(|arc| arc.arc_application_id(10))
            .is_none());
        assert!(!ctx
            .window_placement
            .arc()
            .is_some_and(|arc| arc.has_arc_application_ids()));
    }

    #[test]
    fn arc_policy_identity_is_applied_before_placement_without_app_id() {
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        let aura_surface =
            ensure_host_zaura_surface(&mut ctx, SURFACE.guest).expect("Aura surface");
        assert!(apply_arc_identity(&mut ctx, SURFACE.guest, aura_surface));
        assert!(ctx
            .window_placement
            .arc()
            .is_some_and(|arc| arc.arc_application_id_applied(SURFACE.guest)));
        let application_id = ctx
            .window_placement
            .arc_mut()
            .and_then(|arc| arc.arc_application_id(SURFACE.guest));
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
            assert!(ctx
                .window_placement
                .arc()
                .is_some_and(|arc| arc.has_held_configure(aura_toplevel)));

            done(&mut ctx, callback);
            assert!(!ctx
                .window_placement
                .arc()
                .is_some_and(|arc| arc.has_active_barrier(aura_toplevel)));
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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest));
            assert_eq!(attached.is_some(), expect_toplevel, "v{shell_version}");
            assert_eq!(
                ctx.client_to_host_queue.is_empty(),
                !expect_toplevel,
                "zaura_shell v{shell_version}"
            );
        }
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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest)),
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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(second_toplevel)),
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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest)),
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
            .arc()
            .and_then(|arc| arc.active_barrier_for_toplevel(aura_id))
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
            .arc()
            .and_then(|arc| arc.active_barrier_for_toplevel(aura_id))
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
