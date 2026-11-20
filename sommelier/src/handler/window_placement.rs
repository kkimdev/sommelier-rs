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

//! Window placement: moving the focused XDG toplevel to the bounds that a
//! placement chord names.
//!
//! Window placement subscribes to the keyboard's shortcuts: the keyboard
//! consumes a chord it binds, and hands [`shortcut`] the binding;
//! [`place_focused`] resolves the target (the toplevel the keyboard
//! focuses, the output it entered last, and the bounds the bound rectangle
//! maps to in that output's work area) and starts moving the window.
//!
//! Exo gives Crostini no request that sets a window's bounds
//! (`zaura_toplevel.set_window_bounds` works for ARC windows only, and
//! marking a window ARC bypasses the ChromeOS IME). It moves a window with
//! `zaura_surface.set_parent`, though, which it checks against no
//! permission: `ShellSurface::OnSetParent` puts the window's top-left
//! corner, frame included, at its parent's client area origin plus the
//! request's offset, and a surface named as its own parent is moved
//! relative to itself (the guard against parenting a surface to itself
//! returns before any transient link is made). The guest sets the size
//! itself, from a configure. So a placement takes three steps, each ended
//! by a host barrier ([`InternalCallback::PlacementBarrier`]) whose `done`
//! starts the next:
//!
//! 1. **Restore.** `xdg_toplevel.unset_fullscreen` and `unset_maximized`
//!    leave those states (Exo's restore also leaves snapped and floated
//!    state). Exo answers each with a configure, even when the state does
//!    not change, so by the barrier's `done` it has reported the window's
//!    origin in its restored state.
//! 2. **Move.** `zaura_surface.set_parent(self, dx, dy)` moves the window
//!    by the offset from that origin to the target's. Exo does not report
//!    such a move, so an `unset_fullscreen`, which changes nothing here,
//!    asks it for a configure with the new origin.
//! 3. **Resize.** An origin that neither changed nor lies at the
//!    target's corner means Exo ignored the move ([`MoveOutcome`]): the
//!    proxy logs it ([`MoveLog`]) and stops. Otherwise the guest gets an
//!    `xdg_toplevel.configure` of the target's size, less the caption of a
//!    frame Exo draws, which the new origin shows, with the states and
//!    serial of the newest configure. The guest's commit of that size
//!    resizes the window, which keeps its origin.
//!
//! Exo reports a window's origin only to a toplevel that sent
//! `zaura_toplevel.set_supports_screen_coordinates`: in
//! `zaura_toplevel.configure`, which then replaces `xdg_toplevel.configure`,
//! and in `zaura_toplevel.origin_change`. So while placement is on, every
//! XDG toplevel gets a host-only Aura toplevel in screen coordinates when
//! it is created, and this module translates its configures for the guest.
//! Its per-connection state is [`crate::state::WindowPlacement`].
//!
//! Protocol handlers translate wire events and call in here, one entry
//! point per event. Placement is on for the whole life of a connection or
//! not at all, so each step takes the connection's placement state
//! (`Context::placement_mut`) where it needs it, and does nothing while
//! placement is off, the default:
//!
//! | Caller | Entry point |
//! | :--- | :--- |
//! | a shortcut the keyboard consumed (`wl_keyboard.key`) | [`shortcut`] |
//! | `zaura_shell` bound (`wl_registry.global`) | [`shell_bound`] |
//! | guest `wl_output` bound | [`output_bound`] |
//! | `xdg_surface.get_toplevel`, after dispatch | [`attach_toplevel`] |
//! | XDG role teardown | [`release_toplevel`] |
//! | `wl_callback.done` of a placement barrier | [`complete_barrier`] |
//! | `xdg_surface.configure` | [`surface_configured`] |
//! | `zaura_toplevel.configure` | [`translate_aura_configure`] |
//! | `zaura_toplevel.origin_change` | [`origin_changed`] |
//!
//! The host Aura surface is shared ([`crate::state::AuraSurfaces`]), and
//! `crate::handler::aura_objects` creates and releases every Aura object.

use crate::accelerator::BindingId;
use crate::geometry::Rect;
use crate::handler::aura_objects::{self, ensure_host_zaura_surface, AuraChild};
use crate::handler::callback::queue_internal_sync;
use crate::protocols::aura_shell::zaura_output::EVT_INSETS_SINCE;
use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE;
use crate::protocols::aura_shell::zaura_surface::{
    self, REQ_SET_APPLICATION_ID_SINCE, REQ_SET_PARENT_SINCE,
};
use crate::protocols::aura_shell::zaura_toplevel::{
    self, EVT_ORIGIN_CHANGE_SINCE, REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE,
};
use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel};
use crate::state::{
    Context, GuestId, HostId, InternalCallback, OutputState, Placement, PlacementStep, Point,
};
use crate::warn_once::WarnOnce;
use crate::window_layout::NormalizedRect;
use crate::wire::queue_or_drop;
use std::sync::atomic::{AtomicBool, Ordering};

/// The `zaura_shell` version placement needs.
///
/// The work area needs `zaura_output.insets`, without which it would
/// include the shelf. Every Aura child takes its shell's version, so the
/// requests and events placement uses must be no newer than this. An Aura
/// surface keeps the version of the shell generation that created it,
/// which may be older, but every Aura surface has `set_parent`: it is no
/// newer than `set_application_id`, which the registry's oldest shell has.
const PLACEMENT_SHELL_SINCE: u32 = EVT_INSETS_SINCE;

const _: () = assert!(
    REQ_GET_AURA_TOPLEVEL_FOR_XDG_TOPLEVEL_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE <= PLACEMENT_SHELL_SINCE
        && EVT_ORIGIN_CHANGE_SINCE <= PLACEMENT_SHELL_SINCE
        && REQ_SET_PARENT_SINCE <= REQ_SET_APPLICATION_ID_SINCE
);

/// Whether [`warn_if_shell_cannot_place`] has warned. Every client
/// connection binds the same host shell, so one warning per process says
/// it all.
static OLD_SHELL: WarnOnce = WarnOnce::new();

/// What this process has seen of Exo's `set_parent` moves. Exo honours
/// `set_parent` for every window or for none, so one log per process
/// serves every connection.
static MOVES: MoveLog = MoveLog::new();

