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

use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::protocols::wayland::{wl_display, wl_keyboard};
#[cfg(test)]
use crate::state::GuestKeyOwner;
use crate::state::{
    Context, GuestId, GuestKeyDelivery, GuestKeyEvent, HostActivationState, HostId, SeatFocusChange,
};
use crate::wire::{Action, MessageBuilder};
use std::os::unix::io::RawFd;

/// Push a wire message built by `builder` onto `queue`, binding it to
/// `sender_id` / `opcode`. Centralizes the (header + payload) assembly that
/// used to be open-coded with `extend_from_slice` + `(len << 16) | opcode`.
fn push_msg(
    queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>,
    sender_id: u32,
    opcode: u16,
    builder: MessageBuilder,
) -> bool {
    match builder.try_build_message(sender_id, opcode) {
        Ok(message) => {
            queue.push((message, Vec::new()));
            true
        }
        Err(error) => {
            // Host-originated text is untrusted. A malformedly large commit
            // or preedit must be dropped, not allowed to panic the proxy while
            // encoding Wayland's 16-bit message length.
            log::warn!(
                "Dropping oversized text-input message sender={} opcode={}: {}",
                sender_id,
                opcode,
                error
            );
            false
        }
    }
}

fn push_done_to(queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>, guest_id: u32, serial: u32) -> bool {
    let mut builder = MessageBuilder::new();
    builder.write_u32(serial);
    push_msg(queue, guest_id, 5, builder)
}

fn push_done(ctx: &mut Context, guest_id: u32, serial: u32) {
    push_done_to(&mut ctx.host_to_client_queue, guest_id, serial);
}

fn active_guest_for_host_text_input(ctx: &Context, host_id: u32) -> Option<u32> {
    let guest_id = ctx.shadow_table.get_guest_id(host_id)?;
    ctx.text_inputs
        .get(&guest_id)
        .filter(|state| state.host_is_active())
        .map(|_| guest_id)
}

fn resolve_preedit_cursor(
    text: &str,
    selection: Option<(u32, u32)>,
    cursor: Option<i32>,
) -> (i32, i32) {
    if let Some((index, length)) = selection {
        let end = index.saturating_add(length);
        if end <= text.len() as u32
            && text.is_char_boundary(index as usize)
            && text.is_char_boundary(end as usize)
        {
            return (index as i32, end as i32);
        }
        log::warn!(
            "Ignoring invalid preedit selection: index={}, length={}, text_len={}",
            index,
            length,
            text.len()
        );
    } else if let Some(cursor) = cursor {
        if cursor < 0 {
            return (-1, -1);
        }
        if (cursor as usize) <= text.len() && text.is_char_boundary(cursor as usize) {
            return (cursor, cursor);
        }
        log::warn!(
            "Ignoring invalid preedit cursor: cursor={}, text_len={}",
            cursor,
            text.len()
        );
    }

    let end = text.len() as i32;
    (end, end)
}

fn convert_delete_range(index: i32, length: u32) -> Option<(u32, u32)> {
    // v1 specifies a byte range beginning at cursor + index. v3 splits the
    // same range into bytes immediately before and after the cursor. A range
    // separated from the cursor cannot be represented without deleting the
    // intervening text, so reject it rather than corrupting client state.
    // i64 prevents overflow for i32::MIN and for `start + length`.
    let start = i64::from(index);
    let end = start + i64::from(length);
    if start > 0 || end < 0 {
        return None;
    }
    Some((start.unsigned_abs() as u32, end as u32))
}

fn keysym_to_evdev_keycode(sym: u32) -> Option<u32> {
    static KEYCODES: std::sync::OnceLock<std::collections::HashMap<u32, u32>> =
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
                return std::collections::HashMap::new();
            };

            let mut keycodes = std::collections::HashMap::new();
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

fn extension_version_allows(version: u32, required: u32) -> bool {
    // Test fixtures and older callers that predate version tracking use
    // UNKNOWN_OBJECT_VERSION. Keep those permissive while production objects
    // are guarded by the negotiated protocol version.
    version == u32::MAX || version >= required
}

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

