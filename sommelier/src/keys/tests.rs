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

//! The keyboard core, driven through its step.

use super::*;
use crate::accelerator::parse_accelerator;

/// The default XKB keymap, as a host keyboard of a test sends it.
fn default_keymap() -> xkb::Keymap {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
    xkb::Keymap::new_from_names(&context, "", "", "", "", None, flags).expect("the default keymap")
}

const KEYBOARD: HostId = HostId::for_test(100);
const GUEST_KEYBOARD: GuestId = GuestId::for_test(10);
/// The guest keyboard of [`OTHER`].
const OTHER_GUEST_KEYBOARD: GuestId = GuestId::for_test(11);
const EXTENDED: HostId = HostId::for_test(1000);
/// Another keyboard of the seat, which focuses no surface.
const OTHER: HostId = HostId::for_test(101);
const OTHER_EXTENDED: HostId = HostId::for_test(1001);
const SEAT: GuestId = GuestId::for_test(1);
const SURFACE: GuestId = GuestId::for_test(20);
/// The host surface of [`SURFACE`].
const HOST_SURFACE: HostId = HostId::for_test(200);
const TI: TiId = TiId(GuestId::for_test(40));
const KEY_Q: u32 = 16;
const KEY_A: u32 = 30;
/// Mod1, Alt in the default keymap.
const ALT: u32 = 1 << 3;

/// The facts the core reads: `shortcuts` are bound, and the text input
/// passes the gate if `gate`.
struct TestEnv {
    shortcuts: Vec<(Accelerator, BindingId)>,
    gate: bool,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            shortcuts: Vec::new(),
            gate: true,
        }
    }

    /// `chord` bound to binding 0.
    fn binding(chord: &str) -> Self {
        let chord = parse_accelerator(chord).unwrap();
        Self {
            shortcuts: vec![(chord, BindingId::new(0))],
            ..Self::new()
        }
    }
}

impl Env for TestEnv {
    fn shortcut(&self, chord: &Accelerator) -> Option<BindingId> {
        let mut shortcuts = self.shortcuts.iter();
        shortcuts.find_map(|(bound, binding)| (bound == chord).then_some(*binding))
    }

    fn admits_recovery(&self, _ti: TiId) -> bool {
        self.gate
    }
}

/// A core whose host keeps `accelerators`, with [`KEYBOARD`], then
/// [`OTHER`], on [`SEAT`], their extended keyboards bound and the default
/// keymap loaded, and [`KEYBOARD`] focused on [`SURFACE`].
fn core(accelerators: &str, env: &TestEnv) -> KeyboardCore {
    let accelerators = HostAccelerators::parse(accelerators).unwrap();
    let mut core = KeyboardCore::new(accelerators);
    let inputs = [
        KeyInput::Created {
            keyboard: KEYBOARD,
            guest: GUEST_KEYBOARD,
            seat: SEAT,
        },
        KeyInput::ExtendedBound {
            keyboard: KEYBOARD,
            extended: EXTENDED,
        },
        KeyInput::Keymap {
            keyboard: KEYBOARD,
            keymap: Some(default_keymap()),
        },
        KeyInput::Created {
            keyboard: OTHER,
            guest: OTHER_GUEST_KEYBOARD,
            seat: SEAT,
        },
        KeyInput::ExtendedBound {
            keyboard: OTHER,
            extended: OTHER_EXTENDED,
        },
        KeyInput::Keymap {
            keyboard: OTHER,
            keymap: Some(default_keymap()),
        },
        KeyInput::Enter {
            keyboard: KEYBOARD,
            focus: KeyboardFocus {
                guest_seat: SEAT,
                guest_surface: SURFACE,
                host_surface: HOST_SURFACE,
            },
            keys: Vec::new(),
            events: Vec::new(),
        },
    ];
    for input in inputs {
        core.step(input, env);
    }
    core
}

fn key(key: u32, state: KeyState, serial: u32) -> KeyInput {
    key_on(KEYBOARD, key, state, serial)
}

fn key_on(keyboard: HostId, key: u32, state: KeyState, serial: u32) -> KeyInput {
    KeyInput::Key {
        keyboard,
        serial,
        key,
        state,
    }
}

fn peek(key: u32, state: KeyState, serial: u32) -> KeyInput {
    peek_on(EXTENDED, key, state, serial)
}

