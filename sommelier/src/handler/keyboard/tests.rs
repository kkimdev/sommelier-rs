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

use super::*;
use crate::handler::window_placement::PlacementSkip;
use crate::keymap::MAX_KEYMAP_SIZE;
use crate::keys::EVDEV_KEY_BACKSPACE;
use crate::placement_config::PlacementConfig;
use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::{
    HANDLED_STATE_HANDLED as HANDLED, HANDLED_STATE_NOT_HANDLED as NOT_HANDLED,
};
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::protocols::wayland::wl_keyboard::{KEYMAP_FORMAT_NO_KEYMAP, KEYMAP_FORMAT_XKB_V1};
use crate::protocols::wayland::wl_keyboard::{KEY_STATE_PRESSED, KEY_STATE_RELEASED};
use crate::protocols::wayland::{
    wl_callback, wl_display, wl_output, wl_registry, wl_seat, wl_surface,
};
use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel, xdg_wm_base};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::{Object, Session};
use crate::wire::WL_DISPLAY_ID;
use xkb::keysyms;
use xkbcommon::xkb;

/// A key's class, from where its press went, with a shortcut's binding
/// resolved to where it placed the focused window.
#[derive(Debug, PartialEq)]
enum Class {
    Guest,
    HostAccelerator,
    Placed([i32; 4]),
}

/// The class of a chord bound to the named preset `name`.
fn placed(name: &str) -> Class {
    Class::Placed(bounds(name))
}

/// Where the named preset places a window, `[x, y, width, height]`, in
/// the work area of [`Kb::placing`]'s output: 1920x1032, above a
/// 48-pixel shelf.
fn bounds(name: &str) -> [i32; 4] {
    match name {
        "top-left" => [0, 0, 960, 516],
        "top" => [0, 0, 1920, 516],
        "top-right" => [960, 0, 960, 516],
        "left" => [0, 0, 960, 1032],
        "fullscreen" => [0, 0, 1920, 1032],
        "right" => [960, 0, 960, 1032],
        "bottom-left" => [0, 516, 960, 516],
        "bottom" => [0, 516, 1920, 516],
        "bottom-right" => [960, 516, 960, 516],
        _ => panic!("no preset {name:?}"),
    }
}

// Evdev keycodes and modifier masks of the default keymap, which
// `keymap_loads_from_non_rewound_memfd` pins.
const KEY_Q: u32 = 16;
const KEY_W: u32 = 17;
const KEY_E: u32 = 18;
const KEY_T: u32 = 20;
const KEY_A: u32 = 30;
const KEY_S: u32 = 31;
const KEY_D: u32 = 32;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_Z: u32 = 44;
const KEY_X: u32 = 45;
const KEY_C: u32 = 46;
const KEY_B: u32 = 48;
const KEY_SPACE: u32 = 57;
const BACKSPACE: u32 = EVDEV_KEY_BACKSPACE;
const SHIFT: u32 = 1 << 0;
const CAPS_LOCK: u32 = 1 << 1;
const CONTROL: u32 = 1 << 2;
const ALT: u32 = 1 << 3;

const PRESSED: u32 = KEY_STATE_PRESSED;
const RELEASED: u32 = KEY_STATE_RELEASED;

// -----------------------------------------------------------------------
// Session fixture
// -----------------------------------------------------------------------

/// A guest keyboard in a [`Session`], and the host objects behind it.
#[derive(Clone, Copy, Debug)]
struct Keyboard {
    id: u32,
    host: u32,
    /// The host `zcr_extended_keyboard_v1` the proxy created for the
    /// keyboard at its first enter; 0 before.
    ext: u32,
}

/// `k`, which no enter has given an extended keyboard yet.
fn unentered(k: Object) -> Keyboard {
    Keyboard {
        id: k.id,
        host: k.host,
        ext: 0,
    }
}

/// A guest text input the host activated, and the host
/// `zwp_text_input_v1` and `zcr_extended_text_input_v1` behind it.
#[derive(Clone, Copy, Debug)]
struct Ime {
    id: u32,
    host: u32,
    ext: u32,
    /// The host `wl_seat` the host text input is activated for.
    host_seat: u32,
    /// The guest's commits, which number its `done` events.
    serial: u32,
}

/// Where a host key event went, from what each peer received.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// The guest got it, and the host's ack says the guest handled it.
    Guest,
    /// The guest did not, and the host's ack lets the host handle it.
    Host,
    /// The guest did not, and the host's ack says it was handled: the
    /// proxy consumed it, as a duplicate press or a shortcut.
    Consumed,
}

/// A whole proxy session whose host has the keyboard extension, with a
/// guest seat, keyboard `k` of it with the default keymap, and a
/// surface.
struct Kb {
    s: Session,
    /// The name of the extension's global.
    extension: u32,
    seat: Object,
    k: Keyboard,
    surface: Object,
    /// The serial of the host's latest `xdg_surface.configure`.
    configure_serial: u32,
}

impl Kb {
    /// `k`'s focus has not entered the surface.
    async fn unfocused(ctx: Context) -> Self {
        Self::start(Session::with_context(ctx)).await
    }

    /// [`Self::unfocused`], in session `s`.
    async fn start(mut s: Session) -> Self {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
        let version = zcr_keyboard_extension_v1::VERSION;
        let extension = s.advertise(zcr_keyboard_extension_v1::NAME, version).await;
        // The proxy binds the extension for itself.
        let bind = s.host_received();
        assert!(
            matches!(
                &bind[..],
                [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { .. }))]
            ),
            "{bind:?}"
        );
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let k = unentered(s.keyboard_of(seat.id).await);
        let surface = s.surface().await;
        Self {
            s,
            extension,
            seat,
            k,
            surface,
            configure_serial: 0,
        }
    }

    /// `k`'s focus is on the surface.
    async fn new() -> Self {
        Self::focused(Context::new_for_test(false, false, Vec::new())).await
    }

    /// [`Self::new`] with host accelerators `spec`.
    async fn with_accelerators(spec: &str) -> Self {
        let accelerators = crate::accelerator::parse_accelerators(spec).unwrap();
        Self::focused(Context::new_for_test(false, false, accelerators)).await
    }

    async fn focused(ctx: Context) -> Self {
        let mut kb = Self::unfocused(ctx).await;
        let surface = kb.surface;
        kb.k = kb.first_enter(kb.k, surface).await;
        kb
    }

    /// The host gives `k`, which holds no modifiers, its first focus,
    /// on `surface`; returns `k` with the extended keyboard the proxy
    /// creates for it.
    async fn first_enter(&mut self, k: Keyboard, surface: Object) -> Keyboard {
        let enter = wl_keyboard::evt::enter(k.host, 1, surface.host, &[]).unwrap();
        self.s.host([enter]).await;
        let k = Keyboard {
            ext: extended_keyboard_of(k, &self.s.host_received()),
            ..k
        };
        assert_eq!(
            self.s.guest_received(),
            [entered(k, 1, surface.id, &[]), modifiers(k, 1, 0)]
        );
        k
    }

    /// Another keyboard of the seat, which the host gives its first
    /// focus on the surface, which the seat's focus is on.
    async fn another_keyboard(&mut self) -> Keyboard {
        let k = unentered(self.s.keyboard_of(self.seat.id).await);
        let surface = self.surface;
        self.first_enter(k, surface).await
    }

    /// `k`'s host sends key event `serial` at time `serial`; returns
    /// where it went. `on_key` ignores the time, so the tests that need
    /// one pass it to a peek.
    async fn key_on(&mut self, k: Keyboard, serial: u32, key: u32, state: u32) -> Route {
        self.s
            .host([wl_keyboard::evt::key(k.host, serial, serial, key, state)])
            .await;
        let guest = self.s.guest_received();
        let host = self.s.host_received();
        let delivered = [key_event(k, serial, serial, key, state)];
        match (guest == delivered, guest.is_empty(), &host[..]) {
            (true, _, [ack]) if *ack == acked(k, serial, HANDLED) => Route::Guest,
            (_, true, [ack]) if *ack == acked(k, serial, NOT_HANDLED) => Route::Host,
            (_, true, [ack]) if *ack == acked(k, serial, HANDLED) => Route::Consumed,
            _ => panic!("key {serial} went astray: the guest got {guest:?}, the host {host:?}"),
        }
    }

    async fn key(&mut self, serial: u32, key: u32, state: u32) -> Route {
        self.key_on(self.k, serial, key, state).await
    }

    /// `k`'s extended keyboard peeks a key, which reaches no peer.
    async fn peek_on(&mut self, k: Keyboard, serial: u32, time: u32, key: u32, state: u32) {
        use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
        self.s
            .host([zcr_extended_keyboard_v1::evt::peek_key(
                k.ext, serial, time, key, state,
            )])
            .await;
        assert_eq!(self.s.guest_received(), []);
        assert_eq!(self.s.host_received(), []);
    }

    async fn peek(&mut self, serial: u32, time: u32, key: u32, state: u32) {
        self.peek_on(self.k, serial, time, key, state).await;
    }

    /// `k`'s host holds the `depressed` modifiers, which the guest gets.
    async fn modifiers_on(&mut self, k: Keyboard, depressed: u32) {
        self.s
            .host([wl_keyboard::evt::modifiers(k.host, 1, depressed, 0, 0, 0)])
            .await;
        assert_eq!(self.s.guest_received(), [modifiers(k, 1, depressed)]);
    }

    async fn modifiers(&mut self, depressed: u32) {
        self.modifiers_on(self.k, depressed).await;
    }

    /// `k`'s host sends the default keymap again, which the guest gets.
    async fn keymap(&mut self) {
        let k = Object {
            id: self.k.id,
            host: self.k.host,
        };
        self.s.keymap(k).await;
    }

    /// `k`'s host sends a keymap of `format` and `size` in a file
    /// holding `bytes`, which the guest gets.
    async fn keymap_of(&mut self, format: u32, bytes: &[u8], size: u32) {
        let k = Object {
            id: self.k.id,
            host: self.k.host,
        };
        self.s.keymap_of(k, format, bytes, size).await;
    }

    /// A guest text input of the seat, which the guest enables on the
    /// focused surface and the host activates.
    async fn text_input(&mut self) -> Ime {
        let t = self.s.text_input(self.seat.id).await;
        let surface = self.surface.id;
        let enter = zwp_text_input_v3::Event::Enter { surface };
        assert_eq!(self.s.guest_received(), [(t.id, enter.into())]);
        // The activation, which the text-input tests pin.
        self.s.enable(t, 1).await;
        Ime {
            id: t.id,
            host: t.host,
            ext: t.ext,
            host_seat: self.seat.host,
            serial: 1,
        }
    }

    /// The host input method shows preedit `text`, which the guest gets.
    async fn compose(&mut self, ime: Ime, text: &str) {
        let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, text, "").unwrap();
        self.s.host([preedit]).await;
        assert_eq!(self.s.guest_received(), [v3_preedit(ime, text), done(ime)]);
    }

    /// The host input method shows a preedit and empties it, as a held
    /// Backspace it takes deletes it: the proxy leases Backspace's
    /// repeat to `ime`.
    async fn lease(&mut self, ime: Ime) {
        self.compose(ime, "가").await;
        self.compose(ime, "").await;
    }

    /// Whether `k`'s held Backspace is leased to a text input. Reads
    /// state no peer can observe: the lease shows only in the delivery
    /// of Backspace's next `wl_keyboard` press, which the test does not
    /// send.
    fn leased(&mut self, k: Keyboard) -> bool {
        let keys = &self.s.ctx().keys;
        keys.backspace_lease(HostId::for_test(k.host)).is_some()
    }

    /// The host input method confirms its empty composition, and sends
    /// nothing else before the `done` of the sync the proxy sends with
    /// a recovery that waits, if it does: an autorepeat's confirmation
    /// (`crate::keys::recovery`).
    async fn confirm(&mut self, ime: Ime) {
        self.s
            .host([zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0)])
            .await;
        self.answer_recovery_sync().await;
    }

    /// The host answers the sync the proxy sent with a recovery that
    /// waits, if it sent one.
    async fn answer_recovery_sync(&mut self) {
        match self.s.host_received()[..] {
            [] => {}
            [(_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] => {
                self.s.host([wl_callback::evt::done(callback, 0)]).await;
            }
            ref other => panic!("the host got {other:?}, not a recovery's sync"),
        }
    }

    /// Exo detaches the input method on its own (H2), as when its
    /// process restarts: it confirms, then sends `leave`, and attaches
    /// again once the proxy activates the text input again. The guest
    /// gets nothing.
    async fn detach(&mut self, ime: Ime) {
        self.s
            .host([
                zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0),
                zwp_text_input_v1::evt::leave(ime.host),
            ])
            .await;
        for request in self.s.host_received() {
            if let (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) = request {
                self.s.host([wl_callback::evt::done(callback, 0)]).await;
            }
        }
        let surface = self.surface.host;
        self.s
            .host([zwp_text_input_v1::evt::enter(ime.host, surface)])
            .await;
        self.s.host_received();
        assert_eq!(self.s.guest_received(), []);
    }

    /// The guest destroys a new surface, whose destroy the host has not
    /// read: the host may still name it. Returns it.
    async fn dying_surface(&mut self) -> Object {
        let surface = self.s.surface().await;
        self.s.guest([wl_surface::req::destroy(surface.id)]).await;
        let destroy = wl_surface::Request::Destroy {};
        assert_eq!(self.s.host_received(), [(surface.host, destroy.into())]);
        surface
    }
}

/// A guest `xdg_toplevel` and the host objects behind it.
#[derive(Clone, Copy, Debug)]
struct Window {
    xdg_surface: Object,
    toplevel: Object,
    /// The host `zaura_toplevel` the proxy creates for it while placement
    /// is on.
    aura_toplevel: Option<u32>,
}