fn held_backspace_keyboard_for_seat(ctx: &Context, guest_seat: u32) -> Option<HostId> {
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
fn ime_repeat_active_for_seat(ctx: &Context, guest_seat: u32) -> bool {
    ctx.keyboard_to_seat
        .iter()
        .filter(|(_, seat)| **seat == guest_seat)
        .filter_map(|(&guest_keyboard_id, _)| {
            ctx.shadow_table.host_id_of(GuestId(guest_keyboard_id))
        })
        .any(|host_keyboard_id| {
            ctx.key_generations.ime_repeat_active(
                host_keyboard_id,
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
                .host_id_of(GuestId(guest_keyboard_id))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HeldRepeatKey {
    guest_keyboard_id: u32,
    host_keyboard_id: HostId,
    key: u32,
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
fn held_repeat_key_for_seat(ctx: &Context, guest_seat: u32) -> Option<HeldRepeatKey> {
    let active_surface = ctx.keyboard_focus.surface_for_seat(guest_seat);
    let mut candidates = Vec::new();

    for (&guest_keyboard_id, &seat) in &ctx.keyboard_to_seat {
        if seat != guest_seat {
            continue;
        }
        let Some(host_keyboard_id) = ctx.shadow_table.host_id_of(GuestId(guest_keyboard_id)) else {
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
        || !repeatable
        || backspace_cancelled
    {
        log::debug!(
            "Ignoring empty IME confirmation for ineligible held key {} \
             (latest_generation={}, physically_held={}, generation_eligible={}, \
             repeatable={}, backspace_cancelled={})",
            key,
            is_latest_generation,
            physically_held,
            press.eligible,
            repeatable,
            backspace_cancelled
        );
        return None;
    }

    Some(HeldRepeatKey {
        guest_keyboard_id,
        host_keyboard_id,
        key,
        time: press.time,
    })
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
                .host_id_of(GuestId(guest_keyboard_id))
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

fn synthesize_ime_consumed_key_pair(ctx: &mut Context, held: HeldRepeatKey) -> bool {
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
    for state in [1, 0] {
        ctx.synthetic_keyboard_serial = ctx.synthetic_keyboard_serial.wrapping_add(1).max(1);
        let mut builder = MessageBuilder::new();
        builder.write_u32(ctx.synthetic_keyboard_serial);
        builder.write_u32(held.time);
        builder.write_u32(held.key);
        builder.write_u32(state);
        push_msg(
            &mut ctx.host_to_client_queue,
            held.guest_keyboard_id,
            wl_keyboard::EVT_KEY,
            builder,
        );
    }
    true
}

fn map_v3_content_type(hint: u32, purpose: u32) -> (u32, u32, u32, u32, u32, u32) {
    const V1_SENSITIVE_DATA: u32 = 0x80;
    const INPUT_FLAG_AUTOCOMPLETE_ON: u32 = 1 << 0;
    const INPUT_FLAG_SPELLCHECK_ON: u32 = 1 << 4;
    const INPUT_FLAG_AUTOCAPITALIZE_NONE: u32 = 1 << 6;
    const INPUT_FLAG_AUTOCAPITALIZE_CHARACTERS: u32 = 1 << 7;
    const INPUT_FLAG_AUTOCAPITALIZE_WORDS: u32 = 1 << 8;
    const INPUT_FLAG_AUTOCAPITALIZE_SENTENCES: u32 = 1 << 9;
    const INPUT_FLAG_HAS_BEEN_PASSWORD: u32 = 1 << 10;

    // zwp_text_input_v1 has no PIN purpose and its values after PASSWORD are
    // shifted by one compared with v3.
    let mut v1_hint = hint;
    let v1_purpose = match purpose {
        0..=8 => purpose,
        9 => {
            v1_hint |= V1_SENSITIVE_DATA;
            2 // PIN -> DIGITS + sensitive_data
        }
        10 => 9,  // DATE
        11 => 10, // TIME
        12 => 11, // DATETIME
        13 => 12, // TERMINAL
        _ => 0,   // Unknown values degrade to NORMAL.
    };

    // Chrome's extension uses TextInputType/TextInputMode enums rather than
    // Wayland's content-purpose enum.
    let input_type = match (purpose, hint & 0x200 != 0) {
        (0 | 1 | 7, true) => 14, // TEXT_AREA
        (2 | 3 | 9, _) => 5,     // NUMBER
        (4, _) => 6,             // TELEPHONE
        (5, _) => 7,             // URL
        (6, _) => 4,             // EMAIL
        (8, _) => 2,             // PASSWORD
        (10, _) => 8,            // DATE
        (11, _) => 12,           // TIME
        (12, _) => 9,            // DATE_TIME
        _ => 1,                  // TEXT
    };
    let input_mode = match purpose {
        2 | 9 => 6, // NUMERIC
        3 => 7,     // DECIMAL
        4 => 3,     // TEL
        5 => 4,     // URL
        6 => 5,     // EMAIL
        _ => 0,     // DEFAULT
    };

    let mut input_flags = 0;
    if hint & 0x1 != 0 {
        input_flags |= INPUT_FLAG_AUTOCOMPLETE_ON;
    }
    if hint & 0x2 != 0 {
        // text-input-v3 calls bit 1 `spellcheck`. Chrome's extension keeps
        // spellchecking distinct from its separate autocorrect flag.
        input_flags |= INPUT_FLAG_SPELLCHECK_ON;
    }
    if hint & 0x4 != 0 {
        input_flags |= INPUT_FLAG_AUTOCAPITALIZE_SENTENCES;
    }
    if hint & 0x8 != 0 {
        input_flags |= INPUT_FLAG_AUTOCAPITALIZE_NONE;
    }
    if hint & 0x10 != 0 {
        input_flags |= INPUT_FLAG_AUTOCAPITALIZE_CHARACTERS;
    }
    if hint & 0x20 != 0 {
        input_flags |= INPUT_FLAG_AUTOCAPITALIZE_WORDS;
    }
    if hint & (0x40 | 0x80) != 0 || matches!(purpose, 8 | 9) {
        input_flags |= INPUT_FLAG_HAS_BEEN_PASSWORD;
    }

    let learning_mode = u32::from(!matches!(purpose, 8 | 9) && hint & (0x40 | 0x80) == 0);
    (
        v1_hint,
        v1_purpose,
        input_type,
        input_mode,
        input_flags,
        learning_mode,
    )
}

pub struct TextInputManagerV1Handler;
impl zwp_text_input_manager_v1::ZwpTextInputManagerV1Handler for TextInputManagerV1Handler {}

pub struct TextInputV1Handler;
impl zwp_text_input_v1::ZwpTextInputV1Handler for TextInputV1Handler {
    fn on_preedit_string(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        text: &String,
        commit: &String,
    ) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            return Action::Drop;
        };
        let Some(plan) = state.prepare_host_preedit() else {
            log::debug!(
                "Ignoring stale preedit_string for inactive text input {}",
                guest_id
            );
            return Action::Drop;
        };
        let repeat_keyboard = held_backspace_keyboard_for_seat(ctx, plan.guest_seat);
        let (cursor_begin, cursor_end) = resolve_preedit_cursor(text, plan.selection, plan.cursor);
        if serial != plan.done_serial {
            log::debug!(
                "Host preedit references guest serial {}, current serial is {}",
                serial,
                plan.done_serial
            );
        }

        log::info!(
            "[ime] host preedit_string: host_v1_id={} guest_id={} serial={} \
             text={:?} commit={:?} had_preedit={} guest_done_serial={}",
            host_id,
            guest_id,
            serial,
            text,
            commit,
            plan.had_preedit,
            plan.done_serial
        );

        // The v1 `commit` argument is the replacement text to use if this
        // preedit is reset, not text to insert alongside every live preedit
        // update. Exo commonly sends identical non-empty `text` and `commit`
        // values while composing Korean; forwarding both would insert every
        // intermediate syllable. Apply the fallback only when the host
        // actually clears the preedit (notably on reset/unfocus).
        let mut transaction = Vec::new();
        if plan.had_preedit && text.is_empty() && !commit.is_empty() {
            let mut builder = MessageBuilder::new();
            builder.write_string(commit);
            if !push_msg(&mut transaction, guest_id, 3, builder) {
                return Action::Drop;
            }
        }

        // v3 preedit_string (opcode 2).
        let mut builder = MessageBuilder::new();
        builder.write_string(text);
        builder.write_i32(cursor_begin);
        builder.write_i32(cursor_end);
        if !push_msg(&mut transaction, guest_id, 2, builder)
            || !push_done_to(&mut transaction, guest_id, plan.done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while encoding preedit");
        let guest_seat = plan.guest_seat;
        let arm_backspace_repeat = state.finish_host_preedit(
            plan,
            text.clone(),
            commit.is_empty(),
            repeat_keyboard.is_some(),
        );
        if arm_backspace_repeat {
            let host_keyboard_id = repeat_keyboard.expect("held Backspace was checked above");
            let armed = ctx.key_generations.arm_ime_repeat(
                host_keyboard_id,
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
                guest_id,
            );
            debug_assert!(armed, "held Backspace disappeared within one event");
        } else {
            end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        }

        // Exo allocates v1 event serials independently. text-input-v3 instead
        // requires the number of the latest guest commit request.
        ctx.host_to_client_queue.extend(transaction);
        Action::Drop
    }

    fn on_commit_string(&mut self, ctx: &mut Context, serial: u32, text: &String) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            return Action::Drop;
        };
        let Some(plan) = state.prepare_host_commit() else {
            log::debug!(
                "Ignoring stale commit_string for inactive text input {}",
                guest_id
            );
            return Action::Drop;
        };
        if serial != plan.done_serial {
            log::debug!(
                "Host commit references guest serial {}, current serial is {}",
                serial,
                plan.done_serial
            );
        }

        log::info!(
            "[ime] host commit_string: host_v1_id={} guest_id={} serial={} \
             text={:?} had_preedit={} guest_done_serial={}",
            host_id,
            guest_id,
            serial,
            text,
            plan.had_preedit,
            plan.done_serial
        );

        let mut transaction = Vec::new();
        if plan.had_preedit {
            let mut builder = MessageBuilder::new();
            builder.write_string("");
            builder.write_i32(0);
            builder.write_i32(0);
            if !push_msg(&mut transaction, guest_id, 2, builder) {
                return Action::Drop;
            }
        }

        // v1 requires delete_surrounding_text and cursor_position to be
        // applied as part of the following commit_string. Preserve that
        // transaction boundary when translating to v3.
        for &(before, after) in &plan.deletes {
            let mut builder = MessageBuilder::new();
            builder.write_u32(before);
            builder.write_u32(after);
            if !push_msg(&mut transaction, guest_id, 4, builder) {
                return Action::Drop;
            }
        }

        let mut builder = MessageBuilder::new();
        builder.write_string(text);
        if !push_msg(&mut transaction, guest_id, 3, builder)
            || !push_done_to(&mut transaction, guest_id, plan.done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while encoding commit");
        let pending_cursor_position = plan.cursor_position;
        let guest_seat = plan.guest_seat;
        state.finish_host_commit(plan);
        end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);

        if let Some((index, anchor)) = pending_cursor_position {
            // text-input-v3 has no cursor-position event. The commit still
            // must be delivered; Chromium will place the cursor at the end of
            // the committed string and then report its resulting surrounding
            // state in the next commit.
            log::debug!(
                "Cannot represent v1 cursor_position({}, {}) in text-input-v3",
                index,
                anchor
            );
        }

        ctx.host_to_client_queue.extend(transaction);
        Action::Drop
    }

    fn on_keysym(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        time: u32,
        sym: u32,
        state: u32,
        _modifiers: u32,
    ) -> Action {
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
                crate::handler::keyboard::WL_KEY_PRESSED
                    | crate::handler::keyboard::WL_KEY_REPEATED
                    | crate::handler::keyboard::WL_KEY_RELEASED
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
                    crate::handler::keyboard::WL_KEY_PRESSED => GuestKeyEvent::TextInputPress {
                        serial: wayland_serial,
                    },
                    crate::handler::keyboard::WL_KEY_REPEATED => GuestKeyEvent::TextInputRepeat,
                    crate::handler::keyboard::WL_KEY_RELEASED => GuestKeyEvent::TextInputRelease {
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
                if ((state == crate::handler::keyboard::WL_KEY_PRESSED
                    || state == crate::handler::keyboard::WL_KEY_REPEATED)
                    && keycode != crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
                    || (decision.ends_repeat
                        && keycode == crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
                {
                    if let Some(guest_seat) = guest_seat {
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
                    }
                }
                log::debug!(
                    "  -> forwarding wl_keyboard.key: keyboard_id={}, serial={}, time={}, keycode={}, state={}",
                    keyboard_id, wayland_serial, timestamp_millis, keycode, state
                );
                // Send wl_keyboard::key (opcode 3).
                let mut builder = MessageBuilder::new();
                builder.write_u32(wayland_serial); // serial
                builder.write_u32(timestamp_millis); // time
                builder.write_u32(keycode); // key
                builder.write_u32(state); // state (0: released, 1: pressed)
                push_msg(&mut ctx.host_to_client_queue, keyboard_id, 3, builder);
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

    fn on_enter(&mut self, ctx: &mut Context, surface: u32) -> Action {
        let host_v1_id = ctx.last_sender_id;
        log::info!(
            "[ime] host text_input.enter: host_v1_id={}, surface={}",
            host_v1_id,
            surface
        );

        // A placement refresh deliberately waits for this host event before
        // replaying the guest editor state.  `wl_display.sync.done` orders
        // requests on the host stream, but it does not prove that Exo has
        // installed the new input-method generation; `on_enter` does.
        let matching_guest = ctx.text_inputs.iter().find_map(|(&guest_id, state)| {
            if state.host_v1_id != host_v1_id {
                return None;
            }
            let host_surface = state
                .active_surface
                .and_then(|guest_surface| ctx.shadow_table.get_host_id(guest_surface));
            (host_surface == Some(surface)).then_some((guest_id, state.host_activation()))
        });
        log::info!(
            "[ime] host text_input.enter resolution: host_v1_id={} surface={} matching={:?}",
            host_v1_id,
            surface,
            matching_guest
        );

        if let Some((guest_id, activation)) = matching_guest {
            if activation == HostActivationState::Active {
                if ctx
                    .text_input_replay_barriers
                    .complete_on_host_enter(guest_id, host_v1_id)
                {
                    log::debug!(
                        "Host text-input enter is ready; replaying editor state for guest {}",
                        guest_id
                    );
                    queue_host_editor_state_replay(ctx, guest_id);
                }
            } else if ctx
                .text_input_activation_barriers
                .expects_replay(guest_id, host_v1_id)
            {
                // Keep the signal until the old generation's sync barrier
                // completes and activation is queued.
                ctx.text_input_replay_barriers
                    .record_early_host_enter(guest_id, host_v1_id);
            }
        }
        Action::Drop
    }

    fn on_leave(&mut self, ctx: &mut Context) -> Action {
        let host_v1_id = ctx.last_sender_id;
        log::info!("[ime] host text_input.leave: host_v1_id={}", host_v1_id);
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_v1_id) else {
            return Action::Drop;
        };

        // Exo can deliver a delayed leave for the host generation that was
        // invalidated by a transient Aura identity change.  The guest still
        // owns the same focused surface and has not sent a disable/leave, so
        // dropping the event silently would leave the host IME inactive
        // forever.  Re-enter the normal reset/deactivate/sync path instead of
        // fabricating a guest text-input focus transition.
        let Some((guest_seat, active_surface, committed_enabled, activation)) =
            ctx.text_inputs.get(&guest_id).map(|state| {
                (
                    state.guest_seat,
                    state.active_surface,
                    state.committed_enabled,
                    state.host_activation(),
                )
            })
        else {
            return Action::Drop;
        };
        let still_focused = active_surface.is_some_and(|surface| {
            ctx.keyboard_focus.surface_for_seat(guest_seat) == Some(surface)
        });
        if activation == HostActivationState::Active && committed_enabled && still_focused {
            let Some(host_seat) = ctx.shadow_table.get_host_id(guest_seat) else {
                log::debug!(
                    "[ime] cannot recover host leave for guest text input {}: \
                     guest seat {} has no host mapping",
                    guest_id,
                    guest_seat
                );
                return Action::Drop;
            };
            log::info!(
                "[ime] recovering focused host text-input generation after leave: \
                 guest_text_input={} host_v1_id={} guest_surface={:?}",
                guest_id,
                host_v1_id,
                active_surface
            );
            queue_host_deactivation_barrier(ctx, guest_id, host_v1_id, host_seat, true, true);
        }
        Action::Drop
    }

    fn on_modifiers_map(&mut self, _ctx: &mut Context, _map: &[u8]) -> Action {
        log::trace!(">>> on_modifiers_map: len={}", _map.len());
        Action::Drop
    }

    fn on_input_panel_state(&mut self, _ctx: &mut Context, _state: u32) -> Action {
        log::trace!(">>> on_input_panel_state: state={}", _state);
        Action::Drop
    }

    fn on_preedit_styling(
        &mut self,
        ctx: &mut Context,
        index: u32,
        length: u32,
        style: u32,
    ) -> Action {
        log::trace!(
            ">>> on_preedit_styling: index={}, length={}, style={}",
            index,
            length,
            style
        );
        // v3 has no general preedit styling protocol. Its cursor range can
        // represent v1's selection styling, which is the part needed by IMEs
        // for highlighted candidate ranges.
        if style == 6 {
            let host_id = ctx.last_sender_id;
            if let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) {
                if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                    state.record_preedit_selection(index, length);
                }
            }
        }
        Action::Drop
    }

    fn on_preedit_cursor(&mut self, ctx: &mut Context, index: i32) -> Action {
        log::trace!(">>> on_preedit_cursor: index={}", index);
        let host_id = ctx.last_sender_id;
        if let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) {
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                state.record_preedit_cursor(index);
            }
        }
        Action::Drop
    }

    fn on_cursor_position(&mut self, ctx: &mut Context, index: i32, anchor: i32) -> Action {
        log::trace!(">>> on_cursor_position: index={}, anchor={}", index, anchor);
        let host_id = ctx.last_sender_id;
        if let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) {
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                state.record_cursor_position(index, anchor);
            }
        }
        Action::Drop
    }

    fn on_delete_surrounding_text(&mut self, ctx: &mut Context, index: i32, length: u32) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) else {
            return Action::Drop;
        };
        log::trace!(
            ">>> on_delete_surrounding_text: host_id={}, guest_id={:?}, index={}, length={}",
            host_id,
            guest_id,
            index,
            length
        );
        if let Some(delete) = convert_delete_range(index, length) {
            let state = ctx
                .text_inputs
                .get_mut(&guest_id)
                .expect("active text input disappeared while handling host event");
            // Per text-input-v1 this event is part of the following
            // commit_string, not a standalone edit.
            if !state.record_delete(delete.0, delete.1) {
                log::warn!("Ignoring text-input delete beyond the pending transaction limit");
            }
        } else {
            log::warn!(
                "Ignoring unrepresentable v1 delete range: index={}, length={}",
                index,
                length
            );
        }
        Action::Drop
    }

    fn on_language(&mut self, ctx: &mut Context, serial: u32, _language: &String) -> Action {
        let host_id = ctx.last_sender_id;
        let guest_id = ctx.shadow_table.get_guest_id(host_id);
        log::trace!(
            ">>> on_language: host_id={}, guest_id={:?}, serial={}, language={:?}",
            host_id,
            guest_id,
            serial,
            _language
        );
        Action::Drop
    }

    fn on_text_direction(&mut self, ctx: &mut Context, serial: u32, _direction: u32) -> Action {
        let host_id = ctx.last_sender_id;
        let guest_id = ctx.shadow_table.get_guest_id(host_id);
        log::trace!(
            ">>> on_text_direction: host_id={}, guest_id={:?}, serial={}, direction={}",
            host_id,
            guest_id,
            serial,
            _direction
        );
        Action::Drop
    }
}

pub struct TextInputExtensionV1Handler;
impl zcr_text_input_extension_v1::ZcrTextInputExtensionV1Handler for TextInputExtensionV1Handler {}

pub struct ExtendedTextInputV1Handler;
impl zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler for ExtendedTextInputV1Handler {
    // Translates zcr_extended_text_input_v1::set_preedit_region (cursor-relative index/length)
    // into zwp_text_input_v1::delete_surrounding_text + preedit_string + done.
    fn on_set_preedit_region(&mut self, ctx: &mut Context, index: i32, length: u32) -> Action {
        let host_ext_id = ctx.last_sender_id;
        log::trace!(
            ">>> on_set_preedit_region: host_ext_id={}, index={}, length={}",
            host_ext_id,
            index,
            length
        );

        let Some((&guest_id, state)) = ctx
            .text_inputs
            .iter()
            .find(|(_, s)| s.host_ext_id == Some(host_ext_id))
        else {
            return Action::Drop;
        };
        let Some(plan) = state.prepare_preedit_region() else {
            log::warn!(
                "on_set_preedit_region: inactive input or no surrounding text for host_ext_id={}",
                host_ext_id
            );
            return Action::Drop;
        };

        let text = &plan.surrounding_text;
        let cursor_i64 = plan.surrounding_cursor as i64;
        let index_i64 = index as i64;
        let start_idx = cursor_i64 + index_i64;
        let length_i64 = length as i64;

        if !(start_idx >= 0
            && start_idx + length_i64 <= text.len() as i64
            && text.is_char_boundary(start_idx as usize)
            && text.is_char_boundary((start_idx + length_i64) as usize)
            && start_idx <= cursor_i64
            && start_idx + length_i64 >= cursor_i64)
        {
            log::warn!(
                "on_set_preedit_region: range [{}, {}] is invalid, non-adjacent to cursor {}, or out of bounds for text length {}",
                start_idx,
                start_idx + length_i64,
                cursor_i64,
                text.len()
            );
            return Action::Drop;
        }

        let preedit_text = text[start_idx as usize..(start_idx + length_i64) as usize].to_string();
        let before_length = if start_idx < cursor_i64 {
            (cursor_i64 - start_idx) as u32
        } else {
            0
        };
        let after_length = if start_idx + length_i64 > cursor_i64 {
            (start_idx + length_i64 - cursor_i64) as u32
        } else {
            0
        };
        let default_cursor = (cursor_i64 - start_idx) as i32;
        let pending_cursor = plan.cursor.or(Some(default_cursor));
        let (cursor_begin, cursor_end) =
            resolve_preedit_cursor(&preedit_text, plan.selection, pending_cursor);

        let mut transaction = Vec::new();
        let mut builder = MessageBuilder::new();
        builder.write_u32(before_length);
        builder.write_u32(after_length);
        if !push_msg(&mut transaction, guest_id, 4, builder) {
            return Action::Drop;
        }

        let mut builder = MessageBuilder::new();
        builder.write_string(&preedit_text);
        builder.write_i32(cursor_begin);
        builder.write_i32(cursor_end);
        if !push_msg(&mut transaction, guest_id, 2, builder)
            || !push_done_to(&mut transaction, guest_id, plan.done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while installing preedit region");
        let guest_seat = plan.guest_seat;
        state.finish_preedit_region(plan, preedit_text);
        end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        ctx.host_to_client_queue.extend(transaction);

        Action::Drop
    }
    fn on_clear_grammar_fragments(&mut self, _ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_clear_grammar_fragments: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_add_grammar_fragment(
        &mut self,
        _ctx: &mut Context,
        _start: u32,
        _end: u32,
        _suggestion: &String,
    ) -> Action {
        log::trace!(
            ">>> on_add_grammar_fragment: start={}, end={}, suggestion={:?}",
            _start,
            _end,
            _suggestion
        );
        Action::Drop
    }
    fn on_set_autocorrect_range(&mut self, _ctx: &mut Context, _start: u32, _end: u32) -> Action {
        log::trace!(
            ">>> on_set_autocorrect_range: start={}, end={}",
            _start,
            _end
        );
        Action::Drop
    }
    fn on_set_virtual_keyboard_occluded_bounds(
        &mut self,
        _ctx: &mut Context,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Action {
        log::trace!(
            ">>> on_set_virtual_keyboard_occluded_bounds: x={}, y={}, w={}, h={}",
            _x,
            _y,
            _width,
            _height
        );
        Action::Drop
    }
    fn on_confirm_preedit(&mut self, ctx: &mut Context, _selection_behavior: u32) -> Action {
        let host_ext_id = ctx.last_sender_id;
        log::trace!(
            ">>> on_confirm_preedit: host_ext_id={}, selection_behavior={}",
            host_ext_id,
            _selection_behavior
        );
        let Some(guest_id) = ctx
            .text_inputs
            .iter()
            .find(|(_, s)| s.host_ext_id == Some(host_ext_id))
            .map(|(&guest_id, _)| guest_id)
        else {
            return Action::Drop;
        };
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            return Action::Drop;
        };
        let Some(plan) = state.prepare_confirm_preedit() else {
            return Action::Drop;
        };
        if plan.preedit_text.is_empty() {
            if let Some(held) = held_repeat_key_for_seat(ctx, plan.guest_seat) {
                let backspace_repeat = held.key == crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
                if synthesize_ime_consumed_key_pair(ctx, held) {
                    if backspace_repeat {
                        let armed = ctx.key_generations.arm_ime_repeat(
                            held.host_keyboard_id,
                            held.key,
                            guest_id,
                        );
                        debug_assert!(armed, "recovered held key disappeared within one event");
                    } else {
                        end_backspace_repeat_for_text_input(ctx, plan.guest_seat, guest_id);
                    }
                    log::debug!(
                        "Recovered empty IME confirmation as held key {} press/release",
                        held.key
                    );
                } else if backspace_repeat {
                    end_backspace_repeat_for_text_input(ctx, plan.guest_seat, guest_id);
                }
                // GTK waits for the matching text-input transaction to finish
                // before applying a key that passed through the active IME.
                // A v1 confirm with no preedit has no text mutation, so its v3
                // equivalent is an empty done.
                push_done(ctx, guest_id, plan.done_serial);
                return Action::Drop;
            }
            end_backspace_repeat_for_text_input(ctx, plan.guest_seat, guest_id);
            log::debug!(
                "Ignoring confirm_preedit without an active preedit for guest {}",
                guest_id
            );
            return Action::Drop;
        }
        log::debug!(
            "  -> committing cached preedit={:?}, guest_id={}",
            plan.preedit_text,
            guest_id
        );
        log::debug!(
            "  -> sending v3 preedit_string(\"\") + commit_string({:?}) + done({})",
            plan.preedit_text,
            plan.done_serial
        );
        let mut transaction = Vec::new();
        let mut builder = MessageBuilder::new();
        builder.write_string("");
        builder.write_i32(0); // cursor_begin
        builder.write_i32(0); // cursor_end
        if !push_msg(&mut transaction, guest_id, 2, builder) {
            return Action::Drop;
        }

        let mut builder = MessageBuilder::new();
        builder.write_string(&plan.preedit_text);
        if !push_msg(&mut transaction, guest_id, 3, builder)
            || !push_done_to(&mut transaction, guest_id, plan.done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while confirming preedit");
        let guest_seat = plan.guest_seat;
        state.finish_confirm_preedit(plan);
        end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        ctx.host_to_client_queue.extend(transaction);
        Action::Drop
    }
}

pub struct TextInputManagerV3Handler;
impl zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler for TextInputManagerV3Handler {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // This manager is synthetic and has no host-side object. The
        // generated local-only destructor path queues its guest delete_id
        // after this handler returns.
        Action::Drop
    }

    fn on_get_text_input(&mut self, ctx: &mut Context, id: u32, seat: u32) -> Action {
        log::trace!(">>> v3 on_get_text_input: guest_id={}, seat={}", id, seat);
        let Some(host_manager_id) = ctx.host_text_input_manager_v1_id else {
            log::error!("Cannot create v3 text input without a host v1 manager");
            return Action::Drop;
        };
        let host_v1_id = ctx.shadow_table.allocate_host_id();
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_v1_id);
        push_msg(&mut ctx.client_to_host_queue, host_manager_id, 0, builder);

        let host_ext_id = ctx
            .host_text_input_extension_v1_id
            .map(|host_ext_manager_id| {
                let host_ext_id = ctx.shadow_table.allocate_host_id();
                let mut builder = MessageBuilder::new();
                builder.write_u32(host_ext_id);
                builder.write_u32(host_v1_id);
                push_msg(
                    &mut ctx.client_to_host_queue,
                    host_ext_manager_id,
                    0,
                    builder,
                );
                host_ext_id
            });

        ctx.shadow_table.map_id(id, host_v1_id);
        ctx.shadow_table
            .track_interface_with_version(id, "zwp_text_input_v3".to_string(), 1);
        ctx.shadow_table.track_host_interface_with_version(
            host_v1_id,
            "zwp_text_input_v1".to_string(),
            1,
        );
        if let Some(host_ext_id) = host_ext_id {
            let host_ext_manager_id = ctx.host_text_input_extension_v1_id;
            let host_ext_version = host_ext_manager_id
                .and_then(|manager_id| ctx.shadow_table.host_object_version(manager_id))
                .unwrap_or(u32::MAX);
            ctx.shadow_table.track_host_interface_with_version(
                host_ext_id,
                "zcr_extended_text_input_v1".to_string(),
                host_ext_version,
            );
        }

        let active_surface = ctx.keyboard_focus.surface_for_seat(seat);

        ctx.text_inputs.insert(
            id,
            crate::state::TextInputState::new(host_v1_id, host_ext_id, seat, active_surface),
        );

        // A text-input object created after keyboard focus was established
        // still needs the current enter event; otherwise the client may wait
        // indefinitely for a focus transition before enabling IME.
        if let Some(surface) = active_surface {
            let mut builder = MessageBuilder::new();
            builder.write_u32(surface);
            push_msg(&mut ctx.host_to_client_queue, id, 0, builder);
        }

        Action::Drop
    }
}

fn queue_host_deactivation_barrier(
    ctx: &mut Context,
    guest_id: u32,
    host_v1_id: u32,
    host_seat: u32,
    replay_editor_state: bool,
    allow_inactive: bool,
) -> bool {
    let callback_id = HostId(ctx.shadow_table.allocate_host_id());
    log::info!(
        "[ime] queue host deactivation barrier: callback={} guest_text_input={} \
         host_v1_id={} host_seat={} replay_editor_state={} allow_inactive={}",
        callback_id.0,
        guest_id,
        host_v1_id,
        host_seat,
        replay_editor_state,
        allow_inactive
    );

    // A compositor-owned Aura identity transition can invalidate Exo's
    // composition generation without changing the guest text-input-v3
    // focus.  The v1 reset request is the protocol-defined way to tell the
    // host IME to discard that generation.  Only placement refreshes use it;
    // ordinary guest focus changes already have their own v3 transaction and
    // must preserve the stock deactivate ordering.
    let reset = if replay_editor_state {
        Some(MessageBuilder::new().build_message(host_v1_id, zwp_text_input_v1::REQ_RESET))
    } else {
        None
    };

    let mut deactivate = MessageBuilder::new();
    deactivate.write_u32(host_seat);
    let Ok(deactivate) = deactivate.try_build_message(host_v1_id, 1) else {
        log::error!(
            "Unable to encode text-input deactivation for guest {}",
            guest_id
        );
        ctx.fatal_protocol_error = true;
        return false;
    };

    let mut sync = MessageBuilder::new();
    sync.write_u32(callback_id.0);
    let Ok(sync) = sync.try_build_message(1, wl_display::REQ_SYNC) else {
        log::error!(
            "Unable to encode text-input activation barrier for guest {}",
            guest_id
        );
        ctx.fatal_protocol_error = true;
        return false;
    };

    ctx.shadow_table
        .track_host_interface_with_version(callback_id.0, "wl_callback".to_string(), 1);
    if !ctx.text_input_activation_barriers.install(
        callback_id,
        guest_id,
        host_v1_id,
        replay_editor_state,
    ) {
        log::error!("Text-input {} already has an activation barrier", guest_id);
        ctx.shadow_table.remove_host_interface(callback_id.0);
        ctx.fatal_protocol_error = true;
        return false;
    }

    let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
        ctx.text_input_activation_barriers.complete(callback_id);
        ctx.shadow_table.remove_host_interface(callback_id.0);
        return false;
    };
    let began_draining = if allow_inactive {
        state.begin_host_draining(callback_id) || state.begin_host_refresh(callback_id)
    } else {
        state.begin_host_draining(callback_id)
    };
    if !began_draining {
        ctx.text_input_activation_barriers.complete(callback_id);
        ctx.shadow_table.remove_host_interface(callback_id.0);
        log::error!(
            "Text-input {} could not enter the host drain/refresh state",
            guest_id
        );
        ctx.fatal_protocol_error = true;
        return false;
    }

    // The ordered host stream is the proof: placement refresh resets the old
    // composition first, then deactivates, then waits for sync.done.
    if let Some(reset) = reset {
        ctx.client_to_host_queue.push((reset, Vec::new()));
    }
    // Draining rejects stale host events until callback.done.
    ctx.client_to_host_queue.push((deactivate, Vec::new()));
    ctx.client_to_host_queue.push((sync, Vec::new()));
    true
}

/// Prepare the IME transition that must precede a transient ARC identity
/// change.
///
/// Exo can invalidate the host v1 generation as soon as
/// `zaura_surface.set_application_id` changes.  If the old v1 generation is
/// still active at that point, a later deactivate/activate repair is not
/// reliable on custom hosts.  Transient placement therefore drains every
/// committed text-input object for the target surface before it queues the
/// ARC identity request.  The caller appends the returned messages before any
/// placement wire.
///
/// We intentionally do not synthesize a guest `text_input_v3.leave` here.
/// Winit treats that event as a real focus loss and clears its `ime_allowed`
/// flag before the matching synthetic `enter` can arrive.  The subsequent
/// enter is then ignored by Winit and Korean composition remains disabled.
/// Keeping the guest focus generation intact and sending only a post-cleanup
/// `enter` lets Winit re-enable the already focused text field.
///
/// The local activation marker is changed only after every request is encoded,
/// so an encoding failure cannot strand the state as inactive without the
/// corresponding host request.
pub(crate) struct PlacementImePreflight {
    pub(crate) host_messages: Vec<(Vec<u8>, Vec<RawFd>)>,
    pub(crate) guest_text_inputs: Vec<u32>,
    pub(crate) guest_surface_id: u32,
}

pub(crate) fn prepare_placement_ime_deactivation(
    ctx: &mut Context,
    guest_surface_id: u32,
) -> Option<PlacementImePreflight> {
    let candidates = ctx
        .text_inputs
        .iter()
        .filter_map(|(&guest_id, state)| {
            // The Aura identity transition can invalidate Exo's host IME
            // generation even while the guest editor is between v3
            // enable/disable commits.  Keep a focused object in the
            // placement-generation ledger regardless of its current
            // committed enabled bit; a later enable must then perform an
            // explicit host reset instead of relying on a stale activation.
            (state.active_surface == Some(guest_surface_id)
                && !state.placement_ime_pending_for(guest_surface_id))
            .then_some((
                guest_id,
                state.host_v1_id,
                state.guest_seat,
                state.host_is_active(),
                state.committed_enabled,
            ))
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        log::info!(
            "[ime] placement preflight: surface={} no committed active text-input generation",
            guest_surface_id
        );
        return Some(PlacementImePreflight {
            host_messages: Vec::new(),
            guest_text_inputs: Vec::new(),
            guest_surface_id,
        });
    }

    let mut host_messages = Vec::with_capacity(candidates.len() * 2);
    let mut guest_text_inputs = Vec::with_capacity(candidates.len());
    for (guest_id, host_v1_id, guest_seat, host_active, committed_enabled) in candidates {
        log::info!(
            "[ime] placement preflight candidate: guest_text_input={} host_v1_id={} \
             guest_seat={} host_active={} committed_enabled={} surface={}",
            guest_id,
            host_v1_id,
            guest_seat,
            host_active,
            committed_enabled,
            guest_surface_id
        );
        if host_active {
            let Some(host_seat) = ctx.shadow_table.get_host_id(guest_seat) else {
                log::warn!(
                    "Cannot drain text input {} before transient placement: guest seat {} \
                     has no host mapping",
                    guest_id,
                    guest_seat
                );
                return None;
            };

            let Ok(reset) =
                MessageBuilder::new().try_build_message(host_v1_id, zwp_text_input_v1::REQ_RESET)
            else {
                log::warn!(
                    "Unable to encode text-input reset for guest {} before transient placement",
                    guest_id
                );
                return None;
            };
            let mut deactivate_builder = MessageBuilder::new();
            deactivate_builder.write_u32(host_seat);
            let Ok(deactivate) = deactivate_builder.try_build_message(host_v1_id, 1) else {
                log::warn!(
                    "Unable to encode text-input deactivation for guest {} before transient placement",
                    guest_id
                );
                return None;
            };

            host_messages.push((reset, Vec::new()));
            host_messages.push((deactivate, Vec::new()));
        }
        guest_text_inputs.push(guest_id);
    }

    // No state is mutated until the placement adapter has serialized its
    // complete identity/bounds/barrier batch. This keeps a late encoding
    // failure from marking the guest generation for re-entry without the
    // host-side transition.
    for &guest_id in &guest_text_inputs {
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            log::warn!(
                "Text input {} disappeared while preparing transient placement",
                guest_id
            );
            return None;
        };
        if state.active_surface != Some(guest_surface_id)
            || state.placement_ime_pending_for(guest_surface_id)
        {
            log::warn!(
                "Text input {} changed while preparing transient placement",
                guest_id
            );
            return None;
        }
    }

    log::debug!(
        "[ime] draining {} committed text-input generation(s) before transient ARC placement on surface {}",
        guest_text_inputs.len(),
        guest_surface_id
    );
    Some(PlacementImePreflight {
        host_messages,
        guest_text_inputs,
        guest_surface_id,
    })
}

/// Prepare the local half of a transient IME generation refresh after the
/// placement wire batch has been validated.
///
/// The guest remains entered on its surface. The committed editor projection
/// is retained so the host-side generation can be reactivated and replayed
/// after Aura identity restoration without fabricating a guest focus event.
pub(crate) fn commit_placement_ime_preflight(
    ctx: &mut Context,
    preflight: PlacementImePreflight,
) -> bool {
    for &guest_id in &preflight.guest_text_inputs {
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            log::warn!(
                "Text input {} disappeared while committing transient placement",
                guest_id
            );
            return false;
        };
        if state.active_surface != Some(preflight.guest_surface_id)
            || state.placement_ime_pending_for(preflight.guest_surface_id)
        {
            log::warn!(
                "Text input {} changed before transient placement was published",
                guest_id
            );
            return false;
        }
    }

    for guest_id in preflight.guest_text_inputs {
        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
            return false;
        };
        // Keep the current surface selected while the identity transition is
        // in flight. The committed projection is retained for host replay
        // after the identity barrier.
        if !state.begin_placement_ime(preflight.guest_surface_id) {
            log::warn!(
                "Text input {} could not begin transient placement refresh",
                guest_id
            );
            return false;
        }
        log::info!(
            "[ime] placement preflight committed: guest_text_input={} host_v1_id={} \
             host activation -> inactive, surface={}",
            guest_id,
            state.host_v1_id,
            preflight.guest_surface_id
        );
        let _ = state.deactivate_host_without_barrier();
        ctx.text_input_replay_barriers
            .cancel_host_enter_replay(guest_id, state.host_v1_id);
    }
    true
}