fn peek_on(extended: HostId, key: u32, state: KeyState, serial: u32) -> KeyInput {
    KeyInput::Peek {
        extended,
        serial,
        time: serial,
        key,
        state,
    }
}

fn modifiers(depressed: u32) -> KeyInput {
    let modifiers = KeyboardModifiers {
        depressed,
        ..KeyboardModifiers::default()
    };
    KeyInput::Modifiers {
        keyboard: KEYBOARD,
        modifiers,
    }
}

/// An edit of Exo's ends the composition [`SEAT`] showed, as the key the
/// input method took last ends it (E15): that key may be recovered.
fn composition_ended() -> KeyInput {
    KeyInput::Ime(KeyEffect::CompositionEnded { seat: SeatId(SEAT) })
}

fn ack(serial: u32, handled: bool) -> Option<Ack> {
    Some(Ack {
        keyboard: KEYBOARD,
        serial,
        handled,
    })
}

/// A chord a shortcut binds is consumed, press and release, and its
/// binding goes to the subscriber once, at the press; a host accelerator
/// is the host's even if a shortcut binds its chord too.
#[test]
fn a_bound_chord_is_consumed_for_its_subscriber_and_an_accelerator_is_the_hosts() {
    let env = TestEnv::binding("<Alt>q");
    let mut keys = core("", &env);
    keys.step(modifiers(ALT), &env);
    let press = keys.step(key(KEY_Q, KeyState::Pressed, 1), &env);
    let binding = BindingId::new(0);
    let shortcut = Shortcut {
        keyboard: KEYBOARD,
        binding,
    };
    assert_eq!(press.shortcut, Some(shortcut));
    assert_eq!((press.ack, press.deliver), (ack(1, true), false));
    let release = keys.step(key(KEY_Q, KeyState::Released, 2), &env);
    assert_eq!(release.shortcut, None);
    assert_eq!((release.ack, release.deliver), (ack(2, true), false));

    let mut keys = core("<Alt>q", &env);
    keys.step(modifiers(ALT), &env);
    let press = keys.step(key(KEY_Q, KeyState::Pressed, 1), &env);
    assert_eq!(press.shortcut, None);
    assert_eq!((press.ack, press.deliver), (ack(1, false), false));
}

/// A waiting recovery reads every host event the core handles before
/// the event, once: the keyboard's modifiers leave it waiting, and a
/// press of another key recovers the held key first, and is then
/// delivered, so the adapter sends the recovery before the press.
#[test]
fn a_waiting_recovery_reads_each_host_event_before_it_is_handled() {
    let env = TestEnv::new();
    let mut keys = core("", &env);
    keys.step(peek(EVDEV_KEY_BACKSPACE, KeyState::Pressed, 1), &env);
    keys.step(composition_ended(), &env);
    let seat = SeatId(SEAT);
    let confirm = KeyEffect::RecoverHeldKey { ti: TI, seat };
    assert!(keys.step(KeyInput::Ime(confirm), &env).sync);
    assert!(keys.awaits_host_event());

    let shift = keys.step(modifiers(1), &env);
    assert!(shift.recovered.is_none() && shift.deliver);
    assert!(keys.awaits_host_event());

    let press = keys.step(key(KEY_A, KeyState::Pressed, 2), &env);
    let Some(Recovered { ti, pair }) = press.recovered else {
        panic!("the press recovers the held key first");
    };
    assert_eq!((ti, pair.len()), (TI, 2));
    assert!(press.deliver);
    assert!(!keys.awaits_host_event());
}

