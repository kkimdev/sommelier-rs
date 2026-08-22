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

//! Protocol adapter for compositor-owned window placement.
//!
//! [`crate::state::WindowPlacementState`] owns placement decisions and
//! lifecycle metadata. This module owns only the wire-facing operations that
//! create Aura children, serialize placement requests, and queue asynchronous
//! barrier cleanup. Keeping those concerns here prevents keyboard, GTK, and
//! XDG handlers from each implementing a slightly different protocol sequence.

use crate::protocols::aura_shell::zaura_shell::{
    REQ_GET_AURA_SURFACE, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
};
use crate::protocols::aura_shell::zaura_surface::{
    REQ_SET_APPLICATION_ID, REQ_SET_PARENT, REQ_UNSET_SNAP,
};
use crate::protocols::aura_shell::zaura_toplevel::{
    REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL, REQ_SET_WINDOW_BOUNDS,
};
use crate::protocols::wayland::wl_display::REQ_SYNC;
use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
use crate::state::{
    Context, PlacementBarrierCleanup, PlacementTarget, WindowPlacementGeometry, WindowPlacementPlan,
};
use crate::window_shortcuts::WindowShortcut;
use crate::wire::MessageBuilder;

/// Return whether a string fits in one Wayland message.
///
/// Wayland strings carry a u32 byte length including the trailing NUL and are
/// padded to four bytes. The complete message length is limited to 16 bits.
pub(crate) fn wayland_string_fits_message(value: &str) -> bool {
    let Some(length_with_nul) = value.len().checked_add(1) else {
        return false;
    };
    let Some(padded_length) = length_with_nul.checked_add(3).map(|len| len & !3) else {
        return false;
    };
    8usize
        .checked_add(4)
        .and_then(|header_and_length| header_and_length.checked_add(padded_length))
        .is_some_and(|total| total <= 0xffff)
}

/// Create or reuse the host Aura surface for a guest `wl_surface`.
///
/// XDG and GTK metadata must share one Aura child. The placement state rejects
/// conflicting associations, so a second child cannot silently orphan the
/// first one.
pub(crate) fn ensure_host_zaura_surface(
    ctx: &mut Context,
    wl_surface_guest_id: u32,
) -> Option<u32> {
    let wl_surface_host_id = ctx.shadow_table.get_host_id(wl_surface_guest_id)?;
    if let Some(zaura_surface_host_id) = ctx
        .window_placement
        .aura_surface_for_wl_surface(wl_surface_host_id)
    {
        return Some(zaura_surface_host_id);
    }

    let zaura_shell_host_id = ctx.window_placement.aura_shell_id()?;
    let zaura_surface_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        zaura_surface_host_id,
        "zaura_surface".to_string(),
        ctx.window_placement.aura_shell_version(),
    );

    let mut builder = MessageBuilder::new();
    builder.write_u32(zaura_surface_host_id);
    builder.write_u32(wl_surface_host_id);
    let Ok(message) = builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_SURFACE) else {
        log::warn!(
            "Unable to encode Aura surface request for wl_surface {}",
            wl_surface_guest_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_surface_host_id);
        return None;
    };

    if !ctx
        .window_placement
        .remember_aura_surface(wl_surface_host_id, zaura_surface_host_id)
    {
        log::error!(
            "Refusing to replace Aura surface mapping for host wl_surface {}",
            wl_surface_host_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_surface_host_id);
        return None;
    }
    ctx.client_to_host_queue.push((message, Vec::new()));
    Some(zaura_surface_host_id)
}

/// Queue a nullable `zaura_surface.set_parent` request.
///
/// `parent_id = None` is the protocol's explicit unparent operation. Both the
/// shortcut path and the asynchronous barrier path use this serializer.
fn build_zaura_surface_parent(
    ctx: &mut Context,
    zaura_surface_id: u32,
    parent_id: Option<u32>,
    x: i32,
    y: i32,
) -> Option<Vec<u8>> {
    // A placement barrier may outlive the guest surface that created it.
    // Do not emit a late request to an Aura object whose release has already
    // retired its dispatch metadata and reserved its host ID for delete_id.
    if !ctx
        .shadow_table
        .host_object_matches(zaura_surface_id, "zaura_surface")
    {
        log::debug!(
            "Skipping parent update for released zaura_surface {}",
            zaura_surface_id
        );
        return None;
    }
    let Some(version) = ctx.shadow_table.host_object_version(zaura_surface_id) else {
        log::debug!(
            "Skipping parent update for zaura_surface {} without a negotiated version",
            zaura_surface_id
        );
        return None;
    };
    if version < 2 {
        return None;
    }

    let mut builder = MessageBuilder::new();
    builder.write_u32(parent_id.unwrap_or(0));
    builder.write_i32(x);
    builder.write_i32(y);
    match builder.try_build_message(zaura_surface_id, REQ_SET_PARENT) {
        Ok(message) => Some(message),
        Err(error) => {
            log::warn!(
                "Unable to encode Aura parent update for zaura_surface {}: {}",
                zaura_surface_id,
                error
            );
            None
        }
    }
}

