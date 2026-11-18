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

//! Window placement: the one boundary between the core protocol handlers
//! and the placement backends.
//!
//! The keyboard handler decides that a chord is a placement shortcut; this
//! module moves the focused window. Resolving the target is shared by every
//! backend: the toplevel the keyboard focuses, the output it entered last,
//! and the bounds the bound rectangle maps to in that output's work area.
//! The connection's backend ([`crate::state::WindowPlacementState`]) then
//! moves the window its own way. Each backend owns its per-connection
//! state, its host objects and its requests in a submodule of its own:
//!
//! - [`arc`]: `zaura_toplevel.set_window_bounds` on a surface that carries
//!   an ARC policy identity.
//!
//! Protocol handlers translate wire events and call in here, one entry
//! point per event. Each entry point dispatches to the active backend and
//! does nothing while placement is off, the default:
//!
//! | Caller | Entry point |
//! | :--- | :--- |
//! | consumed placement chord (`wl_keyboard.key`) | [`place_focused`] |
//! | `zaura_shell` bound (`wl_registry.global`) | [`shell_bound`] |
//! | guest `wl_output` bound | [`output_bound`] |
//! | `xdg_surface.get_toplevel`, after dispatch | [`attach_toplevel`] |
//! | XDG role teardown | [`release_toplevel`] |
//! | `wl_surface` end | [`forget_surface`] |
//! | XDG and GTK application IDs | [`set_aura_application_id`] |
//! | `wl_callback.done` of a placement barrier | [`complete_barrier`] |
//! | `xdg_surface.configure` | [`hold_surface_configure`] |
//! | `xdg_toplevel.configure` | [`hold_toplevel_configure`] |
//! | `zaura_toplevel.configure` | [`translate_aura_configure`] |
//!
//! The host Aura surface is shared ([`crate::state::AuraSurfaces`]), and
//! `crate::handler::aura_objects` creates and releases every Aura object.

mod arc;
mod remote_shell;

#[cfg(test)]
pub(crate) use self::arc::register_window_bounds_barrier_for_test;
pub(crate) use self::remote_shell::RemoteShellHandler;

use crate::handler::aura_objects::{self, ensure_host_zaura_surface};
use crate::protocols::aura_shell::zaura_output::EVT_INSETS_SINCE;
use crate::protocols::aura_shell::zaura_surface;
use crate::state::{Context, HostId, OutputState, WindowPlacementState};
use crate::window_layout::{Rect, WindowLayoutAction};
use crate::wire::queue_or_drop;
use std::sync::atomic::{AtomicBool, Ordering};

/// The `zaura_shell` version placement needs.
///
/// Every backend measures the work area with `zaura_output.insets`, without
/// which the work area would include the shelf. Every Aura child takes its
/// shell's version, so a backend that sends Aura requests asserts that they
/// are no newer than this.
const PLACEMENT_SHELL_SINCE: u32 = EVT_INSETS_SINCE;

/// Whether [`warn_if_shell_cannot_place`] has warned. Every client
/// connection binds the same host shell, so one warning per process says
/// it all.
static OLD_SHELL_WARNED: AtomicBool = AtomicBool::new(false);