/// An empty confirmation recovers only a held key that ended a composition
/// the guest showed: the latest key held when the composition ended, for
/// the rest of its hold. A key the input method took with nothing to end,
/// an older held key and a new press of the key are not recovered, and a
/// key the guest got is recovered with no pair.
#[test]
fn only_a_held_key_that_ended_a_composition_is_recovered() {
    let env = TestEnv::new();
    let seat = SeatId(SEAT);
    let recovers = |keys: &mut KeyboardCore| {
        let confirm = KeyEffect::RecoverHeldKey { ti: TI, seat };
        let waits = keys.step(KeyInput::Ime(confirm), &env).sync;
        if waits {
            keys.step(KeyInput::HostEvent, &env);
        }
        waits
    };
    let composed = |keys: &KeyboardCore, key| keys.peek(KEYBOARD, key).is_some_and(|p| p.composed);
    let mut keys = core("", &env);
    keys.step(peek(KEY_A, KeyState::Pressed, 1), &env);
    assert!(!recovers(&mut keys));
    keys.step(composition_ended(), &env);
    assert!(recovers(&mut keys) && recovers(&mut keys));
    // A new press of the key ended nothing yet.
    keys.step(peek(KEY_A, KeyState::Released, 2), &env);
    keys.step(peek(KEY_A, KeyState::Pressed, 3), &env);
    assert!(!recovers(&mut keys));
    // Only the latest key the input method took ended it.
    keys.step(peek(KEY_Q, KeyState::Pressed, 4), &env);
    keys.step(composition_ended(), &env);
    assert!(composed(&keys, KEY_Q) && !composed(&keys, KEY_A));
    // A key the guest got.
    let mut keys = core("", &env);
    keys.step(peek(KEY_A, KeyState::Pressed, 1), &env);
    keys.step(key(KEY_A, KeyState::Pressed, 1), &env);
    keys.step(composition_ended(), &env);
    let confirm = KeyEffect::RecoverHeldKey { ti: TI, seat };
    assert!(keys.step(KeyInput::Ime(confirm), &env).sync);
    let recovered = keys.step(KeyInput::HostEvent, &env).recovered;
    assert!(recovered.is_some_and(|recovered| recovered.pair.is_empty()));
}

/// Whoever delivers a key generation to the guest first owns it, and a
/// shortcut never acts on a key the guest already received: a chord the
/// IME recovered, which a binding then binds, as a reload of the bindings
/// between its peek and its `wl_keyboard.key` does, stays the recovery's.
/// A peeked chord a shortcut binds is never a recovery candidate.
#[test]
fn a_chord_the_guest_already_received_stays_the_guests() {
    let unbound = TestEnv::new();
    let mut keys = core("", &unbound);
    keys.step(modifiers(ALT), &unbound);
    keys.step(peek(KEY_Q, KeyState::Pressed, 1), &unbound);
    keys.step(composition_ended(), &unbound);
    let seat = SeatId(SEAT);
    let confirm = KeyEffect::RecoverHeldKey { ti: TI, seat };
    keys.step(KeyInput::Ime(confirm), &unbound);
    let recovered = keys.step(KeyInput::HostEvent, &unbound).recovered;
    assert!(recovered.is_some_and(|recovered| recovered.pair.len() == 2));

    let bound = TestEnv::binding("<Alt>q");
    let press = keys.step(key(KEY_Q, KeyState::Pressed, 1), &bound);
    assert_eq!(press.shortcut, None);
    assert!(
        !press.deliver,
        "the physical channel must not duplicate the guest press"
    );
    assert_eq!(
        keys.owner(KEYBOARD, KEY_Q),
        Some(GuestKeyOwner::ImeRecovery)
    );
    // The recovered pair is already balanced.
    let release = keys.step(key(KEY_Q, KeyState::Released, 2), &bound);
    assert!(release.shortcut.is_none() && !release.deliver);
}

/// [`KEYBOARD`] holds Backspace, which the input method took to empty
/// its composition and the IME core leased to [`TI`].
fn leased_backspace(env: &TestEnv) -> KeyboardCore {
    let mut keys = core("", env);
    keys.step(peek(EVDEV_KEY_BACKSPACE, KeyState::Pressed, 1), env);
    keys.step(composition_ended(), env);
    let lease = KeyEffect::Lease {
        ti: TI,
        seat: SeatId(SEAT),
        outcome: LeaseOutcome::ArmIfBackspaceHeld,
    };
    keys.step(KeyInput::Ime(lease), env);
    assert_eq!(keys.backspace_lease(KEYBOARD), Some(TI));
    keys
}

/// An empty confirmation of [`TI`], which waits for the host's next event.
fn confirm(keys: &mut KeyboardCore, env: &TestEnv) {
    let seat = SeatId(SEAT);
    let confirm = KeyEffect::RecoverHeldKey { ti: TI, seat };
    assert!(keys.step(KeyInput::Ime(confirm), env).sync);
}