/// Warn, once per `once` (the process's [`OLD_SHELL`], or a test's own),
/// that placement is disabled because the host `zaura_shell` is older than
/// [`PLACEMENT_SHELL_SINCE`].
///
/// # Returns
///
/// Whether this call logged the warning.
fn warn_if_shell_cannot_place(once: &WarnOnce, shell_version: u32) -> bool {
    if shell_version >= PLACEMENT_SHELL_SINCE || !once.first() {
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

/// What a process (the process's [`MOVES`], or a test's own) has seen of
/// Exo's `set_parent` moves, which decides how an ignored one is logged.
///
/// Exo also ignores a move while the user drags the window
/// (`ShellSurface::OnSetParent` returns while the window is dragged), and a
/// chord can come in the middle of a drag. So an ignored move is warned
/// about only while no move has put a window at its target: once one has,
/// this host can place windows, and a later ignored move is a dragged
/// window, logged at info.
struct MoveLog {
    /// Whether a move has put a window at its target.
    placed: AtomicBool,
    /// Whether an ignored move has been warned about.
    warned: WarnOnce,
}

impl MoveLog {
    const fn new() -> Self {
        Self {
            placed: AtomicBool::new(false),
            warned: WarnOnce::new(),
        }
    }

    /// Record that a move put a window at its target.
    fn placed(&self) {
        self.placed.store(true, Ordering::Relaxed);
    }

    /// Log that Exo ignored `set_parent(self, offset)` for `xdg_toplevel`:
    /// at info once a move has placed a window, else as a warning the first
    /// time and at debug after.
    ///
    /// # Returns
    ///
    /// The level logged at.
    fn ignored(&self, xdg_toplevel: u32, offset: Point) -> log::Level {
        if self.placed.load(Ordering::Relaxed) {
            log::info!(
                "Window placement did not move xdg_toplevel {}: Exo ignored \
                 zaura_surface.set_parent(self, {}, {}), as it does while the window is dragged",
                xdg_toplevel,
                offset.0,
                offset.1
            );
            return log::Level::Info;
        }
        let level = self.warned.level();
        log::log!(
            level,
            "Window placement did not move xdg_toplevel {}: Exo ignored \
             zaura_surface.set_parent(self, {}, {}). Unless the window was being dragged, \
             this host cannot place windows: Chromium may have changed how it handles a \
             surface parented to itself.",
            xdg_toplevel,
            offset.0,
            offset.1
        );
        level
    }
}

/// Set up what placement needs from a newly bound `zaura_shell` of
/// `shell_version`.
///
/// A guest may bind an output, or create a toplevel, before the host
/// registry delivers `zaura_shell`, so this retries what their own paths
/// could not do yet: every live output's Aura output, and every recorded
/// toplevel's Aura toplevel.
pub(crate) fn shell_bound(ctx: &mut Context, shell_version: u32) {
    if ctx.placement().is_none() {
        return;
    }
    for output_host_id in ctx.outputs.live_outputs() {
        aura_objects::ensure_zaura_output(ctx, output_host_id);
    }
    for (xdg_toplevel_guest_id, _) in ctx.xdg_roles.toplevels() {
        attach_toplevel(ctx, xdg_toplevel_guest_id);
    }
    warn_if_shell_cannot_place(&OLD_SHELL, shell_version);
}

/// Create the host-only Aura output of a newly bound guest `wl_output`,
/// whose scale factors and insets the work area needs. A missing shell is
/// retried by [`shell_bound`].
pub(crate) fn output_bound(ctx: &mut Context, output_host_id: u32) {
    if ctx.placement().is_some() {
        aura_objects::ensure_zaura_output(ctx, output_host_id);
    }
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
    /// The host Aura toplevel cannot be attached: zaura_shell is missing or
    /// too old.
    NoAuraToplevel,
    /// The host Aura surface is unavailable (no zaura_shell).
    NoAuraSurface,
}

impl PlacementSkip {
    /// Log level for a skip. A rect the output cannot represent, or an Aura
    /// toplevel that cannot be attached (an old or missing zaura_shell),
    /// persists until the configuration or the proxy changes, so it is
    /// logged at info. Other skips depend on transient focus or output
    /// state and stay at debug.
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
    wl_surface_guest_id: u32,
}

/// Resolve the xdg_toplevel that `host_keyboard_id` focuses, if any.
fn focused_toplevel(ctx: &Context, host_keyboard_id: HostId) -> Option<FocusedToplevel> {
    let guest_seat_id = ctx.keyboards.seat(host_keyboard_id)?;
    let wl_surface_guest_id = ctx.keyboard_focus.surface_for_seat(guest_seat_id)?;
    if !ctx
        .keyboard_focus
        .keyboard_owns_surface(host_keyboard_id, wl_surface_guest_id)
    {
        return None;
    }
    let xdg_toplevel_guest_id = ctx.xdg_roles.toplevel_on_wl_surface(wl_surface_guest_id)?;
    Some(FocusedToplevel {
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
    })
}

/// Resolve the screen bounds of a bound rectangle on an output.
///
/// The rectangle is mapped into the output work area by
/// [`NormalizedRect::to_work_area_bounds`], which owns the rounding rule.
/// Returns `None` when the output has no usable work area or the rectangle
/// cannot be represented in it.
fn bounds_for(rect: NormalizedRect, output: OutputState) -> Option<Rect> {
    rect.to_work_area_bounds(output.work_area()?)
}

/// What one placement moves, and where.
#[derive(Clone, Copy, Debug)]
struct PlacementTarget {
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
    /// The host `wl_output` the window is placed on.
    output_host_id: u32,
    /// The window's new bounds in screen DIPs, inside that output's work
    /// area.
    bounds: Rect,
}

/// Resolve the toplevel that `host_keyboard_id` focuses, the output it
/// entered last, and the bounds `rect` maps to in that output's work area.
fn resolve_target(
    ctx: &Context,
    host_keyboard_id: HostId,
    rect: NormalizedRect,
) -> Result<PlacementTarget, PlacementSkip> {
    let FocusedToplevel {
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
    } = focused_toplevel(ctx, host_keyboard_id).ok_or(PlacementSkip::NoFocusedToplevel)?;
    let (output_host_id, output) = ctx
        .shadow_table
        .get_host_id(wl_surface_guest_id)
        .and_then(|host_surface_id| ctx.outputs.for_surface(host_surface_id))
        .ok_or(PlacementSkip::NoUsableOutput)?;
    let bounds = bounds_for(rect, output).ok_or(PlacementSkip::RectNotRepresentable)?;
    Ok(PlacementTarget {
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
        output_host_id,
        bounds,
    })
}

/// The keyboard `host_keyboard_id` consumed the chord of shortcut
/// `binding`: place the focused toplevel where the binding says.
///
/// The keyboard consumes the chord whether or not the window can move
/// now, so a skip, which is logged, never leaks the key to the guest.
pub(crate) fn shortcut(ctx: &mut Context, host_keyboard_id: HostId, binding: BindingId) {
    // The bindings change only between two messages, so the binding the
    // keyboard read for this key is still current.
    let Some(rect) = ctx.placement_rect(binding) else {
        log::debug!(
            "shortcut {binding:?} of keyboard {} is unbound",
            host_keyboard_id.0
        );
        return;
    };
    if let Err(reason) = place_focused(ctx, host_keyboard_id, rect) {
        log::log!(
            reason.log_level(),
            "window layout {:?} skipped for keyboard {}: {:?}",
            rect,
            host_keyboard_id.0,
            reason
        );
    }
}

/// Start placing the focused toplevel for one consumed placement chord.
///
/// Resolves the target (see [`resolve_target`]), attaches the toplevel's
/// Aura toplevel and surface if they are missing, and queues the first
/// step, which leaves fullscreen and maximized state; the barrier's
/// [`complete_barrier`] takes the next. A newer placement of the same
/// toplevel replaces this one.
///
/// # Arguments
///
/// * `host_keyboard_id` - the keyboard whose chord was consumed.
/// * `rect` - where the chord's binding places the window.
///
/// # Errors
///
/// A [`PlacementSkip`] names why the window was not moved. The chord stays
/// consumed either way (see [`shortcut`]); a skip queues no placement
/// request.
pub(crate) fn place_focused(
    ctx: &mut Context,
    host_keyboard_id: HostId,
    rect: NormalizedRect,
) -> Result<(), PlacementSkip> {
    debug_assert!(
        ctx.placement().is_some(),
        "the keyboard binds shortcuts only while placement is on"
    );
    let PlacementTarget {
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
        output_host_id,
        bounds,
    } = resolve_target(ctx, host_keyboard_id, rect)?;
    // `XdgRoles` records only mapped toplevels, and every path that unmaps
    // one removes its record first, so the lookup cannot miss.
    let host_xdg_toplevel_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id);
    debug_assert!(
        host_xdg_toplevel_id.is_some(),
        "recorded xdg_toplevel {xdg_toplevel_guest_id} has no host object"
    );
    let host_xdg_toplevel_id = host_xdg_toplevel_id.ok_or(PlacementSkip::NoFocusedToplevel)?;
    let aura_toplevel =
        attach_toplevel(ctx, xdg_toplevel_guest_id).ok_or(PlacementSkip::NoAuraToplevel)?;
    ensure_host_zaura_surface(ctx, wl_surface_guest_id).ok_or(PlacementSkip::NoAuraSurface)?;
    // Every check that can skip placement has run, so a skip never leaves
    // part of the step below in the host stream. The Aura objects created
    // above stay valid for the next attempt.
    ctx.client_to_host_queue.extend([
        xdg_toplevel::req::unset_fullscreen(host_xdg_toplevel_id),
        xdg_toplevel::req::unset_maximized(host_xdg_toplevel_id),
    ]);
    start_step(
        ctx,
        xdg_toplevel_guest_id,
        aura_toplevel,
        bounds,
        PlacementStep::Restore,
    );
    log::info!(
        "window layout {:?}: xdg_toplevel={} bounds=({}, {}, {}, {}) output={}",
        rect,
        xdg_toplevel_guest_id,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
        output_host_id
    );
    Ok(())
}

/// Queue the barrier that ends `step` of the placement of `xdg_toplevel`
/// to `target`, and make it the toplevel's placement in flight.
fn start_step(
    ctx: &mut Context,
    xdg_toplevel: u32,
    aura_toplevel: u32,
    target: Rect,
    step: PlacementStep,
) {
    let barrier = queue_internal_sync(
        ctx,
        InternalCallback::PlacementBarrier {
            zaura_toplevel: aura_toplevel,
        },
    );
    if let Some(placement) = ctx.placement_mut() {
        placement.start(
            GuestId(xdg_toplevel),
            Placement {
                barrier,
                target,
                step,
            },
        );
    }
}

/// Handle `wl_callback.done` of a placement barrier
/// ([`InternalCallback::PlacementBarrier`]), which the callback handler
/// has already retired: take the next step of the placement it ends,
/// unless a newer placement replaced it or its toplevel ended.
pub(crate) fn complete_barrier(ctx: &mut Context, callback_host_id: u32, zaura_toplevel: u32) {
    let Some(state) = ctx.placement_mut() else {
        return;
    };
    let Some(xdg_toplevel) = state.xdg_toplevel(HostId(zaura_toplevel)) else {
        return;
    };
    let Some(placement) = state.take_placement(xdg_toplevel, HostId(callback_host_id)) else {
        return;
    };
    let xdg_toplevel = xdg_toplevel.0;
    match placement.step {
        PlacementStep::Restore => {
            move_window(ctx, xdg_toplevel, zaura_toplevel, placement.target);
        }
        PlacementStep::Move { from, offset } => {
            resize_window(ctx, xdg_toplevel, placement.target, from, offset);
        }
    }
}

/// The offset of `zaura_surface.set_parent(self, dx, dy)` that puts the
/// window's top-left corner at `target`'s, from `origin`, the origin of its
/// client area.
///
/// Exo adds the offset to the client area's origin
/// (`ShellSurface::OnSetParent` converts it from the parent's contents
/// view) and puts the window's top-left corner, frame included, there, so a
/// caption above the client area needs no correction.
///
/// # Returns
///
/// `None` when the offset overflows.
fn set_parent_offset(target: Rect, origin: Point) -> Option<Point> {
    Some((
        target.x.checked_sub(origin.0)?,
        target.y.checked_sub(origin.1)?,
    ))
}

/// The height of the caption above a client area at `origin` whose
/// window's top-left corner is at `target`'s.
///
/// A frame Exo draws puts its caption above the client area, so with the
/// window's corner at the target's, the client area is straight below it,
/// within the target's height. Client-side decorated windows have no such
/// frame, and a caption of 0.
///
/// # Returns
///
/// `None` when an origin there cannot belong to a window whose corner is
/// at the target's.
fn caption_height(target: Rect, origin: Point) -> Option<i32> {
    match (
        origin.0.checked_sub(target.x),
        origin.1.checked_sub(target.y),
    ) {
        (Some(0), Some(caption)) if (0..target.height).contains(&caption) => Some(caption),
        _ => None,
    }
}

/// What a move to `target` came to, judged by where the window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveOutcome {
    /// The window moved, and its corner is at the target's.
    Placed,
    /// The window's corner already was at the target's, so Exo had
    /// nothing to move. The offset is not nothing for a window with a
    /// frame Exo draws: it runs from the client area's origin, below the
    /// caption, to the corner.
    AlreadyPlaced,
    /// The window moved elsewhere: Exo adjusted the move, or the user
    /// dragged the window.
    MovedElsewhere,
    /// The window stayed where it was, away from the target: Exo ignored
    /// the move.
    Ignored,
}