/// Warn, once per process, that placement is disabled because the host
/// `zaura_shell` is older than [`PLACEMENT_SHELL_SINCE`].
///
/// # Returns
///
/// Whether this call logged the warning.
fn warn_if_shell_cannot_place(shell_version: u32) -> bool {
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

/// Set up what placement needs from a newly bound `zaura_shell` of
/// `shell_version`.
///
/// A guest may bind an output, or create a toplevel, before the host
/// registry delivers `zaura_shell`, so this retries what their own paths
/// could not do yet: every live output's Aura output, and the active
/// backend's per-toplevel setup.
pub(crate) fn shell_bound(ctx: &mut Context, shell_version: u32) {
    if !ctx.window_placement.is_active() {
        return;
    }
    for output_host_id in ctx.outputs.live_outputs() {
        aura_objects::ensure_zaura_output(ctx, output_host_id);
    }
    match ctx.window_placement {
        WindowPlacementState::Off => {}
        WindowPlacementState::Arc(_) => arc::attach_recorded_toplevels(ctx),
    }
    warn_if_shell_cannot_place(shell_version);
}

/// Create the host-only Aura output of a newly bound guest `wl_output`,
/// whose scale factors and insets the work area needs. A missing shell is
/// retried by [`shell_bound`].
pub(crate) fn output_bound(ctx: &mut Context, output_host_id: u32) {
    if ctx.window_placement.is_active() {
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
    Some(FocusedToplevel {
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
    })
}

/// Resolve the screen bounds for a placement action on an output.
///
/// The action's normalized rectangle is mapped into the output work area
/// by [`crate::window_layout::NormalizedRect::to_work_area_bounds`], which
/// owns the rounding rule. Returns `None` when the output has no usable
/// work area or the rectangle cannot be represented in it.
fn bounds_for_action(action: WindowLayoutAction, output: OutputState) -> Option<Rect> {
    action.rect().to_work_area_bounds(output.work_area()?)
}

/// What one placement moves, and where, resolved the same way for every
/// backend.
#[derive(Clone, Copy, Debug)]
struct PlacementTarget {
    /// The placement, for the log.
    action: WindowLayoutAction,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
    /// The host `wl_output` the window is placed on.
    output_host_id: u32,
    /// The window's new bounds in screen coordinates, inside `output`'s
    /// work area.
    bounds: Rect,
}

/// Resolve the toplevel that `host_keyboard_id` focuses, the output it
/// entered last, and the bounds `action` maps to in that output's work
/// area.
fn resolve_target(
    ctx: &Context,
    host_keyboard_id: HostId,
    action: WindowLayoutAction,
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
    let bounds = bounds_for_action(action, output).ok_or(PlacementSkip::RectNotRepresentable)?;
    Ok(PlacementTarget {
        action,
        xdg_toplevel_guest_id,
        wl_surface_guest_id,
        output_host_id,
        bounds,
    })
}

/// Place the focused toplevel for one consumed placement chord.
///
/// Resolves the toplevel that `host_keyboard_id` focuses, the output it
/// has entered, and the bounds `action` maps to in that output's work area
/// (see [`resolve_target`]), and has the connection's backend move it.
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
        ctx.window_placement.is_active(),
        "classify_key yields placement actions only while a placement backend is active"
    );
    let target = resolve_target(ctx, host_keyboard_id, action)?;
    match ctx.window_placement {
        // Bindings exist only while a backend is active.
        WindowPlacementState::Off => Ok(()),
        WindowPlacementState::Arc(_) => arc::apply_bounds(ctx, target),
    }
}

// ── Toplevel and surface lifecycle ────────────────────────────────────────

/// Prepare a newly created XDG toplevel for placement.
///
/// Called once the generated dispatcher has accepted and mapped
/// `xdg_surface.get_toplevel`.
pub(crate) fn attach_toplevel(
    ctx: &mut Context,
    xdg_toplevel_guest_id: u32,
    wl_surface_guest_id: u32,
) {
    match ctx.window_placement {
        WindowPlacementState::Off => {}
        WindowPlacementState::Arc(_) => {
            arc::attach_toplevel(ctx, xdg_toplevel_guest_id, wl_surface_guest_id);
        }
    }
}

/// Release what placement holds for a guest XDG toplevel whose role ended.
///
/// Called whenever the XDG role ends (see `XdgRoles`' remove methods),
/// before the role's own destructor is queued.
pub(crate) fn release_toplevel(ctx: &mut Context, xdg_toplevel_guest_id: u32) {
    match ctx.window_placement {
        WindowPlacementState::Off => {}
        WindowPlacementState::Arc(_) => arc::release_toplevel(ctx, xdg_toplevel_guest_id),
    }
}

/// Forget what placement holds for a guest `wl_surface` that ended.
pub(crate) fn forget_surface(ctx: &mut Context, wl_surface_guest_id: u32) {
    if let Some(state) = ctx.window_placement.arc_mut() {
        state.forget_surface(wl_surface_guest_id);
    }
}

