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

//! The remote-shell-v2 placement backend: a host `zcr_remote_surface_v2`
//! role, placed with `set_bounds_in_output`.
//!
//! Exo gives a `wl_surface` one shell role, assigned when the client
//! creates its `xdg_surface`, and a remote-surface role cannot coexist with
//! an XDG role. While a host `zcr_remote_shell_v2` is bound, the guest's
//! `xdg_surface`, `xdg_toplevel` and `xdg_popup` are therefore local objects
//! of the proxy ("facades") that never reach the host, and each toplevel's
//! surface gets a remote-surface role instead:
//!
//! - `xdg_surface.get_toplevel` creates the remote surface, with a scale
//!   factor of 1.0, so its coordinates are DIPs like the work area, and
//!   sends the guest the initial configure the host would have.
//! - `set_title` and `set_app_id` (in the Guest OS namespace, also on the
//!   Aura surface) reach the remote surface; the other toplevel requests
//!   have no remote-surface counterpart and are dropped.
//! - `xdg_surface.get_popup` creates a popup that is dismissed at once: a
//!   remote surface cannot parent an XDG popup. Its parent and positioner
//!   are not checked as Exo would, since nothing reaches the host: a guest
//!   that Exo would disconnect for them sees `popup_done` instead.
//! - `zxdg_decoration_manager_v1.get_toplevel_decoration` for a facade
//!   toplevel, which only `--xdg-decoration` offers, creates a local
//!   decoration that never configures, so the client keeps its own
//!   decorations; one for an XDG toplevel is forwarded as usual.
//! - The commit that first shows a toplevel also sets its bounds, centred
//!   in the work area at the committed size: Exo maps a client-controlled
//!   surface without a frame at the output's origin, and sends no bounds
//!   request, until its client sets bounds.
//! - A placement sends `set_bounds_in_output` and a guest configure of the
//!   new size; Exo applies the bounds at the guest's next commit, so no
//!   barrier is needed. `xdg_surface.set_window_geometry` has no remote
//!   surface counterpart and is dropped, so a client-side decorated
//!   window's bounds include its shadow margin.
//! - The host's `close` reaches the guest; its `bounds_changed_in_output`,
//!   which Exo scales by the display's device scale factor, is converted to
//!   DIPs and accepted as a placement.
//!
//! The global is bound host-side only and never advertised to guests. With
//! none bound (stock Crostini advertises none), windows take the normal XDG
//! path and placements are skipped. The boundary in
//! `crate::handler::window_placement` calls in here only while this is the
//! connection's backend.

use super::{PlacementSkip, PlacementTarget};
use crate::protocols::remote_shell_unstable_v2::{
    self, zcr_remote_shell_v2, zcr_remote_surface_v2,
};
use crate::protocols::xdg_decoration_unstable_v1::zxdg_decoration_manager_v1;
use crate::protocols::xdg_shell::{xdg_popup, xdg_surface, xdg_toplevel, xdg_wm_base};
use crate::state::{Context, RemoteShellManager, RemoteShellPlacementState};
use crate::wire::{queue_or_drop, Action, Outgoing, ProtocolError, WireMessage};
use std::sync::atomic::{AtomicBool, Ordering};

/// The oldest `zcr_remote_shell_v2` the backend binds: remote surfaces
/// need `set_scale_factor` to take DIPs.
const MIN_VERSION: u32 = zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR_SINCE;

/// `zcr_remote_shell_v2.container.default`, the container of an ordinary
/// window. The trimmed XML carries no enums.
const CONTAINER_DEFAULT: u32 = 1;

/// The scale factor of every remote surface: Exo divides client
/// coordinates by it, so at 1.0 they are DIPs, the units of the work area.
const SCALE_FACTOR: f32 = 1.0;

/// Whether an advertised manager too old to bind was reported. Every
/// connection sees the same host, so once per process says it all.
static OLD_MANAGER_WARNED: AtomicBool = AtomicBool::new(false);

/// Whether a window that took the XDG path for want of a manager was
/// reported, once per process.
static NO_MANAGER_WARNED: AtomicBool = AtomicBool::new(false);

/// Whether a facade toplevel left without a role, because the manager was
/// removed after its xdg_surface was claimed, was reported, once per
/// process.
static RETIRED_MANAGER_WARNED: AtomicBool = AtomicBool::new(false);

/// The connection's remote-shell state. The boundary calls this module only
/// while the backend is active.
fn state(ctx: &Context) -> &RemoteShellPlacementState {
    ctx.window_placement
        .remote_shell()
        .expect("the remote-shell-v2 backend is the connection's backend")
}

/// [`state`], mutably.
fn state_mut(ctx: &mut Context) -> &mut RemoteShellPlacementState {
    ctx.window_placement
        .remote_shell_mut()
        .expect("the remote-shell-v2 backend is the connection's backend")
}

// ── The host manager ──────────────────────────────────────────────────────

/// Whether the registry binds a host `zcr_remote_shell_v2` advertised at
/// `version`. One older than [`MIN_VERSION`] is reported once and left
/// unbound.
pub(super) fn wants_manager(version: u32) -> bool {
    if version >= MIN_VERSION {
        return true;
    }
    if !OLD_MANAGER_WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "Ignoring zcr_remote_shell_v2 v{version}: the remote-shell-v2 placement backend \
             needs v{MIN_VERSION}"
        );
    }
    false
}

/// Create the host object of the manager the registry binds from global
/// `global_name`, advertised at `version`, and record it.
///
/// # Returns
///
/// The manager: its host ID, and the version to bind it at, the advertised
/// one capped at the vendored XML's.
pub(super) fn bind_manager(
    ctx: &mut Context,
    global_name: u32,
    version: u32,
) -> RemoteShellManager {
    let version = version.min(zcr_remote_shell_v2::VERSION);
    let host_id = ctx
        .shadow_table
        .create_host_only("zcr_remote_shell_v2", version);
    log::info!("Bound zcr_remote_shell_v2 v{version} (host_id={host_id}) for window placement");
    let manager = RemoteShellManager {
        global_name,
        host_id,
        version,
    };
    state_mut(ctx).bind_manager(manager);
    manager
}

/// The registry name of the bound manager.
pub(super) fn manager_name(ctx: &Context) -> Option<u32> {
    state(ctx).manager().map(|manager| manager.global_name)
}

/// Forget the manager of global `global_name`, which is retired.
///
/// Remote surfaces created from it keep their roles: Exo sends their
/// events through it, so it is not destroyed, and its ID stays reserved for
/// the connection. New toplevels use the next manager the registry binds,
/// or take the XDG path.
pub(super) fn manager_retired(ctx: &mut Context, global_name: u32) {
    if let Some(manager) = state_mut(ctx).take_manager_of(global_name) {
        ctx.shadow_table.retire_host(manager.host_id);
    }
}

