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

use crate::protocols::aura_shell::zaura_output::REQ_RELEASE as REQ_RELEASE_AURA_OUTPUT;
use crate::protocols::aura_shell::zaura_shell::{
    REQ_GET_AURA_OUTPUT, REQ_GET_AURA_SURFACE, REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL,
};
use crate::protocols::aura_shell::zaura_surface::{
    REQ_SET_APPLICATION_ID, REQ_SET_PARENT, REQ_UNSET_SNAP,
};
use crate::protocols::aura_shell::zaura_toplevel::{
    REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL, REQ_SET_WINDOW_BOUNDS,
};
use crate::protocols::remote_shell_unstable_v2::zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT as REQ_REMOTE_SET_BOUNDS_IN_OUTPUT;
use crate::protocols::wayland::wl_display::REQ_SYNC;
use crate::protocols::wayland::wl_surface::REQ_COMMIT as REQ_WL_SURFACE_COMMIT;
use crate::protocols::xdg_shell::xdg_surface::{
    EVT_CONFIGURE as EVT_XDG_SURFACE_CONFIGURE, REQ_SET_WINDOW_GEOMETRY,
};
use crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE as EVT_XDG_TOPLEVEL_CONFIGURE;
use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
use crate::state::{
    Context, PlacementBarrierCleanup, WindowPlacementGeometry, WindowPlacementPlan,
    XdgToplevelRelease,
};
use crate::window_shortcuts::WindowShortcut;
use crate::wire::MessageBuilder;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic process-local ID used to correlate one shortcut with every
/// asynchronous host barrier and cleanup phase.
///
/// This is diagnostic metadata only. It is intentionally independent of
/// Wayland object IDs, ARC task IDs, and the host's callback serials.
static NEXT_PLACEMENT_TRACE_ID: AtomicU64 = AtomicU64::new(1);

fn next_placement_trace_id() -> u64 {
    NEXT_PLACEMENT_TRACE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Log the fixed Wayland header of one queued placement request.
///
/// Logging the sender/opcode/byte length at the queue boundary catches an
/// accidentally reordered or partially published batch without dumping
/// untrusted payload strings into the diagnostic log.
fn log_placement_wire_message(trace_id: u64, phase: &str, message: &[u8]) {
    if message.len() < 8 {
        log::warn!(
            "[placement#{}] {} queued malformed wire message of {} bytes",
            trace_id,
            phase,
            message.len()
        );
        return;
    }
    let sender = u32::from_ne_bytes(message[0..4].try_into().unwrap());
    let header = u32::from_ne_bytes(message[4..8].try_into().unwrap());
    let size = header >> 16;
    let opcode = (header & 0xffff) as u16;
    log::debug!(
        "[placement#{}] queue phase={} sender={} opcode={} wire_size={} buffer_bytes={}",
        trace_id,
        phase,
        sender,
        opcode,
        size,
        message.len()
    );
}

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

/// Create the host-only `zaura_output` child that reports ChromeOS work-area
/// insets for one internally-bound `wl_output`.
///
/// The guest never sees this object. ChromeOS sends the `insets` event in
/// logical screen coordinates, which is the missing information needed to
/// avoid requesting the shelf area as part of a full-height placement.
pub(crate) fn ensure_host_zaura_output(ctx: &mut Context, wl_output_host_id: u32) -> Option<u32> {
    if let Some(existing_id) = ctx
        .window_placement
        .aura_output_for_output(wl_output_host_id)
    {
        return Some(existing_id);
    }
    let zaura_shell_host_id = ctx.window_placement.aura_shell_id()?;
    let zaura_shell_version = ctx.window_placement.aura_shell_version();
    // `zaura_output.insets` was added in v33. Older Aura shells do not expose
    // a usable work-area event, so retain the normal full-output behavior.
    if zaura_shell_version < 33
        || !ctx
            .shadow_table
            .host_object_matches(wl_output_host_id, "wl_output")
    {
        return None;
    }

    let zaura_output_host_id = ctx.shadow_table.allocate_host_id();
    ctx.shadow_table.track_host_interface_with_version(
        zaura_output_host_id,
        "zaura_output".to_string(),
        zaura_shell_version,
    );
    let mut builder = MessageBuilder::new();
    builder.write_u32(zaura_output_host_id);
    builder.write_u32(wl_output_host_id);
    let Ok(message) = builder.try_build_message(zaura_shell_host_id, REQ_GET_AURA_OUTPUT) else {
        ctx.shadow_table.remove_host_interface(zaura_output_host_id);
        return None;
    };
    if !ctx
        .window_placement
        .remember_aura_output(wl_output_host_id, zaura_output_host_id)
    {
        ctx.shadow_table.remove_host_interface(zaura_output_host_id);
        return None;
    }
    ctx.client_to_host_queue.push((message, Vec::new()));
    log::debug!(
        "Bound host-only zaura_output {} for wl_output {} (Aura v{})",
        zaura_output_host_id,
        wl_output_host_id,
        zaura_shell_version
    );
    Some(zaura_output_host_id)
}

/// Release a host-only Aura output child after its guest output is released.
pub(crate) fn queue_zaura_output_release(ctx: &mut Context, zaura_output_host_id: u32) {
    if !ctx
        .shadow_table
        .host_object_matches(zaura_output_host_id, "zaura_output")
    {
        return;
    }
    let version = ctx
        .shadow_table
        .host_object_version(zaura_output_host_id)
        .unwrap_or_default();
    if version >= 38 {
        if ctx
            .shadow_table
            .mark_pending_destroy_host(zaura_output_host_id)
        {
            let message =
                MessageBuilder::new().build_message(zaura_output_host_id, REQ_RELEASE_AURA_OUTPUT);
            ctx.client_to_host_queue.push((message, Vec::new()));
        }
    } else {
        // There is no destructor before v38. Retire dispatch metadata so
        // delayed insets cannot mutate a released output, while keeping the
        // numeric host ID unavailable until connection teardown.
        ctx.shadow_table.retire_host_interface(zaura_output_host_id);
    }
}

/// Encode a `zaura_surface.set_parent` request.
///
/// `parent_id = None` is the protocol's explicit unparent operation. Runtime
/// self-parent placement currently uses only the self-parent form; cleanup
/// retains that relationship because the nullable form starts a new host
/// focus/activation transition on the tested compositor.
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
#[cfg(test)]
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
///
/// Metadata cleanup and IME repair intentionally use two host barriers:
///
/// ```text
/// placement sync.done
///   -> set_application_id(native), identity sync
/// identity sync.done
///   -> text_input.deactivate, IME sync
/// IME sync.done
///   -> text_input.activate + editor-state replay
/// ```
///
/// Exo can process Aura identity changes asynchronously.  Keeping the
/// identity sync separate from the text-input deactivate prevents the latter
/// from being associated with the old host generation.
#[cfg(test)]
pub(crate) fn queue_barrier_cleanup(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: &PlacementBarrierCleanup,
) -> bool {
    queue_barrier_cleanup_with_trace(ctx, zaura_toplevel_id, cleanup, None)
}

/// Queue one placement cleanup and preserve its originating trace ID.
///
/// The public three-argument wrapper above is retained for focused unit tests
/// and non-diagnostic callers. Runtime callbacks use this variant so the
/// second identity barrier remains connected to the original shortcut.
pub(crate) fn queue_barrier_cleanup_with_trace(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: &PlacementBarrierCleanup,
    trace_id: Option<u64>,
) -> bool {
    if let Some(trace_id) = trace_id {
        log::info!(
            "[placement#{}] first sync.done accepted: toplevel={} cleanup={:?}",
            trace_id,
            zaura_toplevel_id,
            cleanup
        );
    }
    match cleanup {
        PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id } => {
            if let Some(trace_id) = trace_id {
                log::info!(
                    "[placement#{}] queue phase=self-parent-retain surface={} \
                     (no NULL-parent request)",
                    trace_id,
                    zaura_surface_id
                );
            }
            // Keep the self-parent relationship installed for this proxy
            // instance.  On the tested Exo/Ash host, `set_parent(NULL)` is
            // interpreted as a fresh activation transition: it can animate
            // the window to an unsolicited origin and invalidate the host IME
            // generation.  The ordered follow-up barrier still retires the
            // placement transaction and refreshes IME state, so no nullable
            // parent request is needed here.
            ctx.window_placement
                .finish_self_parent_move(zaura_toplevel_id);
            // The self-parent probe can invalidate Exo's host IME generation
            // even when no ARC identity was installed.  The reverse mapping
            // may have been retired while the callback was pending; in that
            // case there is no focused surface left to repair.
            let Some(guest_surface_id) = ctx
                .window_placement
                .guest_wl_surface_for_aura_surface(&ctx.shadow_table, *zaura_surface_id)
            else {
                ctx.window_placement
                    .abort_self_parent_cleanup(zaura_toplevel_id);
                return true;
            };
            let queued = queue_placement_cleanup_barrier(
                ctx,
                zaura_toplevel_id,
                PlacementBarrierCleanup::RefreshHostActivation { guest_surface_id },
                trace_id,
            );
            if !queued {
                ctx.window_placement
                    .abort_self_parent_cleanup(zaura_toplevel_id);
            }
            queued
        }
        PlacementBarrierCleanup::RestoreNativeApplicationId {
            zaura_surface_id,
            wl_surface_guest_id,
        } => {
            // Bounds placement never installs a parent relationship. Do not
            // emit a speculative set_parent(NULL): on Exo this creates an
            // extra focus/IME generation transition and can leave the host
            // text-input bridge unusable after a shortcut. Resolve the
            // identity at completion time rather than using a barrier-time
            // snapshot because an application may send a newer set_app_id
            // while the sync is in flight.
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
            if let Some(trace_id) = trace_id {
                log::info!(
                    "[placement#{}] queue phase=restore-native-app-id surface={} wl_surface={} app_id={:?}",
                    trace_id,
                    zaura_surface_id,
                    wl_surface_guest_id,
                    application_id
                );
            }
            let restored = queue_zaura_application_id(ctx, *zaura_surface_id, &application_id);
            if !restored {
                return false;
            }
            // The xdg role can be destroyed while its wl_surface remains
            // alive. In that teardown case the surface identity still needs
            // restoration, but there is no live placement transaction to own
            // an IME-refresh barrier.
            if ctx
                .window_placement
                .xdg_toplevel_for_aura_toplevel(zaura_toplevel_id)
                .is_none()
            {
                return true;
            }
            // Do not deactivate text input in this same host batch. First
            // prove that the native identity request crossed the host stream;
            // the follow-up cleanup then starts the normal text-input
            // deactivate/sync generation.
            queue_placement_cleanup_barrier(
                ctx,
                zaura_toplevel_id,
                PlacementBarrierCleanup::RefreshHostActivation {
                    guest_surface_id: *wl_surface_guest_id,
                },
                trace_id,
            )
        }
        PlacementBarrierCleanup::RefreshHostActivation { guest_surface_id } => {
            // The guest never lost focus, so do not synthesize a guest
            // text-input-v3 enter here. Refresh the host v1 generation and
            // replay the last committed editor state after host enter. This
            // preserves Winit's `ime_allowed` flag and restores the content
            // type/surrounding state that ARC identity changes invalidate.
            //
            // Mark the placement-owned follow-up barrier before attempting
            // to advance the transaction. A final Aura origin can arrive
            // before this callback; the explicit phase bit prevents that
            // event from completing cleanup prematurely.
            ctx.window_placement
                .complete_self_parent_cleanup_barrier(zaura_toplevel_id);
            if let Some(trace_id) = trace_id {
                log::info!(
                    "[placement#{}] identity cleanup sync.done accepted: refreshing host IME for wl_surface={}",
                    trace_id,
                    guest_surface_id
                );
            }
            let refreshed = crate::handler::text_input::refresh_host_activation_for_surface(
                ctx,
                *guest_surface_id,
            );
            log::info!(
                "[placement] host IME refresh result: surface={} refreshed={}",
                guest_surface_id,
                refreshed
            );
            if ctx
                .window_placement
                .self_parent_cleanup_pending(zaura_toplevel_id)
            {
                // The sync callback is an ordering boundary and carries no
                // geometry. Some Exo/Ash generations omit the matching
                // target `origin_change`, however. In that case the request
                // target is the only safe baseline: settle it before running
                // the normal completion/deferred-promotion path. Retiring
                // the transaction without this fallback drops the deferred
                // shortcut and leaves the next request OriginUnknown.
                if !ctx
                    .window_placement
                    .self_parent_origin_settled(zaura_toplevel_id)
                    && ctx
                        .window_placement
                        .settle_self_parent_after_cleanup(zaura_toplevel_id)
                {
                    log::warn!(
                        "self-parent cleanup omitted target origin; using requested target \
                         as fallback baseline for host={}",
                        zaura_toplevel_id
                    );
                }
                if ctx
                    .window_placement
                    .self_parent_origin_settled(zaura_toplevel_id)
                    && !advance_self_parent_after_origin(ctx, zaura_toplevel_id)
                {
                    log::warn!(
                        "Unable to advance self-parent target after cleanup for host={}",
                        zaura_toplevel_id
                    );
                }
            }
            true
        }
    }
}