/// Queue a nullable `zaura_surface.set_parent` request.
///
/// `parent_id = None` is the protocol's explicit unparent operation. Both the
/// shortcut path and the asynchronous barrier path use this serializer.
pub(crate) fn queue_zaura_surface_parent(
    ctx: &mut Context,
    zaura_surface_id: u32,
    parent_id: Option<u32>,
    x: i32,
    y: i32,
) -> bool {
    let Some(message) = build_zaura_surface_parent(ctx, zaura_surface_id, parent_id, x, y) else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

/// Queue one placement cleanup operation after its host barrier completes.
pub(crate) fn queue_barrier_cleanup(ctx: &mut Context, cleanup: &PlacementBarrierCleanup) -> bool {
    match cleanup {
        PlacementBarrierCleanup::Unparent { zaura_surface_id } => {
            queue_zaura_surface_parent(ctx, *zaura_surface_id, None, 0, 0)
        }
        PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
            zaura_surface_id,
            wl_surface_guest_id,
        } => {
            // The nullable parent request is intentionally queued first. It
            // releases any host-side relationship established while the ARC
            // policy was active before the native Guest OS identity is
            // restored. Resolve the identity at completion time rather than
            // using a barrier-time snapshot: an application can send a newer
            // set_app_id while the sync is in flight.
            let unparented = queue_zaura_surface_parent(ctx, *zaura_surface_id, None, 0, 0);
            if !unparented {
                // The surface may have been released while the barrier was
                // pending. Do not emit a native-ID restore to the same stale
                // host object after the null-parent request was rejected.
                return false;
            }
            let Some(application_id) = ctx
                .window_placement
                .native_application_id(*wl_surface_guest_id)
            else {
                log::warn!(
                    "Cannot restore native application ID for wl_surface {} after \
                     transient ARC placement",
                    wl_surface_guest_id
                );
                return false;
            };
            let restored = queue_zaura_application_id(ctx, *zaura_surface_id, &application_id);
            unparented && restored
        }
    }
}