/// Finish the local half of a transient placement marker.
///
/// The production transient path refreshes the host generation directly and
/// does not synthesize a guest focus event. This helper remains available for
/// callers that explicitly need to emit a guest enter.
#[cfg(test)]
pub(crate) fn resume_placement_ime_for_surface(ctx: &mut Context, guest_surface_id: u32) -> bool {
    let guest_text_inputs = ctx
        .text_inputs
        .iter()
        .filter_map(|(&guest_id, state)| {
            state
                .placement_ime_pending_for(guest_surface_id)
                .then_some(guest_id)
        })
        .collect::<Vec<_>>();

    for guest_id in guest_text_inputs {
        let mut builder = MessageBuilder::new();
        builder.write_u32(guest_surface_id);
        if !push_msg(&mut ctx.host_to_client_queue, guest_id, 0, builder) {
            log::warn!(
                "Unable to encode guest text-input enter for {} after transient placement",
                guest_id
            );
            continue;
        }
        log::debug!(
            "Queued synthetic text-input-v3 enter for guest {} surface {} after transient placement",
            guest_id,
            guest_surface_id
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            let completed = state.complete_placement_ime(guest_surface_id);
            debug_assert!(completed);
        }
    }
    true
}

/// Complete one host activation barrier and reconcile the newest guest state.
///
/// Returns `true` when `callback_id` belongs to this subsystem.
pub(crate) fn complete_host_activation_barrier(ctx: &mut Context, callback_id: HostId) -> bool {
    let Some((guest_id, host_v1_id, replay_editor_state)) =
        ctx.text_input_activation_barriers.complete(callback_id)
    else {
        return false;
    };
    log::info!(
        "[ime] host text-input activation sync.done: callback={} guest_text_input={} \
         host_v1_id={} replay_editor_state={}",
        callback_id.0,
        guest_id,
        host_v1_id,
        replay_editor_state
    );
    if !ctx.shadow_table.mark_pending_destroy_host(callback_id.0) {
        log::warn!(
            "Text-input activation callback {} was not tracked as host-only",
            callback_id.0
        );
    }
    let completed = ctx.text_inputs.get_mut(&guest_id).is_some_and(|state| {
        state.host_v1_id == host_v1_id && state.complete_host_draining(callback_id)
    });
    if completed {
        if replay_editor_state {
            // Arm before queueing activate.  The host `on_enter` event may
            // arrive before or after this callback is observed by the proxy;
            // the registry handles both orders.
            let replay_immediately = ctx
                .text_input_replay_barriers
                .arm_for_host_enter(guest_id, host_v1_id);
            update_host_activation(ctx, guest_id);
            if replay_immediately
                && ctx
                    .text_inputs
                    .get(&guest_id)
                    .is_some_and(|state| state.host_is_active())
            {
                queue_host_editor_state_replay(ctx, guest_id);
            }
        } else {
            update_host_activation(ctx, guest_id);
        }
    }
    true
}

/// Queue the latest committed editor state after an internal host-generation
/// reset.
///
/// Host text-input-v1 drops its surrounding text, content type, cursor
/// rectangle, and commit generation when it is deactivated. A placement
/// transition is invisible to the guest text-input-v3 client, so the client
/// will not send another commit for us. Replaying the committed state after
/// the deferred `activate` request preserves the same ordering used by a
/// normal initial activation and keeps composition-capable IMEs usable.
fn queue_host_editor_state_replay(ctx: &mut Context, guest_id: u32) -> bool {
    let Some((
        host_v1_id,
        host_ext_id,
        host_ext_version,
        committed_surrounding_text,
        committed_content_type,
        cursor_rect,
        guest_commit_serial,
        committed_enabled,
        active_surface,
    )) = ctx.text_inputs.get(&guest_id).map(|state| {
        (
            state.host_v1_id,
            state.host_ext_id,
            state
                .host_ext_id
                .and_then(|id| ctx.shadow_table.host_object_version(id))
                .unwrap_or(u32::MAX),
            state.committed_surrounding_text.clone(),
            state.committed_content_type,
            state.cursor_rect,
            state.guest_commit_serial,
            state.committed_enabled,
            state.active_surface,
        )
    })
    else {
        return false;
    };

    if !committed_enabled || active_surface.is_none() {
        return false;
    }

    let mut transaction = Vec::with_capacity(6);

    // A host generation reset clears surrounding text even when the guest
    // currently has no surrounding-text value. Explicitly send the empty
    // value so Exo cannot retain editor state from the previous generation.
    let mut surrounding = MessageBuilder::new();
    if let Some((text, cursor, anchor)) = committed_surrounding_text.as_ref() {
        surrounding.write_string(text);
        surrounding.write_u32(*cursor as u32);
        surrounding.write_u32(*anchor as u32);
    } else {
        surrounding.write_string("");
        surrounding.write_u32(0);
        surrounding.write_u32(0);
    }
    if !push_msg(&mut transaction, host_v1_id, 5, surrounding) {
        return false;
    }

    if let Some((hint, purpose)) = committed_content_type {
        let (v1_hint, v1_purpose, input_type, input_mode, input_flags, learning_mode) =
            map_v3_content_type(hint, purpose);

        if let Some(host_ext_id) = host_ext_id {
            if extension_version_allows(host_ext_version, 9) {
                let mut builder = MessageBuilder::new();
                builder.write_u32(u32::from(committed_surrounding_text.is_some()));
                if !push_msg(&mut transaction, host_ext_id, 7, builder) {
                    return false;
                }
            }
        }

        let mut builder = MessageBuilder::new();
        builder.write_u32(v1_hint);
        builder.write_u32(v1_purpose);
        if !push_msg(&mut transaction, host_v1_id, 6, builder) {
            return false;
        }

        if let Some(host_ext_id) = host_ext_id {
            let mut builder = MessageBuilder::new();
            builder.write_u32(input_type);
            builder.write_u32(input_mode);
            builder.write_u32(input_flags);
            builder.write_u32(learning_mode);
            if extension_version_allows(host_ext_version, 8) {
                builder.write_u32(1);
                if !push_msg(&mut transaction, host_ext_id, 6, builder) {
                    return false;
                }
            } else if extension_version_allows(host_ext_version, 2)
                && !push_msg(&mut transaction, host_ext_id, 1, builder)
            {
                return false;
            }
        }
    }

    let (cursor_x, cursor_y, cursor_width, cursor_height) = cursor_rect.unwrap_or((0, 0, 0, 0));
    let mut cursor = MessageBuilder::new();
    cursor.write_i32(cursor_x);
    cursor.write_i32(cursor_y);
    cursor.write_i32(cursor_width);
    cursor.write_i32(cursor_height);
    if !push_msg(&mut transaction, host_v1_id, 7, cursor) {
        return false;
    }

    let host_commit_serial =
        ctx.next_host_text_input_commit_serial(host_v1_id, guest_commit_serial);
    let mut commit = MessageBuilder::new();
    commit.write_u32(host_commit_serial);
    if !push_msg(&mut transaction, host_v1_id, 9, commit) {
        return false;
    }

    log::debug!(
        "Replaying committed editor state for text input {} with host commit serial {} \
         (guest serial {})",
        guest_id,
        host_commit_serial,
        guest_commit_serial,
    );
    ctx.client_to_host_queue.extend(transaction);
    true
}

/// Reconcile the host-side activation state of a reused text-input-v1 object.
///
/// Deactivation opens an internal sync barrier. Reactivation is deferred until
/// callback.done proves that events from the previous focus generation have
/// already been dispatched and dropped.
pub(crate) fn update_host_activation(ctx: &mut Context, guest_id: u32) {
    let Some(state) = ctx.text_inputs.get(&guest_id) else {
        return;
    };
    let host_seat = ctx.shadow_table.get_host_id(state.guest_seat);
    let host_surface = state
        .active_surface
        .and_then(|surface| ctx.shadow_table.get_host_id(surface));
    let target_activated = state.committed_enabled && host_surface.is_some();
    let placement_refresh_pending = state
        .active_surface
        .is_some_and(|surface| state.placement_ime_pending_for(surface));

    match (target_activated, state.host_activation()) {
        (true, HostActivationState::Inactive) | (false, HostActivationState::Active) => {}
        (true, HostActivationState::Draining { .. }) => {
            log::debug!(
                "Deferring text input {} activation until the previous generation drains",
                guest_id
            );
            return;
        }
        (false, HostActivationState::Draining { .. })
        | (true, HostActivationState::Active)
        | (false, HostActivationState::Inactive) => return,
    }

    // A guest wl_seat can be pending destruction while child keyboards and
    // text-input objects are still completing their own lifetimes. The
    // numeric host ID remains reserved for delete_id bookkeeping, but the
    // host proxy is no longer a valid object for a v1 deactivate request.
    // Reconcile local state without queueing traffic against that dead seat.
    if ctx.shadow_table.is_pending_destroy_guest(state.guest_seat) {
        log::debug!(
            "Cannot update text input {} activation: guest seat {} is pending destruction",
            guest_id,
            state.guest_seat
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.deactivate_host_without_barrier();
        }
        return;
    }

    // A host v1 activate/deactivate request must carry a real wl_seat object.
    // Queueing it with ID 0 turns a transient mapping race into a compositor
    // protocol error. Activation remains pending until focus/mapping is valid;
    // deactivation can safely clear the local marker when the seat has already
    // disappeared.
    if host_seat.is_none() {
        log::warn!(
            "Cannot update text input {} activation: guest seat {} has no host mapping",
            guest_id,
            state.guest_seat
        );
        if !target_activated {
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                state.deactivate_host_without_barrier();
            }
        }
        return;
    }

    let host_seat = host_seat.expect("checked above");
    let host_v1_id = state.host_v1_id;
    let host_surface = host_surface.unwrap_or(0);

    if target_activated {
        if placement_refresh_pending && state.host_activation() == HostActivationState::Inactive {
            log::info!(
                "[ime] deferring activation for placement refresh: guest_text_input={} \
                 host_v1_id={} host_surface={}",
                guest_id,
                host_v1_id,
                host_surface
            );
            if queue_host_deactivation_barrier(ctx, guest_id, host_v1_id, host_seat, true, true) {
                let completed = ctx.text_inputs.get_mut(&guest_id).is_some_and(|state| {
                    state
                        .active_surface
                        .is_some_and(|surface| state.complete_placement_ime(surface))
                });
                debug_assert!(completed);
            }
            return;
        }
        log::info!(
                "update_host_activation: activating text input v1 (guest_id={}, host_v1_id={}, host_surface={})",
                guest_id,
                host_v1_id,
                host_surface,
            );
        // activate: opcode 0
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_seat);
        builder.write_u32(host_surface);
        if push_msg(&mut ctx.client_to_host_queue, host_v1_id, 0, builder) {
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                let activated = state.activate_host();
                debug_assert!(activated, "activation state was checked above");
            }
        }
    } else {
        log::info!(
            "update_host_activation: deactivating text input v1 (guest_id={}, host_v1_id={})",
            guest_id,
            host_v1_id
        );
        queue_host_deactivation_barrier(ctx, guest_id, host_v1_id, host_seat, false, false);
    }
}

/// Recycle host IME activation after a compositor-owned metadata transition.
///
/// Changing an Aura application's identity can make Exo discard the active
/// IME generation without sending a Wayland keyboard focus transition. The
/// proxy's focus state is therefore still `Active` even though the host no
/// longer accepts composition events. A deactivate/sync barrier followed by
/// the normal activation reconciler establishes a fresh host generation while
/// preserving the guest text-input object's focus and committed editor state.
///
/// This is deliberately keyed by the focused guest surface instead of by a
/// placement backend. It keeps the repair in the text-input state machine and
/// lets placement request it only after its identity/parent cleanup has been
/// ordered on the host stream.
pub(crate) fn refresh_host_activation_for_surface(
    ctx: &mut Context,
    guest_surface_id: u32,
) -> bool {
    let candidates = ctx
        .text_inputs
        .iter()
        .filter_map(|(&guest_id, state)| {
            let placement_pending = state.placement_ime_pending_for(guest_surface_id);
            if state.active_surface != Some(guest_surface_id)
                || (!state.committed_enabled && !placement_pending)
            {
                return None;
            }
            match state.host_activation() {
                HostActivationState::Active => Some((
                    guest_id,
                    state.host_v1_id,
                    state.guest_seat,
                    true,
                    state.committed_enabled,
                    placement_pending,
                )),
                // Transient placement drains the generation before changing
                // Aura identity.  There is no deactivation barrier to wait
                // for in this state; re-arm the replay and activate directly
                // after the identity cleanup barrier.
                HostActivationState::Inactive => Some((
                    guest_id,
                    state.host_v1_id,
                    state.guest_seat,
                    false,
                    state.committed_enabled,
                    placement_pending,
                )),
                HostActivationState::Draining { .. } => None,
            }
        })
        .collect::<Vec<_>>();

    let mut refreshed = false;
    log::info!(
        "[ime] refresh host activation after placement: surface={} candidates={}",
        guest_surface_id,
        candidates.len()
    );
    for (
        guest_id,
        host_v1_id,
        guest_seat,
        needs_deactivation,
        committed_enabled,
        placement_pending,
    ) in candidates
    {
        let Some(host_seat) = ctx.shadow_table.get_host_id(guest_seat) else {
            log::debug!(
                "Cannot refresh text input {} after placement: guest seat {} \
                 has no host mapping",
                guest_id,
                guest_seat
            );
            continue;
        };
        if needs_deactivation {
            log::info!(
                "[ime] refresh path requires second deactivation: guest_text_input={} \
                 host_v1_id={} guest_seat={}",
                guest_id,
                host_v1_id,
                guest_seat
            );
            if queue_host_deactivation_barrier(ctx, guest_id, host_v1_id, host_seat, true, false) {
                if placement_pending {
                    let completed = ctx
                        .text_inputs
                        .get_mut(&guest_id)
                        .is_some_and(|state| state.complete_placement_ime(guest_surface_id));
                    debug_assert!(completed);
                }
                refreshed = true;
            }
            continue;
        }

        if !committed_enabled {
            // The guest is focused but currently disabled.  Keep the
            // placement marker until its next v3 enable/commit; the normal
            // activation reconciler will then perform a reset/deactivate/sync
            // barrier before activating the host generation.
            log::info!(
                "[ime] refresh deferred until guest enable: guest_text_input={} \
                 host_v1_id={} surface={}",
                guest_id,
                host_v1_id,
                guest_surface_id
            );
            continue;
        }

        // The host generation was deliberately made inactive before the ARC
        // identity transition.  Wait for the post-cleanup host `enter` event
        // before replaying editor state, just like the barrier path below.
        let replay_immediately = ctx
            .text_input_replay_barriers
            .arm_for_host_enter(guest_id, host_v1_id);
        update_host_activation(ctx, guest_id);
        log::info!(
            "[ime] refresh path reactivated host text input: guest_text_input={} \
             host_v1_id={} replay_immediately={}",
            guest_id,
            host_v1_id,
            replay_immediately
        );
        if replay_immediately
            && ctx
                .text_inputs
                .get(&guest_id)
                .is_some_and(|state| state.host_is_active())
        {
            queue_host_editor_state_replay(ctx, guest_id);
        }
        if ctx
            .text_inputs
            .get(&guest_id)
            .is_some_and(|state| state.host_is_active())
        {
            if placement_pending {
                let completed = ctx
                    .text_inputs
                    .get_mut(&guest_id)
                    .is_some_and(|state| state.complete_placement_ime(guest_surface_id));
                debug_assert!(completed);
            }
            refreshed = true;
        }
    }
    refreshed
}

/// Project authoritative seat focus changes onto every text-input object.
///
/// Keeping seat-wide repeat teardown here and delegating per-object mutation
/// to the shared update path prevents handlers from implementing subtly
/// different focus boundaries.
pub(crate) fn apply_keyboard_focus_changes(ctx: &mut Context, changes: &[SeatFocusChange]) {
    for change in changes {
        if change.previous_surface == change.current_surface {
            continue;
        }

        ctx.key_generations
            .clear_peek_watermarks_for_seat(change.guest_seat);
        end_backspace_repeat_for_seat(ctx, change.guest_seat);
        let updates = ctx
            .text_inputs
            .iter()
            .filter_map(|(&guest_text_input_id, state)| {
                (state.guest_seat == change.guest_seat)
                    .then_some((guest_text_input_id, change.current_surface))
            })
            .collect::<Vec<_>>();
        apply_text_input_focus_updates(ctx, &updates);
    }
}

/// Repair text inputs that still reference a surface after its keyboard
/// generation was retired.
///
/// This is intentionally object-scoped. Treating one stale projection as a
/// new seat transition would invalidate healthy text inputs that already
/// track the registry's current surface.
pub(crate) fn repair_destroyed_surface_focus(ctx: &mut Context, destroyed_surface: u32) {
    let updates = ctx
        .text_inputs
        .iter()
        .filter_map(|(&guest_text_input_id, state)| {
            (state.active_surface == Some(destroyed_surface)).then_some((
                guest_text_input_id,
                ctx.keyboard_focus.surface_for_seat(state.guest_seat),
            ))
        })
        .collect::<Vec<_>>();
    apply_text_input_focus_updates(ctx, &updates);
}