// ── Application IDs ───────────────────────────────────────────────────────

/// Send the application ID the guest set (through XDG or GTK) to the
/// surface's host Aura surface, creating that surface if needed. Nothing is
/// sent while no `zaura_shell` is bound.
///
/// The ARC backend replaces the ID with the surface's ARC identity;
/// otherwise the guest's ID is sent in the Guest OS namespace. The Aura
/// surface needs no version check: the registry binds no `zaura_shell`
/// older than `set_application_id`.
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
    match ctx.window_placement {
        WindowPlacementState::Arc(_) => arc::replace_application_id(
            ctx,
            wl_surface_guest_id,
            zaura_surface_host_id,
            native_application_id,
        ),
        WindowPlacementState::Off => {
            if queue_or_drop(
                &mut ctx.client_to_host_queue,
                zaura_surface::req::set_application_id(
                    zaura_surface_host_id,
                    Some(native_application_id),
                ),
                format_args!("the application ID of zaura_surface {zaura_surface_host_id}"),
            ) {
                log::debug!(
                    "Set application ID to {} on zaura_surface (host_id={})",
                    native_application_id,
                    zaura_surface_host_id
                );
            }
        }
    }
}

// ── Configures ────────────────────────────────────────────────────────────

/// Handle `wl_callback.done` for a bounds barrier
/// ([`crate::state::InternalCallback::WindowBoundsBarrier`]), which the
/// callback handler has already retired.
pub(crate) fn complete_barrier(
    ctx: &mut Context,
    callback_host_id: u32,
    zaura_toplevel_host_id: u32,
) {
    match ctx.window_placement {
        WindowPlacementState::Off => {}
        WindowPlacementState::Arc(_) => {
            arc::complete_barrier(ctx, callback_host_id, zaura_toplevel_host_id);
        }
    }
}

/// Hold a host `xdg_surface.configure` while the backend orders it.
///
/// # Returns
///
/// `true` when the configure is held and must not be forwarded now;
/// `false` when the caller forwards the event.
pub(crate) fn hold_surface_configure(
    ctx: &mut Context,
    guest_xdg_surface_id: u32,
    serial: u32,
) -> bool {
    match ctx.window_placement {
        WindowPlacementState::Off => false,
        WindowPlacementState::Arc(_) => {
            arc::hold_surface_configure(ctx, guest_xdg_surface_id, serial)
        }
    }
}

/// Hold a host `xdg_toplevel.configure` while the backend orders it.
///
/// # Returns
///
/// `true` when the configure is held and must not be forwarded now;
/// `false` when the caller forwards the event.
pub(crate) fn hold_toplevel_configure(
    ctx: &mut Context,
    guest_xdg_toplevel_id: u32,
    width: i32,
    height: i32,
    states: &[u8],
) -> bool {
    match ctx.window_placement {
        WindowPlacementState::Off => false,
        WindowPlacementState::Arc(_) => {
            arc::hold_toplevel_configure(ctx, guest_xdg_toplevel_id, width, height, states)
        }
    }
}

/// Deliver a host `zaura_toplevel.configure` to the guest. Only the ARC
/// backend creates Aura toplevels; the event is dropped otherwise.
pub(crate) fn translate_aura_configure(
    ctx: &mut Context,
    zaura_toplevel_host_id: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    states: &[u8],
) {
    match ctx.window_placement {
        WindowPlacementState::Off => {}
        WindowPlacementState::Arc(_) => {
            arc::translate_aura_configure(ctx, zaura_toplevel_host_id, x, y, width, height, states)
        }
    }
}

/// Fixtures shared by the boundary's and the backends' tests.
#[cfg(test)]
mod fixture {
    use super::*;
    use crate::handler::compositor::CompositorHandler;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_toplevel;
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

    /// A guest object and the host object it is paired with.
    #[derive(Clone, Copy)]
    pub(super) struct Paired {
        pub(super) guest: u32,
        pub(super) host: u32,
    }