/// Queue one complete placement operation and its ordered host barrier.
///
/// The keyboard handler has already resolved the focused role and the state
/// owner has validated the geometry. This adapter owns the remaining wire
/// contract:
///
/// ```text
/// [transient ARC identity]
/// unset fullscreen/maximized/snap
/// set_window_bounds(...)
/// [self-parent position probe]
/// wl_display.sync(...)
/// ```
///
/// All messages are built before any are appended to the connection queue.
/// That keeps an encoding or lifecycle failure from leaving a partial
/// placement sequence in front of the guest. The barrier callback is
/// registered before the batch is published so its cleanup metadata and wire
/// request become one state transition.
pub(crate) fn queue_window_placement(ctx: &mut Context, plan: &WindowPlacementPlan) -> bool {
    let mut messages = Vec::with_capacity(6);
    let target = plan.target;

    if !plan.is_well_formed() {
        log::warn!(
            "Skipping malformed window-placement plan for xdg_toplevel {} \
             and zaura_surface {}",
            target.guest_xdg_toplevel_id,
            target.zaura_surface_host_id,
        );
        return false;
    }

    // A plan is prepared before the host event loop can process teardown.
    // Revalidate every association and host interface immediately before
    // serialization so a delayed shortcut cannot pair IDs from different
    // windows or send requests to an object whose destructor is in flight.
    if !ctx.window_placement.target_is_current(target)
        || ctx.shadow_table.get_host_id(target.guest_xdg_toplevel_id)
            != Some(target.host_xdg_toplevel_id)
        || ctx.shadow_table.get_host_id(target.wl_surface_guest_id)
            != Some(target.wl_surface_host_id)
        || !ctx
            .shadow_table
            .host_object_matches(target.host_xdg_toplevel_id, "xdg_toplevel")
        || !ctx
            .shadow_table
            .host_object_matches(target.zaura_toplevel_host_id, "zaura_toplevel")
        || !ctx
            .shadow_table
            .host_object_matches(target.zaura_surface_host_id, "zaura_surface")
    {
        log::debug!(
            "Skipping stale window-placement target xdg={} wl_surface={} \
             zaura_toplevel={} zaura_surface={}",
            target.guest_xdg_toplevel_id,
            target.wl_surface_guest_id,
            target.zaura_toplevel_host_id,
            target.zaura_surface_host_id,
        );
        return false;
    }

    if let Some(identity) = &plan.transient_arc_identity {
        let Some(message) = build_zaura_application_id(
            ctx,
            target.zaura_surface_host_id,
            &identity.arc_application_id,
        ) else {
            log::warn!(
                "Unable to install transient ARC ID on zaura_surface {}",
                target.zaura_surface_host_id
            );
            return false;
        };
        messages.push((message, Vec::new()));
    }

    messages.push((
        MessageBuilder::new().build_message(target.host_xdg_toplevel_id, REQ_UNSET_FULLSCREEN),
        Vec::new(),
    ));
    messages.push((
        MessageBuilder::new().build_message(target.host_xdg_toplevel_id, REQ_UNSET_MAXIMIZED),
        Vec::new(),
    ));
    messages.push((
        MessageBuilder::new().build_message(target.zaura_surface_host_id, REQ_UNSET_SNAP),
        Vec::new(),
    ));

    let (request_x, request_y) = match plan.geometry {
        WindowPlacementGeometry::Bounds => (plan.bounds.0, plan.bounds.1),
        WindowPlacementGeometry::SelfParent { current_origin, .. } => current_origin,
    };
    let mut bounds_builder = MessageBuilder::new();
    bounds_builder.write_i32(request_x);
    bounds_builder.write_i32(request_y);
    bounds_builder.write_i32(plan.bounds.2);
    bounds_builder.write_i32(plan.bounds.3);
    bounds_builder.write_u32(plan.output_host_id);
    messages.push((
        bounds_builder.build_message(target.zaura_toplevel_host_id, REQ_SET_WINDOW_BOUNDS),
        Vec::new(),
    ));

    if let WindowPlacementGeometry::SelfParent {
        relative_position: (relative_x, relative_y),
        ..
    } = plan.geometry
    {
        let Some(message) = build_zaura_surface_parent(
            ctx,
            target.zaura_surface_host_id,
            Some(target.zaura_surface_host_id),
            relative_x,
            relative_y,
        ) else {
            log::warn!(
                "Unable to queue self-parent request for zaura_surface {}",
                target.zaura_surface_host_id
            );
            return false;
        };
        messages.push((message, Vec::new()));
    }

    let Some(barrier_message) = build_and_register_placement_barrier(
        ctx,
        target.zaura_toplevel_host_id,
        plan.barrier_cleanup.clone(),
    ) else {
        return false;
    };
    messages.push((barrier_message, Vec::new()));

    ctx.client_to_host_queue.extend(messages);
    if !ctx.window_placement.commit_placement_plan(plan) {
        log::warn!(
            "Placement target for zaura_toplevel {} was released before \
             its queued state transition could be committed",
            target.zaura_toplevel_host_id
        );
    }
    true
}

