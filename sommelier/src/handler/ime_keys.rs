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

use crate::handler::text_input::active_guest_for_host_text_input;
use crate::protocols::wayland::wl_keyboard;
use crate::state::{Context, GuestKeyDelivery, GuestKeyEvent, HostId};
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

pub(crate) fn held_backspace_keyboard_for_seat(ctx: &Context, guest_seat: u32) -> Option<HostId> {
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

pub(crate) fn end_backspace_repeat_for_text_input(
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

pub(crate) fn keysym_to_evdev_keycode(sym: u32) -> Option<u32> {
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
pub(crate) fn on_keysym(ctx: &mut Context, serial: u32, time: u32, sym: u32, state: u32) -> Action {
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
    let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) else {
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

    let guest_seat = ctx.text_inputs.get(&guest_id).map(|state| state.guest_seat);
    let keyboard = guest_seat.and_then(|seat| keyboard_for_keysym(ctx, seat, sym));
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
                if let Some(guest_seat) = guest_seat {
                    end_backspace_repeat_for_seat(ctx, guest_seat);
                }
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
