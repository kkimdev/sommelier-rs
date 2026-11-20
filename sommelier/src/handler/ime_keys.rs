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

use crate::handler::callback::queue_internal_sync;
use crate::ime::LeaseOutcome;
use crate::ime::{SeatId, TiId};
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::wl_keyboard;
use crate::state::{Context, HostId, InternalCallback, LeaseInput, EVDEV_KEY_BACKSPACE};
use crate::wire::Outgoing;

/// Move the IME's Backspace repeat lease on the keyboards of `guest_seat`,
/// none if the input's keyboard has no seat
/// ([`crate::state::KeyGenerationRegistry::lease`]).
pub(crate) fn lease(ctx: &mut Context, guest_seat: Option<u32>, input: LeaseInput) {
    let seat = guest_seat.map_or_else(Vec::new, |seat| ctx.keyboards.of_seat(seat));
    ctx.key_generations.lease(&seat, input);
    // One text input on a connection is enabled at a time, so the IME
    // never arms a Backspace another text input holds.
    if let LeaseInput::Ime {
        ti,
        outcome: LeaseOutcome::ArmIfBackspaceHeld,
    } = input
    {
        let leases = seat
            .iter()
            .filter_map(|&k| ctx.key_generations.backspace_lease(k));
        debug_assert!(
            leases.clone().all(|leased| leased == ti),
            "a held Backspace is leased to another text input than {ti:?}: {:?}",
            leases.collect::<Vec<_>>()
        );
    }
}