/// Window placement, through a whole session.
impl Kb {
    /// [`Self::new`] under `ctx`, whose host has the Aura shell, which
    /// the proxy binds, and an output, 1920x1080 at the origin, which it
    /// has measured ([`Self::measure`]). Returns the shell's global name
    /// too.
    async fn placing(ctx: Context, device_scale: bool) -> (Self, u32) {
        let mut s = Session::with_context(ctx);
        let registry = s.registry().await;
        // The shell, as `Session::aura_shell`, whose name a test removes.
        let shell = s.advertise(zaura_shell::NAME, zaura_shell::VERSION).await;
        let host = s.host_received();
        assert!(
            matches!(
                &host[..],
                [(sender, AnyRequest::WlRegistry(wl_registry::Request::Bind { name, .. }))]
                    if (*sender, *name) == (registry.host, shell)
            ),
            "the proxy did not bind zaura_shell: {host:?}"
        );
        let advertised = s.guest_received();
        assert!(
            matches!(
                &advertised[..],
                [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { interface, .. }))]
                    if interface == "gtk_shell1"
            ),
            "{advertised:?}"
        );
        let mut kb = Self::start(s).await;
        let (output, aura_output) = kb.output().await;
        kb.measure(output, aura_output, 0, device_scale).await;
        let surface = kb.surface;
        kb.k = kb.first_enter(kb.k, surface).await;
        (kb, shell)
    }

    /// The host advertises a `wl_output`, which the guest binds; returns
    /// it and the host `zaura_output` the proxy gets for it while
    /// placement is on.
    async fn output(&mut self) -> (Object, Option<u32>) {
        let registry = self.s.registry().await;
        let name = self.s.advertise("wl_output", wl_output::VERSION).await;
        let advertised = self.s.guest_received();
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            &advertised[..]
        else {
            panic!("the output was not advertised: {advertised:?}");
        };
        let id = self.s.new_id();
        let bind = wl_registry::req::bind(registry.id, name, "wl_output", *version, id);
        self.s.guest([bind.unwrap()]).await;
        let host = self.s.host_received();
        let (bound, aura_output) = match &host[..] {
            [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: bound, .. }))] => {
                (bound.2, None)
            }
            [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: bound, .. })), (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id, .. }))] => {
                (bound.2, Some(*id))
            }
            _ => panic!("the host got {host:?}"),
        };
        (Object { id, host: bound }, aura_output)
    }

    /// The host sends `output`'s metadata: its 1920x1080 mode at
    /// `origin_x`, and, through its Aura output, if it has one, a scale
    /// of 1, insets for a 48-pixel shelf, and its device scale, 1, if
    /// `device_scale`. The guest gets the output's events.
    async fn measure(
        &mut self,
        output: Object,
        aura_output: Option<u32>,
        origin_x: i32,
        device_scale: bool,
    ) {
        let geometry = wl_output::evt::geometry(output.host, origin_x, 0, 0, 0, 0, "", "", 0);
        let mode = wl_output::evt::mode(output.host, wl_output::MODE_CURRENT, 1920, 1080, 60);
        let mut events = vec![geometry.unwrap(), mode];
        if let Some(aura) = aura_output {
            let current = zaura_output::SCALE_PROPERTY_CURRENT;
            events.push(zaura_output::evt::scale(aura, current, 1000));
            if device_scale {
                events.push(zaura_output::evt::device_scale_factor(aura, 1000));
            }
            events.push(zaura_output::evt::insets(aura, 0, 0, 48, 0));
        }
        events.push(wl_output::evt::done(output.host));
        self.s.host(events).await;
        let guest = self.s.guest_received();
        assert!(
            !guest.is_empty() && guest.iter().all(|(sender, _)| *sender == output.id),
            "{guest:?}"
        );
        assert_eq!(self.s.host_received(), []);
    }

    /// The guest makes the surface an `xdg_toplevel`, from an
    /// `xdg_wm_base` it binds.
    async fn window(&mut self) -> Window {
        let wm_base = self.s.bind("xdg_wm_base", xdg_wm_base::VERSION).await;
        let id = self.s.new_id();
        let get_xdg_surface = xdg_wm_base::req::get_xdg_surface(wm_base, id, self.surface.id);
        let host = self.s.create(get_xdg_surface).await;
        let xdg_surface = Object { id, host };
        let id = self.s.new_id();
        self.s
            .guest([xdg_surface::req::get_toplevel(xdg_surface.id, id)])
            .await;
        let host = self.s.host_received();
        use xdg_surface::Request::GetToplevel;
        let (toplevel, aura_toplevel) = match host[..] {
            [(sender, AnyRequest::XdgSurface(GetToplevel { id: host }))]
                if sender == xdg_surface.host =>
            {
                (host, None)
            }
            [(sender, AnyRequest::XdgSurface(GetToplevel { id: host })), (
                _,
                AnyRequest::ZauraShell(zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                    id: aura,
                    toplevel,
                }),
            ), (
                supports,
                AnyRequest::ZauraToplevel(zaura_toplevel::Request::SetSupportsScreenCoordinates {}),
            )] if sender == xdg_surface.host && toplevel == host && supports == aura => {
                (host, Some(aura))
            }
            _ => panic!("the host got {host:?}"),
        };
        Window {
            xdg_surface,
            toplevel: Object { id, host: toplevel },
            aura_toplevel,
        }
    }

    /// The host's Aura toplevel configures `w` at `origin`, 640x480 and
    /// activated, with a new `xdg_surface.configure`, which the guest
    /// gets as an XDG configure.
    async fn configured(&mut self, w: Window, origin: (i32, i32)) {
        self.configure_serial += 1;
        let serial = self.configure_serial;
        let aura = w.aura_toplevel.expect("an Aura toplevel");
        let states = activated();
        let configure = zaura_toplevel::evt::configure(aura, origin.0, origin.1, 640, 480, &states);
        self.s
            .host([
                configure.unwrap(),
                xdg_surface::evt::configure(w.xdg_surface.host, serial),
            ])
            .await;
        let configure = xdg_toplevel::Event::Configure {
            width: 640,
            height: 480,
            states,
        };
        assert_eq!(
            self.s.guest_received(),
            [
                (w.toplevel.id, configure.into()),
                (
                    w.xdg_surface.id,
                    xdg_surface::Event::Configure { serial }.into()
                ),
            ]
        );
    }

    /// The host answers placement barrier `callback`, and deletes it.
    async fn barrier_done(&mut self, callback: u32) {
        self.s
            .host([
                wl_callback::evt::done(callback, 0),
                wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
            ])
            .await;
    }

    /// The host sends the press of Q, which with Alt is the `<Alt>q`
    /// chord, as key event `serial`, on the focused window `w`: the guest
    /// gets nothing, and the host the placement's first step, which
    /// restores `w` and ends with its barrier, and then the press's
    /// acknowledgement. Returns the step and the barrier's callback.
    async fn place(&mut self, w: Window, serial: u32) -> (Vec<(u32, AnyRequest)>, u32) {
        self.s
            .host([wl_keyboard::evt::key(
                self.k.host,
                serial,
                serial,
                KEY_Q,
                PRESSED,
            )])
            .await;
        assert_eq!(self.s.guest_received(), [], "the chord is consumed");
        let mut host = self.s.host_received();
        assert_eq!(host.pop(), Some(acked(self.k, serial, HANDLED)), "{host:?}");
        let restore = match &host[..] {
            [(_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { surface, .. })), restore @ ..]
                if *surface == self.surface.host =>
            {
                restore
            }
            restore => restore,
        };
        let [(fullscreen, AnyRequest::XdgToplevel(xdg_toplevel::Request::UnsetFullscreen {})), (maximized, AnyRequest::XdgToplevel(xdg_toplevel::Request::UnsetMaximized {})), (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            restore
        else {
            panic!("no placement: {host:?}");
        };
        assert_eq!(
            (*fullscreen, *maximized),
            (w.toplevel.host, w.toplevel.host)
        );
        let callback = *callback;
        (host, callback)
    }

    /// Exo answers the placement step whose barrier is `callback` with
    /// `w` at `from`, and the proxy moves it; Exo then reports it where
    /// moved, which ends the placement. Returns where `w` went, `[x, y,
    /// width, height]`, from the move and the size the guest's configure
    /// gives it.
    async fn finish(&mut self, w: Window, callback: u32, from: (i32, i32)) -> [i32; 4] {
        self.configured(w, from).await;
        self.barrier_done(callback).await;
        assert_eq!(self.s.guest_received(), []);
        let host = self.s.host_received();
        let [(
            aura_surface,
            AnyRequest::ZauraSurface(zaura_surface::Request::SetParent { parent, x, y }),
        ), (unset, AnyRequest::XdgToplevel(xdg_toplevel::Request::UnsetFullscreen {})), (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
            host[..]
        else {
            panic!("no move: {host:?}");
        };
        assert_eq!((parent, unset), (aura_surface, w.toplevel.host));
        let at = (from.0 + x, from.1 + y);
        self.configured(w, at).await;
        self.barrier_done(callback).await;
        assert_eq!(self.s.host_received(), []);
        let guest = self.s.guest_received();
        let [(toplevel, AnyEvent::XdgToplevel(xdg_toplevel::Event::Configure { width, height, .. })), (xdg, AnyEvent::XdgSurface(xdg_surface::Event::Configure { serial }))] =
            &guest[..]
        else {
            panic!("the window was not resized: {guest:?}");
        };
        assert_eq!(
            (*toplevel, *xdg, *serial),
            (w.toplevel.id, w.xdg_surface.id, self.configure_serial)
        );
        [at.0, at.1, *width, *height]
    }

    /// The class of `key`, pressed as key event `serial` on the focused
    /// window `w`, from where it goes: a placement it starts is finished
    /// ([`Self::finish`]).
    async fn class(&mut self, w: Window, serial: u32, key: u32) -> Class {
        self.s
            .host([wl_keyboard::evt::key(
                self.k.host,
                serial,
                serial,
                key,
                PRESSED,
            )])
            .await;
        let guest = self.s.guest_received();
        let mut host = self.s.host_received();
        let ack = host.pop();
        let delivered = [key_event(self.k, serial, serial, key, PRESSED)];
        match ack {
            Some(ack) if ack == acked(self.k, serial, HANDLED) && guest == delivered => {
                assert_eq!(host, []);
                Class::Guest
            }
            Some(ack) if ack == acked(self.k, serial, NOT_HANDLED) && guest.is_empty() => {
                assert_eq!(host, []);
                Class::HostAccelerator
            }
            Some(ack) if ack == acked(self.k, serial, HANDLED) && guest.is_empty() => {
                let Some(&(_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))) =
                    host.last()
                else {
                    panic!("a consumed chord placed nothing: {host:?}");
                };
                Class::Placed(self.finish(w, callback, (300, 200)).await)
            }
            _ => panic!("key {serial} went astray: the guest got {guest:?}, the host {host:?}"),
        }
    }
}

/// The `xdg_toplevel.configure` states of an activated window.
fn activated() -> Vec<u8> {
    xdg_toplevel::STATE_ACTIVATED.to_ne_bytes().to_vec()
}

/// The host `zcr_extended_keyboard_v1` the proxy creates for `k`, from
/// the requests the host got.
fn extended_keyboard_of(k: Keyboard, host: &[(u32, AnyRequest)]) -> u32 {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1::Request;
    let [(_, AnyRequest::ZcrKeyboardExtensionV1(Request::GetExtendedKeyboard { id, keyboard }))] =
        host[..]
    else {
        panic!("the host got no extended keyboard: {host:?}");
    };
    assert_eq!(keyboard, k.host);
    id
}

fn entered(k: Keyboard, serial: u32, surface: u32, keys: &[u32]) -> (u32, AnyEvent) {
    let keys = keys.iter().flat_map(|key| key.to_ne_bytes()).collect();
    let enter = wl_keyboard::Event::Enter {
        serial,
        surface,
        keys,
    };
    (k.id, enter.into())
}

fn left(k: Keyboard, serial: u32, surface: u32) -> (u32, AnyEvent) {
    (k.id, wl_keyboard::Event::Leave { serial, surface }.into())
}

fn modifiers(k: Keyboard, serial: u32, depressed: u32) -> (u32, AnyEvent) {
    let modifiers = wl_keyboard::Event::Modifiers {
        serial,
        mods_depressed: depressed,
        mods_latched: 0,
        mods_locked: 0,
        group: 0,
    };
    (k.id, modifiers.into())
}

fn key_event(k: Keyboard, serial: u32, time: u32, key: u32, state: u32) -> (u32, AnyEvent) {
    let event = wl_keyboard::Event::Key {
        serial,
        time,
        key,
        state,
    };
    (k.id, event.into())
}

/// What the guest gets when IME recovery replays `key` on `k`, whose
/// latest peek had `serial` and `time`: a press and release, both with
/// that serial and time, and the `done` of `ime`'s transaction.
fn recovered(ime: Ime, k: Keyboard, serial: u32, time: u32, key: u32) -> [(u32, AnyEvent); 3] {
    [
        key_event(k, serial, time, key, PRESSED),
        key_event(k, serial, time, key, RELEASED),
        done(ime),
    ]
}

fn acked(k: Keyboard, serial: u32, handled: u32) -> (u32, AnyRequest) {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::Request;
    (k.ext, Request::AckKey { serial, handled }.into())
}

/// What the host gets when the guest releases `k`.
fn released(k: Keyboard) -> [(u32, AnyRequest); 2] {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    [
        (k.host, wl_keyboard::Request::Release {}.into()),
        (k.ext, zcr_extended_keyboard_v1::Request::Destroy {}.into()),
    ]
}

/// Whether `host` is what the host gets when the proxy deactivates the
/// host text input of `ime`: the deactivate, and the sync that drains
/// the events the host sent before it.
fn deactivated(ime: Ime, host: &[(u32, AnyRequest)]) -> bool {
    deactivation(ime, host).is_some()
}

/// The callback of the sync that drains `ime`'s deactivation, if `host`
/// is what the host gets for it ([`deactivated`]).
fn deactivation(ime: Ime, host: &[(u32, AnyRequest)]) -> Option<u32> {
    let deactivate = (
        ime.host,
        zwp_text_input_v1::Request::Deactivate {
            seat: ime.host_seat,
        }
        .into(),
    );
    match host {
        [first, (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))]
            if *first == deactivate =>
        {
            Some(*callback)
        }
        _ => None,
    }
}

/// The guest's preedit `text`, with the cursor at its end.
fn v3_preedit(ime: Ime, text: &str) -> (u32, AnyEvent) {
    let end = text.len() as i32;
    let preedit = zwp_text_input_v3::Event::PreeditString {
        text: Some(text.into()),
        cursor_begin: end,
        cursor_end: end,
    };
    (ime.id, preedit.into())
}

fn v3_commit(ime: Ime, text: &str) -> (u32, AnyEvent) {
    let commit = zwp_text_input_v3::Event::CommitString {
        text: Some(text.into()),
    };
    (ime.id, commit.into())
}

fn v3_enter(ime: Ime, surface: u32) -> (u32, AnyEvent) {
    (ime.id, zwp_text_input_v3::Event::Enter { surface }.into())
}

fn v3_leave(ime: Ime, surface: u32) -> (u32, AnyEvent) {
    (ime.id, zwp_text_input_v3::Event::Leave { surface }.into())
}

/// The text-input `done` that closes the guest's latest commit.
fn done(ime: Ime) -> (u32, AnyEvent) {
    let serial = ime.serial;
    (ime.id, zwp_text_input_v3::Event::Done { serial }.into())
}

// -----------------------------------------------------------------------
// Chords
// -----------------------------------------------------------------------

