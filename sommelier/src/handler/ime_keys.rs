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
use crate::handler::text_input::active_guest_for_host_text_input;
use crate::ime::{SeatId, TiId};
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::wl_keyboard;
use crate::state::{Context, GuestKeyDelivery, GuestKeyEvent, HostId, InternalCallback};
use crate::wire::Action;

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

pub(super) fn held_backspace_keyboard_for_seat(ctx: &Context, guest_seat: u32) -> Option<HostId> {
    keyboard_for_seat(ctx, guest_seat)
        .map(|(_, host_keyboard_id)| host_keyboard_id)
        .filter(|host_keyboard_id| {
            !ctx.key_generations.backspace_repeat_cancelled(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            ) && ctx.key_generations.physically_held(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            )
        })
}

#[cfg(test)]
pub(crate) fn backspace_pressed_for_seat(ctx: &Context, guest_seat: u32) -> bool {
    held_backspace_keyboard_for_seat(ctx, guest_seat).is_some()
}

fn keyboard_for_seat(ctx: &Context, guest_seat: u32) -> Option<(u32, HostId)> {
    let mut candidates: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter(|(_, seat)| **seat == guest_seat)
        .filter_map(|(&guest_keyboard_id, _)| {
            ctx.shadow_table
                .get_host_id(guest_keyboard_id)
                .map(HostId)
                .map(|host_keyboard_id| (guest_keyboard_id, host_keyboard_id))
        })
        .collect();
    candidates.sort_unstable_by_key(|(guest_keyboard_id, _)| *guest_keyboard_id);

    candidates
        .iter()
        .find(|(_, host_keyboard_id)| {
            !ctx.key_generations.backspace_repeat_cancelled(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            ) && ctx.key_generations.physically_held(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            )
        })
        .copied()
        .or_else(|| candidates.into_iter().next())
}

pub(crate) fn end_backspace_repeat_for_seat(ctx: &mut Context, guest_seat: u32) {
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter_map(|(&guest_keyboard_id, &seat)| {
            (seat == guest_seat)
                .then(|| ctx.shadow_table.get_host_id(guest_keyboard_id).map(HostId))
                .flatten()
        })
        .collect();
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
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter_map(|(&guest_keyboard_id, &seat)| {
            (seat == guest_seat)
                .then(|| ctx.shadow_table.get_host_id(guest_keyboard_id).map(HostId))
                .flatten()
        })
        .collect();
    for host_keyboard_id in host_keyboards {
        ctx.key_generations.cancel_backspace_repeat_for_owner(
            host_keyboard_id,
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            guest_text_input,
        );
    }
}

fn keysym_to_evdev_keycode(sym: u32) -> Option<u32> {
    static KEYCODES: std::sync::OnceLock<std::collections::BTreeMap<u32, u32>> =
        std::sync::OnceLock::new();
    KEYCODES
        .get_or_init(|| {
            let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
            let Some(keymap) = xkbcommon::xkb::Keymap::new_from_names(
                &context,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) else {
                log::error!("Could not initialize the fallback XKB keysym map");
                return std::collections::BTreeMap::new();
            };

            let mut keycodes = std::collections::BTreeMap::new();
            for keycode_raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                if keycode_raw < 8 {
                    // Wayland exposes evdev keycodes (XKB code minus 8);
                    // malformed/custom keymaps below the XKB offset cannot be
                    // represented without underflow.
                    continue;
                }
                let keycode = xkbcommon::xkb::Keycode::new(keycode_raw);
                let layout_count = keymap.num_layouts_for_key(keycode);
                for layout in 0..layout_count {
                    let level_count = keymap.num_levels_for_key(keycode, layout);
                    for level in 0..level_count {
                        for keysym in keymap.key_get_syms_by_level(keycode, layout, level) {
                            // Include shifted and alternate-layout symbols.
                            // Host text-input `keysym` events contain the
                            // effective symbol (for example `A`, not only
                            // the base-level `a`), so scanning level 0 alone
                            // silently dropped uppercase and non-US layouts.
                            // Preserve the linear scan's choice of the
                            // lowest keycode when a keysym has aliases.
                            keycodes.entry(keysym.raw()).or_insert(keycode_raw - 8);
                        }
                    }
                }
            }
            keycodes
        })
        .get(&sym)
        .copied()
}

