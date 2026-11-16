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

//! Experimental `zcr_remote_shell_v2` placement backend.
//!
//! Selected with `SOMMELIER_WINDOW_PLACEMENT_REMOTE_SHELL_V2`; every entry
//! point is a no-op otherwise. The remote-shell global is never advertised to
//! guests: the registry binds it host-side only.
//!
//! Exo gives a `wl_surface` one shell role, and a remote-surface role cannot
//! coexist with an XDG role. When a manager is bound, the guest's XDG objects
//! are therefore answered locally ("facades"), and the host surface gets a
//! `zcr_remote_surface_v2` role:
//!
//! - `xdg_surface` → facade. `get_toplevel` creates the remote surface, sets
//!   its scale factor to 1.0 (client coordinates are DIPs), and synthesizes
//!   the initial XDG configure. `get_popup` creates a facade popup that is
//!   dismissed at once with `popup_done`, because a remote surface cannot
//!   parent an XDG popup.
//! - `xdg_toplevel` → facade. `set_title` and `set_app_id` (Guest OS
//!   namespace) go to the remote surface. Requests without a remote-surface
//!   equivalent are consumed.
//! - Remote `close` → `xdg_toplevel.close`. Remote `bounds_changed_in_output`
//!   is a compositor request in Exo's client-controlled model. It is accepted
//!   by echoing `set_bounds_in_output` and configuring the guest to the new
//!   size.
//! - Placement: [`queue_window_bounds`] sends `set_bounds_in_output` relative
//!   to the output and configures the guest. Exo applies the bounds on the
//!   guest's next `wl_surface.commit`, so no sync barrier is needed.
//!
//! If the host does not advertise the global (stock Crostini does not), no
//! surface is claimed. Windows use the normal XDG path and placement
//! shortcuts stay with the client.

use std::os::unix::io::RawFd;

use crate::handler::display::queue_protocol_error;
use crate::protocols::remote_shell_unstable_v2::{zcr_remote_shell_v2, zcr_remote_surface_v2};
use crate::protocols::xdg_shell::{xdg_popup, xdg_surface, xdg_toplevel};
use crate::state::{Context, OutputState, RemoteShellFacade, REMOTE_SHELL_V2_ENV};
use crate::wire::{Action, MessageBuilder, ProtocolError, WireMessage};

/// Lowest manager version the backend binds: `set_scale_factor` (v4) is
/// needed to pin client coordinates to DIPs.
pub(crate) const MIN_VERSION: u32 = 4;
/// Highest version of the vendored protocol.
const MAX_VERSION: u32 = 6;
/// `zcr_remote_shell_v2.container.default`.
const CONTAINER_DEFAULT: u32 = 1;
/// `xdg_surface.error.already_constructed`.
const XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED: u32 = 2;
/// Interface name of host remote surfaces in the shadow table.
const REMOTE_SURFACE_INTERFACE: &str = "zcr_remote_surface_v2";

type DispatchResult = Result<Option<(Vec<u8>, Vec<RawFd>)>, ProtocolError>;

/// Return the version to bind a host manager at, or `None` when the host
/// version is too old for this backend.
pub(crate) fn bind_version(host_version: u32) -> Option<u32> {
    (host_version >= MIN_VERSION).then(|| host_version.min(MAX_VERSION))
}

// ── Facade lifecycle ──────────────────────────────────────────────────────

/// Claim a new guest `xdg_surface` for the remote-shell backend.
///
/// Called from `xdg_wm_base.get_xdg_surface` after the generated dispatcher
/// has validated `xdg_surface_guest_id` as new and `wl_surface_guest_id` as a
/// live surface. `ctx.last_sender_id` is the guest `xdg_wm_base`.
///
/// # Returns
///
/// `true` when the surface became a facade and the request must not be
/// forwarded. `false` when the backend is disabled or no host manager is
/// bound; the caller then takes the normal XDG path.
pub(crate) fn claim_xdg_surface(
    ctx: &mut Context,
    xdg_surface_guest_id: u32,
    wl_surface_guest_id: u32,
) -> bool {
    let Some(remote_shell) = ctx.window_placement.remote_shell_mut() else {
        return false;
    };
    if remote_shell.manager().is_none() {
        if remote_shell.note_missing_manager() {
            log::warn!(
                "{} is set but the host did not provide zcr_remote_shell_v2 v{}+; \
                 windows use the normal XDG path and placement shortcuts are inactive",
                REMOTE_SHELL_V2_ENV,
                MIN_VERSION
            );
        }
        return false;
    }
    let version = ctx
        .shadow_table
        .guest_object_version(ctx.last_sender_id)
        .unwrap_or(1);
    register_facade(
        ctx,
        xdg_surface_guest_id,
        "xdg_surface",
        version,
        RemoteShellFacade::Surface {
            wl_surface_guest_id,
            constructed: false,
        },
    );
    ctx.xdg_surface_to_wl_surface
        .insert(xdg_surface_guest_id, wl_surface_guest_id);
    true
}

/// Register a facade guest object.
///
/// Facades are local-only: they have no host mapping. Reserving a host ID
/// that never reaches the wire would leave a gap in the client's object ID
/// sequence, and libwayland-server rejects a later `new_id` past that gap.
/// Without a mapping, a forwarded request that references a facade is
/// dropped by the generated dispatcher instead of carrying a bogus ID.
fn register_facade(
    ctx: &mut Context,
    guest_id: u32,
    interface: &str,
    version: u32,
    facade: RemoteShellFacade,
) {
    ctx.shadow_table
        .track_local_only_guest_object(guest_id, interface.to_string(), version);
    if let Some(remote_shell) = ctx.window_placement.remote_shell_mut() {
        remote_shell.remember_facade(guest_id, facade);
    }
}

/// Destroy a facade guest object: there is no host object to acknowledge
/// the destructor, so the proxy sends `wl_display.delete_id` itself.
fn retire_facade(ctx: &mut Context, guest_id: u32) {
    if let Some(remote_shell) = ctx.window_placement.remote_shell_mut() {
        let _ = remote_shell.take_facade(guest_id);
    }
    crate::handler::display::queue_local_delete_id(ctx, guest_id);
}