#[tokio::test]
async fn chord_classification_follows_the_configuration() {
    let none = crate::accelerator::HostAccelerators::default;
    let active = |bindings: &[(&str, &str)]| (none(), PlacementConfig::active_for_test(bindings));
    let nine = active(&[
        ("<Alt>q", "top-left"),
        ("<Alt>w", "top"),
        ("<Alt>e", "top-right"),
        ("<Alt>a", "left"),
        ("<Alt>s", "fullscreen"),
        ("<Alt>d", "right"),
        ("<Alt>z", "bottom-left"),
        ("<Alt>x", "bottom"),
        ("<Alt>c", "bottom-right"),
    ]);
    let alt_q = active(&[("<Alt>q", "top-left")]);
    let control_q = active(&[("<Control>q", "top-left")]);
    // Placement is off, so nothing is bound.
    let off = (none(), PlacementConfig::for_test());
    let host = crate::accelerator::HostAccelerators::parse("<Control>a").unwrap();
    let both = (
        host.clone(),
        PlacementConfig::loaded_for_test(&host, &[("<Control>q", "top-left")]),
    );
    let cases = [
        (&nine, ALT, KEY_Q, placed("top-left")),
        (&nine, ALT, KEY_W, placed("top")),
        (&nine, ALT, KEY_E, placed("top-right")),
        (&nine, ALT, KEY_A, placed("left")),
        (&nine, ALT, KEY_S, placed("fullscreen")),
        (&nine, ALT, KEY_D, placed("right")),
        (&nine, ALT, KEY_Z, placed("bottom-left")),
        (&nine, ALT, KEY_X, placed("bottom")),
        (&nine, ALT, KEY_C, placed("bottom-right")),
        // A binding needs exactly its modifiers.
        (&alt_q, 0, KEY_Q, Class::Guest),
        (&alt_q, ALT | SHIFT, KEY_Q, Class::Guest),
        (&control_q, CONTROL, KEY_Q, placed("top-left")),
        (&off, ALT, KEY_Q, Class::Guest),
        (&both, CONTROL, KEY_A, Class::HostAccelerator),
        (&both, CONTROL, KEY_Q, placed("top-left")),
        (&both, CONTROL, KEY_Z, Class::Guest),
    ];
    for ((host_accelerators, placement), modifiers, key, class) in cases {
        let settings = crate::settings::Settings {
            host_accelerators: host_accelerators.clone(),
            ..crate::settings::Settings::for_test()
        };
        let ctx = Context::for_test(settings, placement.clone());
        let (mut kb, _) = Kb::placing(ctx, true).await;
        let w = kb.window().await;
        kb.modifiers(modifiers).await;
        assert_eq!(
            kb.class(w, 2, key).await,
            class,
            "key {key} with modifiers {modifiers:#x}"
        );
    }
}

/// A keyboard with no keymap yet classifies no chord, and a key beyond
/// the keymap's keycode range (XKB's keycode is the evdev keycode plus 8)
/// forms none.
#[tokio::test]
async fn a_key_forms_no_chord_without_a_keymap_or_beyond_the_keycode_range() {
    let accelerators = crate::accelerator::parse_accelerators("a").unwrap();
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;
    let id = kb.s.new_id();
    let host =
        kb.s.create(wl_seat::req::get_keyboard(kb.seat.id, id))
            .await;
    kb.k = Keyboard { id, host, ext: 0 };
    let surface = kb.surface;
    kb.k = kb.first_enter(kb.k, surface).await;
    assert_eq!(kb.key(1, KEY_A, PRESSED).await, Route::Guest);
    assert_eq!(kb.key(2, KEY_A, RELEASED).await, Route::Guest);

    kb.keymap().await;
    assert_eq!(kb.key(3, u32::MAX, PRESSED).await, Route::Guest);
    assert_eq!(kb.key(4, KEY_A, PRESSED).await, Route::Host);
}

// -----------------------------------------------------------------------
// Placement shortcuts
// -----------------------------------------------------------------------

/// The `<Alt>q=top-left` placement, which is on.
fn top_left() -> Context {
    Context::with_placement_for_test(PlacementConfig::active_for_test(&[("<Alt>q", "top-left")]))
}

/// A placement configuration whose bindings come from a file in `dir`,
/// which reloads as the watcher would.
fn file_placement(
    dir: &crate::test_support::TempDir,
    document: &[u8],
) -> (crate::placement_config::BindingsFile, PlacementConfig) {
    let path = dir.write("window-placement.toml", document);
    crate::placement_config::BindingsFile::loaded_for_test(&path)
}

/// A matched chord belongs to Sommelier while the policy is active, so
/// it is consumed even when placement has to be skipped. The skip leaves
/// no Aura objects or barriers behind: the host gets only the keys'
/// acknowledgements.
#[tokio::test]
async fn placement_chord_is_consumed_while_aura_scale_metadata_is_incomplete() {
    // The device scale event has not arrived yet.
    let (mut kb, _) = Kb::placing(top_left(), false).await;
    kb.window().await;
    kb.modifiers(ALT).await;

    assert_eq!(
        kb.key(1, KEY_Q, PRESSED).await,
        Route::Consumed,
        "a skipped placement must not leak the chord to the guest"
    );
    assert_eq!(kb.key(2, KEY_Q, RELEASED).await, Route::Consumed);
}

/// The focused surface entered an output that cannot place it yet, so
/// placement is skipped, though another output could.
#[tokio::test]
async fn placement_is_skipped_rather_than_moved_to_another_output() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    kb.window().await;
    let (entered_output, aura_output) = kb.output().await;
    // The device scale event for this output has not arrived.
    kb.measure(entered_output, aura_output, 1920, false).await;
    let surface = kb.surface;
    kb.s.host([wl_surface::evt::enter(surface.host, entered_output.host)])
        .await;
    let enter = wl_surface::Event::Enter {
        output: entered_output.id,
    };
    assert_eq!(kb.s.guest_received(), [(surface.id, enter.into())]);
    kb.modifiers(ALT).await;

    assert_eq!(kb.key(2, KEY_Q, PRESSED).await, Route::Consumed);
}

/// The press and release of a matched chord stay with Sommelier
/// even when its placement is skipped, for every reason to skip it.
#[tokio::test]
async fn a_skipped_placement_still_consumes_its_chord() {
    let dir = crate::test_support::TempDir::new("keyboard-sliver");
    // 0.0001 of the 1920-pixel work area floors onto the left border.
    let sliver_binding =
        b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\nrect = [0.0001, 0.0, 0.9999, 1.0]\n";
    for skip in [
        PlacementSkip::NoFocusedToplevel,
        PlacementSkip::RectNotRepresentable,
        PlacementSkip::NoAuraToplevel,
    ] {
        let mut kb = match skip {
            // The focused surface is not an xdg_toplevel.
            PlacementSkip::NoFocusedToplevel => Kb::placing(top_left(), true).await.0,
            PlacementSkip::RectNotRepresentable => {
                let placement = file_placement(&dir, sliver_binding).1;
                let ctx = Context::with_placement_for_test(placement);
                let mut kb = Kb::placing(ctx, true).await.0;
                kb.window().await;
                kb
            }
            // The host removed its Aura shell before the window, which
            // has no Aura toplevel.
            _ => {
                let (mut kb, shell) = Kb::placing(top_left(), true).await;
                let registry = kb.s.registry().await;
                kb.s.host([wl_registry::evt::global_remove(registry.host, shell)])
                    .await;
                let removed = wl_registry::Event::GlobalRemove { name: shell };
                assert_eq!(kb.s.guest_received(), [(registry.id, removed.into())]);
                let release = kb.s.host_received();
                assert!(
                    matches!(
                        &release[..],
                        [(_, AnyRequest::ZauraShell(zaura_shell::Request::Release {}))]
                    ),
                    "{release:?}"
                );
                let w = kb.window().await;
                assert_eq!(w.aura_toplevel, None);
                kb
            }
        };
        kb.modifiers(ALT).await;
        assert_eq!(kb.key(2, KEY_Q, PRESSED).await, Route::Consumed, "{skip:?}");
        assert_eq!(
            kb.key(3, KEY_Q, RELEASED).await,
            Route::Consumed,
            "{skip:?}"
        );
    }
    assert_eq!(
        PlacementSkip::NoFocusedToplevel.log_level(),
        log::Level::Debug,
        "a transient skip stays at debug"
    );
    assert_eq!(
        PlacementSkip::RectNotRepresentable.log_level(),
        log::Level::Info,
        "a persistent misconfiguration is visible at info"
    );
}

/// An edit of the bindings file, which the watcher reloads as it
/// happens, takes effect on the next press; the key path never reads
/// the file. A key already captured as a shortcut stays captured until
/// its release.
#[tokio::test]
async fn a_bindings_file_edit_takes_effect_on_the_next_press() {
    let dir = crate::test_support::TempDir::new("keyboard-shortcut-reload");
    let (mut file, placement) = file_placement(&dir, b"version = 1\n");
    let (mut kb, _) = Kb::placing(Context::with_placement_for_test(placement), true).await;
    let w = kb.window().await;
    kb.modifiers(ALT).await;
    assert_eq!(
        kb.key(1, KEY_Q, PRESSED).await,
        Route::Guest,
        "the file binds nothing"
    );
    assert_eq!(kb.key(2, KEY_Q, RELEASED).await, Route::Guest);

    dir.write(
        "window-placement.toml",
        b"version = 1\n[[bindings]]\nchord = \"<Alt>q\"\nrect = [0.25, 0.0, 0.5, 1.0]\n",
    );
    assert_eq!(
        kb.key(3, KEY_Q, PRESSED).await,
        Route::Guest,
        "only a reload adopts an edit"
    );
    assert_eq!(kb.key(4, KEY_Q, RELEASED).await, Route::Guest);
    // What the watcher does as the file changes.
    file.reload();
    // The new chord is consumed by the compositor.
    let (_, callback) = kb.place(w, 5).await;
    assert_eq!(
        kb.finish(w, callback, (300, 200)).await,
        [480, 0, 960, 1032]
    );

    // Removing the binding while the key is held still suppresses the
    // matching release: ownership, not the binding, owns the release.
    dir.write("window-placement.toml", b"version = 1\n");
    file.reload();
    assert_eq!(kb.key(6, KEY_Q, RELEASED).await, Route::Consumed);
    assert_eq!(
        kb.key(7, KEY_Q, PRESSED).await,
        Route::Guest,
        "a removed binding is delivered to the guest"
    );
}

/// A placement chord's peek, which comes before its `wl_keyboard` press,
/// makes it no candidate of IME recovery: an empty confirmation then
/// replays nothing, and the press places the window.
#[tokio::test]
async fn peeked_placement_chord_is_not_an_ime_recovery_candidate() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    let ime = kb.text_input().await;
    kb.modifiers(ALT).await;

    kb.peek(2, 100, KEY_Q, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a placement chord must never become an IME recovery candidate"
    );

    // The chord stays Sommelier's, and the window is placed.
    let (_, callback) = kb.place(w, 2).await;
    kb.peek(3, 101, KEY_Q, RELEASED).await;
    assert_eq!(kb.key(3, KEY_Q, RELEASED).await, Route::Consumed);
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

/// The host IME consumed the chord, so no `wl_keyboard.key` arrives.
/// The chord is neither placed (placement runs only on
/// `wl_keyboard.key`) nor delivered to the guest.
#[tokio::test]
async fn placement_chord_consumed_by_the_host_ime_is_neither_placed_nor_delivered() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    kb.window().await;
    let ime = kb.text_input().await;
    kb.compose(ime, "한").await;
    kb.modifiers(ALT).await;

    // Neither peek reaches a peer.
    kb.peek(2, 100, KEY_Q, PRESSED).await;
    kb.peek(3, 101, KEY_Q, RELEASED).await;
}

#[tokio::test]
async fn duplicate_shortcut_press_does_not_queue_another_placement() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    kb.modifiers(ALT).await;
    let (_, callback) = kb.place(w, 2).await;

    assert_eq!(
        kb.key(2, KEY_Q, PRESSED).await,
        Route::Consumed,
        "a duplicate press of a consumed shortcut stays consumed, and places nothing"
    );
    assert_eq!(kb.key(3, KEY_Q, RELEASED).await, Route::Consumed);
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

#[tokio::test]
async fn peek_after_consumed_shortcut_press_is_not_an_ime_recovery_candidate() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    let ime = kb.text_input().await;
    kb.modifiers(ALT).await;
    let (_, callback) = kb.place(w, 2).await;

    // The extension's peek for the same generation trails the
    // wl_keyboard press.
    kb.peek(2, 100, KEY_Q, PRESSED).await;
    // An empty IME confirmation never replays a shortcut's key
    // (`ime_recovery_does_not_replay_a_compositor_shortcut_key`).
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a consumed shortcut generation must not be recoverable"
    );

    kb.peek(3, 101, KEY_Q, RELEASED).await;
    assert_eq!(kb.key(3, KEY_Q, RELEASED).await, Route::Consumed);
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

/// Placement moves a window with `zaura_surface.set_parent` and sizes
/// it with a configure, so it never changes the surface's application
/// ID (an ARC ID would make Chrome bypass the IME) and never touches
/// the focused IME generation, an in-progress composition included:
/// the steps are all the host gets, and the guest's text input gets
/// nothing until the composition goes on.
#[tokio::test]
async fn placement_shortcuts_leave_the_focused_ime_generation_untouched() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    let ime = kb.text_input().await;
    kb.compose(ime, "한").await;
    kb.modifiers(ALT).await;

    for serial in [2, 4] {
        let (_, callback) = kb.place(w, serial).await;
        assert_eq!(kb.key(serial + 1, KEY_Q, RELEASED).await, Route::Consumed);
        // Exo reports the window at (100, 50) before each.
        assert_eq!(kb.finish(w, callback, (100, 50)).await, bounds("top-left"));
    }

    // The composition goes on, on the focused surface.
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [v3_preedit(ime, ""), v3_commit(ime, "한"), done(ime)]
    );
}

#[tokio::test]
async fn focus_leave_during_consumed_shortcut_does_not_swallow_the_next_press() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    kb.modifiers(ALT).await;
    let (_, callback) = kb.place(w, 2).await;

    // Focus leaves before the chord's release, which the old surface
    // will never receive. The consumed generation must not survive and
    // swallow the first press after focus returns.
    let surface = kb.surface;
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 3, surface.id)]);
    // Exo sends the released Alt before the enter.
    kb.modifiers(0).await;
    let enter = wl_keyboard::evt::enter(kb.k.host, 5, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [entered(kb.k, 5, surface.id, &[]), modifiers(kb.k, 5, 0)]
    );

    assert_eq!(
        kb.key(6, KEY_Q, PRESSED).await,
        Route::Guest,
        "a plain press after focus returns is ordinary guest input"
    );
    assert_eq!(kb.key(7, KEY_Q, RELEASED).await, Route::Guest);
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