/// Queue the follow-up barrier used to separate Aura metadata from IME
/// generation repair.
fn queue_placement_cleanup_barrier(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: PlacementBarrierCleanup,
    trace_id: Option<u64>,
) -> bool {
    let Some((_callback_host_id, message)) = build_and_register_placement_barrier_with_trace(
        ctx,
        zaura_toplevel_id,
        Some(cleanup),
        trace_id,
    ) else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

/// Queue a client-side XDG configure that asks the guest to resize its
/// committed buffer without changing the host application identity.
///
/// Native Guest OS windows are not authorized to use
/// `zaura_toplevel.set_window_bounds` for arbitrary resizing on ChromeOS.
/// They can, however, be resized through the normal XDG configure/commit
/// handshake.  The configure serial is proxy-owned and acknowledged locally;
/// forwarding that acknowledgement would make the host reject a serial it
/// never generated.
fn build_synthetic_xdg_resize(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    guest_wl_surface_id: u32,
    width: i32,
    height: i32,
    trace_id: u64,
) -> Option<(u32, u32, Vec<u8>, Vec<u8>)> {
    let Some(xdg_surface_guest_id) = ctx
        .window_placement
        .xdg_surface_for_wl_surface(guest_wl_surface_id)
    else {
        log::warn!(
            "[placement#{}] cannot synthesize XDG resize: no xdg_surface for wl_surface {}",
            trace_id,
            guest_wl_surface_id
        );
        return None;
    };
    let serial = ctx
        .window_placement
        .allocate_synthetic_xdg_configure_serial(xdg_surface_guest_id)?;

    let mut toplevel_builder = MessageBuilder::new();
    toplevel_builder.write_i32(width);
    toplevel_builder.write_i32(height);
    // No maximize/fullscreen/tiled state is implied by a shortcut rectangle.
    toplevel_builder.write_array(&[]);
    let Some(toplevel_message) = toplevel_builder
        .try_build_message(guest_xdg_toplevel_id, EVT_XDG_TOPLEVEL_CONFIGURE)
        .ok()
    else {
        ctx.window_placement
            .consume_synthetic_xdg_configure_serial(xdg_surface_guest_id, serial);
        return None;
    };

    let mut surface_builder = MessageBuilder::new();
    surface_builder.write_u32(serial);
    let Some(surface_message) = surface_builder
        .try_build_message(xdg_surface_guest_id, EVT_XDG_SURFACE_CONFIGURE)
        .ok()
    else {
        ctx.window_placement
            .consume_synthetic_xdg_configure_serial(xdg_surface_guest_id, serial);
        return None;
    };

    log::info!(
        "[placement#{}] queue phase=synthetic-xdg-resize xdg_surface={} \
         xdg_toplevel={} configure_serial={} size={}x{}",
        trace_id,
        xdg_surface_guest_id,
        guest_xdg_toplevel_id,
        serial,
        width,
        height
    );
    Some((
        xdg_surface_guest_id,
        serial,
        toplevel_message,
        surface_message,
    ))
}

/// Queue a client-side XDG configure and retain its proxy-owned serial.
pub(crate) fn queue_synthetic_xdg_resize(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    guest_wl_surface_id: u32,
    width: i32,
    height: i32,
    trace_id: u64,
) -> bool {
    let Some((_xdg_surface_guest_id, _serial, toplevel_message, surface_message)) =
        build_synthetic_xdg_resize(
            ctx,
            guest_xdg_toplevel_id,
            guest_wl_surface_id,
            width,
            height,
            trace_id,
        )
    else {
        return false;
    };
    ctx.host_to_client_queue
        .push((toplevel_message, Vec::new()));
    ctx.host_to_client_queue.push((surface_message, Vec::new()));
    true
}

/// Build the host-side XDG geometry that applies a synthetic resize.
///
/// `xdg_surface.set_window_geometry` is double-buffered host state.  Unlike
/// Aura's direct `set_window_bounds`, Exo applies it from the next surface
/// commit without consulting `ChromeSecurityDelegate::CanSetBounds()`.  This
/// gives a native Guest OS surface a supported size path while preserving its
/// normal application identity.
fn build_host_xdg_window_geometry(
    ctx: &Context,
    guest_wl_surface_id: u32,
    width: i32,
    height: i32,
    trace_id: u64,
) -> Option<Vec<u8>> {
    let guest_xdg_surface_id = ctx
        .window_placement
        .xdg_surface_for_wl_surface(guest_wl_surface_id)?;
    let host_xdg_surface_id = ctx.shadow_table.get_host_id(guest_xdg_surface_id)?;
    if !ctx
        .shadow_table
        .host_object_matches(host_xdg_surface_id, "xdg_surface")
    {
        log::warn!(
            "[placement#{}] refusing synthetic host XDG geometry for unmapped \
             xdg_surface guest={} host={}",
            trace_id,
            guest_xdg_surface_id,
            host_xdg_surface_id
        );
        return None;
    }
    if width <= 0 || height <= 0 {
        log::warn!(
            "[placement#{}] refusing synthetic host XDG geometry with invalid \
             size={}x{}",
            trace_id,
            width,
            height
        );
        return None;
    }

    let mut builder = MessageBuilder::new();
    builder.write_i32(0);
    builder.write_i32(0);
    builder.write_i32(width);
    builder.write_i32(height);
    let message = builder
        .try_build_message(host_xdg_surface_id, REQ_SET_WINDOW_GEOMETRY)
        .ok()?;
    log::info!(
        "[placement#{}] queue phase=host-xdg-window-geometry xdg_surface={} \
         size={}x{}",
        trace_id,
        host_xdg_surface_id,
        width,
        height
    );
    Some(message)
}

/// Fully encoded wire messages for one native self-parent resize phase.
///
/// Encoding is intentionally separate from publication. The placement state
/// can be armed only after every fallible message has been built, so a stale
/// toplevel or an oversized host message cannot leave a live reducer waiting
/// for a configure that was never sent.
struct SelfParentResizeWire {
    configure_token: (u32, u32),
    guest_toplevel_configure: Vec<u8>,
    guest_surface_configure: Vec<u8>,
    host_geometry: Vec<u8>,
}

/// Wire batch for one self-parent request and its cleanup barrier.
///
/// The barrier is registered while the batch is staged, but neither request is
/// appended to the transport queue until the matching reducer transition has
/// succeeded. This keeps state and host-stream publication atomic from the
/// proxy's point of view.
struct StagedSelfParentMove {
    parent_message: Vec<u8>,
    callback_host_id: u32,
    barrier_message: Vec<u8>,
}

impl StagedSelfParentMove {
    fn publish(self, ctx: &mut Context) {
        ctx.client_to_host_queue
            .push((self.parent_message, Vec::new()));
        ctx.client_to_host_queue
            .push((self.barrier_message, Vec::new()));
    }

    fn rollback(self, ctx: &mut Context) {
        if !ctx.window_placement.cancel_barrier(self.callback_host_id) {
            log::debug!(
                "self-parent rollback callback {} was already retired",
                self.callback_host_id
            );
        }
        ctx.shadow_table
            .remove_host_interface(self.callback_host_id);
    }
}

/// Build the two wire halves of one native self-parent resize phase.
///
/// The guest configure and the host XDG geometry are intentionally built
/// before either queue is changed. This helper is also used when a newer
/// shortcut replaces a deferred target, so a failed encoding cannot leave a
/// half-published resize transaction behind.
fn build_self_parent_resize_phase(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    guest_wl_surface_id: u32,
    width: i32,
    height: i32,
    trace_id: u64,
) -> Option<SelfParentResizeWire> {
    let host_geometry =
        build_host_xdg_window_geometry(ctx, guest_wl_surface_id, width, height, trace_id)?;

    let (_xdg_surface_guest_id, _serial, toplevel_message, surface_message) =
        build_synthetic_xdg_resize(
            ctx,
            guest_xdg_toplevel_id,
            guest_wl_surface_id,
            width,
            height,
            trace_id,
        )?;
    let configure_token = (_xdg_surface_guest_id, _serial);
    log_placement_wire_message(trace_id, "host-xdg-window-geometry", &host_geometry);
    Some(SelfParentResizeWire {
        configure_token,
        guest_toplevel_configure: toplevel_message,
        guest_surface_configure: surface_message,
        host_geometry,
    })
}

/// Publish one already-encoded native self-parent resize phase.
fn queue_self_parent_resize_phase(wire: SelfParentResizeWire, ctx: &mut Context) {
    ctx.host_to_client_queue
        .push((wire.guest_toplevel_configure, Vec::new()));
    ctx.host_to_client_queue
        .push((wire.guest_surface_configure, Vec::new()));
    ctx.client_to_host_queue
        .push((wire.host_geometry, Vec::new()));
}

/// Queue the deferred self-parent move once the resize phase has been
/// acknowledged by the host.
///
/// Keeping this in the placement adapter makes the parent request and its
/// cleanup barrier one indivisible transition for both the normal configure
/// path and the same-size deferred-target path.
pub(crate) fn queue_pending_self_parent_move(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    trace_id: Option<u64>,
) -> bool {
    let Some((zaura_surface_id, _current_origin, relative)) = ctx
        .window_placement
        .pending_self_parent_move(zaura_toplevel_host_id)
    else {
        return false;
    };
    let Some(parent_message) =
        build_self_parent_move_message(ctx, zaura_surface_id, relative, trace_id)
    else {
        return false;
    };
    let Some(staged_move) = stage_self_parent_move(
        ctx,
        zaura_toplevel_host_id,
        zaura_surface_id,
        parent_message,
        trace_id,
    ) else {
        return false;
    };
    if !ctx
        .window_placement
        .mark_self_parent_move_queued(zaura_toplevel_host_id)
    {
        log::warn!(
            "self-parent move was queued after its state was released for host={}",
            zaura_toplevel_host_id
        );
        // The barrier was registered before the reducer transition so a
        // callback cannot observe an untracked host request. If the live-role
        // check fails at this final boundary, also abort the resize phase;
        // otherwise every subsequent shortcut would remain stuck waiting for
        // a move that was never published.
        ctx.window_placement
            .abort_self_parent_resize(zaura_toplevel_host_id);
        staged_move.rollback(ctx);
        return false;
    }
    staged_move.publish(ctx);
    true
}

/// Encode a self-parent request without mutating placement state or queues.
fn build_self_parent_move_message(
    ctx: &mut Context,
    zaura_surface_id: u32,
    relative: (i32, i32),
    trace_id: Option<u64>,
) -> Option<Vec<u8>> {
    if let Some(trace_id) = trace_id {
        log::info!(
            "[placement#{}] queue phase=self-parent-move surface={} \
             relative=({}, {})",
            trace_id,
            zaura_surface_id,
            relative.0,
            relative.1
        );
    }
    let parent_message = build_zaura_surface_parent(
        ctx,
        zaura_surface_id,
        Some(zaura_surface_id),
        relative.0,
        relative.1,
    )?;
    Some(parent_message)
}

/// Register the cleanup barrier for an already encoded self-parent request.
///
/// Registration is deliberately separate from message encoding so a deferred
/// target can be promoted first. The callback then captures the new
/// generation instead of the superseded transaction.
fn stage_self_parent_move(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    zaura_surface_id: u32,
    parent_message: Vec<u8>,
    trace_id: Option<u64>,
) -> Option<StagedSelfParentMove> {
    let cleanup = PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id };
    let Some((callback_host_id, barrier_message)) = build_and_register_placement_barrier_with_trace(
        ctx,
        zaura_toplevel_host_id,
        Some(cleanup),
        trace_id,
    ) else {
        log::warn!(
            "self-parent move was encoded but its cleanup barrier could not be registered \
             for host={}",
            zaura_toplevel_host_id
        );
        return None;
    };
    Some(StagedSelfParentMove {
        parent_message,
        callback_host_id,
        barrier_message,
    })
}