/// Give a facade xdg_surface a role object, or report `already_constructed`.
fn construct_xdg_surface(ctx: &mut Context, xdg_surface_guest_id: u32) -> bool {
    let constructed = ctx
        .window_placement
        .remote_shell_mut()
        .is_some_and(|remote_shell| {
            remote_shell.set_xdg_surface_constructed(xdg_surface_guest_id, true)
        });
    if !constructed {
        queue_protocol_error(
            ctx,
            xdg_surface_guest_id,
            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED,
            "xdg_surface already has a role object",
        );
    }
    constructed
}

/// Let a facade xdg_surface accept a new role object after its current one
/// is destroyed. A no-op if the xdg_surface itself is already gone.
fn deconstruct_xdg_surface(ctx: &mut Context, xdg_surface_guest_id: u32) {
    if let Some(remote_shell) = ctx.window_placement.remote_shell_mut() {
        let _ = remote_shell.set_xdg_surface_constructed(xdg_surface_guest_id, false);
    }
}

/// Create the host remote surface for a facade toplevel and send the guest
/// its initial configure.
fn create_remote_toplevel(
    ctx: &mut Context,
    xdg_surface_guest_id: u32,
    xdg_toplevel_guest_id: u32,
) {
    let Some(remote_shell) = ctx.window_placement.remote_shell() else {
        return;
    };
    let (
        Some(RemoteShellFacade::Surface {
            wl_surface_guest_id,
            ..
        }),
        Some(manager),
    ) = (
        remote_shell.facade(xdg_surface_guest_id),
        remote_shell.manager(),
    )
    else {
        log::error!(
            "remote-shell: xdg_surface {} is not a facade with a bound manager",
            xdg_surface_guest_id
        );
        ctx.fatal_protocol_error = true;
        return;
    };
    let Some(wl_surface_host_id) = ctx
        .shadow_table
        .get_host_id(wl_surface_guest_id)
        .filter(|_| {
            !ctx.shadow_table
                .is_pending_destroy_guest(wl_surface_guest_id)
        })
    else {
        log::error!(
            "remote-shell: wl_surface {} of xdg_surface {} is gone",
            wl_surface_guest_id,
            xdg_surface_guest_id
        );
        ctx.fatal_protocol_error = true;
        return;
    };
    if !construct_xdg_surface(ctx, xdg_surface_guest_id) {
        return;
    }

    let remote_surface_host_id = ctx.shadow_table.allocate_host_id();
    let mut builder = MessageBuilder::new();
    builder.write_u32(remote_surface_host_id);
    builder.write_u32(wl_surface_host_id);
    builder.write_u32(CONTAINER_DEFAULT);
    let get_remote_surface =
        builder.try_build_message(manager.host_id, zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE);
    // Client coordinates are scaled by this factor into DIPs. 1.0 makes
    // set_bounds_in_output and bounds_changed_in_output use the same DIP
    // work area as the Aura backend.
    let mut builder = MessageBuilder::new();
    builder.write_u32(1.0f32.to_bits());
    let set_scale_factor = builder.try_build_message(
        remote_surface_host_id,
        zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR,
    );
    let (Ok(get_remote_surface), Ok(set_scale_factor)) = (get_remote_surface, set_scale_factor)
    else {
        log::error!("remote-shell: cannot encode get_remote_surface");
        deconstruct_xdg_surface(ctx, xdg_surface_guest_id);
        ctx.fatal_protocol_error = true;
        return;
    };
    ctx.shadow_table.track_host_interface_with_version(
        remote_surface_host_id,
        REMOTE_SURFACE_INTERFACE.to_string(),
        manager.version,
    );
    ctx.client_to_host_queue
        .push((get_remote_surface, Vec::new()));
    ctx.client_to_host_queue
        .push((set_scale_factor, Vec::new()));

    let version = ctx
        .shadow_table
        .guest_object_version(xdg_surface_guest_id)
        .unwrap_or(1);
    register_facade(
        ctx,
        xdg_toplevel_guest_id,
        "xdg_toplevel",
        version,
        RemoteShellFacade::Toplevel {
            xdg_surface_guest_id,
        },
    );
    if let Some(remote_shell) = ctx.window_placement.remote_shell_mut() {
        remote_shell.remember_remote_surface(xdg_toplevel_guest_id, remote_surface_host_id);
    }
    ctx.xdg_surface_to_xdg_toplevel
        .insert(xdg_surface_guest_id, xdg_toplevel_guest_id);
    ctx.xdg_toplevel_to_wl_surface
        .insert(xdg_toplevel_guest_id, wl_surface_guest_id);
    // XDG clients must receive a configure before their first commit. 0x0
    // lets the client choose its initial size.
    let _ = queue_guest_configure(ctx, xdg_toplevel_guest_id, 0, 0);
    log::debug!(
        "remote-shell: xdg_toplevel {} uses remote surface {} on host wl_surface {}",
        xdg_toplevel_guest_id,
        remote_surface_host_id,
        wl_surface_host_id
    );
}

/// Queue the host destructor of a facade toplevel's remote surface.
fn release_remote_surface(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    let Some(remote_surface_host_id) = ctx
        .window_placement
        .remote_shell_mut()
        .and_then(|remote_shell| remote_shell.take_remote_surface(xdg_toplevel_guest_id))
    else {
        return;
    };
    // Keep the host ID reserved until the host acknowledges the destructor
    // with wl_display.delete_id.
    if ctx
        .shadow_table
        .mark_pending_destroy_host(remote_surface_host_id)
    {
        let message = MessageBuilder::new()
            .build_message(remote_surface_host_id, zcr_remote_surface_v2::REQ_DESTROY);
        ctx.client_to_host_queue.push((message, Vec::new()));
    }
}