/// Resolve one focused XDG role into a validated placement plan and publish
/// its complete host wire sequence.
///
/// Keyboard handling owns focus and key ownership. Once it has identified the
/// focused guest role, all placement-specific lookup, capability validation,
/// planning, serialization, and lifecycle commit belong here. This keeps the
/// keyboard handler from pairing independent XDG/Aura IDs or choosing a
/// backend-specific protocol sequence.
pub(crate) fn apply_window_shortcut(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    guest_wl_surface_id: u32,
    shortcut: WindowShortcut,
) -> bool {
    let Some(host_xdg_toplevel_id) = ctx.shadow_table.get_host_id(guest_xdg_toplevel_id) else {
        log::debug!(
            "window shortcut {:?} ignored: guest xdg_toplevel {} has no host mapping",
            shortcut,
            guest_xdg_toplevel_id
        );
        return false;
    };
    let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(guest_wl_surface_id) else {
        log::debug!(
            "window shortcut {:?} ignored: guest wl_surface {} has no host mapping",
            shortcut,
            guest_wl_surface_id
        );
        return false;
    };
    let Some(zaura_toplevel_host_id) = ensure_zaura_toplevel(ctx, guest_xdg_toplevel_id) else {
        log::debug!(
            "window shortcut {:?} ignored: no zaura_toplevel for xdg_toplevel {}",
            shortcut,
            guest_xdg_toplevel_id
        );
        return false;
    };
    let Some(zaura_surface_host_id) = ensure_host_zaura_surface(ctx, guest_wl_surface_id) else {
        log::debug!(
            "window shortcut {:?} ignored: no zaura_surface for wl_surface {}",
            shortcut,
            guest_wl_surface_id
        );
        return false;
    };
    let zaura_surface_version = ctx
        .shadow_table
        .host_object_version(zaura_surface_host_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    let target = PlacementTarget {
        guest_xdg_toplevel_id,
        wl_surface_guest_id: guest_wl_surface_id,
        wl_surface_host_id,
        host_xdg_toplevel_id,
        zaura_toplevel_host_id,
        zaura_surface_host_id,
        zaura_surface_version,
    };
    let plan = match ctx
        .window_placement
        .prepare_placement(shortcut.rect, target)
    {
        Ok(plan) => plan,
        Err(error) => {
            if matches!(
                error,
                crate::state::WindowPlacementPlanError::UnsupportedSurfaceVersion
            ) {
                let required_version = if ctx.window_placement.uses_transient_arc_id() {
                    5
                } else {
                    2
                };
                let required_capability = if ctx.window_placement.uses_transient_arc_id() {
                    "set_application_id plus nullable set_parent"
                } else {
                    "nullable set_parent"
                };
                log::warn!(
                    "window layout {:?}: placement requires zaura_surface v{} \
                     ({}), got v{}",
                    shortcut,
                    required_version,
                    required_capability,
                    zaura_surface_version
                );
            } else {
                log::debug!(
                    "window layout {:?} ignored for xdg_toplevel {}: {:?}",
                    shortcut,
                    guest_xdg_toplevel_id,
                    error
                );
            }
            return error.consumes_shortcut();
        }
    };

    if !queue_window_placement(ctx, &plan) {
        log::warn!(
            "window layout {:?}: failed to queue placement sequence for zaura_toplevel {}",
            shortcut,
            zaura_toplevel_host_id
        );
        return false;
    }
    if plan.barrier_cleanup.is_some() {
        log::debug!(
            "window layout {:?}: placement cleanup is deferred until sync.done",
            shortcut,
        );
    }
    if let WindowPlacementGeometry::SelfParent {
        current_origin: (origin_x, origin_y),
        relative_position: (relative_x, relative_y),
    } = plan.geometry
    {
        log::warn!(
            "window layout {:?}: experimental self-parent probe sent for zaura_surface={} \
             target_screen_position=({}, {}) origin=({}, {}) \
             relative_position=({}, {})",
            shortcut,
            zaura_surface_host_id,
            plan.bounds.0,
            plan.bounds.1,
            origin_x,
            origin_y,
            relative_x,
            relative_y
        );
        log::info!(
            "window layout {:?}: self-parent resize-then-move sent \
             current_origin=({}, {}) target_screen_bounds=({}, {}, {}, {}) output={}",
            shortcut,
            origin_x,
            origin_y,
            plan.bounds.0,
            plan.bounds.1,
            plan.bounds.2,
            plan.bounds.3,
            plan.output_host_id
        );
    } else {
        log::info!(
            "window layout {:?}: xdg_toplevel={} zaura_toplevel={} bounds=({}, {}, {}, {}) output={}",
            shortcut,
            guest_xdg_toplevel_id,
            zaura_toplevel_host_id,
            plan.bounds.0,
            plan.bounds.1,
            plan.bounds.2,
            plan.bounds.3,
            plan.output_host_id
        );
    }
    true
}

/// Build and register the host-stream barrier for one placement generation.
///
/// Registration happens before the message is returned so callback cleanup
/// cannot race a batch that has not yet been published. The caller owns the
/// final queue insertion.
fn build_and_register_placement_barrier(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
) -> Option<Vec<u8>> {
    let callback_host_id = ctx.shadow_table.allocate_host_id();
    let mut barrier_builder = MessageBuilder::new();
    barrier_builder.write_u32(callback_host_id);
    let Ok(barrier_message) = barrier_builder.try_build_message(1, REQ_SYNC) else {
        log::warn!(
            "Unable to encode window-placement barrier for zaura_toplevel {}",
            zaura_toplevel_id,
        );
        return None;
    };
    ctx.shadow_table.track_host_interface_with_version(
        callback_host_id,
        "wl_callback".to_string(),
        1,
    );
    if !ctx
        .window_placement
        .register_barrier(callback_host_id, zaura_toplevel_id, cleanup)
    {
        log::error!(
            "Refusing to register window-placement barrier callback {}",
            callback_host_id
        );
        ctx.shadow_table.remove_host_interface(callback_host_id);
        return None;
    }
    Some(barrier_message)
}

/// Queue a nullable Aura application ID update for a live host surface.
fn build_zaura_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    application_id: &str,
) -> Option<Vec<u8>> {
    let version = ctx
        .shadow_table
        .host_object_version(zaura_surface_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version < 5 || !wayland_string_fits_message(application_id) {
        return None;
    }

    let mut builder = MessageBuilder::new();
    builder.write_nullable_string(Some(application_id));
    match builder.try_build_message(zaura_surface_id, REQ_SET_APPLICATION_ID) {
        Ok(message) => Some(message),
        Err(error) => {
            log::warn!(
                "Unable to encode Aura application ID for zaura_surface {}: {}",
                zaura_surface_id,
                error
            );
            None
        }
    }
}

/// Queue a nullable Aura application ID update for a live host surface.
pub(crate) fn queue_zaura_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    application_id: &str,
) -> bool {
    let Some(message) = build_zaura_application_id(ctx, zaura_surface_id, application_id) else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

/// Queue the Aura identity selected by the placement policy.
///
/// `persistent-native-shell` queues ARC authorization followed by the native
/// shell identity. The host currently keeps the ARC policy properties after
/// the second update, so that mode remains an explicit experiment.
pub(crate) fn queue_policy_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    wl_surface_guest_id: u32,
    native_application_id: &str,
) -> bool {
    if !ctx.window_placement.uses_arc_policy() {
        return queue_zaura_application_id(ctx, zaura_surface_id, native_application_id);
    }

    let Some(arc_application_id) = ctx
        .window_placement
        .arc_policy_application_id(wl_surface_guest_id)
    else {
        log::warn!(
            "ARC policy has no allocated task ID for wl_surface {}",
            wl_surface_guest_id
        );
        return false;
    };

    match ctx.window_placement.arc_id_lifetime() {
        crate::state::WindowArcIdLifetime::Persistent => {
            queue_zaura_application_id(ctx, zaura_surface_id, &arc_application_id)
        }
        crate::state::WindowArcIdLifetime::Transient => {
            queue_zaura_application_id(ctx, zaura_surface_id, native_application_id)
        }
        crate::state::WindowArcIdLifetime::PersistentNativeShell => {
            // Build both messages before queueing either one. Otherwise a
            // malformed/oversized native identity could leave the ARC
            // authorization request in the host stream without its intended
            // native-shell follow-up.
            let Some(arc_message) =
                build_zaura_application_id(ctx, zaura_surface_id, &arc_application_id)
            else {
                return false;
            };
            let Some(native_message) =
                build_zaura_application_id(ctx, zaura_surface_id, native_application_id)
            else {
                return false;
            };
            ctx.client_to_host_queue.push((arc_message, Vec::new()));
            ctx.client_to_host_queue.push((native_message, Vec::new()));
            true
        }
    }
}