#[tokio::test]
async fn chord_key_held_across_refocus_is_released_to_the_guest_that_saw_it() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    kb.modifiers(ALT).await;
    let (_, callback) = kb.place(w, 2).await;

    // The host re-enters with the chord key still held. The re-sent
    // enter's keys array reports it pressed to the guest, so its later
    // release belongs to the guest rather than to the consumed shortcut.
    let surface = kb.surface;
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 3, surface.id)]);
    let keys = KEY_Q.to_ne_bytes();
    let enter = wl_keyboard::evt::enter(kb.k.host, 4, surface.host, &keys).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 4, surface.id, &[KEY_Q]),
            modifiers(kb.k, 4, ALT)
        ]
    );
    assert_eq!(kb.key(5, KEY_Q, RELEASED).await, Route::Guest);
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

/// A host focus cycle that lands while a placement's barrier is pending
/// drains the host IME generation through a barrier of its own, which
/// activates the text input again without ending the placement, whose
/// barrier then moves the window.
#[tokio::test]
async fn ime_focus_churn_inside_a_placement_barrier_keeps_both_barriers_separate() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    let mut ime = kb.text_input().await;
    kb.compose(ime, "한").await;
    kb.modifiers(ALT).await;
    let (_, placement_barrier) = kb.place(w, 2).await;

    // The leave commits the preedit, and drains the host IME generation.
    let surface = kb.surface;
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(
        kb.s.guest_received(),
        [
            v3_preedit(ime, ""),
            v3_commit(ime, "한"),
            done(ime),
            left(kb.k, 3, surface.id),
            v3_leave(ime, surface.id),
        ]
    );
    let host = kb.s.host_received();
    let ime_barrier = deactivation(ime, &host).expect("focus leave must drain the host IME");
    assert_ne!(ime_barrier, placement_barrier);
    // The guest enables again on the new enter.
    let enter = wl_keyboard::evt::enter(kb.k.host, 4, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 4, surface.id, &[]),
            modifiers(kb.k, 4, ALT),
            v3_enter(ime, surface.id)
        ]
    );
    kb.s.guest([
        zwp_text_input_v3::req::enable(ime.id),
        zwp_text_input_v3::req::commit(ime.id),
    ])
    .await;
    ime.serial = 2;
    assert_eq!(kb.s.guest_received(), [done(ime)]);
    // The enable resets the host's state, but activates nothing while
    // the drain is pending. Exo's display answers the reset's barrier.
    for (_, request) in kb.s.host_received() {
        match request {
            AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Activate { .. }) => {
                panic!("an activation while the drain is pending")
            }
            AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) => {
                kb.barrier_done(callback).await;
            }
            _ => {}
        }
    }

    // The IME barrier activates the text input again.
    kb.barrier_done(ime_barrier).await;
    let host = kb.s.host_received();
    let activate = zwp_text_input_v1::Request::Activate {
        seat: ime.host_seat,
        surface: surface.host,
    };
    assert_eq!(host.first(), Some(&(ime.host, activate.into())), "{host:?}");
    assert_eq!(kb.s.guest_received(), []);

    // The placement goes on: its barrier moves the window, and leaves
    // the fresh IME generation alone.
    assert_eq!(
        kb.finish(w, placement_barrier, (300, 200)).await,
        bounds("top-left")
    );
}

/// A chord's placement reaches the host before the chord's `ack_key`, as
/// Sommelier acts on a shortcut before it acknowledges its key.
#[tokio::test]
async fn a_shortcut_places_before_its_key_is_acknowledged() {
    let (mut kb, _) = Kb::placing(top_left(), true).await;
    let w = kb.window().await;
    kb.modifiers(ALT).await;
    // The placement's step, then the acknowledgement.
    let (step, callback) = kb.place(w, 2).await;
    let ext = kb.k.ext;
    assert!(step.iter().all(|(sender, _)| *sender != ext), "{step:?}");
    assert_eq!(kb.finish(w, callback, (300, 200)).await, bounds("top-left"));
}

// -----------------------------------------------------------------------
// Keymaps
// -----------------------------------------------------------------------

/// The proxy maps a keymap, rather than reading it, so it loads from a
/// memfd whose offset the host left at its end, as the session's host
/// sends it: a chord then classifies. It compiles to the keycodes and
/// modifier masks the tests name.
#[tokio::test]
async fn keymap_loads_from_non_rewound_memfd() {
    let mut kb = Kb::with_accelerators("<Control>a").await;
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);

    // Reads state no peer can observe: the compiled keymap, which the
    // test's constants name.
    let keyboard = HostId::for_test(kb.k.host);
    let ctx = kb.s.ctx();
    let keymap = ctx.keys.keyboards().keymap_for_test(keyboard).unwrap();
    let state = keymap.state();
    for (sym, key) in [
        (keysyms::KEY_q, KEY_Q),
        (keysyms::KEY_w, KEY_W),
        (keysyms::KEY_e, KEY_E),
        (keysyms::KEY_t, KEY_T),
        (keysyms::KEY_a, KEY_A),
        (keysyms::KEY_s, KEY_S),
        (keysyms::KEY_d, KEY_D),
        (keysyms::KEY_Shift_L, KEY_LEFTSHIFT),
        (keysyms::KEY_z, KEY_Z),
        (keysyms::KEY_x, KEY_X),
        (keysyms::KEY_c, KEY_C),
        (keysyms::KEY_b, KEY_B),
        (keysyms::KEY_space, KEY_SPACE),
        (keysyms::KEY_BackSpace, BACKSPACE),
    ] {
        let keycode = xkb::Keycode::new(key + 8);
        assert_eq!(state.key_get_one_sym(keycode).raw(), sym, "key {key}");
    }
    let repeatable = keymap.repeatable();
    assert!(repeatable.contains(&KEY_SPACE) && repeatable.contains(&BACKSPACE));
    assert!(!repeatable.contains(&KEY_LEFTSHIFT));
    let keymap = state.get_keymap();
    let mask = |name| 1 << keymap.mod_get_index(name);
    assert_eq!(
        [SHIFT, CAPS_LOCK, CONTROL, ALT],
        [
            mask(xkb::MOD_NAME_SHIFT),
            mask(xkb::MOD_NAME_CAPS),
            mask(xkb::MOD_NAME_CTRL),
            mask(xkb::MOD_NAME_ALT)
        ]
    );
}

/// A keymap the proxy cannot use for each way `on_keymap` rejects one:
/// what the case is, and the keymap's format, file contents and size.
fn unusable_keymaps() -> [(&'static str, u32, Vec<u8>, u32); 9] {
    let xkb_v1 = KEYMAP_FORMAT_XKB_V1;
    let not_xkb = b"this is not a valid xkb keymap\0".to_vec();
    let not_xkb_size = not_xkb.len() as u32;
    // A keymap only its format makes unusable.
    let usable = crate::proxy::harness::default_keymap();
    let usable_size = usable.len() as u32;
    [
        ("no keymap", KEYMAP_FORMAT_NO_KEYMAP, Vec::new(), 100),
        ("an unsupported format", 99, usable, usable_size),
        // mmap(len=0) is undefined.
        ("an empty keymap", xkb_v1, Vec::new(), 0),
        // The size is refused before the descriptor is mapped.
        (
            "a keymap over the size limit",
            xkb_v1,
            Vec::new(),
            MAX_KEYMAP_SIZE + 1,
        ),
        // Mapping past the end of the file would raise SIGBUS.
        ("a size beyond the file", xkb_v1, b"\0".to_vec(), 2),
        ("no trailing NUL", xkb_v1, b"xkb_keymap".to_vec(), 10),
        ("invalid UTF-8", xkb_v1, vec![0xFF, 0xFE, 0xFD, 0], 4),
        // xkbcommon takes the text as a C string.
        ("a NUL inside", xkb_v1, b"xkb\0keymap\0".to_vec(), 11),
        ("an invalid XKB keymap", xkb_v1, not_xkb, not_xkb_size),
    ]
}

/// A keymap the proxy cannot use replaces the loaded one: it is
/// forwarded, and the keyboard's XKB state goes, so that no chord is
/// classified until a usable keymap arrives. The keys held and their
/// generations, with their accelerator suppressions and the IME's
/// tombstone and cancelled repeat of a held Backspace, the keyboard's
/// modifiers, which Exo does not send again, and another keyboard's
/// keymap stay.
#[tokio::test]
async fn an_unusable_keymap_clears_the_loaded_one_and_is_forwarded() {
    for (case, format, bytes, size) in unusable_keymaps() {
        let mut kb = Kb::with_accelerators("<Control>a,<Control>b").await;
        let ime = kb.text_input().await;
        let other = kb.another_keyboard().await;
        // The input method takes Backspace and repeats it: its press is
        // the repeat's tombstone.
        kb.peek(1, 123, BACKSPACE, PRESSED).await;
        kb.lease(ime).await;
        assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host, "{case}");
        // Control+A, a host accelerator, is suppressed, and ends the
        // lease for the rest of Backspace's hold.
        kb.modifiers(CONTROL).await;
        assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host, "{case}");

        kb.keymap_of(format, &bytes, size).await;

        // No chord without a keymap.
        assert_eq!(kb.key(3, KEY_B, PRESSED).await, Route::Guest, "{case}");
        assert_eq!(kb.key(4, KEY_B, RELEASED).await, Route::Guest, "{case}");
        // Backspace keeps its tombstone, and A its suppression: the
        // host keeps their presses again.
        assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host, "{case}");
        assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host, "{case}");
        // Another keyboard's keymap stays.
        kb.modifiers_on(other, CONTROL).await;
        assert_eq!(
            kb.key_on(other, 5, KEY_B, PRESSED).await,
            Route::Host,
            "{case}: another keyboard's keymap stays"
        );
        // Control stays held for the next keymap.
        kb.keymap().await;
        assert_eq!(kb.key(6, KEY_B, PRESSED).await, Route::Host, "{case}");
        // Backspace stays held, its repeat cancelled.
        kb.confirm(ime).await;
        assert_eq!(kb.s.guest_received(), [], "{case}");
    }
}

/// A key that arrives before its keyboard's first enter has bound an
/// extended keyboard goes unacknowledged at debug, since the next enter
/// binds one. A host without `zcr_keyboard_extension_v1` acknowledges no
/// key ever, so the first such key in the process warns and the rest go
/// at debug. The flag is the process's, so the test runs alone in a
/// child process.
#[test]
fn a_host_without_the_keyboard_extension_warns_once_per_process() {
    crate::test_support::in_own_process(
        "handler::keyboard::tests::a_host_without_the_keyboard_extension_warns_once_per_process",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            runtime.block_on(async {
                // The host has the extension, but no enter has bound the
                // keyboard's extended keyboard yet.
                let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
                let press = wl_keyboard::evt::key(kb.k.host, 1, 1, KEY_A, PRESSED);
                kb.s.host([press]).await;
                assert_eq!(
                    kb.s.guest_received(),
                    [key_event(kb.k, 1, 1, KEY_A, PRESSED)]
                );
                assert_eq!(kb.s.host_received(), []);
                assert!(!NO_KEYBOARD_EXTENSION_WARNED.logged());

                // The host removes the extension.
                let registry = kb.s.registry().await;
                kb.s.host([wl_registry::evt::global_remove(registry.host, kb.extension)])
                    .await;
                assert_eq!(kb.s.guest_received(), []);
                assert_eq!(kb.s.host_received(), []);
                let release = wl_keyboard::evt::key(kb.k.host, 2, 2, KEY_A, RELEASED);
                kb.s.host([release]).await;
                assert_eq!(
                    kb.s.guest_received(),
                    [key_event(kb.k, 2, 2, KEY_A, RELEASED)]
                );
                assert_eq!(kb.s.host_received(), []);
                assert!(NO_KEYBOARD_EXTENSION_WARNED.logged());
                assert_eq!(
                    take_no_keyboard_extension_log_level(&NO_KEYBOARD_EXTENSION_WARNED),
                    log::Level::Debug
                );
            });
        },
    );
}

/// A keymap of the size limit loads, and one byte more is refused: the
/// keyboard then classifies no chord.
#[tokio::test]
async fn on_keymap_ignores_a_keymap_over_the_size_limit() {
    for (size, accepted) in [(MAX_KEYMAP_SIZE, true), (MAX_KEYMAP_SIZE + 1, false)] {
        let mut kb = Kb::with_accelerators("<Control>a").await;
        kb.modifiers(CONTROL).await;
        // A valid keymap, padded with blanks to `size` with its NUL.
        let mut bytes = crate::proxy::harness::default_keymap();
        bytes.pop();
        bytes.resize(size as usize - 1, b' ');
        bytes.push(0);

        kb.keymap_of(KEYMAP_FORMAT_XKB_V1, &bytes, size).await;
        let chord = match accepted {
            true => Route::Host,
            false => Route::Guest,
        };
        assert_eq!(
            kb.key(2, KEY_A, PRESSED).await,
            chord,
            "a keymap of {size} bytes"
        );
    }
}

// -----------------------------------------------------------------------
// Acknowledgments and key generations
// -----------------------------------------------------------------------

#[tokio::test]
async fn host_accelerators_stay_with_the_host_and_other_keys_reach_the_guest() {
    let mut kb = Kb::with_accelerators("<Control>a").await;
    kb.modifiers(CONTROL).await;

    // ChromiumOS acks an accelerator's press and its unmatched release
    // NOT_HANDLED and forwards neither.
    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
    assert_eq!(kb.key(3, KEY_A, RELEASED).await, Route::Host);
    assert_eq!(
        kb.key(4, KEY_A, PRESSED).await,
        Route::Host,
        "the next press is classified again"
    );
    assert_eq!(kb.key(5, KEY_B, PRESSED).await, Route::Guest);
    assert_eq!(kb.key(6, KEY_B, RELEASED).await, Route::Guest);
}

#[tokio::test]
async fn each_key_generation_reaches_the_guest_once() {
    use Route::*;
    // Each event, where it goes, and whether the key is down after it:
    // from its press to its release, wherever they go.
    type Event = (u32, Route, bool);
    let cases: [(&str, u32, &[Event]); 4] = [
        ("an untracked release", KEY_A, &[(RELEASED, Host, false)]),
        (
            "a duplicate press",
            KEY_A,
            &[
                (PRESSED, Guest, true),
                (PRESSED, Consumed, true),
                (RELEASED, Guest, false),
            ],
        ),
        (
            "a held key",
            KEY_A,
            &[(PRESSED, Guest, true), (RELEASED, Guest, false)],
        ),
        (
            "a host accelerator",
            KEY_B,
            &[(PRESSED, Host, true), (RELEASED, Host, false)],
        ),
    ];
    for (case, key, events) in cases {
        let mut kb = Kb::with_accelerators("b").await;
        for (serial, &(state, route, held)) in (2..).zip(events) {
            assert_eq!(kb.key(serial, key, state).await, route, "{case}");
            // That a key is down shows in no message until a later
            // event of it, which each row would then have to add.
            let keys = &kb.s.ctx().keys;
            assert_eq!(keys.held(HostId::for_test(kb.k.host), key), held, "{case}");
        }
    }
}