/// Destroy the remote-surface roles on a guest wl_surface that is being
/// destroyed, before its own destructor is queued.
///
/// The facade XDG objects stay registered until the guest destroys them.
/// A no-op unless the remote-shell backend is selected.
pub(crate) fn release_surface_roles(ctx: &mut Context, wl_surface_guest_id: u32) {
    let Some(remote_shell) = ctx.window_placement.remote_shell() else {
        return;
    };
    let toplevels = ctx
        .xdg_toplevel_to_wl_surface
        .iter()
        .filter(|&(&toplevel, &surface)| {
            surface == wl_surface_guest_id
                && remote_shell.remote_surface_for_toplevel(toplevel).is_some()
        })
        .map(|(&toplevel, _)| toplevel)
        .collect::<Vec<_>>();
    for toplevel in toplevels {
        release_remote_surface(ctx, toplevel);
    }
}

// ── Guest configure synthesis ─────────────────────────────────────────────

/// Send a facade toplevel `xdg_toplevel.configure` + `xdg_surface.configure`.
///
/// No XDG states are sent: the remote-surface state stream is not bridged.
fn queue_guest_configure(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    width: i32,
    height: i32,
) -> bool {
    let Some(remote_shell) = ctx.window_placement.remote_shell_mut() else {
        return false;
    };
    let Some(RemoteShellFacade::Toplevel {
        xdg_surface_guest_id,
    }) = remote_shell.facade(xdg_toplevel_guest_id)
    else {
        return false;
    };
    if remote_shell.facade(xdg_surface_guest_id).is_none() {
        return false;
    }
    let serial = remote_shell.next_configure_serial();
    if !crate::handler::compositor::queue_guest_xdg_configure(
        ctx,
        xdg_toplevel_guest_id,
        width,
        height,
        &[],
    ) {
        return false;
    }
    crate::handler::compositor::queue_guest_xdg_surface_configure(
        ctx,
        xdg_surface_guest_id,
        serial,
    );
    true
}

