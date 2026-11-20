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

//! X2': an empty IME confirmation's recovery of a held key, which waits
//! until the host shows whose confirmation it was.
//!
//! At extension v11 Exo confirms before every press its input method did
//! not handle (E13), and sends that press's `peek_key` and
//! `wl_keyboard.key` after the confirmation (E8), in the same task. So an
//! empty confirmation that finds a held key the input method took is
//! either an autorepeat of that key or the confirmation of a new press,
//! which the proxy cannot tell until the host's next events. The keyboard
//! core holds the recovery ([`PendingRecovery`]), and the next host event,
//! as its [`HostSignal`], decides it ([`PendingRecovery::decide`]):
//!
//! - before any peek, the keyboard's `modifiers` leave it waiting: Exo
//!   sends them right before the key they apply to;
//! - the `peek_key` press of a key the keyboard does not hold, or holds
//!   only across a detach ([`KeyboardCore::detached`]), leaves it waiting,
//!   as that key's possible press. The keyboard may be any of the
//!   connection's, of any guest seat: Exo has one seat, and sends each of
//!   its keyboards the press in turn;
//! - that key's `wl_keyboard.key` press on that keyboard, with the peek's
//!   serial, shows that the input method did not handle the press, so the
//!   confirmation was its own: no key is recovered, as when a confirmation
//!   finds no held key;
//! - the text input's `leave` shows that the confirmation was the one Exo
//!   sends as it detaches the input method (E6): no key is recovered
//!   either, and the detach ends the recovery of every key held;
//! - any other event shows that the confirmation was an autorepeat: the
//!   held key is recovered, and then the event is handled.
//!
//! A recovery that waits also syncs with the host, whose `done` follows
//! every event of the confirmation's task (E11), so that one round trip
//! decides it at the latest. No timer is involved.

use super::generations::{LeaseInput, EVDEV_KEY_BACKSPACE};
use super::{Env, KeyEffects, KeyboardCore, Recovered};
use crate::ime::{LeaseOutcome, SeatId, TiId};
use crate::protocols::wayland::wl_keyboard;
use crate::state::HostId;
use crate::wire::Outgoing;

/// The held key an empty confirmation may stand for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HeldRepeatKey {
    pub(super) guest_keyboard_id: u32,
    pub(super) host_keyboard_id: HostId,
    pub(super) key: u32,
    /// The serial of the key's latest `peek_key`.
    serial: u32,
    /// The time of the key's latest `peek_key`.
    time: u32,
}

/// The recovery of an empty confirmation's held key, waiting for the
/// host's next event; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingRecovery {
    pub(super) ti: TiId,
    seat: SeatId,
    /// The seat's eligible held key when the confirmation came.
    pub(super) held: HeldRepeatKey,
    wait: RecoveryWait,
}

/// What a [`PendingRecovery`] waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryWait {
    /// The host's next event: no `peek_key` press has come since the
    /// confirmation.
    NextEvent,
    /// The `wl_keyboard.key` press of the `peek_key` press of a key not
    /// held before, or held only across a detach, since the confirmation:
    /// its host keyboard, key and serial.
    Press {
        keyboard: HostId,
        key: u32,
        serial: u32,
    },
}

/// A host event, as far as a [`PendingRecovery`] reads it. The core reads
/// it off each [`super::KeyInput`] that is a host event, which the
/// dispatcher decoded once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostSignal {
    /// A `wl_keyboard.modifiers`.
    Modifiers,
    /// A `peek_key` press of `key` under `serial` on `keyboard`, the host
    /// keyboard of its extended keyboard if the proxy knows it. `new`: the
    /// keyboard does not hold the key, or holds it only across a detach.
    PeekPress {
        keyboard: Option<HostId>,
        key: u32,
        serial: u32,
        new: bool,
    },
    /// A `wl_keyboard.key` press of `key` under `serial` on `keyboard`.
    KeyPress {
        keyboard: HostId,
        key: u32,
        serial: u32,
    },
    /// A `wl_keyboard.key` of `keyboard` the proxy drops undecoded, as the
    /// guest released the keyboard: the press a peek on it announced, if
    /// one did.
    UnhandledKey { keyboard: HostId },
    /// A `zwp_text_input_v1.leave` of the host text input of guest text
    /// input `ti`, if it has one.
    TextInputLeave { ti: Option<TiId> },
    /// Any other event.
    Other,
}