/// `wl_keyboard` has no repeated key state before version 10, beyond
/// the version 5 the proxy binds `wl_seat` at, and Exo sends no
/// repeats: such an event reaches no peer, even for a key the guest
/// holds, and changes nothing of the key.
#[tokio::test]
async fn a_repeated_key_state_reaches_no_peer() {
    let mut kb = Kb::new().await;
    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Guest);
    let repeat = wl_keyboard::evt::key(kb.k.host, 3, 3, KEY_A, wl_keyboard::KEY_STATE_REPEATED);
    kb.s.host([repeat]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
    assert_eq!(kb.key(4, KEY_A, RELEASED).await, Route::Guest);
}

/// The proxy creates a keyboard's extended keyboard at its first enter
/// and acks every key through it from then on, even once the host
/// removed the extension's global, until the keyboard ends.
#[tokio::test]
async fn the_extended_keyboard_acks_keys_from_the_first_enter_until_the_keyboard_ends() {
    let accelerators = crate::accelerator::parse_accelerators("<Control>a").unwrap();
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;

    // Before its first enter, a key has no extended keyboard to ack it.
    let press = wl_keyboard::evt::key(kb.k.host, 1, 1, KEY_B, PRESSED);
    kb.s.host([press]).await;
    assert_eq!(
        kb.s.guest_received(),
        [key_event(kb.k, 1, 1, KEY_B, PRESSED)]
    );
    assert_eq!(kb.s.host_received(), []);

    let surface = kb.surface;
    kb.k = kb.first_enter(kb.k, surface).await;
    let other_surface = kb.s.surface().await;
    let enter = wl_keyboard::evt::enter(kb.k.host, 2, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 2, other_surface.id, &[]),
            modifiers(kb.k, 2, 0)
        ]
    );
    assert_eq!(
        kb.s.host_received(),
        [],
        "one extended keyboard per keyboard"
    );

    // The manager's global goes, but its child lives on.
    let registry = kb.s.registry().await;
    kb.s.host([wl_registry::evt::global_remove(registry.host, kb.extension)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(3, KEY_A, PRESSED).await, Route::Host);

    // Ending the keyboard ends its extended keyboard, its modifiers and
    // its suppressed accelerator, as every batch checks.
    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), released(kb.k));
}

/// Releasing a keyboard forgets the key state it owns, as every batch
/// checks: its peek watermark, the release a retired generation owes the
/// guest, and its record. And only that: another keyboard's newer peek
/// watermark stays, so that an empty confirmation recovers no older key.
#[tokio::test]
async fn on_release_destroys_extended_keyboard_and_cleans_map() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = kb.another_keyboard().await;
    // The keyboard's peek watermark.
    kb.peek(1, 1, KEY_D, PRESSED).await;
    // A guest press whose release is retired: the peek released A before
    // `wl_keyboard` did, and A is pressed again.
    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Guest);
    kb.peek(3, 3, KEY_A, RELEASED).await;
    kb.peek(4, 4, KEY_A, PRESSED).await;
    // The other keyboard holds S, then Shift, which does not repeat.
    kb.peek_on(other, 5, 5, KEY_S, PRESSED).await;
    kb.peek_on(other, 6, 6, KEY_LEFTSHIFT, PRESSED).await;

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), released(kb.k));

    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "another keyboard's watermark stays"
    );
}

/// Accelerator suppression and modifiers belong to their keyboard.
#[tokio::test]
async fn accelerator_state_is_scoped_to_its_keyboard() {
    let mut kb = Kb::with_accelerators("<Control>a").await;
    // Control is held when the other keyboard gets its keymap, and
    // pressed again once it has one.
    kb.modifiers(CONTROL).await;
    let other = kb.another_keyboard().await;
    kb.modifiers(0).await;
    kb.modifiers(CONTROL).await;

    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
    assert_eq!(
        kb.key_on(other, 3, KEY_A, RELEASED).await,
        Route::Host,
        "the other keyboard never pressed the key"
    );
    assert_eq!(kb.key(4, KEY_A, RELEASED).await, Route::Host);
    assert_eq!(
        kb.key_on(other, 5, KEY_A, PRESSED).await,
        Route::Guest,
        "the other keyboard holds no Control"
    );
    assert_eq!(kb.key(6, KEY_A, PRESSED).await, Route::Host);
}

/// A new keymap keeps the keys held under the old one, with the
/// suppression of a held accelerator's key, and the modifiers Exo sent
/// before it, which it does not send again.
#[tokio::test]
async fn a_new_keymap_keeps_held_keys_and_modifiers() {
    let mut kb = Kb::with_accelerators("<Control>a").await;
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(2, KEY_A, PRESSED).await, Route::Host);
    assert_eq!(kb.key(3, KEY_B, PRESSED).await, Route::Guest);

    kb.keymap().await;
    assert_eq!(kb.key(4, KEY_B, RELEASED).await, Route::Guest);
    assert_eq!(
        kb.key(5, KEY_A, PRESSED).await,
        Route::Host,
        "the held Control survives the keymap"
    );

    kb.modifiers(0).await;
    assert_eq!(
        kb.key(6, KEY_A, PRESSED).await,
        Route::Host,
        "the accelerator's key stays suppressed"
    );
    kb.keymap().await;
    assert_eq!(
        kb.key(7, KEY_A, PRESSED).await,
        Route::Host,
        "the suppression survives the keymap"
    );
}

/// Exo switches the layout on Ctrl+Space, a ChromeOS accelerator, so
/// the keymap arrives while Space is held. The accelerator's key stays
/// the host's: a peek that trails it, once Control is up, gives the IME
/// nothing to recover.
#[tokio::test]
async fn an_accelerator_held_across_a_keymap_is_not_recovered() {
    let mut kb = Kb::with_accelerators("<Control>space").await;
    let ime = kb.text_input().await;
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(2, KEY_SPACE, PRESSED).await, Route::Host);

    kb.keymap().await;
    kb.modifiers(0).await;
    kb.peek(4, 501, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);
}

/// A keymap the proxy cannot use ends the chords of the one before it
/// but, like a new keymap, keeps the keys held under it, so their
/// releases reach the guest that got their presses.
#[tokio::test]
async fn a_rejected_keymap_keeps_held_keys() {
    for (case, format, bytes, size) in unusable_keymaps() {
        let mut kb = Kb::with_accelerators("<Control>a").await;
        kb.modifiers(CONTROL).await;
        assert_eq!(kb.key(2, KEY_B, PRESSED).await, Route::Guest, "{case}");

        kb.keymap_of(format, &bytes, size).await;
        assert_eq!(
            kb.key(3, KEY_B, RELEASED).await,
            Route::Guest,
            "{case}: the held key's release"
        );
        assert_eq!(
            kb.key(4, KEY_A, PRESSED).await,
            Route::Guest,
            "{case}: no chord without a keymap"
        );
        assert_eq!(kb.key(5, KEY_A, RELEASED).await, Route::Guest, "{case}");
    }
}

// -----------------------------------------------------------------------
// Modifiers
// -----------------------------------------------------------------------

// Exo's `Keyboard::SetFocus` sends the seat's modifiers before `enter`,
// and `WaylandKeyboardDelegate::OnKeyboardModifiers` sends them only
// when they differ from the last ones it sent that keyboard. A key's
// chord shows in where it goes: `<Alt>q` is the host's only with Alt
// alone held.
#[tokio::test]
async fn modifiers_sent_before_enter_survive_it() {
    let accelerators = crate::accelerator::parse_accelerators("<Alt>q").unwrap();
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;
    kb.modifiers(ALT).await;
    let enter = wl_keyboard::evt::enter(kb.k.host, 2, kb.surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 2, kb.surface.id, &[]),
            modifiers(kb.k, 2, ALT)
        ]
    );

    assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
}

#[tokio::test]
async fn modifiers_survive_a_focus_change() {
    let mut kb = Kb::with_accelerators("<Alt>q").await;
    let other_surface = kb.s.surface().await;
    kb.modifiers(ALT).await;

    // Alt stays held, so Exo sends no modifiers with the new focus.
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, kb.surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 2, kb.surface.id)]);
    assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
    assert_eq!(kb.key(4, KEY_Q, RELEASED).await, Route::Host);
    let enter = wl_keyboard::evt::enter(kb.k.host, 5, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 5, other_surface.id, &[]),
            modifiers(kb.k, 5, ALT)
        ]
    );
    assert_eq!(kb.key(6, KEY_Q, PRESSED).await, Route::Host);
}

/// The protocol requires a modifiers event after every enter, which Exo
/// sends before it, if at all.
#[tokio::test]
async fn every_forwarded_enter_is_followed_by_the_keyboards_modifiers() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let (_, other) = kb.s.keyboard().await;
    kb.modifiers(ALT).await;

    let surface = kb.surface;
    let enter = wl_keyboard::evt::enter(kb.k.host, 2, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [entered(kb.k, 2, surface.id, &[]), modifiers(kb.k, 2, ALT)]
    );
    let other = Keyboard {
        id: other.id,
        host: other.host,
        ext: 0,
    };
    let enter = wl_keyboard::evt::enter(other.host, 3, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    extended_keyboard_of(other, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [entered(other, 3, surface.id, &[]), modifiers(other, 3, 0)],
        "a keyboard with no modifiers yet holds none"
    );
}

// Exo's `Keyboard::OnKeyboardLayoutNameChanged` sends only the keymap.
#[tokio::test]
async fn modifiers_carry_over_to_a_new_keymap() {
    let mut kb = Kb::with_accelerators("<Control>q").await;
    // Exo reports an active Caps Lock as depressed, not locked; a lock
    // is not part of a chord.
    kb.modifiers(CONTROL | CAPS_LOCK).await;
    kb.keymap().await;
    assert_eq!(kb.key(2, KEY_Q, PRESSED).await, Route::Host);
    // No chord holds a lock, so only the XKB state shows that the Caps
    // Lock carried over.
    let ctx = kb.s.ctx();
    let keymap = ctx
        .keys
        .keyboards()
        .keymap_for_test(HostId::for_test(kb.k.host))
        .unwrap();
    assert!(keymap
        .state()
        .mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_DEPRESSED));
}

#[tokio::test]
async fn modifiers_sent_before_the_keymap_apply_to_it() {
    let accelerators = crate::accelerator::parse_accelerators("<Alt>q").unwrap();
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, accelerators)).await;
    let id = kb.s.new_id();
    let host =
        kb.s.create(wl_seat::req::get_keyboard(kb.seat.id, id))
            .await;
    kb.k = Keyboard { id, host, ext: 0 };
    kb.modifiers(ALT).await;
    kb.keymap().await;
    let surface = kb.surface;
    let enter = wl_keyboard::evt::enter(kb.k.host, 2, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [entered(kb.k, 2, surface.id, &[]), modifiers(kb.k, 2, ALT)]
    );

    assert_eq!(kb.key(3, KEY_Q, PRESSED).await, Route::Host);
}

/// A keyboard's modifiers end with it, even when no keymap arrived to
/// apply them to, as every batch checks.
#[tokio::test]
async fn modifiers_without_a_keymap_end_with_their_keyboard() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let id = kb.s.new_id();
    let host =
        kb.s.create(wl_seat::req::get_keyboard(kb.seat.id, id))
            .await;
    let k = Keyboard { id, host, ext: 0 };
    kb.modifiers_on(k, CONTROL).await;

    kb.s.guest([wl_keyboard::req::release(id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(
        kb.s.host_received(),
        [(host, wl_keyboard::Request::Release {}.into())]
    );
}

// -----------------------------------------------------------------------
// Focus
// -----------------------------------------------------------------------

#[tokio::test]
async fn keyboard_enter_initializes_held_key_state() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let keys = BACKSPACE.to_ne_bytes();
    let enter = wl_keyboard::evt::enter(kb.k.host, 1, kb.surface.host, &keys).unwrap();
    kb.s.host([enter]).await;
    kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 1, kb.surface.id, &[BACKSPACE]),
            modifiers(kb.k, 1, 0)
        ]
    );

    // The guest was told the key is held, so its release is the guest's.
    assert_eq!(kb.key(2, BACKSPACE, RELEASED).await, Route::Guest);
}

/// A compositor may repeat an enter for the focus a keyboard has, while
/// a key is held and the guest composes: it changes nothing, as in
/// ChromiumOS, which suppresses it.
#[tokio::test]
async fn a_duplicate_enter_changes_nothing() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, "한", "").unwrap();
    kb.s.host([preedit]).await;
    assert_eq!(kb.s.guest_received(), [v3_preedit(ime, "한"), done(ime)]);
    assert_eq!(kb.key(2, BACKSPACE, PRESSED).await, Route::Guest);

    let enter = wl_keyboard::evt::enter(kb.k.host, 3, kb.surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);

    assert_eq!(kb.key(4, BACKSPACE, RELEASED).await, Route::Guest);
    // The composition goes on.
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [v3_preedit(ime, ""), v3_commit(ime, "한"), done(ime)]
    );
}

#[tokio::test]
async fn direct_focus_replacement_sends_balanced_leave_then_enter() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let (k, surface) = (kb.k, kb.surface);
    let other_surface = kb.s.surface().await;
    let other = kb.s.keyboard_of(kb.seat.id).await;
    let other = Keyboard {
        id: other.id,
        host: other.host,
        ext: 0,
    };

    // Another keyboard of the seat enters another surface, which takes
    // the seat's focus without a leave.
    let enter = wl_keyboard::evt::enter(other.host, 2, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    let host = kb.s.host_received();
    let other = Keyboard {
        ext: extended_keyboard_of(other, &host[..1]),
        ..other
    };
    assert!(deactivated(ime, &host[1..]), "{host:?}");
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(other, 2, other_surface.id, &[]),
            modifiers(other, 2, 0),
            v3_leave(ime, surface.id),
            v3_enter(ime, other_surface.id),
        ]
    );

    // The first keyboard's delayed leave balances its enter, once.
    kb.s.host([wl_keyboard::evt::leave(k.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(k, 3, surface.id)]);
    assert_eq!(kb.s.host_received(), []);
    kb.s.host([wl_keyboard::evt::leave(k.host, 4, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
}

/// A host sends every keyboard of the seat its leave, so the first
/// one ends the seat's focus on the surface, though the other keyboard
/// still focuses it; the other's leave then balances its enter.
#[tokio::test]
async fn leave_from_one_keyboard_ends_the_seats_focus_on_the_surface() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let surface = kb.surface;
    // Each keyboard gets its own enter.
    let other = kb.another_keyboard().await;

    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, surface.host)])
        .await;
    assert_eq!(
        kb.s.guest_received(),
        [left(kb.k, 2, surface.id), v3_leave(ime, surface.id)]
    );
    assert!(deactivated(ime, &kb.s.host_received()));

    kb.s.host([wl_keyboard::evt::leave(other.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(other, 3, surface.id)]);
    assert_eq!(kb.s.host_received(), []);
}