/// Start a deferred target after the previous self-parent and IME refresh
/// barriers have completed.
pub(crate) fn advance_deferred_self_parent_after_cleanup(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    guest_wl_surface_id: u32,
    zaura_toplevel_host_id: u32,
) -> bool {
    if !ctx
        .window_placement
        .self_parent_origin_settled(zaura_toplevel_host_id)
    {
        log::debug!(
            "deferring self-parent target for host={} until the host reports \
             the final origin",
            zaura_toplevel_host_id
        );
        return false;
    }
    let Some(target) = ctx
        .window_placement
        .deferred_self_parent_target(zaura_toplevel_host_id)
    else {
        return false;
    };
    let trace_id = next_placement_trace_id();
    let old_size = ctx
        .window_placement
        .active_self_parent_size(zaura_toplevel_host_id);
    if old_size == Some((target.2, target.3)) {
        let Some((zaura_surface_id, _current_origin, relative, _)) = ctx
            .window_placement
            .deferred_self_parent_move(zaura_toplevel_host_id)
        else {
            return false;
        };
        let Some(parent_message) =
            build_self_parent_move_message(ctx, zaura_surface_id, relative, Some(trace_id))
        else {
            return false;
        };
        let Some(rollback) = ctx
            .window_placement
            .promote_deferred_self_parent_target_with_rollback(zaura_toplevel_host_id, target)
        else {
            return false;
        };
        let Some(staged_move) = stage_self_parent_move(
            ctx,
            zaura_toplevel_host_id,
            zaura_surface_id,
            parent_message,
            Some(trace_id),
        ) else {
            let _ = ctx
                .window_placement
                .rollback_deferred_self_parent_promotion(zaura_toplevel_host_id, rollback);
            return false;
        };
        if !ctx
            .window_placement
            .mark_self_parent_move_queued(zaura_toplevel_host_id)
        {
            let _ = ctx
                .window_placement
                .rollback_deferred_self_parent_promotion(zaura_toplevel_host_id, rollback);
            staged_move.rollback(ctx);
            return false;
        }
        staged_move.publish(ctx);
        return true;
    }

    let Some(resize_wire) = build_self_parent_resize_phase(
        ctx,
        guest_xdg_toplevel_id,
        guest_wl_surface_id,
        target.2,
        target.3,
        trace_id,
    ) else {
        return false;
    };
    let configure_token = resize_wire.configure_token;
    let Some(rollback) = ctx
        .window_placement
        .promote_deferred_self_parent_resize_with_rollback(zaura_toplevel_host_id, target)
    else {
        ctx.window_placement
            .cancel_synthetic_xdg_configure_serial(configure_token.0, configure_token.1);
        return false;
    };
    if !ctx.window_placement.gate_resize_on_guest_commit(
        zaura_toplevel_host_id,
        configure_token.0,
        configure_token.1,
    ) {
        ctx.window_placement
            .cancel_synthetic_xdg_configure_serial(configure_token.0, configure_token.1);
        let _ = ctx
            .window_placement
            .rollback_deferred_self_parent_promotion(zaura_toplevel_host_id, rollback);
        return false;
    }
    queue_self_parent_resize_phase(resize_wire, ctx);
    true
}

/// Finish or advance a self-parent transaction after a final origin event.
///
/// The host sync callback only orders the persistent self-parent transition;
/// it does not mean that Ash has finished animating the widget. This helper is
/// called from both `origin_change` and `configure`, so either host event can
/// release the queued latest target without duplicating the transition logic.
pub(crate) fn advance_self_parent_after_origin(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
) -> bool {
    if !ctx
        .window_placement
        .self_parent_cleanup_pending(zaura_toplevel_host_id)
        || !ctx
            .window_placement
            .self_parent_origin_settled(zaura_toplevel_host_id)
    {
        return false;
    }
    // `origin_change` and the ordered cleanup callback can race. The origin
    // is useful evidence even when it arrives first, but promotion must wait
    // for the callback's host-stream barrier. Returning here (instead of
    // treating the unavailable move as an encoding failure) preserves the
    // deferred target for the callback path.
    if ctx
        .window_placement
        .self_parent_cleanup_barrier_pending(zaura_toplevel_host_id)
    {
        return false;
    }
    let Some(deferred) = ctx
        .window_placement
        .deferred_self_parent_target(zaura_toplevel_host_id)
    else {
        return ctx
            .window_placement
            .complete_self_parent_cleanup(zaura_toplevel_host_id);
    };
    let Some(zaura_surface_id) = ctx
        .window_placement
        .pending_surface_for_toplevel(zaura_toplevel_host_id)
    else {
        return false;
    };
    let Some(guest_surface_id) = ctx
        .window_placement
        .guest_wl_surface_for_aura_surface(&ctx.shadow_table, zaura_surface_id)
    else {
        log::warn!(
            "Dropping deferred self-parent target {:?}: surface for host={} \
             was released",
            deferred,
            zaura_toplevel_host_id
        );
        ctx.window_placement
            .abort_self_parent_cleanup(zaura_toplevel_host_id);
        return false;
    };
    let Some(guest_xdg_toplevel_id) = ctx
        .window_placement
        .xdg_toplevel_for_wl_surface(guest_surface_id)
    else {
        ctx.window_placement
            .abort_self_parent_cleanup(zaura_toplevel_host_id);
        return false;
    };
    let advanced = advance_deferred_self_parent_after_cleanup(
        ctx,
        guest_xdg_toplevel_id,
        guest_surface_id,
        zaura_toplevel_host_id,
    );
    if !advanced {
        // The cleanup barrier has already crossed the host stream. If the
        // deferred phase cannot be encoded or registered, no later callback
        // is guaranteed to retry it. Leave the reducer idle and retryable
        // rather than retaining a CleanupPending target forever.
        if ctx
            .window_placement
            .abort_self_parent_cleanup(zaura_toplevel_host_id)
        {
            log::warn!(
                "Aborted deferred self-parent target after cleanup advancement \
                 failed for host={}",
                zaura_toplevel_host_id
            );
        }
    }
    advanced
}