/// What a [`HostSignal`] makes of a [`PendingRecovery`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryStep {
    /// It waits on, for this.
    Wait(RecoveryWait),
    /// The confirmation was the host's before a new press or a detach: no
    /// key is recovered.
    Drop,
    /// The confirmation was an autorepeat: the held key is recovered
    /// before the event is handled.
    Recover,
}

impl PendingRecovery {
    /// The guest text input, guest keyboard and host keyboard it names.
    #[cfg(test)]
    pub(super) fn objects_for_test(&self) -> (u32, u32, HostId) {
        let held = self.held;
        (self.ti.0, held.guest_keyboard_id, held.host_keyboard_id)
    }

    /// The recovery's transition for the host's next event.
    fn decide(&self, signal: HostSignal) -> RecoveryStep {
        match (self.wait, signal) {
            (RecoveryWait::NextEvent, HostSignal::Modifiers) => {
                RecoveryStep::Wait(RecoveryWait::NextEvent)
            }
            (
                RecoveryWait::NextEvent,
                HostSignal::PeekPress {
                    keyboard: Some(keyboard),
                    key,
                    serial,
                    new: true,
                },
            ) => RecoveryStep::Wait(RecoveryWait::Press {
                keyboard,
                key,
                serial,
            }),
            (
                press @ RecoveryWait::Press { .. },
                HostSignal::KeyPress {
                    keyboard,
                    key,
                    serial,
                },
            ) if press
                == (RecoveryWait::Press {
                    keyboard,
                    key,
                    serial,
                }) =>
            {
                RecoveryStep::Drop
            }
            (
                RecoveryWait::Press {
                    keyboard: waited, ..
                },
                HostSignal::UnhandledKey { keyboard },
            ) if waited == keyboard => RecoveryStep::Drop,
            (_, HostSignal::TextInputLeave { ti: Some(ti) }) if ti == self.ti => RecoveryStep::Drop,
            _ => RecoveryStep::Recover,
        }
    }
}