/// Apply focused-surface targets to selected text-input objects atomically.
///
/// Both authoritative seat transitions and lifecycle recovery use this path,
/// so editor invalidation, guest events, and host-v1 activation cannot drift.
fn apply_text_input_focus_updates(ctx: &mut Context, updates: &[(u32, Option<u32>)]) {
    let mut events = Vec::new();
    let mut text_inputs_to_update = Vec::new();
    for &(guest_text_input_id, current_surface) in updates {
        let Some(state) = ctx.text_inputs.get_mut(&guest_text_input_id) else {
            continue;
        };

        let host_v1_id = state.host_v1_id;
        let previous_surface = state.apply_focus(current_surface);
        ctx.text_input_replay_barriers
            .cancel_host_enter_replay(guest_text_input_id, host_v1_id);

        // A stale local projection can already equal the authoritative target
        // even though the seat crossed a real focus boundary. Always
        // invalidate and reconcile host activation, but do not fabricate
        // duplicate guest leave/enter events in that case.
        if previous_surface != current_surface {
            if let Some(surface) = previous_surface {
                let mut builder = MessageBuilder::new();
                builder.write_u32(surface);
                events.push((builder.build_message(guest_text_input_id, 1), Vec::new()));
            }
            if let Some(surface) = current_surface {
                let mut builder = MessageBuilder::new();
                builder.write_u32(surface);
                events.push((builder.build_message(guest_text_input_id, 0), Vec::new()));
            }
        }
        text_inputs_to_update.push(guest_text_input_id);
    }

    ctx.host_to_client_queue.extend(events);
    for guest_text_input_id in text_inputs_to_update {
        update_host_activation(ctx, guest_text_input_id);
    }
}

/// Text-input-v3 requests are ignored while the object is not entered on a
/// surface.  In particular, a leave event must form a hard boundary: queued
/// editor state from the old surface must not be sent to the host IME until a
/// subsequent enter establishes a new focus.
fn v3_is_focused(ctx: &Context, guest_id: u32) -> bool {
    ctx.text_inputs
        .get(&guest_id)
        .is_some_and(|state| state.active_surface.is_some())
}