    pub(super) const SURFACE: Paired = Paired {
        guest: 100,
        host: 200,
    };
    pub(super) const XDG_SURFACE: Paired = Paired {
        guest: 110,
        host: 210,
    };
    pub(super) const TOPLEVEL: Paired = Paired {
        guest: 120,
        host: 220,
    };
    /// The host `zaura_shell`, at the vendored version.
    pub(super) const SHELL: u32 = 300;
    /// Host keyboard 5 (guest 10, on seat 11).
    pub(super) const KEYBOARD: Paired = Paired { guest: 10, host: 5 };
    /// A 3840x2160 output at 1x.
    pub(super) const OUTPUT: u32 = 25;

    /// Give `ctx` wl_surface [`SURFACE`] with the toplevel role:
    /// [`XDG_SURFACE`] and its [`TOPLEVEL`].
    pub(super) fn add_toplevel(ctx: &mut Context) {
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
    pub(super) fn toplevel_ctx_with(placement: PlacementConfig) -> Context {
        let mut ctx = Context::with_placement_for_test(placement);
        add_toplevel(&mut ctx);
        ctx.bind_aura_shell_for_test(SHELL);
        ctx
    }

    /// [`toplevel_ctx_with`] the ARC backend, without bindings.
    pub(super) fn toplevel_ctx() -> Context {
        toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""))
    }

    /// Focus [`KEYBOARD`] on [`SURFACE`], which is all focus resolution
    /// reads.
    pub(super) fn focus(ctx: &mut Context) {
        let seat = 11;
        ctx.shadow_table
            .insert_paired_for_test(KEYBOARD.guest, KEYBOARD.host, "wl_keyboard");
        ctx.keyboard_to_seat.insert(KEYBOARD.guest, seat);
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

    /// [`toplevel_ctx_with`] the ARC bounds policy, focused through
    /// [`KEYBOARD`], on [`OUTPUT`].
    pub(super) fn focused_ctx() -> Context {
        let mut ctx = toplevel_ctx_with(PlacementConfig::arc_bounds_for_test(""));
        focus(&mut ctx);
        ctx.outputs.bind_for_test(OUTPUT, output(0, (3840, 2160)));
        ctx
    }

    /// Place the focused toplevel with `action`; returns what it sends the
    /// host.
    pub(super) fn place(ctx: &mut Context, action: WindowLayoutAction) -> Vec<(u32, AnyRequest)> {
        ctx.client_to_host_queue.clear();
        assert_eq!(place_focused(ctx, HostId(KEYBOARD.host), action), Ok(()));
        host_requests(ctx)
    }

    pub(super) fn window_bounds(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        output: u32,
    ) -> AnyRequest {
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
    pub(super) fn guest_events(ctx: &Context) -> Vec<(u32, AnyEvent)> {
        let table = &ctx.shadow_table;
        decode_queue(
            &ctx.host_to_client_queue,
            |id| table.get_interface(id),
            AnyEvent::from_wire,
        )
    }

    /// Deliver the host `wl_callback.done` of `callback_host_id`.
    pub(super) fn done(ctx: &mut Context, callback_host_id: u32) {
        use crate::protocols::wayland::wl_callback::WlCallbackHandler;

        ctx.last_sender_id = callback_host_id;
        assert_eq!(
            crate::handler::callback::CallbackHandler.on_done(ctx, 0),
            Action::Drop
        );
    }

    /// The host's `zaura_toplevel.configure` of `aura_toplevel`.
    pub(super) fn aura_configure(
        ctx: &mut Context,
        aura_toplevel: u32,
        size: (i32, i32),
        states: &[u8],
    ) {
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

    /// Give [`TOPLEVEL`] the v38 Aura toplevel `aura_toplevel`.
    pub(super) fn remember_aura_toplevel(ctx: &mut Context, aura_toplevel: u32) {
        ctx.window_placement
            .arc_mut()
            .expect("the ARC backend")
            .remember_aura_toplevel(TOPLEVEL.guest, aura_toplevel);
        ctx.shadow_table
            .insert_host_only_for_test(aura_toplevel, "zaura_toplevel", 38);
    }

    /// A whole proxy session for placement tests, with the objects every
    /// backend needs: the proxy's own `zaura_shell`, a 1920x1080 output at
    /// 1x whose shelf takes 48 pixels, an `xdg_wm_base`, a keyboard and a
    /// surface.
    pub(super) struct Desk {
        pub(super) s: Session,
        /// The host `zaura_shell` the proxy bound.
        pub(super) shell: u32,
        pub(super) output: Object,
        pub(super) wm_base: Object,
        pub(super) keyboard: Object,
        pub(super) surface: Object,
    }

    /// The work area of [`Desk`]'s output: the shelf takes its bottom.
    pub(super) const DESK_WORK_AREA: (i32, i32) = (1920, 1080 - 48);

    impl Desk {
        /// A session under `placement`, which must have a backend: the
        /// output gets its Aura output, whose metadata the host sends.
        /// What either peer gets for this is checked and not kept.
        pub(super) async fn new(placement: PlacementConfig) -> Self {
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
            s.host([
                wl_output::evt::geometry(output.host, 0, 0, 0, 0, 0, "", "", 0).unwrap(),
                wl_output::evt::mode(output.host, wl_output::MODE_CURRENT, 1920, 1080, 60),
                zaura_output::evt::scale(aura_output, zaura_output::SCALE_PROPERTY_CURRENT, 1000),
                zaura_output::evt::device_scale_factor(aura_output, 1000),
                zaura_output::evt::insets(aura_output, 0, 0, 1080 - DESK_WORK_AREA.1, 0),
                wl_output::evt::done(output.host),
            ])
            .await;
            assert!(
                s.guest_received()
                    .iter()
                    .all(|(sender, _)| *sender == output.id),
                "the guest gets only its output's events"
            );
            assert_eq!(s.host_received(), []);

            let wm_base = s.bind_paired("xdg_wm_base", xdg_wm_base::VERSION).await;
            let (_, keyboard) = s.keyboard().await;
            let surface = s.surface().await;
            Self {
                s,
                shell,
                output,
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
    use crate::arc_task_ids::ArcIdAllocator;
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::zaura_shell;
    use crate::protocols::aura_shell::zaura_toplevel;
    use crate::protocols::wayland::wl_surface::{self};
    use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel};
    use crate::protocols::{AnyEvent, AnyRequest};

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
                .arc()
                .and_then(|arc| arc.aura_toplevel_for_xdg_toplevel(TOPLEVEL.guest))
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
    fn an_old_shell_is_reported_once_per_process() {
        OLD_SHELL_WARNED.store(false, Ordering::Relaxed);
        assert!(!warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE));
        assert!(warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE - 1));
        assert!(!warn_if_shell_cannot_place(PLACEMENT_SHELL_SINCE - 1));
    }

    /// With placement off, the default, the proxy sends either peer what it
    /// sent before placement had backends: it binds no remote shell and no
    /// Aura output or toplevel, the Aura surface gets the guest's ID in the
    /// Guest OS namespace, configures pass through, and a chord that a
    /// binding source names reaches the guest, since no source is read.
    #[tokio::test]
    async fn placement_off_traffic_is_unchanged() {
        use crate::protocols::wayland::{wl_keyboard, wl_output, wl_registry};
        use crate::protocols::xdg_shell::xdg_wm_base;
        use crate::proxy::harness::Session;

        let placement = PlacementConfig::resolve_for_test(
            &[("SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS", "<Alt>q=top-left")],
            None,
            ArcIdAllocator::block_for_test(),
        );
        let mut s = Session::with_context(Context::with_placement_for_test(placement));
        let registry = s.registry().await;

        // A remote shell is neither bound nor advertised.
        s.advertise("zcr_remote_shell_v2", 6).await;
        assert_eq!(s.host_received(), []);
        assert_eq!(s.guest_received(), []);
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