// ── Facades ───────────────────────────────────────────────────────────────

/// Make the new guest `xdg_surface` of `wl_surface_guest_id` a facade if a
/// manager is bound.
///
/// A wl_surface takes one role, so a second xdg_surface for it is the
/// `xdg_wm_base.role` error Exo would raise.
///
/// # Returns
///
/// The request's action when it must not reach the host; `None` when it
/// takes the XDG path.
pub(super) fn claim_xdg_surface(
    ctx: &mut Context,
    xdg_surface_guest_id: u32,
    wl_surface_guest_id: u32,
) -> Option<Action> {
    if state(ctx).manager().is_none() {
        if !NO_MANAGER_WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "The host has no zcr_remote_shell_v2 v{MIN_VERSION}+: windows take the XDG path \
                 and window placement is skipped"
            );
        }
        return None;
    }
    if ctx
        .xdg_roles
        .has_other_xdg_surface(wl_surface_guest_id, xdg_surface_guest_id)
    {
        return Some(ctx.reject(
            ctx.last_sender_id,
            xdg_wm_base::ERROR_ROLE,
            "the wl_surface already has an xdg_surface",
        ));
    }
    let version = ctx
        .shadow_table
        .guest_object_version_or_unknown(ctx.last_sender_id);
    ctx.shadow_table
        .create_local(xdg_surface_guest_id, "xdg_surface", version);
    Some(Action::Drop)
}

/// Dispatch a guest request whose sender is a local XDG object: a facade.
///
/// # Returns
///
/// `None` when the sender is not local; the caller dispatches it as usual.
pub(super) fn dispatch_facade_request(
    ctx: &mut Context,
    interface: &str,
    msg: &mut WireMessage,
) -> Option<Result<Option<Outgoing>, ProtocolError>> {
    let mut facade = Facade;
    // The manager is the host's, but a decoration for a facade toplevel
    // could not name it on the host.
    if interface == "zxdg_decoration_manager_v1" {
        return Some(zxdg_decoration_manager_v1::dispatch_request(
            msg,
            &mut facade,
            ctx,
        ));
    }
    if !ctx.shadow_table.is_local(msg.sender_id) {
        return None;
    }
    Some(match interface {
        "xdg_surface" => xdg_surface::dispatch_request(msg, &mut facade, ctx),
        "xdg_toplevel" => xdg_toplevel::dispatch_request(msg, &mut facade, ctx),
        "xdg_popup" => xdg_popup::dispatch_request(msg, &mut facade, ctx),
        _ => return None,
    })
}

/// Give the facade toplevel `xdg_toplevel_guest_id` of `wl_surface_guest_id`
/// its host remote surface.
///
/// Without a manager, which the registry retired after it claimed the
/// xdg_surface and before this `get_toplevel`, the toplevel gets no role on
/// the host at all, so the window is never shown. The global would have to
/// be removed in that instant, and a removed manager is never used again.
fn create_remote_surface(ctx: &mut Context, xdg_toplevel_guest_id: u32, wl_surface_guest_id: u32) {
    let Some(manager) = state(ctx).manager() else {
        if !RETIRED_MANAGER_WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(
                "xdg_toplevel {xdg_toplevel_guest_id} will never be shown: the host removed \
                 zcr_remote_shell_v2 after its xdg_surface was created"
            );
        }
        return;
    };
    let Some(wl_surface_host_id) = ctx.shadow_table.get_host_id(wl_surface_guest_id) else {
        return;
    };
    let remote_surface = ctx
        .shadow_table
        .create_host_only("zcr_remote_surface_v2", manager.version);
    ctx.client_to_host_queue.extend([
        zcr_remote_shell_v2::req::get_remote_surface(
            manager.host_id,
            remote_surface,
            wl_surface_host_id,
            CONTAINER_DEFAULT,
        ),
        zcr_remote_surface_v2::req::set_scale_factor(remote_surface, SCALE_FACTOR.to_bits()),
    ]);
    state_mut(ctx).remember_remote_surface(xdg_toplevel_guest_id, remote_surface);
}

/// Release the remote surface of a guest toplevel whose role ended, before
/// the wl_surface's destructor if that ends it.
pub(super) fn release_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    if let Some(remote_surface) = state_mut(ctx).take_remote_surface(xdg_toplevel_guest_id) {
        ctx.client_to_host_queue
            .push(zcr_remote_surface_v2::req::destroy(remote_surface));
        ctx.retire_destroyed_host_object(remote_surface);
    }
}

/// Send a facade toplevel the configure the host would: an
/// `xdg_toplevel.configure` of `width`x`height` without states, which the
/// remote surface does not report, and its `xdg_surface.configure`.
fn queue_configure(ctx: &mut Context, xdg_toplevel_guest_id: u32, width: i32, height: i32) {
    let Some(xdg_surface_guest_id) = ctx.xdg_roles.xdg_surface_of_toplevel(xdg_toplevel_guest_id)
    else {
        return;
    };
    let serial = state_mut(ctx).next_configure_serial();
    ctx.host_to_client_queue.extend([
        xdg_toplevel::evt::configure(xdg_toplevel_guest_id, width, height, &[])
            .expect("an empty states array fits a message"),
        xdg_surface::evt::configure(xdg_surface_guest_id, serial),
    ]);
}

/// Move a facade toplevel to `bounds`, in screen DIPs on `output_host_id`.
///
/// `set_bounds_in_output` takes bounds relative to the output's origin. The
/// guest's configure of the new size makes it commit, and that commit
/// applies them.
fn queue_bounds(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    remote_surface: u32,
    output_host_id: u32,
    (x, y, width, height): (i32, i32, i32, i32),
) {
    ctx.client_to_host_queue
        .push(zcr_remote_surface_v2::req::set_bounds_in_output(
            remote_surface,
            output_host_id,
            x,
            y,
            width,
            height,
        ));
    state_mut(ctx).mark_placed(xdg_toplevel_guest_id);
    queue_configure(ctx, xdg_toplevel_guest_id, width, height);
}

/// Bounds for a facade toplevel's remote surface on one output.
struct OutputBounds {
    remote_surface: u32,
    output_host_id: u32,
    /// `x, y, width, height`, relative to the output.
    bounds: (i32, i32, i32, i32),
}