impl KeyboardCore {
    /// Resolve the physical key generation responsible for an empty IME
    /// confirmation on `guest_seat`.
    ///
    /// Physical held state alone is insufficient: it has neither event
    /// order nor proof that the key was visible when the host IME consumed
    /// normal `wl_keyboard` delivery. `peek_key` provides both. Select the
    /// newest generation, preferring the keyboard focused on the seat's
    /// current surface, then validate that exact generation without
    /// falling back to an older key.
    ///
    /// None while a keyboard of the seat holds a key across the input
    /// method's own detach (H2): its release may have died with the input
    /// method, so Exo's keyboard may still hold it and give its next press
    /// no `peek_key` (`Keyboard::OnKeyEvent` handles a press only of a key
    /// it does not hold), only the confirmation before it (E13), which no
    /// wire event tells from an autorepeat.
    fn held_repeat_key(&self, guest_seat: u32, env: &impl Env) -> Option<HeldRepeatKey> {
        let active_surface = env.seat_focus(guest_seat);
        let mut candidates = Vec::new();
        let mut stale_key_held = false;

        for host_keyboard_id in self.keyboards.of_seat(guest_seat) {
            let Some(guest_keyboard_id) = self.keyboards.get(host_keyboard_id).map(|k| k.guest)
            else {
                continue;
            };
            stale_key_held |= self.generations.holds_a_stale_key(host_keyboard_id);
            let focused =
                active_surface.is_some_and(|surface| env.focuses(host_keyboard_id, surface));
            for (key, press) in self.generations.peek_keys(host_keyboard_id) {
                candidates.push((
                    focused,
                    press.sequence,
                    guest_keyboard_id,
                    host_keyboard_id,
                    key,
                    press,
                ));
            }
        }

        if stale_key_held {
            log::debug!(
                "Ignoring empty IME confirmation while seat {guest_seat} holds a key across the \
                 input method's detach, whose unannounced press it may precede"
            );
            return None;
        }
        let (_, _, guest_keyboard_id, host_keyboard_id, key, press) = candidates
            .into_iter()
            .filter(|candidate| active_surface.is_none() || candidate.0)
            .max_by_key(|candidate| candidate.1)?;

        let latest_sequence = self
            .generations
            .latest_peek_sequence(guest_seat, active_surface);
        let is_latest_generation =
            latest_sequence.is_none_or(|sequence| sequence == press.sequence);
        let physically_held = self.generations.physically_held(host_keyboard_id, key);
        let stale = self.generations.held_key_is_stale(host_keyboard_id, key);
        let repeatable = self
            .keyboards
            .get(host_keyboard_id)
            .and_then(|keyboard| keyboard.keymap.as_ref())
            .is_some_and(|keymap| keymap.repeats(key));
        let backspace_cancelled = key == EVDEV_KEY_BACKSPACE
            && self
                .generations
                .backspace_repeat_cancelled(host_keyboard_id);
        if !is_latest_generation
            || !physically_held
            || !press.eligible
            || stale
            || !repeatable
            || backspace_cancelled
        {
            log::debug!(
                "Ignoring empty IME confirmation for ineligible held key {} \
                 (latest_generation={}, physically_held={}, generation_eligible={}, \
                 stale={}, repeatable={}, backspace_cancelled={})",
                key,
                is_latest_generation,
                physically_held,
                press.eligible,
                stale,
                repeatable,
                backspace_cancelled
            );
            return None;
        }

        Some(HeldRepeatKey {
            guest_keyboard_id,
            host_keyboard_id,
            key,
            serial: press.serial,
            time: press.time,
        })
    }

    /// X2': text input `ti` of `seat` confirmed an empty composition
    /// (`ime::KeyEffect::RecoverHeldKey`). With no eligible held key,
    /// nothing is sent, and the Backspace lease ends. With one, the
    /// recovery waits for the host's next event, and the core syncs with
    /// the host.
    pub(super) fn hold_recovery(
        &mut self,
        ti: TiId,
        seat: SeatId,
        env: &impl Env,
        fx: &mut KeyEffects,
    ) {
        debug_assert!(
            self.recovery.is_none(),
            "the confirmation's own event decided the recovery before it"
        );
        let Some(held) = self.held_repeat_key(seat.0, env) else {
            self.end_lease(seat, ti);
            log::debug!(
                "Ignoring confirm_preedit without an active preedit for guest {}",
                ti.0
            );
            return;
        };
        self.recovery = Some(PendingRecovery {
            ti,
            seat,
            held,
            wait: RecoveryWait::NextEvent,
        });
        fx.sync = true;
    }

    /// Decide the waiting recovery, if any, by the host event `signal`
    /// reads, before the event is handled.
    pub(super) fn decide(&mut self, signal: HostSignal, env: &impl Env, fx: &mut KeyEffects) {
        let Some(pending) = self.recovery else {
            return;
        };
        match pending.decide(signal) {
            RecoveryStep::Wait(wait) => {
                self.recovery = Some(PendingRecovery { wait, ..pending });
            }
            RecoveryStep::Drop => {
                self.recovery = None;
                log::debug!(
                    "An empty IME confirmation was the host's before a new press or a \
                     detach, not a repeat of held key {}",
                    pending.held.key
                );
                self.end_lease(pending.seat, pending.ti);
            }
            RecoveryStep::Recover => {
                self.recovery = None;
                self.recover(pending, env, fx);
            }
        }
    }