/// End text input `ti`'s lease on `seat`: a confirmation recovered no key.
fn end_lease(ctx: &mut Context, seat: SeatId, ti: TiId) {
    let outcome = LeaseOutcome::End;
    lease(ctx, Some(seat.0), LeaseInput::Ime { ti, outcome });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HeldRepeatKey {
    guest_keyboard_id: u32,
    pub(super) host_keyboard_id: HostId,
    pub(super) key: u32,
    /// The serial of the key's latest `peek_key`.
    serial: u32,
    /// The time of the key's latest `peek_key`.
    time: u32,
}

/// Resolve the physical key generation responsible for an empty IME
/// confirmation.
///
/// Physical held state alone is insufficient: it has neither event order
/// nor proof that the key was visible when the host IME consumed normal
/// `wl_keyboard` delivery. `peek_key` provides both. Select the newest
/// generation, preferring the keyboard focused on the seat's current surface,
/// then validate that exact generation without falling back to an older key.
///
/// None while a keyboard of the seat holds a key across the input method's
/// own detach (H2): its release may have died with the input method, so
/// Exo's keyboard may still hold it and give its next press no `peek_key`
/// (`Keyboard::OnKeyEvent` handles a press only of a key it does not hold),
/// only the confirmation before it (E13), which no wire event tells from an
/// autorepeat.
pub(super) fn held_repeat_key_for_seat(ctx: &Context, guest_seat: u32) -> Option<HeldRepeatKey> {
    let active_surface = ctx.keyboard_focus.surface_for_seat(guest_seat);
    let mut candidates = Vec::new();
    let mut stale_key_held = false;

    for host_keyboard_id in ctx.keyboards.of_seat(guest_seat) {
        let Some(guest_keyboard_id) = ctx.keyboards.get(host_keyboard_id).map(|k| k.guest) else {
            continue;
        };
        stale_key_held |= ctx.key_generations.holds_a_stale_key(host_keyboard_id);
        let focused = active_surface.is_some_and(|surface| {
            ctx.keyboard_focus
                .keyboard_owns_surface(host_keyboard_id, surface)
        });
        for (key, press) in ctx.key_generations.peek_keys(host_keyboard_id) {
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

    let latest_sequence = ctx
        .key_generations
        .latest_peek_sequence(guest_seat, active_surface);
    let is_latest_generation = latest_sequence.is_none_or(|sequence| sequence == press.sequence);
    let physically_held = ctx.key_generations.physically_held(host_keyboard_id, key);
    let stale = ctx.key_generations.held_key_is_stale(host_keyboard_id, key);
    let repeatable = ctx
        .keyboards
        .get(host_keyboard_id)
        .and_then(|keyboard| keyboard.keymap.as_ref())
        .is_some_and(|keymap| keymap.repeats(key));
    let backspace_cancelled = key == EVDEV_KEY_BACKSPACE
        && ctx
            .key_generations
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

/// The press and the release of the held key the host input method
/// consumed, for the guest, unless another channel already delivered it.
///
/// Both carry the serial and time of the key's latest `peek_key`, which
/// are those of the `wl_keyboard.key` Exo would have sent; see
/// INVARIANTS "A recovered key carries the host's serial".
fn synthesize_ime_consumed_key_pair(ctx: &mut Context, held: HeldRepeatKey) -> Vec<Outgoing> {
    if !ctx.key_generations.recover(held.host_keyboard_id, held.key) {
        // The normal wl_keyboard path already delivered this physical
        // generation to the guest. The confirmation still closes a
        // text-input transaction, but another key pair would duplicate it.
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

/// X2': an empty confirmation's recovery of a held key, which waits until
/// the host shows whose confirmation it was.
///
/// At extension v11 Exo confirms before every press its input method did
/// not handle (E13), and sends that press's `peek_key` and
/// `wl_keyboard.key` after the confirmation (E8), in the same task. So an
/// empty confirmation that finds a held key the input method took is
/// either an autorepeat of that key or the confirmation of a new press,
/// which the proxy cannot tell until the host's next events. It holds the
/// recovery in [`Context::pending_recovery`], and the next host event
/// decides it ([`before_host_event`]):
///
/// - before any peek, the keyboard's `modifiers` leave it waiting: Exo
///   sends them right before the key they apply to;
/// - the `peek_key` press of a key the keyboard does not hold, or holds
///   only across a detach ([`forget_held_keys_for_seat`]), leaves it
///   waiting, as that key's possible press. The keyboard may be any of the
///   connection's, of any guest seat: Exo has one seat, and sends each of
///   its keyboards the press in turn;
/// - that key's `wl_keyboard.key` press on that keyboard, with the peek's
///   serial, shows that the input method did not handle the press, so the
///   confirmation was its own: no key is recovered, as when a confirmation
///   finds no held key;
/// - the text input's `leave` shows that the confirmation was the one Exo
///   sends as it detaches the input method (E6): no key is recovered
///   either, and the detach ends the recovery of every key held
///   ([`forget_held_keys_for_seat`]);
/// - any other event shows that the confirmation was an autorepeat: the
///   held key is recovered ([`recover_pending`]), and then the event is
///   handled.
///
/// A recovery that waits also syncs with the host, whose `done` follows
/// every event of the confirmation's task (E11), so that one round trip
/// decides it at the latest. No timer is involved.
///
/// [`Self::decide`] is its transition function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingRecovery {
    ti: TiId,
    seat: SeatId,
    /// The seat's eligible held key when the confirmation came.
    held: HeldRepeatKey,
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

/// A host event, as far as a [`PendingRecovery`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostSignal {
    /// A `wl_keyboard.modifiers`.
    Modifiers,
    /// A `peek_key` press of `key` under `serial` on `keyboard`, the host
    /// keyboard of its extended keyboard if the proxy knows it. `new`: the
    /// keyboard does not hold the key, or holds it only across a detach
    /// ([`crate::state::KeyGenerationRegistry::peek_press_is_new`]).
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
    /// A `zwp_text_input_v1.leave` of the host text input of guest text
    /// input `ti`, if it has one.
    TextInputLeave { ti: Option<u32> },
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
    pub(crate) fn objects_for_test(&self) -> (u32, u32, HostId) {
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
            (_, HostSignal::TextInputLeave { ti: Some(ti) }) if ti == self.ti.0 => {
                RecoveryStep::Drop
            }
            _ => RecoveryStep::Recover,
        }
    }
}

/// Read the host event of `interface` (none for an untracked sender),
/// whose sender is `sender`, `opcode` and `payload` as a [`HostSignal`].
fn host_signal(
    ctx: &Context,
    sender: u32,
    interface: Option<&str>,
    opcode: u16,
    payload: &[u8],
) -> HostSignal {
    let mut msg = crate::wire::WireMessage::new(sender, opcode, payload);
    match interface {
        Some(wl_keyboard::NAME) if opcode == wl_keyboard::EVT_MODIFIERS => HostSignal::Modifiers,
        Some(wl_keyboard::NAME) if opcode == wl_keyboard::EVT_KEY => {
            match wl_keyboard::Event::from_wire(&mut msg) {
                Ok(wl_keyboard::Event::Key {
                    serial, key, state, ..
                }) if state == wl_keyboard::KEY_STATE_PRESSED => HostSignal::KeyPress {
                    keyboard: HostId(sender),
                    key,
                    serial,
                },
                _ => HostSignal::Other,
            }
        }
        Some(zcr_extended_keyboard_v1::NAME) => {
            match zcr_extended_keyboard_v1::Event::from_wire(&mut msg) {
                Ok(zcr_extended_keyboard_v1::Event::PeekKey {
                    serial, key, state, ..
                }) if state == wl_keyboard::KEY_STATE_PRESSED => {
                    // Any keyboard of the connection: Exo has one seat,
                    // which sends each keyboard of every guest seat the
                    // press in turn. A key held only across a detach is
                    // pressed anew too.
                    let keyboard = ctx.keyboards.of_extended(HostId(sender));
                    HostSignal::PeekPress {
                        keyboard,
                        key,
                        serial,
                        new: keyboard.is_some_and(|keyboard| {
                            ctx.key_generations.peek_press_is_new(keyboard, key)
                        }),
                    }
                }
                _ => HostSignal::Other,
            }
        }
        Some(zwp_text_input_v1::NAME) if opcode == zwp_text_input_v1::EVT_LEAVE => {
            HostSignal::TextInputLeave {
                ti: ctx.shadow_table.get_guest_id(sender),
            }
        }
        _ => HostSignal::Other,
    }
}

/// X2': text input `ti` of `seat` confirmed an empty composition
/// ([`crate::ime::KeyEffect::RecoverHeldKey`]). With no eligible held key,
/// nothing is sent, and the Backspace lease ends. With one, the recovery
/// waits for the host's next event ([`PendingRecovery`]).
pub(super) fn hold_recovery(ctx: &mut Context, ti: TiId, seat: SeatId) {
    debug_assert!(
        ctx.pending_recovery.is_none(),
        "the confirmation's own event decided the recovery before it"
    );
    let Some(held) = held_repeat_key_for_seat(ctx, seat.0) else {
        end_lease(ctx, seat, ti);
        log::debug!(
            "Ignoring confirm_preedit without an active preedit for guest {}",
            ti.0
        );
        return;
    };
    ctx.pending_recovery = Some(PendingRecovery {
        ti,
        seat,
        held,
        wait: RecoveryWait::NextEvent,
    });
    queue_internal_sync(ctx, InternalCallback::KeyRecovery);
}

/// Decide the pending recovery, if any, before the host event of
/// `interface` (none for an untracked sender), whose sender is `sender`,
/// `opcode` and `payload` are handled ([`PendingRecovery`]). What it sends
/// the guest goes before that event.
pub(crate) fn before_host_event(
    ctx: &mut Context,
    sender: u32,
    interface: Option<&str>,
    opcode: u16,
    payload: &[u8],
) {
    let Some(pending) = ctx.pending_recovery else {
        return;
    };
    let signal = host_signal(ctx, sender, interface, opcode, payload);
    match pending.decide(signal) {
        RecoveryStep::Wait(wait) => {
            ctx.pending_recovery = Some(PendingRecovery { wait, ..pending });
        }
        RecoveryStep::Drop => drop_pending(ctx),
        RecoveryStep::Recover => recover_pending(ctx),
    }
}

/// The pending recovery's confirmation was Exo's before the press it was
/// waiting on (E13), or before its detach (E6): no key is recovered, and
/// the Backspace lease ends, as when a confirmation finds no held key.
fn drop_pending(ctx: &mut Context) {
    let Some(PendingRecovery { ti, seat, held, .. }) = ctx.pending_recovery.take() else {
        return;
    };
    log::debug!(
        "An empty IME confirmation was the host's before a new press or a detach, not a \
         repeat of held key {}",
        held.key
    );
    end_lease(ctx, seat, ti);
}

/// The pending recovery's confirmation was an autorepeat of its held key:
/// the guest gets the key's press and release unless another channel
/// already delivered it, then the transaction that closes the
/// confirmation, whether or not the pair went. If the text input no longer
/// passes the gate, or the key is no longer held, nothing is sent, and the
/// lease ends. The lease:
/// - a held Backspace whose pair is sent arms it, unless a press since the
///   confirmation cancelled its repeat;
/// - a held Backspace whose pair is skipped ends it;
/// - another held key whose pair is sent ends it;
/// - another held key whose pair is skipped leaves it alone.
pub(crate) fn recover_pending(ctx: &mut Context) {
    let Some(PendingRecovery { ti, seat, held, .. }) = ctx.pending_recovery.take() else {
        return;
    };
    let keyboard_held = ctx
        .keyboards
        .get(held.host_keyboard_id)
        .is_some_and(|keyboard| keyboard.guest == held.guest_keyboard_id)
        && ctx
            .key_generations
            .physically_held(held.host_keyboard_id, held.key);
    if !(keyboard_held && ctx.ime.admits_recovery(ti)) {
        end_lease(ctx, seat, ti);
        log::debug!(
            "Dropping the recovery of held key {} for text input {}, which no longer \
             passes the gate or holds the key",
            held.key,
            ti.0
        );
        return;
    }
    let pair = synthesize_ime_consumed_key_pair(ctx, held);
    let recovered = LeaseInput::Recovered {
        ti,
        keyboard: held.host_keyboard_id,
        backspace: held.key == EVDEV_KEY_BACKSPACE,
        sent: !pair.is_empty(),
    };
    // A press since the confirmation may have cancelled a held Backspace's
    // repeat, as it would have right after an immediate recovery.
    lease(ctx, Some(seat.0), recovered);
    if !pair.is_empty() {
        log::debug!(
            "Recovered empty IME confirmation as held key {} press/release",
            held.key
        );
    }
    // GTK waits for the matching text-input transaction to finish before
    // applying a key that passed through the active IME. A v1 confirm with
    // no preedit has no text mutation, so its v3 equivalent is the guest's
    // state again, which the IME core sends after the pair.
    crate::handler::text_input::ime(ctx, crate::ime::Input::HeldKeyRecovered { ti, pair });
}

/// H2: the input method detached text input on its own, as when its
/// process restarts. The key events in flight died with it, as their
/// callbacks never come, so a held key's release may never reach the
/// keyboard, and the keys it took are no longer its to confirm. No key a
/// keyboard of `guest_seat` holds now is recovered again; its next press
/// starts a generation that can be. This ends only the recovery: the keys
/// are still guest input.
pub(super) fn forget_held_keys_for_seat(ctx: &mut Context, guest_seat: u32) {
    for keyboard in ctx.keyboards.of_seat(guest_seat) {
        ctx.key_generations.mark_held_keys_stale(keyboard);
    }
}

/// Forget the pending recovery if it names guest text input `ti`, which
/// ended.
pub(crate) fn forget_recovery_of_text_input(ctx: &mut Context, ti: TiId) {
    if ctx.pending_recovery.is_some_and(|pending| pending.ti == ti) {
        ctx.pending_recovery = None;
    }
}

/// Forget the pending recovery if its held key is on guest keyboard
/// `guest_keyboard`, which ended.
pub(crate) fn forget_recovery_of_keyboard(ctx: &mut Context, guest_keyboard: u32) {
    if ctx
        .pending_recovery
        .is_some_and(|pending| pending.held.guest_keyboard_id == guest_keyboard)
    {
        ctx.pending_recovery = None;
    }
}

#[cfg(test)]
mod tests;