/// The bounds that centre facade toplevel `xdg_toplevel_guest_id`, of
/// `wl_surface_guest_id`, at `width`x`height` in the work area of the output
/// it would be placed on, relative to that output, with its remote surface
/// and that output.
///
/// # Returns
///
/// `None` without a remote surface, an output whose Aura metadata is
/// complete, or bounds that fit the output's coordinates.
fn centred_bounds(
    ctx: &Context,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
    (width, height): (i32, i32),
) -> Option<OutputBounds> {
    let remote_surface = state(ctx).remote_surface_for_toplevel(xdg_toplevel_guest_id)?;
    let (output_host_id, output) = ctx
        .shadow_table
        .get_host_id(wl_surface_guest_id)
        .and_then(|host_surface| ctx.outputs.for_surface(host_surface))?;
    let area = output.work_area()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    let centred =
        |origin: i32, extent: i32, size: i32| origin.checked_add((extent - size).max(0) / 2);
    let x = centred(area.x, area.width, width)?.checked_sub(output.origin_x)?;
    let y = centred(area.y, area.height, height)?.checked_sub(output.origin_y)?;
    Some(OutputBounds {
        remote_surface,
        output_host_id,
        bounds: (x, y, width, height),
    })
}

/// Give a facade toplevel on `wl_surface_guest_id` its first bounds, as the
/// commit that first shows it at `size` reaches the host, so that it
/// applies them.
///
/// Exo creates a client-controlled window at that commit without a frame,
/// at its output's origin, and sends it no bounds request until its client
/// has set bounds. The proxy centres it in the work area of the output the
/// surface would be placed on, at its own size; a toplevel already placed
/// keeps its bounds. Until that output's Aura metadata is complete, the
/// toplevel waits at the origin with its size recorded, and
/// [`output_committed`] places it.
pub(super) fn first_bounds(ctx: &mut Context, wl_surface_guest_id: u32, size: (i32, i32)) {
    let Some(toplevel) = ctx.xdg_roles.toplevel_on_wl_surface(wl_surface_guest_id) else {
        return;
    };
    if !state(ctx).is_unplaced(toplevel) {
        return;
    }
    let Some(OutputBounds {
        remote_surface,
        output_host_id,
        bounds: (x, y, width, height),
    }) = centred_bounds(ctx, toplevel, wl_surface_guest_id, size)
    else {
        state_mut(ctx).record_shown_size(toplevel, size);
        return;
    };
    ctx.client_to_host_queue
        .push(zcr_remote_surface_v2::req::set_bounds_in_output(
            remote_surface,
            output_host_id,
            x,
            y,
            width,
            height,
        ));
    state_mut(ctx).mark_placed(toplevel);
}

/// Place the facade toplevels that were shown before their output's Aura
/// metadata was complete, now that an output committed its metadata.
///
/// Bounds sent outside a commit apply at the next one, so each also gets
/// a configure of its size, as a placement does, to make the guest commit.
pub(super) fn output_committed(ctx: &mut Context) {
    for (toplevel, size) in state(ctx).waiting_for_output() {
        let Some(wl_surface_guest_id) = ctx.xdg_roles.wl_surface_of_toplevel(toplevel) else {
            continue;
        };
        if let Some(placed) = centred_bounds(ctx, toplevel, wl_surface_guest_id, size) {
            queue_bounds(
                ctx,
                toplevel,
                placed.remote_surface,
                placed.output_host_id,
                placed.bounds,
            );
        }
    }
}

/// Move `target` with `set_bounds_in_output`.
///
/// # Errors
///
/// [`PlacementSkip::NoRemoteSurface`] when the toplevel has none (it took
/// the XDG path), or [`PlacementSkip::RectNotRepresentable`] when the bounds
/// do not fit the output's coordinates.
pub(super) fn apply_bounds(
    ctx: &mut Context,
    target: PlacementTarget,
) -> Result<(), PlacementSkip> {
    let PlacementTarget {
        action,
        xdg_toplevel_guest_id,
        output_host_id,
        output,
        bounds,
        ..
    } = target;
    let remote_surface = state(ctx)
        .remote_surface_for_toplevel(xdg_toplevel_guest_id)
        .ok_or(PlacementSkip::NoRemoteSurface)?;
    let x = bounds.x.checked_sub(output.origin_x);
    let y = bounds.y.checked_sub(output.origin_y);
    let (Some(x), Some(y)) = (x, y) else {
        return Err(PlacementSkip::RectNotRepresentable);
    };
    queue_bounds(
        ctx,
        xdg_toplevel_guest_id,
        remote_surface,
        output_host_id,
        (x, y, bounds.width, bounds.height),
    );
    log::info!(
        "window layout {:?}: xdg_toplevel={} remote_surface={} bounds_in_output=({}, {}, {}, {}) output={}",
        action,
        xdg_toplevel_guest_id,
        remote_surface,
        x,
        y,
        bounds.width,
        bounds.height,
        output_host_id
    );
    Ok(())
}

/// The guest's requests to facades. A request the host would act on and a
/// remote surface cannot keeps the generated default, which drops it.
struct Facade;

/// The version of the facade's own guest object, which its children take.
fn facade_version(ctx: &Context) -> u32 {
    ctx.shadow_table
        .guest_object_version_or_unknown(ctx.last_sender_id)
}

/// Whether facade xdg_surface `xdg_surface_guest_id` has a live role
/// object: a toplevel or a popup.
fn has_role(ctx: &Context, xdg_surface_guest_id: u32) -> bool {
    ctx.xdg_roles
        .toplevel_of_xdg_surface(xdg_surface_guest_id)
        .is_some()
        || state(ctx).has_popup(xdg_surface_guest_id)
}

/// Reject a second role object of facade xdg_surface
/// `xdg_surface_guest_id`, as Exo would.
fn reject_second_role(ctx: &mut Context, xdg_surface_guest_id: u32) -> Action {
    ctx.reject(
        xdg_surface_guest_id,
        xdg_surface::ERROR_ALREADY_CONSTRUCTED,
        "xdg_surface already has a role object",
    )
}

/// Forget facade popup `xdg_popup_guest_id`, which ended, so its
/// xdg_surface may take another role.
pub(super) fn forget_popup(ctx: &mut Context, xdg_popup_guest_id: u32) {
    state_mut(ctx).forget_popup(xdg_popup_guest_id);
}

