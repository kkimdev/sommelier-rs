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

//! The keyboard core: what the proxy knows of each host keyboard and its
//! keys, as a sans-IO state machine, the IME core's twin.
//! [`KeyboardCore::step`] takes one [`KeyInput`] and returns the
//! [`KeyEffects`] the adapter (`crate::handler::keyboard`) applies; it
//! reads the facts it does not own (the keyboard focus, the shortcut
//! bindings, the IME's gate) through [`Env`].
//!
//! It owns one record per host keyboard ([`Keyboard`]), each key's
//! generations with their delivery, the IME's Backspace repeat lease and
//! the peek watermarks (`generations`), the recovery of an empty IME
//! confirmation's held key (`recovery`), and the host accelerators,
//! whose chords the host keeps. `docs/KEYBOARD.md` describes the machine.
//!
//! The core and the IME core talk only through typed messages: the IME
//! core's requests reach it as [`KeyInput::Ime`], and it hands the IME core
//! [`ime::Input`]s ([`KeyEffects::ime`], [`KeyEffects::recovered`]).

mod generations;
mod keyboards;
mod recovery;
#[cfg(test)]
mod tests;

use xkbcommon::xkb;

use crate::accelerator::{Accelerator, BindingId, HostAccelerators};
use crate::ime::{self, KeyEffect, LeaseOutcome, SeatId, SurfaceId, TiId};
use crate::keymap::{KeyboardModifiers, Keymap};
use crate::protocols::wayland::wl_keyboard::{self, KEY_STATE_PRESSED, KEY_STATE_RELEASED};
use crate::state::{HostId, KeyboardFocusUpdate};
use crate::wire::Outgoing;
use generations::{
    ChordClass, KeyAdmission, KeyGenerationRegistry, LeaseInput, PeekInput, PhysicalKey,
};
use recovery::{HostSignal, PendingRecovery};

pub(crate) use generations::EVDEV_KEY_BACKSPACE;
#[cfg(test)]
pub(crate) use generations::{GuestKeyOwner, PeekKeyProvenance};
pub(crate) use keyboards::{Keyboard, Keyboards};

/// One message the keyboard core handles. Every trigger of a change of
/// its state is one of these.
pub(crate) enum KeyInput {
    // A keyboard's own life.
    /// `wl_seat.get_keyboard` of guest seat `seat` made guest keyboard
    /// `guest`, whose host keyboard `keyboard` its forward mapped.
    Created {
        keyboard: HostId,
        guest: u32,
        seat: u32,
    },
    /// The adapter bound `keyboard`'s `zcr_extended_keyboard_v1`,
    /// `extended`, through which its keys are acknowledged and peeked.
    ExtendedBound { keyboard: HostId, extended: HostId },
    /// `wl_keyboard.keymap`: the keymap the adapter compiled, `None` if
    /// the proxy cannot use it. Held keys and the modifiers stay (INVARIANTS
    /// "A keymap ends no held key").
    Keymap {
        keyboard: HostId,
        keymap: Option<xkb::Keymap>,
    },
    /// `keyboard` ended: its record and its keys end, and so does a
    /// recovery of a key it holds. Its extended keyboard comes back in
    /// [`KeyEffects::ended`].
    Ended(HostId),

    // Host events a waiting recovery reads (`HostSignal`), each decoded
    // once, by the dispatcher.
    /// `wl_keyboard.modifiers`.
    Modifiers {
        keyboard: HostId,
        modifiers: KeyboardModifiers,
    },
    /// `wl_keyboard.key`.
    Key {
        keyboard: HostId,
        serial: u32,
        key: u32,
        state: u32,
    },
    /// `zcr_extended_keyboard_v1.peek_key` of extended keyboard
    /// `extended`.
    Peek {
        extended: HostId,
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
    },
    /// `zwp_text_input_v1.leave` of the host text input of guest text
    /// input `ti`, if it has one. The IME core hears of it after.
    TextInputLeave(Option<TiId>),
    /// A `wl_keyboard.key` of `keyboard`, which the guest has released, so
    /// that the proxy drops it unhandled, undecoded. Exo sends a key right
    /// after its peek, so it is the press a peek on that keyboard announced.
    UnhandledKey { keyboard: HostId },
    /// A `wl_keyboard.modifiers` the proxy drops unhandled, as
    /// [`Self::UnhandledKey`].
    UnhandledModifiers,
    /// Any other host event, before the proxy handles it. The adapter
    /// sends these three only while a recovery waits
    /// ([`KeyboardCore::awaits_host_event`]).
    HostEvent,