pub(crate) fn end_backspace_repeat_for_seat(ctx: &mut Context, guest_seat: u32) {
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter_map(|(&guest_keyboard_id, &seat)| {
            (seat == guest_seat)
                .then(|| ctx.shadow_table.host_id_of(GuestId(guest_keyboard_id)))
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

fn end_backspace_repeat_for_text_input(ctx: &mut Context, guest_seat: u32, guest_text_input: u32) {
    let host_keyboards: Vec<_> = ctx
        .keyboard_to_seat
        .iter()
        .filter_map(|(&guest_keyboard_id, &seat)| {
            (seat == guest_seat)
                .then(|| ctx.shadow_table.host_id_of(GuestId(guest_keyboard_id)))
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

pub struct TextInputV3Handler;
impl zwp_text_input_v3::ZwpTextInputV3Handler for TextInputV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        // Destruction is the final disabled transition. Route it through the
        // same activation reconciler as commit/focus changes so parent-seat
        // lifetime validation cannot drift between teardown paths.
        let (guest_seat, host_v1_id) = ctx
            .text_inputs
            .get(&guest_id)
            .map(|state| (Some(state.guest_seat), state.host_v1_id))
            .unwrap_or((None, 0));
        if host_v1_id != 0 {
            ctx.text_input_replay_barriers
                .cancel_host_enter_replay(guest_id, host_v1_id);
        }
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.begin_destroy();
        }
        if let Some(guest_seat) = guest_seat {
            end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        }
        update_host_activation(ctx, guest_id);

        let Some(state) = ctx.text_inputs.remove(&guest_id) else {
            // Keep the guest lifecycle well-formed even if local text-input
            // state was already cleaned by a focus/seat teardown path.
            if crate::handler::display::queue_guest_delete_id(ctx, guest_id) {
                ctx.shadow_table.remove_guest_mapping(guest_id);
            }
            return Action::Drop;
        };

        // zwp_text_input_v1 has no destructor, but its ChromeOS extension does.
        // Stop extension events and remove all local routing state.
        if let Some(host_ext_id) = state.host_ext_id {
            push_msg(
                &mut ctx.client_to_host_queue,
                host_ext_id,
                0,
                MessageBuilder::new(),
            );
            ctx.shadow_table.mark_pending_destroy_host(host_ext_id);
        }
        // The v3 object is guest-facing but backed by a v1 host object whose
        // protocol has no destructor. Complete the guest-only lifecycle
        // locally before removing the paired mapping.
        if !crate::handler::display::queue_guest_delete_id(ctx, guest_id) {
            log::debug!(
                "Text input {} had already been retired before its destroy handler ran",
                guest_id
            );
            return Action::Drop;
        }
        // The host v1 object has no destructor request. Keep its host-side
        // interface reservation until connection teardown so a new text input
        // cannot reuse the ID while stale host events are still in flight.
        ctx.shadow_table.remove_guest_mapping(guest_id);
        Action::Drop
    }

    fn on_enable(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::info!(">>> v3 on_enable: guest_id={}", guest_id);
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.begin_enabled_transaction(true);
        }
        Action::Drop
    }

    fn on_disable(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::info!(">>> v3 on_disable: guest_id={}", guest_id);
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.begin_enabled_transaction(false);
        }
        Action::Drop
    }

    fn on_set_surrounding_text(
        &mut self,
        ctx: &mut Context,
        text: &String,
        cursor: i32,
        anchor: i32,
    ) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::trace!(
            ">>> v3 on_set_surrounding_text: guest_id={}, text={:?}, cursor={}, anchor={}",
            guest_id,
            text,
            cursor,
            anchor
        );
        let cursor_valid = cursor >= 0
            && anchor >= 0
            && (cursor as usize) <= text.len()
            && (anchor as usize) <= text.len()
            && text.is_char_boundary(cursor as usize)
            && text.is_char_boundary(anchor as usize);
        if !cursor_valid {
            log::warn!(
                "Ignoring invalid surrounding text offsets: len={}, cursor={}, anchor={}",
                text.len(),
                cursor,
                anchor
            );
            return Action::Drop;
        }
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.set_surrounding_text(text.clone(), cursor, anchor);
        }
        Action::Drop
    }

    fn on_set_text_change_cause(&mut self, ctx: &mut Context, cause: u32) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::trace!(
            ">>> v3 on_text_change_cause: guest_id={}, cause={}",
            guest_id,
            cause
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.set_text_change_cause(cause);
        }
        Action::Drop
    }

    fn on_set_content_type(&mut self, ctx: &mut Context, hint: u32, purpose: u32) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::trace!(
            ">>> v3 on_set_content_type: guest_id={}, hint={}, purpose={}",
            guest_id,
            hint,
            purpose
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.set_content_type(hint, purpose);
        }
        Action::Drop
    }

    fn on_set_cursor_rectangle(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::trace!(
            ">>> v3 on_set_cursor_rectangle: guest_id={}, rect=({}, {}, {}, {})",
            guest_id,
            x,
            y,
            width,
            height
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.set_cursor_rect((x, y, width, height));
        }
        Action::Drop
    }

    fn on_commit(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        let enable_conflict = ctx.text_inputs.get(&guest_id).is_some_and(|state| {
            state.enabled_dirty
                && state.pending_enabled
                && ctx.text_inputs.iter().any(|(&other_id, other)| {
                    other_id != guest_id
                        && other.guest_seat == state.guest_seat
                        && other.committed_enabled
                })
        });
        let Some(plan) = ctx
            .text_inputs
            .get(&guest_id)
            .map(|state| state.prepare_guest_commit(enable_conflict))
        else {
            return Action::Drop;
        };
        let placement_refresh_pending = ctx
            .text_inputs
            .get(&guest_id)
            .and_then(|state| state.active_surface)
            .is_some_and(|surface| ctx.text_inputs[&guest_id].placement_ime_pending_for(surface));
        let plan_enabled = plan.enabled;
        log::trace!(
            ">>> v3 on_commit: guest_id={}, serial={}, enabled={}, host_v1_id={}",
            guest_id,
            plan.serial,
            plan.enabled,
            plan.host_v1_id
        );

        // Construct the complete v1 transaction before publishing any part of
        // it or advancing the double-buffered state. This prevents an
        // unencodable variable-size field from leaving Exo and the bridge on
        // different committed editor states.
        let mut transaction = Vec::new();
        if let Some(surrounding_text) = &plan.surrounding_text {
            if let Some((text, cursor, anchor)) = surrounding_text {
                log::debug!(
                    "  -> sending v1 set_surrounding_text({:?}, cursor={}, anchor={})",
                    text,
                    cursor,
                    anchor
                );
                // set_surrounding_text: opcode 5
                let mut builder = MessageBuilder::new();
                builder.write_string(text);
                builder.write_u32(*cursor as u32);
                builder.write_u32(*anchor as u32);
                if !push_msg(&mut transaction, plan.host_v1_id, 5, builder) {
                    return Action::Drop;
                }
            } else {
                // v3 enable/disable double-buffers surrounding text. Clearing
                // the guest value must be mirrored to v1; otherwise Exo keeps
                // the previous surrounding string and can make a later IME
                // transaction operate on stale private/editor contents.
                log::debug!("  -> clearing v1 surrounding text for guest {}", guest_id);
                let mut builder = MessageBuilder::new();
                builder.write_string("");
                builder.write_u32(0);
                builder.write_u32(0);
                if !push_msg(&mut transaction, plan.host_v1_id, 5, builder) {
                    return Action::Drop;
                }
            }
        }

        if let Some((hint, purpose)) = plan.content_type {
            let (v1_hint, v1_purpose, input_type, input_mode, input_flags, learning_mode) =
                map_v3_content_type(hint, purpose);

            if let Some(host_ext_id) = plan.host_ext_id {
                let host_ext_version = ctx
                    .shadow_table
                    .host_object_version(host_ext_id)
                    .unwrap_or(u32::MAX);
                if extension_version_allows(host_ext_version, 9) {
                    // This request applies to the following content-type
                    // request, so keep both in the same transaction.
                    let mut builder = MessageBuilder::new();
                    builder.write_u32(u32::from(plan.has_surrounding_text));
                    if !push_msg(&mut transaction, host_ext_id, 7, builder) {
                        return Action::Drop;
                    }
                }
            }

            // set_content_type: opcode 6 (on zwp_text_input_v1)
            let mut builder = MessageBuilder::new();
            builder.write_u32(v1_hint);
            builder.write_u32(v1_purpose);
            if !push_msg(&mut transaction, plan.host_v1_id, 6, builder) {
                return Action::Drop;
            }

            if let Some(host_ext_id) = plan.host_ext_id {
                let mut builder = MessageBuilder::new();
                builder.write_u32(input_type);
                builder.write_u32(input_mode);
                builder.write_u32(input_flags);
                builder.write_u32(learning_mode);
                // zcr_extended_text_input_v1 v8 replaced the deprecated
                // four-argument request with a five-argument request that
                // carries inline-composition support. Older hosts must
                // receive opcode 1 instead; sending opcode 6 to v1-v7 is a
                // protocol error.
                let host_ext_version = ctx
                    .shadow_table
                    .host_object_version(host_ext_id)
                    .unwrap_or(u32::MAX);
                if extension_version_allows(host_ext_version, 8) {
                    // A text-input-v3 client supports inline preedit by
                    // definition.
                    builder.write_u32(1);
                    if !push_msg(&mut transaction, host_ext_id, 6, builder) {
                        return Action::Drop;
                    }
                } else if extension_version_allows(host_ext_version, 2)
                    && !push_msg(&mut transaction, host_ext_id, 1, builder)
                {
                    return Action::Drop;
                }
            }
        }

        if let Some((x, y, w, h)) = plan.cursor_rect {
            log::debug!(
                "  -> sending v1 set_cursor_rectangle({}, {}, {}, {})",
                x,
                y,
                w,
                h
            );
            // set_cursor_rectangle: opcode 7
            let mut builder = MessageBuilder::new();
            builder.write_i32(x);
            builder.write_i32(y);
            builder.write_i32(w);
            builder.write_i32(h);
            if !push_msg(&mut transaction, plan.host_v1_id, 7, builder) {
                return Action::Drop;
            }
        }

        let host_commit_serial =
            ctx.next_host_text_input_commit_serial(plan.host_v1_id, plan.serial);
        log::debug!(
            "  -> sending v1 commit_state(serial={}, guest_serial={})",
            host_commit_serial,
            plan.serial
        );
        // commit_state: opcode 9
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_commit_serial);
        if !push_msg(&mut transaction, plan.host_v1_id, 9, builder) {
            return Action::Drop;
        }

        if plan.enable_conflict {
            log::warn!(
                "Ignoring text-input enable for guest {}: another input is enabled on seat {}",
                guest_id,
                plan.guest_seat
            );
        }
        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("focused text input disappeared while committing state");
        let end_repeat = plan.resets_composition;
        let guest_seat = plan.guest_seat;
        state.finish_guest_commit(plan);
        if end_repeat {
            end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        }

        // A placement recovery that was deferred while the guest editor was
        // disabled must publish this v3 editor transaction before its
        // reset/deactivate/sync barrier. Otherwise the barrier callback can
        // reactivate the host generation before the newly committed editor
        // state reaches Exo, leaving Korean composition disabled again.
        if placement_refresh_pending && plan_enabled {
            ctx.client_to_host_queue.extend(transaction);
            update_host_activation(ctx, guest_id);
        } else {
            update_host_activation(ctx, guest_id);
            ctx.client_to_host_queue.extend(transaction);
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::callback::CallbackHandler;
    use crate::handler::keyboard::KeyboardHandler;
    use crate::handler::seat::SeatHandler;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::wire::WireMessage;

    /// Helper: extract opcode from a wire message at the given index in a queue.
    fn msg_opcode(queue: &[(Vec<u8>, Vec<std::os::unix::io::RawFd>)], idx: usize) -> u16 {
        let word2 = u32::from_ne_bytes(queue[idx].0[4..8].try_into().unwrap());
        (word2 & 0xffff) as u16
    }

    /// Helper: extract sender_id from a wire message.
    fn msg_sender(queue: &[(Vec<u8>, Vec<std::os::unix::io::RawFd>)], idx: usize) -> u32 {
        u32::from_ne_bytes(queue[idx].0[0..4].try_into().unwrap())
    }

    fn set_test_focus(
        ctx: &mut Context,
        host_keyboard: HostId,
        guest_seat: u32,
        guest_surface: u32,
    ) {
        ctx.keyboard_focus
            .set_for_test(host_keyboard, guest_seat, guest_surface, guest_surface);
    }

    #[test]
    fn oversized_host_text_is_dropped_without_panicking() {
        let mut queue = Vec::new();
        let mut builder = MessageBuilder::new();
        builder.write_string(&"x".repeat(65_528));

        assert!(!push_msg(&mut queue, 7, 2, builder));
        assert!(queue.is_empty());
    }

    #[test]
    fn oversized_surrounding_commit_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let oversized = "x".repeat(65_528);
        let mut handler = TextInputV3Handler;

        assert_eq!(
            handler.on_set_surrounding_text(
                &mut ctx,
                &oversized,
                oversized.len() as i32,
                oversized.len() as i32,
            ),
            Action::Drop
        );
        assert_eq!(handler.on_set_content_type(&mut ctx, 0, 13), Action::Drop);
        let before = ctx.text_inputs[&guest_id].clone();
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.client_to_host_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state, &before);
        assert_eq!(state.guest_commit_serial, 0);
        assert!(state.committed_surrounding_text.is_none());
        assert!(state.surrounding_text_dirty);
        assert!(state.committed_content_type.is_none());
        assert!(state.content_type_dirty);
    }

    /// Helper: set up a context with a host→guest mapping for text input testing.
    fn setup_v1_ctx() -> (Context, u32, u32) {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_v1_id = 10u32;
        let guest_id = 20u32;
        let host_ext_id = 30u32;
        // Keep the fixture's historical guest seat ID (0), but provide the
        // real host mapping that production activation requests require.
        ctx.shadow_table.map_id(0, 2);
        ctx.shadow_table.track_interface(0, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_id, host_v1_id);
        ctx.shadow_table
            .track_interface(guest_id, "zwp_text_input_v3".to_string());
        ctx.shadow_table
            .track_host_interface(host_v1_id, "zwp_text_input_v1".to_string());
        ctx.text_inputs.insert(
            guest_id,
            crate::state::TextInputState {
                host_v1_id,
                host_ext_id: Some(host_ext_id),
                guest_seat: 0,
                // Most unit tests exercise the v3 transaction bridge without
                // modelling a keyboard enter. Use a stable synthetic focus
                // by default; tests covering the protocol's pre-enter/after-
                // leave behavior explicitly clear this field.
                active_surface: Some(900),
                pending_enabled: true,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: String::new(),
                guest_commit_serial: 0,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: HostActivationState::Active,
                placement_ime: crate::state::PlacementImeState::None,
            },
        );
        (ctx, host_v1_id, guest_id)
    }

    #[test]
    fn late_host_leave_restarts_focused_ime_generation() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let mut handler = TextInputV1Handler;
        ctx.keyboard_focus.set_for_test(HostId(99), 0, 900, 900);

        // A transient Aura identity change can make Exo emit text-input.leave
        // after the placement refresh has already reactivated the reused v1
        // object.  The guest still owns the same focused surface, so this
        // leave is stale rather than a guest focus boundary.
        ctx.last_sender_id = host_v1_id;
        assert_eq!(handler.on_leave(&mut ctx), Action::Drop);

        assert!(
            matches!(
                ctx.text_inputs[&guest_id].host_activation(),
                HostActivationState::Draining { .. }
            ),
            "a stale host leave must drain the generation before reactivation"
        );
        assert_eq!(
            ctx.client_to_host_queue.len(),
            3,
            "reset, deactivate, and sync must be ordered as one recovery"
        );
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 0),
            zwp_text_input_v1::REQ_RESET
        );
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 1), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 2), 1);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 2),
            wl_display::REQ_SYNC
        );
    }

    fn msg_done_serial(queue: &[(Vec<u8>, Vec<std::os::unix::io::RawFd>)], idx: usize) -> u32 {
        let payload = &queue[idx].0[8..];
        u32::from_ne_bytes(payload[0..4].try_into().unwrap())
    }

    fn wire_message(
        queue: &[(Vec<u8>, Vec<std::os::unix::io::RawFd>)],
        idx: usize,
    ) -> WireMessage<'_> {
        WireMessage::new(
            msg_sender(queue, idx),
            msg_opcode(queue, idx),
            &queue[idx].0[8..],
            &queue[idx].1,
        )
    }

    fn hold_repeatable_peek_key(
        ctx: &mut Context,
        host_keyboard_id: HostId,
        key: u32,
        serial: u32,
        time: u32,
    ) {
        ctx.key_generations
            .observe_peek_press(host_keyboard_id, key, serial, time, true);
        ctx.keyboard_repeatable_keys
            .entry(host_keyboard_id)
            .or_default()
            .insert(key);
    }

    fn arm_backspace_repeat_for_test(
        ctx: &mut Context,
        guest_keyboard_id: u32,
        host_keyboard_id: HostId,
        guest_text_input: u32,
    ) {
        let backspace = crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
        ctx.shadow_table
            .map_id(guest_keyboard_id, host_keyboard_id.0);
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.key_generations
            .observe_physical_state(host_keyboard_id, backspace, 1);
        assert!(ctx
            .key_generations
            .arm_ime_repeat(host_keyboard_id, backspace, guest_text_input));
    }

    #[test]
    fn ending_one_keyboard_repeat_does_not_cancel_another_held_keyboard() {
        let (mut ctx, _, _) = setup_v1_ctx();
        let backspace = crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
        for (guest_keyboard, host_keyboard) in [(40, 41), (42, 43)] {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.keyboard_to_seat.insert(guest_keyboard, 0);
            ctx.key_generations
                .observe_physical_state(HostId(host_keyboard), backspace, 1);
        }
        assert!(ctx
            .key_generations
            .arm_ime_repeat(HostId(41), backspace, 40));

        end_backspace_repeat_for_seat(&mut ctx, 0);

        assert!(ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(41), backspace));
        assert!(!ctx
            .key_generations
            .backspace_repeat_cancelled(HostId(43), backspace));
        assert!(ctx.key_generations.physically_held(HostId(43), backspace));
    }

    fn extension_opcodes_for_version(version: Option<u32>) -> Vec<u16> {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        if let Some(version) = version {
            ctx.shadow_table.track_host_interface_with_version(
                30,
                "zcr_extended_text_input_v1".to_string(),
                version,
            );
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_set_content_type(&mut ctx, 0, 9);
        handler.on_commit(&mut ctx);
        ctx.client_to_host_queue
            .iter()
            .filter(|message| msg_sender(std::slice::from_ref(message), 0) == 30)
            .map(|message| msg_opcode(std::slice::from_ref(message), 0))
            .collect()
    }

    #[test]
    fn on_preedit_string_sends_v3_preedit_and_done() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.guest_commit_serial = 42;
        }

        let mut handler = TextInputV1Handler;
        let text = "こんにちは".to_string();
        let action = handler.on_preedit_string(&mut ctx, 42, &text, &String::new());
        assert_eq!(action, Action::Drop);

        // Should produce exactly 2 messages: preedit_string (opcode 2) + done (opcode 5)
        assert_eq!(ctx.host_to_client_queue.len(), 2);

        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), guest_id);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 2); // preedit_string

        assert_eq!(msg_sender(&ctx.host_to_client_queue, 1), guest_id);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 5); // done
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 1), 42); // serial
    }

    #[test]
    fn empty_preedit_applies_v1_reset_fallback() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("test text input")
            .current_preedit = "미완성".to_string();

        let mut handler = TextInputV1Handler;
        let action = handler.on_preedit_string(&mut ctx, 42, &"".to_string(), &"확정".to_string());
        assert_eq!(action, Action::Drop);

        // v3 commit_string removes the old composition; the empty preedit
        // event then records the replacement state, followed by done.
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 3);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 2);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 2), 5);
        let commit_payload = &ctx.host_to_client_queue[0].0[8..];
        let commit_len =
            u32::from_ne_bytes(commit_payload[0..4].try_into().expect("commit length")) as usize;
        assert_eq!(&commit_payload[4..4 + commit_len - 1], "확정".as_bytes());
        assert!(!ime_repeat_active_for_seat(&ctx, 0));
    }

    #[test]
    fn live_preedit_does_not_commit_v1_reset_fallback() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        let action = handler.on_preedit_string(
            &mut ctx,
            42,
            &"간".to_string(),
            &"reset fallback".to_string(),
        );
        assert_eq!(action, Action::Drop);

        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 2);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 5);
        assert_eq!(ctx.text_inputs[&guest_id].current_preedit, "간");
    }

    #[test]
    fn oversized_preedit_does_not_partially_advance_state() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "old".to_string();
            state.pending_preedit_cursor = Some(1);
            state.pending_preedit_selection = Some((0, 1));
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        let oversized = "x".repeat(65_528);
        assert_eq!(
            TextInputV1Handler.on_preedit_string(&mut ctx, 0, &oversized, &String::new()),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.current_preedit, "old");
        assert_eq!(state.pending_preedit_cursor, Some(1));
        assert_eq!(state.pending_preedit_selection, Some((0, 1)));
        assert!(backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn oversized_commit_does_not_partially_advance_state() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "old".to_string();
            state.pending_deletes.push((3, 0));
            state.pending_cursor_position = Some((1, 1));
            state.pending_preedit_cursor = Some(1);
            state.pending_preedit_selection = Some((0, 1));
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        let oversized = "x".repeat(65_528);
        assert_eq!(
            TextInputV1Handler.on_commit_string(&mut ctx, 0, &oversized),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.current_preedit, "old");
        assert_eq!(state.pending_deletes, vec![(3, 0)]);
        assert_eq!(state.pending_cursor_position, Some((1, 1)));
        assert_eq!(state.pending_preedit_cursor, Some(1));
        assert_eq!(state.pending_preedit_selection, Some((0, 1)));
        assert!(backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn korean_trace_commits_each_completed_syllable_once() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        for text in ["ㄱ", "가", "간"] {
            handler.on_preedit_string(&mut ctx, 1, &text.to_string(), &text.to_string());
        }
        handler.on_commit_string(&mut ctx, 2, &"가".to_string());
        handler.on_preedit_string(&mut ctx, 3, &"나".to_string(), &"나".to_string());
        handler.on_preedit_string(&mut ctx, 4, &"낟".to_string(), &"낟".to_string());
        handler.on_commit_string(&mut ctx, 5, &"나".to_string());
        handler.on_preedit_string(&mut ctx, 6, &"다".to_string(), &"다".to_string());

        let commits = ctx
            .host_to_client_queue
            .iter()
            .filter(|message| msg_opcode(std::slice::from_ref(message), 0) == 3)
            .map(|message| {
                wire_message(std::slice::from_ref(message), 0)
                    .read_string()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(commits, ["가", "나"]);
        assert_eq!(ctx.text_inputs[&guest_id].current_preedit, "다");
    }

    #[test]
    fn reset_fallback_without_previous_preedit_is_not_inserted() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        let action = TextInputV1Handler.on_preedit_string(
            &mut ctx,
            42,
            &String::new(),
            &"stale fallback".to_string(),
        );

        assert_eq!(action, Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(|message| msg_opcode(std::slice::from_ref(message), 0))
                .collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert!(ctx.text_inputs[&guest_id].current_preedit.is_empty());
    }

    #[test]
    fn on_commit_string_sends_preedit_clear_then_commit_then_done() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.guest_commit_serial = 42;
            state.current_preedit = "미완성".to_string();
        }

        let mut handler = TextInputV1Handler;
        let text = "確定".to_string();
        let action = handler.on_commit_string(&mut ctx, 42, &text);
        assert_eq!(action, Action::Drop);

        // Should produce 3 messages: preedit_string("") + commit_string + done
        assert_eq!(ctx.host_to_client_queue.len(), 3);

        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 2); // preedit_string (clear)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 3); // commit_string
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 2), 5); // done
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 2), 42); // serial

        // All messages should target the guest_id
        for i in 0..3 {
            assert_eq!(msg_sender(&ctx.host_to_client_queue, i), guest_id);
        }
    }

    #[test]
    fn empty_commit_clears_stale_backspace_repeat_state() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_commit_string(&mut ctx, 1, &String::new()),
            Action::Drop
        );
        assert!(!backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn on_delete_surrounding_text_negative_index_spanning_cursor() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.guest_commit_serial = 42;
        }

        let mut handler = TextInputV1Handler;
        let action = handler.on_delete_surrounding_text(&mut ctx, -3, 5);
        assert_eq!(action, Action::Drop);

        // v1 deletion is buffered until the following commit_string.
        assert!(ctx.host_to_client_queue.is_empty());

        let action = handler.on_commit_string(&mut ctx, 42, &String::new());
        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 3);

        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), guest_id);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 4);
        let payload = &ctx.host_to_client_queue[0].0[8..];
        let before_length = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
        let after_length = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        assert_eq!(before_length, 3);
        assert_eq!(after_length, 2);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 3);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 2), 5);
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 2), 42);
    }

    #[test]
    fn non_adjacent_delete_is_ignored_instead_of_deleting_intervening_text() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        let action = handler.on_delete_surrounding_text(&mut ctx, -5, 3);
        assert_eq!(action, Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
        handler.on_commit_string(&mut ctx, 1, &String::new());

        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(|message| msg_opcode(std::slice::from_ref(message), 0))
                .collect::<Vec<_>>(),
            vec![3, 5]
        );
    }

    #[test]
    fn multiple_v1_deletes_are_preserved_until_commit_string() {
        let (mut ctx, host_v1_id, _) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        handler.on_delete_surrounding_text(&mut ctx, -3, 3);
        handler.on_delete_surrounding_text(&mut ctx, 0, 2);
        assert!(ctx.host_to_client_queue.is_empty());
        handler.on_commit_string(&mut ctx, 0, &"한".to_string());

        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(|message| msg_opcode(std::slice::from_ref(message), 0))
                .collect::<Vec<_>>(),
            vec![4, 4, 3, 5]
        );
        let mut first_delete = wire_message(&ctx.host_to_client_queue, 0);
        assert_eq!(first_delete.read_u32().unwrap(), 3);
        assert_eq!(first_delete.read_u32().unwrap(), 0);
        let mut second_delete = wire_message(&ctx.host_to_client_queue, 1);
        assert_eq!(second_delete.read_u32().unwrap(), 0);
        assert_eq!(second_delete.read_u32().unwrap(), 2);
    }

    #[test]
    fn on_set_preedit_region_translates_correctly() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        let host_ext_id = 30u32;
        ctx.last_sender_id = host_ext_id;

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.committed_surrounding_text = Some(("가나다".to_string(), 6, 6));
            state.guest_commit_serial = 42;
        }

        let mut handler = ExtendedTextInputV1Handler;
        let action = handler.on_set_preedit_region(&mut ctx, -3, 3); // "나" (3 bytes)
        assert_eq!(action, Action::Drop);

        // Should produce 3 messages: delete_surrounding_text, preedit_string, done
        assert_eq!(ctx.host_to_client_queue.len(), 3);

        // 1. delete_surrounding_text (opcode 4)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 4);
        let payload = &ctx.host_to_client_queue[0].0[8..];
        let before_length = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
        let after_length = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        assert_eq!(before_length, 3);
        assert_eq!(after_length, 0);

        // 2. preedit_string (opcode 2)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 2);
        let payload = &ctx.host_to_client_queue[1].0[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let preedit_str = String::from_utf8(payload[4..4 + str_len - 1].to_vec()).unwrap();
        assert_eq!(preedit_str, "나");
        let cursor_offset = 4 + ((str_len + 3) & !3);
        let cursor_begin = i32::from_ne_bytes(
            payload[cursor_offset..cursor_offset + 4]
                .try_into()
                .unwrap(),
        );
        let cursor_end = i32::from_ne_bytes(
            payload[cursor_offset + 4..cursor_offset + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!((cursor_begin, cursor_end), (3, 3));
        let mut preedit = wire_message(&ctx.host_to_client_queue, 1);
        assert_eq!(
            preedit.read_nullable_string().unwrap(),
            Some("나".to_string())
        );
        assert_eq!(preedit.read_i32().unwrap(), 3);
        assert_eq!(preedit.read_i32().unwrap(), 3);
        assert!(
            preedit.is_payload_consumed(),
            "translated preedit must contain only its declared fields"
        );

        // 3. done (opcode 5)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 2), 5);
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 2), 42); // serial

        // Cached preedit should be updated
        if let Some(state) = ctx.text_inputs.get(&guest_id) {
            assert_eq!(state.current_preedit, "나");
        } else {
            panic!("state not found");
        }
    }

    #[test]
    fn setting_preedit_region_clears_stale_backspace_repeat() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_surrounding_text = Some(("가나다".to_string(), 9, 9));
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        let mut handler = ExtendedTextInputV1Handler;
        assert_eq!(handler.on_set_preedit_region(&mut ctx, -3, 3), Action::Drop);
        assert!(!backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn oversized_preedit_region_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        let oversized = "x".repeat(65_528);
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_surrounding_text = Some((oversized, 65_528, 65_528));
            state.pending_preedit_cursor = Some(7);
            state.pending_preedit_selection = Some((1, 2));
            state.current_preedit = "old".to_string();
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        assert_eq!(
            ExtendedTextInputV1Handler.on_set_preedit_region(&mut ctx, -65_528, 65_528),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.pending_preedit_cursor, Some(7));
        assert_eq!(state.pending_preedit_selection, Some((1, 2)));
        assert_eq!(state.current_preedit, "old");
        assert!(backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn non_adjacent_preedit_region_is_ignored_instead_of_deleting_text_gap() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .committed_surrounding_text = Some(("가나다".to_string(), 9, 9));
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_set_preedit_region(&mut ctx, -9, 3), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.text_inputs[&guest_id].current_preedit.is_empty());
    }

    #[test]
    fn on_confirm_preedit_commits_cached_preedit() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        let host_ext_id = 30u32;
        ctx.last_sender_id = host_ext_id;

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.current_preedit = "나".to_string();
            state.guest_commit_serial = 42;
            state.record_preedit_selection(0, 3);
            state.record_preedit_cursor(3);
            assert!(state.record_delete(3, 0));
            state.record_cursor_position(1, 1);
        }

        let mut handler = ExtendedTextInputV1Handler;
        let action = handler.on_confirm_preedit(&mut ctx, 0);
        assert_eq!(action, Action::Drop);

        // Should produce 3 messages: preedit_string(""), commit_string, done
        assert_eq!(ctx.host_to_client_queue.len(), 3);

        // 0. preedit_string("") (opcode 2)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 2);

        // 1. commit_string (opcode 3)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 1), 3);
        let payload = &ctx.host_to_client_queue[1].0[8..];
        let str_len = u32::from_ne_bytes(payload[0..4].try_into().unwrap()) as usize;
        let commit_str = String::from_utf8(payload[4..4 + str_len - 1].to_vec()).unwrap();
        assert_eq!(commit_str, "나");

        // 2. done (opcode 5)
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 2), 5);
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 2), 42); // serial

        // Confirming commits only the preedit. v1 delete/cursor metadata
        // belongs to the following commit_string transaction.
        if let Some(state) = ctx.text_inputs.get(&guest_id) {
            assert_eq!(state.current_preedit, "");
            assert!(state.pending_preedit_selection.is_none());
            assert!(state.pending_preedit_cursor.is_none());
            assert_eq!(state.pending_deletes, vec![(3, 0)]);
            assert_eq!(state.pending_cursor_position, Some((1, 1)));
        } else {
            panic!("state not found");
        }

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 10;
        assert_eq!(
            TextInputV1Handler.on_commit_string(&mut ctx, 42, &"다".to_string()),
            Action::Drop
        );
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 4);
        assert!(ctx.text_inputs[&guest_id].pending_deletes.is_empty());
        assert!(ctx.text_inputs[&guest_id].pending_cursor_position.is_none());
    }

    #[test]
    fn oversized_confirm_preedit_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "x".repeat(65_528);
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 0),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.current_preedit.len(), 65_528);
        assert!(backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn guest_commit_count_is_sent_to_v1_and_used_by_v3_done() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut v3_handler = TextInputV3Handler;
        v3_handler.on_commit(&mut ctx);
        v3_handler.on_commit(&mut ctx);

        let commit_serials: Vec<u32> = ctx
            .client_to_host_queue
            .iter()
            .filter_map(|(msg, _)| {
                let opcode = u32::from_ne_bytes(msg[4..8].try_into().unwrap()) & 0xffff;
                (opcode == 9).then(|| u32::from_ne_bytes(msg[8..12].try_into().unwrap()))
            })
            .collect();
        assert_eq!(commit_serials, vec![1, 2]);

        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activation = HostActivationState::Active;
        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = host_v1_id;
        let mut v1_handler = TextInputV1Handler;
        v1_handler.on_preedit_string(&mut ctx, 2942, &"한".to_string(), &String::new());
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 1), 2);

        // Multiple host events based on the same guest state must not advance
        // the v3 serial, regardless of Exo's independent v1 event serials.
        ctx.host_to_client_queue.clear();
        v1_handler.on_preedit_string(&mut ctx, 2944, &"한글".to_string(), &String::new());
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, 1), 2);

        ctx.host_to_client_queue.clear();
        v1_handler.on_commit_string(&mut ctx, 2945, &"한글".to_string());
        let done_index = ctx.host_to_client_queue.len() - 1;
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, done_index), 5);
        assert_eq!(msg_done_serial(&ctx.host_to_client_queue, done_index), 2);
    }

    #[test]
    fn confirm_preedit_without_preedit_has_no_effect() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .guest_commit_serial = 2;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn input_mode_switch_boundary_clears_stale_repeat_before_empty_confirmation() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.keyboard_to_seat.insert(40, 0);
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_surrounding_text = Some(("english".to_string(), 7, 7));
            state.pending_surrounding_text = state.committed_surrounding_text.clone();
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);
        end_backspace_repeat_for_seat(&mut ctx, 0);
        ctx.last_sender_id = 30;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a routine empty confirmation must not become a synthetic Backspace"
        );
        assert_eq!(
            ctx.text_inputs[&guest_id].committed_surrounding_text,
            Some(("english".to_string(), 7, 7))
        );
    }

    #[test]
    fn held_backspace_repeats_over_committed_korean_without_preedit() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        let host_keyboard_id = 41;
        ctx.shadow_table.map_id(40, host_keyboard_id);
        ctx.keyboard_to_seat.insert(40, 0);
        hold_repeatable_peek_key(
            &mut ctx,
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            700,
            100,
        );
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .committed_surrounding_text = Some(("가나다라".to_string(), 12, 12));
        ctx.last_sender_id = 30;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 9);
        assert!(backspace_repeat_active_for_keyboard(
            &ctx,
            HostId(host_keyboard_id)
        ));
        let mut key_serials = Vec::new();
        let (transactions, remainder) = ctx.host_to_client_queue.as_chunks::<3>();
        assert!(
            remainder.is_empty(),
            "synthetic key transactions must have three messages"
        );
        for messages in transactions {
            assert_eq!(msg_sender(messages, 0), 40);
            assert_eq!(msg_opcode(messages, 0), 3);
            assert_eq!(msg_sender(&messages[1..], 0), 40);
            assert_eq!(msg_opcode(&messages[1..], 0), 3);
            assert_eq!(msg_sender(&messages[2..], 0), guest_id);
            assert_eq!(msg_opcode(&messages[2..], 0), 5);
            key_serials.push(wire_message(messages, 0).read_u32().unwrap());
            key_serials.push(wire_message(&messages[1..], 0).read_u32().unwrap());
        }
        key_serials.sort_unstable();
        key_serials.dedup();
        assert_eq!(
            key_serials.len(),
            6,
            "each synthetic key event must have a distinct serial"
        );
    }

    #[test]
    fn empty_confirmation_recovers_any_repeatable_ime_consumed_key() {
        for key in [
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            15, // Tab
            28, // Enter
            57, // Space
        ] {
            let (mut ctx, _, guest_id) = setup_v1_ctx();
            let guest_keyboard_id = 40;
            let host_keyboard_id = HostId(41);
            ctx.shadow_table
                .map_id(guest_keyboard_id, host_keyboard_id.0);
            ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
            hold_repeatable_peek_key(&mut ctx, host_keyboard_id, key, 700, 100);
            ctx.last_sender_id = 30;

            assert_eq!(
                ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
                Action::Drop
            );
            assert_eq!(ctx.host_to_client_queue.len(), 3);
            for (message, expected_state) in ctx.host_to_client_queue[..2].iter().zip([
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_RELEASED,
            ]) {
                assert_eq!(
                    msg_sender(std::slice::from_ref(message), 0),
                    guest_keyboard_id
                );
                let mut event = wire_message(std::slice::from_ref(message), 0);
                event.read_u32().unwrap();
                assert_eq!(event.read_u32().unwrap(), 100);
                assert_eq!(event.read_u32().unwrap(), key);
                assert_eq!(event.read_u32().unwrap(), expected_state);
            }
            assert_eq!(msg_sender(&ctx.host_to_client_queue[2..], 0), guest_id);
            assert_eq!(msg_opcode(&ctx.host_to_client_queue[2..], 0), 5);
        }
    }

    #[test]
    fn empty_confirmation_does_not_fall_back_from_newest_ineligible_key() {
        let (mut ctx, _, _) = setup_v1_ctx();
        let host_keyboard_id = HostId(41);
        ctx.shadow_table.map_id(40, host_keyboard_id.0);
        ctx.keyboard_to_seat.insert(40, 0);
        hold_repeatable_peek_key(&mut ctx, host_keyboard_id, 57, 700, 100);

        // Shift is newer but XKB does not mark it repeatable. The confirmation
        // belongs to that newest generation or to no recoverable key; it must
        // never synthesize the older held Space.
        ctx.key_generations
            .observe_peek_press(host_keyboard_id, 42, 701, 101, true);
        ctx.last_sender_id = 30;

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());

        ctx.key_generations
            .observe_physical_state(host_keyboard_id, 42, 0);
        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "releasing the superseding key must not revive an older generation"
        );
    }

    #[test]
    fn empty_confirmation_prefers_the_keyboard_focused_on_the_current_surface() {
        let (mut ctx, _, _) = setup_v1_ctx();
        let focused_host_keyboard = HostId(41);
        let stale_host_keyboard = HostId(51);
        for (guest_keyboard, host_keyboard) in
            [(40, focused_host_keyboard), (50, stale_host_keyboard)]
        {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard.0);
            ctx.keyboard_to_seat.insert(guest_keyboard, 0);
        }
        set_test_focus(&mut ctx, focused_host_keyboard, 0, 900);
        hold_repeatable_peek_key(&mut ctx, focused_host_keyboard, 57, 700, 100);
        hold_repeatable_peek_key(&mut ctx, stale_host_keyboard, 28, 701, 101);
        ctx.last_sender_id = 30;

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), 40);
        let mut press = wire_message(&ctx.host_to_client_queue, 0);
        press.read_u32().unwrap();
        press.read_u32().unwrap();
        assert_eq!(press.read_u32().unwrap(), 57);
    }

    #[test]
    fn empty_confirmation_never_uses_a_stale_unfocused_keyboard() {
        let (mut ctx, _, _) = setup_v1_ctx();
        let stale_host_keyboard = HostId(41);
        ctx.shadow_table.map_id(40, stale_host_keyboard.0);
        ctx.keyboard_to_seat.insert(40, 0);
        set_test_focus(&mut ctx, HostId(99), 0, 900);
        hold_repeatable_peek_key(&mut ctx, stale_host_keyboard, 57, 700, 100);
        ctx.last_sender_id = 30;

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "an active surface without a matching keyboard must not use stale focus"
        );
    }

    #[test]
    fn emptied_preedit_enables_held_backspace_fallback_for_committed_text() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 40;
        ctx.shadow_table.map_id(guest_keyboard_id, 41);
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        hold_repeatable_peek_key(
            &mut ctx,
            HostId(41),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            701,
            101,
        );
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.guest_commit_serial = 2;
            state.committed_surrounding_text = Some(("A가".to_string(), 4, 4));
            state.pending_surrounding_text = state.committed_surrounding_text.clone();
        }
        ctx.last_sender_id = host_v1_id;
        let mut v1_handler = TextInputV1Handler;
        v1_handler.on_preedit_string(&mut ctx, 100, &"가".to_string(), &String::new());
        v1_handler.on_preedit_string(&mut ctx, 101, &String::new(), &String::new());
        ctx.host_to_client_queue.clear();

        ctx.last_sender_id = 30;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(|message| msg_opcode(std::slice::from_ref(message), 0))
                .collect::<Vec<_>>(),
            vec![3, 3, 5]
        );
        for message in &ctx.host_to_client_queue[..2] {
            assert_eq!(
                msg_sender(std::slice::from_ref(message), 0),
                guest_keyboard_id
            );
        }
        let mut press = wire_message(&ctx.host_to_client_queue, 0);
        let press_serial = press.read_u32().unwrap();
        let press_time = press.read_u32().unwrap();
        assert_eq!(press.read_u32().unwrap(), 14);
        assert_eq!(press.read_u32().unwrap(), 1);
        let mut release = wire_message(&ctx.host_to_client_queue, 1);
        let release_serial = release.read_u32().unwrap();
        assert_ne!(press_serial, release_serial);
        assert_eq!(release.read_u32().unwrap(), press_time);
        assert_eq!(release.read_u32().unwrap(), 14);
        assert_eq!(release.read_u32().unwrap(), 0);
        assert_eq!(
            ctx.text_inputs[&guest_id].committed_surrounding_text,
            Some(("A가".to_string(), 4, 4))
        );

        ctx.host_to_client_queue.clear();
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 3);

        end_backspace_repeat_for_seat(&mut ctx, 0);
        ctx.host_to_client_queue.clear();
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn empty_confirmation_does_not_duplicate_forwarded_backspace() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 40;
        let host_keyboard_id = 41;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        hold_repeatable_peek_key(
            &mut ctx,
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            1,
            100,
        );
        assert!(ctx.claim_guest_key(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            GuestKeyOwner::Physical
        ));
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "가".to_string();
            state.committed_surrounding_text = Some(("A가".to_string(), 4, 4));
        }

        // Model a normal physical press that was already forwarded, followed
        // by the host IME clearing its preedit and confirming the transaction.
        ctx.last_sender_id = host_v1_id;
        let mut v1_handler = TextInputV1Handler;
        v1_handler.on_preedit_string(&mut ctx, 1, &String::new(), &String::new());
        assert!(backspace_repeat_active_for_keyboard(
            &ctx,
            HostId(host_keyboard_id)
        ));

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 30;
        let mut extended_handler = ExtendedTextInputV1Handler;
        assert_eq!(
            extended_handler.on_confirm_preedit(&mut ctx, 0),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), guest_id);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 5);
        assert!(!backspace_repeat_active_for_keyboard(
            &ctx,
            HostId(host_keyboard_id)
        ));
    }

    #[test]
    fn held_backspace_fallback_requires_guest_keyboard_for_seat() {
        let (mut ctx, _, _) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn preedit_cursor_and_selection_are_forwarded_as_byte_ranges() {
        let (mut ctx, host_v1_id, _) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        handler.on_preedit_cursor(&mut ctx, 3);
        handler.on_preedit_string(&mut ctx, 0, &"가나".to_string(), &String::new());
        let mut preedit = wire_message(&ctx.host_to_client_queue, 0);
        assert_eq!(preedit.read_string().unwrap(), "가나");
        assert_eq!(preedit.read_i32().unwrap(), 3);
        assert_eq!(preedit.read_i32().unwrap(), 3);

        ctx.host_to_client_queue.clear();
        handler.on_preedit_styling(&mut ctx, 0, 3, 6);
        handler.on_preedit_cursor(&mut ctx, 6);
        handler.on_preedit_string(&mut ctx, 0, &"가나".to_string(), &String::new());
        let mut preedit = wire_message(&ctx.host_to_client_queue, 0);
        assert_eq!(preedit.read_string().unwrap(), "가나");
        assert_eq!(preedit.read_i32().unwrap(), 0);
        assert_eq!(preedit.read_i32().unwrap(), 3);
    }

    #[test]
    fn invalid_preedit_utf8_ranges_fall_back_to_string_end() {
        let (mut ctx, host_v1_id, _) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        handler.on_preedit_cursor(&mut ctx, 1);
        handler.on_preedit_string(&mut ctx, 0, &"가".to_string(), &String::new());
        let mut preedit = wire_message(&ctx.host_to_client_queue, 0);
        assert_eq!(preedit.read_string().unwrap(), "가");
        assert_eq!(preedit.read_i32().unwrap(), 3);
        assert_eq!(preedit.read_i32().unwrap(), 3);

        ctx.host_to_client_queue.clear();
        handler.on_preedit_styling(&mut ctx, 1, 1, 6);
        handler.on_preedit_string(&mut ctx, 0, &"가".to_string(), &String::new());
        let mut preedit = wire_message(&ctx.host_to_client_queue, 0);
        assert_eq!(preedit.read_string().unwrap(), "가");
        assert_eq!(preedit.read_i32().unwrap(), 3);
        assert_eq!(preedit.read_i32().unwrap(), 3);
    }

    #[test]
    fn enable_is_not_activated_until_commit() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        let guest_surface = 40;
        ctx.shadow_table.map_id(guest_surface, 41);
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.active_surface = Some(guest_surface);
            state.pending_enabled = false;
            state.committed_enabled = false;
            state.host_activation = HostActivationState::Inactive;
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_enable(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert!(ctx.client_to_host_queue.is_empty());

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.text_inputs[&guest_id].host_is_active());
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 0);
    }

    #[test]
    fn focus_generation_barrier_drops_old_events_before_reactivation() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let next_surface = 901;
        ctx.shadow_table.map_id(next_surface, 902);
        ctx.shadow_table
            .track_interface(next_surface, "wl_surface".to_string());

        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        update_host_activation(&mut ctx, guest_id);

        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 1), 1);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 1),
            wl_display::REQ_SYNC
        );
        let callback_id = ctx.text_inputs[&guest_id]
            .draining_callback()
            .expect("deactivation must open a barrier");

        // Events already queued by the old surface are dispatched before
        // callback.done and must not mutate or reach the next guest focus.
        let mut v1 = TextInputV1Handler;
        ctx.last_sender_id = host_v1_id;
        assert_eq!(
            v1.on_preedit_string(&mut ctx, 1, &"old".to_string(), &String::new()),
            Action::Drop
        );
        assert_eq!(
            v1.on_commit_string(&mut ctx, 1, &"old".to_string()),
            Action::Drop
        );
        assert!(ctx.host_to_client_queue.is_empty());

        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = true;
            state.active_surface = Some(next_surface);
        }
        update_host_activation(&mut ctx, guest_id);
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "activation must remain deferred while old events drain"
        );

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert!(ctx.text_inputs[&guest_id].host_is_active());
        assert!(ctx.shadow_table.is_pending_destroy_host_only(callback_id.0));
        assert_eq!(ctx.client_to_host_queue.len(), 3);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 2), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 2), 0);

        // Once the ordered barrier completes, events belong to the new focus
        // generation and may be translated normally.
        ctx.last_sender_id = host_v1_id;
        assert_eq!(
            v1.on_preedit_string(&mut ctx, 2, &"new".to_string(), &String::new()),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 2);
    }

    #[test]
    fn placement_refresh_drains_and_reactivates_focused_host_text_input() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let focused_surface = ctx.text_inputs[&guest_id]
            .active_surface
            .expect("fixture must start focused");
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_surrounding_text = Some(("editor".to_string(), 6, 6));
            state.committed_content_type = Some((0, 13));
            state.cursor_rect = Some((1, 2, 3, 4));
            state.guest_commit_serial = 17;
        }
        // Model the v1 serial already consumed by the guest's last normal
        // commit. The placement replay must advance it even though the guest
        // v3 serial remains 17 and no new guest commit was received.
        assert_eq!(ctx.next_host_text_input_commit_serial(host_v1_id, 17), 17);
        ctx.shadow_table
            .map_id(focused_surface, focused_surface + 1);
        ctx.shadow_table
            .track_interface(focused_surface, "wl_surface".to_string());

        assert!(refresh_host_activation_for_surface(
            &mut ctx,
            focused_surface
        ));
        let callback_id = ctx.text_inputs[&guest_id]
            .draining_callback()
            .expect("placement refresh must install a drain barrier");
        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert_eq!(
            ctx.client_to_host_queue.len(),
            3,
            "placement refresh must reset before deactivation"
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), host_v1_id);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 0),
            zwp_text_input_v1::REQ_RESET
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 1), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 1), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 2), 1);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 2),
            wl_display::REQ_SYNC
        );

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert!(ctx.text_inputs[&guest_id].host_is_active());
        assert_eq!(
            ctx.client_to_host_queue.len(),
            4,
            "refresh must wait for host text-input enter before replaying editor state"
        );
        let host_surface = ctx
            .shadow_table
            .get_host_id(focused_surface)
            .expect("focused surface host mapping");
        ctx.last_sender_id = host_v1_id;
        let mut v1 = TextInputV1Handler;
        assert_eq!(v1.on_enter(&mut ctx, host_surface), Action::Drop);
        assert_eq!(
            ctx.client_to_host_queue.len(),
            10,
            "editor state must be replayed only after host text-input enter"
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 4), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 4), 5);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 5), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 5), 7);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 6), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 6), 6);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 7), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 7), 6);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 8), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 8), 7);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 9), host_v1_id);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 9), 9);
        assert_eq!(
            msg_done_serial(&ctx.client_to_host_queue, 9),
            18,
            "placement replay must use a fresh host v1 serial"
        );
    }

    #[test]
    fn placement_refresh_recovers_when_guest_was_disabled() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let focused_surface = ctx.text_inputs[&guest_id]
            .active_surface
            .expect("fixture must start focused");
        let host_surface = focused_surface + 1;
        ctx.shadow_table.map_id(focused_surface, host_surface);
        ctx.shadow_table
            .track_interface(focused_surface, "wl_surface".to_string());
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.pending_enabled = false;
            state.committed_enabled = false;
            state.host_activation = HostActivationState::Inactive;
        }

        // The identity transition still invalidates the host-side generation
        // even though the guest had committed disable.  The marker must
        // survive until the next v3 enable/commit.
        let preflight = prepare_placement_ime_deactivation(&mut ctx, focused_surface)
            .expect("focused text input should be tracked during placement");
        assert_eq!(preflight.guest_text_inputs, vec![guest_id]);
        assert!(preflight.host_messages.is_empty());
        assert!(commit_placement_ime_preflight(&mut ctx, preflight));
        assert!(ctx.text_inputs[&guest_id].placement_ime_pending_for(focused_surface));
        assert!(!refresh_host_activation_for_surface(
            &mut ctx,
            focused_surface
        ));
        assert!(
            ctx.text_inputs[&guest_id].placement_ime_pending_for(focused_surface),
            "disabled guest must retain the marker for its next enable"
        );

        // Re-enable exactly as a real v3 client does.  The editor transaction
        // must reach Exo before the placement reset/deactivate/sync barrier,
        // otherwise host activation can race ahead of the new IME state.
        ctx.last_sender_id = guest_id;
        let mut v3 = TextInputV3Handler;
        assert_eq!(v3.on_enable(&mut ctx), Action::Drop);
        assert_eq!(v3.on_commit(&mut ctx), Action::Drop);
        let callback_id = ctx.text_inputs[&guest_id]
            .draining_callback()
            .expect("re-enable must install a placement refresh barrier");
        let commit_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == host_v1_id
                    && msg_opcode(std::slice::from_ref(message), 0)
                        == zwp_text_input_v1::REQ_COMMIT_STATE
            })
            .expect("v3 enable must send commit_state");
        let reset_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == host_v1_id
                    && msg_opcode(std::slice::from_ref(message), 0) == zwp_text_input_v1::REQ_RESET
            })
            .expect("placement recovery must reset the host generation");
        assert!(
            commit_index < reset_index,
            "editor state must precede the placement reset barrier"
        );

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert!(ctx.text_inputs[&guest_id].host_is_active());
        assert!(!ctx.text_inputs[&guest_id].placement_ime_pending_for(focused_surface));

        ctx.last_sender_id = host_v1_id;
        let mut v1 = TextInputV1Handler;
        assert_eq!(v1.on_enter(&mut ctx, host_surface), Action::Drop);
        assert!(
            ctx.client_to_host_queue
                .iter()
                .filter(|message| {
                    msg_sender(std::slice::from_ref(message), 0) == host_v1_id
                        && msg_opcode(std::slice::from_ref(message), 0)
                            == zwp_text_input_v1::REQ_COMMIT_STATE
                })
                .count()
                >= 2,
            "host enter must trigger a replay commit for Korean IME state"
        );
    }

    #[test]
    fn placement_refresh_is_cancelled_when_keyboard_focus_leaves_before_cleanup() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        let focused_surface = ctx.text_inputs[&guest_id]
            .active_surface
            .expect("fixture must start focused");

        let preflight = prepare_placement_ime_deactivation(&mut ctx, focused_surface)
            .expect("focused text input should be eligible for placement refresh");
        assert!(commit_placement_ime_preflight(&mut ctx, preflight));
        assert!(
            ctx.text_inputs[&guest_id].placement_ime_pending_for(focused_surface),
            "placement must own one pending recovery generation"
        );

        // A host keyboard leave can arrive before the Aura cleanup barrier.
        // The real focus transition must take ownership of the next guest
        // enter, so the placement marker is cancelled instead of becoming a
        // stale connection-wide re-entry request.
        apply_keyboard_focus_changes(
            &mut ctx,
            &[SeatFocusChange {
                guest_seat: 0,
                previous_surface: Some(focused_surface),
                current_surface: None,
            }],
        );
        assert_eq!(ctx.text_inputs[&guest_id].active_surface, None);
        assert!(
            !ctx.text_inputs[&guest_id].placement_ime_pending_for(focused_surface),
            "focus leave must cancel placement recovery"
        );

        let queue_len = ctx.host_to_client_queue.len();
        assert!(resume_placement_ime_for_surface(&mut ctx, focused_surface));
        assert_eq!(
            ctx.host_to_client_queue.len(),
            queue_len,
            "cleanup must not synthesize an enter for a surface that already lost focus"
        );
    }

    #[test]
    fn activation_barrier_orders_new_editor_transaction_before_activate() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let next_surface = 901;
        ctx.shadow_table.map_id(next_surface, 902);
        ctx.shadow_table
            .track_interface(next_surface, "wl_surface".to_string());

        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        update_host_activation(&mut ctx, guest_id);
        let callback_id = ctx.text_inputs[&guest_id]
            .draining_callback()
            .expect("deactivation must open a barrier");

        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.active_surface = Some(next_surface);
        }
        ctx.last_sender_id = guest_id;
        let mut v3 = TextInputV3Handler;
        assert_eq!(v3.on_enable(&mut ctx), Action::Drop);
        assert_eq!(
            v3.on_set_surrounding_text(&mut ctx, &"new".to_string(), 3, 3),
            Action::Drop
        );
        assert_eq!(v3.on_commit(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert!(
            ctx.client_to_host_queue.iter().all(|message| msg_sender(
                std::slice::from_ref(message),
                0
            ) != host_v1_id
                || msg_opcode(std::slice::from_ref(message), 0) != 0),
            "activation must remain deferred while the old generation drains"
        );
        let commit_state_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == host_v1_id
                    && msg_opcode(std::slice::from_ref(message), 0) == 9
            })
            .expect("the new editor transaction must include commit_state");

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        let activate_index = ctx.client_to_host_queue.len() - 1;
        assert_eq!(
            (
                msg_sender(&ctx.client_to_host_queue, activate_index),
                msg_opcode(&ctx.client_to_host_queue, activate_index)
            ),
            (host_v1_id, 0)
        );
        assert!(
            commit_state_index < activate_index,
            "the host must receive the new editor transaction before reactivation"
        );
    }

    #[test]
    fn activation_barrier_coalesces_changes_and_honors_final_disabled_target() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        for (guest_surface, host_surface) in [(901, 902), (903, 904)] {
            ctx.shadow_table.map_id(guest_surface, host_surface);
            ctx.shadow_table
                .track_interface(guest_surface, "wl_surface".to_string());
        }
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        update_host_activation(&mut ctx, guest_id);
        let callback_id = ctx.text_inputs[&guest_id].draining_callback().unwrap();

        for (enabled, surface) in [(true, Some(901)), (true, Some(903)), (false, Some(903))] {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = enabled;
            state.active_surface = surface;
            update_host_activation(&mut ctx, guest_id);
        }
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "one deactivation generation must own exactly one sync barrier"
        );

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert_eq!(
            ctx.client_to_host_queue.len(),
            2,
            "a final disabled target must not reactivate after callback.done"
        );
    }

    #[test]
    fn stale_activation_callback_cannot_reconcile_reused_guest_id() {
        let (mut ctx, _old_host_v1_id, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed_enabled = false;
            state.active_surface = None;
        }
        update_host_activation(&mut ctx, guest_id);
        let callback_id = ctx.text_inputs[&guest_id].draining_callback().unwrap();

        let mut replacement = ctx.text_inputs.remove(&guest_id).unwrap();
        replacement.host_v1_id = 11;
        replacement.committed_enabled = true;
        replacement.active_surface = Some(901);
        replacement.host_activation = HostActivationState::Inactive;
        ctx.shadow_table.map_id(901, 902);
        ctx.text_inputs.insert(guest_id, replacement);
        ctx.client_to_host_queue.clear();

        ctx.last_sender_id = callback_id.0;
        assert_eq!(CallbackHandler.on_done(&mut ctx, 0), Action::Drop);
        assert!(!ctx.text_inputs[&guest_id].host_is_active());
        assert!(
            ctx.client_to_host_queue.is_empty(),
            "an old host generation callback must not activate the replacement object"
        );
    }

    #[test]
    fn second_text_input_enable_on_same_seat_is_ignored() {
        let (mut ctx, _, first_id) = setup_v1_ctx();
        let surface = 40;
        ctx.shadow_table.map_id(surface, 41);
        {
            let state = ctx.text_inputs.get_mut(&first_id).unwrap();
            state.active_surface = Some(surface);
            state.pending_enabled = false;
            state.committed_enabled = false;
            state.host_activation = HostActivationState::Inactive;
        }
        let second_id = 21;
        ctx.shadow_table.map_id(second_id, 11);
        ctx.text_inputs.insert(
            second_id,
            crate::state::TextInputState {
                host_v1_id: 11,
                host_ext_id: None,
                guest_seat: 0,
                active_surface: Some(surface),
                pending_enabled: false,
                committed_enabled: false,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: String::new(),
                guest_commit_serial: 0,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                host_activation: HostActivationState::Inactive,
                placement_ime: crate::state::PlacementImeState::None,
            },
        );
        let mut handler = TextInputV3Handler;

        ctx.last_sender_id = first_id;
        handler.on_enable(&mut ctx);
        handler.on_commit(&mut ctx);
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), first_id);
        ctx.last_sender_id = second_id;
        handler.on_enable(&mut ctx);
        handler.on_commit(&mut ctx);

        assert!(ctx.text_inputs[&first_id].committed_enabled);
        assert!(ctx.text_inputs[&first_id].host_is_active());
        assert!(!ctx.text_inputs[&second_id].committed_enabled);
        assert!(!ctx.text_inputs[&second_id].host_is_active());
        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(41), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
            Some(first_id),
            "a rejected sibling enable must not cancel the active input's repeat lease"
        );
        assert!(!ctx.client_to_host_queue.iter().any(|message| {
            msg_sender(std::slice::from_ref(message), 0) == 11
                && msg_opcode(std::slice::from_ref(message), 0) == 0
        }));
    }

    #[test]
    fn uncommitted_surrounding_text_is_hidden_from_host_extension() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut v3_handler = TextInputV3Handler;
        v3_handler.on_set_surrounding_text(&mut ctx, &"가나다".to_string(), 6, 6);

        ctx.last_sender_id = 30;
        let mut ext_handler = ExtendedTextInputV1Handler;
        ext_handler.on_set_preedit_region(&mut ctx, -3, 3);
        assert!(ctx.host_to_client_queue.is_empty());

        ctx.last_sender_id = guest_id;
        v3_handler.on_commit(&mut ctx);
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activation = HostActivationState::Active;
        ctx.last_sender_id = 30;
        ext_handler.on_set_preedit_region(&mut ctx, -3, 3);
        assert_eq!(
            ctx.host_to_client_queue
                .iter()
                .map(|message| msg_opcode(std::slice::from_ref(message), 0))
                .collect::<Vec<_>>(),
            vec![4, 2, 5]
        );
    }

    #[test]
    fn disable_commit_invalidates_surrounding_text_and_deactivates() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.pending_surrounding_text = Some(("가".to_string(), 3, 3));
            state.committed_surrounding_text = Some(("가".to_string(), 3, 3));
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_disable(&mut ctx);

        assert!(ctx.text_inputs[&guest_id]
            .committed_surrounding_text
            .is_some());
        handler.on_commit(&mut ctx);

        let state = &ctx.text_inputs[&guest_id];
        assert!(!state.committed_enabled);
        assert!(state.committed_surrounding_text.is_none());
        assert!(!state.host_is_active());
        let clear_message = ctx.client_to_host_queue.iter().find(|message| {
            msg_sender(std::slice::from_ref(message), 0) == 10
                && msg_opcode(std::slice::from_ref(message), 0) == 5
        });
        let clear_message = clear_message.expect("disable must clear v1 surrounding text");
        let mut clear_wire = WireMessage::new(10, 5, &clear_message.0[8..], &clear_message.1);
        assert_eq!(clear_wire.read_string().unwrap(), "");
        assert_eq!(clear_wire.read_u32().unwrap(), 0);
        assert_eq!(clear_wire.read_u32().unwrap(), 0);
    }

    #[test]
    fn committed_enable_resets_previous_ime_event_transaction() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "이전".to_string();
            state.pending_preedit_cursor = Some(3);
            state.pending_preedit_selection = Some((0, 3));
            state.pending_deletes.push((3, 0));
            state.pending_cursor_position = Some((1, 1));
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_enable(&mut ctx);

        assert_eq!(ctx.text_inputs[&guest_id].current_preedit, "이전");
        handler.on_commit(&mut ctx);

        let state = &ctx.text_inputs[&guest_id];
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_preedit_cursor.is_none());
        assert!(state.pending_preedit_selection.is_none());
        assert!(state.pending_deletes.is_empty());
        assert!(state.pending_cursor_position.is_none());
    }

    #[test]
    fn keyboard_focus_transition_invalidates_editor_state_but_not_serial() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.guest_commit_serial = 17;
            state.pending_surrounding_text = Some(("pending".to_string(), 7, 7));
            state.committed_surrounding_text = Some(("current".to_string(), 7, 7));
            state.surrounding_text_dirty = true;
            state.content_hint = 0x200;
            state.content_type_dirty = true;
            state.cursor_rect = Some((1, 2, 3, 4));
            state.cursor_rect_dirty = true;
            state.current_preedit = "한".to_string();
            state.pending_deletes.push((3, 0));
        }

        let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
        let active_surface = state.active_surface;
        assert_eq!(state.apply_focus(active_surface), active_surface);

        assert_eq!(state.guest_commit_serial, 17);
        assert!(state.host_is_active());
        assert!(!state.pending_enabled);
        assert!(!state.committed_enabled);
        assert!(state.pending_surrounding_text.is_none());
        assert!(state.committed_surrounding_text.is_none());
        assert!(!state.surrounding_text_dirty);
        assert_eq!(state.content_hint, 0);
        assert!(!state.content_type_dirty);
        assert!(state.cursor_rect.is_none());
        assert!(!state.cursor_rect_dirty);
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_deletes.is_empty());
    }

    #[test]
    fn invalid_utf8_surrounding_offsets_do_not_replace_pending_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_set_surrounding_text(&mut ctx, &"가".to_string(), 1, 1);

        let state = &ctx.text_inputs[&guest_id];
        assert!(state.pending_surrounding_text.is_none());
        assert!(!state.surrounding_text_dirty);
    }

    #[test]
    fn content_type_mapping_preserves_pin_privacy_and_inline_composition() {
        assert_eq!(map_v3_content_type(0, 9), (0x80, 2, 5, 6, 1 << 10, 0));
        assert_eq!(map_v3_content_type(0x80, 8), (0x80, 8, 2, 0, 1 << 10, 0));
        assert_eq!(map_v3_content_type(0x200, 0).2, 14);
        assert_eq!(
            map_v3_content_type(0x2, 0).4,
            1 << 4,
            "v3 spellcheck must map to Chrome spellcheck_on"
        );

        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_set_content_type(&mut ctx, 0, 9);
        handler.on_commit(&mut ctx);

        let surrounding_support_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == 30
                    && msg_opcode(std::slice::from_ref(message), 0) == 7
            })
            .unwrap();
        let v1_content_type_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == 10
                    && msg_opcode(std::slice::from_ref(message), 0) == 6
            })
            .unwrap();
        let set_input_type_index = ctx
            .client_to_host_queue
            .iter()
            .position(|message| {
                msg_sender(std::slice::from_ref(message), 0) == 30
                    && msg_opcode(std::slice::from_ref(message), 0) == 6
            })
            .unwrap();
        assert!(surrounding_support_index < v1_content_type_index);
        assert!(v1_content_type_index < set_input_type_index);
        let mut set_input_type = wire_message(&ctx.client_to_host_queue, set_input_type_index);
        assert_eq!(set_input_type.read_u32().unwrap(), 5);
        assert_eq!(set_input_type.read_u32().unwrap(), 6);
        assert_eq!(set_input_type.read_u32().unwrap(), 1 << 10);
        assert_eq!(set_input_type.read_u32().unwrap(), 0);
        assert_eq!(set_input_type.read_u32().unwrap(), 1);
    }

    #[test]
    fn repeated_content_type_does_not_reset_active_host_composition() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        handler.on_set_content_type(&mut ctx, 0, 13);
        handler.on_commit(&mut ctx);
        ctx.client_to_host_queue.clear();

        // GTK resends the unchanged terminal content type after IME events.
        // Replaying set_content_type/set_input_type into Exo at that point
        // resets its active Korean preedit before the next key arrives.
        handler.on_set_content_type(&mut ctx, 0, 13);
        handler.on_commit(&mut ctx);

        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 9);
    }

    #[test]
    fn content_type_change_reverted_before_commit_is_a_host_noop() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        handler.on_set_content_type(&mut ctx, 0, 13);
        handler.on_commit(&mut ctx);
        ctx.client_to_host_queue.clear();

        handler.on_set_content_type(&mut ctx, 0, 9);
        handler.on_set_content_type(&mut ctx, 0, 13);
        handler.on_commit(&mut ctx);

        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 9);
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.committed_content_type, Some((0, 13)));
        assert!(!state.content_type_dirty);
    }

    #[test]
    fn extension_input_type_requests_follow_negotiated_version() {
        assert_eq!(
            extension_opcodes_for_version(Some(1)),
            Vec::<u16>::new(),
            "v1 has no extension input-type request"
        );
        assert_eq!(
            extension_opcodes_for_version(Some(7)),
            vec![1],
            "v2-v7 must use deprecated_set_input_type"
        );
        assert_eq!(
            extension_opcodes_for_version(Some(8)),
            vec![6],
            "v8 adds the five-argument set_input_type"
        );
        assert_eq!(
            extension_opcodes_for_version(Some(9)),
            vec![7, 6],
            "v9 adds surrounding-text support before set_input_type"
        );
        assert_eq!(
            extension_opcodes_for_version(None),
            vec![7, 6],
            "unversioned test fixtures remain permissive"
        );
    }

    #[test]
    fn child_extension_object_inherits_manager_version() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_text_input_manager_v1_id = Some(2);
        ctx.host_text_input_extension_v1_id = Some(3);
        ctx.shadow_table.track_host_interface_with_version(
            2,
            "zwp_text_input_manager_v1".to_string(),
            1,
        );
        ctx.shadow_table.track_host_interface_with_version(
            3,
            "zcr_text_input_extension_v1".to_string(),
            7,
        );
        ctx.last_sender_id = 40;

        let mut handler = TextInputManagerV3Handler;
        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 30), Action::Drop);
        let child_id = ctx.text_inputs[&20]
            .host_ext_id
            .expect("extended text input child");
        assert_eq!(
            ctx.shadow_table.host_object_version(child_id),
            Some(7),
            "version guards must use the manager's negotiated host version"
        );
    }

    #[test]
    fn inactive_host_ime_events_are_ignored() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activation = HostActivationState::Inactive;
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        handler.on_preedit_string(&mut ctx, 0, &"가".to_string(), &String::new());
        handler.on_delete_surrounding_text(&mut ctx, -3, 3);
        handler.on_commit_string(&mut ctx, 0, &"가".to_string());
        handler.on_keysym(&mut ctx, 0, 0, u32::from(b'a'), 1, 0);

        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.text_inputs[&guest_id].pending_deletes.is_empty());
    }

    #[test]
    fn guest_commit_serial_wraps_and_is_forwarded_unchanged() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.guest_commit_serial = u32::MAX;
            state.host_activation = HostActivationState::Inactive;
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_commit(&mut ctx);

        assert_eq!(ctx.text_inputs[&guest_id].guest_commit_serial, 0);
        let commit_state = ctx.client_to_host_queue.last().unwrap();
        assert_eq!(msg_done_serial(std::slice::from_ref(commit_state), 0), 0);
    }

    #[test]
    fn destroy_drops_v3_request_and_cleans_host_extension_routing() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.shadow_table
            .track_host_interface(30, "zcr_extended_text_input_v1".to_string());
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs.contains_key(&guest_id));
        assert!(ctx.shadow_table.get_host_id(guest_id).is_none());
        assert!(
            !ctx.shadow_table.is_host_id_available(10),
            "the host v1 backing ID must remain reserved after guest v3 destroy"
        );
        assert!(ctx.shadow_table.get_host_interface(30).is_none());
        assert!(
            !ctx.shadow_table.is_host_id_available(30),
            "the destroyed host extension ID must remain reserved until host delete_id"
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 1);
        assert_eq!(
            u32::from_ne_bytes(ctx.client_to_host_queue[0].0[8..12].try_into().unwrap()),
            2,
            "deactivate must carry the live host wl_seat ID"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 3);
        assert_eq!(
            msg_sender(&ctx.client_to_host_queue, 2),
            30,
            "host extension release follows the text-input generation barrier"
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 1), 1);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue, 1),
            wl_display::REQ_SYNC
        );
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 2), 0);
        assert_eq!(
            msg_sender(&ctx.host_to_client_queue, 0),
            1,
            "guest-local destruction must be acknowledged by wl_display"
        );
        assert_eq!(
            msg_opcode(&ctx.host_to_client_queue, 0),
            crate::protocols::wayland::wl_display::EVT_DELETE_ID
        );
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[8..12].try_into().unwrap()),
            guest_id
        );
    }

    #[test]
    fn destroying_inactive_sibling_preserves_active_repeat_lease() {
        let (mut ctx, _, active_id) = setup_v1_ctx();
        let sibling_id = 21;
        ctx.shadow_table.map_id(sibling_id, 11);
        ctx.text_inputs.insert(
            sibling_id,
            crate::state::TextInputState::new(11, None, 0, Some(900)),
        );
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), active_id);

        ctx.last_sender_id = sibling_id;
        assert_eq!(TextInputV3Handler.on_destroy(&mut ctx), Action::Drop);

        assert_eq!(
            ctx.key_generations
                .ime_repeat_owner(HostId(41), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
            Some(active_id)
        );
    }

    #[test]
    fn destroy_after_seat_release_does_not_deactivate_destroyed_host_seat() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let guest_seat = 7;
        let host_seat = 70;
        ctx.shadow_table.map_id(guest_seat, host_seat);
        ctx.shadow_table
            .track_interface_with_version(guest_seat, "wl_seat".to_string(), 5);
        ctx.shadow_table.set_host_version(host_seat, 5);
        ctx.text_inputs.get_mut(&guest_id).unwrap().guest_seat = guest_seat;
        ctx.shadow_table
            .track_host_interface(30, "zcr_extended_text_input_v1".to_string());
        ctx.last_sender_id = guest_seat;
        let mut release = WireMessage::new(
            guest_seat,
            crate::protocols::wayland::wl_seat::REQ_RELEASE,
            &[],
            &[],
        );
        assert!(crate::protocols::wayland::wl_seat::dispatch_request(
            &mut release,
            &mut SeatHandler,
            &mut ctx,
        )
        .expect("wl_seat.release should decode")
        .is_some());
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_seat));

        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs.contains_key(&guest_id));
        assert!(ctx.shadow_table.get_host_id(guest_id).is_none());
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|message| msg_sender(std::slice::from_ref(message), 0) != host_v1_id),
            "a child text-input must not reference its released parent host seat"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 0);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            msg_opcode(&ctx.host_to_client_queue, 0),
            crate::protocols::wayland::wl_display::EVT_DELETE_ID
        );
    }

    #[test]
    fn destroy_without_seat_mapping_completes_without_invalid_deactivate() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.text_inputs.get_mut(&guest_id).unwrap().guest_seat = 7;
        ctx.shadow_table
            .track_host_interface(30, "zcr_extended_text_input_v1".to_string());
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_destroy(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs.contains_key(&guest_id));
        assert!(ctx.shadow_table.get_host_id(guest_id).is_none());
        assert!(
            ctx.client_to_host_queue
                .iter()
                .all(|message| msg_sender(std::slice::from_ref(message), 0) != host_v1_id),
            "a missing parent seat must not be encoded as object ID zero"
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 0);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
    }

    #[test]
    fn text_input_creation_works_without_optional_chromeos_extension() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_text_input_manager_v1_id = Some(50);
        set_test_focus(&mut ctx, HostId(70), 7, 99);
        let mut handler = TextInputManagerV3Handler;

        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 7), Action::Drop);
        let state = &ctx.text_inputs[&20];
        assert!(state.host_ext_id.is_none());
        assert_eq!(state.guest_seat, 7);
        assert_eq!(state.active_surface, Some(99));
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 50);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 0);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), 20);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 0);
        assert_eq!(
            wire_message(&ctx.host_to_client_queue, 0)
                .read_u32()
                .unwrap(),
            99
        );

        ctx.last_sender_id = 20;
        let mut v3_handler = TextInputV3Handler;
        v3_handler.on_enable(&mut ctx);
        v3_handler.on_commit(&mut ctx);
        assert!(ctx
            .client_to_host_queue
            .iter()
            .all(|message| { matches!(msg_sender(std::slice::from_ref(message), 0), 50 | 2) }));
    }

    #[test]
    fn text_input_creation_without_host_manager_does_not_create_broken_state() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = TextInputManagerV3Handler;

        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 7), Action::Drop);
        assert!(!ctx.text_inputs.contains_key(&20));
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn on_keysym_translates_exo_timestamp_and_serial_to_wl_keyboard() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;

        // Register a guest wl_keyboard ID to capture the forwarded key
        let guest_keyboard_id = 999u32;
        ctx.shadow_table.map_id(guest_keyboard_id, 888);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);

        let mut handler = TextInputV1Handler;
        // 0xff08 is KEY_BackSpace
        let action = handler.on_keysym(&mut ctx, 123, 456, 0xff08, 1, 0);
        assert_eq!(action, Action::Drop);

        // Should produce 1 message on host_to_client_queue: wl_keyboard::key (opcode 3)
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(msg_opcode(&ctx.host_to_client_queue, 0), 3);
        assert_eq!(msg_sender(&ctx.host_to_client_queue, 0), guest_keyboard_id);

        let payload = &ctx.host_to_client_queue[0].0[8..];
        let serial = u32::from_ne_bytes(payload[0..4].try_into().unwrap());
        let time = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
        assert_eq!(serial, 456);
        assert_eq!(time, 123);
        assert!(
            !ctx.key_generations
                .physically_held(HostId(888), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
            "a text-input keysym must not invent physical-key state"
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(888), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
            Some(GuestKeyOwner::TextInputKeysym),
            "a synthetic keysym press must be paired with a later release"
        );
    }

    #[test]
    fn duplicate_keysym_press_does_not_duplicate_a_real_keyboard_press() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.key_generations.observe_physical_state(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            1,
        );
        assert!(ctx.claim_guest_key(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            GuestKeyOwner::Physical
        ));
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_BackSpace,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a keysym duplicate must not emit a second wl_keyboard.key press"
        );
        assert_eq!(
            ctx.guest_key_owner(
                HostId(host_keyboard_id),
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE
            ),
            Some(GuestKeyOwner::Physical),
            "a real keyboard press must not be mislabeled as synthetic"
        );
        assert!(
            ctx.key_generations.physically_held(
                HostId(host_keyboard_id),
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE
            ),
            "dropping the duplicate must preserve the real physical press"
        );
    }

    #[test]
    fn repeated_keysym_forwards_only_for_an_existing_guest_press() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        assert!(ctx.claim_guest_key(HostId(host_keyboard_id), keycode, GuestKeyOwner::Physical));
        ctx.key_generations
            .observe_physical_state(HostId(host_keyboard_id), keycode, 1);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_REPEATED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "a valid repeated keysym must reach the guest keyboard"
        );
        let payload = &ctx.host_to_client_queue[0].0[8..];
        assert_eq!(
            u32::from_ne_bytes(payload[8..12].try_into().unwrap()),
            keycode
        );
        assert_eq!(
            u32::from_ne_bytes(payload[12..16].try_into().unwrap()),
            crate::handler::keyboard::WL_KEY_REPEATED
        );
    }

    #[test]
    fn delayed_peek_and_keyboard_press_keep_keysym_pair_balanced() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        let mut text_input = TextInputV1Handler;
        let mut keyboard = KeyboardHandler::new();

        ctx.last_sender_id = host_v1_id;
        assert_eq!(
            text_input.on_keysym(
                &mut ctx,
                10,
                1,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );

        ctx.last_sender_id = host_extended_id;
        assert_eq!(
            keyboard.on_peek_key(
                &mut ctx,
                1,
                10,
                keycode,
                crate::handler::keyboard::WL_KEY_PRESSED,
            ),
            Action::Drop
        );
        ctx.last_sender_id = host_keyboard_id;
        assert_eq!(
            keyboard.on_key(
                &mut ctx,
                1,
                10,
                keycode,
                crate::handler::keyboard::WL_KEY_PRESSED,
            ),
            Action::Drop,
            "the delayed keyboard channel must not emit a duplicate press"
        );

        ctx.last_sender_id = host_v1_id;
        assert_eq!(
            text_input.on_keysym(
                &mut ctx,
                20,
                2,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_RELEASED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        let states = ctx
            .host_to_client_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[20..24].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_RELEASED,
            ]
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            Some(GuestKeyOwner::ImeRecovery)
        );
    }

    #[test]
    fn keysym_serial_distinguishes_delayed_duplicate_from_next_generation() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        for (serial, state) in [
            (1, crate::handler::keyboard::WL_KEY_PRESSED),
            (2, crate::handler::keyboard::WL_KEY_RELEASED),
        ] {
            handler.on_keysym(
                &mut ctx,
                serial * 10,
                serial,
                xkbcommon::xkb::keysyms::KEY_a,
                state,
                0,
            );
        }
        assert_eq!(ctx.host_to_client_queue.len(), 2);

        handler.on_keysym(
            &mut ctx,
            10,
            1,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "the completed generation's press serial must remain suppressed"
        );

        for (serial, state) in [
            (3, crate::handler::keyboard::WL_KEY_PRESSED),
            (4, crate::handler::keyboard::WL_KEY_RELEASED),
        ] {
            handler.on_keysym(
                &mut ctx,
                serial * 10,
                serial,
                xkbcommon::xkb::keysyms::KEY_a,
                state,
                0,
            );
        }
        assert_eq!(
            ctx.host_to_client_queue.len(),
            4,
            "a newer serial must open and close the next keysym generation"
        );

        handler.on_keysym(
            &mut ctx,
            10,
            1,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            4,
            "an older press must not reopen a generation after a newer pair completed"
        );
    }

    #[test]
    fn keysym_serial_wrap_opens_the_next_generation() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        ctx.shadow_table.map_id(guest_keyboard_id, 888);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        for (serial, state) in [
            (u32::MAX - 1, crate::handler::keyboard::WL_KEY_PRESSED),
            (u32::MAX, crate::handler::keyboard::WL_KEY_RELEASED),
            (0, crate::handler::keyboard::WL_KEY_PRESSED),
            (1, crate::handler::keyboard::WL_KEY_RELEASED),
        ] {
            handler.on_keysym(
                &mut ctx,
                10,
                serial,
                xkbcommon::xkb::keysyms::KEY_a,
                state,
                0,
            );
        }

        assert_eq!(
            ctx.host_to_client_queue.len(),
            4,
            "serial wrap must not suppress the next balanced keysym pair"
        );
    }

    #[test]
    fn released_tombstone_accepts_new_keysym_before_new_peek() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        let mut text_input = TextInputV1Handler;
        let mut keyboard = KeyboardHandler::new();

        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            0x8000_0010,
            1000,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1000,
            0x8000_0010,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1010,
            0x8000_0011,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            0x8000_0012,
            1020,
            keycode,
            crate::handler::keyboard::WL_KEY_RELEASED,
        );

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1100,
            0x8000_0020,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        assert_eq!(ctx.host_to_client_queue.len(), 3);
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            Some(GuestKeyOwner::TextInputKeysym)
        );

        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            0x8000_0020,
            1100,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            Some(GuestKeyOwner::TextInputKeysym),
            "the delayed physical channel must attach to the keysym-first generation"
        );
    }

    #[test]
    fn released_generation_drops_delayed_keysym_press_before_release_boundary() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        let mut text_input = TextInputV1Handler;
        let mut keyboard = KeyboardHandler::new();

        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            u32::MAX - 3,
            1000,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1000,
            u32::MAX - 2,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            0,
            1010,
            keycode,
            crate::handler::keyboard::WL_KEY_RELEASED,
        );

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1005,
            u32::MAX - 1,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "a delayed press before the wrapped release boundary must not reach the guest"
        );
        assert_eq!(
            ctx.key_generations
                .guest_press_serial(HostId(host_keyboard_id), keycode),
            Some(u32::MAX - 2)
        );

        text_input.on_keysym(
            &mut ctx,
            1100,
            1,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1010,
            u32::MAX,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1110,
            2,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );

        let states = ctx
            .host_to_client_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[20..24].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_RELEASED,
                crate::handler::keyboard::WL_KEY_RELEASED,
            ],
            "the old and current generations must each retain one balanced key pair"
        );
    }

    #[test]
    fn delayed_keysym_release_closes_retired_generation_only() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        let mut text_input = TextInputV1Handler;
        let mut keyboard = KeyboardHandler::new();

        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            10,
            100,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            100,
            10,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            11,
            110,
            keycode,
            crate::handler::keyboard::WL_KEY_RELEASED,
        );
        keyboard.on_peek_key(
            &mut ctx,
            20,
            200,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            110,
            11,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );
        assert!(
            ctx.key_generations
                .physically_held(HostId(host_keyboard_id), keycode),
            "the retired release must not close the current physical generation"
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            None,
            "the new generation must remain available after the retired release"
        );

        for (serial, state) in [
            (20, crate::handler::keyboard::WL_KEY_PRESSED),
            (21, crate::handler::keyboard::WL_KEY_RELEASED),
        ] {
            text_input.on_keysym(
                &mut ctx,
                serial * 10,
                serial,
                xkbcommon::xkb::keysyms::KEY_a,
                state,
                0,
            );
        }
        let states = ctx
            .host_to_client_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[20..24].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_RELEASED,
                crate::handler::keyboard::WL_KEY_PRESSED,
                crate::handler::keyboard::WL_KEY_RELEASED,
            ],
            "each physical generation must produce one balanced guest pair"
        );
    }

    #[test]
    fn current_keysym_release_does_not_consume_retired_release() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.keyboard_to_extended_keyboard
            .insert(HostId(host_keyboard_id), HostId(host_extended_id));
        ctx.extended_keyboard_to_keyboard
            .insert(HostId(host_extended_id), HostId(host_keyboard_id));
        let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
            .expect("fallback keymap must contain KEY_a");
        let mut text_input = TextInputV1Handler;
        let mut keyboard = KeyboardHandler::new();

        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            10,
            100,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            100,
            10,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            11,
            110,
            keycode,
            crate::handler::keyboard::WL_KEY_RELEASED,
        );
        keyboard.on_peek_key(
            &mut ctx,
            20,
            200,
            keycode,
            crate::handler::keyboard::WL_KEY_PRESSED,
        );

        ctx.last_sender_id = host_v1_id;
        for (serial, state) in [
            (20, crate::handler::keyboard::WL_KEY_PRESSED),
            (21, crate::handler::keyboard::WL_KEY_RELEASED),
        ] {
            text_input.on_keysym(
                &mut ctx,
                serial * 10,
                serial,
                xkbcommon::xkb::keysyms::KEY_a,
                state,
                0,
            );
        }
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            Some(GuestKeyOwner::ImeRecovery),
            "the current release must complete the current owner"
        );

        text_input.on_keysym(
            &mut ctx,
            110,
            11,
            xkbcommon::xkb::keysyms::KEY_a,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
            Some(GuestKeyOwner::ImeRecovery),
            "the delayed retired release must not disturb the current tombstone"
        );
        let states = ctx
            .host_to_client_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[20..24].try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(
            states
                .iter()
                .filter(|&&state| state == crate::handler::keyboard::WL_KEY_PRESSED)
                .count(),
            2
        );
        assert_eq!(
            states
                .iter()
                .filter(|&&state| state == crate::handler::keyboard::WL_KEY_RELEASED)
                .count(),
            2,
            "both guest presses must eventually receive one release"
        );
    }

    #[test]
    fn keysym_first_generation_retires_open_previous_owner() {
        for old_release_first in [true, false] {
            let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
            let guest_keyboard_id = 999u32;
            let host_keyboard_id = 888u32;
            let host_extended_id = 777u32;
            ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
            ctx.shadow_table
                .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
            ctx.keyboard_to_extended_keyboard
                .insert(HostId(host_keyboard_id), HostId(host_extended_id));
            ctx.extended_keyboard_to_keyboard
                .insert(HostId(host_extended_id), HostId(host_keyboard_id));
            let keycode = keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a)
                .expect("fallback keymap must contain KEY_a");
            let mut text_input = TextInputV1Handler;
            let mut keyboard = KeyboardHandler::new();

            ctx.last_sender_id = host_extended_id;
            keyboard.on_peek_key(
                &mut ctx,
                u32::MAX - 3,
                1000,
                keycode,
                crate::handler::keyboard::WL_KEY_PRESSED,
            );
            ctx.last_sender_id = host_v1_id;
            text_input.on_keysym(
                &mut ctx,
                1000,
                u32::MAX - 2,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            );
            ctx.last_sender_id = host_extended_id;
            keyboard.on_peek_key(
                &mut ctx,
                u32::MAX - 1,
                1010,
                keycode,
                crate::handler::keyboard::WL_KEY_RELEASED,
            );

            // The next keysym press is the first event of its generation. It
            // must retire the still-open previous keysym owner before the new
            // peek channel catches up.
            ctx.last_sender_id = host_v1_id;
            text_input.on_keysym(
                &mut ctx,
                1100,
                0,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            );
            assert_eq!(
                ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
                Some(GuestKeyOwner::TextInputKeysym)
            );
            ctx.last_sender_id = host_extended_id;
            keyboard.on_peek_key(
                &mut ctx,
                1,
                1100,
                keycode,
                crate::handler::keyboard::WL_KEY_PRESSED,
            );

            ctx.last_sender_id = host_v1_id;
            let releases = if old_release_first {
                [(1010, u32::MAX - 1), (1200, 2)]
            } else {
                [(1200, 2), (1010, u32::MAX - 1)]
            };
            for (timestamp, serial) in releases {
                text_input.on_keysym(
                    &mut ctx,
                    timestamp,
                    serial,
                    xkbcommon::xkb::keysyms::KEY_a,
                    crate::handler::keyboard::WL_KEY_RELEASED,
                    0,
                );
            }

            assert_eq!(
                ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
                Some(GuestKeyOwner::ImeRecovery),
                "the current owner must complete regardless of release order"
            );
            let states = ctx
                .host_to_client_queue
                .iter()
                .map(|(message, _)| u32::from_ne_bytes(message[20..24].try_into().unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(
                states
                    .iter()
                    .filter(|&&state| state == crate::handler::keyboard::WL_KEY_PRESSED)
                    .count(),
                2
            );
            assert_eq!(
                states
                    .iter()
                    .filter(|&&state| state == crate::handler::keyboard::WL_KEY_RELEASED)
                    .count(),
                2,
                "both keysym-first generations must remain balanced"
            );
        }
    }

    #[test]
    fn repeated_keysym_without_a_guest_press_is_dropped() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_a,
                crate::handler::keyboard::WL_KEY_REPEATED,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a repeated keysym must not invent a press for the guest"
        );
    }

    #[test]
    fn keysym_backspace_does_not_duplicate_an_ime_synthetic_pair() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.key_generations.observe_physical_state(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            1,
        );
        assert!(ctx.claim_guest_key(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            GuestKeyOwner::ImeRecovery
        ));
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_BackSpace,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a keysym Backspace must not duplicate a pair already synthesized for the IME"
        );
        assert!(
            ctx.guest_key_owner(
                HostId(host_keyboard_id),
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE
            ) == Some(GuestKeyOwner::ImeRecovery),
            "the physical release marker must remain until the real release arrives"
        );
    }

    #[test]
    fn keysym_non_backspace_prefers_keyboard_without_held_backspace() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let first_guest_keyboard = 100;
        let second_guest_keyboard = 200;
        let first_host_keyboard = 101;
        let second_host_keyboard = 201;
        for (guest_keyboard, host_keyboard) in [
            (first_guest_keyboard, first_host_keyboard),
            (second_guest_keyboard, second_host_keyboard),
        ] {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.shadow_table
                .track_interface(guest_keyboard, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard, 0);
        }
        ctx.key_generations.observe_physical_state(
            HostId(first_host_keyboard),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            1,
        );
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_A,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            msg_sender(&ctx.host_to_client_queue, 0),
            second_guest_keyboard,
            "an ordinary keysym must not be routed to the keyboard currently
             reserved for IME Backspace fallback"
        );
    }

    #[test]
    fn keysym_prefers_keyboard_with_current_surface_focus() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let stale_guest_keyboard = 100;
        let focused_guest_keyboard = 200;
        let stale_host_keyboard = 101;
        let focused_host_keyboard = 201;
        let current_surface = 300;

        for (guest_keyboard, host_keyboard) in [
            (stale_guest_keyboard, stale_host_keyboard),
            (focused_guest_keyboard, focused_host_keyboard),
        ] {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.shadow_table
                .track_interface(guest_keyboard, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard, 0);
        }
        set_test_focus(&mut ctx, HostId(focused_host_keyboard), 0, current_surface);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_A,
                crate::handler::keyboard::WL_KEY_PRESSED,
                0,
            ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            msg_sender(&ctx.host_to_client_queue, 0),
            focused_guest_keyboard,
            "a host keysym must target the keyboard whose surface owns the seat focus"
        );
    }

    #[test]
    fn keysym_release_without_synthetic_press_is_dropped() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        ctx.shadow_table.map_id(guest_keyboard_id, host_keyboard_id);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.key_generations.observe_physical_state(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            1,
        );
        assert!(ctx.claim_guest_key(
            HostId(host_keyboard_id),
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
            GuestKeyOwner::Physical
        ));
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                124,
                457,
                xkbcommon::xkb::keysyms::KEY_BackSpace,
                crate::handler::keyboard::WL_KEY_RELEASED,
                0,
            ),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a keysym release must not steal the release for a real press"
        );
        assert!(
            ctx.guest_key_owner(
                HostId(host_keyboard_id),
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE
            ) == Some(GuestKeyOwner::Physical),
            "the real press must remain paired with its physical release"
        );
        assert!(
            ctx.key_generations.physically_held(
                HostId(host_keyboard_id),
                crate::handler::keyboard::EVDEV_KEY_BACKSPACE
            ),
            "dropping the duplicate must preserve the physical state"
        );
    }

    #[test]
    fn on_keysym_release_closes_synthetic_keyboard_pair() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        ctx.shadow_table.map_id(guest_keyboard_id, 888);
        ctx.shadow_table
            .track_interface(guest_keyboard_id, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        handler.on_keysym(
            &mut ctx,
            123,
            456,
            xkbcommon::xkb::keysyms::KEY_BackSpace,
            crate::handler::keyboard::WL_KEY_PRESSED,
            0,
        );
        handler.on_keysym(
            &mut ctx,
            124,
            457,
            xkbcommon::xkb::keysyms::KEY_BackSpace,
            crate::handler::keyboard::WL_KEY_RELEASED,
            0,
        );

        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert!(
            !ctx.key_generations
                .physically_held(HostId(888), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
            "keysym delivery must remain independent of physical-key state"
        );
        assert!(
            ctx.guest_key_owner(HostId(888), crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
                == Some(GuestKeyOwner::ImeRecovery),
            "keysym release must retain a completed-generation tombstone"
        );
    }

    #[test]
    fn empty_preedit_confirmation_requires_a_held_backspace() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = host_v1_id;
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("text input")
            .current_preedit = "가".to_string();

        let mut v1_handler = TextInputV1Handler;
        assert_eq!(
            v1_handler.on_preedit_string(&mut ctx, 1, &String::new(), &String::new()),
            Action::Drop
        );
        assert!(!ime_repeat_active_for_seat(&ctx, 0));

        ctx.host_to_client_queue.clear();
        ctx.last_sender_id = 30;
        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 0),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "confirm_preedit must not synthesize Backspace without a held key"
        );
    }

    #[test]
    fn fallback_keysym_lookup_is_stable_and_reusable() {
        assert_eq!(
            keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_BackSpace),
            Some(crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
        );
        assert_eq!(
            keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_BackSpace),
            Some(crate::handler::keyboard::EVDEV_KEY_BACKSPACE)
        );
        assert_eq!(keysym_to_evdev_keycode(u32::MAX), None);
    }

    #[test]
    fn fallback_keysym_lookup_includes_shifted_symbols() {
        assert_eq!(
            keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_a),
            keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_A)
        );
        assert!(
            keysym_to_evdev_keycode(xkbcommon::xkb::keysyms::KEY_A).is_some(),
            "uppercase keysym events must map to the same physical key"
        );
    }

    #[test]
    fn on_keysym_selects_guest_keyboard_deterministically() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        for (guest_keyboard, host_keyboard) in [(999, 888), (100, 777)] {
            ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
            ctx.shadow_table
                .track_interface(guest_keyboard, "wl_keyboard".to_string());
            ctx.keyboard_to_seat.insert(guest_keyboard, 0);
        }
        ctx.last_sender_id = host_v1_id;

        assert_eq!(
            TextInputV1Handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_BackSpace,
                1,
                0,
            ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(
            msg_sender(&ctx.host_to_client_queue, 0),
            100,
            "keysym routing must not depend on HashMap iteration order"
        );
    }

    #[test]
    fn held_backspace_fallback_rejects_unmapped_guest_keyboard() {
        let (mut ctx, _, _) = setup_v1_ctx();
        // A stale route can survive a malformed/reordered bind, but without a
        // host keyboard there is no physical key or compositor time domain from
        // which a synthetic pair may safely be generated.
        ctx.keyboard_to_seat.insert(40, 0);
        ctx.last_sender_id = 30;

        let mut handler = ExtendedTextInputV1Handler;
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "synthetic Backspace must require a mapped host keyboard"
        );
    }

    #[test]
    fn non_empty_v1_commit_clears_stale_repeat_fallback() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
        state.current_preedit = "가".to_string();
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        handler.on_preedit_string(&mut ctx, 1, &String::new(), &"확정".to_string());

        assert!(!backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn test_activation_state_machine() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("text input fixture")
            .active_surface = None;
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activation = HostActivationState::Inactive;

        // Guest calls commit before focus (active_surface is None).
        ctx.last_sender_id = guest_id;
        let mut v3_handler = TextInputV3Handler;
        let action = v3_handler.on_commit(&mut ctx);
        assert_eq!(action, Action::Drop);

        // Should NOT send activate (opcode 0) to host because active_surface is None.
        let mut found_activate = false;
        for (msg, _) in &ctx.client_to_host_queue {
            let opcode = u32::from_ne_bytes(msg[4..8].try_into().unwrap()) & 0xffff;
            if opcode == 0 {
                found_activate = true;
            }
        }
        assert!(
            !found_activate,
            "Should not send activate when active_surface is None"
        );

        // 2. Keyboard enter is received from host. Set active_surface to a mock guest surface ID (1234).
        let guest_surface = 1234u32;
        let host_surface = 5678u32;
        ctx.shadow_table.map_id(guest_surface, host_surface);

        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.active_surface = Some(guest_surface);
        }
        update_host_activation(&mut ctx, guest_id);

        // Now it should have sent activate (opcode 0) with surface host_surface (5678).
        let mut found_activate = false;
        let mut activate_surface = 0u32;
        for (msg, _) in &ctx.client_to_host_queue {
            let opcode = u32::from_ne_bytes(msg[4..8].try_into().unwrap()) & 0xffff;
            if opcode == 0 {
                found_activate = true;
                let payload = &msg[8..];
                // activate(seat, surface) -> seat is first u32, surface is second u32 (offset 4)
                activate_surface = u32::from_ne_bytes(payload[4..8].try_into().unwrap());
            }
        }
        assert!(found_activate, "Should send activate when focused");
        assert_eq!(activate_surface, host_surface);

        // Clear the queue to check next transition.
        ctx.client_to_host_queue.clear();

        // 3. Keyboard leave is received (active_surface is None).
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.active_surface = None;
        }
        update_host_activation(&mut ctx, guest_id);

        // Now it should have sent deactivate (opcode 1).
        let mut found_deactivate = false;
        for (msg, _) in &ctx.client_to_host_queue {
            let opcode = u32::from_ne_bytes(msg[4..8].try_into().unwrap()) & 0xffff;
            if opcode == 1 {
                found_deactivate = true;
            }
        }
        assert!(
            found_deactivate,
            "Should send deactivate when focus is lost"
        );
    }

    #[test]
    fn v3_requests_after_leave_are_ignored_until_next_enter() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("text input fixture")
            .active_surface = None;
        let before = {
            let state = &ctx.text_inputs[&guest_id];
            (
                state.pending_enabled,
                state.enabled_dirty,
                state.pending_surrounding_text.clone(),
                state.content_hint,
                state.content_purpose,
                state.cursor_rect,
                state.text_change_cause,
                state.guest_commit_serial,
            )
        };
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_enable(&mut ctx), Action::Drop);
        assert_eq!(
            handler.on_set_surrounding_text(&mut ctx, &"stale".to_string(), 0, 0),
            Action::Drop
        );
        assert_eq!(handler.on_set_text_change_cause(&mut ctx, 7), Action::Drop);
        assert_eq!(handler.on_set_content_type(&mut ctx, 3, 4), Action::Drop);
        assert_eq!(
            handler.on_set_cursor_rectangle(&mut ctx, 1, 2, 3, 4),
            Action::Drop
        );
        assert_eq!(handler.on_disable(&mut ctx), Action::Drop);
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(
            ctx.client_to_host_queue.is_empty(),
            "requests after v3 leave must not reach the host IME"
        );
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(
            (
                state.pending_enabled,
                state.enabled_dirty,
                state.pending_surrounding_text.clone(),
                state.content_hint,
                state.content_purpose,
                state.cursor_rect,
                state.text_change_cause,
                state.guest_commit_serial,
            ),
            before,
            "inactive v3 requests must not mutate double-buffered state"
        );
    }
}