/// Queue `set_bounds_in_output` and the matching guest configure as one
/// batch. Both queues are rolled back if either part cannot be queued.
#[allow(clippy::too_many_arguments)]
fn queue_bounds_in_output(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    remote_surface_host_id: u32,
    output_host_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> bool {
    if !ctx
        .shadow_table
        .host_object_matches(remote_surface_host_id, REMOTE_SURFACE_INTERFACE)
    {
        return false;
    }
    let mut builder = MessageBuilder::new();
    builder.write_u32(output_host_id);
    builder.write_i32(x);
    builder.write_i32(y);
    builder.write_i32(width);
    builder.write_i32(height);
    let Ok(message) = builder.try_build_message(
        remote_surface_host_id,
        zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT,
    ) else {
        return false;
    };
    let host_queue_start = ctx.client_to_host_queue.len();
    let guest_queue_start = ctx.host_to_client_queue.len();
    ctx.client_to_host_queue.push((message, Vec::new()));
    if !queue_guest_configure(ctx, xdg_toplevel_guest_id, width, height) {
        ctx.client_to_host_queue.truncate(host_queue_start);
        ctx.host_to_client_queue.truncate(guest_queue_start);
        return false;
    }
    true
}

// ── Placement backend ─────────────────────────────────────────────────────

/// Place a facade toplevel at `bounds`, given in the same screen-space DIPs
/// as [`OutputState::work_area`].
///
/// # Arguments
///
/// * `xdg_toplevel_guest_id` - Focused guest toplevel.
/// * `output_host_id` - Host `wl_output` that holds `bounds`.
/// * `output` - That output's state; its origin converts `bounds` to the
///   output-relative coordinates `set_bounds_in_output` expects.
/// * `bounds` - `(x, y, width, height)` from the placement action.
///
/// # Returns
///
/// `true` when the bounds request and guest configure were queued. `false`
/// leaves the shortcut with the client: the window has no remote surface
/// (for example, the host never advertised the global), or the request
/// cannot be encoded.
pub(crate) fn queue_window_bounds(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    output_host_id: u32,
    output: OutputState,
    (x, y, width, height): (i32, i32, i32, i32),
) -> bool {
    let Some(remote_surface_host_id) = ctx
        .window_placement
        .remote_shell()
        .and_then(|remote_shell| remote_shell.remote_surface_for_toplevel(xdg_toplevel_guest_id))
    else {
        log::debug!(
            "remote-shell: xdg_toplevel {} has no remote surface; placement skipped",
            xdg_toplevel_guest_id
        );
        return false;
    };
    let (Some(output_x), Some(output_y)) = (
        x.checked_sub(output.origin_x),
        y.checked_sub(output.origin_y),
    ) else {
        return false;
    };
    if !queue_bounds_in_output(
        ctx,
        xdg_toplevel_guest_id,
        remote_surface_host_id,
        output_host_id,
        output_x,
        output_y,
        width,
        height,
    ) {
        return false;
    }
    log::info!(
        "remote-shell placement: xdg_toplevel={} remote_surface={} bounds_in_output=({}, {}, {}, {}) output={}",
        xdg_toplevel_guest_id,
        remote_surface_host_id,
        output_x,
        output_y,
        width,
        height,
        output_host_id
    );
    true
}

// ── Guest requests on facades ─────────────────────────────────────────────

/// Route a guest request whose sender is a facade.
///
/// # Returns
///
/// `None` when the sender is not a facade (including whenever the backend is
/// disabled); the caller dispatches it normally. Otherwise the result of the
/// generated dispatcher, which still validates the request. Any message it
/// would have forwarded is consumed, because the facade has no host object.
pub(crate) fn dispatch_facade_request(
    ctx: &mut Context,
    interface: &str,
    msg: &mut WireMessage,
) -> Option<DispatchResult> {
    ctx.window_placement.remote_shell()?.facade(msg.sender_id)?;
    let mut facade = XdgFacade;
    let result = match interface {
        "xdg_surface" => xdg_surface::dispatch_request(msg, &mut facade, ctx),
        "xdg_toplevel" => xdg_toplevel::dispatch_request(msg, &mut facade, ctx),
        "xdg_popup" => xdg_popup::dispatch_request(msg, &mut facade, ctx),
        _ => {
            debug_assert!(false, "facades are only XDG surface/role objects");
            return None;
        }
    };
    Some(result.map(|translated| {
        if let Some((_, fds)) = translated {
            debug_assert!(fds.is_empty(), "xdg-shell requests carry no fds");
            log::trace!(
                "remote-shell: consumed {} opcode {} from facade {}",
                interface,
                msg.opcode,
                msg.sender_id
            );
        }
        None
    }))
}

/// Request handler for facade XDG objects.
///
/// Requests with `new_id` arguments or destructor semantics are overridden.
/// The rest keep the generated default (`Forward`); the generated
/// dispatcher drops them because a facade has no host sender, and
/// [`dispatch_facade_request`] would consume any encoded message anyway.
struct XdgFacade;

impl xdg_surface::XdgSurfaceHandler for XdgFacade {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_surface_guest_id = ctx.last_sender_id;
        // A client that destroys xdg_surface before its toplevel leaves an
        // inert toplevel facade. Its host role must not outlive the surface.
        if let Some(xdg_toplevel_guest_id) = ctx
            .xdg_surface_to_xdg_toplevel
            .remove(&xdg_surface_guest_id)
        {
            release_remote_surface(ctx, xdg_toplevel_guest_id);
            ctx.xdg_toplevel_to_wl_surface
                .remove(&xdg_toplevel_guest_id);
        }
        ctx.xdg_surface_to_wl_surface.remove(&xdg_surface_guest_id);
        retire_facade(ctx, xdg_surface_guest_id);
        Action::Drop
    }

    fn on_get_toplevel(&mut self, ctx: &mut Context, id: u32) -> Action {
        create_remote_toplevel(ctx, ctx.last_sender_id, id);
        Action::Drop
    }

    fn on_get_popup(
        &mut self,
        ctx: &mut Context,
        id: u32,
        _parent: u32,
        _positioner: u32,
    ) -> Action {
        let xdg_surface_guest_id = ctx.last_sender_id;
        if !construct_xdg_surface(ctx, xdg_surface_guest_id) {
            return Action::Drop;
        }
        let version = ctx
            .shadow_table
            .guest_object_version(xdg_surface_guest_id)
            .unwrap_or(1);
        register_facade(
            ctx,
            id,
            "xdg_popup",
            version,
            RemoteShellFacade::Popup {
                xdg_surface_guest_id,
            },
        );
        // Exo requires a non-null XDG parent for popups, and the parent here
        // has a remote-surface role. Dismiss the popup the way a compositor
        // would; the client then destroys it.
        let message = MessageBuilder::new().build_message(id, xdg_popup::EVT_POPUP_DONE);
        ctx.host_to_client_queue.push((message, Vec::new()));
        log::debug!("remote-shell: dismissed xdg_popup {}", id);
        Action::Drop
    }

    fn on_set_window_geometry(
        &mut self,
        _ctx: &mut Context,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Action {
        // zcr_remote_surface_v2 has no window-geometry request.
        Action::Drop
    }

    fn on_ack_configure(&mut self, _ctx: &mut Context, _serial: u32) -> Action {
        // Configures are synthesized locally; the host has nothing to ack.
        Action::Drop
    }
}

impl xdg_toplevel::XdgToplevelHandler for XdgFacade {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_toplevel_guest_id = ctx.last_sender_id;
        release_remote_surface(ctx, xdg_toplevel_guest_id);
        if let Some(RemoteShellFacade::Toplevel {
            xdg_surface_guest_id,
        }) = ctx
            .window_placement
            .remote_shell()
            .and_then(|remote_shell| remote_shell.facade(xdg_toplevel_guest_id))
        {
            if ctx.xdg_surface_to_xdg_toplevel.get(&xdg_surface_guest_id)
                == Some(&xdg_toplevel_guest_id)
            {
                ctx.xdg_surface_to_xdg_toplevel
                    .remove(&xdg_surface_guest_id);
                deconstruct_xdg_surface(ctx, xdg_surface_guest_id);
            }
        }
        ctx.xdg_toplevel_to_wl_surface
            .remove(&xdg_toplevel_guest_id);
        retire_facade(ctx, xdg_toplevel_guest_id);
        Action::Drop
    }

    fn on_set_title(&mut self, ctx: &mut Context, title: &String) -> Action {
        queue_remote_string(
            ctx,
            ctx.last_sender_id,
            zcr_remote_surface_v2::REQ_SET_TITLE,
            title,
        );
        Action::Drop
    }

    fn on_set_app_id(&mut self, ctx: &mut Context, app_id: &String) -> Action {
        // Same Guest OS namespace the normal XDG path uses for shelf
        // matching.
        let app_id = crate::handler::compositor::native_wayland_app_id(&ctx.vm_identifier, app_id);
        if crate::handler::compositor::wayland_string_fits_message(&app_id) {
            queue_remote_string(
                ctx,
                ctx.last_sender_id,
                zcr_remote_surface_v2::REQ_SET_APP_ID,
                &app_id,
            );
        }
        Action::Drop
    }
}

impl xdg_popup::XdgPopupHandler for XdgFacade {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_popup_guest_id = ctx.last_sender_id;
        if let Some(RemoteShellFacade::Popup {
            xdg_surface_guest_id,
        }) = ctx
            .window_placement
            .remote_shell()
            .and_then(|remote_shell| remote_shell.facade(xdg_popup_guest_id))
        {
            deconstruct_xdg_surface(ctx, xdg_surface_guest_id);
        }
        retire_facade(ctx, xdg_popup_guest_id);
        Action::Drop
    }
}