/// Queue one complete placement operation and its ordered host barrier.
///
/// The keyboard handler has already resolved the focused role and the state
/// owner has validated the geometry. This adapter owns the remaining wire
/// contract:
///
/// ```text
/// self-parent:
///   synthetic guest configure + host XDG geometry
///   unset fullscreen/maximized/snap
///   (wait for the matching host configure)
///   set_parent(self, relative_position) + wl_display.sync
///
/// direct bounds:
///   [transient ARC identity]
///   unset fullscreen/maximized/snap
///   set_window_bounds(...)
///   wl_display.sync(...)
/// ```
///
/// All messages are built before any are appended to the connection queue.
/// That keeps an encoding or lifecycle failure from leaving a partial
/// placement sequence in front of the guest. The barrier callback is
/// registered before the batch is published so its cleanup metadata and wire
/// request become one state transition.
pub(crate) fn queue_window_placement(ctx: &mut Context, plan: &WindowPlacementPlan) -> bool {
    let mut messages = Vec::with_capacity(8);
    let mut message_phases = Vec::with_capacity(8);
    let mut ime_preflight = None;
    let target = plan.target();

    if !plan.is_well_formed() {
        log::warn!(
            "Skipping malformed window-placement plan for xdg_toplevel {} \
             and zaura_surface {}",
            target.guest_xdg_toplevel_id(),
            target.zaura_surface_host_id(),
        );
        return false;
    }

    // A plan is prepared before the host event loop can process teardown.
    // Revalidate every association and host interface immediately before
    // serialization so a delayed shortcut cannot pair IDs from different
    // windows or send requests to an object whose destructor is in flight.
    if !ctx
        .window_placement
        .plan_is_current(&ctx.shadow_table, plan)
    {
        log::debug!(
            "Skipping stale window-placement target xdg={} wl_surface={} \
             zaura_toplevel={} zaura_surface={}",
            target.guest_xdg_toplevel_id(),
            target.wl_surface_guest_id(),
            target.zaura_toplevel_host_id(),
            target.zaura_surface_host_id(),
        );
        return false;
    }

    let trace_id = next_placement_trace_id();
    let bounds = plan.bounds();
    let geometry = plan.geometry();
    log::info!(
        "[placement#{}] begin backend={:?} arc_lifetime={:?} geometry={:?} \
         guest_xdg={} guest_wl_surface={} host_xdg={} aura_toplevel={} aura_surface={} \
         output={} bounds=({}, {}, {}, {})",
        trace_id,
        ctx.window_placement.mode(),
        ctx.window_placement.arc_id_lifetime(),
        plan.geometry(),
        target.guest_xdg_toplevel_id(),
        target.wl_surface_guest_id(),
        target.host_xdg_toplevel_id(),
        target.zaura_toplevel_host_id(),
        target.zaura_surface_host_id(),
        plan.output_host_id(),
        plan.bounds().0,
        plan.bounds().1,
        plan.bounds().2,
        plan.bounds().3,
    );

    if matches!(geometry, WindowPlacementGeometry::RemoteShell) {
        let mut builder = MessageBuilder::new();
        builder.write_u32(plan.output_host_id());
        builder.write_i32(bounds.0);
        builder.write_i32(bounds.1);
        builder.write_i32(bounds.2);
        builder.write_i32(bounds.3);
        let Ok(bounds_message) = builder.try_build_message(
            target.zaura_surface_host_id(),
            REQ_REMOTE_SET_BOUNDS_IN_OUTPUT,
        ) else {
            log::warn!(
                "[placement#{}] unable to encode remote-shell bounds request \
                 for surface {}",
                trace_id,
                target.zaura_surface_host_id()
            );
            return false;
        };
        let commit_message =
            MessageBuilder::new().build_message(target.wl_surface_host_id(), REQ_WL_SURFACE_COMMIT);
        log_placement_wire_message(trace_id, "remote-set-bounds-in-output", &bounds_message);
        log_placement_wire_message(trace_id, "remote-surface-commit", &commit_message);
        ctx.client_to_host_queue.push((bounds_message, Vec::new()));
        ctx.client_to_host_queue.push((commit_message, Vec::new()));
        let _ = ctx.window_placement.commit_placement_plan(plan);
        return true;
    }

    let self_parent_transaction_active =
        matches!(geometry, WindowPlacementGeometry::SelfParent { .. })
            && ctx
                .window_placement
                .self_parent_transaction_active(target.zaura_toplevel_host_id());
    if self_parent_transaction_active {
        // Do not append another synthetic configure, XDG geometry request,
        // or parent/barrier pair. The state owner retains this plan as the
        // latest deferred target and starts it at the next safe phase
        // boundary.
        if !ctx.window_placement.commit_placement_plan(plan) {
            log::warn!(
                "[placement#?] unable to retain deferred self-parent target for host={}",
                target.zaura_toplevel_host_id()
            );
            return false;
        }
        log::info!(
            "deferred self-parent target for host={} bounds=({}, {}, {}, {})",
            target.zaura_toplevel_host_id(),
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3
        );
        return true;
    }

    if plan.transient_arc_identity().is_some() {
        let Some(preflight) = crate::handler::text_input::prepare_placement_ime_deactivation(
            ctx,
            target.wl_surface_guest_id(),
        ) else {
            log::warn!(
                "Unable to drain the focused IME before transient placement on wl_surface {}",
                target.wl_surface_guest_id()
            );
            return false;
        };
        message_phases.extend(preflight.host_messages.iter().map(|_| "ime-preflight"));
        messages.extend(preflight.host_messages.iter().cloned());
        ime_preflight = Some(preflight);
    }

    if let Some(identity) = plan.transient_arc_identity() {
        let Some(message) = build_zaura_application_id(
            ctx,
            target.zaura_surface_host_id(),
            identity.arc_application_id(),
        ) else {
            log::warn!(
                "Unable to install transient ARC ID on zaura_surface {}",
                target.zaura_surface_host_id()
            );
            return false;
        };
        log::info!(
            "[placement#{}] queue phase=install-transient-arc-id surface={} app_id={:?}",
            trace_id,
            target.zaura_surface_host_id(),
            identity.arc_application_id()
        );
        message_phases.push("install-transient-arc-id");
        messages.push((message, Vec::new()));
    }

    message_phases.push("unset-fullscreen");
    messages.push((
        MessageBuilder::new().build_message(target.host_xdg_toplevel_id(), REQ_UNSET_FULLSCREEN),
        Vec::new(),
    ));
    message_phases.push("unset-maximized");
    messages.push((
        MessageBuilder::new().build_message(target.host_xdg_toplevel_id(), REQ_UNSET_MAXIMIZED),
        Vec::new(),
    ));
    message_phases.push("unset-snap");
    messages.push((
        MessageBuilder::new().build_message(target.zaura_surface_host_id(), REQ_UNSET_SNAP),
        Vec::new(),
    ));

    if matches!(geometry, WindowPlacementGeometry::Bounds) {
        let mut bounds_builder = MessageBuilder::new();
        bounds_builder.write_i32(bounds.0);
        bounds_builder.write_i32(bounds.1);
        bounds_builder.write_i32(bounds.2);
        bounds_builder.write_i32(bounds.3);
        bounds_builder.write_u32(plan.output_host_id());
        log::info!(
            "[placement#{}] queue phase=set-window-bounds sender={} request=({}, {}, {}, {}) output={}",
            trace_id,
            target.zaura_toplevel_host_id(),
            bounds.0,
            bounds.1,
            bounds.2,
            bounds.3,
            plan.output_host_id()
        );
        message_phases.push("set-window-bounds");
        messages.push((
            bounds_builder.build_message(target.zaura_toplevel_host_id(), REQ_SET_WINDOW_BOUNDS),
            Vec::new(),
        ));
    }

    // Build the self-parent resize only after every other fallible message
    // encoder has succeeded. The helper owns the synthetic serial reservation
    // and cancels it if either configure message cannot be encoded.
    let synthetic_resize_wire = if matches!(geometry, WindowPlacementGeometry::SelfParent { .. }) {
        let Some(wire) = build_self_parent_resize_phase(
            ctx,
            target.guest_xdg_toplevel_id(),
            target.wl_surface_guest_id(),
            bounds.2,
            bounds.3,
            trace_id,
        ) else {
            log::warn!(
                "[placement#{}] refusing native self-parent placement without \
                 a synthetic XDG resize handshake",
                trace_id
            );
            return false;
        };
        Some(wire)
    } else {
        None
    };

    let configure_token = synthetic_resize_wire
        .as_ref()
        .map(|wire| wire.configure_token);
    if let Some(preflight) = ime_preflight.as_ref() {
        if !crate::handler::text_input::commit_placement_ime_preflight(ctx, preflight) {
            if let Some((xdg_surface_id, serial)) = configure_token {
                ctx.window_placement
                    .cancel_synthetic_xdg_configure_serial(xdg_surface_id, serial);
            }
            log::warn!("Transient placement IME preflight could not be committed");
            return false;
        }
    }

    let committed = ctx
        .window_placement
        .commit_placement_plan_with_configure(plan, configure_token);
    if !committed {
        if let Some(preflight) = ime_preflight.as_ref() {
            crate::handler::text_input::rollback_placement_ime_preflight(ctx, preflight);
        }
        if let Some((xdg_surface_id, serial)) = configure_token {
            ctx.window_placement
                .cancel_synthetic_xdg_configure_serial(xdg_surface_id, serial);
        }
        log::warn!(
            "Placement target for zaura_toplevel {} was released before \
             its state transition could be committed",
            target.zaura_toplevel_host_id()
        );
        return false;
    }

    // Self-parent cleanup is registered only after the matching host size
    // configure arrives. Registering it in this first batch would allow the
    // follow-up IME barrier to run before Exo has applied the resize.
    // Direct-bounds barriers are registered only after all fallible local
    // state transitions, so a registration failure can still roll back the
    // IME preflight without leaving a callback record behind.
    if matches!(geometry, WindowPlacementGeometry::Bounds) {
        let Some((_callback_host_id, barrier_message)) =
            build_and_register_placement_barrier_with_trace(
                ctx,
                target.zaura_toplevel_host_id(),
                plan.barrier_cleanup().cloned(),
                Some(trace_id),
            )
        else {
            if let Some(preflight) = ime_preflight.as_ref() {
                crate::handler::text_input::rollback_placement_ime_preflight(ctx, preflight);
            }
            return false;
        };
        message_phases.push("placement-sync");
        messages.push((barrier_message, Vec::new()));
    }

    debug_assert_eq!(messages.len(), message_phases.len());
    for (message, phase) in messages.iter().zip(message_phases.iter()) {
        log_placement_wire_message(trace_id, phase, &message.0);
    }
    log::info!(
        "[placement#{}] publish {} host request(s); cleanup={:?}",
        trace_id,
        messages.len(),
        plan.barrier_cleanup()
    );
    // Keep host XDG geometry ahead of the unset-state requests, matching the
    // original wire sequence. The reducer is already armed, so a guest
    // configure cannot race an untracked state transition.
    if let Some(wire) = synthetic_resize_wire.as_ref() {
        ctx.client_to_host_queue
            .push((wire.host_geometry.clone(), Vec::new()));
    }
    ctx.client_to_host_queue.extend(messages);
    if let Some(wire) = synthetic_resize_wire {
        ctx.host_to_client_queue
            .push((wire.guest_toplevel_configure, Vec::new()));
        ctx.host_to_client_queue
            .push((wire.guest_surface_configure, Vec::new()));
    }
    if let Some(preflight) = ime_preflight.as_ref() {
        crate::handler::text_input::finalize_placement_ime_preflight(ctx, preflight);
    }
    log::info!("[placement#{}] queued successfully", trace_id);
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
    if !ctx.window_placement.uses_remote_shell()
        && ensure_zaura_toplevel(ctx, guest_xdg_toplevel_id).is_none()
    {
        log::debug!(
            "window shortcut {:?} ignored: no zaura_toplevel for xdg_toplevel {}",
            shortcut,
            guest_xdg_toplevel_id
        );
        return false;
    }
    if ctx.window_placement.uses_remote_shell() {
        let Some(host_wl_surface_id) = ctx.shadow_table.get_host_id(guest_wl_surface_id) else {
            return false;
        };
        if ctx
            .window_placement
            .remote_surface_for_wl_surface(host_wl_surface_id)
            .is_none()
        {
            log::debug!(
                "window shortcut {:?} ignored: no remote surface for wl_surface {}",
                shortcut,
                guest_wl_surface_id
            );
            return false;
        }
    } else if ensure_host_zaura_surface(ctx, guest_wl_surface_id).is_none() {
        log::debug!(
            "window shortcut {:?} ignored: no zaura_surface for wl_surface {}",
            shortcut,
            guest_wl_surface_id
        );
        return false;
    }
    let zaura_surface_version = if ctx.window_placement.uses_remote_shell() {
        6
    } else {
        ctx.window_placement
            .aura_surface_version_for_guest_surface(&ctx.shadow_table, guest_wl_surface_id)
            .unwrap_or_default()
    };
    let plan = match ctx.window_placement.prepare_placement(
        &ctx.shadow_table,
        guest_xdg_toplevel_id,
        guest_wl_surface_id,
        shortcut.rect,
    ) {
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
                    "set_application_id"
                } else {
                    "set_parent(self)"
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
            guest_xdg_toplevel_id
        );
        return false;
    }
    if plan.barrier_cleanup().is_some() {
        log::debug!(
            "window layout {:?}: placement cleanup is deferred until sync.done",
            shortcut,
        );
    }
    if let WindowPlacementGeometry::SelfParent {
        current_origin: (origin_x, origin_y),
        relative_position: (relative_x, relative_y),
    } = plan.geometry()
    {
        log::warn!(
            "window layout {:?}: deferred self-parent resize phase queued for \
             zaura_surface={} target_screen_position=({}, {}) origin=({}, {}) \
             relative_position=({}, {}); position waits for host configure",
            shortcut,
            plan.target().zaura_surface_host_id(),
            plan.bounds().0,
            plan.bounds().1,
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
            plan.bounds().0,
            plan.bounds().1,
            plan.bounds().2,
            plan.bounds().3,
            plan.output_host_id()
        );
    } else {
        log::info!(
            "window layout {:?}: xdg_toplevel={} zaura_toplevel={} bounds=({}, {}, {}, {}) output={}",
            shortcut,
            guest_xdg_toplevel_id,
            plan.target().zaura_toplevel_host_id(),
            plan.bounds().0,
            plan.bounds().1,
            plan.bounds().2,
            plan.bounds().3,
            plan.output_host_id()
        );
    }
    true
}