    // The keyboard focus, and what ends.
    /// The keyboard focus registry moved (`update`): each keyboard it
    /// retired loses its keys; then `enter`'s keyboard holds the keys its
    /// `wl_keyboard.enter` listed; then each seat whose focus changed loses
    /// its peek watermarks and its Backspace leases, and the IME core
    /// hears of the change (F1), with the guest's copy of the keyboard
    /// events that moved it, `keyboard`.
    Focus {
        update: KeyboardFocusUpdate,
        enter: Option<(HostId, Vec<u32>)>,
        keyboard: Vec<Outgoing>,
    },
    /// The guest's text input `ti` ended: a recovery for it ends.
    TextInputEnded(TiId),

    /// A request of the IME core.
    Ime(KeyEffect),
}

/// What one step asks the adapter to do, in the order of the fields.
#[derive(Debug, Default)]
pub(crate) struct KeyEffects {
    /// X2': the waiting recovery recovered its held key at this input's
    /// host event. It precedes everything else of the input, the event's
    /// own delivery included.
    pub(crate) recovered: Option<Recovered>,
    /// The keyboard consumed the chord of a shortcut, whose binding goes to
    /// its subscriber.
    pub(crate) shortcut: Option<Shortcut>,
    /// The `ack_key` of the `wl_keyboard.key` handled, if it is
    /// acknowledged.
    pub(crate) ack: Option<Ack>,
    /// A recovery waits: sync with the host
    /// (`InternalCallback::KeyRecovery`), whose `done`, a host event,
    /// decides it at the latest.
    pub(crate) sync: bool,
    /// The extended keyboard of an ended keyboard, which the adapter
    /// destroys.
    pub(crate) ended: Option<HostId>,
    /// Whether the guest gets the host event handled: a `wl_keyboard.key`
    /// or `modifiers`. A recovery before it ([`Self::recovered`]) must
    /// reach the guest first.
    pub(crate) deliver: bool,
    /// Guest `wl_keyboard` events, after the event's own delivery.
    pub(crate) to_guest: Vec<Outgoing>,
    /// Inputs of the IME core, in order, after `to_guest`.
    pub(crate) ime: Vec<ime::Input>,
}

/// X2': the held key the core recovered for text input `ti`'s empty
/// confirmation: its press and release, none if another channel already
/// delivered it. The IME core closes the confirmation after them
/// (`ime::Input::HeldKeyRecovered`).
#[derive(Debug)]
pub(crate) struct Recovered {
    pub(crate) ti: TiId,
    pub(crate) pair: Vec<Outgoing>,
}

/// A consumed shortcut chord: `keyboard` pressed the chord of `binding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shortcut {
    pub(crate) keyboard: HostId,
    pub(crate) binding: BindingId,
}

/// A `zcr_extended_keyboard_v1.ack_key` of `keyboard`'s key under `serial`:
/// whether the guest handles it, or the host keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ack {
    pub(crate) keyboard: HostId,
    pub(crate) serial: u32,
    pub(crate) handled: bool,
}

/// Read-only facts the core needs and does not own. The adapter answers
/// them from the keyboard focus, the shortcut subscriber and the IME core;
/// tests use maps.
pub(crate) trait Env {
    /// The guest surface the keyboards of guest seat `seat` focus.
    fn seat_focus(&self, seat: u32) -> Option<u32>;
    /// Whether host keyboard `keyboard` focuses guest surface `surface`.
    fn focuses(&self, keyboard: HostId, surface: u32) -> bool;
    /// The shortcut binding of `chord`, if a subscriber binds it now.
    fn shortcut(&self, chord: &Accelerator) -> Option<BindingId>;
    /// X2': whether text input `ti` still passes the IME core's gate
    /// (`ime::ImeCore::admits_recovery`).
    fn admits_recovery(&self, ti: TiId) -> bool;
}