/// Send a string request to a facade toplevel's remote surface, if it still
/// has one.
fn queue_remote_string(ctx: &mut Context, xdg_toplevel_guest_id: u32, opcode: u16, value: &str) {
    let Some(remote_surface_host_id) = ctx
        .window_placement
        .remote_shell()
        .and_then(|remote_shell| remote_shell.remote_surface_for_toplevel(xdg_toplevel_guest_id))
        .filter(|&id| {
            ctx.shadow_table
                .host_object_matches(id, REMOTE_SURFACE_INTERFACE)
        })
    else {
        return;
    };
    let mut builder = MessageBuilder::new();
    builder.write_string(value);
    if let Ok(message) = builder.try_build_message(remote_surface_host_id, opcode) {
        ctx.client_to_host_queue.push((message, Vec::new()));
    }
}

// ── Host events ───────────────────────────────────────────────────────────

/// Host event handler for the host-only remote-shell objects.
pub struct RemoteShellHandler;

impl crate::protocols::remote_shell_unstable_v2::ProtocolHandler for RemoteShellHandler {}

impl zcr_remote_shell_v2::ZcrRemoteShellV2Handler for RemoteShellHandler {
    fn on_default_device_scale_factor(&mut self, _ctx: &mut Context, _scale: i32) -> Action {
        // Remote surfaces pin their own scale factor to 1.0.
        Action::Drop
    }

    fn on_layout_mode(&mut self, _ctx: &mut Context, _layout_mode: u32) -> Action {
        Action::Drop
    }

    fn on_desktop_focus_state_changed(&mut self, _ctx: &mut Context, _focus_state: u32) -> Action {
        Action::Drop
    }
}

impl zcr_remote_surface_v2::ZcrRemoteSurfaceV2Handler for RemoteShellHandler {
    fn on_close(&mut self, ctx: &mut Context) -> Action {
        if let Some(xdg_toplevel_guest_id) = ctx
            .window_placement
            .remote_shell()
            .and_then(|remote_shell| remote_shell.toplevel_for_remote_surface(ctx.last_sender_id))
        {
            let message =
                MessageBuilder::new().build_message(xdg_toplevel_guest_id, xdg_toplevel::EVT_CLOSE);
            ctx.host_to_client_queue.push((message, Vec::new()));
        }
        Action::Drop
    }

    fn on_bounds_changed(
        &mut self,
        _ctx: &mut Context,
        _display_id_hi: u32,
        _display_id_lo: u32,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
        _bounds_change_reason: u32,
    ) -> Action {
        // Exo sends bounds_changed_in_output for the same change to v2
        // clients; handling only that event avoids applying it twice.
        Action::Drop
    }

    fn on_bounds_changed_in_output(
        &mut self,
        ctx: &mut Context,
        output: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        bounds_change_reason: u32,
    ) -> Action {
        let remote_surface_host_id = ctx.last_sender_id;
        let Some(xdg_toplevel_guest_id) =
            ctx.window_placement
                .remote_shell()
                .and_then(|remote_shell| {
                    remote_shell.toplevel_for_remote_surface(remote_surface_host_id)
                })
        else {
            return Action::Drop;
        };
        if width <= 0 || height <= 0 {
            log::debug!(
                "remote-shell: ignoring empty bounds {}x{} for remote surface {}",
                width,
                height,
                remote_surface_host_id
            );
            return Action::Drop;
        }
        log::debug!(
            "remote-shell: host bounds request ({}, {}, {}, {}) reason={} for xdg_toplevel {}",
            x,
            y,
            width,
            height,
            bounds_change_reason,
            xdg_toplevel_guest_id
        );
        // Client-controlled surfaces only move when the client applies the
        // compositor's request, so accept it as-is.
        let _ = queue_bounds_in_output(
            ctx,
            xdg_toplevel_guest_id,
            remote_surface_host_id,
            output,
            x,
            y,
            width,
            height,
        );
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::remote_shell_unstable_v2::zcr_remote_surface_v2::ZcrRemoteSurfaceV2Handler;
    use crate::protocols::wayland::wl_display;
    use crate::state::RemoteShellManager;

    type Message = (Vec<u8>, Vec<RawFd>);

    const WM_BASE: u32 = 5;
    const WL_SURFACE: u32 = 10;
    const HOST_WL_SURFACE: u32 = 20;
    const XDG_SURFACE: u32 = 11;
    const XDG_TOPLEVEL: u32 = 12;
    const MANAGER: u32 = 30;
    const HOST_OUTPUT: u32 = 26;

    fn sender(message: &Message) -> u32 {
        u32::from_ne_bytes(message.0[0..4].try_into().unwrap())
    }

    fn opcode(message: &Message) -> u16 {
        (u32::from_ne_bytes(message.0[4..8].try_into().unwrap()) & 0xffff) as u16
    }

    fn arg(message: &Message, index: usize) -> u32 {
        let start = 8 + index * 4;
        u32::from_ne_bytes(message.0[start..start + 4].try_into().unwrap())
    }

    fn summary(queue: &[Message]) -> Vec<(u32, u16)> {
        queue
            .iter()
            .map(|message| (sender(message), opcode(message)))
            .collect()
    }

    /// Encode a guest request and route it through the facade dispatcher,
    /// as the proxy does.
    fn dispatch(
        ctx: &mut Context,
        sender_id: u32,
        opcode: u16,
        payload: MessageBuilder,
    ) -> Option<DispatchResult> {
        let message = payload.build_message(sender_id, opcode);
        let interface = ctx
            .shadow_table
            .get_interface(sender_id)
            .cloned()
            .unwrap_or_default();
        ctx.last_sender_id = sender_id;
        let mut wire = WireMessage::new(sender_id, opcode, &message[8..], &[]);
        dispatch_facade_request(ctx, &interface, &mut wire)
    }

    fn new_id(id: u32) -> MessageBuilder {
        let mut builder = MessageBuilder::new();
        builder.write_u32(id);
        builder
    }

    /// Remote-shell context with a bound manager, a guest xdg_wm_base, and a
    /// mapped guest wl_surface. `with_manager = false` models a host that
    /// does not advertise the global.
    fn context(with_manager: bool) -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement.enable_remote_shell_for_test();
        if with_manager {
            ctx.shadow_table.track_host_interface_with_version(
                MANAGER,
                "zcr_remote_shell_v2".to_string(),
                6,
            );
            ctx.window_placement
                .remote_shell_mut()
                .unwrap()
                .bind_manager(
                    7,
                    RemoteShellManager {
                        host_id: MANAGER,
                        version: 6,
                    },
                );
        }
        ctx.shadow_table.map_id(WM_BASE, 3);
        ctx.shadow_table
            .track_interface_with_version(WM_BASE, "xdg_wm_base".to_string(), 3);
        ctx.shadow_table.map_id(WL_SURFACE, HOST_WL_SURFACE);
        ctx.shadow_table
            .track_interface_with_version(WL_SURFACE, "wl_surface".to_string(), 4);
        ctx
    }