/// Build and register the host-stream barrier for one placement generation.
///
/// Registration happens before the message is returned so callback cleanup
/// cannot race a batch that has not yet been published. The caller owns the
/// final queue insertion.
#[cfg(test)]
fn build_and_register_placement_barrier(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
) -> Option<Vec<u8>> {
    build_and_register_placement_barrier_with_trace(ctx, zaura_toplevel_id, cleanup, None)
        .map(|(_callback_host_id, message)| message)
}

/// Build/register a host sync barrier and carry an optional placement trace.
fn build_and_register_placement_barrier_with_trace(
    ctx: &mut Context,
    zaura_toplevel_id: u32,
    cleanup: Option<PlacementBarrierCleanup>,
    trace_id: Option<u64>,
) -> Option<(u32, Vec<u8>)> {
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
    if !ctx.window_placement.register_barrier_with_trace(
        callback_host_id,
        zaura_toplevel_id,
        cleanup.clone(),
        trace_id,
    ) {
        log::error!(
            "Refusing to register window-placement barrier callback {}",
            callback_host_id
        );
        ctx.shadow_table.remove_host_interface(callback_host_id);
        return None;
    }
    if let Some(trace_id) = trace_id {
        log::info!(
            "[placement#{}] registered sync callback={} toplevel={} cleanup={:?}",
            trace_id,
            callback_host_id,
            zaura_toplevel_id,
            cleanup
        );
    }
    Some((callback_host_id, barrier_message))
}