/// Who owns a key, decided from its chord ([`KeyboardCore::classify`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyClass {
    /// Listed in `SOMMELIER_ACCELERATORS`: the host runs the accelerator.
    HostAccelerator,
    /// Bound to a shortcut: Sommelier owns the chord, and its subscriber
    /// runs the binding.
    Shortcut(BindingId),
    /// Ordinary guest input.
    Guest,
}

impl KeyClass {
    /// The class the key generations know, without the binding.
    fn chord(self) -> ChordClass {
        match self {
            Self::HostAccelerator => ChordClass::HostAccelerator,
            Self::Shortcut(_) => ChordClass::Shortcut,
            Self::Guest => ChordClass::Guest,
        }
    }
}

/// The keyboards of one connection, and their keys.
#[derive(Debug)]
pub(crate) struct KeyboardCore {
    /// The chords the host keeps (`SOMMELIER_ACCELERATORS`).
    accelerators: HostAccelerators,
    /// What the keyboards' keymaps are compiled in.
    keymaps: crate::keymap::Compiler,
    keyboards: Keyboards,
    generations: KeyGenerationRegistry,
    /// X2': an empty IME confirmation's recovery of a held key, which
    /// waits for the host's next event to show whether the confirmation
    /// was an autorepeat or Exo's before a new press (E13).
    recovery: Option<PendingRecovery>,
}

impl KeyboardCore {
    /// A connection's keyboard core, whose host keeps `accelerators`.
    pub(crate) fn new(accelerators: HostAccelerators) -> Self {
        Self {
            accelerators,
            keymaps: crate::keymap::Compiler::default(),
            keyboards: Keyboards::default(),
            generations: KeyGenerationRegistry::default(),
            recovery: None,
        }
    }

    /// What a host keyboard's keymap is compiled in, before the core
    /// takes it ([`KeyInput::Keymap`]).
    pub(crate) fn keymaps(&self) -> &crate::keymap::Compiler {
        &self.keymaps
    }

    /// The host keyboards' records.
    pub(crate) fn keyboards(&self) -> &Keyboards {
        &self.keyboards
    }

    /// Whether a recovery waits for the host's next event, which the
    /// adapter then hands the core first ([`KeyInput::HostEvent`]).
    pub(crate) fn awaits_host_event(&self) -> bool {
        self.recovery.is_some()
    }

    /// Handle `input`, reading what the core does not own from `env`.
    pub(crate) fn step(&mut self, input: KeyInput, env: &impl Env) -> KeyEffects {
        let mut fx = KeyEffects::default();
        if let Some(signal) = self.signal(&input) {
            self.decide(signal, env, &mut fx);
        }
        match input {
            KeyInput::Created {
                keyboard,
                guest,
                seat,
            } => self.keyboards.insert(keyboard, Keyboard::new(guest, seat)),
            KeyInput::ExtendedBound { keyboard, extended } => {
                if let Some(record) = self.keyboards.get_mut(keyboard) {
                    record.extended = Some(extended);
                }
            }
            KeyInput::Keymap { keyboard, keymap } => {
                if let Some(record) = self.keyboards.get_mut(keyboard) {
                    // Exo sends only the keymap on a layout change, so the
                    // modifiers held and locked before it still apply.
                    record.keymap = keymap.map(|keymap| Keymap::new(&keymap, record.modifiers));
                    if record.keymap.is_some() {
                        log::info!("XKB keymap loaded successfully");
                    }
                }
            }
            KeyInput::Ended(keyboard) => self.ended(keyboard, &mut fx),
            KeyInput::Modifiers {
                keyboard,
                modifiers,
            } => {
                // Kept even before the keyboard has a keymap: Exo sends
                // them only when they change, so none may follow the
                // keymap.
                if let Some(record) = self.keyboards.get_mut(keyboard) {
                    record.modifiers = modifiers;
                    if let Some(keymap) = &mut record.keymap {
                        keymap.set_modifiers(modifiers);
                    }
                }
                fx.deliver = true;
            }
            KeyInput::Key {
                keyboard,
                serial,
                key,
                state,
            } => self.key(keyboard, serial, key, state, env, &mut fx),
            KeyInput::Peek {
                extended,
                serial,
                time,
                key,
                state,
            } => self.peek_key(extended, serial, time, key, state, env, &mut fx),
            KeyInput::TextInputLeave(_)
            | KeyInput::UnhandledKey { .. }
            | KeyInput::UnhandledModifiers
            | KeyInput::HostEvent => {}
            KeyInput::Focus {
                update,
                enter,
                keyboard,
            } => self.focus(update, enter, keyboard, &mut fx),
            KeyInput::TextInputEnded(ti) => {
                if self.recovery.is_some_and(|pending| pending.ti == ti) {
                    self.recovery = None;
                }
            }
            KeyInput::Ime(effect) => self.ime(effect, env, &mut fx),
        }
        fx
    }