    fn claim(ctx: &mut Context) -> bool {
        ctx.last_sender_id = WM_BASE;
        claim_xdg_surface(ctx, XDG_SURFACE, WL_SURFACE)
    }

    /// Claim the surface and create its toplevel; returns the remote surface.
    fn toplevel(ctx: &mut Context) -> u32 {
        assert!(claim(ctx));
        assert!(matches!(
            dispatch(
                ctx,
                XDG_SURFACE,
                xdg_surface::REQ_GET_TOPLEVEL,
                new_id(XDG_TOPLEVEL)
            ),
            Some(Ok(None))
        ));
        let remote_surface = ctx
            .window_placement
            .remote_shell()
            .unwrap()
            .remote_surface_for_toplevel(XDG_TOPLEVEL)
            .expect("remote surface");
        ctx.client_to_host_queue.clear();
        ctx.host_to_client_queue.clear();
        remote_surface
    }

    #[test]
    fn vendored_protocol_keeps_upstream_opcodes() {
        assert_eq!(zcr_remote_shell_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE, 1);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_OUTPUT, 5);
        assert_eq!(zcr_remote_shell_v2::EVT_DESKTOP_FOCUS_STATE_CHANGED, 2);
        assert_eq!(zcr_remote_surface_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_APP_ID, 1);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_TITLE, 2);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT, 37);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR, 40);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SHADOW_CORNER_RADII, 42);
        assert_eq!(zcr_remote_surface_v2::EVT_CLOSE, 0);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED, 3);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED_IN_OUTPUT, 7);
    }

    #[test]
    fn bind_version_requires_set_scale_factor_and_caps_at_vendored_version() {
        assert_eq!(bind_version(3), None);
        assert_eq!(bind_version(4), Some(4));
        assert_eq!(bind_version(6), Some(6));
        assert_eq!(bind_version(9), Some(6));
    }

    #[test]
    fn disabled_backend_claims_nothing() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = WM_BASE;
        assert!(!claim_xdg_surface(&mut ctx, XDG_SURFACE, WL_SURFACE));
        assert!(ctx.shadow_table.get_interface(XDG_SURFACE).is_none());
        let mut wire = WireMessage::new(XDG_SURFACE, 0, &[], &[]);
        assert!(dispatch_facade_request(&mut ctx, "xdg_surface", &mut wire).is_none());
        assert!(!queue_window_bounds(
            &mut ctx,
            XDG_TOPLEVEL,
            HOST_OUTPUT,
            OutputState::default(),
            (0, 0, 10, 10)
        ));
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn missing_manager_fails_closed_to_the_normal_xdg_path() {
        let mut ctx = context(false);
        assert!(
            !claim(&mut ctx),
            "no manager: the surface stays a host XDG role"
        );
        assert!(ctx.shadow_table.get_interface(XDG_SURFACE).is_none());
        assert!(!ctx.xdg_surface_to_wl_surface.contains_key(&XDG_SURFACE));
        assert!(
            !ctx.window_placement
                .remote_shell_mut()
                .unwrap()
                .note_missing_manager(),
            "the fallback is reported once"
        );
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn get_toplevel_creates_a_remote_surface_and_initial_configure() {
        let mut ctx = context(true);
        assert!(claim(&mut ctx));
        assert_eq!(
            ctx.window_placement
                .remote_shell()
                .unwrap()
                .facade(XDG_SURFACE),
            Some(RemoteShellFacade::Surface {
                wl_surface_guest_id: WL_SURFACE,
                constructed: false
            })
        );
        assert_eq!(
            ctx.xdg_surface_to_wl_surface.get(&XDG_SURFACE),
            Some(&WL_SURFACE)
        );

        assert!(matches!(
            dispatch(
                &mut ctx,
                XDG_SURFACE,
                xdg_surface::REQ_GET_TOPLEVEL,
                new_id(XDG_TOPLEVEL)
            ),
            Some(Ok(None))
        ));
        let remote_surface = ctx
            .window_placement
            .remote_shell()
            .unwrap()
            .remote_surface_for_toplevel(XDG_TOPLEVEL)
            .expect("remote surface");

        let host = &ctx.client_to_host_queue;
        assert_eq!(
            summary(host),
            vec![
                (MANAGER, zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE),
                (remote_surface, zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR),
            ]
        );
        assert_eq!(arg(&host[0], 0), remote_surface);
        assert_eq!(arg(&host[0], 1), HOST_WL_SURFACE);
        assert_eq!(arg(&host[0], 2), CONTAINER_DEFAULT);
        assert_eq!(f32::from_bits(arg(&host[1], 0)), 1.0);
        assert!(ctx
            .shadow_table
            .host_object_matches(remote_surface, REMOTE_SURFACE_INTERFACE));

        let guest = &ctx.host_to_client_queue;
        assert_eq!(
            summary(guest),
            vec![
                (XDG_TOPLEVEL, xdg_toplevel::EVT_CONFIGURE),
                (XDG_SURFACE, xdg_surface::EVT_CONFIGURE),
            ]
        );
        assert_eq!((arg(&guest[0], 0), arg(&guest[0], 1)), (0, 0));
        assert_eq!(arg(&guest[1], 0), 1, "first synthesized serial");
        assert_eq!(
            ctx.xdg_toplevel_to_wl_surface.get(&XDG_TOPLEVEL),
            Some(&WL_SURFACE)
        );
        assert_eq!(
            ctx.xdg_surface_to_xdg_toplevel.get(&XDG_SURFACE),
            Some(&XDG_TOPLEVEL)
        );
        assert_eq!(
            ctx.shadow_table
                .get_interface(XDG_TOPLEVEL)
                .map(String::as_str),
            Some("xdg_toplevel")
        );
        // Facades never consume a host object ID: an ID the host never sees
        // would leave a gap that libwayland-server rejects as an invalid
        // new_id on the next real object.
        for facade in [XDG_SURFACE, XDG_TOPLEVEL] {
            assert_eq!(ctx.shadow_table.get_host_id(facade), None);
            assert!(ctx.shadow_table.is_local_only_guest_object(facade));
        }
    }

    #[test]
    fn second_role_object_is_already_constructed() {
        let mut ctx = context(true);
        toplevel(&mut ctx);
        dispatch(
            &mut ctx,
            XDG_SURFACE,
            xdg_surface::REQ_GET_TOPLEVEL,
            new_id(13),
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![(1, wl_display::EVT_ERROR)]
        );
        assert_eq!(arg(&ctx.host_to_client_queue[0], 0), XDG_SURFACE);
        assert_eq!(
            arg(&ctx.host_to_client_queue[0], 1),
            XDG_SURFACE_ERROR_ALREADY_CONSTRUCTED
        );
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn title_and_app_id_go_to_the_remote_surface() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);
        let mut title = MessageBuilder::new();
        title.write_string("Terminal");
        dispatch(&mut ctx, XDG_TOPLEVEL, xdg_toplevel::REQ_SET_TITLE, title);
        let mut app_id = MessageBuilder::new();
        app_id.write_string("org.example.App");
        dispatch(&mut ctx, XDG_TOPLEVEL, xdg_toplevel::REQ_SET_APP_ID, app_id);

        assert_eq!(
            summary(&ctx.client_to_host_queue),
            vec![
                (remote_surface, zcr_remote_surface_v2::REQ_SET_TITLE),
                (remote_surface, zcr_remote_surface_v2::REQ_SET_APP_ID),
            ]
        );
        let expected = crate::handler::compositor::native_wayland_app_id(
            &ctx.vm_identifier,
            "org.example.App",
        );
        let encoded = &ctx.client_to_host_queue[1].0[12..12 + expected.len()];
        assert_eq!(encoded, expected.as_bytes());
    }

    #[test]
    fn untranslated_facade_requests_are_consumed() {
        let mut ctx = context(true);
        toplevel(&mut ctx);
        let mut ack = MessageBuilder::new();
        ack.write_u32(1);
        assert!(matches!(
            dispatch(&mut ctx, XDG_SURFACE, xdg_surface::REQ_ACK_CONFIGURE, ack),
            Some(Ok(None))
        ));
        assert!(matches!(
            dispatch(
                &mut ctx,
                XDG_TOPLEVEL,
                xdg_toplevel::REQ_SET_MAXIMIZED,
                MessageBuilder::new()
            ),
            Some(Ok(None))
        ));
        let mut size = MessageBuilder::new();
        size.write_i32(100);
        size.write_i32(50);
        assert!(matches!(
            dispatch(&mut ctx, XDG_TOPLEVEL, xdg_toplevel::REQ_SET_MIN_SIZE, size),
            Some(Ok(None))
        ));
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(!ctx.fatal_protocol_error);
    }

    #[test]
    fn popup_is_dismissed_without_host_traffic() {
        let mut ctx = context(true);
        assert!(claim(&mut ctx));
        ctx.shadow_table.map_id(14, 40);
        ctx.shadow_table
            .track_interface_with_version(14, "xdg_positioner".to_string(), 3);
        let mut popup = new_id(15);
        popup.write_u32(XDG_SURFACE);
        popup.write_u32(14);
        // A popup parented to its own surface is enough to exercise the
        // facade path; validation only requires a live xdg_surface parent.
        assert!(matches!(
            dispatch(&mut ctx, XDG_SURFACE, xdg_surface::REQ_GET_POPUP, popup),
            Some(Ok(None))
        ));
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![(15, xdg_popup::EVT_POPUP_DONE)]
        );

        ctx.host_to_client_queue.clear();
        assert!(matches!(
            dispatch(&mut ctx, 15, xdg_popup::REQ_DESTROY, MessageBuilder::new()),
            Some(Ok(None))
        ));
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![(1, wl_display::EVT_DELETE_ID)]
        );
        assert_eq!(arg(&ctx.host_to_client_queue[0], 0), 15);
        assert!(ctx.shadow_table.get_interface(15).is_none());
        assert!(
            ctx.window_placement
                .remote_shell_mut()
                .unwrap()
                .set_xdg_surface_constructed(XDG_SURFACE, true),
            "destroying the popup frees the xdg_surface for a new role"
        );
    }

    #[test]
    fn destroying_toplevel_then_surface_releases_role_and_acknowledges_both() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);

        dispatch(
            &mut ctx,
            XDG_TOPLEVEL,
            xdg_toplevel::REQ_DESTROY,
            MessageBuilder::new(),
        );
        assert_eq!(
            summary(&ctx.client_to_host_queue),
            vec![(remote_surface, zcr_remote_surface_v2::REQ_DESTROY)]
        );
        assert!(ctx
            .shadow_table
            .is_pending_destroy_host_only(remote_surface));
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![(1, wl_display::EVT_DELETE_ID)]
        );
        assert_eq!(arg(&ctx.host_to_client_queue[0], 0), XDG_TOPLEVEL);
        assert!(!ctx.xdg_toplevel_to_wl_surface.contains_key(&XDG_TOPLEVEL));
        assert!(!ctx.xdg_surface_to_xdg_toplevel.contains_key(&XDG_SURFACE));

        dispatch(
            &mut ctx,
            XDG_SURFACE,
            xdg_surface::REQ_DESTROY,
            MessageBuilder::new(),
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1, "no second destructor");
        assert_eq!(arg(&ctx.host_to_client_queue[1], 0), XDG_SURFACE);
        assert!(!ctx.xdg_surface_to_wl_surface.contains_key(&XDG_SURFACE));
        let remote_shell = ctx.window_placement.remote_shell().unwrap();
        assert_eq!(remote_shell.facade_count(), 0);
        assert_eq!(remote_shell.remote_surface_count(), 0);
        assert!(ctx.shadow_table.get_interface(XDG_SURFACE).is_none());
    }

    #[test]
    fn destroying_surface_first_releases_role_once() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);

        dispatch(
            &mut ctx,
            XDG_SURFACE,
            xdg_surface::REQ_DESTROY,
            MessageBuilder::new(),
        );
        assert_eq!(
            summary(&ctx.client_to_host_queue),
            vec![(remote_surface, zcr_remote_surface_v2::REQ_DESTROY)]
        );
        assert!(!ctx.xdg_toplevel_to_wl_surface.contains_key(&XDG_TOPLEVEL));

        // The inert toplevel facade is still destroyable.
        dispatch(
            &mut ctx,
            XDG_TOPLEVEL,
            xdg_toplevel::REQ_DESTROY,
            MessageBuilder::new(),
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(arg(&ctx.host_to_client_queue[1], 0), XDG_TOPLEVEL);
        assert_eq!(
            ctx.window_placement.remote_shell().unwrap().facade_count(),
            0
        );
    }

    #[test]
    fn wl_surface_teardown_releases_remote_role_before_the_surface() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);
        release_surface_roles(&mut ctx, 99);
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "other surfaces are untouched"
        );

        release_surface_roles(&mut ctx, WL_SURFACE);
        release_surface_roles(&mut ctx, WL_SURFACE);
        assert_eq!(
            summary(&ctx.client_to_host_queue),
            vec![(remote_surface, zcr_remote_surface_v2::REQ_DESTROY)]
        );
        assert_eq!(
            ctx.window_placement
                .remote_shell()
                .unwrap()
                .remote_surface_for_toplevel(XDG_TOPLEVEL),
            None
        );
    }

    #[test]
    fn host_close_reaches_the_guest_toplevel() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);
        ctx.last_sender_id = remote_surface;
        assert_eq!(RemoteShellHandler.on_close(&mut ctx), Action::Drop);
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![(XDG_TOPLEVEL, xdg_toplevel::EVT_CLOSE)]
        );

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 999;
        assert_eq!(RemoteShellHandler.on_close(&mut ctx), Action::Drop);
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "unknown senders are ignored"
        );
    }

    #[test]
    fn host_bounds_request_is_echoed_and_configured() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);
        ctx.last_sender_id = remote_surface;
        assert_eq!(
            RemoteShellHandler.on_bounds_changed_in_output(
                &mut ctx,
                HOST_OUTPUT,
                10,
                20,
                640,
                480,
                1
            ),
            Action::Drop
        );
        let host = &ctx.client_to_host_queue;
        assert_eq!(
            summary(host),
            vec![(
                remote_surface,
                zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT
            )]
        );
        assert_eq!(
            (0..5).map(|index| arg(&host[0], index)).collect::<Vec<_>>(),
            vec![HOST_OUTPUT, 10, 20, 640, 480]
        );
        let guest = &ctx.host_to_client_queue;
        assert_eq!(
            summary(guest),
            vec![
                (XDG_TOPLEVEL, xdg_toplevel::EVT_CONFIGURE),
                (XDG_SURFACE, xdg_surface::EVT_CONFIGURE),
            ]
        );
        assert_eq!((arg(&guest[0], 0), arg(&guest[0], 1)), (640, 480));
        assert_eq!(arg(&guest[1], 0), 2, "serial follows the initial configure");

        ctx.client_to_host_queue.clear();
        ctx.host_to_client_queue.clear();
        RemoteShellHandler.on_bounds_changed_in_output(&mut ctx, HOST_OUTPUT, 0, 0, 0, 480, 1);
        RemoteShellHandler.on_bounds_changed(&mut ctx, 0, 1, 0, 0, 640, 480, 1);
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn placement_uses_output_relative_bounds() {
        let mut ctx = context(true);
        let remote_surface = toplevel(&mut ctx);
        let output = OutputState {
            origin_x: 1920,
            origin_y: 0,
            ..OutputState::default()
        };
        assert!(queue_window_bounds(
            &mut ctx,
            XDG_TOPLEVEL,
            HOST_OUTPUT,
            output,
            (1920, 48, 960, 1032)
        ));
        let host = &ctx.client_to_host_queue;
        assert_eq!(
            summary(host),
            vec![(
                remote_surface,
                zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT
            )]
        );
        assert_eq!(
            (0..5).map(|index| arg(&host[0], index)).collect::<Vec<_>>(),
            vec![HOST_OUTPUT, 0, 48, 960, 1032]
        );
        assert_eq!(
            summary(&ctx.host_to_client_queue),
            vec![
                (XDG_TOPLEVEL, xdg_toplevel::EVT_CONFIGURE),
                (XDG_SURFACE, xdg_surface::EVT_CONFIGURE),
            ]
        );
    }

    #[test]
    fn placement_without_remote_surface_or_after_teardown_is_skipped() {
        let mut ctx = context(true);
        assert!(!queue_window_bounds(
            &mut ctx,
            XDG_TOPLEVEL,
            HOST_OUTPUT,
            OutputState::default(),
            (0, 0, 10, 10)
        ));
        let remote_surface = toplevel(&mut ctx);
        // A destructor already claimed by wl_surface teardown makes the
        // remote surface stale; nothing may be queued on it.
        assert!(ctx.shadow_table.mark_pending_destroy_host(remote_surface));
        assert!(!queue_window_bounds(
            &mut ctx,
            XDG_TOPLEVEL,
            HOST_OUTPUT,
            OutputState::default(),
            (0, 0, 10, 10)
        ));
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx.host_to_client_queue.is_empty());
    }
}