/// Create or reuse the internal Aura toplevel for a guest `xdg_toplevel`.
pub(crate) fn ensure_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) -> Option<u32> {
    if let Some(existing_id) = ctx
        .window_placement
        .aura_toplevel_for_xdg_toplevel(xdg_toplevel_guest_id)
    {
        return Some(existing_id);
    }

    let xdg_toplevel_host_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id)?;
    let zaura_shell_host_id = ctx.window_placement.aura_shell_id()?;
    if ctx.window_placement.aura_shell_version() < 29 {
        return None;
    }

    let zaura_toplevel_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        zaura_toplevel_host_id,
        "zaura_toplevel".to_string(),
        ctx.window_placement.aura_shell_version(),
    );

    let mut builder = MessageBuilder::new();
    builder.write_u32(zaura_toplevel_host_id);
    builder.write_u32(xdg_toplevel_host_id);
    let Ok(get_message) =
        builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL)
    else {
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    };
    let Ok(coordinate_message) = MessageBuilder::new().try_build_message(
        zaura_toplevel_host_id,
        crate::protocols::aura_shell::zaura_toplevel::REQ_SET_SUPPORTS_SCREEN_COORDINATES,
    ) else {
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    };
    if !ctx
        .window_placement
        .remember_aura_toplevel(xdg_toplevel_guest_id, zaura_toplevel_host_id)
    {
        log::error!(
            "Refusing to replace existing Aura toplevel mapping for xdg_toplevel {}",
            xdg_toplevel_guest_id
        );
        ctx.shadow_table
            .remove_host_interface(zaura_toplevel_host_id);
        return None;
    }
    ctx.client_to_host_queue.push((get_message, Vec::new()));
    ctx.client_to_host_queue
        .push((coordinate_message, Vec::new()));
    Some(zaura_toplevel_host_id)
}