impl MoveOutcome {
    /// The outcome of a move to `target` of a window whose client area's
    /// origin was `from` before the move and is `to` after it.
    fn of(target: Rect, from: Point, to: Point) -> Self {
        match (to != from, caption_height(target, to).is_some()) {
            (true, true) => Self::Placed,
            (false, true) => Self::AlreadyPlaced,
            (true, false) => Self::MovedElsewhere,
            (false, false) => Self::Ignored,
        }
    }
}

/// The size of the client area that fills `target` once the window's
/// top-left corner is at `target`'s and its client area at `origin`: the
/// target less the caption above it ([`caption_height`]). An origin that
/// is not where such a window's would be shows no caption: Exo adjusted
/// the move, and the client area gets the whole target.
fn client_size(target: Rect, origin: Point) -> (i32, i32) {
    (
        target.width,
        target.height - caption_height(target, origin).unwrap_or(0),
    )
}

/// The second step: move the window with `set_parent` from the origin Exo
/// reported once it was restored, and ask for its new origin.
fn move_window(ctx: &mut Context, xdg_toplevel: u32, aura_toplevel: u32, target: Rect) {
    let Some(origin) = ctx
        .placement()
        .and_then(|state| state.origin(GuestId(xdg_toplevel)))
    else {
        log::info!(
            "Window placement did not move xdg_toplevel {}: Exo reported no origin for it",
            xdg_toplevel
        );
        return;
    };
    let Some(offset) = set_parent_offset(target, origin) else {
        log::info!(
            "Window placement did not move xdg_toplevel {}: the offset from ({}, {}) overflows",
            xdg_toplevel,
            origin.0,
            origin.1
        );
        return;
    };
    // The toplevel's role and Aura objects end together with its record, so
    // they are all live while it is.
    let (Some(host_xdg_toplevel), Some(zaura_surface)) = (
        ctx.shadow_table.get_host_id(xdg_toplevel),
        ctx.xdg_roles
            .wl_surface_of_toplevel(xdg_toplevel)
            .and_then(|wl_surface| ensure_host_zaura_surface(ctx, wl_surface)),
    ) else {
        return;
    };
    ctx.client_to_host_queue.extend([
        zaura_surface::req::set_parent(zaura_surface, Some(zaura_surface), offset.0, offset.1),
        xdg_toplevel::req::unset_fullscreen(host_xdg_toplevel),
    ]);
    start_step(
        ctx,
        xdg_toplevel,
        aura_toplevel,
        target,
        PlacementStep::Move {
            from: origin,
            offset,
        },
    );
    log::debug!(
        "window layout: xdg_toplevel={} zaura_surface={} set_parent(self, {}, {}) from ({}, {})",
        xdg_toplevel,
        zaura_surface,
        offset.0,
        offset.1,
        origin.0,
        origin.1
    );
}

/// The last step: check that Exo did not ignore the move
/// ([`MoveOutcome`]), and give the guest a configure of the size that
/// fills the target.
///
/// The configure repeats the newest configure's states and serial. The
/// guest's ack of that serial reaches Exo, which accepts an ack of a
/// serial it already had acked.
fn resize_window(ctx: &mut Context, xdg_toplevel: u32, target: Rect, from: Point, offset: Point) {
    let Some(state) = ctx.placement() else {
        return;
    };
    let Some(origin) = state.origin(GuestId(xdg_toplevel)) else {
        return;
    };
    match MoveOutcome::of(target, from, origin) {
        MoveOutcome::Ignored => {
            MOVES.ignored(xdg_toplevel, offset);
            return;
        }
        MoveOutcome::Placed => MOVES.placed(),
        MoveOutcome::AlreadyPlaced | MoveOutcome::MovedElsewhere => {}
    }
    let Some((states, serial)) = state
        .last_configure(GuestId(xdg_toplevel))
        .map(|(states, serial)| (states.to_vec(), serial))
    else {
        return;
    };
    let Some(xdg_surface) = ctx.xdg_roles.xdg_surface_of_toplevel(xdg_toplevel) else {
        return;
    };
    let (width, height) = client_size(target, origin);
    // The states are a copy of an array that fit the host's message.
    if queue_or_drop(
        &mut ctx.host_to_client_queue,
        xdg_toplevel::evt::configure(xdg_toplevel, width, height, &states),
        format_args!("the configure of xdg_toplevel {xdg_toplevel}"),
    ) {
        ctx.host_to_client_queue
            .push(xdg_surface::evt::configure(xdg_surface, serial));
    }
    log::debug!(
        "window layout: xdg_toplevel={} moved to ({}, {}); configure {}x{} serial={}",
        xdg_toplevel,
        origin.0,
        origin.1,
        width,
        height,
        serial
    );
}

// ── Toplevel lifecycle ────────────────────────────────────────────────────

/// Give a guest XDG toplevel its host-only Aura toplevel in screen
/// coordinates, unless it has one.
///
/// Called once the generated dispatcher has accepted and mapped
/// `xdg_surface.get_toplevel`, so that Exo gets
/// `set_supports_screen_coordinates` before the toplevel's first commit,
/// as the protocol asks; [`shell_bound`] and [`place_focused`] retry a
/// toplevel created while no `zaura_shell` was bound.
///
/// # Returns
///
/// The Aura toplevel, or `None` while placement is off or the host has no
/// `zaura_shell` new enough for placement.
pub(crate) fn attach_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) -> Option<u32> {
    if let Some(aura_toplevel) = ctx
        .placement()?
        .aura_toplevel(GuestId(xdg_toplevel_guest_id))
    {
        return Some(aura_toplevel.0);
    }
    let xdg_toplevel_host_id = ctx.shadow_table.get_host_id(xdg_toplevel_guest_id)?;
    if ctx
        .host_zaura_shell
        .is_none_or(|shell| shell.version < PLACEMENT_SHELL_SINCE)
    {
        return None;
    }
    let aura_toplevel = aura_objects::create(ctx, AuraChild::TOPLEVEL, xdg_toplevel_host_id)?;
    ctx.client_to_host_queue
        .push(zaura_toplevel::req::set_supports_screen_coordinates(
            aura_toplevel,
        ));
    ctx.placement_mut()?
        .attach(GuestId(xdg_toplevel_guest_id), HostId(aura_toplevel));
    Some(aura_toplevel)
}