    /// The pending recovery's confirmation was an autorepeat of its held
    /// key: the guest gets the key's press and release unless another
    /// channel already delivered it, then the transaction that closes the
    /// confirmation, whether or not the pair went ([`Recovered`]). If the
    /// text input no longer passes the gate, or the key is no longer held,
    /// nothing is sent, and the lease ends. The lease:
    /// - a held Backspace whose pair is sent arms it, unless a press since
    ///   the confirmation cancelled its repeat;
    /// - a held Backspace whose pair is skipped ends it;
    /// - another held key whose pair is sent ends it;
    /// - another held key whose pair is skipped leaves it alone.
    fn recover(&mut self, pending: PendingRecovery, env: &impl Env, fx: &mut KeyEffects) {
        let PendingRecovery { ti, seat, held, .. } = pending;
        // A keyboard that ended took the recovery of its key with it
        // ([`KeyInput::Ended`](super::KeyInput::Ended)), so a keyboard that
        // still holds the key is the one the recovery found.
        let keyboard_held = self
            .generations
            .physically_held(held.host_keyboard_id, held.key);
        if !(keyboard_held && env.admits_recovery(ti)) {
            self.end_lease(seat, ti);
            log::debug!(
                "Dropping the recovery of held key {} for text input {}, which no longer \
                 passes the gate or holds the key",
                held.key,
                ti.0
            );
            return;
        }
        let pair = self.key_pair(held);
        let recovered = LeaseInput::Recovered {
            ti,
            keyboard: held.host_keyboard_id,
            backspace: held.key == EVDEV_KEY_BACKSPACE,
            sent: !pair.is_empty(),
        };
        // A press since the confirmation may have cancelled a held
        // Backspace's repeat, as it would have right after an immediate
        // recovery.
        self.lease(Some(seat.0), recovered);
        if !pair.is_empty() {
            log::debug!(
                "Recovered empty IME confirmation as held key {} press/release",
                held.key
            );
        }
        // GTK waits for the matching text-input transaction to finish
        // before applying a key that passed through the active IME. A v1
        // confirm with no preedit has no text mutation, so its v3
        // equivalent is the guest's state again, which the IME core sends
        // after the pair.
        fx.recovered = Some(Recovered { ti, pair });
    }

    /// The press and the release of the held key the host input method
    /// consumed, for the guest, unless another channel already delivered
    /// it.
    ///
    /// Both carry the serial and time of the key's latest `peek_key`, which
    /// are those of the `wl_keyboard.key` Exo would have sent; see
    /// INVARIANTS "A recovered key carries the host's serial".
    fn key_pair(&mut self, held: HeldRepeatKey) -> Vec<Outgoing> {
        if !self.generations.recover(held.host_keyboard_id, held.key) {
            // The normal wl_keyboard path already delivered this physical
            // generation to the guest. The confirmation still closes a
            // text-input transaction, but another key pair would duplicate
            // it.
            log::debug!(
                "Skipping IME repeat recovery: key {} was already forwarded for host keyboard {}",
                held.key,
                held.host_keyboard_id.0
            );
            return Vec::new();
        }
        [
            wl_keyboard::KEY_STATE_PRESSED,
            wl_keyboard::KEY_STATE_RELEASED,
        ]
        .map(|state| {
            wl_keyboard::evt::key(
                held.guest_keyboard_id,
                held.serial,
                held.time,
                held.key,
                state,
            )
        })
        .into()
    }

    /// End text input `ti`'s lease on `seat`: a confirmation recovered no
    /// key.
    fn end_lease(&mut self, seat: SeatId, ti: TiId) {
        let outcome = LeaseOutcome::End;
        self.lease(Some(seat.0), LeaseInput::Ime { ti, outcome });
    }
}

#[cfg(test)]
mod tests;