    /// The host event `input` is, as a waiting recovery reads it, if it is
    /// one. A peek's keyboard and whether it is a new press are read
    /// before the peek moves its key.
    fn signal(&self, input: &KeyInput) -> Option<HostSignal> {
        let signal = match *input {
            KeyInput::Modifiers { .. } | KeyInput::UnhandledModifiers => HostSignal::Modifiers,
            KeyInput::UnhandledKey { keyboard } => HostSignal::UnhandledKey { keyboard },
            KeyInput::Key {
                keyboard,
                serial,
                key,
                state: KEY_STATE_PRESSED,
                ..
            } => HostSignal::KeyPress {
                keyboard,
                key,
                serial,
            },
            KeyInput::Peek {
                extended,
                serial,
                key,
                state: KEY_STATE_PRESSED,
                ..
            } => {
                // Any keyboard of the connection: Exo has one seat, which
                // sends each keyboard of every guest seat the press in
                // turn. A key held only across a detach is pressed anew
                // too.
                let keyboard = self.keyboards.of_extended(extended);
                let new = keyboard
                    .is_some_and(|keyboard| self.generations.peek_press_is_new(keyboard, key));
                HostSignal::PeekPress {
                    keyboard,
                    key,
                    serial,
                    new,
                }
            }
            KeyInput::TextInputLeave(ti) => HostSignal::TextInputLeave { ti },
            KeyInput::Key { .. } | KeyInput::Peek { .. } | KeyInput::HostEvent => HostSignal::Other,
            KeyInput::Created { .. }
            | KeyInput::ExtendedBound { .. }
            | KeyInput::Keymap { .. }
            | KeyInput::Ended(_)
            | KeyInput::Focus { .. }
            | KeyInput::TextInputEnded(_)
            | KeyInput::Ime(_) => return None,
        };
        Some(signal)
    }

    /// Decide who owns a key from its chord alone.
    ///
    /// Both key channels (`wl_keyboard.key` and `peek_key`) ask this one
    /// classification. The subscriber binds a chord only while it is on,
    /// and the bindings' validator rejects a chord that is both a host
    /// accelerator and a binding; if one slipped through, the host keeps
    /// its accelerator.
    pub(crate) fn classify(&self, keyboard: HostId, key: u32, env: &impl Env) -> KeyClass {
        let keymap = self
            .keyboards
            .get(keyboard)
            .and_then(|record| record.keymap.as_ref());
        // A keyboard with no keymap yet forms no chord.
        let Some(chord) = keymap.and_then(|keymap| keymap.chord(key)) else {
            return KeyClass::Guest;
        };
        let class = if self.accelerators.contains(&chord) {
            KeyClass::HostAccelerator
        } else if let Some(binding) = env.shortcut(&chord) {
            KeyClass::Shortcut(binding)
        } else {
            KeyClass::Guest
        };
        log::trace!(
            "key class: keyboard={} key={} chord={:?} class={:?}",
            keyboard.0,
            key,
            chord,
            class
        );
        class
    }