/// Release the Aura toplevel of a guest XDG toplevel whose role ended.
///
/// Called whenever the XDG role ends (see `XdgRoles`' remove methods),
/// before the role's own destructor is queued. A barrier still in flight
/// stays pending until its `done`, which then finds nothing to continue.
pub(crate) fn release_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    let Some(state) = ctx.placement_mut() else {
        return;
    };
    if let Some(aura_toplevel) = state.take(GuestId(xdg_toplevel_guest_id)) {
        aura_objects::release(ctx, AuraChild::TOPLEVEL, aura_toplevel.0);
    }
}

// ── Configures ────────────────────────────────────────────────────────────

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

/// Deliver a host `zaura_toplevel.configure` to the guest as its
/// `xdg_toplevel.configure`, with its states filtered to the guest's
/// `xdg_toplevel` version, and record the window's origin and those
/// states. An event for an Aura toplevel that is no longer attached is
/// dropped.
pub(crate) fn translate_aura_configure(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    states: &[u8],
) {
    let Some(xdg_toplevel) = ctx
        .placement()
        .and_then(|state| state.xdg_toplevel(HostId(zaura_toplevel_host_id)))
    else {
        return;
    };
    log::debug!(
        "zaura_toplevel.configure host={} guest={} bounds={}x{} origin=({}, {})",
        zaura_toplevel_host_id,
        xdg_toplevel.0,
        width,
        height,
        x,
        y
    );
    // Without a recorded version, assume the baseline so only states every
    // xdg_toplevel client understands are delivered.
    let guest_version = ctx
        .shadow_table
        .guest_object_version(xdg_toplevel.0)
        .unwrap_or(1);
    let states = guest_xdg_toplevel_states(states, guest_version);
    // The states array is host data, so its encoder is fallible. It is a
    // filtered copy of an array that already fit in the host's message, so
    // a failure means corrupted state and is only logged.
    queue_or_drop(
        &mut ctx.host_to_client_queue,
        xdg_toplevel::evt::configure(xdg_toplevel.0, width, height, &states),
        format_args!("the configure of xdg_toplevel {}", xdg_toplevel.0),
    );
    if let Some(state) = ctx.placement_mut() {
        state.configured(xdg_toplevel, (x, y), states);
    }
}

/// Record a host `zaura_toplevel.origin_change`: the window moved without
/// a configure. The guest gets nothing; `xdg_toplevel` has no origin.
pub(crate) fn origin_changed(ctx: &mut Context, zaura_toplevel_host_id: u32, x: i32, y: i32) {
    log::debug!(
        "zaura_toplevel.origin_change host={} origin=({}, {})",
        zaura_toplevel_host_id,
        x,
        y
    );
    let Some(state) = ctx.placement_mut() else {
        return;
    };
    if let Some(xdg_toplevel) = state.xdg_toplevel(HostId(zaura_toplevel_host_id)) {
        state.moved(xdg_toplevel, (x, y));
    }
}

/// Record the serial of a host `xdg_surface.configure`, which the caller
/// forwards, for the configure a placement sends the guest.
pub(crate) fn surface_configured(ctx: &mut Context, guest_xdg_surface_id: u32, serial: u32) {
    let Some(xdg_toplevel) = ctx.xdg_roles.toplevel_of_xdg_surface(guest_xdg_surface_id) else {
        return;
    };
    if let Some(state) = ctx.placement_mut() {
        state.serial_sent(GuestId(xdg_toplevel), serial);
    }
}