#[tokio::test]
async fn same_keyboard_reenter_drops_delayed_old_leave() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let surface = kb.surface;
    let other_surface = kb.s.surface().await;

    let enter = wl_keyboard::evt::enter(kb.k.host, 2, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 2, other_surface.id, &[]),
            modifiers(kb.k, 2, 0),
            v3_leave(ime, surface.id),
            v3_enter(ime, other_surface.id),
        ]
    );
    assert!(deactivated(ime, &kb.s.host_received()));

    // An old leave must not end the newer enter of the same keyboard.
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
}

/// A delayed leave for a surface the guest already destroyed, which the
/// keyboard's focus is not on, does not tear down the live focus: the
/// held Backspace stays held, on the focused field.
#[tokio::test]
async fn unmapped_leave_does_not_clear_live_keyboard_focus_state() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(1, 1, BACKSPACE, PRESSED).await;
    let gone = kb.dying_surface().await;

    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, gone.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), recovered(ime, kb.k, 1, 1, BACKSPACE));
}

/// The guest destroys the focused surface, which ends the keyboard's
/// focus at once (the host-surface mapping a focus keeps never outlives
/// the guest surface): the host's leave that follows, which names the
/// surface whose mapping waits for the host's `delete_id`, sends nothing
/// more, and no focus outlives the surface. The next enter is the
/// keyboard's and the text input's only focus.
#[tokio::test]
async fn matching_leave_uses_stored_host_surface_after_mapping_is_unavailable() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let surface = kb.surface;
    kb.s.guest([wl_surface::req::destroy(surface.id)]).await;
    let host = kb.s.host_received();
    assert_eq!(
        host[0],
        (surface.host, wl_surface::Request::Destroy {}.into())
    );
    assert!(deactivated(ime, &host[1..]), "{host:?}");
    assert_eq!(kb.s.guest_received(), [v3_leave(ime, surface.id)]);

    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
    kb.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
        .await;
    let delete_id = wl_display::Event::DeleteId { id: surface.id };
    assert_eq!(kb.s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
    let next = kb.s.surface().await;
    let enter = wl_keyboard::evt::enter(kb.k.host, 3, next.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 3, next.id, &[]),
            modifiers(kb.k, 3, 0),
            v3_enter(ime, next.id)
        ]
    );
    assert_eq!(kb.s.host_received(), []);
}

/// A surface keeps its mapping until the host acknowledges its destroy,
/// so a delayed host enter for it still translates; it must not make
/// the dead surface current again, nor bind the keyboard's extended
/// keyboard.
#[tokio::test]
async fn delayed_enter_for_pending_destroy_surface_does_not_revive_focus() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let surface = kb.surface;
    kb.s.guest([wl_surface::req::destroy(surface.id)]).await;
    let destroy = wl_surface::Request::Destroy {};
    assert_eq!(kb.s.host_received(), [(surface.host, destroy.into())]);

    let enter = wl_keyboard::evt::enter(kb.k.host, 1, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "the stale event must not be forwarded to the guest"
    );
    assert_eq!(kb.s.host_received(), []);
    // The seat has no focus for a text input to enter.
    kb.s.text_input(kb.seat.id).await;
    assert_eq!(kb.s.guest_received(), []);
}

/// An enter for a keyboard the guest has released, which the host sent
/// before it read the release, cannot be delivered: it binds no extended
/// keyboard.
#[tokio::test]
async fn enter_without_keyboard_route_has_no_extension_side_effects() {
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    let release = wl_keyboard::Request::Release {};
    assert_eq!(kb.s.host_received(), [(kb.k.host, release.into())]);

    let enter = wl_keyboard::evt::enter(kb.k.host, 1, kb.surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "an enter without a guest keyboard-to-seat route is undeliverable"
    );
    assert_eq!(kb.s.host_received(), []);
}

/// The other keyboard of a seat enters another surface while the first
/// still holds Backspace: that enter retires the first keyboard's key
/// state, and its later leave changes nothing more than its own focus.
/// Its keymap and modifiers stay.
#[tokio::test]
async fn mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus() {
    let mut kb = Kb::with_accelerators("<Control>BackSpace").await;
    let ime = kb.text_input().await;
    let other = unentered(kb.s.keyboard_of(kb.seat.id).await);
    let (surface, other_surface) = (kb.surface, kb.s.surface().await);
    // The input method takes Backspace and repeats it. Control+Backspace,
    // a host accelerator, then comes as the repeat's tombstone, suppressed,
    // and another key's press ends the lease for the rest of the hold.
    kb.peek(1, 123, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host);
    kb.peek(2, 124, KEY_X, PRESSED).await;
    // Exo resends modifiers only when they change, so the focus teardown
    // keeps them.
    kb.modifiers(SHIFT).await;

    let enter = wl_keyboard::evt::enter(other.host, 3, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    let host = kb.s.host_received();
    let other = Keyboard {
        ext: extended_keyboard_of(other, &host[..1]),
        ..other
    };
    assert!(deactivated(ime, &host[1..]), "{host:?}");
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(other, 3, other_surface.id, &[]),
            modifiers(other, 3, 0),
            v3_leave(ime, surface.id),
            v3_enter(ime, other_surface.id),
        ]
    );
    // Only the keyboard's own leave, which the IME core re-sends.
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 4, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 4, surface.id)]);
    assert_eq!(kb.s.host_received(), []);

    // Backspace is idle: no tombstone, suppression or lease keeps its
    // next press from the guest.
    assert_eq!(kb.key(5, BACKSPACE, PRESSED).await, Route::Guest);
    assert_eq!(kb.key(6, BACKSPACE, RELEASED).await, Route::Guest);
    // The keyboard's modifiers come with its next enter, and its keymap
    // still makes chords.
    let enter = wl_keyboard::evt::enter(kb.k.host, 7, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 7, other_surface.id, &[]),
            modifiers(kb.k, 7, SHIFT)
        ]
    );
    kb.modifiers(CONTROL).await;
    assert_eq!(kb.key(8, BACKSPACE, PRESSED).await, Route::Host);
}

/// Another keyboard's enter of another surface retires the first
/// keyboard's key state before its delayed leave, which names a surface
/// the guest destroyed: a dead keyboard must not keep the seat's
/// Backspace fallback armed.
#[tokio::test]
async fn new_focus_retires_stale_keyboard_state_before_delayed_leave() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = unentered(kb.s.keyboard_of(kb.seat.id).await);
    let (surface, other_surface) = (kb.surface, kb.s.surface().await);
    // The input method took Backspace and repeated it, until another
    // key's press ended the lease.
    kb.peek(1, 123, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host);
    kb.peek(2, 124, KEY_X, PRESSED).await;

    let enter = wl_keyboard::evt::enter(other.host, 3, other_surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    let host = kb.s.host_received();
    let other = Keyboard {
        ext: extended_keyboard_of(other, &host[..1]),
        ..other
    };
    assert!(deactivated(ime, &host[1..]), "{host:?}");
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(other, 3, other_surface.id, &[]),
            modifiers(other, 3, 0),
            v3_leave(ime, surface.id),
            v3_enter(ime, other_surface.id),
        ]
    );
    let gone = kb.dying_surface().await;
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 4, gone.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);

    assert_eq!(kb.key(5, BACKSPACE, PRESSED).await, Route::Guest);
    assert_eq!(kb.key(6, BACKSPACE, RELEASED).await, Route::Guest);
}

/// A leave of a surface the keyboard no longer focuses changes nothing:
/// the seat's Backspace repeat goes on. A matching leave ends it, with
/// the focus.
#[tokio::test]
async fn stale_leave_does_not_end_current_seat_backspace_repeat() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let (surface, other_surface) = (kb.surface, kb.s.surface().await);
    kb.peek(1, 1, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;

    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, other_surface.host)])
        .await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "stale leave must not emit a text-input leave"
    );
    assert_eq!(kb.s.host_received(), []);
    assert!(kb.leased(kb.k));
    // The repeat's tombstone.
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host);

    // A matching leave still ends the fallback and clears focus.
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(
        kb.s.guest_received(),
        [left(kb.k, 3, surface.id), v3_leave(ime, surface.id)]
    );
    assert!(deactivated(ime, &kb.s.host_received()));
    assert!(!kb.leased(kb.k));
}

/// A keyboard's enter of another surface invalidates the text input's
/// state for the old one before the guest enables it again: the shown
/// preedit is committed, the request the guest did not commit is
/// dropped, and the host text input waits, deactivated, for the new
/// enable. The commits still count. The keys of the new focus then
/// start afresh.
#[tokio::test]
async fn keyboard_enter_invalidates_stale_text_input_before_new_enable() {
    let mut kb = Kb::new().await;
    let mut ime = kb.text_input().await;
    kb.compose(ime, "한").await;
    // A request the guest has not committed.
    let stale = zwp_text_input_v3::req::set_surrounding_text(ime.id, "stale", 5, 5).unwrap();
    kb.s.guest([stale]).await;
    assert_eq!(kb.s.host_received(), []);
    let (surface, next) = (kb.surface, kb.s.surface().await);

    let enter = wl_keyboard::evt::enter(kb.k.host, 2, next.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            v3_preedit(ime, ""),
            v3_commit(ime, "한"),
            done(ime),
            entered(kb.k, 2, next.id, &[]),
            modifiers(kb.k, 2, 0),
            v3_leave(ime, surface.id),
            v3_enter(ime, next.id),
        ]
    );
    let host = kb.s.host_received();
    let barrier = deactivation(ime, &host).expect("the deactivation of the old focus");

    // Not enabled: a commit activates nothing, and gets no `done`.
    kb.s.guest([zwp_text_input_v3::req::commit(ime.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    let commit_state = zwp_text_input_v1::Request::CommitState { serial: 2 };
    assert_eq!(kb.s.host_received(), [(ime.host, commit_state.into())]);
    kb.s.guest([
        zwp_text_input_v3::req::enable(ime.id),
        zwp_text_input_v3::req::commit(ime.id),
    ])
    .await;
    ime.serial = 3;
    assert_eq!(kb.s.guest_received(), [done(ime)]);
    // The enable resets the host's state to the guest's, without the
    // dropped request, but activates nothing while the old focus
    // drains. Exo's display answers the reset's barrier.
    let reset = kb.s.host_received();
    for (_, request) in &reset {
        if let AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) = request {
            kb.barrier_done(*callback).await;
        }
    }
    kb.barrier_done(barrier).await;
    let activation = kb.s.host_received();
    let activate = zwp_text_input_v1::Request::Activate {
        seat: ime.host_seat,
        surface: next.host,
    };
    assert_eq!(
        activation.first(),
        Some(&(ime.host, activate.into())),
        "{activation:?}"
    );
    let stale = |(_, request): &(u32, AnyRequest)| match request {
        AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::SetSurroundingText {
            text, ..
        }) => *text == "stale".into(),
        AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Activate { .. }) => true,
        _ => false,
    };
    assert!(!reset.iter().any(stale), "{reset:?}");
    assert!(!activation[1..].iter().any(stale), "{activation:?}");
    assert_eq!(kb.s.guest_received(), []);

    // A Backspace the input method took: another key cancels its IME
    // repeat, so its confirmation recovers nothing.
    kb.peek(5, 9, BACKSPACE, PRESSED).await;
    assert_eq!(kb.key(6, KEY_A, PRESSED).await, Route::Guest);
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);

    // Its release clears the cancellation, so the next Backspace, which
    // the IME consumes, may repeat.
    kb.peek(7, 13, BACKSPACE, RELEASED).await;
    assert_eq!(kb.key(7, BACKSPACE, RELEASED).await, Route::Host);
    kb.peek(8, 14, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;
    assert_eq!(kb.key(8, BACKSPACE, PRESSED).await, Route::Host);
    kb.peek(9, 15, BACKSPACE, RELEASED).await;
    assert_eq!(kb.key(9, BACKSPACE, RELEASED).await, Route::Host);
    assert!(!kb.leased(kb.k));
}

// -----------------------------------------------------------------------
// Keyboard release
// -----------------------------------------------------------------------

/// Releasing the keyboard that gave a seat its focus ends the seat's
/// text-input focus: the guest commits its preedit and gets its leave,
/// and the host input method is deactivated and drained.
#[tokio::test]
async fn keyboard_release_ends_focus_when_no_other_keyboard_owns_the_seat() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, "한", "").unwrap();
    kb.s.host([preedit]).await;
    assert_eq!(kb.s.guest_received(), [v3_preedit(ime, "한"), done(ime)]);

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    let surface = kb.surface.id;
    let clear = zwp_text_input_v3::Event::PreeditString {
        text: Some("".into()),
        cursor_begin: 0,
        cursor_end: 0,
    };
    let commit = zwp_text_input_v3::Event::CommitString {
        text: Some("한".into()),
    };
    assert_eq!(
        kb.s.guest_received(),
        [
            (ime.id, clear.into()),
            (ime.id, commit.into()),
            done(ime),
            v3_leave(ime, surface)
        ]
    );
    let host = kb.s.host_received();
    assert_eq!(host[..2], released(kb.k));
    assert!(deactivated(ime, &host[2..]), "{host:?}");
}

#[tokio::test]
async fn keyboard_release_keeps_focus_owned_by_another_keyboard() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = kb.another_keyboard().await;

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), released(kb.k));

    // The other keyboard still focuses the surface for the seat.
    let surface = kb.surface;
    kb.s.host([wl_keyboard::evt::leave(other.host, 2, surface.host)])
        .await;
    assert_eq!(
        kb.s.guest_received(),
        [left(other, 2, surface.id), v3_leave(ime, surface.id),]
    );
    assert!(deactivated(ime, &kb.s.host_received()));
}

/// A host sends each keyboard of a seat its focus change in turn, so
/// the first keyboard's `leave` comes while the other still focuses
/// the field. That leave moves the seat's focus off it: the guest
/// commits its preedit before the leave, as with one keyboard (F1),
/// since a client listening on that keyboard drops input-method events
/// for a window it has left. The other keyboard's leave and enter then
/// move nothing.
#[tokio::test]
async fn the_first_keyboard_leave_of_a_seat_commits_the_preedit_before_it() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = kb.another_keyboard().await;
    let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 0, "한", "").unwrap();
    kb.s.host([preedit]).await;
    assert_eq!(kb.s.guest_received(), [v3_preedit(ime, "한"), done(ime)]);

    let (surface, next) = (kb.surface, kb.s.surface().await);
    kb.s.host([
        wl_keyboard::evt::leave(kb.k.host, 2, surface.host),
        wl_keyboard::evt::enter(kb.k.host, 3, next.host, &[]).unwrap(),
        wl_keyboard::evt::leave(other.host, 2, surface.host),
        wl_keyboard::evt::enter(other.host, 3, next.host, &[]).unwrap(),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [
            v3_preedit(ime, ""),
            v3_commit(ime, "한"),
            done(ime),
            left(kb.k, 2, surface.id),
            v3_leave(ime, surface.id),
            entered(kb.k, 3, next.id, &[]),
            modifiers(kb.k, 3, 0),
            v3_enter(ime, next.id),
            left(other, 2, surface.id),
            entered(other, 3, next.id, &[]),
            modifiers(other, 3, 0),
        ]
    );
    assert!(deactivated(ime, &kb.s.host_received()));
}