/// Select a keyboard for a host `keysym` event.
///
/// A keysym is seat-scoped rather than tied to a particular `wl_keyboard`
/// object. If one of several keyboards on the seat is currently reserved for
/// the IME's held-Backspace fallback, routing an ordinary character through
/// that object can make the focused guest client miss the character. Prefer a
/// keyboard that is not in that fallback session for non-Backspace keysyms;
/// Backspace itself still uses the keyboard carrying the physical key.
fn keyboard_for_keysym(ctx: &Context, guest_seat: u32, sym: u32) -> Option<(u32, HostId)> {
    let mut candidates: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter(|(_, seat)| **seat == guest_seat)
        .filter_map(|(&guest_keyboard_id, _)| {
            ctx.shadow_table
                .get_host_id(guest_keyboard_id)
                .map(HostId)
                .map(|host_keyboard_id| (guest_keyboard_id, host_keyboard_id))
        })
        .collect();
    candidates.sort_unstable_by_key(|(guest_keyboard_id, _)| *guest_keyboard_id);

    // A host keysym is seat-scoped, but the guest event still has to be sent
    // through the wl_keyboard whose surface owns the seat's current focus.
    // With multiple keyboard objects on one seat, choosing solely by guest ID
    // can route text to a stale keyboard (and therefore to the wrong client).
    let active_surface = ctx.keyboard_focus.surface_for_seat(guest_seat);
    let is_focused = |(_, host_keyboard_id): &(u32, HostId)| {
        active_surface.is_some_and(|surface| {
            ctx.keyboard_focus
                .keyboard_owns_surface(*host_keyboard_id, surface)
        })
    };

    // Keep text-input keysym delivery on the same wl_keyboard object as the
    // physical generation observed through peek_key. Recovery and duplicate
    // suppression are host-keyboard scoped; routing the keysym to a different
    // object on the same seat would split ownership and could emit the key
    // twice. A globally monotonic peek sequence makes this deterministic even
    // when several keyboard resources share the focused surface.
    if let Some((_, candidate)) = candidates
        .iter()
        .filter(|candidate| active_surface.is_none() || is_focused(candidate))
        .filter_map(|candidate| {
            let keycode = ctx
                .keyboard_keysym_to_keycode
                .get(&candidate.1)
                .and_then(|keycodes| keycodes.get(&sym))
                .copied()
                .or_else(|| keysym_to_evdev_keycode(sym))?;
            let sequence = ctx
                .key_generations
                .peek(candidate.1, keycode)
                .map(|press| press.sequence)?;
            Some((sequence, *candidate))
        })
        .max_by_key(|(sequence, _)| *sequence)
    {
        return Some(candidate);
    }

    // If several keyboard objects share the focused surface, retain the
    // existing preference for one that is not reserved for IME Backspace
    // fallback. Otherwise, any focused keyboard takes precedence over stale
    // unfocused objects.
    if sym != xkbcommon::xkb::keysyms::KEY_BackSpace {
        if let Some(candidate) = candidates.iter().find(|candidate| {
            is_focused(candidate)
                && (ctx.key_generations.backspace_repeat_cancelled(
                    candidate.1,
                    crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
                ) || !ctx
                    .key_generations
                    .physically_held(candidate.1, crate::handler::keyboard::EVDEV_KEY_BACKSPACE))
        }) {
            return Some(*candidate);
        }
    }
    if let Some(candidate) = candidates.iter().find(|candidate| is_focused(candidate)) {
        return Some(*candidate);
    }

    // No keyboard is currently associated with the seat's focused surface.
    // Fall back to the previous deterministic selection policy.
    if sym != xkbcommon::xkb::keysyms::KEY_BackSpace {
        if let Some(candidate) = candidates.iter().find(|(_, host_keyboard_id)| {
            ctx.key_generations.backspace_repeat_cancelled(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            ) || !ctx.key_generations.physically_held(
                *host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            )
        }) {
            return Some(*candidate);
        }
    }

    keyboard_for_seat(ctx, guest_seat).or_else(|| candidates.into_iter().next())
}