/// Fixtures of the placement tests.
#[cfg(test)]
mod fixture {
    use super::*;
    use crate::handler::compositor::CompositorHandler;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell};
    use crate::protocols::wayland::{wl_keyboard, wl_output, wl_registry};
    use crate::protocols::xdg_shell::xdg_toplevel;
    use crate::protocols::xdg_shell::xdg_wm_base;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};
    use crate::state::OutputState;
    use crate::test_support::{decode, msg_sender};
    use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};
    use std::collections::VecDeque;

    pub(super) use crate::handler::test_support::{
        add_toplevel, toplevel_ctx_with, Paired, SURFACE, TOPLEVEL, XDG_SURFACE,
    };

    /// Host keyboard 5 (guest 10, on seat 11).
    pub(super) const KEYBOARD: Paired = Paired { guest: 10, host: 5 };
    /// A 3840x2160 output at 1x.
    pub(super) const OUTPUT: u32 = 25;

    /// Focus [`KEYBOARD`] on [`SURFACE`], which is all focus resolution
    /// reads.
    pub(super) fn focus(ctx: &mut Context) {
        let seat = 11;
        ctx.shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        ctx.keyboards
            .add_for_test(KEYBOARD.guest, KEYBOARD.host, seat);
        ctx.keyboard_focus
            .set_for_test(HostId(KEYBOARD.host), seat, SURFACE.guest, SURFACE.host);
    }

    /// An output of `mode` at 1x, at `origin_x`.
    pub(super) fn output(origin_x: i32, mode: (i32, i32)) -> OutputState {
        OutputState {
            origin_x,
            mode_width: mode.0,
            mode_height: mode.1,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        }
    }

    /// [`toplevel_ctx_with`] placement on, without bindings.
    pub(super) fn placement_ctx() -> Context {
        toplevel_ctx_with(PlacementConfig::active_for_test(&[]))
    }

    /// [`placement_ctx`], focused through [`KEYBOARD`], on [`OUTPUT`].
    pub(super) fn focused_ctx() -> Context {
        let mut ctx = placement_ctx();
        focus(&mut ctx);
        ctx.outputs.bind_for_test(OUTPUT, output(0, (3840, 2160)));
        ctx
    }

    /// The named preset `name`.
    pub(super) fn preset(name: &str) -> NormalizedRect {
        NormalizedRect::preset(name).unwrap_or_else(|| panic!("no preset {name:?}"))
    }

    /// Place the focused toplevel at the named preset `name`; returns what
    /// it sends the host.
    pub(super) fn place(ctx: &mut Context, name: &str) -> Vec<(u32, AnyRequest)> {
        ctx.client_to_host_queue.clear();
        assert_eq!(
            place_focused(ctx, HostId(KEYBOARD.host), preset(name)),
            Ok(())
        );
        host_requests(ctx)
    }

    /// Each message of `queue`, decoded as the interface `interface_of` its
    /// sender has. Panics if a message leaves a descriptor unread.
    pub(super) fn decode_queue<'a, M>(
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
    pub(super) fn host_requests(ctx: &Context) -> Vec<(u32, AnyRequest)> {
        let table = &ctx.shadow_table;
        // The fixtures do not pair the display.
        let interface_of = |id| match id {
            WL_DISPLAY_ID => Some("wl_display"),
            id => table.get_host_interface(id),
        };
        decode_queue(
            &ctx.client_to_host_queue,
            interface_of,
            AnyRequest::from_wire,
        )
    }

    /// What `ctx` queued for the guest.
    pub(super) fn guest_events(ctx: &Context) -> Vec<(u32, AnyEvent)> {
        let table = &ctx.shadow_table;
        decode_queue(
            &ctx.host_to_client_queue,
            |id| table.get_interface(id),
            AnyEvent::from_wire,
        )
    }

    /// Deliver the host `wl_callback.done` of `callback`.
    pub(super) fn done(ctx: &mut Context, callback: HostId) {
        use crate::protocols::wayland::wl_callback::WlCallbackHandler;

        ctx.last_sender_id = callback.0;
        assert_eq!(
            crate::handler::callback::CallbackHandler.on_done(ctx, 0),
            Action::Drop
        );
    }

    /// The host's `zaura_toplevel.configure` of `aura_toplevel`, at
    /// `origin`.
    pub(super) fn aura_configure(
        ctx: &mut Context,
        aura_toplevel: u32,
        origin: (i32, i32),
        size: (i32, i32),
        states: &[u8],
    ) {
        ctx.last_sender_id = aura_toplevel;
        let action = ZauraToplevelHandler::on_configure(
            &mut CompositorHandler,
            ctx,
            origin.0,
            origin.1,
            size.0,
            size.1,
            states,
        );
        assert_eq!(action, Action::Drop);
    }

    /// The guest's `xdg_toplevel.configure` of [`TOPLEVEL`].
    pub(super) fn toplevel_configure(size: (i32, i32), states: &[u32]) -> (u32, AnyEvent) {
        let configure = xdg_toplevel::Event::Configure {
            width: size.0,
            height: size.1,
            states: encode_states(states),
        };
        (TOPLEVEL.guest, configure.into())
    }

    pub(super) fn encode_states(states: &[u32]) -> Vec<u8> {
        states
            .iter()
            .flat_map(|state| state.to_ne_bytes())
            .collect()
    }

    /// [`TOPLEVEL`]'s Aura toplevel, created if it has none.
    pub(super) fn aura_toplevel(ctx: &mut Context) -> u32 {
        attach_toplevel(ctx, TOPLEVEL.guest).expect("an Aura toplevel")
    }

    /// The placement in flight for [`TOPLEVEL`].
    pub(super) fn in_flight(ctx: &Context) -> Option<Placement> {
        ctx.placement()?.placement(GuestId(TOPLEVEL.guest))
    }

    /// A whole proxy session for placement tests, with the objects
    /// placement needs: the proxy's own `zaura_shell`, a 1920x1080 output at
    /// 1x whose shelf takes 48 pixels, an `xdg_wm_base`, a keyboard and a
    /// surface.
    pub(super) struct Desk {
        pub(super) s: Session,
        /// The host `zaura_shell` the proxy bound.
        pub(super) shell: u32,
        pub(super) output: Object,
        /// The host-only `zaura_output` of `output`.
        pub(super) aura_output: u32,
        pub(super) wm_base: Object,
        pub(super) keyboard: Object,
        pub(super) surface: Object,
    }

    /// The work area of [`Desk`]'s output: the shelf takes its bottom.
    pub(super) const DESK_WORK_AREA: (i32, i32) = (1920, 1080 - 48);

    impl Desk {
        /// A session under `placement`, which must be on: the output gets
        /// its Aura output, whose metadata the host sends. What either
        /// peer gets for this is checked and not kept.
        pub(super) async fn new(placement: PlacementConfig) -> Self {
            let mut d = Self::unmeasured(placement).await;
            assert_eq!(d.measure(0, 1000, 1000).await, []);
            assert_eq!(d.s.host_received(), []);
            d
        }

        /// The host sends the output's metadata, its mode at `origin_x`
        /// and Aura's `current_scale` and `device_scale` thousandths and
        /// insets, which the guest gets as its output's events first; returns
        /// what else the guest gets. What the proxy sends the host is kept.
        pub(super) async fn measure(
            &mut self,
            origin_x: i32,
            current_scale: u32,
            device_scale: u32,
        ) -> Vec<(u32, AnyEvent)> {
            let (output, aura_output) = (self.output, self.aura_output);
            self.s
                .host([
                    wl_output::evt::geometry(output.host, origin_x, 0, 0, 0, 0, "", "", 0).unwrap(),
                    wl_output::evt::mode(output.host, wl_output::MODE_CURRENT, 1920, 1080, 60),
                    zaura_output::evt::scale(
                        aura_output,
                        zaura_output::SCALE_PROPERTY_CURRENT,
                        current_scale,
                    ),
                    zaura_output::evt::device_scale_factor(aura_output, device_scale),
                    zaura_output::evt::insets(aura_output, 0, 0, 1080 - DESK_WORK_AREA.1, 0),
                    wl_output::evt::done(output.host),
                ])
                .await;
            let mut guest = self.s.guest_received();
            let others = guest.split_off(
                guest
                    .iter()
                    .position(|(sender, _)| *sender != output.id)
                    .unwrap_or(guest.len()),
            );
            assert!(!guest.is_empty(), "the guest gets its output's events");
            others
        }

        /// [`Self::new`] whose host has not sent the output's metadata, so
        /// that the output cannot place a window yet.
        pub(super) async fn unmeasured(placement: PlacementConfig) -> Self {
            let mut s = Session::with_context(Context::with_placement_for_test(placement));
            let registry = s.registry().await;
            s.advertise("zaura_shell", zaura_shell::VERSION).await;
            let host = s.host_received();
            let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] = &host[..]
            else {
                panic!("the proxy did not bind zaura_shell: {host:?}");
            };
            let shell = id.2;
            assert_eq!(s.guest_received().len(), 1, "gtk_shell1 is advertised");

            let output_name = s.advertise("wl_output", wl_output::VERSION).await;
            let advertised = s.guest_received();
            let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
                &advertised[..]
            else {
                panic!("the output was not advertised: {advertised:?}");
            };
            let id = s.new_id();
            s.guest([
                wl_registry::req::bind(registry.id, output_name, "wl_output", *version, id)
                    .unwrap(),
            ])
            .await;
            let host = s.host_received();
            let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: bound, .. })), (
                aura_shell,
                AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput {
                    id: aura_output,
                    output: aura_of,
                }),
            )] = &host[..]
            else {
                panic!("the output got no Aura output: {host:?}");
            };
            assert_eq!((*aura_shell, *aura_of), (shell, bound.2));
            let output = Object { id, host: bound.2 };
            let aura_output = *aura_output;

            let wm_base = s.bind_paired("xdg_wm_base", xdg_wm_base::VERSION).await;
            let (_, keyboard) = s.keyboard().await;
            let surface = s.surface().await;
            Self {
                s,
                shell,
                output,
                aura_output,
                wm_base,
                keyboard,
                surface,
            }
        }

        /// The host gives the keyboard's focus to the surface and holds
        /// Alt.
        pub(super) async fn focus(&mut self) {
            let alt = 1 << 3;
            self.s
                .host([
                    wl_keyboard::evt::enter(self.keyboard.host, 1, self.surface.host, &[]).unwrap(),
                    wl_keyboard::evt::modifiers(self.keyboard.host, 2, alt, 0, 0, 0),
                ])
                .await;
            assert_eq!(self.s.guest_received().len(), 3, "enter and modifiers");
            assert_eq!(self.s.host_received(), []);
        }

        /// The host sends a press and release of Q, which with Alt is the
        /// `<Alt>q` chord, as key events `serial` and `serial + 1`.
        pub(super) async fn chord(&mut self, serial: u32) {
            let q = 16;
            self.s
                .host([
                    wl_keyboard::evt::key(
                        self.keyboard.host,
                        serial,
                        serial,
                        q,
                        wl_keyboard::KEY_STATE_PRESSED,
                    ),
                    wl_keyboard::evt::key(
                        self.keyboard.host,
                        serial + 1,
                        serial + 1,
                        q,
                        wl_keyboard::KEY_STATE_RELEASED,
                    ),
                ])
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::handler::compositor::CompositorHandler;
    use crate::handler::display::DisplayHandler;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_shell;
    use crate::protocols::aura_shell::zaura_toplevel::ZauraToplevelHandler;
    use crate::protocols::wayland::wl_callback;
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::protocols::wayland::wl_surface::{self, WlSurfaceHandler};
    use crate::protocols::xdg_shell::xdg_toplevel::XdgToplevelHandler;
    use crate::protocols::xdg_shell::xdg_wm_base;
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::test_support::queued_headers;
    use crate::wire::{Action, WL_DISPLAY_ID};

    #[test]
    fn grid_bounds_use_two_by_two_work_area_cells() {
        for (name, bounds) in [
            ("top-left", Rect::new(0, 0, 1920, 1080)),
            ("top", Rect::new(0, 0, 3840, 1080)),
            ("top-right", Rect::new(1920, 0, 1920, 1080)),
            ("left", Rect::new(0, 0, 1920, 2160)),
            ("fullscreen", Rect::new(0, 0, 3840, 2160)),
            ("bottom-left", Rect::new(0, 1080, 1920, 1080)),
            ("bottom", Rect::new(0, 1080, 3840, 1080)),
            ("bottom-right", Rect::new(1920, 1080, 1920, 1080)),
            ("right", Rect::new(1920, 0, 1920, 2160)),
        ] {
            assert_eq!(
                bounds_for(preset(name), output(0, (3840, 2160))),
                Some(bounds),
                "{name}"
            );
        }
    }

    #[test]
    fn grid_bounds_reject_coordinate_overflow() {
        let far_right = output(i32::MAX, (4, 4));
        for name in ["top-right", "bottom-right", "right"] {
            assert_eq!(bounds_for(preset(name), far_right), None, "{name}");
        }
        assert_eq!(
            bounds_for(preset("top-left"), output(0, (1, 1))),
            None,
            "split layouts must never emit zero-sized bounds"
        );
        assert_eq!(
            bounds_for(preset("fullscreen"), output(0, (1, 1))),
            Some(Rect::new(0, 0, 1, 1))
        );
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
        // Where placing the toplevel on the right half puts it.
        let place_right = |ctx: &mut Context| {
            let target =
                resolve_target(ctx, HostId(KEYBOARD.host), preset("right")).expect("a target");
            (target.bounds, target.output_host_id)
        };

        move_surface(&mut ctx, SURFACE.host, None, external);
        assert_eq!(
            place_right(&mut ctx),
            (Rect::new(-960, 0, 960, 1080), external)
        );
        move_surface(&mut ctx, SURFACE.host, Some(external), panel);
        assert_eq!(place_right(&mut ctx), (Rect::new(640, 0, 640, 752), panel));

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
            (Rect::new(-540, 0, 540, 1920), external)
        );
    }

    #[test]
    fn an_old_shell_is_reported_once_per_process() {
        // The test's own flag: the process's belongs to the sessions that
        // other tests run in parallel.
        let once = WarnOnce::new();
        assert!(!warn_if_shell_cannot_place(&once, PLACEMENT_SHELL_SINCE));
        assert!(warn_if_shell_cannot_place(&once, PLACEMENT_SHELL_SINCE - 1));
        assert!(!warn_if_shell_cannot_place(
            &once,
            PLACEMENT_SHELL_SINCE - 1
        ));
    }

    // ── Origin math ───────────────────────────────────────────────────────

    /// The offset takes the window's corner from its client area's origin
    /// to the target's corner, whether or not a caption sits above the
    /// client area, and an offset that overflows is none.
    #[test]
    fn set_parent_offset_runs_from_the_client_origin_to_the_target() {
        let target = Rect::new(960, 0, 960, 516);
        assert_eq!(set_parent_offset(target, (300, 200)), Some((660, -200)));
        assert_eq!(set_parent_offset(target, (960, 0)), Some((0, 0)));
        assert_eq!(set_parent_offset(target, (-1080, 32)), Some((2040, -32)));
        assert_eq!(set_parent_offset(target, (i32::MIN, 0)), None);
        assert_eq!(
            set_parent_offset(Rect::new(i32::MIN, 0, 1, 1), (1, 0)),
            None
        );
    }

    /// The client area fills the target below a caption Exo draws; a
    /// client-side decorated window, or a move Exo adjusted, gets the
    /// whole target.
    #[test]
    fn the_client_size_leaves_room_for_a_caption() {
        let target = Rect::new(960, 0, 960, 516);
        // Client-side decorations: the client area is the window.
        assert_eq!(client_size(target, (960, 0)), (960, 516));
        // A 32-DIP caption.
        assert_eq!(client_size(target, (960, 32)), (960, 484));
        // Not straight below the corner, above it, or below the target.
        for origin in [(961, 32), (960, -1), (960, 516), (i32::MIN, 0)] {
            assert_eq!(client_size(target, origin), (960, 516), "{origin:?}");
        }
    }

    /// A move is judged by where the window ended: Exo ignored it only if
    /// the window stayed away from the target's corner. A window that
    /// stayed with its corner there, below a caption or not, was already
    /// placed.
    #[test]
    fn a_move_is_ignored_only_if_the_window_stayed_away_from_the_target() {
        use MoveOutcome::{AlreadyPlaced, Ignored, MovedElsewhere, Placed};
        let target = Rect::new(0, 0, 960, 516);
        for (from, to, outcome) in [
            ((300, 200), (0, 32), Placed),
            ((300, 200), (0, 0), Placed),
            ((300, 200), (300, 200), Ignored),
            // A 32-DIP caption, or none.
            ((0, 32), (0, 32), AlreadyPlaced),
            ((0, 0), (0, 0), AlreadyPlaced),
            ((300, 200), (310, 200), MovedElsewhere),
            ((0, 32), (0, 516), MovedElsewhere),
        ] {
            assert_eq!(
                MoveOutcome::of(target, from, to),
                outcome,
                "{from:?} {to:?}"
            );
        }
    }

    /// An ignored move is warned about once, and only while no move has
    /// placed a window; once one has, it is a dragged window, logged at
    /// info, and the warning is left for a host that cannot place.
    #[test]
    fn an_ignored_move_is_warned_about_once_until_a_move_works() {
        use log::Level::{Debug, Info, Warn};
        // The tests' own logs: the process's belongs to the sessions that
        // other tests run in parallel.
        let moves = MoveLog::new();
        assert_eq!(moves.ignored(1, (5, 0)), Warn);
        assert_eq!(moves.ignored(1, (5, 0)), Debug);
        moves.placed();
        assert_eq!(moves.ignored(1, (5, 0)), Info);

        let moves = MoveLog::new();
        moves.placed();
        assert_eq!(moves.ignored(1, (5, 0)), Info);
        assert!(!moves.warned.logged());
    }

    // ── The placement on the wire ─────────────────────────────────────────

    /// A guest toplevel in a [`Desk`] under `<Alt>q=top-left`, with its
    /// host objects.
    struct Window {
        xdg_surface: u32,
        host_xdg_surface: u32,
        toplevel: u32,
        host_toplevel: u32,
        aura_toplevel: u32,
    }

    /// The `xdg_toplevel.configure` states of an activated window.
    const ACTIVATED: [u32; 1] = [xdg_toplevel::STATE_ACTIVATED];

    impl Window {
        /// Create a toplevel on the desk's surface: it gets an Aura
        /// toplevel in screen coordinates before its first commit.
        async fn new(d: &mut Desk) -> Self {
            let xdg_surface = d.s.new_id();
            let host_xdg_surface =
                d.s.create(xdg_wm_base::req::get_xdg_surface(
                    d.wm_base.id,
                    xdg_surface,
                    d.surface.id,
                ))
                .await;
            let toplevel = d.s.new_id();
            d.s.guest([xdg_surface::req::get_toplevel(xdg_surface, toplevel)])
                .await;
            let host = d.s.host_received();
            let [(
                _,
                AnyRequest::XdgSurface(xdg_surface::Request::GetToplevel { id: host_toplevel }),
            ), (
                _,
                AnyRequest::ZauraShell(zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                    id: aura_toplevel,
                    ..
                }),
            ), _] = host[..]
            else {
                panic!("the toplevel got no Aura toplevel: {host:?}");
            };
            assert_eq!(
                host,
                [
                    (
                        host_xdg_surface,
                        xdg_surface::Request::GetToplevel { id: host_toplevel }.into()
                    ),
                    (
                        d.shell,
                        zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                            id: aura_toplevel,
                            toplevel: host_toplevel
                        }
                        .into()
                    ),
                    (
                        aura_toplevel,
                        zaura_toplevel::Request::SetSupportsScreenCoordinates {}.into()
                    ),
                ]
            );
            Self {
                xdg_surface,
                host_xdg_surface,
                toplevel,
                host_toplevel,
                aura_toplevel,
            }
        }

        /// The host configures the window at `origin`, 640x480 and
        /// activated, with `serial`; the guest gets it as an XDG
        /// configure.
        async fn configured(&self, d: &mut Desk, origin: (i32, i32), serial: u32) {
            let states = encode_states(&ACTIVATED);
            d.s.host([
                zaura_toplevel::evt::configure(
                    self.aura_toplevel,
                    origin.0,
                    origin.1,
                    640,
                    480,
                    &states,
                )
                .unwrap(),
                xdg_surface::evt::configure(self.host_xdg_surface, serial),
            ])
            .await;
            assert_eq!(
                d.s.guest_received(),
                [
                    self.configure((640, 480)),
                    (
                        self.xdg_surface,
                        xdg_surface::Event::Configure { serial }.into()
                    ),
                ]
            );
        }

        /// The guest's activated `xdg_toplevel.configure` of `size`.
        fn configure(&self, size: (i32, i32)) -> (u32, AnyEvent) {
            let configure = xdg_toplevel::Event::Configure {
                width: size.0,
                height: size.1,
                states: encode_states(&ACTIVATED),
            };
            (self.toplevel, configure.into())
        }
    }

    /// The host's `done` of placement barrier `callback`, and its
    /// `delete_id`.
    async fn barrier_done(d: &mut Desk, callback: u32) {
        d.s.host([
            wl_callback::evt::done(callback, 0),
            wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
        ])
        .await;
    }

    /// The placement contract on the wire, through a whole session: the
    /// window leaves fullscreen and maximized state, moves with
    /// `set_parent(self, dx, dy)` by the offset from the origin Exo then
    /// reports to the target's corner, and once Exo reports the origin it
    /// moved to, the guest gets a configure of the target's size less the
    /// caption that origin shows, with the newest states and serial. Its
    /// Aura surface gets no application ID and its text input nothing:
    /// these are every message either peer gets.
    #[tokio::test]
    async fn a_placement_moves_the_window_with_set_parent_and_resizes_it() {
        let mut d = Desk::new(PlacementConfig::active_for_test(&[("<Alt>q", "top-left")])).await;
        let w = Window::new(&mut d).await;
        w.configured(&mut d, (300, 200), 5).await;

        d.focus().await;
        d.chord(3).await;
        assert_eq!(d.s.guest_received(), [], "the chord is consumed");
        let host = d.s.host_received();
        let [(
            _,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface {
                id: aura_surface, ..
            }),
        ), .., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("no placement: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    d.shell,
                    zaura_shell::Request::GetAuraSurface {
                        id: aura_surface,
                        surface: d.surface.host
                    }
                    .into()
                ),
                (
                    w.host_toplevel,
                    xdg_toplevel::Request::UnsetFullscreen {}.into()
                ),
                (
                    w.host_toplevel,
                    xdg_toplevel::Request::UnsetMaximized {}.into()
                ),
                (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into()),
            ]
        );

        // Exo answers with a configure at the window's origin.
        w.configured(&mut d, (300, 200), 6).await;
        barrier_done(&mut d, callback).await;
        assert_eq!(d.s.guest_received(), []);
        let host = d.s.host_received();
        let [.., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("no move: {host:?}");
        };
        assert_eq!(
            host,
            [
                (
                    aura_surface,
                    zaura_surface::Request::SetParent {
                        parent: aura_surface,
                        x: -300,
                        y: -200
                    }
                    .into()
                ),
                (
                    w.host_toplevel,
                    xdg_toplevel::Request::UnsetFullscreen {}.into()
                ),
                (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into()),
            ]
        );

        // Exo put the window's corner at the work area's, and its client
        // area 32 DIPs lower, below its caption.
        w.configured(&mut d, (0, 32), 7).await;
        barrier_done(&mut d, callback).await;
        let (width, height) = (DESK_WORK_AREA.0 / 2, DESK_WORK_AREA.1 / 2);
        assert_eq!(
            d.s.guest_received(),
            [
                w.configure((width, height - 32)),
                (
                    w.xdg_surface,
                    xdg_surface::Event::Configure { serial: 7 }.into()
                ),
            ]
        );
        assert_eq!(d.s.host_received(), []);
    }

    /// A window Exo did not move is not resized: the placement stops.
    #[tokio::test]
    async fn a_move_exo_ignores_ends_the_placement() {
        let mut d = Desk::new(PlacementConfig::active_for_test(&[("<Alt>q", "top-left")])).await;
        let w = Window::new(&mut d).await;
        w.configured(&mut d, (300, 200), 5).await;
        d.focus().await;
        d.chord(3).await;
        for step in ["restore", "move"] {
            let host = d.s.host_received();
            let [.., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
            else {
                panic!("no {step} barrier: {host:?}");
            };
            w.configured(&mut d, (300, 200), 6).await;
            barrier_done(&mut d, callback).await;
        }
        assert_eq!(d.s.guest_received(), []);
        assert_eq!(d.s.host_received(), []);
    }

    /// A window with a frame Exo draws whose corner already is at the
    /// target's, as a top-left placement leaves it before a left one, is
    /// resized: the offset runs from its client area, below the caption,
    /// to the corner, so it is not nothing, and Exo, with nothing to move,
    /// reports the same origin, which is no sign of an ignored move.
    #[tokio::test]
    async fn a_framed_window_already_at_the_corner_is_resized() {
        let mut d = Desk::new(PlacementConfig::active_for_test(&[("<Alt>q", "left")])).await;
        let w = Window::new(&mut d).await;
        // The client area is 32 DIPs below the work area's corner.
        w.configured(&mut d, (0, 32), 5).await;
        d.focus().await;
        d.chord(3).await;
        let host = d.s.host_received();
        let [(
            _,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface {
                id: aura_surface, ..
            }),
        ), .., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("no placement: {host:?}");
        };
        w.configured(&mut d, (0, 32), 6).await;
        barrier_done(&mut d, callback).await;
        let host = d.s.host_received();
        let [.., (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..]
        else {
            panic!("no move: {host:?}");
        };
        assert_eq!(
            host[0],
            (
                aura_surface,
                zaura_surface::Request::SetParent {
                    parent: aura_surface,
                    x: 0,
                    y: -32
                }
                .into()
            )
        );
        w.configured(&mut d, (0, 32), 7).await;
        barrier_done(&mut d, callback).await;
        assert_eq!(
            d.s.guest_received(),
            [
                w.configure((DESK_WORK_AREA.0 / 2, DESK_WORK_AREA.1 - 32)),
                (
                    w.xdg_surface,
                    xdg_surface::Event::Configure { serial: 7 }.into()
                ),
            ]
        );
        assert_eq!(d.s.host_received(), []);
    }

    /// A newer chord replaces the placement in flight: the older one's
    /// barrier then moves nothing, and the newer one's moves the window to
    /// its own target.
    #[test]
    fn a_newer_placement_replaces_the_one_in_flight() {
        let mut ctx = focused_ctx();
        let aura_toplevel = aura_toplevel(&mut ctx);
        aura_configure(&mut ctx, aura_toplevel, (100, 100), (640, 480), &[]);
        ctx.host_to_client_queue.clear();
        place(&mut ctx, "top-left");
        let older = in_flight(&ctx).expect("a placement").barrier;
        place(&mut ctx, "right");
        let newer = in_flight(&ctx).expect("a placement");
        assert_eq!(newer.target, Rect::new(1920, 0, 1920, 2160));

        ctx.client_to_host_queue.clear();
        done(&mut ctx, older);
        assert_eq!(host_requests(&ctx), []);
        assert_eq!(in_flight(&ctx), Some(newer));

        done(&mut ctx, newer.barrier);
        let aura_surface = ctx
            .aura_surfaces
            .get(SURFACE.host)
            .expect("an Aura surface");
        let sent = host_requests(&ctx);
        assert_eq!(
            sent[0],
            (
                aura_surface,
                zaura_surface::Request::SetParent {
                    parent: aura_surface,
                    x: 1820,
                    y: -100
                }
                .into()
            )
        );
        assert!(guest_events(&ctx).is_empty());
    }

    /// A toplevel whose role ends while its placement is in flight is not
    /// moved: the barrier finds nothing to continue.
    #[test]
    fn a_released_toplevel_is_not_moved() {
        let mut ctx = focused_ctx();
        let aura_toplevel = aura_toplevel(&mut ctx);
        aura_configure(&mut ctx, aura_toplevel, (100, 100), (640, 480), &[]);
        place(&mut ctx, "top-left");
        let barrier = in_flight(&ctx).expect("a placement").barrier;
        release_toplevel(&mut ctx, TOPLEVEL.guest);
        ctx.client_to_host_queue.clear();
        ctx.host_to_client_queue.clear();

        done(&mut ctx, barrier);
        assert_eq!(host_requests(&ctx), []);
        assert_eq!(guest_events(&ctx), []);
    }

    // ── Aura toplevels and their configures ───────────────────────────────

    #[test]
    fn placement_needs_a_shell_with_output_insets() {
        for (shell_version, expect_toplevel) in [
            (REQ_SET_SUPPORTS_SCREEN_COORDINATES_SINCE, false),
            (EVT_INSETS_SINCE - 1, false),
            (EVT_INSETS_SINCE, true),
        ] {
            let mut ctx = placement_ctx();
            ctx.host_zaura_shell
                .as_mut()
                .expect("bound zaura_shell")
                .version = shell_version;
            let attached = attach_toplevel(&mut ctx, TOPLEVEL.guest);
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
        let mut ctx = placement_ctx();
        ctx.host_zaura_shell
            .as_mut()
            .expect("bound zaura_shell")
            .version = zaura_toplevel::REQ_RELEASE_SINCE - 1;
        let aura_id = aura_toplevel(&mut ctx);
        ctx.client_to_host_queue.clear();

        release_toplevel(&mut ctx, TOPLEVEL.guest);
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            ctx.placement()
                .and_then(|state| state.aura_toplevel(GuestId(TOPLEVEL.guest))),
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
        let mut ctx = placement_ctx();
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
        let first_aura = aura_toplevel(&mut ctx);
        let second_aura = attach_toplevel(&mut ctx, second_toplevel).expect("toplevel");
        ctx.client_to_host_queue.clear();

        ctx.guest_destroyed(XDG_SURFACE.guest);
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(first_aura, zaura_toplevel::REQ_RELEASE)]
        );
        assert_eq!(ctx.xdg_roles.wl_surface_of_toplevel(TOPLEVEL.guest), None);
        assert_eq!(
            ctx.xdg_roles.wl_surface_of_toplevel(second_toplevel),
            Some(SURFACE.guest)
        );
        assert_eq!(
            ctx.placement()
                .and_then(|state| state.aura_toplevel(GuestId(second_toplevel))),
            Some(HostId(second_aura))
        );
    }

    /// A wl_surface destroyed under a live XDG toplevel ends the toplevel's
    /// role: its Aura toplevel and Aura surface are both released before
    /// the wl_surface destructor.
    #[test]
    fn wl_surface_destroy_releases_its_aura_children_first() {
        let mut ctx = placement_ctx();
        let aura_toplevel = aura_toplevel(&mut ctx);
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
                (aura_toplevel, zaura_toplevel::REQ_RELEASE),
                (aura_surface, zaura_surface::REQ_RELEASE),
                (SURFACE.host, wl_surface::REQ_DESTROY),
            ]
        );
    }

    #[test]
    fn xdg_toplevel_destroy_reserves_aura_id_until_host_delete_id() {
        let mut ctx = placement_ctx();
        let aura_id = aura_toplevel(&mut ctx);
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = TOPLEVEL.guest;

        assert_eq!(
            XdgToplevelHandler::on_destroy(&mut CompositorHandler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.placement()
                .and_then(|state| state.aura_toplevel(GuestId(TOPLEVEL.guest))),
            None
        );
        // Child-first: the Aura extension is released before its xdg role.
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [
                (aura_id, zaura_toplevel::REQ_RELEASE),
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
            let mut ctx = placement_ctx();
            ctx.shadow_table
                .set_guest_version_for_test(TOPLEVEL.guest, guest_version);
            let aura_toplevel = aura_toplevel(&mut ctx);
            aura_configure(
                &mut ctx,
                aura_toplevel,
                (10, 20),
                (1920, 1080),
                &encode_states(states),
            );
            assert_eq!(
                guest_events(&ctx),
                [toplevel_configure((1920, 1080), expected)],
                "v{guest_version} {states:?}"
            );
            // A placement repeats what the guest got.
            let state = ctx.placement().expect("placement on");
            assert_eq!(state.origin(GuestId(TOPLEVEL.guest)), Some((10, 20)));
            assert_eq!(
                state.last_configure(GuestId(TOPLEVEL.guest)),
                None,
                "no serial yet"
            );
        }
    }

    #[test]
    fn aura_configure_drops_truncated_state_entry() {
        // State 2 (fullscreen), then half of another entry.
        let mut states = 2_u32.to_ne_bytes().to_vec();
        states.extend([4, 0]);
        assert_eq!(guest_xdg_toplevel_states(&states, 6), 2_u32.to_ne_bytes());
    }

    /// `origin_change` moves the origin placement knows; the guest gets
    /// nothing, since `xdg_toplevel` has no origin.
    #[test]
    fn an_origin_change_is_recorded_and_not_forwarded() {
        let mut ctx = placement_ctx();
        let aura_id = aura_toplevel(&mut ctx);
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = aura_id;
        assert_eq!(
            ZauraToplevelHandler::on_origin_change(&mut CompositorHandler, &mut ctx, 123, 456),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());
        assert_eq!(
            ctx.placement()
                .and_then(|state| state.origin(GuestId(TOPLEVEL.guest))),
            Some((123, 456))
        );
    }

    /// With placement off, the default, the proxy sends either peer what it
    /// sent without placement: it creates no Aura output or toplevel, the
    /// Aura surface gets the guest's ID in the Guest OS namespace,
    /// configures pass through, and a chord reaches the guest, since
    /// nothing is bound.
    #[tokio::test]
    async fn placement_off_traffic_is_unchanged() {
        use crate::protocols::wayland::{wl_keyboard, wl_output, wl_registry};
        use crate::protocols::xdg_shell::xdg_wm_base;
        use crate::proxy::harness::Session;

        let placement = PlacementConfig::for_test();
        let mut s = Session::with_context(Context::with_placement_for_test(placement));
        let registry = s.registry().await;

        // An output gets no Aura output: the host gets only the bind, and
        // a zaura_shell bound later retries none.
        let output_name = s.advertise("wl_output", wl_output::VERSION).await;
        let advertised = s.guest_received();
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            &advertised[..]
        else {
            panic!("the output was not advertised: {advertised:?}");
        };
        let output = s.new_id();
        s.guest([
            wl_registry::req::bind(registry.id, output_name, "wl_output", *version, output)
                .unwrap(),
        ])
        .await;
        let host = s.host_received();
        assert!(
            matches!(
                &host[..],
                [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { name, .. }))]
                    if *name == output_name
            ),
            "{host:?}"
        );
        // The proxy binds zaura_shell for itself, and only it, even with a
        // live output.
        let shell_name = s.advertise("zaura_shell", zaura_shell::VERSION).await;
        let host = s.host_received();
        let [(sender, AnyRequest::WlRegistry(wl_registry::Request::Bind { name, id }))] = &host[..]
        else {
            panic!("the proxy did not bind zaura_shell alone: {host:?}");
        };
        assert_eq!((*sender, *name), (registry.host, shell_name));
        assert_eq!((id.0.as_str(), id.1), ("zaura_shell", zaura_shell::VERSION));
        let shell = id.2;
        // It stands behind the guest's gtk_shell1.
        assert_eq!(
            s.guest_received(),
            [(
                registry.id,
                wl_registry::Event::Global {
                    name: shell_name,
                    interface: "gtk_shell1".to_string(),
                    version: 1
                }
                .into()
            )]
        );
        let (_, keyboard) = s.keyboard().await;
        let wm_base = s.bind_paired("xdg_wm_base", xdg_wm_base::VERSION).await;
        assert_eq!(s.host_received(), []);

        let surface = s.surface().await;
        let xdg_surface = s.new_id();
        let host_xdg_surface = s
            .create(xdg_wm_base::req::get_xdg_surface(
                wm_base.id,
                xdg_surface,
                surface.id,
            ))
            .await;
        let toplevel = s.new_id();
        let host_toplevel = s
            .create(xdg_surface::req::get_toplevel(xdg_surface, toplevel))
            .await;
        assert_eq!(s.host_received(), [], "no Aura toplevel");
        assert_eq!(s.guest_received(), []);

        s.guest([xdg_toplevel::req::set_app_id(toplevel, "org.example.App").unwrap()])
            .await;
        let native = "org.chromium.guest_os.termina.wayland.org.example.App".to_string();
        let host = s.host_received();
        let aura_surface = match &host[..] {
            [_, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), _] => {
                *id
            }
            _ => panic!("no Aura surface: {host:?}"),
        };
        assert_eq!(
            host,
            [
                (
                    host_toplevel,
                    xdg_toplevel::Request::SetAppId {
                        app_id: native.clone()
                    }
                    .into()
                ),
                (
                    shell,
                    zaura_shell::Request::GetAuraSurface {
                        id: aura_surface,
                        surface: surface.host
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

        s.host([
            xdg_toplevel::evt::configure(host_toplevel, 800, 600, &[]).unwrap(),
            xdg_surface::evt::configure(host_xdg_surface, 7),
        ])
        .await;
        assert_eq!(
            s.guest_received(),
            [
                (
                    toplevel,
                    xdg_toplevel::Event::Configure {
                        width: 800,
                        height: 600,
                        states: Vec::new()
                    }
                    .into()
                ),
                (
                    xdg_surface,
                    xdg_surface::Event::Configure { serial: 7 }.into()
                ),
            ]
        );

        // The chord reaches the guest.
        let alt = 1 << 3;
        let q = 16;
        s.host([
            wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap(),
            wl_keyboard::evt::modifiers(keyboard.host, 2, alt, 0, 0, 0),
            wl_keyboard::evt::key(keyboard.host, 3, 3, q, wl_keyboard::KEY_STATE_PRESSED),
            wl_keyboard::evt::key(keyboard.host, 4, 4, q, wl_keyboard::KEY_STATE_RELEASED),
        ])
        .await;
        let key = |serial, state| {
            let key = wl_keyboard::Event::Key {
                serial,
                time: serial,
                key: q,
                state,
            };
            (keyboard.id, key.into())
        };
        let modifiers = |serial, mods_depressed| {
            let modifiers = wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed,
                mods_latched: 0,
                mods_locked: 0,
                group: 0,
            };
            (keyboard.id, modifiers.into())
        };
        let enter = wl_keyboard::Event::Enter {
            serial: 1,
            surface: surface.id,
            keys: Vec::new(),
        };
        assert_eq!(
            s.guest_received(),
            [
                (keyboard.id, enter.into()),
                modifiers(1, 0),
                modifiers(2, alt),
                key(3, wl_keyboard::KEY_STATE_PRESSED),
                key(4, wl_keyboard::KEY_STATE_RELEASED),
            ]
        );
        assert_eq!(s.host_received(), []);

        s.guest([
            xdg_toplevel::req::destroy(toplevel),
            xdg_surface::req::destroy(xdg_surface),
            wl_surface::req::destroy(surface.id),
        ])
        .await;
        assert_eq!(
            s.host_received(),
            [
                (host_toplevel, xdg_toplevel::Request::Destroy {}.into()),
                (host_xdg_surface, xdg_surface::Request::Destroy {}.into()),
                (aura_surface, zaura_surface::Request::Release {}.into()),
                (surface.host, wl_surface::Request::Destroy {}.into()),
            ]
        );
        assert_eq!(s.guest_received(), []);
    }
}