#[tokio::test]
async fn keyboard_release_after_seat_release_does_not_deactivate_destroyed_host_seat() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.s.guest([wl_seat::req::release(kb.seat.id)]).await;
    assert_eq!(
        kb.s.host_received(),
        [(ime.host_seat, wl_seat::Request::Release {}.into())]
    );

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    let surface = kb.surface.id;
    assert_eq!(kb.s.guest_received(), [v3_leave(ime, surface)]);
    assert_eq!(
        kb.s.host_received(),
        released(kb.k),
        "no deactivate through a released host seat"
    );
}

#[tokio::test]
async fn keyboard_release_does_not_cancel_another_keyboard_ime_repeat() {
    let mut kb = Kb::new().await;
    kb.text_input().await;
    let other = kb.another_keyboard().await;
    // The other keyboard holds Backspace, whose repeat another key's
    // press cancelled.
    kb.peek_on(other, 1, 1, BACKSPACE, PRESSED).await;
    kb.peek_on(other, 2, 2, KEY_X, PRESSED).await;

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), released(kb.k));
    // Reads state no peer can observe: Backspace's cancelled repeat
    // shows only in a confirmation while Backspace is the seat's latest
    // press, which X is.
    let keys = &kb.s.ctx().keys;
    assert!(keys.held(HostId::for_test(other.host), BACKSPACE));
    assert!(keys.backspace_repeat_cancelled(HostId::for_test(other.host)));
}

/// Releasing the keyboard that gave a seat its focus ends the focus of
/// every text input of the seat: each focused one gets its leave, and
/// only the active one's host text input is deactivated. None takes a
/// request then, and the keyboard's delayed leave sends nothing again.
#[tokio::test]
async fn keyboard_release_invalidates_all_same_seat_text_inputs_and_delayed_leave_is_not_duplicate()
{
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let surface = kb.surface;
    // Two more text inputs of the seat, which the guest does not enable:
    // one text input of the connection is enabled at a time.
    let mut others = Vec::new();
    for _ in 0..2 {
        let t = kb.s.text_input(kb.seat.id).await;
        let enter = zwp_text_input_v3::Event::Enter {
            surface: surface.id,
        };
        assert_eq!(kb.s.guest_received(), [(t.id, enter.into())]);
        others.push(t.id);
    }

    kb.s.guest([wl_keyboard::req::release(kb.k.id)]).await;
    let leaves: Vec<_> = [ime.id]
        .iter()
        .chain(&others)
        .map(|&t| {
            (
                t,
                zwp_text_input_v3::Event::Leave {
                    surface: surface.id,
                }
                .into(),
            )
        })
        .collect();
    assert_eq!(kb.s.guest_received(), leaves);
    let host = kb.s.host_received();
    assert_eq!(host[..2], released(kb.k));
    assert!(
        deactivated(ime, &host[2..]),
        "the active host v1 input must be deactivated, and no other: {host:?}"
    );

    // None is focused, so none takes a request.
    for t in [ime.id].iter().chain(&others) {
        kb.s.guest([
            zwp_text_input_v3::req::enable(*t),
            zwp_text_input_v3::req::commit(*t),
        ])
        .await;
        assert_eq!(kb.s.host_received(), []);
        assert_eq!(kb.s.guest_received(), []);
    }
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 2, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), []);
}

// -----------------------------------------------------------------------
// Physical keys the host IME consumes
// -----------------------------------------------------------------------

/// A pressed peek that repeats a held key's refreshes the key's serial
/// and time but keeps its generation: a key pressed since stays the
/// latest, which an empty confirmation recovers.
#[tokio::test]
async fn a_duplicate_pressed_peek_keeps_its_generation() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(1, 100, KEY_SPACE, PRESSED).await;
    kb.peek(2, 110, KEY_A, PRESSED).await;
    kb.peek(3, 200, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), recovered(ime, kb.k, 2, 110, KEY_A));
}

#[tokio::test]
async fn backspace_peek_identity_survives_unrelated_release() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(20, 200, BACKSPACE, PRESSED).await;
    assert_eq!(kb.key(21, KEY_A, RELEASED).await, Route::Host);
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 20, 200, BACKSPACE),
        "an unrelated key event must not replace the Backspace peek identity"
    );
}

/// Whether the key's peeks leave it held shows in what an empty
/// confirmation recovers.
#[tokio::test]
async fn equal_serial_peek_release_uses_timestamp_order() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let key = KEY_T;
    let serial = 18_857;

    kb.peek(serial, 100, key, PRESSED).await;
    kb.peek(serial, 110, key, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a later release must close a press even when ChromeOS reuses its serial"
    );

    kb.peek(serial, 130, key, PRESSED).await;
    kb.peek(serial, 120, key, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, serial, 130, key),
        "an older delayed release must not close the current generation"
    );
    kb.peek(serial, 130, key, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "equal timestamps must remain valid for fast press/release pairs"
    );

    kb.peek(serial + 2, 150, key, PRESSED).await;
    kb.peek(serial + 1, 160, key, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, serial + 2, 150, key),
        "an older serial must stay stale even when delivered with a newer timestamp"
    );
}

#[tokio::test]
async fn ime_repeat_state_is_scoped_to_the_originating_keyboard() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    // A keyboard of another seat, focused on the surface too.
    let (_, other) = kb.s.keyboard().await;
    let surface = kb.surface;
    let other = kb.first_enter(unentered(other), surface).await;

    // The IME consumed Backspace on the first keyboard.
    kb.peek(1, 500, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Host);

    // Its IME repeat must not swallow a Backspace of the other.
    assert_eq!(kb.key_on(other, 2, BACKSPACE, PRESSED).await, Route::Guest);
}

#[tokio::test]
async fn forwarded_backspace_release_is_not_dropped_by_ime_repeat_fallback() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    assert_eq!(kb.key(1, BACKSPACE, PRESSED).await, Route::Guest);

    // Exo later consumes repeat confirmations. The real release still
    // has to reach the guest to close the press/release pair.
    kb.lease(ime).await;
    assert!(kb.leased(kb.k));
    assert_eq!(kb.key(2, BACKSPACE, RELEASED).await, Route::Guest);
}

/// A held Backspace the IME repeats: one the guest got (`forwarded`), or
/// one whose peek, under serial 2, announced it.
async fn repeating_backspace(kb: &mut Kb, ime: Ime, forwarded: bool) {
    if forwarded {
        assert_eq!(kb.key(3, BACKSPACE, PRESSED).await, Route::Guest);
    } else {
        kb.peek(2, 400, BACKSPACE, PRESSED).await;
    }
    kb.lease(ime).await;
    assert!(kb.leased(kb.k));
}

#[tokio::test]
async fn non_backspace_press_cancels_repeat_without_losing_physical_backspace() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    repeating_backspace(&mut kb, ime, true).await;

    assert_eq!(kb.key(1, KEY_A, PRESSED).await, Route::Guest);
    assert!(!kb.leased(kb.k));
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a cancelled repeat must not rearm while Backspace remains physically held"
    );

    // Pressing another key did not falsify the physical Backspace
    // state: its release is the guest's.
    assert_eq!(kb.key(2, BACKSPACE, RELEASED).await, Route::Guest);
    // Backspace release clears the repeat's cancellation: the next one
    // the input method takes is leased.
    kb.peek(4, 4, BACKSPACE, PRESSED).await;
    kb.lease(ime).await;
    assert_eq!(kb.key(4, BACKSPACE, PRESSED).await, Route::Host);
}

#[tokio::test]
async fn backspace_auto_repeat_press_does_not_rearm_cancelled_repeat() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    repeating_backspace(&mut kb, ime, false).await;

    // A newer key cancels the IME repeat while Backspace stays held.
    assert_eq!(kb.key(1, KEY_A, PRESSED).await, Route::Guest);

    // The press the peek announced must not clear the cancellation; only
    // the physical release does.
    assert_eq!(
        kb.key(2, BACKSPACE, PRESSED).await,
        Route::Guest,
        "a repeated Backspace press must not rearm a cancelled IME repeat"
    );
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a repeated Backspace press must not synthesize another key pair"
    );
}

/// One step of [`an_empty_confirmation_recovers_the_latest_eligible_held_key`].
#[derive(Clone, Copy, Debug)]
enum Step {
    Peek {
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
    },
    Press(u32, Route),
    Modifiers(u32),
}

/// The press and release an empty confirmation replays: of `key`, with
/// the `serial` and `time` of its latest peek.
struct Replay {
    key: u32,
    serial: u32,
    time: u32,
}

/// When the host IME consumes a held key, its repeat ticks arrive as
/// empty confirmations, each of which the proxy replays to the guest
/// as a press and release of the latest physical key the guest could
/// type, with the serial and time of its latest peek: synthetic events
/// use the host compositor's serials and time domain.
#[tokio::test]
async fn an_empty_confirmation_recovers_the_latest_eligible_held_key() {
    use Step::*;
    let peek = |serial, time, key, state| Peek {
        serial,
        time,
        key,
        state,
    };
    // ChromeOS may share one serial among the transitions of keys.
    let shared = 18_857;
    let cases: [(&str, Vec<Step>, Option<Replay>); 7] = [
        (
            "a held key",
            vec![peek(1, 1234, BACKSPACE, PRESSED)],
            Some(Replay {
                key: BACKSPACE,
                serial: 1,
                time: 1234,
            }),
        ),
        (
            "a duplicate pressed peek",
            vec![
                peek(1, 100, KEY_SPACE, PRESSED),
                peek(2, 200, KEY_SPACE, PRESSED),
            ],
            Some(Replay {
                key: KEY_SPACE,
                serial: 2,
                time: 200,
            }),
        ),
        (
            "a key under a modifier no accelerator has",
            vec![Modifiers(SHIFT), peek(2, 500, KEY_SPACE, PRESSED)],
            Some(Replay {
                key: KEY_SPACE,
                serial: 2,
                time: 500,
            }),
        ),
        (
            "keys sharing a serial",
            vec![
                peek(shared, 100, BACKSPACE, PRESSED),
                peek(shared, 110, BACKSPACE, RELEASED),
                peek(shared, 120, KEY_T, PRESSED),
                peek(shared, 130, KEY_T, RELEASED),
                peek(shared, 140, BACKSPACE, PRESSED),
            ],
            Some(Replay {
                key: BACKSPACE,
                serial: shared,
                time: 140,
            }),
        ),
        (
            "a released key, while an older one stays held",
            vec![
                peek(1, 100, KEY_A, PRESSED),
                peek(2, 110, KEY_SPACE, PRESSED),
                peek(3, 120, KEY_SPACE, RELEASED),
            ],
            None,
        ),
        (
            "an accelerator, whose modifier then goes",
            vec![
                Modifiers(CONTROL),
                peek(2, 500, KEY_SPACE, PRESSED),
                Modifiers(0),
            ],
            None,
        ),
        (
            "an accelerator whose peek trails its key",
            vec![
                Modifiers(CONTROL),
                Press(2, Route::Host),
                Modifiers(0),
                peek(4, 501, KEY_SPACE, PRESSED),
            ],
            None,
        ),
    ];
    for (case, steps, recovery) in cases {
        let mut kb = Kb::with_accelerators("<Control>space").await;
        let ime = kb.text_input().await;
        for step in steps {
            match step {
                Peek {
                    serial,
                    time,
                    key,
                    state,
                } => kb.peek(serial, time, key, state).await,
                Press(serial, route) => {
                    assert_eq!(kb.key(serial, KEY_SPACE, PRESSED).await, route, "{case}")
                }
                Modifiers(depressed) => kb.modifiers(depressed).await,
            }
        }
        kb.confirm(ime).await;
        let expected = match recovery {
            Some(Replay { key, serial, time }) => recovered(ime, kb.k, serial, time, key).into(),
            None => Vec::new(),
        };
        assert_eq!(kb.s.guest_received(), expected, "{case}");
    }
}

/// ChromeOS has one physical seat, which every guest `wl_seat` stands
/// for, and Exo sends each keyboard of the client its key events in
/// turn, so another seat's keyboard may hear of a press first. Its
/// peek keeps a recovery waiting as one on the held key's keyboard
/// would, and its key press, with the peek's serial, shows the
/// confirmation was Exo's before that press (E13): the held Space is
/// not recovered.
#[tokio::test]
async fn a_press_on_another_seats_keyboard_keeps_a_recovery_waiting() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let (_, other) = kb.s.keyboard().await;
    let other = Keyboard {
        id: other.id,
        host: other.host,
        ext: 0,
    };
    let surface = kb.surface;
    let other = kb.first_enter(other, surface).await;
    kb.peek(700, 1234, KEY_SPACE, PRESSED).await;

    let confirm = zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0);
    kb.s.host([confirm]).await;
    let [(_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] =
        kb.s.host_received()[..]
    else {
        panic!("no recovery waits");
    };
    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(other.ext, 701, 1300, KEY_A, PRESSED),
        wl_keyboard::evt::key(other.host, 701, 1300, KEY_A, PRESSED),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [key_event(other, 701, 1300, KEY_A, PRESSED)]
    );
    assert_eq!(kb.s.host_received(), [acked(other, 701, HANDLED)]);
    kb.s.host([wl_callback::evt::done(callback, 0)]).await;
    assert_eq!(kb.s.guest_received(), []);
}

/// Exo's own detach (H2) may take a held key's release with the input
/// method, so the key's next `peek_key` press is a new press. The
/// guest, which still holds the old press that `wl_keyboard` gave it,
/// gets its release first, with the peek's serial and time, and then
/// the new press.
#[tokio::test]
async fn a_press_after_a_detach_releases_the_old_press_the_guest_holds() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let k = kb.k;
    let press = |serial: u32, time: u32| {
        [
            zcr_extended_keyboard_v1::evt::peek_key(k.ext, serial, time, KEY_A, PRESSED),
            wl_keyboard::evt::key(k.host, serial, time, KEY_A, PRESSED),
        ]
    };

    kb.s.host(press(700, 1234)).await;
    assert_eq!(
        kb.s.guest_received(),
        [key_event(k, 700, 1234, KEY_A, PRESSED)]
    );
    assert_eq!(kb.s.host_received(), [acked(k, 700, HANDLED)]);
    kb.detach(ime).await;

    kb.s.host(press(702, 1400)).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            key_event(k, 702, 1400, KEY_A, RELEASED),
            key_event(k, 702, 1400, KEY_A, PRESSED),
        ]
    );
    assert_eq!(kb.s.host_received(), [acked(k, 702, HANDLED)]);
}