/// A Backspace release on any keyboard of the seat, on either channel,
/// ends the seat's lease, and so does a move of the seat's focus.
#[test]
fn a_backspace_release_or_a_focus_change_ends_the_seats_lease() {
    let env = TestEnv::new();
    let release = |keys: &mut KeyboardCore, peeked: bool| {
        let (press, release) = match peeked {
            true => (
                peek_on(OTHER_EXTENDED, EVDEV_KEY_BACKSPACE, KeyState::Pressed, 5),
                peek_on(OTHER_EXTENDED, EVDEV_KEY_BACKSPACE, KeyState::Released, 6),
            ),
            false => (
                key_on(OTHER, EVDEV_KEY_BACKSPACE, KeyState::Pressed, 5),
                key_on(OTHER, EVDEV_KEY_BACKSPACE, KeyState::Released, 6),
            ),
        };
        keys.step(press, &env);
        assert_eq!(keys.backspace_lease(KEYBOARD), Some(TI), "peeked: {peeked}");
        keys.step(release, &env);
        assert_eq!(keys.backspace_lease(KEYBOARD), None, "peeked: {peeked}");
        assert!(
            keys.backspace_repeat_cancelled(KEYBOARD),
            "peeked: {peeked}"
        );
    };
    for peeked in [false, true] {
        release(&mut leased_backspace(&env), peeked);
    }

    // A focus move retires the keyboard that left, and the keys it held,
    // so hold the Backspace on the seat's other keyboard, which focuses
    // nothing and stays.
    let mut keys = core("", &env);
    keys.step(
        peek_on(OTHER_EXTENDED, EVDEV_KEY_BACKSPACE, KeyState::Pressed, 1),
        &env,
    );
    let lease = KeyEffect::Lease {
        ti: TI,
        seat: SeatId(SEAT),
        outcome: LeaseOutcome::ArmIfBackspaceHeld,
    };
    keys.step(KeyInput::Ime(lease), &env);
    assert_eq!(keys.backspace_lease(OTHER), Some(TI));
    let leave = KeyInput::Leave {
        keyboard: KEYBOARD,
        surface: HOST_SURFACE,
        events: Vec::new(),
    };
    keys.step(leave, &env);
    assert_eq!(keys.backspace_lease(OTHER), None);
    assert!(keys.backspace_repeat_cancelled(OTHER));
}

/// A move of a seat's focus ends its peek watermarks.
#[test]
fn a_focus_change_ends_the_seats_peek_watermarks() {
    let env = TestEnv::new();
    let mut keys = core("", &env);
    keys.step(peek(KEY_A, KeyState::Pressed, 1), &env);
    assert!(keys.latest_peek_sequence(SEAT, Some(SURFACE)).is_some());
    let leave = KeyInput::Leave {
        keyboard: KEYBOARD,
        surface: HOST_SURFACE,
        events: Vec::new(),
    };
    keys.step(leave, &env);
    assert_eq!(keys.latest_peek_sequence(SEAT, Some(SURFACE)), None);
}

/// A recovery the host event decides sends nothing once the text input
/// no longer passes the gate, and ends the lease.
#[test]
fn a_recovery_sends_nothing_once_its_text_input_fails_the_gate() {
    let open = TestEnv::new();
    let mut keys = leased_backspace(&open);
    confirm(&mut keys, &open);
    let closed = TestEnv {
        gate: false,
        ..TestEnv::new()
    };
    let fx = keys.step(KeyInput::HostEvent, &closed);
    assert!(fx.recovered.is_none());
    assert!(!keys.awaits_host_event());
    assert_eq!(keys.backspace_lease(KEYBOARD), None);
}

/// A key the proxy drops undecoded, of a keyboard the guest released, is
/// the press a peek on that keyboard announced: the confirmation was that
/// press's, and nothing is recovered. Another keyboard's is any other
/// event.
#[test]
fn an_unhandled_key_of_the_announcing_keyboard_is_its_press() {
    let env = TestEnv::new();
    for (dropped_on, recovered) in [(OTHER, false), (KEYBOARD, true)] {
        let mut keys = leased_backspace(&env);
        confirm(&mut keys, &env);
        keys.step(KeyInput::UnhandledModifiers, &env);
        assert!(keys.awaits_host_event(), "modifiers leave it waiting");
        keys.step(peek_on(OTHER_EXTENDED, KEY_A, KeyState::Pressed, 7), &env);
        assert!(keys.awaits_host_event());
        let keyboard = dropped_on;
        let fx = keys.step(KeyInput::UnhandledKey { keyboard }, &env);
        assert_eq!(fx.recovered.is_some(), recovered, "{dropped_on:?}");
        assert!(!keys.awaits_host_event());
    }
}