/// Forward host `zwp_text_input_v1.keysym` as a `wl_keyboard.key` of the
/// seat's keyboard (see the call in `TextInputV1Handler::on_keysym`).
pub(super) fn on_keysym(ctx: &mut Context, serial: u32, time: u32, sym: u32, state: u32) -> Action {
    // Exo intentionally sends these two fields contrary to the unstable
    // protocol names for compatibility with Lacros: `serial` contains the
    // event timestamp and `time` contains the Wayland serial. Translate
    // them back to wl_keyboard semantics here, and use only the Wayland
    // serial for cross-channel generation correlation.
    let timestamp_millis = serial;
    let wayland_serial = time;
    let host_id = ctx.last_sender_id;
    let sym_char = std::char::from_u32(sym)
        .map(|c| c.to_string())
        .unwrap_or_default();
    let Some((guest_id, guest_seat)) = active_guest_for_host_text_input(ctx, host_id) else {
        log::debug!(
            "Ignoring stale keysym for inactive host text input {}",
            host_id
        );
        return Action::Drop;
    };
    log::trace!(
        ">>> on_keysym: host_id={}, guest_id={}, serial={}, time={}, sym=0x{:x} ({:?}), state={}",
        host_id,
        guest_id,
        wayland_serial,
        timestamp_millis,
        sym,
        sym_char,
        state
    );

    let keyboard = keyboard_for_keysym(ctx, guest_seat, sym);
    if let Some((keyboard_id, host_keyboard_id)) = keyboard {
        if !matches!(
            state,
            wl_keyboard::KEY_STATE_PRESSED
                | wl_keyboard::KEY_STATE_REPEATED
                | wl_keyboard::KEY_STATE_RELEASED
        ) {
            log::warn!(
                "Ignoring keysym with invalid key state {} for sym=0x{:x}",
                state,
                sym
            );
            return Action::Drop;
        }
        let keycode = ctx
            .keyboard_keysym_to_keycode
            .get(&host_keyboard_id)
            .and_then(|keycodes| keycodes.get(&sym))
            .copied()
            .or_else(|| keysym_to_evdev_keycode(sym));
        if let Some(keycode) = keycode {
            let event = match state {
                wl_keyboard::KEY_STATE_PRESSED => GuestKeyEvent::TextInputPress {
                    serial: wayland_serial,
                },
                wl_keyboard::KEY_STATE_REPEATED => GuestKeyEvent::TextInputRepeat,
                wl_keyboard::KEY_STATE_RELEASED => GuestKeyEvent::TextInputRelease {
                    serial: wayland_serial,
                },
                _ => unreachable!("keysym state validated above"),
            };
            let decision = ctx.transition_guest_key(host_keyboard_id, keycode, event);
            if decision.delivery != GuestKeyDelivery::Forward {
                log::debug!(
                    "Dropping text-input keysym for host keyboard {} key {} state {}",
                    host_keyboard_id.0,
                    keycode,
                    state
                );
                return Action::Drop;
            }
            if ((state == wl_keyboard::KEY_STATE_PRESSED
                || state == wl_keyboard::KEY_STATE_REPEATED)
                && keycode != crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
                || (decision.ends_repeat
                    && keycode == crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
            {
                end_backspace_repeat_for_seat(ctx, guest_seat);
            }
            log::debug!(
                "  -> forwarding wl_keyboard.key: keyboard_id={}, serial={}, time={}, keycode={}, state={}",
                keyboard_id, wayland_serial, timestamp_millis, keycode, state
            );
            ctx.host_to_client_queue.push(wl_keyboard::evt::key(
                keyboard_id,
                wayland_serial,
                timestamp_millis,
                keycode,
                state,
            ));
        } else {
            log::warn!(
                "  -> could not find keycode for sym=0x{:x} ({:?})",
                sym,
                sym_char
            );
        }
    } else {
        log::warn!("  -> no wl_keyboard found to forward keysym to");
    }
    Action::Drop
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
pub(super) fn held_repeat_key_for_seat(ctx: &Context, guest_seat: u32) -> Option<HeldRepeatKey> {
    let active_surface = ctx.keyboard_focus.surface_for_seat(guest_seat);
    let mut candidates = Vec::new();

    for (&guest_keyboard_id, &seat) in &ctx.keyboard_to_seat {
        if seat != guest_seat {
            continue;
        }
        let Some(host_keyboard_id) = ctx.shadow_table.get_host_id(guest_keyboard_id).map(HostId)
        else {
            continue;
        };
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

    let (_, _, guest_keyboard_id, host_keyboard_id, key, press) = candidates
        .into_iter()
        .filter(|candidate| active_surface.is_none() || candidate.0)
        .max_by_key(|candidate| candidate.1)?;

    let latest_sequence = ctx
        .key_generations
        .latest_peek_sequence(guest_seat, active_surface);
    let is_latest_generation = latest_sequence.is_none_or(|sequence| sequence == press.sequence);
    let physically_held = ctx.key_generations.physically_held(host_keyboard_id, key);
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
        || press.stale
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
            press.stale,
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

/// Send the guest a press and a release of the held key the host input
/// method consumed, unless another channel already delivered it.
///
/// Both carry the serial and time of the key's latest `peek_key`, which
/// are those of the `wl_keyboard.key` Exo would have sent; see
/// INVARIANTS "A recovered key carries the host's serial".
pub(super) fn synthesize_ime_consumed_key_pair(ctx: &mut Context, held: HeldRepeatKey) -> bool {
    let decision =
        ctx.transition_guest_key(held.host_keyboard_id, held.key, GuestKeyEvent::RecoverIme);
    if decision.delivery != GuestKeyDelivery::EmitBalancedPair {
        // The normal wl_keyboard path already delivered this physical
        // generation to the guest. The confirmation still closes a
        // text-input transaction, but another key pair would duplicate it.
        log::debug!(
            "Skipping IME repeat recovery: key {} was already forwarded for host keyboard {}",
            held.key,
            held.host_keyboard_id.0
        );
        return false;
    }
    for state in [
        wl_keyboard::KEY_STATE_PRESSED,
        wl_keyboard::KEY_STATE_RELEASED,
    ] {
        ctx.host_to_client_queue.push(wl_keyboard::evt::key(
            held.guest_keyboard_id,
            held.serial,
            held.time,
            held.key,
            state,
        ));
    }
    true
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
/// - the `peek_key` press of a key the keyboard does not hold leaves it
///   waiting, as that key's possible press;
/// - that key's `wl_keyboard.key` press, with the peek's serial, shows that
///   the input method did not handle the press, so the confirmation was
///   its own: no key is recovered, as when a confirmation finds no held
///   key;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingRecovery {
    ti: TiId,
    seat: SeatId,
    /// The seat's eligible held key when the confirmation came.
    held: HeldRepeatKey,
    /// The `peek_key` press of a key not held before, since the
    /// confirmation: its host keyboard, key and serial.
    press: Option<(HostId, u32, u32)>,
}

impl PendingRecovery {
    /// The guest text input, guest keyboard and host keyboard it names.
    #[cfg(test)]
    pub(crate) fn objects_for_test(&self) -> (u32, u32, HostId) {
        let held = self.held;
        (self.ti.0, held.guest_keyboard_id, held.host_keyboard_id)
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
        press: None,
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
    let mut msg = crate::wire::WireMessage::new(sender, opcode, payload);
    match (interface, pending.press) {
        (Some(wl_keyboard::NAME), None) if opcode == wl_keyboard::EVT_MODIFIERS => {}
        (Some(zcr_extended_keyboard_v1::NAME), None) => {
            let peek = zcr_extended_keyboard_v1::Event::from_wire(&mut msg);
            let Ok(zcr_extended_keyboard_v1::Event::PeekKey {
                serial, key, state, ..
            }) = peek
            else {
                return recover_pending(ctx);
            };
            let keyboard = ctx
                .extended_keyboard_to_keyboard
                .get(&HostId(sender))
                .copied();
            let press = keyboard.filter(|&keyboard| {
                let seat = ctx
                    .shadow_table
                    .get_guest_id(keyboard.0)
                    .and_then(|guest| ctx.keyboard_to_seat.get(&guest));
                state == wl_keyboard::KEY_STATE_PRESSED
                    && seat == Some(&pending.seat.0)
                    && !ctx.key_generations.physically_held(keyboard, key)
            });
            match press {
                Some(keyboard) => {
                    let pending = ctx.pending_recovery.as_mut().expect("checked above");
                    pending.press = Some((keyboard, key, serial));
                }
                None => recover_pending(ctx),
            }
        }
        (Some(wl_keyboard::NAME), Some(press)) if opcode == wl_keyboard::EVT_KEY => {
            let own_press = match wl_keyboard::Event::from_wire(&mut msg) {
                Ok(wl_keyboard::Event::Key {
                    serial, key, state, ..
                }) => {
                    state == wl_keyboard::KEY_STATE_PRESSED
                        && press == (HostId(sender), key, serial)
                }
                _ => false,
            };
            match own_press {
                true => drop_pending(ctx),
                false => recover_pending(ctx),
            }
        }
        (Some(zwp_text_input_v1::NAME), _)
            if opcode == zwp_text_input_v1::EVT_LEAVE
                && ctx.shadow_table.get_guest_id(sender) == Some(pending.ti.0) =>
        {
            drop_pending(ctx);
        }
        _ => recover_pending(ctx),
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
    let done = keyboard_held.then(|| ctx.ime.recovery_done(ti)).flatten();
    let Some(done) = done else {
        end_backspace_repeat_for_text_input(ctx, seat.0, ti.0);
        log::debug!(
            "Dropping the recovery of held key {} for text input {}, which no longer \
             passes the gate or holds the key",
            held.key,
            ti.0
        );
        return;
    };
    let backspace = held.key == crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
    if synthesize_ime_consumed_key_pair(ctx, held) {
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
    // state again.
    ctx.host_to_client_queue.extend(done);
}

/// H2: the input method detached text input on its own, as when its
/// process restarts. The key events in flight died with it, as their
/// callbacks never come, so a held key's release may never reach the
/// keyboard, and the keys it took are no longer its to confirm. No key a
/// keyboard of `guest_seat` holds now is recovered again; its next press
/// starts a generation that can be. This ends only the recovery: the keys
/// are still guest input, so their keysyms reach the guest.
pub(super) fn forget_held_keys_for_seat(ctx: &mut Context, guest_seat: u32) {
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter(|(_, &seat)| seat == guest_seat)
        .filter_map(|(&guest, _)| ctx.shadow_table.get_host_id(guest).map(HostId))
        .collect();
    for keyboard in host_keyboards {
        ctx.key_generations.mark_peeks_stale(keyboard);
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

    /// An IME edit ends the held-Backspace fallback a confirmation leased.
    #[test]
    fn ime_edits_end_a_stale_backspace_repeat() {
        type Event = fn(&mut Ime);
        let cases: [(&str, Event); 3] = [
            ("an empty commit", |ime| ime.commit("")),
            ("a reset preedit", |ime| ime.preedit("", "확정")),
            ("a preedit region", |ime| ime.preedit_region(-3, 3)),
        ];
        for (case, event) in cases {
            let mut ime = Ime::with_keyboard();
            ime.committed().surrounding_text = Some(("가나다".to_string(), 9, 9));
            ime.state().show_for_test("가");
            ime.lease_backspace_repeat(TI);

            event(&mut ime);
            assert!(!ime.repeat_active(), "{case}");
        }
    }

    #[test]
    fn input_mode_switch_boundary_clears_stale_repeat_before_empty_confirmation() {
        let mut ime = Ime::with_keyboard();
        ime.committed().surrounding_text = Some(("english".to_string(), 7, 7));
        ime.lease_backspace_repeat(TI);
        end_backspace_repeat_for_seat(&mut ime.ctx, SEAT);

        ime.confirm();
        assert_eq!(
            ime.events(),
            [],
            "a routine empty confirmation must not become a synthetic Backspace"
        );
        assert_eq!(
            ime.committed().surrounding_text,
            Some(("english".to_string(), 7, 7))
        );
    }

    /// Exo consumes a held Backspace over committed text and confirms an
    /// empty preedit for each repeat: each becomes a key pair with the
    /// serial of the held key's peek, and the repeat stays leased.
    #[test]
    fn held_backspace_repeats_over_committed_korean_without_preedit() {
        let mut ime = Ime::with_keyboard();
        ime.hold_peeked(HOST_KEYBOARD, BACKSPACE, 700, 100);
        ime.committed().surrounding_text = Some(("가나다라".to_string(), 12, 12));

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
        generations.observe_peek_press(HOST_KEYBOARD, SHIFT, 701, 101, true);
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
        ime.committed().surrounding_text = Some(("A가".to_string(), 4, 4));
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
            assert_eq!(
                ime.committed().surrounding_text,
                Some(("A가".to_string(), 4, 4))
            );
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
        ime.committed().surrounding_text = Some(("A가".to_string(), 4, 4));
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

    /// Exo sends a keysym's timestamp as its serial and its serial as its
    /// time; the guest gets them back in their places, as a key pair of
    /// the text input's that invents no physical key state.
    #[test]
    fn on_keysym_translates_exo_timestamp_and_serial_to_wl_keyboard() {
        use xkbcommon::xkb::keysyms::KEY_BackSpace;
        let mut ime = Ime::with_keyboard();
        ime.keysym(KEY_BackSpace, PRESSED, 456, 123);
        assert_eq!(ime.events(), [key(KEYBOARD, BACKSPACE, PRESSED, 456, 123)]);
        let generations = &ime.ctx.key_generations;
        assert!(!generations.physically_held(HOST_KEYBOARD, BACKSPACE));
        assert_eq!(ime.owner(BACKSPACE), Some(GuestKeyOwner::TextInputKeysym));

        ime.keysym(KEY_BackSpace, RELEASED, 457, 124);
        assert_eq!(ime.events(), [key(KEYBOARD, BACKSPACE, RELEASED, 457, 124)]);
        let generations = &ime.ctx.key_generations;
        assert!(!generations.physically_held(HOST_KEYBOARD, BACKSPACE));
        assert_eq!(
            ime.owner(BACKSPACE),
            Some(GuestKeyOwner::ImeRecovery),
            "the release leaves a completed-generation tombstone"
        );
    }

    /// A keysym of a key another owner holds for the guest, its physical
    /// press or a pair synthesized for the IME, neither duplicates nor
    /// steals its press or release.
    #[test]
    fn keysyms_leave_a_key_another_owner_holds() {
        use xkbcommon::xkb::keysyms::KEY_BackSpace;
        for (owner, state) in [
            (GuestKeyOwner::Physical, PRESSED),
            (GuestKeyOwner::Physical, RELEASED),
            (GuestKeyOwner::ImeRecovery, PRESSED),
        ] {
            let mut ime = Ime::with_keyboard();
            ime.hold_owned(BACKSPACE, owner);

            ime.keysym(KEY_BackSpace, state, 456, 123);
            let case = format!("{owner:?} {state}");
            assert_eq!(ime.events(), [], "{case}");
            assert_eq!(ime.owner(BACKSPACE), Some(owner), "{case}");
            let generations = &ime.ctx.key_generations;
            assert!(
                generations.physically_held(HOST_KEYBOARD, BACKSPACE),
                "{case}"
            );
        }
    }

    /// A repeated keysym repeats a guest press, and invents none.
    #[test]
    fn repeated_keysym_forwards_only_for_an_existing_guest_press() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        for pressed in [true, false] {
            let mut ime = Ime::with_keyboard();
            if pressed {
                ime.hold_owned(a, GuestKeyOwner::Physical);
            }
            ime.keysym(KEY_a, REPEATED, 456, 123);
            let expected = Vec::from_iter(pressed.then(|| key(KEYBOARD, a, REPEATED, 456, 123)));
            assert_eq!(ime.events(), expected, "pressed: {pressed}");
        }
    }

    /// A keysym press whose serial a completed generation had is a delayed
    /// duplicate, also across the serial's wrap; a newer one opens the next
    /// generation.
    #[test]
    fn keysym_serial_distinguishes_delayed_duplicate_from_next_generation() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        // Each sequence opens and closes two generations; the presses that
        // repeat serial 1 are dropped.
        for (keysyms, forwarded) in [
            (
                &[
                    (1, PRESSED),
                    (2, RELEASED),
                    (1, PRESSED),
                    (3, PRESSED),
                    (4, RELEASED),
                    (1, PRESSED),
                ][..],
                [1, 2, 3, 4],
            ),
            (
                &[
                    (u32::MAX - 1, PRESSED),
                    (u32::MAX, RELEASED),
                    (0, PRESSED),
                    (1, RELEASED),
                ],
                [u32::MAX - 1, u32::MAX, 0, 1],
            ),
        ] {
            let mut ime = Ime::with_keyboard();
            for &(serial, state) in keysyms {
                ime.keysym(KEY_a, state, serial, 10);
            }
            let states = [PRESSED, RELEASED, PRESSED, RELEASED];
            let expected = Vec::from_iter(
                states
                    .into_iter()
                    .zip(forwarded)
                    .map(|(state, serial)| key(KEYBOARD, a, state, serial, 10)),
            );
            assert_eq!(ime.events(), expected, "{keysyms:?}");
        }
    }

    /// The physical channel's press of a generation that started as a
    /// keysym is no second press.
    #[test]
    fn delayed_peek_and_keyboard_press_keep_keysym_pair_balanced() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        let mut ime = Ime::with_extended_keyboard();
        ime.keysym(KEY_a, PRESSED, 1, 10);
        ime.peek(a, PRESSED, 1, 10);
        assert_eq!(
            ime.key(a, PRESSED, 1, 10),
            Action::Drop,
            "the delayed keyboard channel must not emit a duplicate press"
        );
        ime.keysym(KEY_a, RELEASED, 2, 20);
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, a, PRESSED, 1, 10),
                key(KEYBOARD, a, RELEASED, 2, 20)
            ]
        );
        assert_eq!(ime.owner(a), Some(GuestKeyOwner::ImeRecovery));
    }

    #[test]
    fn released_tombstone_accepts_new_keysym_before_new_peek() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        let mut ime = Ime::with_extended_keyboard();
        ime.peek(a, PRESSED, 0x8000_0010, 1000);
        ime.keysym(KEY_a, PRESSED, 0x8000_0010, 1000);
        ime.keysym(KEY_a, RELEASED, 0x8000_0011, 1010);
        ime.peek(a, RELEASED, 0x8000_0012, 1020);

        ime.keysym(KEY_a, PRESSED, 0x8000_0020, 1100);
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, a, PRESSED, 0x8000_0010, 1000),
                key(KEYBOARD, a, RELEASED, 0x8000_0011, 1010),
                key(KEYBOARD, a, PRESSED, 0x8000_0020, 1100),
            ]
        );
        assert_eq!(ime.owner(a), Some(GuestKeyOwner::TextInputKeysym));
        ime.peek(a, PRESSED, 0x8000_0020, 1100);
        assert_eq!(
            ime.owner(a),
            Some(GuestKeyOwner::TextInputKeysym),
            "the delayed physical channel must attach to the keysym-first generation"
        );
    }

    #[test]
    fn released_generation_drops_delayed_keysym_press_before_release_boundary() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        let mut ime = Ime::with_extended_keyboard();
        ime.peek(a, PRESSED, u32::MAX - 3, 1000);
        ime.keysym(KEY_a, PRESSED, u32::MAX - 2, 1000);
        ime.peek(a, RELEASED, 0, 1010);

        ime.keysym(KEY_a, PRESSED, u32::MAX - 1, 1005);
        assert_eq!(
            ime.events(),
            [key(KEYBOARD, a, PRESSED, u32::MAX - 2, 1000)],
            "a delayed press before the wrapped release boundary must not reach the guest"
        );
        let generations = &ime.ctx.key_generations;
        assert_eq!(
            generations.guest_press_serial(HOST_KEYBOARD, a),
            Some(u32::MAX - 2)
        );

        ime.keysym(KEY_a, PRESSED, 1, 1100);
        ime.keysym(KEY_a, RELEASED, u32::MAX, 1010);
        ime.keysym(KEY_a, RELEASED, 2, 1110);
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, a, PRESSED, 1, 1100),
                key(KEYBOARD, a, RELEASED, u32::MAX, 1010),
                key(KEYBOARD, a, RELEASED, 2, 1110),
            ],
            "the old and current generations must each retain one balanced key pair"
        );
    }

    #[test]
    fn delayed_keysym_release_closes_retired_generation_only() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        let mut ime = Ime::with_extended_keyboard();
        ime.peek(a, PRESSED, 10, 100);
        ime.keysym(KEY_a, PRESSED, 10, 100);
        ime.peek(a, RELEASED, 11, 110);
        ime.peek(a, PRESSED, 20, 200);

        ime.keysym(KEY_a, RELEASED, 11, 110);
        assert!(
            ime.ctx.key_generations.physically_held(HOST_KEYBOARD, a),
            "the retired release must not close the current physical generation"
        );
        assert_eq!(
            ime.owner(a),
            None,
            "the new generation must remain available after the retired release"
        );

        ime.keysym(KEY_a, PRESSED, 20, 200);
        ime.keysym(KEY_a, RELEASED, 21, 210);
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, a, PRESSED, 10, 100),
                key(KEYBOARD, a, RELEASED, 11, 110),
                key(KEYBOARD, a, PRESSED, 20, 200),
                key(KEYBOARD, a, RELEASED, 21, 210),
            ],
            "each physical generation must produce one balanced guest pair"
        );
    }

    /// The releases of a retired keysym generation and of the current one
    /// each close their own press, in either order.
    #[test]
    fn current_keysym_release_does_not_consume_retired_release() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        let mut ime = Ime::with_extended_keyboard();
        ime.peek(a, PRESSED, 10, 100);
        ime.keysym(KEY_a, PRESSED, 10, 100);
        ime.peek(a, RELEASED, 11, 110);
        ime.peek(a, PRESSED, 20, 200);

        ime.keysym(KEY_a, PRESSED, 20, 200);
        ime.keysym(KEY_a, RELEASED, 21, 210);
        assert_eq!(
            ime.owner(a),
            Some(GuestKeyOwner::ImeRecovery),
            "the current release must complete the current owner"
        );
        ime.keysym(KEY_a, RELEASED, 11, 110);
        assert_eq!(
            ime.owner(a),
            Some(GuestKeyOwner::ImeRecovery),
            "the delayed retired release must not disturb the current tombstone"
        );
        assert_eq!(
            ime.events(),
            [
                key(KEYBOARD, a, PRESSED, 10, 100),
                key(KEYBOARD, a, PRESSED, 20, 200),
                key(KEYBOARD, a, RELEASED, 21, 210),
                key(KEYBOARD, a, RELEASED, 11, 110),
            ],
            "both guest presses must eventually receive one release"
        );
    }

    /// The next keysym press is the first event of its generation: it
    /// retires the still-open previous keysym owner before the new peek
    /// catches up, and either release order balances both.
    #[test]
    fn keysym_first_generation_retires_open_previous_owner() {
        use xkbcommon::xkb::keysyms::KEY_a;
        let a = keysym_to_evdev_keycode(KEY_a).expect("the fallback keymap has KEY_a");
        for old_release_first in [true, false] {
            let mut ime = Ime::with_extended_keyboard();
            ime.peek(a, PRESSED, u32::MAX - 3, 1000);
            ime.keysym(KEY_a, PRESSED, u32::MAX - 2, 1000);
            ime.peek(a, RELEASED, u32::MAX - 1, 1010);

            ime.keysym(KEY_a, PRESSED, 0, 1100);
            assert_eq!(ime.owner(a), Some(GuestKeyOwner::TextInputKeysym));
            ime.peek(a, PRESSED, 1, 1100);

            let mut releases = [(u32::MAX - 1, 1010), (2, 1200)];
            if !old_release_first {
                releases.reverse();
            }
            for (serial, time) in releases {
                ime.keysym(KEY_a, RELEASED, serial, time);
            }
            assert_eq!(
                ime.owner(a),
                Some(GuestKeyOwner::ImeRecovery),
                "the current owner must complete regardless of release order"
            );
            let mut expected = vec![
                key(KEYBOARD, a, PRESSED, u32::MAX - 2, 1000),
                key(KEYBOARD, a, PRESSED, 0, 1100),
            ];
            expected
                .extend(releases.map(|(serial, time)| key(KEYBOARD, a, RELEASED, serial, time)));
            assert_eq!(
                ime.events(),
                expected,
                "both keysym-first generations must remain balanced"
            );
        }
    }

    /// Without a focused keyboard on the seat, a keysym takes the keyboard
    /// with the lowest ID, whatever order a map holds them in, but one
    /// whose Backspace the IME fallback holds only for Backspace.
    #[test]
    fn on_keysym_selects_guest_keyboard_deterministically() {
        use xkbcommon::xkb::keysyms::{KEY_BackSpace, KEY_A};
        let mut ime = Ime::new();
        ime.keyboard(999, HostId(888));
        ime.keyboard(100, HostId(777));
        ime.keysym(KEY_BackSpace, PRESSED, 456, 123);
        assert_eq!(ime.events(), [key(100, BACKSPACE, PRESSED, 456, 123)]);

        let mut ime = Ime::new();
        ime.keyboard(100, HostId(101));
        ime.keyboard(200, HostId(201));
        let generations = &mut ime.ctx.key_generations;
        generations.observe_physical_state(HostId(101), BACKSPACE, 1);
        ime.keysym(KEY_A, PRESSED, 456, 123);
        let [(keyboard, _)] = ime.events()[..] else {
            panic!("the keysym reached no one keyboard");
        };
        assert_eq!(
            keyboard, 200,
            "an ordinary keysym must not be routed to the keyboard reserved for IME Backspace fallback"
        );
    }

    #[test]
    fn keysym_prefers_keyboard_with_current_surface_focus() {
        use xkbcommon::xkb::keysyms::KEY_A;
        let mut ime = Ime::new();
        ime.keyboard(100, HostId(101));
        ime.keyboard(200, HostId(201));
        ime.focus(HostId(201));

        ime.keysym(KEY_A, PRESSED, 456, 123);
        let [(keyboard, _)] = ime.events()[..] else {
            panic!("the keysym reached no one keyboard");
        };
        assert_eq!(
            keyboard, 200,
            "a host keysym must target the keyboard whose surface owns the seat focus"
        );
    }

    #[test]
    fn fallback_keysym_lookup_is_stable_and_reusable() {
        use xkbcommon::xkb::keysyms::{KEY_BackSpace, KEY_a, KEY_A};
        for _ in 0..2 {
            assert_eq!(keysym_to_evdev_keycode(KEY_BackSpace), Some(BACKSPACE));
        }
        assert_eq!(keysym_to_evdev_keycode(u32::MAX), None);
        assert!(
            keysym_to_evdev_keycode(KEY_A).is_some(),
            "uppercase keysym events must map to the same physical key"
        );
        assert_eq!(
            keysym_to_evdev_keycode(KEY_a),
            keysym_to_evdev_keycode(KEY_A)
        );
    }
}