    /// `wl_keyboard.key`: its acknowledgement, whether the guest gets it,
    /// and what it does to the lease, a shortcut and the IME.
    fn key(
        &mut self,
        keyboard: HostId,
        serial: u32,
        key: u32,
        state: u32,
        env: &impl Env,
        fx: &mut KeyEffects,
    ) {
        let seat = self.keyboards.seat(keyboard);
        // Read before the physical transition: a press that starts a new
        // generation of Backspace holds no lease.
        let backspace_leased =
            key == EVDEV_KEY_BACKSPACE && self.generations.backspace_lease(keyboard).is_some();
        if let KeyAdmission::Settled(verdict) =
            self.generations.admit_key(keyboard, key, state, serial)
        {
            fx.ack = Some(Ack {
                keyboard,
                serial,
                handled: verdict.handled(),
            });
            fx.deliver = verdict.forward();
            return;
        }
        let (event, class) = match state {
            KEY_STATE_PRESSED => {
                if key != EVDEV_KEY_BACKSPACE {
                    self.lease(seat, LeaseInput::OtherPress { keyboard });
                }
                let class = self.classify(keyboard, key, env);
                let chord = class.chord();
                let press = PhysicalKey::Press {
                    chord,
                    backspace_leased,
                };
                (press, class)
            }
            KEY_STATE_RELEASED => (PhysicalKey::Release, KeyClass::Guest),
            // Exo sends no repeats, and `wl_keyboard` has none before
            // version 10, beyond the version 5 the proxy binds `wl_seat` at.
            other => {
                log::warn!("on_key: received unknown key state {}, ignoring", other);
                return;
            }
        };
        let verdict = self.generations.transition_physical(keyboard, key, event);
        if let (generations::KeyVerdict::Shortcut, KeyClass::Shortcut(binding)) = (verdict, class) {
            // The subscriber owns every matched chord, so the chord is
            // consumed whether or not it can act now; a skip must not leak
            // the key to the guest.
            fx.shortcut = Some(Shortcut { keyboard, binding });
        }
        if state == KEY_STATE_RELEASED && key == EVDEV_KEY_BACKSPACE {
            self.lease(seat, LeaseInput::BackspaceRelease);
        }
        fx.ack = Some(Ack {
            keyboard,
            serial,
            handled: verdict.handled(),
        });
        fx.deliver = verdict.forward();
        // A key of a keyboard with no keymap yet may be anything.
        let modifier = self
            .keyboards
            .get(keyboard)
            .and_then(|record| record.keymap.as_ref())
            .is_some_and(|keymap| keymap.is_modifier(key));
        if let (true, KEY_STATE_PRESSED, false, Some(seat)) = (fx.deliver, state, modifier, seat) {
            // The guest may edit its text where Exo does not see.
            fx.ime.push(ime::Input::GuestKey(SeatId(seat)));
        }
    }

    /// `zcr_extended_keyboard_v1.peek_key`: the key's generation, the
    /// release the guest is owed, the seat's watermark and the lease.
    #[allow(clippy::too_many_arguments)]
    fn peek_key(
        &mut self,
        extended: HostId,
        serial: u32,
        time: u32,
        key: u32,
        state: u32,
        env: &impl Env,
        fx: &mut KeyEffects,
    ) {
        let Some(keyboard) = self.keyboards.of_extended(extended) else {
            log::warn!(
                "peek_key: no host keyboard mapping for extended keyboard {}",
                extended.0
            );
            return;
        };
        let seat = self.keyboards.seat(keyboard);
        // Only guest input may be recovered for an empty IME confirmation.
        // Host accelerators and shortcut chords belong to the host and to
        // Sommelier: if a recovery claimed one first, the wl_keyboard press
        // that follows would find the guest already owning it.
        let class = (state == KEY_STATE_PRESSED).then(|| self.classify(keyboard, key, env));
        let input = match state {
            KEY_STATE_PRESSED => PeekInput::Press {
                serial,
                time,
                guest_input: class == Some(KeyClass::Guest),
            },
            KEY_STATE_RELEASED => PeekInput::Release { serial, time },
            // Exo sends no repeats.
            other => {
                log::warn!("peek_key: received unknown key state {}, ignoring", other);
                return;
            }
        };
        let outcome = self.generations.peek_key(keyboard, key, input);
        if outcome.owes_release {
            // A peek the input method's own detach made stale (H2)
            // describes a press whose release may have died with it, so a
            // press peek of the key is its next press, of a generation of
            // its own. The guest gets the old press's release first, under
            // the serial and time of the peek, which announces the new
            // press.
            if let Some(record) = self.keyboards.get(keyboard) {
                let release =
                    wl_keyboard::evt::key(record.guest, serial, time, key, KEY_STATE_RELEASED);
                fx.to_guest.push(release);
            }
        }
        if let Some(sequence) = outcome.numbered {
            if let Some(KeyClass::Shortcut(binding)) = class {
                log::debug!(
                    "peek_key: shortcut chord {:?} on keyboard {}; it runs only on the \
                     matching wl_keyboard.key, so if the host IME consumes it the chord \
                     is neither run nor delivered",
                    binding,
                    keyboard.0
                );
            }
            if let Some(seat) = seat {
                let focused = env
                    .seat_focus(seat)
                    .filter(|&surface| env.focuses(keyboard, surface));
                self.generations
                    .record_latest_peek(seat, focused, keyboard, sequence);
            }
        }
        match input {
            // The press of another key ends the Backspace repeat lease.
            PeekInput::Press { .. } if key != EVDEV_KEY_BACKSPACE => {
                self.lease(seat, LeaseInput::OtherPress { keyboard });
            }
            // A Backspace release ends the Backspace repeat lease.
            PeekInput::Release { .. } if !outcome.ignored && key == EVDEV_KEY_BACKSPACE => {
                self.lease(seat, LeaseInput::BackspaceRelease);
            }
            _ => {}
        }
    }