/// Queue a nullable Aura application ID update for a live host surface.
fn build_zaura_application_id(
    ctx: &mut Context,
    zaura_surface_id: u32,
    application_id: &str,
) -> Option<Vec<u8>> {
    if !ctx
        .shadow_table
        .host_object_matches(zaura_surface_id, "zaura_surface")
    {
        log::debug!(
            "Skipping application ID update for released zaura_surface {}",
            zaura_surface_id
        );
        return None;
    }
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

/// Queue release of a state-owned host Aura toplevel.
///
/// The placement state has already removed the guest-role association and its
/// per-toplevel state before this function is called. Keeping this adapter
/// host-ID-only prevents wire serialization from mutating lifecycle state or
/// accidentally releasing a different guest role after ID reuse.
pub(crate) fn queue_zaura_toplevel_release(ctx: &mut Context, release: XdgToplevelRelease) {
    let Some(zaura_toplevel_host_id) = release.zaura_toplevel_host_id else {
        return;
    };
    if !ctx
        .shadow_table
        .host_object_matches(zaura_toplevel_host_id, "zaura_toplevel")
    {
        log::debug!(
            "Skipping release for unknown or already retired zaura_toplevel {}",
            zaura_toplevel_host_id
        );
        return;
    }
    let version = ctx
        .shadow_table
        .host_object_version(zaura_toplevel_host_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version >= 38 {
        if ctx
            .shadow_table
            .mark_pending_destroy_host(zaura_toplevel_host_id)
        {
            let message = MessageBuilder::new()
                .build_message(zaura_toplevel_host_id, REQ_RELEASE_AURA_TOPLEVEL);
            ctx.client_to_host_queue.push((message, Vec::new()));
        }
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
        queue_zaura_toplevel_release, wayland_string_fits_message,
    };
    use crate::handler::callback::CallbackHandler;
    use crate::handler::compositor::CompositorHandler;
    use crate::protocols::aura_shell::zaura_surface::{
        REQ_SET_APPLICATION_ID, REQ_SET_PARENT, REQ_UNSET_SNAP,
    };
    use crate::protocols::aura_shell::zaura_toplevel::{
        REQ_RELEASE as REQ_RELEASE_AURA_TOPLEVEL, REQ_SET_WINDOW_BOUNDS,
    };
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_display::REQ_SYNC;
    use crate::protocols::xdg_shell::xdg_surface::{
        EVT_CONFIGURE as EVT_XDG_SURFACE_CONFIGURE, REQ_SET_WINDOW_GEOMETRY,
    };
    use crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE as EVT_XDG_TOPLEVEL_CONFIGURE;
    use crate::protocols::xdg_shell::xdg_toplevel::{REQ_UNSET_FULLSCREEN, REQ_UNSET_MAXIMIZED};
    use crate::state::{
        Context, HostActivationState, PlacementBarrierCleanup, PlacementTarget, TextInputState,
        TransientArcIdentity, WindowPlacementGeometry, WindowPlacementMode, WindowPlacementPlan,
        WindowPlacementPlanError, XdgToplevelRelease,
    };
    use crate::wire::Action;

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
        ctx.shadow_table
            .track_interface_with_version(10, "xdg_toplevel".to_string(), 6);
        ctx.shadow_table
            .track_interface_with_version(11, "wl_surface".to_string(), 6);
        ctx.shadow_table.track_host_interface_with_version(
            host_xdg_toplevel_id,
            "xdg_toplevel".to_string(),
            6,
        );
        ctx.shadow_table
            .track_host_interface_with_version(12, "wl_surface".to_string(), 6);
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
        assert!(ctx.window_placement.remember_xdg_surface(9, 11));
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
        PlacementTarget::for_test(
            10,
            11,
            12,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            zaura_surface_version,
        )
    }

    fn acknowledge_synthetic_resize(ctx: &mut Context) {
        let serial = ctx
            .host_to_client_queue
            .iter()
            .rev()
            .find(|message| sender(message) == 9 && opcode(message) == EVT_XDG_SURFACE_CONFIGURE)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("synthetic resize configure");
        assert!(ctx
            .window_placement
            .consume_synthetic_xdg_configure_serial(9, serial));
        assert!(
            ctx.window_placement.note_guest_surface_commit(11),
            "synthetic resize requires both guest ack and commit"
        );
    }

    fn direct_plan() -> WindowPlacementPlan {
        WindowPlacementPlan::for_test(
            placement_target(20, 30, 40, 5),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            None,
            None,
        )
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
    fn aura_release_adapter_serializes_without_mutating_placement_state() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(&mut ctx, 20, 30, 40, 5);
        ctx.client_to_host_queue.clear();

        queue_zaura_toplevel_release(
            &mut ctx,
            XdgToplevelRelease {
                wl_surface_guest_id: 11,
                zaura_toplevel_host_id: Some(30),
            },
        );

        assert_eq!(
            ctx.window_placement.aura_toplevel_for_xdg_toplevel(10),
            Some(30),
            "wire serialization must not own or mutate placement lifecycle state"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(
            opcode(&ctx.client_to_host_queue[0]),
            REQ_RELEASE_AURA_TOPLEVEL
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
        let plan = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: zaura_surface_id + 1,
            }),
        );

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
        // Runtime XDG roles have a live host xdg_surface mapping. Add it to
        // this fixture so the regression test exercises the host-side
        // geometry request that performs the native Guest resize.
        ctx.shadow_table.map_id(9, 13);
        ctx.shadow_table
            .track_interface_with_version(9, "xdg_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(13, "xdg_surface".to_string(), 6);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));
        let plan = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (500, 700, 800, 600),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (400, 500),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );

        assert!(queue_window_placement(&mut ctx, &plan,));
        let opcodes: Vec<_> = ctx.client_to_host_queue.iter().map(opcode).collect();
        assert_eq!(
            opcodes,
            vec![
                REQ_SET_WINDOW_GEOMETRY,
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
            ]
        );
        assert_eq!(sender(&ctx.client_to_host_queue[0]), 13);
        assert_eq!(
            &ctx.client_to_host_queue[0].0[8..24],
            &[
                0i32.to_ne_bytes(),
                0i32.to_ne_bytes(),
                800i32.to_ne_bytes(),
                600i32.to_ne_bytes(),
            ]
            .concat()
        );
        assert_eq!(
            ctx.window_placement.origin(zaura_toplevel_id),
            Some((100, 200))
        );
        assert_eq!(
            ctx.window_placement.pending_origin(zaura_toplevel_id),
            Some((500, 700))
        );
        assert_eq!(
            ctx.window_placement.pending_resize_size(zaura_toplevel_id),
            Some((800, 600))
        );
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![EVT_XDG_TOPLEVEL_CONFIGURE, EVT_XDG_SURFACE_CONFIGURE]
        );
        assert_eq!(sender(&ctx.host_to_client_queue[0]), 10);
        assert_eq!(sender(&ctx.host_to_client_queue[1]), 9);
        assert_eq!(
            &ctx.host_to_client_queue[0].0[8..16],
            &[800i32.to_ne_bytes(), 600i32.to_ne_bytes()].concat()[..]
        );
        let synthetic_serial = u32::from_ne_bytes(
            ctx.host_to_client_queue[1].0[8..12]
                .try_into()
                .expect("synthetic xdg_surface.configure serial"),
        );
        assert!(synthetic_serial >= 0xf000_0000);
    }

    #[test]
    fn rapid_self_parent_shortcuts_publish_only_the_latest_deferred_target() {
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
        ctx.shadow_table.map_id(9, 13);
        ctx.shadow_table
            .track_interface_with_version(9, "xdg_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(13, "xdg_surface".to_string(), 6);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));

        let first = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (500, 700, 1920, 2160),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (400, 500),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );
        let second = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (0, 0, 1920, 2160),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (-100, -200),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );
        let latest = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (1920, 0, 1920, 2160),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (1820, -200),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );

        assert!(queue_window_placement(&mut ctx, &first));
        acknowledge_synthetic_resize(&mut ctx);
        let first_client_queue_len = ctx.client_to_host_queue.len();
        let first_guest_queue_len = ctx.host_to_client_queue.len();
        assert!(queue_window_placement(&mut ctx, &second));
        assert!(queue_window_placement(&mut ctx, &latest));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            first_client_queue_len,
            "rapid updates must not append stale host geometry/unset operations"
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            first_guest_queue_len,
            "rapid updates must not append stale synthetic configures"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            Some((1920, 0, 1920, 2160))
        );

        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                100,
                200,
                1920,
                2112,
                &[],
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement.pending_resize_size(zaura_toplevel_id),
            None,
            "a decorated client-size report must still allow a same-size \
             deferred move"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| opcode(message) == REQ_SET_WINDOW_GEOMETRY)
                .count(),
            1,
            "a same-size deferred move must not publish a second geometry phase"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            Some((1920, 0, 1920, 2160)),
            "a deferred shortcut must wait for the first parent origin and cleanup barrier"
        );

        // The host must acknowledge the first move's target origin before
        // cleanup can promote the deferred shortcut. This is deliberately an
        // explicit origin event; the preceding sync callback is only an
        // ordering boundary.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                500,
                700,
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| opcode(message) == REQ_SET_PARENT)
                .count(),
            1,
            "intermediate origin events must not duplicate the parent move"
        );

        // The same-size deferred move must register its cleanup barrier after
        // promotion, so the callback belongs to the new generation and can
        // run the ordered IME refresh without changing the persistent
        // self-parent relationship.
        let move_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("same-size deferred move barrier callback");
        let parent_count_before_cleanup = ctx
            .client_to_host_queue
            .iter()
            .filter(|message| opcode(message) == REQ_SET_PARENT)
            .count();
        ctx.last_sender_id = move_callback;
        assert_eq!(
            CallbackHandler.on_done(&mut ctx, 3),
            Action::Drop,
            "same-size move barrier must be consumed internally"
        );
        assert!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| opcode(message) == REQ_SET_PARENT)
                .count()
                == parent_count_before_cleanup,
            "barrier completion must not queue a nullable-unparent request"
        );
        let cleanup_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("same-size cleanup barrier callback");
        assert_ne!(cleanup_callback, move_callback);
        ctx.last_sender_id = cleanup_callback;
        assert_eq!(
            CallbackHandler.on_done(&mut ctx, 4),
            Action::Drop,
            "same-size cleanup barrier must retire the promoted generation"
        );
        assert_eq!(
            ctx.window_placement
                .active_self_parent_size(zaura_toplevel_id),
            Some((1920, 2160)),
            "the latest deferred target must become the active move after cleanup"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| opcode(message) == REQ_SET_PARENT)
                .count(),
            2,
            "only the first move and the latest deferred move may be published"
        );

        // The promoted move has its own authoritative origin and two ordered
        // barriers. Complete that generation before asserting idle; the
        // previous cleanup callback cannot retire a newly published move.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                1920,
                0,
            ),
            Action::Drop
        );
        let latest_move_callback = ctx
            .window_placement
            .active_barrier_for_toplevel(zaura_toplevel_id)
            .expect("latest deferred move barrier callback");
        assert_ne!(latest_move_callback, cleanup_callback);
        ctx.last_sender_id = latest_move_callback;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 5), Action::Drop);
        let latest_cleanup_callback = ctx
            .window_placement
            .active_barrier_for_toplevel(zaura_toplevel_id)
            .expect("latest deferred cleanup barrier callback");
        assert_ne!(latest_cleanup_callback, latest_move_callback);
        ctx.last_sender_id = latest_cleanup_callback;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 6), Action::Drop);
        assert!(
            !ctx.window_placement
                .self_parent_transaction_active(zaura_toplevel_id),
            "completed same-size deferred placement must return to idle"
        );
    }

    #[test]
    fn deferred_self_parent_target_waits_for_unparent_and_ime_barriers() {
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
        ctx.shadow_table.map_id(9, 13);
        ctx.shadow_table
            .track_interface_with_version(9, "xdg_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(13, "xdg_surface".to_string(), 6);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));

        let first = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (500, 700, 800, 600),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (400, 500),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );
        let latest = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (1920, 0, 1920, 2160),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (1820, -200),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );

        assert!(queue_window_placement(&mut ctx, &first));
        acknowledge_synthetic_resize(&mut ctx);
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                100,
                200,
                800,
                600,
                &[],
            ),
            Action::Drop
        );
        let first_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("first self-parent barrier callback");
        ctx.last_sender_id = first_callback;
        assert_eq!(
            crate::protocols::wayland::wl_callback::WlCallbackHandler::on_done(
                &mut CallbackHandler,
                &mut ctx,
                1,
            ),
            Action::Drop
        );

        // A real Aura origin acknowledgement is required before the
        // follow-up IME barrier may promote a deferred target. `sync.done`
        // alone does not carry screen-space geometry.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                500,
                700,
            ),
            Action::Drop
        );

        // The first callback has queued the follow-up activation barrier. The
        // latest shortcut is retained, not appended to the host stream while
        // that cleanup is in flight.
        let queue_len_before_deferred = ctx.client_to_host_queue.len();
        assert!(queue_window_placement(&mut ctx, &latest));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            queue_len_before_deferred,
            "a shortcut during cleanup must not publish a second operation"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            Some((1920, 0, 1920, 2160))
        );

        let followup_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("follow-up IME barrier callback");
        ctx.last_sender_id = followup_callback;
        assert_eq!(
            crate::protocols::wayland::wl_callback::WlCallbackHandler::on_done(
                &mut CallbackHandler,
                &mut ctx,
                2,
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement.pending_resize_size(zaura_toplevel_id),
            Some((1920, 2160)),
            "the cleanup barrier is sufficient to promote a deferred target \
             when the host omits origin_change"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            None,
            "the promoted target must not remain deferred after cleanup"
        );

        // A late intermediate coordinate from the old parent generation must
        // not rebase the newly promoted resize phase.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                500,
                700,
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement.pending_resize_size(zaura_toplevel_id),
            Some((1920, 2160)),
            "a late origin must not cancel the promoted resize"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            None
        );

        // The host may still report an intermediate animation origin while
        // the newly promoted resize is pending. It must not enqueue another
        // parent request or rebase the target.
        let parent_count_before_origin = ctx
            .client_to_host_queue
            .iter()
            .filter(|message| opcode(message) == REQ_SET_PARENT)
            .count();
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.window_placement.pending_resize_size(zaura_toplevel_id),
            Some((1920, 2160)),
            "an intermediate origin must not cancel the promoted resize"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            None
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| opcode(message) == REQ_SET_PARENT)
                .count(),
            parent_count_before_origin,
            "an intermediate origin must not duplicate the parent request"
        );
    }

    #[test]
    fn late_origin_after_completion_does_not_rebase_the_same_shortcut() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Guest,
                crate::state::WindowGeometryMethod::SelfParent,
            ));
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            2,
        );
        ctx.shadow_table.map_id(9, 13);
        ctx.shadow_table
            .track_interface_with_version(9, "xdg_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(13, "xdg_surface".to_string(), 6);
        assert!(ctx.window_placement.remember_output(50));
        ctx.window_placement
            .update_output_mode(50, true, 1920, 1080);
        ctx.window_placement.update_output_scale(50, 1);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));

        let rect = crate::window_shortcuts::NormalizedRect::new(0.0, 0.0, 0.5, 1.0);
        let first = ctx
            .window_placement
            .prepare_placement(&ctx.shadow_table, 10, 11, rect)
            .expect("first self-parent shortcut should produce a plan");
        assert!(queue_window_placement(&mut ctx, &first));
        acknowledge_synthetic_resize(&mut ctx);

        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_configure(
                &mut CompositorHandler,
                &mut ctx,
                100,
                200,
                960,
                1080,
                &[],
            ),
            Action::Drop
        );
        let first_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("first self-parent barrier callback");
        ctx.last_sender_id = first_callback;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 1), Action::Drop);
        // The placement barrier only orders host processing. The first
        // generation is complete once Aura reports the requested screen
        // origin.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                0,
                0,
            ),
            Action::Drop
        );
        let cleanup_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("self-parent cleanup barrier callback");
        assert_ne!(cleanup_callback, first_callback);
        ctx.last_sender_id = cleanup_callback;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 2), Action::Drop);
        assert!(
            !ctx.window_placement
                .self_parent_transaction_active(zaura_toplevel_id),
            "the first placement must be fully idle before testing an external move"
        );

        // A focus/activation transition can emit a late animation origin
        // after self-parent cleanup. Do not rebase the next relative request
        // on that intermediate coordinate: it would make repeated shortcuts
        // drift toward the lower-right corner.
        ctx.last_sender_id = zaura_toplevel_id;
        assert_eq!(
            crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler::on_origin_change(
                &mut CompositorHandler,
                &mut ctx,
                500,
                700,
            ),
            Action::Drop
        );
        assert_eq!(ctx.window_placement.origin(zaura_toplevel_id), Some((0, 0)));
        assert_eq!(
            ctx.window_placement
                .prepare_placement(&ctx.shadow_table, 10, 11, rect)
                .expect_err("late origin must not turn a duplicate into a new move"),
            WindowPlacementPlanError::AlreadyAtTarget
        );
    }

    #[test]
    fn remote_shell_adapter_uses_official_bounds_request_then_surface_commit() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(WindowPlacementMode::remote_shell());
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "xdg_toplevel".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(20, "xdg_toplevel".to_string(), 6);
        ctx.shadow_table.map_id(11, 12);
        ctx.shadow_table
            .track_interface_with_version(11, "wl_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(12, "wl_surface".to_string(), 6);
        ctx.shadow_table.track_host_interface_with_version(
            40,
            "zcr_remote_surface_v2".to_string(),
            6,
        );
        assert!(ctx.window_placement.remember_xdg_surface(9, 11));
        assert!(ctx.window_placement.remember_xdg_toplevel(10, 11));
        assert!(ctx.window_placement.remember_remote_surface(12, 40));

        let plan = WindowPlacementPlan::for_test(
            PlacementTarget::for_test(10, 11, 12, 20, 0, 40, 6),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::RemoteShell,
            None,
            None,
        );
        assert!(queue_window_placement(&mut ctx, &plan));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![
                crate::protocols::remote_shell_unstable_v2::zcr_remote_surface_v2::
                    REQ_SET_BOUNDS_IN_OUTPUT,
                crate::protocols::wayland::wl_surface::REQ_COMMIT,
            ]
        );
        assert_eq!(sender(&ctx.client_to_host_queue[0]), 40);
        assert_eq!(sender(&ctx.client_to_host_queue[1]), 12);
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
        let plan = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            Some(TransientArcIdentity::for_test(
                "org.chromium.arc.2000000001".to_string(),
                11,
            )),
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id,
                wl_surface_guest_id: 11,
            }),
        );

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
            plan.barrier_cleanup().cloned()
        );
    }

    #[test]
    fn transient_placement_drains_focused_ime_before_arc_identity_transition() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let guest_surface_id = 11;
        let guest_seat_id = 90;
        let host_seat_id = 91;
        let guest_text_input_id = 70;
        let host_text_input_id = 71;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            5,
        );
        ctx.shadow_table.map_id(guest_seat_id, host_seat_id);
        ctx.shadow_table
            .track_interface(guest_seat_id, "wl_seat".to_string());
        ctx.shadow_table
            .track_host_interface(host_text_input_id, "zwp_text_input_v1".to_string());
        ctx.text_inputs.insert(
            guest_text_input_id,
            TextInputState {
                host_activation: HostActivationState::Active,
                committed_enabled: true,
                ..TextInputState::new(
                    host_text_input_id,
                    None,
                    guest_seat_id,
                    Some(guest_surface_id),
                )
            },
        );
        let plan = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            Some(TransientArcIdentity::for_test(
                "org.chromium.arc.2000000001".to_string(),
                guest_surface_id,
            )),
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id,
                wl_surface_guest_id: guest_surface_id,
            }),
        );

        assert!(queue_window_placement(&mut ctx, &plan));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![
                crate::protocols::text_input_unstable_v1::zwp_text_input_v1::REQ_RESET,
                1,
                REQ_SET_APPLICATION_ID,
                REQ_UNSET_FULLSCREEN,
                REQ_UNSET_MAXIMIZED,
                REQ_UNSET_SNAP,
                REQ_SET_WINDOW_BOUNDS,
                REQ_SYNC,
            ],
            "the old IME generation must be drained before ARC identity changes"
        );
        assert_eq!(
            ctx.text_inputs[&guest_text_input_id].host_activation(),
            HostActivationState::Inactive,
            "placement must not leave the host IME marked active while ARC metadata changes"
        );
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            Vec::<u16>::new(),
            "transient placement must not synthesize guest focus loss before changing host identity"
        );
    }

    #[test]
    fn transient_placement_refreshes_an_inactive_committed_guest_without_leave() {
        let guest_surface_id = 11;
        let guest_text_input_id = 70;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.text_inputs.insert(
            guest_text_input_id,
            TextInputState {
                committed_enabled: true,
                host_activation: HostActivationState::Inactive,
                ..TextInputState::new(71, None, 90, Some(guest_surface_id))
            },
        );

        let preflight = crate::handler::text_input::prepare_placement_ime_deactivation(
            &mut ctx,
            guest_surface_id,
        )
        .expect("committed guest IME should be eligible for refresh");
        assert_eq!(preflight.guest_text_inputs, vec![guest_text_input_id]);
        assert!(
            preflight.host_messages.is_empty(),
            "an already inactive host generation needs no duplicate deactivate"
        );
        assert!(crate::handler::text_input::commit_placement_ime_preflight(
            &mut ctx, &preflight
        ));
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "placement must not send a synthetic leave that makes Winit clear ime_allowed"
        );
        assert!(ctx.text_inputs[&guest_text_input_id].placement_ime_pending_for(guest_surface_id));

        assert!(
            crate::handler::text_input::resume_placement_ime_for_surface(
                &mut ctx,
                guest_surface_id
            )
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(opcode(&ctx.host_to_client_queue[0]), 0);
        assert_eq!(sender(&ctx.host_to_client_queue[0]), guest_text_input_id);
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
        let plan = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 5),
            7,
            (100, 200, 800, 600),
            WindowPlacementGeometry::Bounds,
            Some(TransientArcIdentity::for_test("x".repeat(65_520), 10)),
            Some(PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id,
                wl_surface_guest_id: 10,
            }),
        );

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
    fn transient_cleanup_restores_native_identity_without_unparenting() {
        let surface_id = 55;
        let native_id = "org.chromium.guest_os.termina.wayland.test";
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );
        assert!(ctx.window_placement.remember_xdg_surface(9, 11));
        assert!(ctx.window_placement.remember_xdg_toplevel(10, 11));
        assert!(ctx.window_placement.remember_aura_toplevel(10, 30));
        ctx.window_placement
            .remember_native_application_id(91, native_id.to_string());

        assert!(queue_barrier_cleanup(
            &mut ctx,
            30,
            &PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: 91,
            }
        ));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "identity restoration must be followed by its own host sync"
        );
        assert_eq!(opcode(&ctx.client_to_host_queue[0]), REQ_SET_APPLICATION_ID);
        assert_eq!(opcode(&ctx.client_to_host_queue[1]), REQ_SYNC);
    }

    #[test]
    fn transient_cleanup_refreshes_ime_after_identity_restore() {
        let surface_id = 55;
        let guest_surface_id = 91;
        let guest_text_input_id = 20;
        let host_text_input_id = 30;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(0, 2);
        ctx.shadow_table.track_interface(0, "wl_seat".to_string());
        ctx.shadow_table
            .track_host_interface(host_text_input_id, "zwp_text_input_v1".to_string());
        ctx.shadow_table
            .map_id(guest_text_input_id, host_text_input_id);
        ctx.text_inputs.insert(
            guest_text_input_id,
            TextInputState {
                host_activation: HostActivationState::Active,
                committed_enabled: true,
                ..TextInputState::new(host_text_input_id, None, 0, Some(guest_surface_id))
            },
        );
        ctx.shadow_table.map_id(guest_surface_id, 92);
        ctx.shadow_table
            .track_interface(guest_surface_id, "wl_surface".to_string());
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            5,
        );
        ctx.window_placement.remember_native_application_id(
            guest_surface_id,
            "org.chromium.guest_os.termina.wayland.test".to_string(),
        );
        assert!(ctx
            .window_placement
            .remember_xdg_surface(9, guest_surface_id));
        assert!(ctx
            .window_placement
            .remember_xdg_toplevel(10, guest_surface_id));
        assert!(ctx.window_placement.remember_aura_toplevel(10, 30));

        assert!(queue_barrier_cleanup(
            &mut ctx,
            30,
            &PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: guest_surface_id,
            }
        ));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![REQ_SET_APPLICATION_ID, REQ_SYNC]
        );
        assert!(
            ctx.text_inputs[&guest_text_input_id]
                .draining_callback()
                .is_none(),
            "IME refresh must wait for the identity barrier"
        );

        let identity_callback_id = u32::from_ne_bytes(
            ctx.client_to_host_queue[1].0[8..12]
                .try_into()
                .expect("identity sync callback payload"),
        );
        ctx.last_sender_id = identity_callback_id;
        assert_eq!(
            CallbackHandler.on_done(&mut ctx, 0),
            Action::Drop,
            "identity barrier must be consumed internally"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![
                REQ_SET_APPLICATION_ID,
                REQ_SYNC,
                crate::protocols::text_input_unstable_v1::zwp_text_input_v1::REQ_RESET,
                1,
                REQ_SYNC
            ]
        );
        assert!(
            ctx.text_inputs[&guest_text_input_id]
                .draining_callback()
                .is_some(),
            "IME deactivate must be queued only after identity sync.done"
        );
    }

    #[test]
    fn transient_cleanup_reactivates_ime_after_preflight_deactivation() {
        let guest_surface_id = 91;
        let guest_text_input_id = 20;
        let host_text_input_id = 30;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(0, 2);
        ctx.shadow_table.track_interface(0, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_surface_id, 92);
        ctx.shadow_table
            .track_interface(guest_surface_id, "wl_surface".to_string());
        ctx.shadow_table
            .track_host_interface(host_text_input_id, "zwp_text_input_v1".to_string());
        ctx.shadow_table
            .map_id(guest_text_input_id, host_text_input_id);
        ctx.text_inputs.insert(
            guest_text_input_id,
            TextInputState {
                host_activation: HostActivationState::Inactive,
                committed_enabled: true,
                ..TextInputState::new(host_text_input_id, None, 0, Some(guest_surface_id))
            },
        );

        assert!(queue_barrier_cleanup(
            &mut ctx,
            30,
            &PlacementBarrierCleanup::RefreshHostActivation { guest_surface_id }
        ));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![crate::protocols::text_input_unstable_v1::zwp_text_input_v1::REQ_ACTIVATE],
            "a transient preflight must reactivate only after identity cleanup"
        );
        assert_eq!(
            ctx.text_inputs[&guest_text_input_id].host_activation(),
            HostActivationState::Active
        );
    }

    #[test]
    fn self_parent_cleanup_refreshes_focused_ime_without_unparent() {
        let surface_id = 55;
        let guest_surface_id = 91;
        let guest_text_input_id = 20;
        let host_text_input_id = 30;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(0, 2);
        ctx.shadow_table.track_interface(0, "wl_seat".to_string());
        ctx.shadow_table
            .map_id(guest_surface_id, guest_surface_id + 1);
        ctx.shadow_table
            .track_interface(guest_surface_id, "wl_surface".to_string());
        ctx.shadow_table
            .track_host_interface(host_text_input_id, "zwp_text_input_v1".to_string());
        ctx.shadow_table
            .map_id(guest_text_input_id, host_text_input_id);
        ctx.text_inputs.insert(
            guest_text_input_id,
            TextInputState {
                host_activation: HostActivationState::Active,
                committed_enabled: true,
                ..TextInputState::new(host_text_input_id, None, 0, Some(guest_surface_id))
            },
        );
        ctx.shadow_table.track_host_interface_with_version(
            surface_id,
            "zaura_surface".to_string(),
            2,
        );
        assert!(ctx
            .window_placement
            .remember_xdg_surface(9, guest_surface_id));
        assert!(ctx
            .window_placement
            .remember_xdg_toplevel(10, guest_surface_id));
        assert!(ctx.window_placement.remember_aura_toplevel(10, 30));
        assert!(ctx
            .window_placement
            .remember_aura_surface(guest_surface_id + 1, surface_id));

        assert!(queue_barrier_cleanup(
            &mut ctx,
            30,
            &PlacementBarrierCleanup::RetainSelfParent {
                zaura_surface_id: surface_id,
            }
        ));
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![REQ_SYNC],
            "self-parent cleanup keeps only the ordered IME barrier on the host stream"
        );
        assert!(ctx.text_inputs[&guest_text_input_id]
            .draining_callback()
            .is_none());

        let placement_callback = ctx
            .client_to_host_queue
            .iter()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("persistent self-parent follow-up barrier callback");
        ctx.last_sender_id = placement_callback;
        assert_eq!(
            CallbackHandler.on_done(&mut ctx, 0),
            Action::Drop,
            "the follow-up barrier must be consumed before IME refresh"
        );
        assert_eq!(
            ctx.client_to_host_queue
                .iter()
                .map(opcode)
                .collect::<Vec<_>>(),
            vec![
                REQ_SYNC,
                crate::protocols::text_input_unstable_v1::zwp_text_input_v1::REQ_RESET,
                1,
                REQ_SYNC,
            ],
            "self-parent cleanup must refresh IME after its own sync without \
             sending set_parent(NULL)"
        );
        let ime_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("IME refresh barrier callback");
        assert_ne!(ime_callback, placement_callback);
        assert!(ctx.text_inputs[&guest_text_input_id]
            .draining_callback()
            .is_some());

        ctx.last_sender_id = ime_callback;
        assert_eq!(
            CallbackHandler.on_done(&mut ctx, 0),
            Action::Drop,
            "the host IME barrier must settle its generation"
        );
        assert!(ctx.text_inputs[&guest_text_input_id]
            .draining_callback()
            .is_none());
        assert_eq!(
            ctx.text_inputs[&guest_text_input_id].host_activation(),
            HostActivationState::Active
        );
        assert!(
            ctx.client_to_host_queue.iter().any(|message| {
                opcode(message)
                    == crate::protocols::text_input_unstable_v1::zwp_text_input_v1::REQ_ACTIVATE
            }),
            "IME refresh must reactivate the host text input after sync.done"
        );
    }

    #[test]
    fn self_parent_cleanup_does_not_require_nullable_unparent() {
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        track_placement_objects(&mut ctx, 20, zaura_toplevel_id, zaura_surface_id, 1);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));
        assert!(ctx.window_placement.arm_self_parent_target(
            zaura_toplevel_id,
            zaura_surface_id,
            (0, 0, 800, 600),
        ));
        assert!(ctx.window_placement.accept_pending_resize_at_origin(
            zaura_toplevel_id,
            (800, 600),
            (100, 200),
        ));
        assert!(ctx
            .window_placement
            .mark_self_parent_move_queued(zaura_toplevel_id));
        assert!(ctx
            .window_placement
            .self_parent_transaction_active(zaura_toplevel_id));

        assert!(queue_barrier_cleanup(
            &mut ctx,
            zaura_toplevel_id,
            &PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id },
        ));
        assert!(
            ctx.client_to_host_queue.iter().map(opcode).eq([REQ_SYNC]),
            "persistent self-parent cleanup queues only the follow-up barrier"
        );
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|message| opcode(message) != REQ_SET_PARENT),
            "cleanup must never emit set_parent(NULL), even for v1 surfaces"
        );

        // The cleanup barrier does not prove that the host applied the
        // requested position. Supply the authoritative origin event before
        // completing the barrier.
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (0, 0)));

        let cleanup_callback = ctx
            .client_to_host_queue
            .iter()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("persistent cleanup barrier callback");
        ctx.last_sender_id = cleanup_callback;
        assert_eq!(
            WlCallbackHandler::on_done(&mut CallbackHandler, &mut ctx, 1),
            Action::Drop
        );
        assert!(
            !ctx.window_placement
                .self_parent_transaction_active(zaura_toplevel_id),
            "the ordered barrier must still settle the transaction"
        );
    }

    #[test]
    fn omitted_origin_cleanup_retires_and_drops_deferred_shortcut() {
        let host_xdg_toplevel_id = 20;
        let zaura_toplevel_id = 30;
        let zaura_surface_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Guest,
                crate::state::WindowGeometryMethod::SelfParent,
            ));
        track_placement_objects(
            &mut ctx,
            host_xdg_toplevel_id,
            zaura_toplevel_id,
            zaura_surface_id,
            2,
        );
        assert!(ctx.window_placement.remember_output(50));
        ctx.window_placement
            .update_output_mode(50, true, 1920, 1080);
        ctx.window_placement.update_output_scale(50, 1);
        assert!(ctx
            .window_placement
            .record_origin(zaura_toplevel_id, (100, 200)));
        assert!(ctx.window_placement.arm_self_parent_target(
            zaura_toplevel_id,
            zaura_surface_id,
            (0, 0, 800, 600),
        ));
        assert!(ctx.window_placement.accept_pending_resize_at_origin(
            zaura_toplevel_id,
            (800, 600),
            (100, 200),
        ));
        assert!(ctx
            .window_placement
            .mark_self_parent_move_queued(zaura_toplevel_id));

        // This models the first self-parent barrier after the host has
        // acknowledged the resize. No target origin event is delivered before
        // the ordered IME follow-up callback.
        assert!(queue_barrier_cleanup(
            &mut ctx,
            zaura_toplevel_id,
            &PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id },
        ));
        let deferred = WindowPlacementPlan::for_test(
            placement_target(host_xdg_toplevel_id, zaura_toplevel_id, zaura_surface_id, 2),
            7,
            (1920, 0, 800, 600),
            WindowPlacementGeometry::SelfParent {
                current_origin: (100, 200),
                relative_position: (1820, -200),
            },
            None,
            Some(PlacementBarrierCleanup::RetainSelfParent { zaura_surface_id }),
        );
        assert!(
            ctx.window_placement.commit_placement_plan(&deferred),
            "a newer shortcut should be retained while cleanup is pending"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            Some((1920, 0, 800, 600))
        );

        let cleanup_callback = ctx
            .client_to_host_queue
            .iter()
            .rev()
            .find(|message| opcode(message) == REQ_SYNC)
            .map(|message| u32::from_ne_bytes(message.0[8..12].try_into().unwrap()))
            .expect("ordered self-parent cleanup callback");
        ctx.last_sender_id = cleanup_callback;
        assert_eq!(
            WlCallbackHandler::on_done(&mut CallbackHandler, &mut ctx, 1),
            Action::Drop
        );
        assert!(
            ctx.window_placement
                .self_parent_transaction_active(zaura_toplevel_id),
            "cleanup omission must promote the deferred target instead of \
             stranding or dropping it"
        );
        assert_eq!(
            ctx.window_placement
                .deferred_self_parent_target(zaura_toplevel_id),
            None,
            "the deferred target should be consumed by the promoted generation"
        );
        assert_eq!(
            ctx.window_placement.origin(zaura_toplevel_id),
            Some((0, 0)),
            "the fallback baseline must be the requested first target"
        );
        assert_eq!(
            ctx.window_placement.confirmed_origin(zaura_toplevel_id),
            Some((0, 0)),
            "the requested target is authoritative after the ordered barrier"
        );
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
        assert!(ctx.window_placement.remember_xdg_surface(9, 11));
        assert!(ctx.window_placement.remember_xdg_toplevel(10, 11));
        assert!(ctx.window_placement.remember_aura_toplevel(10, 30));

        assert!(!queue_barrier_cleanup(
            &mut ctx,
            30,
            &PlacementBarrierCleanup::RestoreNativeApplicationId {
                zaura_surface_id: surface_id,
                wl_surface_guest_id: 91,
            }
        ));
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "without a native ID there must be no speculative unparent request"
        );
    }

    #[test]
    fn transient_cleanup_drops_released_surface_without_stale_wire() {
        let surface_id = 55;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement.set_aura_shell_binding_for_test(300, 5);
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
            30,
            &PlacementBarrierCleanup::RestoreNativeApplicationId {
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
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::arc_bounds(
                crate::state::WindowArcIdLifetime::PersistentNativeShell,
            ));

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
