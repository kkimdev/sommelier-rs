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
use crate::ime::{SeatId, TiId};
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::wl_keyboard;
use crate::state::{Context, HostId, InternalCallback, ShadowTable};
use crate::wire::Outgoing;
use std::collections::BTreeMap;

/// Return whether the repeat fallback belongs to this physical host
/// keyboard. A seat may expose multiple keyboard objects; checking only the
/// seat would let a stale IME confirmation swallow Backspace events from a
/// different keyboard on that seat.
pub(crate) fn backspace_repeat_active_for_keyboard(
    ctx: &Context,
    host_keyboard_id: HostId,
) -> bool {
    ctx.key_generations.ime_repeat_active(
        host_keyboard_id,
        crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
    )
}

/// The host keyboards of `guest_seat`, in the order of their guest IDs.
/// It borrows only the fields it reads, so that a caller can change
/// another field of the context as it walks them.
fn host_keyboards_of_seat<'a>(
    keyboard_to_seat: &'a BTreeMap<u32, u32>,
    shadow_table: &'a ShadowTable,
    guest_seat: u32,
) -> impl Iterator<Item = HostId> + 'a {
    keyboard_to_seat
        .iter()
        .filter(move |(_, &seat)| seat == guest_seat)
        .filter_map(|(&guest_keyboard_id, _)| shadow_table.get_host_id(guest_keyboard_id))
        .map(HostId)
}

/// The host keyboard of `guest_seat` with the lowest guest ID that holds
/// Backspace with its repeat not cancelled, if any.
pub(super) fn held_backspace_keyboard_for_seat(ctx: &Context, guest_seat: u32) -> Option<HostId> {
    host_keyboards_of_seat(&ctx.keyboard_to_seat, &ctx.shadow_table, guest_seat).find(
        |host_keyboard_id| {
            !ctx.key_generations.backspace_repeat_cancelled(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            ) && ctx.key_generations.physically_held(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            )
        },
    )
}

#[cfg(test)]
pub(crate) fn backspace_pressed_for_seat(ctx: &Context, guest_seat: u32) -> bool {
    held_backspace_keyboard_for_seat(ctx, guest_seat).is_some()
}

pub(crate) fn end_backspace_repeat_for_seat(ctx: &mut Context, guest_seat: u32) {
    let host_keyboards =
        host_keyboards_of_seat(&ctx.keyboard_to_seat, &ctx.shadow_table, guest_seat);
    // Ending the IME fallback is distinct from releasing the physical key.
    // Keep the cancellation marker until that release arrives; otherwise the
    // next empty confirm_preedit in the same held-key session would synthesize
    // another Backspace after the fallback was explicitly stopped.
    for host_keyboard_id in host_keyboards {
        if ctx.key_generations.ime_repeat_active(
            host_keyboard_id,
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
        ) {
            ctx.key_generations.cancel_backspace_repeat(
                host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            );
        }
    }
}