    /// The keyboard focus moved; see [`KeyInput::Focus`].
    fn focus(
        &mut self,
        update: KeyboardFocusUpdate,
        enter: Option<(HostId, Vec<u32>)>,
        mut keyboard: Vec<Outgoing>,
        fx: &mut KeyEffects,
    ) {
        for &retired in &update.retired_keyboards {
            self.generations.clear_keyboard(retired);
        }
        if let Some((entered, keys)) = enter {
            self.generations.install_enter_snapshot(entered, keys);
        }
        let changes = update
            .seat_changes
            .iter()
            .filter(|change| change.previous_surface != change.current_surface);
        // Keyboard events come from one keyboard, which belongs to one
        // seat, so at most one seat's focus changes with them.
        debug_assert!(
            keyboard.is_empty() || changes.clone().count() <= 1,
            "keyboard events moved the focus of several seats: {:?}",
            update.seat_changes
        );
        for change in changes {
            self.generations
                .clear_peek_watermarks_for_seat(change.guest_seat);
            self.lease(Some(change.guest_seat), LeaseInput::FocusLost);
            fx.ime.push(ime::Input::FocusChanged {
                seat: SeatId(change.guest_seat),
                surface: change.current_surface.map(SurfaceId),
                keyboard: std::mem::take(&mut keyboard),
            });
        }
        fx.to_guest.extend(keyboard);
    }

    /// `keyboard` ended; see [`KeyInput::Ended`]. A re-created keyboard
    /// starts from a clean slate: an accelerator's key suppressed under
    /// the old one would have its release swallowed on the new one, and
    /// the old modifiers would classify its keys before its first
    /// `wl_keyboard.modifiers`.
    fn ended(&mut self, keyboard: HostId, fx: &mut KeyEffects) {
        if self
            .recovery
            .is_some_and(|pending| pending.held.host_keyboard_id == keyboard)
        {
            self.recovery = None;
        }
        let record = self.keyboards.remove(keyboard);
        self.generations.clear_keyboard(keyboard);
        fx.ended = record.and_then(|record| record.extended);
    }

    /// A request of the IME core.
    fn ime(&mut self, effect: KeyEffect, env: &impl Env, fx: &mut KeyEffects) {
        match effect {
            KeyEffect::Lease { ti, seat, outcome } => {
                self.lease(Some(seat.0), LeaseInput::Ime { ti, outcome });
            }
            KeyEffect::RecoverHeldKey { ti, seat } => self.hold_recovery(ti, seat, env, fx),
            KeyEffect::InputMethodDetached { seat } => self.detached(seat),
        }
    }