impl xdg_surface::XdgSurfaceHandler for Facade {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let xdg_surface_guest_id = ctx.last_sender_id;
        if has_role(ctx, xdg_surface_guest_id) {
            return ctx.reject(
                xdg_surface_guest_id,
                xdg_surface::ERROR_DEFUNCT_ROLE_OBJECT,
                "xdg_surface destroyed before its role object",
            );
        }
        // The local destructor sends the delete_id and ends the role.
        Action::Drop
    }

    fn on_get_toplevel(&mut self, ctx: &mut Context, id: u32) -> Action {
        let xdg_surface_guest_id = ctx.last_sender_id;
        if has_role(ctx, xdg_surface_guest_id) {
            return reject_second_role(ctx, xdg_surface_guest_id);
        }
        let version = facade_version(ctx);
        ctx.shadow_table.create_local(id, "xdg_toplevel", version);
        if let Some(wl_surface_guest_id) = ctx.xdg_roles.add_toplevel(xdg_surface_guest_id, id) {
            create_remote_surface(ctx, id, wl_surface_guest_id);
        }
        // Size 0x0 lets the client choose its own.
        queue_configure(ctx, id, 0, 0);
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
        if has_role(ctx, xdg_surface_guest_id) {
            return reject_second_role(ctx, xdg_surface_guest_id);
        }
        let version = facade_version(ctx);
        ctx.shadow_table.create_local(id, "xdg_popup", version);
        state_mut(ctx).remember_popup(id, xdg_surface_guest_id);
        // Exo parents a popup only to an XDG surface, and the host has no
        // XDG surface here: dismiss it as a compositor would.
        ctx.host_to_client_queue
            .push(xdg_popup::evt::popup_done(id));
        Action::Drop
    }

    fn on_ack_configure(&mut self, _ctx: &mut Context, _serial: u32) -> Action {
        // The proxy sent the configure; the host has nothing to acknowledge.
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
        // A remote surface has no window geometry: its bounds are the
        // whole surface, shadow margin included.
        Action::Drop
    }
}

impl xdg_toplevel::XdgToplevelHandler for Facade {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // The local destructor sends the delete_id, and ending the toplevel
        // releases its remote surface.
        Action::Drop
    }

    fn on_set_title(&mut self, ctx: &mut Context, title: &String) -> Action {
        let toplevel = ctx.last_sender_id;
        if let Some(remote_surface) = state(ctx).remote_surface_for_toplevel(toplevel) {
            queue_or_drop(
                &mut ctx.client_to_host_queue,
                zcr_remote_surface_v2::req::set_title(remote_surface, title),
                format_args!("the title of zcr_remote_surface_v2 {remote_surface}"),
            );
        }
        Action::Drop
    }

    fn on_set_app_id(&mut self, ctx: &mut Context, app_id: &String) -> Action {
        let toplevel = ctx.last_sender_id;
        // The Guest OS namespace, as on the XDG path, for the shelf.
        let native_app_id =
            crate::handler::compositor::native_wayland_app_id(&ctx.vm_identifier, app_id);
        if let Some(remote_surface) = state(ctx).remote_surface_for_toplevel(toplevel) {
            if !queue_or_drop(
                &mut ctx.client_to_host_queue,
                zcr_remote_surface_v2::req::set_app_id(remote_surface, &native_app_id),
                format_args!("the application ID of zcr_remote_surface_v2 {remote_surface}"),
            ) {
                return Action::Drop;
            }
        }
        if let Some(wl_surface_guest_id) = ctx.xdg_roles.wl_surface_of_toplevel(toplevel) {
            super::set_aura_application_id(ctx, wl_surface_guest_id, &native_app_id);
        }
        Action::Drop
    }
}

impl zxdg_decoration_manager_v1::ZxdgDecorationManagerV1Handler for Facade {
    fn on_get_toplevel_decoration(&mut self, ctx: &mut Context, id: u32, toplevel: u32) -> Action {
        if !ctx.shadow_table.is_local(toplevel) {
            return Action::Forward;
        }
        // The host has no toplevel to decorate: the decoration is local,
        // never configures, and completes its own destructor.
        let version = facade_version(ctx);
        ctx.shadow_table
            .create_local(id, "zxdg_toplevel_decoration_v1", version);
        Action::Drop
    }
}

impl xdg_popup::XdgPopupHandler for Facade {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // The local destructor sends the delete_id.
        Action::Drop
    }
}

// ── Host events ───────────────────────────────────────────────────────────

/// Accept the host's request to move remote surface `remote_surface` to
/// `x, y, width, height` on host output `output_host_id`, which the
/// generated dispatcher checked is a guest's, in that display's pixels, as
/// Exo sends `bounds_changed_in_output`.
///
/// Exo moves a client-controlled surface only to the bounds its client
/// sets, so the proxy sets the requested bounds, converted to the DIPs its
/// scale factor of 1.0 takes (`OutputState::pixels_to_dips`, which includes
/// the display zoom), and configures the guest to their size.
fn accept_host_bounds(
    ctx: &mut Context,
    remote_surface: u32,
    output_host_id: u32,
    (x, y, width, height): (i32, i32, i32, i32),
) {
    let Some(toplevel) = ctx
        .window_placement
        .remote_shell()
        .and_then(|state| state.toplevel_for_remote_surface(remote_surface))
    else {
        return;
    };
    let Some(output) = ctx.outputs.usable(output_host_id) else {
        log::debug!("Ignoring bounds for remote surface {remote_surface}: output {output_host_id} is not usable yet");
        return;
    };
    let dips = [x, y, width, height].map(|value| output.pixels_to_dips(value));
    let [Some(x), Some(y), Some(width), Some(height)] = dips else {
        return;
    };
    if width <= 0 || height <= 0 {
        return;
    }
    queue_bounds(
        ctx,
        toplevel,
        remote_surface,
        output_host_id,
        (x, y, width, height),
    );
}

/// The host event handler of the host-only remote-shell objects. No guest
/// object pairs with one, so nothing is forwarded.
pub struct RemoteShellHandler;

impl remote_shell_unstable_v2::ProtocolHandler for RemoteShellHandler {}

impl zcr_remote_shell_v2::ZcrRemoteShellV2Handler for RemoteShellHandler {
    fn on_default_device_scale_factor(&mut self, _ctx: &mut Context, _scale: i32) -> Action {
        // Each remote surface sets its own scale factor.
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
        if let Some(toplevel) = ctx
            .window_placement
            .remote_shell()
            .and_then(|state| state.toplevel_for_remote_surface(ctx.last_sender_id))
        {
            ctx.host_to_client_queue
                .push(xdg_toplevel::evt::close(toplevel));
        }
        Action::Drop
    }

    fn on_state_type_changed(&mut self, _ctx: &mut Context, _state_type: u32) -> Action {
        // Window states are not bridged.
        Action::Drop
    }