/// A key the proxy recovered (X2') before Exo's own detach (H2) leaves
/// the guest holding nothing, and the key's next press, after the
/// detach, is a new press: it reaches the guest, rather than being
/// taken for a delayed channel of the recovered one.
#[tokio::test]
async fn a_recovered_key_pressed_again_after_a_detach_reaches_the_guest() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(700, 1234, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 700, 1234, KEY_SPACE)
    );
    kb.detach(ime).await;

    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(kb.k.ext, 702, 1400, KEY_SPACE, PRESSED),
        wl_keyboard::evt::key(kb.k.host, 702, 1400, KEY_SPACE, PRESSED),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [key_event(kb.k, 702, 1400, KEY_SPACE, PRESSED)]
    );
    assert_eq!(kb.s.host_received(), [acked(kb.k, 702, HANDLED)]);
}

/// A key held since `wl_keyboard.enter` has no peek, but the detach
/// (H2) may take its release too: while it is held, an empty
/// confirmation may precede its next press, which Exo's keyboard would
/// not announce, and recovers nothing; and its next `peek_key` press is
/// a new press, so the guest, which the enter told the key is held,
/// gets its release before the new press.
#[tokio::test]
async fn a_key_held_since_enter_is_pressed_anew_after_a_detach() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::unfocused(Context::new_for_test(false, false, Vec::new())).await;
    let keys = KEY_A.to_ne_bytes();
    let enter = wl_keyboard::evt::enter(kb.k.host, 1, kb.surface.host, &keys).unwrap();
    kb.s.host([enter]).await;
    kb.k.ext = extended_keyboard_of(kb.k, &kb.s.host_received());
    assert_eq!(
        kb.s.guest_received(),
        [
            entered(kb.k, 1, kb.surface.id, &[KEY_A]),
            modifiers(kb.k, 1, 0)
        ]
    );
    let ime = kb.text_input().await;
    kb.detach(ime).await;
    kb.peek(701, 1300, KEY_SPACE, PRESSED).await;

    let confirm = zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0);
    kb.s.host([confirm]).await;
    assert_eq!(kb.s.host_received(), [], "no recovery waits");
    assert_eq!(kb.s.guest_received(), []);
    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(kb.k.ext, 702, 1400, KEY_A, PRESSED),
        wl_keyboard::evt::key(kb.k.host, 702, 1400, KEY_A, PRESSED),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [
            key_event(kb.k, 702, 1400, KEY_A, RELEASED),
            key_event(kb.k, 702, 1400, KEY_A, PRESSED),
        ]
    );
    assert_eq!(kb.s.host_received(), [acked(kb.k, 702, HANDLED)]);
}

/// The guest never got the press of a held accelerator's key, so a
/// press of that key after a detach (H2) gives it no release; and the
/// new press, without Control, is no longer the accelerator's, so the
/// suppression does not carry over to it.
#[tokio::test]
async fn a_press_after_a_detach_ends_an_accelerators_suppression() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::with_accelerators("<Control>a").await;
    let ime = kb.text_input().await;
    kb.modifiers(CONTROL).await;
    kb.peek(700, 700, KEY_A, PRESSED).await;
    assert_eq!(kb.key(700, KEY_A, PRESSED).await, Route::Host);
    kb.detach(ime).await;
    kb.modifiers(0).await;

    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(kb.k.ext, 702, 1400, KEY_A, PRESSED),
        wl_keyboard::evt::key(kb.k.host, 702, 1400, KEY_A, PRESSED),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [key_event(kb.k, 702, 1400, KEY_A, PRESSED)]
    );
    assert_eq!(kb.s.host_received(), [acked(kb.k, 702, HANDLED)]);
}

/// Once a press after a detach (H2) retired the old press, a late
/// release of the old one, under a serial before the new press's, ends
/// nothing: the guest keeps the new press until its own release.
#[tokio::test]
async fn a_late_release_of_a_press_retired_by_a_detach_is_dropped() {
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(700, 700, KEY_A, PRESSED).await;
    assert_eq!(kb.key(700, KEY_A, PRESSED).await, Route::Guest);
    kb.detach(ime).await;
    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(kb.k.ext, 702, 1400, KEY_A, PRESSED),
        wl_keyboard::evt::key(kb.k.host, 702, 1400, KEY_A, PRESSED),
    ])
    .await;
    assert_eq!(
        kb.s.guest_received(),
        [
            key_event(kb.k, 702, 1400, KEY_A, RELEASED),
            key_event(kb.k, 702, 1400, KEY_A, PRESSED),
        ]
    );
    assert_eq!(kb.s.host_received(), [acked(kb.k, 702, HANDLED)]);

    kb.s.host([
        zcr_extended_keyboard_v1::evt::peek_key(kb.k.ext, 701, 1300, KEY_A, RELEASED),
        wl_keyboard::evt::key(kb.k.host, 701, 1300, KEY_A, RELEASED),
    ])
    .await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), [acked(kb.k, 701, NOT_HANDLED)]);

    kb.peek(703, 1500, KEY_A, RELEASED).await;
    assert_eq!(kb.key(703, KEY_A, RELEASED).await, Route::Guest);
}

#[tokio::test]
async fn held_space_repeats_after_ime_commit_until_physical_release() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;

    // Captured ChromeOS sequence: the host IME consumes the physical
    // Space and includes the first one in its committed string. Further
    // repeat ticks arrive only as empty confirm_preedit events.
    kb.peek(700, 1234, KEY_SPACE, PRESSED).await;
    let preedit = zwp_text_input_v1::evt::preedit_string(ime.host, 1, "가", "").unwrap();
    let commit = zwp_text_input_v1::evt::commit_string(ime.host, 1, "가 ").unwrap();
    kb.s.host([preedit, commit]).await;
    assert_eq!(
        kb.s.guest_received(),
        [
            v3_preedit(ime, "가"),
            done(ime),
            v3_preedit(ime, ""),
            v3_commit(ime, "가 "),
            done(ime),
        ],
        "the first Space commits once"
    );

    for _ in 0..3 {
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            recovered(ime, kb.k, 700, 1234, KEY_SPACE)
        );
    }

    kb.peek(701, 1300, KEY_SPACE, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "a confirmation after the physical release changes nothing"
    );
}

/// The delayed wl_keyboard events of a key that IME recovery replayed
/// stay with the host, until the key's next press.
#[tokio::test]
async fn delayed_keyboard_events_do_not_duplicate_an_ime_recovered_key() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(1, 500, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 1, 500, KEY_SPACE)
    );

    assert_eq!(kb.key(1, KEY_SPACE, PRESSED).await, Route::Host);
    kb.peek(4, 503, KEY_SPACE, RELEASED).await;
    assert_eq!(kb.key(4, KEY_SPACE, RELEASED).await, Route::Host);

    assert_eq!(
        kb.key(5, KEY_SPACE, PRESSED).await,
        Route::Guest,
        "the next physical generation retires the recovery"
    );
}

/// Whatever order the delayed `wl_keyboard` events and the peek
/// release of a key that IME recovery replayed arrive in, none reaches
/// the guest; the key's next physical generation does.
#[tokio::test]
async fn recovery_tombstone_suppresses_delayed_events_in_any_order() {
    #[derive(Clone, Copy, Debug)]
    enum Delayed {
        KeyPress,
        KeyRelease,
        PeekRelease,
    }
    use Delayed::*;

    let orderings = [
        [KeyPress, KeyRelease, PeekRelease],
        [KeyPress, PeekRelease, KeyRelease],
        [PeekRelease, KeyPress, KeyRelease],
    ];
    for ordering in orderings {
        let mut kb = Kb::new().await;
        let ime = kb.text_input().await;
        kb.peek(1, 500, KEY_SPACE, PRESSED).await;
        kb.confirm(ime).await;
        assert_eq!(
            kb.s.guest_received(),
            recovered(ime, kb.k, 1, 500, KEY_SPACE)
        );

        for event in ordering {
            match event {
                KeyPress => {
                    assert_eq!(kb.key(1, KEY_SPACE, PRESSED).await, Route::Host)
                }
                KeyRelease => {
                    assert_eq!(kb.key(2, KEY_SPACE, RELEASED).await, Route::Host)
                }
                PeekRelease => kb.peek(2, 503, KEY_SPACE, RELEASED).await,
            }
            assert_eq!(kb.s.guest_received(), [], "{ordering:?}: {event:?}");
        }

        kb.peek(6, 600, KEY_SPACE, PRESSED).await;
        assert_eq!(
            kb.key(6, KEY_SPACE, PRESSED).await,
            Route::Guest,
            "{ordering:?}"
        );
    }
}

#[tokio::test]
async fn stale_keyboard_press_cannot_steal_a_new_peek_generation() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    kb.peek(1, 500, KEY_SPACE, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 1, 500, KEY_SPACE)
    );
    kb.peek(2, 510, KEY_SPACE, RELEASED).await;
    kb.peek(10, 600, KEY_SPACE, PRESSED).await;

    assert_eq!(
        kb.key(1, KEY_SPACE, PRESSED).await,
        Route::Host,
        "the prior generation's serial must not open the current key"
    );
    assert_eq!(
        kb.key(10, KEY_SPACE, PRESSED).await,
        Route::Guest,
        "the current peek generation must still accept its matching key"
    );
}

#[tokio::test]
async fn delayed_release_closes_only_its_retired_physical_generation() {
    let mut kb = Kb::new().await;
    kb.peek(1, 500, KEY_A, PRESSED).await;
    assert_eq!(kb.key(1, KEY_A, PRESSED).await, Route::Guest);
    kb.peek(2, 510, KEY_A, RELEASED).await;
    kb.peek(10, 600, KEY_A, PRESSED).await;
    assert_eq!(kb.key(10, KEY_A, PRESSED).await, Route::Guest);

    assert_eq!(
        kb.key(2, KEY_A, RELEASED).await,
        Route::Guest,
        "the delayed release must close the retired guest press"
    );
    kb.peek(11, 610, KEY_A, RELEASED).await;
    assert_eq!(
        kb.key(11, KEY_A, RELEASED).await,
        Route::Guest,
        "the current generation kept its press"
    );
}

#[tokio::test]
async fn newer_key_on_another_keyboard_remains_the_causal_watermark_after_release() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = kb.another_keyboard().await;
    kb.peek(1, 500, KEY_SPACE, PRESSED).await;
    // Shift does not repeat.
    kb.peek_on(other, 2, 501, KEY_LEFTSHIFT, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);

    kb.peek_on(other, 3, 502, KEY_LEFTSHIFT, RELEASED).await;
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        [],
        "releasing a newer key must not revive an older held generation"
    );
}

#[tokio::test]
async fn keyboard_retirement_removes_its_watermark_and_recovers_other_held_key() {
    let mut kb = Kb::new().await;
    let ime = kb.text_input().await;
    let other = kb.another_keyboard().await;
    // The first keyboard holds Space; the other is then the latest
    // with a key that does not repeat.
    kb.peek(1, 500, KEY_SPACE, PRESSED).await;
    kb.peek_on(other, 2, 501, KEY_LEFTSHIFT, PRESSED).await;
    kb.confirm(ime).await;
    assert_eq!(kb.s.guest_received(), []);

    // The other keyboard ends, and its key with it.
    kb.s.guest([wl_keyboard::req::release(other.id)]).await;
    assert_eq!(kb.s.guest_received(), []);
    assert_eq!(kb.s.host_received(), released(other));
    kb.confirm(ime).await;
    assert_eq!(
        kb.s.guest_received(),
        recovered(ime, kb.k, 1, 500, KEY_SPACE)
    );
}

/// A seat's peek watermarks end with its focus on the surface: at the
/// keyboard's leave, and as the guest destroys the surface.
#[tokio::test]
async fn a_seats_peek_watermarks_end_with_its_focus() {
    let mut kb = Kb::new().await;
    let surface = kb.surface;
    // Session::ctx: a watermark reaches no peer until a confirmation.
    let watermark = |kb: &mut Kb| {
        let seat = GuestId::for_test(kb.seat.id);
        let surface = GuestId::for_test(surface.id);
        kb.s.ctx().keys.latest_peek_sequence(seat, Some(surface))
    };
    kb.peek(2, 2, KEY_A, PRESSED).await;
    assert!(watermark(&mut kb).is_some());
    kb.s.host([wl_keyboard::evt::leave(kb.k.host, 3, surface.host)])
        .await;
    assert_eq!(kb.s.guest_received(), [left(kb.k, 3, surface.id)]);
    assert_eq!(watermark(&mut kb), None, "at the leave");

    let enter = wl_keyboard::evt::enter(kb.k.host, 4, surface.host, &[]).unwrap();
    kb.s.host([enter]).await;
    assert_eq!(
        kb.s.guest_received(),
        [entered(kb.k, 4, surface.id, &[]), modifiers(kb.k, 4, 0)]
    );
    kb.peek(5, 5, KEY_S, PRESSED).await;
    assert!(watermark(&mut kb).is_some());
    kb.s.guest([wl_surface::req::destroy(surface.id)]).await;
    let destroy = wl_surface::Request::Destroy {};
    assert_eq!(kb.s.host_received(), [(surface.host, destroy.into())]);
    assert_eq!(watermark(&mut kb), None, "as the surface ends");
    kb.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
        .await;
    let delete_id = wl_display::Event::DeleteId { id: surface.id };
    assert_eq!(kb.s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
}

/// The guest releases a keyboard between Exo's `peek_key` of a new press
/// on it and the press, which the proxy then drops undecoded: it is still
/// that press, so the empty confirmation before them was the press's, and
/// the held Backspace is not recovered.
#[tokio::test]
async fn a_press_dropped_with_its_released_keyboard_is_still_the_confirmations() {
    let mut kb = Kb::new().await;
    let other = kb.another_keyboard().await;
    let ime = kb.text_input().await;
    // The input method takes Backspace.
    kb.peek(2, 2, BACKSPACE, PRESSED).await;
    kb.s.host([zcr_extended_text_input_v1::evt::confirm_preedit(ime.ext, 0)])
        .await;
    let host = kb.s.host_received();
    let [(_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback }))] = host[..] else {
        panic!("the recovery waits with no sync: {host:?}");
    };
    kb.peek_on(other, 3, 3, KEY_A, PRESSED).await;
    kb.s.guest([wl_keyboard::req::release(other.id)]).await;
    assert_eq!(kb.s.host_received(), released(other));
    let press = wl_keyboard::evt::key(other.host, 3, 3, KEY_A, PRESSED);
    kb.s.host([press, wl_callback::evt::done(callback, 0)])
        .await;
    assert_eq!(kb.s.guest_received(), [], "no Backspace and no done");
    assert_eq!(kb.s.host_received(), []);
}

mod recovery;