    /// H2: the input method detached a text input of `seat` on its own, as
    /// when its process restarts. The key events in flight died with it,
    /// as their callbacks never come, so a held key's release may never
    /// reach the keyboard, and the keys it took are no longer its to
    /// confirm. No key a keyboard of `seat` holds now is recovered again;
    /// its next press starts a generation that can be. This ends only the
    /// recovery: the keys are still guest input.
    fn detached(&mut self, seat: SeatId) {
        for keyboard in self.keyboards.of_seat(seat.0) {
            self.generations.mark_held_keys_stale(keyboard);
        }
    }

    /// Move the IME's Backspace repeat lease on the keyboards of `seat`,
    /// none if the input's keyboard has no seat
    /// (`KeyGenerationRegistry::lease`).
    fn lease(&mut self, seat: Option<u32>, input: LeaseInput) {
        let keyboards = seat.map_or_else(Vec::new, |seat| self.keyboards.of_seat(seat));
        self.generations.lease(&keyboards, input);
        // One text input on a connection is enabled at a time, so the IME
        // never arms a Backspace another text input holds.
        if let LeaseInput::Ime {
            ti,
            outcome: LeaseOutcome::ArmIfBackspaceHeld,
        } = input
        {
            let leases = keyboards
                .iter()
                .filter_map(|&keyboard| self.generations.backspace_lease(keyboard));
            debug_assert!(
                leases.clone().all(|leased| leased == ti),
                "a held Backspace is leased to another text input than {ti:?}: {:?}",
                leases.collect::<Vec<_>>()
            );
        }
    }
}

/// What tests read of the keys, as no peer can, and the objects the core
/// names, which the orphan check reads.
#[cfg(test)]
impl KeyboardCore {
    /// Who gave the guest `keyboard`'s `key`.
    pub(crate) fn owner(&self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        self.generations.guest_owner(keyboard, key)
    }

    /// Whether `keyboard` holds `key`.
    pub(crate) fn held(&self, keyboard: HostId, key: u32) -> bool {
        self.generations.physically_held(keyboard, key)
    }

    /// The live peek of `keyboard`'s `key`, which makes it a candidate for
    /// recovery.
    pub(crate) fn peek(&self, keyboard: HostId, key: u32) -> Option<PeekKeyProvenance> {
        self.generations.peek(keyboard, key)
    }

    /// The text input `keyboard`'s held Backspace is leased to.
    pub(crate) fn backspace_lease(&self, keyboard: HostId) -> Option<TiId> {
        self.generations.backspace_lease(keyboard)
    }

    /// Whether `keyboard`'s held Backspace may not be leased again in this
    /// hold.
    pub(crate) fn backspace_repeat_cancelled(&self, keyboard: HostId) -> bool {
        self.generations.backspace_repeat_cancelled(keyboard)
    }

    /// Whether a host accelerator's press suppressed `keyboard`'s `key`.
    pub(crate) fn accelerator_suppressed(&self, keyboard: HostId, key: u32) -> bool {
        self.generations.host_accelerator_suppressed(keyboard, key)
    }

    /// The sequence of the latest peek of guest seat `seat` on `surface`.
    pub(crate) fn latest_peek_sequence(&self, seat: u32, surface: Option<u32>) -> Option<u64> {
        self.generations.latest_peek_sequence(seat, surface)
    }

    /// The serial of the release a retired generation of `keyboard`'s `key`
    /// still owes the guest.
    pub(crate) fn retired_release(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.generations.retired_release(keyboard, key)
    }

    /// Every host keyboard the key state names.
    pub(crate) fn key_state_keyboards(&self) -> impl Iterator<Item = HostId> + '_ {
        self.generations.keyboards_for_test()
    }

    /// Every peek watermark's guest seat, guest surface and host keyboard.
    pub(crate) fn peek_watermarks(&self) -> impl Iterator<Item = (u32, Option<u32>, HostId)> + '_ {
        self.generations.peek_watermarks_for_test()
    }

    /// The guest text input, guest keyboard and host keyboard a waiting
    /// recovery names.
    pub(crate) fn recovery_objects(&self) -> Option<(u32, u32, HostId)> {
        self.recovery.map(|pending| pending.objects_for_test())
    }
}