/// Release the Aura toplevel owned by a guest XDG role.
pub(crate) fn release_zaura_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    let Some(zaura_toplevel_host_id) = ctx
        .window_placement
        .take_aura_toplevel(xdg_toplevel_guest_id)
    else {
        return;
    };
    let version = ctx
        .shadow_table
        .host_object_version(zaura_toplevel_host_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version >= 38 {
        let message =
            MessageBuilder::new().build_message(zaura_toplevel_host_id, REQ_RELEASE_AURA_TOPLEVEL);
        ctx.client_to_host_queue.push((message, Vec::new()));
        ctx.shadow_table
            .mark_pending_destroy_host(zaura_toplevel_host_id);
    } else {
        ctx.shadow_table
            .retire_host_interface(zaura_toplevel_host_id);
    }
}

/// Queue a host `wl_display.sync` after a placement request.
///
/// The callback is an ordered host-stream barrier. Cleanup metadata is retained
/// in placement state until `wl_callback.done`; stale callbacks complete their
/// own lifecycle but cannot clean up a newer placement.
#[cfg(test)]
pub(crate) fn queue_window_placement_barrier(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
) -> bool {
    let Some(message) = build_and_register_placement_barrier(ctx, zaura_toplevel_host_id, cleanup)
    else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

#[cfg(test)]
mod tests {
    use super::{
        queue_barrier_cleanup, queue_window_placement, queue_zaura_surface_parent,
        wayland_string_fits_message,
    };
    use crate::protocols::aura_shell::zaura_surface::{
        REQ_SET_APPLICATION_ID, REQ_SET_PARENT, REQ_UNSET_SNAP,
    };
    use crate::protocols::aura_shell::zaura_toplevel::REQ_SET_WINDOW_BOUNDS;
    use crate::protocols::wayland::wl_display::REQ_SYNC;
    use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
    use crate::state::{
        Context, PlacementBarrierCleanup, PlacementTarget, TransientArcIdentity,
        WindowPlacementGeometry, WindowPlacementPlan,
    };

    fn opcode(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u16 {
        (u32::from_ne_bytes(message.0[4..8].try_into().unwrap()) & 0xffff) as u16
    }

    fn sender(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u32 {
        u32::from_ne_bytes(message.0[0..4].try_into().unwrap())
    }

    fn application_id(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> &str {
        let payload = &message.0[8..];
        let length = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        assert!(length > 0, "Wayland nullable string must include its NUL");
        std::str::from_utf8(&payload[4..4 + length - 1]).expect("valid application ID")
    }

    fn track_placement_objects(
        ctx: &mut Context,
        host_xdg_toplevel_id: u32,
        zaura_toplevel_id: u32,
        zaura_surface_id: u32,
        zaura_surface_version: u32,
    ) {
        ctx.shadow_table.map_id(10, host_xdg_toplevel_id);
        ctx.shadow_table.map_id(11, 12);
        ctx.shadow_table.track_host_interface_with_version(
            host_xdg_toplevel_id,
            "xdg_toplevel".to_string(),
            6,
        );
        ctx.shadow_table.track_host_interface_with_version(
            zaura_toplevel_id,
            "zaura_toplevel".to_string(),
            38,
        );
        ctx.shadow_table.track_host_interface_with_version(
            zaura_surface_id,
            "zaura_surface".to_string(),
            zaura_surface_version,
        );
        assert!(ctx.window_placement.remember_xdg_toplevel(10, 11));
        assert!(ctx
            .window_placement
            .remember_aura_surface(12, zaura_surface_id));
        assert!(ctx
            .window_placement
            .remember_aura_toplevel(10, zaura_toplevel_id));
    }

    fn placement_target(
        host_xdg_toplevel_id: u32,
        zaura_toplevel_id: u32,
        zaura_surface_id: u32,
        zaura_surface_version: u32,
    ) -> PlacementTarget {
        PlacementTarget {
            guest_xdg_toplevel_id: 10,
            wl_surface_guest_id: 11,
            wl_surface_host_id: 12,
            host_xdg_toplevel_id,
            zaura_toplevel_host_id: zaura_toplevel_id,
            zaura_surface_host_id: zaura_surface_id,
            zaura_surface_version,
        }
    }

    fn direct_plan() -> WindowPlacementPlan {
        WindowPlacementPlan {
            target: placement_target(20, 30, 40, 5),
            output_host_id: 7,
            bounds: (100, 200, 800, 600),
            geometry: WindowPlacementGeometry::Bounds,
            transient_arc_identity: None,
            barrier_cleanup: None,
        }
    }

    #[test]
    fn message_string_limit_matches_wayland_header_and_padding() {
        assert!(wayland_string_fits_message("native"));
        assert!(wayland_string_fits_message(&"x".repeat(65_519)));
        assert!(!wayland_string_fits_message(&"x".repeat(65_520)));
    }

    #[test]
    fn direct_placement_adapter_publishes_one_ordered_wire_batch() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );

        assert!(queue_window_placement(&mut ctx, &direct_plan(),));

        let opcodes: Vec<_> = ctx.client_to_host_queue.iter().map(opcode).collect();
        assert_eq!(
            opcodes,
            vec![
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                REQ_SYNC,
            ]
        );
        assert_eq!(
            ctx.client_to_host_queue[3].0[8..28],
            [
                100i32.to_ne_bytes(),
                200i32.to_ne_bytes(),
                800i32.to_ne_bytes(),
                600i32.to_ne_bytes(),
                7u32.to_ne_bytes(),
            ]
            .concat()[..]
        );
        assert_eq!(sender(&ctx.client_to_host_queue[3]), zaura_toplevel_id);
        assert_eq!(sender(&ctx.client_to_host_queue[4]), 1);
        let callback_id = u32::from_ne_bytes(
            ctx.client_to_host_queue[4].0[8..12]
                .try_into()
                .expect("sync callback payload"),
        );
        assert_eq!(
            ctx.window_placement.barrier_for_callback(callback_id),
            Some(zaura_toplevel_id)
        );
    }

    #[test]
    fn malformed_plan_is_rejected_before_wire_or_barrier_mutation() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        let mut plan = direct_plan();
        plan.barrier_cleanup = Some(PlacementBarrierCleanup::Unparent {
            zaura_surface_id: zaura_surface_id + 1,
        });

        assert!(!queue_window_placement(&mut ctx, &plan));
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(!ctx.window_placement.has_any_barriers());

        let mut remapped = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut remapped,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        remapped.shadow_table.map_id(10, 99);
        assert!(!queue_window_placement(&mut remapped, &direct_plan()));
        assert!(remapped.client_to_host_queue.is_empty());
        assert!(!remapped.window_placement.has_any_barriers());
    }

    #[test]
    fn self_parent_adapter_resizes_before_position_probe() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            2,
        );
        let plan = WindowPlacementPlan {
            target: placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            output_host_id: 7,
            bounds: (500, 700, 800, 600),
            geometry: WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (400, 500),
            },
            transient_arc_identity: None,
            barrier_cleanup: Some(PlacementBarrierCleanup::Unparent { zaura_surface_id }),
        };

        assert!(queue_window_placement(&mut ctx, &plan,));
        let opcodes: Vec<_> = ctx.client_to_host_queue.iter().map(opcode).collect();
        assert_eq!(
            opcodes,
            vec![
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                REQ_SET_PARENT,
                REQ_SYNC,
            ]
        );
        assert_eq!(sender(&ctx.client_to_host_queue[3]), zaura_toplevel_id);
        assert_eq!(sender(&ctx.client_to_host_queue[4]), zaura_surface_id);
        assert_eq!(
            &ctx.client_to_host_queue[4].0[8..20],
            &[
                zaura_surface_id.to_ne_bytes(),
                400i32.to_ne_bytes(),
                500i32.to_ne_bytes(),
            ]
            .concat()
        );
        assert_eq!(
            ctx.window_placement.origin(zaura_toplevel_id),
            Some((500, 700))
        );
    }

    #[test]
    fn transient_adapter_queues_arc_identity_and_records_post_barrier_cleanup() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        let plan = WindowPlacementPlan {
            target: placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            output_host_id: 7,
            bounds: (100, 200, 800, 600),
            geometry: WindowPlacementGeometry::Bounds,
            transient_arc_identity: Some(TransientArcIdentity {
                arc_application_id: "org.chromium.arc.2000000001".to_string(),
                wl_surface_guest_id: 11,
            }),
            barrier_cleanup: Some(
                PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
                    zaura_surface_id,
                    wl_surface_guest_id: 11,
                },
            ),
        };

        assert!(queue_window_placement(&mut ctx, &plan,));

        let opcodes: Vec<_> = ctx.client_to_host_queue.iter().map(opcode).collect();
        assert_eq!(
            opcodes,
            vec![
                REQ_SET_APPLICATION_ID,
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                REQ_SYNC,
            ]
        );
        assert_eq!(sender(&ctx.client_to_host_queue[0]), zaura_surface_id);
        assert_eq!(
            application_id(&ctx.client_to_host_queue[0]),
            "org.chromium.arc.2000000001"
        );
        let callback_id = u32::from_ne_bytes(
            ctx.client_to_host_queue[5].0[8..12]
                .try_into()
                .expect("sync callback payload"),
        );
        assert_eq!(
            ctx.window_placement
                .complete_barrier(callback_id)
                .expect("queued placement barrier")
                .cleanup,
            plan.barrier_cleanup
        );
    }

    #[test]
    fn oversized_transient_identity_leaves_no_partial_wire_or_barrier_state() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        let plan = WindowPlacementPlan {
            target: placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            output_host_id: 7,
            bounds: (100, 200, 800, 600),
            geometry: WindowPlacementGeometry::Bounds,
            transient_arc_identity: Some(TransientArcIdentity {
                arc_application_id: "x".repeat(65_520),
                wl_surface_guest_id: 10,
            }),
            barrier_cleanup: Some(
                PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
                    zaura_surface_id,
                    wl_surface_guest_id: 10,
                },
            ),
        };

        assert!(!queue_window_placement(&mut ctx, &plan,));
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(!ctx.window_placement.has_any_barriers());
        assert_eq!(
            ctx.shadow_table.get_host_interface(1).map(String::as_str),
            None
        );
    }

    #[test]
    fn stale_host_surface_rejects_plan_before_wire_or_barrier_mutation() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        assert!(ctx.shadow_table.mark_pending_destroy_host(zaura_surface_id));

        assert!(
            !queue_window_placement(&mut ctx, &direct_plan(),),
            "released Aura surfaces must not receive a queued placement"
        );
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(!ctx.window_placement.has_any_barriers());
    }

    #[test]
    fn nullable_parent_serializes_zero_parent_and_offsets() {
        let surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            2,
        );

        assert!(queue_zaura_surface_parent(&mut ctx, surface_id, None, 0, 0));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(opcode(&(message.clone(), Vec::new())), REQ_SET_PARENT);
        assert_eq!(u32::from_ne_bytes(message[8..12].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(message[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_ne_bytes(message[16..20].try_into().unwrap()), 0);
    }

    #[test]
    fn transient_cleanup_queues_unparent_before_native_identity() {
        let surface_id = 55;
        let native_id = "org.chromium.guest_os.termina.wayland.test";
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );
        ctx.window_placement
            .remember_native_application_id(91, native_id.to_string());

        assert!(queue_barrier_cleanup(
            &mut ctx,
            &PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: 91,
            }
        ));
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(opcode(&ctx.client_to_host_queue[0]), REQ_SET_PARENT);
        assert_eq!(opcode(&ctx.client_to_host_queue[1]), REQ_SET_APPLICATION_ID);
    }

    #[test]
    fn transient_cleanup_still_unparents_when_native_identity_is_missing() {
        let surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );

        assert!(!queue_barrier_cleanup(
            &mut ctx,
            &PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: 91,
            }
        ));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(opcode(&ctx.client_to_host_queue[0]), REQ_SET_PARENT);
    }

    #[test]
    fn transient_cleanup_drops_released_surface_without_stale_wire() {
        let surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );
        assert!(ctx.shadow_table.mark_pending_destroy_host(surface_id));
        ctx.window_placement
            .remember_native_application_id(91, "org.chromium.guest_os.native".to_string());

        assert!(!queue_barrier_cleanup(
            &mut ctx,
            &PlacementBarrierCleanup::UnparentAndRestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: 91,
            }
        ));
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "a late barrier must not target a released Aura surface"
        );
    }

    #[test]
    fn persistent_native_shell_rejects_partial_identity_before_queueing() {
        let surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );
        ctx.window_placement.set_mode_for_test(
            crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            )
            .with_arc_id_lifetime(crate::state::WindowArcIdLifetime::PersistentNativeShell),
        );

        let oversized_native_id = "x".repeat(65_520);
        assert!(!super::queue_policy_application_id(
            &mut ctx,
            surface_id,
            91,
            &oversized_native_id,
        ));
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "ARC authorization must not be queued without its native follow-up"
        );
    }
}