    fn on_window_geometry_changed(
        &mut self,
        _ctx: &mut Context,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Action {
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
        // Exo follows it with bounds_changed_in_output, which names the
        // output; that one is accepted.
        Action::Drop
    }

    fn on_drag_started(&mut self, _ctx: &mut Context, _direction: u32) -> Action {
        Action::Drop
    }

    fn on_drag_finished(&mut self, _ctx: &mut Context, _x: i32, _y: i32, _canceled: i32) -> Action {
        Action::Drop
    }

    fn on_change_zoom_level(&mut self, _ctx: &mut Context, _change: i32) -> Action {
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
        _bounds_change_reason: u32,
    ) -> Action {
        accept_host_bounds(ctx, ctx.last_sender_id, output, (x, y, width, height));
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture::*;
    use super::*;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_shell, zaura_surface};
    use crate::protocols::wayland::{wl_display, wl_registry, wl_surface};
    use crate::protocols::xdg_shell::xdg_wm_base;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::Object;
    use crate::wire::WL_DISPLAY_ID;

    /// A facade toplevel and the host remote surface behind it.
    #[derive(Clone, Copy, Debug)]
    struct Window {
        xdg_surface: u32,
        toplevel: u32,
        remote: u32,
    }

    /// A [`Desk`] under the remote-shell-v2 backend, with its output at
    /// `origin_x` and `device_scale`, whose host advertises a
    /// `zcr_remote_shell_v2`; returns the desk, the global's name and the
    /// manager the proxy bound for itself.
    async fn remote_desk(origin_x: i32, device_scale: u32) -> (Desk, u32, u32) {
        let placement = PlacementConfig::remote_shell_for_test("<Alt>q=top-left");
        with_remote_shell(Desk::with_output(placement, origin_x, device_scale).await).await
    }

    /// The host of `d`, a desk under the remote-shell-v2 backend,
    /// advertises a `zcr_remote_shell_v2`; returns the desk, the global's
    /// name and the manager the proxy bound for itself.
    async fn with_remote_shell(mut d: Desk) -> (Desk, u32, u32) {
        let name =
            d.s.advertise("zcr_remote_shell_v2", zcr_remote_shell_v2::VERSION)
                .await;
        assert_eq!(d.s.guest_received(), [], "the guest never sees it");
        let host = d.s.host_received();
        let [(
            _,
            AnyRequest::WlRegistry(wl_registry::Request::Bind {
                name: bound,
                ref id,
            }),
        )] = host[..]
        else {
            panic!("the proxy did not bind the remote shell: {host:?}");
        };
        assert_eq!(bound, name);
        assert_eq!(
            (id.0.as_str(), id.1),
            ("zcr_remote_shell_v2", zcr_remote_shell_v2::VERSION)
        );
        (d, name, id.2)
    }

    /// The guest creates an xdg_surface and a toplevel on `surface`, which
    /// become facades; the host gets a remote surface with a scale factor
    /// of 1.0 from `manager`, and the guest the configure it would send.
    /// `serial` is the serial the proxy gives its configure.
    async fn window(d: &mut Desk, manager: u32, surface: Object, serial: u32) -> Window {
        let xdg_surface = d.s.new_id();
        d.s.guest([xdg_wm_base::req::get_xdg_surface(
            d.wm_base.id,
            xdg_surface,
            surface.id,
        )])
        .await;
        assert_eq!(d.s.host_received(), [], "the xdg_surface stays local");
        let toplevel = d.s.new_id();
        d.s.guest([xdg_surface::req::get_toplevel(xdg_surface, toplevel)])
            .await;
        let host = d.s.host_received();
        let [(
            _,
            AnyRequest::ZcrRemoteShellV2(zcr_remote_shell_v2::Request::GetRemoteSurface {
                id: remote,
                ..
            }),
        ), _] = host[..]
        else {
            panic!("no remote surface: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    manager,
                    zcr_remote_shell_v2::Request::GetRemoteSurface {
                        id: remote,
                        surface: surface.host,
                        container: CONTAINER_DEFAULT
                    }
                    .into()
                ),
                (
                    remote,
                    zcr_remote_surface_v2::Request::SetScaleFactor {
                        scale_factor_as_uint: 1.0f32.to_bits()
                    }
                    .into()
                ),
            ]
        );
        let window = Window {
            xdg_surface,
            toplevel,
            remote,
        };
        assert_eq!(d.s.guest_received(), configured(window, 0, 0, serial));
        window
    }

    /// What the guest gets when the proxy configures `window` to
    /// `width`x`height` with `serial`.
    fn configured(window: Window, width: i32, height: i32, serial: u32) -> [(u32, AnyEvent); 2] {
        let configure = xdg_toplevel::Event::Configure {
            width,
            height,
            states: Vec::new(),
        };
        [
            (window.toplevel, configure.into()),
            (
                window.xdg_surface,
                xdg_surface::Event::Configure { serial }.into(),
            ),
        ]
    }

    fn bounds(window: Window, output: u32, x: i32, y: i32, w: i32, h: i32) -> (u32, AnyRequest) {
        let request = zcr_remote_surface_v2::Request::SetBoundsInOutput {
            output,
            x,
            y,
            width: w,
            height: h,
        };
        (window.remote, request.into())
    }

    fn delete_id(id: u32) -> (u32, AnyEvent) {
        (WL_DISPLAY_ID, wl_display::Event::DeleteId { id }.into())
    }