pub(super) fn end_backspace_repeat_for_text_input(
    ctx: &mut Context,
    guest_seat: u32,
    guest_text_input: u32,
) {
    let host_keyboards =
        host_keyboards_of_seat(&ctx.keyboard_to_seat, &ctx.shadow_table, guest_seat);
    for host_keyboard_id in host_keyboards {
        ctx.key_generations.cancel_backspace_repeat_for_owner(
            host_keyboard_id,
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            guest_text_input,
        );
    }
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

    for (&guest_keyboard_id, &seat) in &ctx.keyboard_to_seat {
        if seat != guest_seat {
            continue;
        }
        let Some(host_keyboard_id) = ctx.shadow_table.get_host_id(guest_keyboard_id).map(HostId)
        else {
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
        .keyboard_repeatable_keys
        .get(&host_keyboard_id)
        .is_some_and(|keys| keys.contains(&key));
    let backspace_cancelled = key == crate::handler::keyboard::EVDEV_KEY_BACKSPACE
        && ctx.key_generations.backspace_repeat_cancelled(
            host_keyboard_id,
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
        );
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
                    let keyboard = ctx
                        .extended_keyboard_to_keyboard
                        .get(&HostId(sender))
                        .copied();
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
        end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
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
    end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
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
    let keyboard_held = ctx.shadow_table.get_host_id(held.guest_keyboard_id)
        == Some(held.host_keyboard_id.0)
        && ctx
            .key_generations
            .physically_held(held.host_keyboard_id, held.key);
    if !(keyboard_held && ctx.ime.admits_recovery(ti)) {
        end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
        log::debug!(
            "Dropping the recovery of held key {} for text input {}, which no longer \
             passes the gate or holds the key",
            held.key,
            ti.0
        );
        return;
    }
    let backspace = held.key == crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
    let pair = synthesize_ime_consumed_key_pair(ctx, held);
    if !pair.is_empty() {
        if backspace {
            // A press since the confirmation may have cancelled the repeat,
            // as it would have right after an immediate recovery.
            ctx.key_generations
                .arm_ime_repeat(held.host_keyboard_id, held.key, ti.0);
        } else {
            end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
        }
        log::debug!(
            "Recovered empty IME confirmation as held key {} press/release",
            held.key
        );
    } else if backspace {
        end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
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
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter(|(_, &seat)| seat == guest_seat)
        .filter_map(|(&guest, _)| ctx.shadow_table.get_host_id(guest).map(HostId))
        .collect();
    for keyboard in host_keyboards {
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
mod tests {
    use super::*;
    use crate::handler::text_input::tests::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Event as V3;
    use crate::state::GuestKeyOwner;

    /// The recovery's transition, for each host event it can meet in each
    /// of its states.
    #[test]
    fn a_waiting_recovery_is_decided_by_the_hosts_next_event() {
        use HostSignal::{KeyPress, Modifiers, Other, PeekPress, TextInputLeave};
        use RecoveryStep::{Drop, Recover, Wait};
        use RecoveryWait::NextEvent;
        let held = HeldRepeatKey {
            guest_keyboard_id: 5,
            host_keyboard_id: HostId(50),
            key: 14,
            serial: 7,
            time: 70,
        };
        let (other_keyboard, key, serial) = (HostId(51), 30, 8);
        let press = RecoveryWait::Press {
            keyboard: other_keyboard,
            key,
            serial,
        };
        let peek = |keyboard, new| PeekPress {
            keyboard,
            key,
            serial,
            new,
        };
        let key_press = |keyboard, serial| KeyPress {
            keyboard,
            key,
            serial,
        };
        for (wait, signal, step) in [
            (NextEvent, Modifiers, Wait(NextEvent)),
            (NextEvent, peek(Some(other_keyboard), true), Wait(press)),
            (NextEvent, peek(Some(other_keyboard), false), Recover),
            (NextEvent, peek(None, true), Recover),
            (NextEvent, key_press(other_keyboard, serial), Recover),
            (NextEvent, TextInputLeave { ti: Some(9) }, Drop),
            (NextEvent, TextInputLeave { ti: Some(10) }, Recover),
            (NextEvent, TextInputLeave { ti: None }, Recover),
            (NextEvent, Other, Recover),
            (press, key_press(other_keyboard, serial), Drop),
            (press, key_press(other_keyboard, serial + 1), Recover),
            (press, key_press(HostId(50), serial), Recover),
            (press, Modifiers, Recover),
            (press, peek(Some(other_keyboard), true), Recover),
            (press, TextInputLeave { ti: Some(9) }, Drop),
            (press, Other, Recover),
        ] {
            let pending = PendingRecovery {
                ti: TiId(9),
                seat: SeatId(1),
                held,
                wait,
            };
            assert_eq!(pending.decide(signal), step, "{wait:?} {signal:?}");
        }
    }

    #[test]
    fn ending_one_keyboard_repeat_does_not_cancel_another_held_keyboard() {
        let mut ime = Ime::with_keyboard();
        let other = HostId(43);
        ime.keyboard(44, other);
        ime.lease_backspace_repeat(TI);
        let generations = &mut ime.ctx.key_generations;
        generations.observe_physical_state(other, BACKSPACE, 1);

        end_backspace_repeat_for_seat(&mut ime.ctx, SEAT);

        let generations = &ime.ctx.key_generations;
        assert!(generations.backspace_repeat_cancelled(HOST_KEYBOARD, BACKSPACE));
        assert!(!generations.backspace_repeat_cancelled(other, BACKSPACE));
        assert!(generations.physically_held(other, BACKSPACE));
    }

    /// An IME edit, whether or not it reaches the guest, ends the
    /// held-Backspace fallback a confirmation leased.
    #[test]
    fn ime_edits_end_a_stale_backspace_repeat() {
        type Event = fn(&mut Ime);
        let cases: [(&str, Event); 3] = [
            ("an empty commit", |ime| ime.commit("")),
            ("a new preedit", |ime| ime.preedit("나", "나")),
            // Out of step with Exo's mirror, the core drops it.
            ("a dropped preedit region", |ime| ime.preedit_region(-3, 3)),
        ];
        for (case, event) in cases {
            let mut ime = Ime::with_keyboard();
            ime.committed().surrounding_text_support = true;
            ime.state().show_for_test("가");
            ime.lease_backspace_repeat(TI);

            event(&mut ime);
            assert!(!ime.repeat_active(), "{case}");
        }
    }

    #[test]
    fn input_mode_switch_boundary_clears_stale_repeat_before_empty_confirmation() {
        let mut ime = Ime::with_keyboard();
        ime.committed().surrounding_text_support = true;
        ime.lease_backspace_repeat(TI);
        end_backspace_repeat_for_seat(&mut ime.ctx, SEAT);

        ime.confirm();
        assert_eq!(
            ime.events(),
            [],
            "a routine empty confirmation must not become a synthetic Backspace"
        );
        assert!(ime.committed().surrounding_text_support);
    }

    /// Exo consumes a held Backspace over committed text and confirms an
    /// empty preedit for each repeat: each becomes a key pair with the
    /// serial of the held key's peek, and the repeat stays leased.
    #[test]
    fn held_backspace_repeats_over_committed_korean_without_preedit() {
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 700, 100);
        ime.committed().surrounding_text_support = true;

        for _ in 0..3 {
            ime.confirm();
            assert_eq!(
                ime.events(),
                [
                    key(KEYBOARD, BACKSPACE, PRESSED, 700, 100),
                    key(KEYBOARD, BACKSPACE, RELEASED, 700, 100),
                    uncommitted_done(),
                ]
            );
        }
        assert!(ime.repeat_active());
    }

    #[test]
    fn empty_confirmation_recovers_any_repeatable_ime_consumed_key() {
        const TAB: u32 = 15;
        const ENTER: u32 = 28;
        const SPACE: u32 = 57;
        for held in [BACKSPACE, TAB, ENTER, SPACE] {
            let mut ime = Ime::with_keyboard();
            ime.hold_peeked(HOST_KEYBOARD, held, 700, 100);

            ime.confirm();
            assert_eq!(
                ime.events(),
                [
                    key(KEYBOARD, held, PRESSED, 700, 100),
                    key(KEYBOARD, held, RELEASED, 700, 100),
                    uncommitted_done(),
                ]
            );
        }
    }

    #[test]
    fn empty_confirmation_does_not_fall_back_from_newest_ineligible_key() {
        const SHIFT: u32 = 42;
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);

        // Shift is newer but XKB does not mark it repeatable. The
        // confirmation belongs to that newest generation or to no
        // recoverable key; it must never synthesize the older held Space.
        let generations = &mut ime.ctx.key_generations;
        generations.peek_key(
            HOST_KEYBOARD,
            SHIFT,
            crate::state::PeekInput::Press {
                serial: 701,
                time: 101,
                guest_input: true,
            },
        );
        ime.confirm();
        assert_eq!(ime.events(), []);

        let generations = &mut ime.ctx.key_generations;
        generations.observe_physical_state(HOST_KEYBOARD, SHIFT, 0);
        ime.confirm();
        assert_eq!(
            ime.events(),
            [],
            "releasing the superseding key must not revive an older generation"
        );
    }

    /// Keys held since `wl_keyboard.enter` are ordered by their peeks, not
    /// by the order the enter listed them in: an empty confirmation stands
    /// for the key peeked last. The guest holds both from the enter, so it
    /// gets no pair, only the `done` of a held key found.
    #[test]
    fn keys_held_since_enter_are_ordered_by_their_peeks() {
        const TAB: u32 = 15;
        const SPACE: u32 = 57;
        let mut ime = Ime::with_extended_keyboard();
        let generations = &mut ime.ctx.key_generations;
        generations.install_enter_snapshot(HOST_KEYBOARD, [TAB, SPACE]);
        let repeatable = ime.ctx.keyboard_repeatable_keys.entry(HOST_KEYBOARD);
        repeatable.or_default().extend([TAB, SPACE]);

        ime.peek(SPACE, PRESSED, 700, 100);
        ime.peek(TAB, PRESSED, 701, 101);
        ime.confirm();
        assert_eq!(ime.events(), [uncommitted_done()]);
    }

    #[test]
    fn empty_confirmation_prefers_the_keyboard_focused_on_the_current_surface() {
        let mut ime = Ime::with_keyboard();
        let stale = HostId(51);
        ime.keyboard(50, stale);
        ime.focus(HOST_KEYBOARD);
        ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);
        ime.hold_peeked(stale, 28, 701, 101);

        ime.confirm();
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, 57, PRESSED, 700, 100),
                key(KEYBOARD, 57, RELEASED, 700, 100),
                uncommitted_done(),
            ]
        );
    }

    #[test]
    fn empty_confirmation_never_uses_a_stale_unfocused_keyboard() {
        let mut ime = Ime::with_keyboard();
        ime.focus(HostId(99));
        ime.hold_peeked(HOST_KEYBOARD, 57, 700, 100);

        ime.confirm();
        assert_eq!(
            ime.events(),
            [],
            "an active surface without a matching keyboard must not use stale focus"
        );
    }

    /// Exo empties the preedit as Backspace deletes it, then confirms each
    /// repeat over the committed text: each becomes a Backspace pair, until
    /// the fallback ends.
    #[test]
    fn emptied_preedit_enables_held_backspace_fallback_for_committed_text() {
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 701, 101);
        ime.state().set_commits_for_test(2);
        ime.committed().surrounding_text_support = true;
        ime.preedit("가", "");
        ime.preedit("", "");
        ime.events();

        for _ in 0..2 {
            ime.confirm();
            assert_eq!(
                ime.events(),
                [
                    key(KEYBOARD, BACKSPACE, PRESSED, 701, 101),
                    key(KEYBOARD, BACKSPACE, RELEASED, 701, 101),
                    (TI, V3::Done { serial: 2 }.into()),
                ]
            );
            assert!(ime.committed().surrounding_text_support);
        }

        end_backspace_repeat_for_seat(&mut ime.ctx, SEAT);
        ime.confirm();
        assert_eq!(ime.events(), []);
    }

    /// A physical Backspace press the guest already got, then the host
    /// clearing its preedit and confirming: the confirmation closes the
    /// transaction without another pair.
    #[test]
    fn empty_confirmation_does_not_duplicate_forwarded_backspace() {
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 1, 100);
        assert!(ime
            .ctx
            .claim_guest_key(HOST_KEYBOARD, BACKSPACE, GuestKeyOwner::Physical));
        ime.state().show_for_test("가");
        ime.committed().surrounding_text_support = true;
        ime.preedit("", "");
        assert!(ime.repeat_active());
        ime.events();

        ime.confirm();
        assert_eq!(ime.events(), [uncommitted_done()]);
        assert!(!ime.repeat_active());
    }

    #[test]
    fn held_backspace_fallback_rejects_unmapped_guest_keyboard() {
        let mut ime = Ime::new();
        // A stale route can survive a malformed or reordered bind, but
        // without a host keyboard there is no physical key or compositor
        // time domain from which a synthetic pair may safely be generated,
        // even while host keyboards, none of them its own, hold a key.
        ime.ctx.keyboard_to_seat.insert(KEYBOARD, SEAT);
        for keyboard in [HOST_KEYBOARD, HostId(KEYBOARD)] {
            ime.hold_peeked(keyboard, BACKSPACE, 700, 100);
        }

        ime.confirm();
        assert!(
            ime.ctx.host_to_client_queue.is_empty(),
            "synthetic Backspace must require a mapped host keyboard"
        );
    }
}