    /// A facade toplevel's title and application ID, in the Guest OS
    /// namespace, reach its remote surface, and the ID its Aura surface
    /// too; the configure the proxy sent needs no host acknowledgement.
    #[tokio::test]
    async fn a_remote_shell_toplevel_is_a_facade_on_a_remote_surface() {
        let (mut d, _, manager) = remote_desk(0, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        d.s.guest([
            xdg_toplevel::req::set_title(w.toplevel, "Terminal").unwrap(),
            xdg_toplevel::req::set_app_id(w.toplevel, "org.example.App").unwrap(),
            xdg_surface::req::ack_configure(w.xdg_surface, 1),
        ])
        .await;
        let native = "org.chromium.guest_os.termina.wayland.org.example.App".to_string();
        let host = d.s.host_received();
        let [_, _, (
            _,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface {
                id: aura_surface, ..
            }),
        ), _] = host[..]
        else {
            panic!("no Aura surface: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    w.remote,
                    zcr_remote_surface_v2::Request::SetTitle {
                        title: "Terminal".to_string()
                    }
                    .into()
                ),
                (
                    w.remote,
                    zcr_remote_surface_v2::Request::SetAppId {
                        app_id: native.clone()
                    }
                    .into()
                ),
                (
                    d.shell,
                    zaura_shell::Request::GetAuraSurface {
                        id: aura_surface,
                        surface: d.surface.host
                    }
                    .into()
                ),
                (
                    aura_surface,
                    zaura_surface::Request::SetApplicationId {
                        application_id: Some(native)
                    }
                    .into()
                ),
            ]
        );
        assert_eq!(d.s.guest_received(), []);
    }

    /// A chord sends the bounds relative to the output's origin, and the
    /// guest a configure of their size, whose commit applies them.
    #[tokio::test]
    async fn a_remote_shell_placement_sends_bounds_in_output() {
        let (mut d, _, manager) = remote_desk(1920, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        d.focus().await;
        d.chord(3).await;
        let (width, height) = (DESK_WORK_AREA.0 / 2, DESK_WORK_AREA.1 / 2);
        assert_eq!(
            d.s.host_received(),
            [bounds(w, d.output.host, 0, 0, width, height)]
        );
        assert_eq!(d.s.guest_received(), configured(w, width, height, 2));
    }

    /// Exo sends `bounds_changed_in_output` in the display's pixels; the
    /// proxy accepts it in the DIPs its remote surfaces use. The host's
    /// close reaches the guest.
    #[tokio::test]
    async fn host_bounds_requests_are_accepted_in_dips() {
        let (mut d, _, manager) = remote_desk(0, 2000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        d.s.host([zcr_remote_surface_v2::evt::bounds_changed_in_output(
            w.remote,
            d.output.host,
            200,
            -100,
            1280,
            960,
            0,
        )])
        .await;
        assert_eq!(
            d.s.host_received(),
            [bounds(w, d.output.host, 100, -50, 640, 480)]
        );
        assert_eq!(d.s.guest_received(), configured(w, 640, 480, 2));

        d.s.host([zcr_remote_surface_v2::evt::close(w.remote)])
            .await;
        assert_eq!(
            d.s.guest_received(),
            [(w.toplevel, xdg_toplevel::Event::Close {}.into())]
        );
        assert_eq!(d.s.host_received(), []);
    }

    /// Exo scales the bounds it sends by the display's effective scale,
    /// which includes the display zoom that Aura reports as the output's
    /// current scale: at 125% on a 1x panel, 960x750 pixels are 768x600
    /// DIPs.
    #[tokio::test]
    async fn host_bounds_requests_include_the_display_zoom() {
        let placement = PlacementConfig::remote_shell_for_test("");
        let d = Desk::with_output_zoom(placement, 0, 1250, 1000).await;
        let (mut d, _, manager) = with_remote_shell(d).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        d.s.host([zcr_remote_surface_v2::evt::bounds_changed_in_output(
            w.remote,
            d.output.host,
            250,
            125,
            960,
            750,
            0,
        )])
        .await;
        assert_eq!(
            d.s.host_received(),
            [bounds(w, d.output.host, 200, 100, 768, 600)]
        );
        assert_eq!(d.s.guest_received(), configured(w, 768, 600, 2));
    }

    /// Ending a facade toplevel, by its destructor or its wl_surface's,
    /// destroys its remote surface; the facades complete their own
    /// destructors.
    #[tokio::test]
    async fn ending_a_remote_shell_window_destroys_its_remote_surface() {
        let (mut d, _, manager) = remote_desk(0, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        d.s.guest([
            xdg_toplevel::req::destroy(w.toplevel),
            xdg_surface::req::destroy(w.xdg_surface),
        ])
        .await;
        assert_eq!(
            d.s.host_received(),
            [(w.remote, zcr_remote_surface_v2::Request::Destroy {}.into())]
        );
        assert_eq!(
            d.s.guest_received(),
            [delete_id(w.toplevel), delete_id(w.xdg_surface)]
        );
        d.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, w.remote)])
            .await;
        assert_eq!(d.s.guest_received(), []);

        let w = window(&mut d, manager, surface, 2).await;
        d.s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            d.s.host_received(),
            [
                (w.remote, zcr_remote_surface_v2::Request::Destroy {}.into()),
                (surface.host, wl_surface::Request::Destroy {}.into()),
            ],
            "the role goes before its surface"
        );
        d.s.guest([
            xdg_toplevel::req::destroy(w.toplevel),
            xdg_surface::req::destroy(w.xdg_surface),
        ])
        .await;
        assert_eq!(d.s.host_received(), []);
        assert_eq!(
            d.s.guest_received(),
            [delete_id(w.toplevel), delete_id(w.xdg_surface)]
        );
    }

    /// A remote surface cannot parent an XDG popup, so a popup is dismissed
    /// at once, without host traffic.
    #[tokio::test]
    async fn a_popup_of_a_remote_shell_window_is_dismissed() {
        let (mut d, _, manager) = remote_desk(0, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        let positioner = d.s.new_id();
        d.s.create(xdg_wm_base::req::create_positioner(
            d.wm_base.id,
            positioner,
        ))
        .await;
        let menu = d.s.surface().await;
        let menu_xdg_surface = d.s.new_id();
        let popup = d.s.new_id();
        d.s.guest([
            xdg_wm_base::req::get_xdg_surface(d.wm_base.id, menu_xdg_surface, menu.id),
            xdg_surface::req::get_popup(menu_xdg_surface, popup, Some(w.xdg_surface), positioner),
        ])
        .await;
        assert_eq!(d.s.host_received(), []);
        assert_eq!(
            d.s.guest_received(),
            [(popup, xdg_popup::Event::PopupDone {}.into())]
        );
        d.s.guest([xdg_popup::req::destroy(popup)]).await;
        assert_eq!(d.s.guest_received(), [delete_id(popup)]);
    }

    /// The proxy answers in the host's place the xdg-shell errors a facade
    /// can cause: a second role object, a toplevel or a popup, whichever
    /// came first; an xdg_surface destroyed before its role object; and a
    /// second xdg_surface for one wl_surface, which takes one role.
    #[tokio::test]
    async fn a_facade_raises_the_xdg_shell_errors_the_host_would() {
        #[derive(Clone, Copy, Debug)]
        enum Case {
            SecondToplevel,
            ToplevelAfterPopup,
            SecondPopup,
            DestroyedBeforeToplevel,
            DestroyedBeforePopup,
            SecondXdgSurface,
        }
        for case in [
            Case::SecondToplevel,
            Case::ToplevelAfterPopup,
            Case::SecondPopup,
            Case::DestroyedBeforeToplevel,
            Case::DestroyedBeforePopup,
            Case::SecondXdgSurface,
        ] {
            let (mut d, _, manager) = remote_desk(0, 1000).await;
            let surface = d.surface;
            let w = window(&mut d, manager, surface, 1).await;
            let positioner = d.s.new_id();
            d.s.create(xdg_wm_base::req::create_positioner(
                d.wm_base.id,
                positioner,
            ))
            .await;
            // A facade xdg_surface of another surface, whose role is a popup.
            let menu = d.s.surface().await;
            let menu_xdg_surface = d.s.new_id();
            let popup = d.s.new_id();
            d.s.guest([
                xdg_wm_base::req::get_xdg_surface(d.wm_base.id, menu_xdg_surface, menu.id),
                xdg_surface::req::get_popup(menu_xdg_surface, popup, None, positioner),
            ])
            .await;
            assert_eq!(
                d.s.guest_received(),
                [(popup, xdg_popup::Event::PopupDone {}.into())]
            );
            let id = d.s.new_id();
            let (request, error) = match case {
                Case::SecondToplevel => (
                    xdg_surface::req::get_toplevel(w.xdg_surface, id),
                    (w.xdg_surface, xdg_surface::ERROR_ALREADY_CONSTRUCTED),
                ),
                Case::ToplevelAfterPopup => (
                    xdg_surface::req::get_toplevel(menu_xdg_surface, id),
                    (menu_xdg_surface, xdg_surface::ERROR_ALREADY_CONSTRUCTED),
                ),
                Case::SecondPopup => (
                    xdg_surface::req::get_popup(menu_xdg_surface, id, None, positioner),
                    (menu_xdg_surface, xdg_surface::ERROR_ALREADY_CONSTRUCTED),
                ),
                Case::DestroyedBeforeToplevel => (
                    xdg_surface::req::destroy(w.xdg_surface),
                    (w.xdg_surface, xdg_surface::ERROR_DEFUNCT_ROLE_OBJECT),
                ),
                Case::DestroyedBeforePopup => (
                    xdg_surface::req::destroy(menu_xdg_surface),
                    (menu_xdg_surface, xdg_surface::ERROR_DEFUNCT_ROLE_OBJECT),
                ),
                Case::SecondXdgSurface => (
                    xdg_wm_base::req::get_xdg_surface(d.wm_base.id, id, surface.id),
                    (d.wm_base.id, xdg_wm_base::ERROR_ROLE),
                ),
            };
            d.s.guest_fatal([request]).await;
            assert_eq!(d.s.guest_received_and_error(), (vec![], error), "{case:?}");
            assert_eq!(d.s.host_received(), [], "{case:?}");
        }
    }

    /// A popup's end frees its xdg_surface for another role object.
    #[tokio::test]
    async fn a_dismissed_popups_xdg_surface_takes_another_role() {
        let (mut d, _, manager) = remote_desk(0, 1000).await;
        let positioner = d.s.new_id();
        d.s.create(xdg_wm_base::req::create_positioner(
            d.wm_base.id,
            positioner,
        ))
        .await;
        let menu = d.s.surface().await;
        let menu_xdg_surface = d.s.new_id();
        let popup = d.s.new_id();
        d.s.guest([
            xdg_wm_base::req::get_xdg_surface(d.wm_base.id, menu_xdg_surface, menu.id),
            xdg_surface::req::get_popup(menu_xdg_surface, popup, None, positioner),
            xdg_popup::req::destroy(popup),
        ])
        .await;
        assert_eq!(
            d.s.guest_received(),
            [
                (popup, xdg_popup::Event::PopupDone {}.into()),
                delete_id(popup)
            ]
        );
        let toplevel = d.s.new_id();
        d.s.guest([xdg_surface::req::get_toplevel(menu_xdg_surface, toplevel)])
            .await;
        let host = d.s.host_received();
        assert!(
            matches!(
                &host[..],
                [(sender, AnyRequest::ZcrRemoteShellV2(zcr_remote_shell_v2::Request::GetRemoteSurface { .. })), _]
                    if *sender == manager
            ),
            "{host:?}"
        );
        assert_eq!(d.s.guest_received().len(), 2, "the initial configure");
    }

    /// The commit that first shows a facade toplevel also sets its bounds,
    /// centred in the work area at its size, ahead of the commit, so that
    /// Exo does not map it at the output's origin and sends it bounds
    /// requests; later commits set none.
    #[tokio::test]
    async fn a_remote_shell_window_is_centred_when_first_shown() {
        use crate::protocols::wayland::wl_shm;
        let (mut d, _, manager) = remote_desk(1920, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        let shm = d.s.bind("wl_shm", wl_shm::VERSION).await;
        let (width, height) = (400, 300);
        let pixels = vec![0; (width * height * 4) as usize];
        let pool =
            d.s.shm_pool(shm, crate::test_support::shm_pool_file(&pixels))
                .await;
        let buffer = d.s.shm_buffer(pool, width, height).await;
        let show = [
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ];
        d.s.guest(show).await;
        let host = d.s.host_received();
        let centred = bounds(
            w,
            d.output.host,
            (DESK_WORK_AREA.0 - width) / 2,
            (DESK_WORK_AREA.1 - height) / 2,
            width,
            height,
        );
        let commit: (u32, AnyRequest) = (surface.host, wl_surface::Request::Commit {}.into());
        assert_eq!(
            host.iter().filter(|request| **request == centred).count(),
            1,
            "{host:?}"
        );
        let position =
            |wanted: &(u32, AnyRequest)| host.iter().position(|request| request == wanted);
        assert!(position(&centred) < position(&commit), "{host:?}");

        d.s.guest([wl_surface::req::commit(surface.id)]).await;
        let host = d.s.host_received();
        assert!(
            !host.iter().any(|(sender, _)| *sender == w.remote),
            "{host:?}"
        );
        let _ = d.s.guest_received();
    }

    /// Without a usable remote shell (none advertised, or one too old for
    /// `set_scale_factor`), windows take the XDG path, and a chord is
    /// consumed but places nothing.
    #[tokio::test]
    async fn without_a_remote_shell_windows_take_the_xdg_path() {
        let placement = PlacementConfig::remote_shell_for_test("<Alt>q=top-left");
        let mut d = Desk::new(placement).await;
        d.s.advertise("zcr_remote_shell_v2", MIN_VERSION - 1).await;
        assert_eq!(d.s.host_received(), [], "too old to bind");
        assert_eq!(d.s.guest_received(), []);
        let xdg_surface = d.s.new_id();
        d.s.create(xdg_wm_base::req::get_xdg_surface(
            d.wm_base.id,
            xdg_surface,
            d.surface.id,
        ))
        .await;
        let toplevel = d.s.new_id();
        d.s.create(xdg_surface::req::get_toplevel(xdg_surface, toplevel))
            .await;
        d.focus().await;
        d.chord(3).await;
        assert_eq!(d.s.guest_received(), [], "the chord is consumed");
        assert_eq!(d.s.host_received(), []);
    }

    /// A removed remote-shell global keeps its windows' roles, and is not
    /// destroyed, since Exo sends their events through it; new windows
    /// take the XDG path until another is advertised.
    #[tokio::test]
    async fn a_removed_remote_shell_keeps_its_windows() {
        let (mut d, name, manager) = remote_desk(0, 1000).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        let registry = d.s.registry().await;
        d.s.host([wl_registry::evt::global_remove(registry.host, name)])
            .await;
        assert_eq!(d.s.host_received(), []);
        assert_eq!(d.s.guest_received(), [], "the guest never saw it");

        d.focus().await;
        d.chord(3).await;
        let (width, height) = (DESK_WORK_AREA.0 / 2, DESK_WORK_AREA.1 / 2);
        assert_eq!(
            d.s.host_received(),
            [bounds(w, d.output.host, 0, 0, width, height)]
        );
        assert_eq!(d.s.guest_received(), configured(w, width, height, 2));

        let other = d.s.surface().await;
        let xdg_surface = d.s.new_id();
        d.s.create(xdg_wm_base::req::get_xdg_surface(
            d.wm_base.id,
            xdg_surface,
            other.id,
        ))
        .await;
    }

    /// With `--xdg-decoration`, the decoration manager is forwarded; a
    /// decoration for a facade toplevel, which the host could not name, is
    /// the proxy's own and completes its own destructor.
    #[tokio::test]
    async fn a_facades_decoration_stays_in_the_proxy() {
        use crate::protocols::xdg_decoration_unstable_v1::{
            zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
        };
        let (mut d, _, manager) = remote_desk(0, 1000).await;
        // The --xdg-decoration switch, which the process reads at startup.
        d.s.ctx().xdg_decoration = true;
        let decorations =
            d.s.bind_paired(
                "zxdg_decoration_manager_v1",
                zxdg_decoration_manager_v1::VERSION,
            )
            .await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        let decoration = d.s.new_id();
        d.s.guest([
            zxdg_decoration_manager_v1::req::get_toplevel_decoration(
                decorations.id,
                decoration,
                w.toplevel,
            ),
            zxdg_toplevel_decoration_v1::req::set_mode(
                decoration,
                zxdg_toplevel_decoration_v1::MODE_SERVER_SIDE,
            ),
            zxdg_toplevel_decoration_v1::req::destroy(decoration),
        ])
        .await;
        assert_eq!(d.s.host_received(), []);
        assert_eq!(d.s.guest_received(), [delete_id(decoration)]);
    }

    /// Under this backend, a decoration of an XDG toplevel, one created
    /// while no remote shell was bound, still reaches the host unchanged:
    /// only a facade's decoration stays in the proxy.
    #[tokio::test]
    async fn a_decoration_of_an_xdg_toplevel_reaches_the_host() {
        use crate::protocols::xdg_decoration_unstable_v1::zxdg_decoration_manager_v1;
        let placement = PlacementConfig::remote_shell_for_test("");
        let mut d = Desk::new(placement).await;
        // The --xdg-decoration switch, which the process reads at startup.
        d.s.ctx().xdg_decoration = true;
        let decorations =
            d.s.bind_paired(
                "zxdg_decoration_manager_v1",
                zxdg_decoration_manager_v1::VERSION,
            )
            .await;
        let xdg_surface = d.s.new_id();
        d.s.create(xdg_wm_base::req::get_xdg_surface(
            d.wm_base.id,
            xdg_surface,
            d.surface.id,
        ))
        .await;
        let toplevel = d.s.new_id();
        let host_toplevel =
            d.s.create(xdg_surface::req::get_toplevel(xdg_surface, toplevel))
                .await;
        let decoration = d.s.new_id();
        d.s.guest([zxdg_decoration_manager_v1::req::get_toplevel_decoration(
            decorations.id,
            decoration,
            toplevel,
        )])
        .await;
        let host = d.s.host_received();
        let [(
            sender,
            AnyRequest::ZxdgDecorationManagerV1(
                zxdg_decoration_manager_v1::Request::GetToplevelDecoration {
                    id,
                    toplevel: named,
                },
            ),
        )] = host[..]
        else {
            panic!("the decoration was not forwarded alone: {host:?}");
        };
        assert_eq!((sender, named), (decorations.host, host_toplevel));
        assert_ne!(id, 0);
        assert_eq!(d.s.guest_received(), []);
    }

    /// A window shown before its output's Aura metadata is complete waits
    /// at the origin, then gets its centred bounds, with a configure that
    /// makes the guest commit them, as soon as the metadata is.
    #[tokio::test]
    async fn a_window_shown_before_its_output_is_measured_is_centred_later() {
        use crate::protocols::wayland::wl_shm;
        let placement = PlacementConfig::remote_shell_for_test("");
        let (mut d, _, manager) = with_remote_shell(Desk::unmeasured(placement).await).await;
        let surface = d.surface;
        let w = window(&mut d, manager, surface, 1).await;
        let shm = d.s.bind("wl_shm", wl_shm::VERSION).await;
        let (width, height) = (400, 300);
        let pixels = vec![0; (width * height * 4) as usize];
        let pool =
            d.s.shm_pool(shm, crate::test_support::shm_pool_file(&pixels))
                .await;
        let buffer = d.s.shm_buffer(pool, width, height).await;
        d.s.guest([
            wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        let host = d.s.host_received();
        assert!(
            !host.iter().any(|(sender, _)| *sender == w.remote),
            "no output can place it yet: {host:?}"
        );
        assert_eq!(d.s.guest_received(), []);

        let others = d.measure(0, 1000, 1000).await;
        let (x, y) = (
            (DESK_WORK_AREA.0 - width) / 2,
            (DESK_WORK_AREA.1 - height) / 2,
        );
        assert_eq!(
            d.s.host_received(),
            [bounds(w, d.output.host, x, y, width, height)]
        );
        assert_eq!(others, configured(w, width, height, 2));
    }

    /// Wayland opcodes are positional, and the vendored XML is trimmed:
    /// these are Exo's indices (`zcr_remote_shell_v2.cc`), which a dropped
    /// or reordered message would shift.
    #[test]
    fn vendored_remote_shell_keeps_upstream_opcodes() {
        assert_eq!(zcr_remote_shell_v2::VERSION, 6);
        assert_eq!(zcr_remote_shell_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE, 1);
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_OUTPUT, 5);
        assert_eq!(zcr_remote_shell_v2::EVT_DESKTOP_FOCUS_STATE_CHANGED, 2);
        assert_eq!(zcr_remote_surface_v2::VERSION, 6);
        assert_eq!(zcr_remote_surface_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_APP_ID, 1);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_TITLE, 2);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT, 37);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR, 40);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SCALE_FACTOR_SINCE, 4);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_SHADOW_CORNER_RADII, 42);
        assert_eq!(zcr_remote_surface_v2::EVT_CLOSE, 0);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED, 3);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED_IN_OUTPUT, 7);
    }
}
