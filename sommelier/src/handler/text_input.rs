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
use crate::protocols::wayland::wl_keyboard;
use crate::state::{Context, GuestId, GuestKeyOwner, HostId, SeatFocusChange};
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
        .filter(|state| state.host_activated)
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
    let Some(guest_keyboard_id) = ctx.shadow_table.guest_id_of(host_keyboard_id) else {
        return false;
    };
    let Some(&guest_seat) = ctx.keyboard_to_seat.get(&guest_keyboard_id.0) else {
        return false;
    };
    !ctx.key_generations.backspace_repeat_cancelled(
        host_keyboard_id,
        crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
    ) && ctx.key_generations.physically_held(
        host_keyboard_id,
        crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
    ) && ctx
        .text_inputs
        .values()
        .any(|state| state.guest_seat == guest_seat && state.empty_preedit_repeat_active)
}

pub(crate) fn backspace_pressed_for_seat(ctx: &Context, guest_seat: u32) -> bool {
    ctx.keyboard_to_seat
        .iter()
        .any(|(&guest_keyboard_id, &seat)| {
            seat == guest_seat
                && ctx
                    .shadow_table
                    .host_id_of(GuestId(guest_keyboard_id))
                    .is_some_and(|host_keyboard_id| {
                        !ctx.key_generations.backspace_repeat_cancelled(
                            host_keyboard_id,
                            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
                        ) && ctx.key_generations.physically_held(
                            host_keyboard_id,
                            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
                        )
                    })
        })
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
        .keyboard_latest_peek_sequences
        .get(&(guest_seat, active_surface))
        .copied();
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
    let owner = ctx.guest_key_owner(held.host_keyboard_id, held.key);
    if matches!(
        owner,
        Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
    ) {
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
    if owner.is_none() {
        let claimed =
            ctx.claim_guest_key(held.host_keyboard_id, held.key, GuestKeyOwner::ImeRecovery);
        debug_assert!(claimed, "guest owner was checked above");
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
        let guest_seat = state.guest_seat;
        if !state.host_activated {
            log::debug!(
                "Ignoring stale preedit_string for inactive text input {}",
                guest_id
            );
            return Action::Drop;
        }
        let pending_selection = state.pending_preedit_selection;
        let pending_cursor = state.pending_preedit_cursor;
        let had_preedit = !state.current_preedit.is_empty();
        let done_serial = state.guest_commit_serial;
        let backspace_held = backspace_pressed_for_seat(ctx, guest_seat);
        let (cursor_begin, cursor_end) =
            resolve_preedit_cursor(text, pending_selection, pending_cursor);
        if serial != done_serial {
            log::debug!(
                "Host preedit references guest serial {}, current serial is {}",
                serial,
                done_serial
            );
        }

        log::trace!(
            ">>> on_preedit_string: serial={}, text={:?}, commit={:?}, guest_id={}",
            serial,
            text,
            commit,
            guest_id
        );

        // The v1 `commit` argument is the replacement text to use if this
        // preedit is reset, not text to insert alongside every live preedit
        // update. Exo commonly sends identical non-empty `text` and `commit`
        // values while composing Korean; forwarding both would insert every
        // intermediate syllable. Apply the fallback only when the host
        // actually clears the preedit (notably on reset/unfocus).
        let mut transaction = Vec::new();
        if had_preedit && text.is_empty() && !commit.is_empty() {
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
            || !push_done_to(&mut transaction, guest_id, done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while encoding preedit");
        state.pending_preedit_selection = None;
        state.pending_preedit_cursor = None;
        if text.is_empty() {
            // A non-empty v1 `commit` replaces the old preedit during reset.
            // That is a commit operation, not an IME-consumed Backspace, so it
            // must never arm the synthetic repeat fallback.
            state.empty_preedit_repeat_active = had_preedit && commit.is_empty() && backspace_held;
        } else {
            state.empty_preedit_repeat_active = false;
        }
        state.current_preedit = text.clone();

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
        if !state.host_activated {
            log::debug!(
                "Ignoring stale commit_string for inactive text input {}",
                guest_id
            );
            return Action::Drop;
        }
        let had_preedit = !state.current_preedit.is_empty();
        let pending_deletes = state.pending_deletes.clone();
        let pending_cursor_position = state.pending_cursor_position;
        let done_serial = state.guest_commit_serial;
        if serial != done_serial {
            log::debug!(
                "Host commit references guest serial {}, current serial is {}",
                serial,
                done_serial
            );
        }

        log::trace!(
            ">>> on_commit_string: serial={}, text={:?}, guest_id={}",
            serial,
            text,
            guest_id
        );

        let mut transaction = Vec::new();
        if had_preedit {
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
        for (before, after) in pending_deletes {
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
            || !push_done_to(&mut transaction, guest_id, done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while encoding commit");
        state.pending_deletes.clear();
        state.pending_cursor_position = None;
        state.pending_preedit_cursor = None;
        state.pending_preedit_selection = None;
        state.current_preedit.clear();
        // A v1 commit event completes the host transaction even when the
        // committed string is empty. Do not carry a prior empty-preedit
        // confirmation into a later transaction and synthesize Backspace
        // without a currently held physical key.
        state.empty_preedit_repeat_active = false;

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
                match state {
                    crate::handler::keyboard::WL_KEY_PRESSED => {
                        let owner = ctx.guest_key_owner(host_keyboard_id, keycode);
                        // A text-input keysym event can describe the same
                        // physical key as a normal wl_keyboard.key event.
                        // Only synthesize a press when the guest has not
                        // already received one; otherwise the duplicate
                        // would make clients observe two presses for one key.
                        if owner == Some(GuestKeyOwner::Physical) {
                            log::debug!(
                                "Dropping duplicate keysym press for host keyboard {} key {}",
                                host_keyboard_id.0,
                                keycode
                            );
                            return Action::Drop;
                        }
                        if !ctx.claim_text_input_key(host_keyboard_id, keycode, wayland_serial) {
                            // A completed synthetic pair remains a tombstone
                            // while the physical generation is held. This
                            // suppresses a delayed duplicate keysym without
                            // blocking the next released generation.
                            log::debug!(
                                "Dropping keysym key {} already completed for host keyboard {}",
                                keycode,
                                host_keyboard_id.0,
                            );
                            return Action::Drop;
                        }
                        if keycode != crate::handler::keyboard::EVDEV_KEY_BACKSPACE {
                            if let Some(guest_seat) = guest_seat {
                                crate::handler::text_input::end_backspace_repeat_for_seat(
                                    ctx, guest_seat,
                                );
                            }
                        }
                    }
                    crate::handler::keyboard::WL_KEY_REPEATED => {
                        // A repeated keysym follows wl_keyboard.key's v10
                        // repeated state. It is a real event, but it must not
                        // invent a new press/release pair or mark a physical
                        // press as synthetic. Drop malformed repeats and
                        // repeats already consumed by the IME fallback.
                        let owner = ctx.guest_key_owner(host_keyboard_id, keycode);
                        let already_forwarded = matches!(
                            owner,
                            Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                        );
                        let ime_suppressed = owner == Some(GuestKeyOwner::ImeRecovery);
                        if !already_forwarded || ime_suppressed {
                            log::debug!(
                                "Dropping repeated keysym without a live guest press (key={}, ime_suppressed={})",
                                keycode,
                                ime_suppressed
                            );
                            return Action::Drop;
                        }
                        if keycode != crate::handler::keyboard::EVDEV_KEY_BACKSPACE {
                            if let Some(guest_seat) = guest_seat {
                                crate::handler::text_input::end_backspace_repeat_for_seat(
                                    ctx, guest_seat,
                                );
                            }
                        }
                    }
                    crate::handler::keyboard::WL_KEY_RELEASED => {
                        if ctx.key_generations.take_pending_text_input_release(
                            host_keyboard_id,
                            keycode,
                            wayland_serial,
                        ) {
                            // The release closes a press from a retired
                            // physical generation. Leave the current
                            // generation untouched.
                            log::debug!(
                                "Forwarding delayed keysym release for retired host keyboard {} key {}",
                                host_keyboard_id.0,
                                keycode
                            );
                        } else {
                            if let Some(press_serial) = ctx
                                .key_generations
                                .guest_press_serial(host_keyboard_id, keycode)
                            {
                                if !crate::state::serial_is_after(wayland_serial, press_serial) {
                                    log::debug!(
                                    "Dropping stale keysym release serial {} before press serial {}",
                                    wayland_serial,
                                    press_serial
                                );
                                    return Action::Drop;
                                }
                            }
                            // A keysym release is valid only for a press that this
                            // path synthesized. If the physical keyboard path
                            // owns the press, leave its marker and wait for the
                            // real wl_keyboard release.
                            let synthetic_press = ctx.complete_guest_key_if(
                                host_keyboard_id,
                                keycode,
                                GuestKeyOwner::TextInputKeysym,
                            );
                            if !synthetic_press {
                                log::debug!(
                                "Dropping keysym release without a synthetic press for host keyboard {} key {}",
                                host_keyboard_id.0,
                                keycode
                            );
                                return Action::Drop;
                            }
                            if keycode == crate::handler::keyboard::EVDEV_KEY_BACKSPACE {
                                if let Some(guest_seat) = guest_seat {
                                    crate::handler::text_input::end_backspace_repeat_for_seat(
                                        ctx, guest_seat,
                                    );
                                }
                            }
                        }
                    }
                    _ => unreachable!("keysym state validated above"),
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
        let _host_v1_id = ctx.last_sender_id;
        log::info!(">>> on_enter: surface={}", surface);
        Action::Drop
    }

    fn on_leave(&mut self, ctx: &mut Context) -> Action {
        let host_v1_id = ctx.last_sender_id;
        log::info!(">>> on_leave: host_v1_id={}", host_v1_id);
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
                    state.pending_preedit_selection = Some((index, length));
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
                state.pending_preedit_cursor = Some(index);
            }
        }
        Action::Drop
    }

    fn on_cursor_position(&mut self, ctx: &mut Context, index: i32, anchor: i32) -> Action {
        log::trace!(">>> on_cursor_position: index={}, anchor={}", index, anchor);
        let host_id = ctx.last_sender_id;
        if let Some(guest_id) = active_guest_for_host_text_input(ctx, host_id) {
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                state.pending_cursor_position = Some((index, anchor));
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
            state.pending_deletes.push(delete);
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
        if !state.host_activated {
            return Action::Drop;
        }

        let Some((text, cursor, _anchor)) = state.committed_surrounding_text.as_ref() else {
            log::warn!(
                "on_set_preedit_region: no surrounding text available for host_ext_id={}",
                host_ext_id
            );
            return Action::Drop;
        };

        let done_serial = state.guest_commit_serial;
        let cursor_i64 = *cursor as i64;
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
        let pending_selection = state.pending_preedit_selection;
        let pending_cursor = state.pending_preedit_cursor.or(Some(default_cursor));
        let (cursor_begin, cursor_end) =
            resolve_preedit_cursor(&preedit_text, pending_selection, pending_cursor);

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
            || !push_done_to(&mut transaction, guest_id, done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while installing preedit region");
        state.pending_preedit_selection = None;
        state.pending_preedit_cursor = None;
        state.current_preedit = preedit_text;
        // Installing a fresh preedit completes any prior empty-confirm
        // transaction. Do not let its held-Backspace fallback survive into
        // this new composition.
        state.empty_preedit_repeat_active = false;
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
        if !state.host_activated {
            return Action::Drop;
        }
        let guest_seat = state.guest_seat;
        let preedit_text = state.current_preedit.clone();
        let done_serial = state.guest_commit_serial;
        if preedit_text.is_empty() {
            if let Some(held) = held_repeat_key_for_seat(ctx, guest_seat) {
                let backspace_repeat = held.key == crate::handler::keyboard::EVDEV_KEY_BACKSPACE;
                ctx.text_inputs
                    .get_mut(&guest_id)
                    .expect("text input disappeared while confirming preedit")
                    .empty_preedit_repeat_active = backspace_repeat;
                if synthesize_ime_consumed_key_pair(ctx, held) {
                    log::debug!(
                        "Recovered empty IME confirmation as held key {} press/release",
                        held.key
                    );
                } else if backspace_repeat {
                    if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                        state.empty_preedit_repeat_active = false;
                    }
                }
                // GTK waits for the matching text-input transaction to finish
                // before applying a key that passed through the active IME.
                // A v1 confirm with no preedit has no text mutation, so its v3
                // equivalent is an empty done.
                push_done(ctx, guest_id, done_serial);
                return Action::Drop;
            }
            ctx.text_inputs
                .get_mut(&guest_id)
                .expect("text input disappeared while confirming preedit")
                .empty_preedit_repeat_active = false;
            log::debug!(
                "Ignoring confirm_preedit without an active preedit for guest {}",
                guest_id
            );
            return Action::Drop;
        }
        log::debug!(
            "  -> committing cached preedit={:?}, guest_id={}",
            preedit_text,
            guest_id
        );
        log::debug!(
            "  -> sending v3 preedit_string(\"\") + commit_string({:?}) + done({})",
            preedit_text,
            done_serial
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
        builder.write_string(&preedit_text);
        if !push_msg(&mut transaction, guest_id, 3, builder)
            || !push_done_to(&mut transaction, guest_id, done_serial)
        {
            return Action::Drop;
        }

        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("active text input disappeared while confirming preedit");
        state.current_preedit.clear();
        state.empty_preedit_repeat_active = false;
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
            crate::state::TextInputState {
                host_v1_id,
                host_ext_id,
                guest_seat: seat,
                active_surface,
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
                empty_preedit_repeat_active: false,
                host_activated: false,
            },
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

/// Tracks the host-side activation state of a text input v1 object, sending
/// `activate` / `deactivate` requests only when the state actually transitions.
/// Called when the guest's enabled state or focused surface changes.
pub(crate) fn update_host_activation(ctx: &mut Context, guest_id: u32) {
    let Some(state) = ctx.text_inputs.get(&guest_id) else {
        return;
    };
    let host_seat = ctx.shadow_table.get_host_id(state.guest_seat);
    let host_surface = state
        .active_surface
        .and_then(|surface| ctx.shadow_table.get_host_id(surface));
    let target_activated = state.committed_enabled && host_surface.is_some();

    if target_activated == state.host_activated {
        return;
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
            state.host_activated = false;
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
                state.host_activated = false;
            }
        }
        return;
    }

    let host_seat = host_seat.expect("checked above");
    let host_v1_id = state.host_v1_id;
    let host_surface = host_surface.unwrap_or(0);
    if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
        state.host_activated = target_activated;
    }

    if target_activated {
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
        push_msg(&mut ctx.client_to_host_queue, host_v1_id, 0, builder);
    } else {
        log::info!(
            "update_host_activation: deactivating text input v1 (guest_id={}, host_v1_id={})",
            guest_id,
            host_v1_id
        );
        // deactivate: opcode 1
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_seat);
        push_msg(&mut ctx.client_to_host_queue, host_v1_id, 1, builder);
    }
}

/// Invalidate all text-input-v3 state when keyboard focus leaves or enters.
///
/// The protocol requires clients to resend enable and editor state after a
/// new `enter`. Keep the commit counter and host activation marker intact:
/// the latter lets `update_host_activation` emit the required v1 deactivate
/// transition after this reset.
pub(crate) fn invalidate_for_keyboard_focus(state: &mut crate::state::TextInputState) {
    state.pending_enabled = false;
    state.committed_enabled = false;
    state.enabled_dirty = false;
    state.pending_surrounding_text = None;
    state.committed_surrounding_text = None;
    state.surrounding_text_dirty = false;
    state.content_hint = 0;
    state.content_purpose = 0;
    state.committed_content_type = None;
    state.content_type_dirty = false;
    state.cursor_rect = None;
    state.cursor_rect_dirty = false;
    state.text_change_cause = 0;
    state.current_preedit.clear();
    state.pending_preedit_cursor = None;
    state.pending_preedit_selection = None;
    state.pending_deletes.clear();
    state.pending_cursor_position = None;
    state.empty_preedit_repeat_active = false;
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

        ctx.keyboard_latest_peek_sequences
            .retain(|(guest_seat, _), _| *guest_seat != change.guest_seat);
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

        let previous_surface = state.active_surface;
        invalidate_for_keyboard_focus(state);
        state.active_surface = current_surface;

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
        ctx.key_generations.cancel_backspace_repeat(
            host_keyboard_id,
            crate::handler::keyboard::EVDEV_KEY_BACKSPACE,
        );
    }
    for state in ctx.text_inputs.values_mut() {
        if state.guest_seat == guest_seat {
            state.empty_preedit_repeat_active = false;
        }
    }
}

pub struct TextInputV3Handler;
impl zwp_text_input_v3::ZwpTextInputV3Handler for TextInputV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        // Destruction is the final disabled transition. Route it through the
        // same activation reconciler as commit/focus changes so parent-seat
        // lifetime validation cannot drift between teardown paths.
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.committed_enabled = false;
            state.active_surface = None;
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
            // v3 enable resets all client state. Later requests in the same
            // transaction repopulate these pending values before commit.
            state.pending_enabled = true;
            state.enabled_dirty = true;
            state.pending_surrounding_text = None;
            state.surrounding_text_dirty = true;
            state.content_hint = 0;
            state.content_purpose = 0;
            state.committed_content_type = None;
            state.content_type_dirty = true;
            state.cursor_rect = None;
            state.cursor_rect_dirty = true;
            state.text_change_cause = 0;
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
            state.pending_enabled = false;
            state.enabled_dirty = true;
            state.pending_surrounding_text = None;
            state.surrounding_text_dirty = true;
            state.content_hint = 0;
            state.content_purpose = 0;
            state.committed_content_type = None;
            state.content_type_dirty = true;
            state.cursor_rect = None;
            state.cursor_rect_dirty = true;
            state.text_change_cause = 0;
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
            state.pending_surrounding_text = Some((text.clone(), cursor, anchor));
            state.surrounding_text_dirty = true;
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
            state.text_change_cause = cause;
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
            state.content_hint = hint;
            state.content_purpose = purpose;
            state.content_type_dirty = state.committed_content_type != Some((hint, purpose));
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
            state.cursor_rect = Some((x, y, width, height));
            state.cursor_rect_dirty = true;
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
        let Some(state) = ctx.text_inputs.get(&guest_id) else {
            return Action::Drop;
        };
        let reset_state = state.enabled_dirty;
        let committed_enabled = if reset_state && !enable_conflict {
            state.pending_enabled
        } else {
            state.committed_enabled
        };
        let commit_serial = state.guest_commit_serial.wrapping_add(1);
        let host_v1_id = state.host_v1_id;
        let host_ext_id = state.host_ext_id;
        let surrounding_text_dirty = state.surrounding_text_dirty;
        let committed_surrounding_text = if surrounding_text_dirty {
            state.pending_surrounding_text.clone()
        } else {
            state.committed_surrounding_text.clone()
        };
        let content_type_dirty = state.content_type_dirty;
        let content_type = (state.content_hint, state.content_purpose);
        let cursor_rect_dirty = state.cursor_rect_dirty;
        let cursor_rect = state.cursor_rect.unwrap_or((0, 0, 0, 0));
        log::trace!(
            ">>> v3 on_commit: guest_id={}, serial={}, enabled={}, host_v1_id={}",
            guest_id,
            commit_serial,
            committed_enabled,
            host_v1_id
        );

        // Construct the complete v1 transaction before publishing any part of
        // it or advancing the double-buffered state. This prevents an
        // unencodable variable-size field from leaving Exo and the bridge on
        // different committed editor states.
        let mut transaction = Vec::new();
        if surrounding_text_dirty {
            if let Some((text, cursor, anchor)) = &committed_surrounding_text {
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
                if !push_msg(&mut transaction, host_v1_id, 5, builder) {
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
                if !push_msg(&mut transaction, host_v1_id, 5, builder) {
                    return Action::Drop;
                }
            }
        }

        if content_type_dirty {
            let (hint, purpose) = content_type;
            let (v1_hint, v1_purpose, input_type, input_mode, input_flags, learning_mode) =
                map_v3_content_type(hint, purpose);

            if let Some(host_ext_id) = host_ext_id {
                let host_ext_version = ctx
                    .shadow_table
                    .host_object_version(host_ext_id)
                    .unwrap_or(u32::MAX);
                if extension_version_allows(host_ext_version, 9) {
                    // This request applies to the following content-type
                    // request, so keep both in the same transaction.
                    let mut builder = MessageBuilder::new();
                    builder.write_u32(u32::from(committed_surrounding_text.is_some()));
                    if !push_msg(&mut transaction, host_ext_id, 7, builder) {
                        return Action::Drop;
                    }
                }
            }

            // set_content_type: opcode 6 (on zwp_text_input_v1)
            let mut builder = MessageBuilder::new();
            builder.write_u32(v1_hint);
            builder.write_u32(v1_purpose);
            if !push_msg(&mut transaction, host_v1_id, 6, builder) {
                return Action::Drop;
            }

            if let Some(host_ext_id) = host_ext_id {
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

        if cursor_rect_dirty {
            let (x, y, w, h) = cursor_rect;
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
            if !push_msg(&mut transaction, host_v1_id, 7, builder) {
                return Action::Drop;
            }
        }

        log::debug!("  -> sending v1 commit_state(serial={})", commit_serial);
        // commit_state: opcode 9
        let mut builder = MessageBuilder::new();
        builder.write_u32(commit_serial);
        if !push_msg(&mut transaction, host_v1_id, 9, builder) {
            return Action::Drop;
        }

        if enable_conflict {
            log::warn!(
                "Ignoring text-input enable for guest {}: another input is enabled on seat {}",
                guest_id,
                state.guest_seat
            );
        }
        let state = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("focused text input disappeared while committing state");
        state.guest_commit_serial = commit_serial;
        if reset_state {
            if enable_conflict {
                state.pending_enabled = state.committed_enabled;
            } else {
                state.committed_enabled = state.pending_enabled;
            }
            state.enabled_dirty = false;
            state.current_preedit.clear();
            state.pending_preedit_cursor = None;
            state.pending_preedit_selection = None;
            state.pending_deletes.clear();
            state.pending_cursor_position = None;
            state.empty_preedit_repeat_active = false;
        }
        if surrounding_text_dirty {
            state.surrounding_text_dirty = false;
            state.committed_surrounding_text = committed_surrounding_text;
        }
        if content_type_dirty {
            state.content_type_dirty = false;
            state.committed_content_type = Some(content_type);
        }
        if cursor_rect_dirty {
            state.cursor_rect_dirty = false;
        }
        state.text_change_cause = 0;

        update_host_activation(ctx, guest_id);
        ctx.client_to_host_queue.extend(transaction);
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::keyboard::KeyboardHandler;
    use crate::handler::seat::SeatHandler;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;
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
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.client_to_host_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
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
                empty_preedit_repeat_active: false,
                host_activated: true,
            },
        );
        (ctx, host_v1_id, guest_id)
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
        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "committing a reset preedit must not trigger Backspace repeat"
        );
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
            state.empty_preedit_repeat_active = true;
        }

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
        assert!(state.empty_preedit_repeat_active);
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
            state.empty_preedit_repeat_active = true;
        }

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
        assert!(state.empty_preedit_repeat_active);
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
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("text input")
            .empty_preedit_repeat_active = true;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_commit_string(&mut ctx, 1, &String::new()),
            Action::Drop
        );
        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "an empty commit must finish the old transaction"
        );
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
            state.empty_preedit_repeat_active = true;
        }

        let mut handler = ExtendedTextInputV1Handler;
        assert_eq!(handler.on_set_preedit_region(&mut ctx, -3, 3), Action::Drop);
        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "a newly installed preedit must not inherit an old empty-confirm repeat"
        );
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
            state.empty_preedit_repeat_active = true;
        }

        assert_eq!(
            ExtendedTextInputV1Handler.on_set_preedit_region(&mut ctx, -65_528, 65_528),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.pending_preedit_cursor, Some(7));
        assert_eq!(state.pending_preedit_selection, Some((1, 2)));
        assert_eq!(state.current_preedit, "old");
        assert!(state.empty_preedit_repeat_active);
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

        // Cached preedit should be cleared
        if let Some(state) = ctx.text_inputs.get(&guest_id) {
            assert_eq!(state.current_preedit, "");
        } else {
            panic!("state not found");
        }
    }

    #[test]
    fn oversized_confirm_preedit_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "x".repeat(65_528);
            state.empty_preedit_repeat_active = true;
        }

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 0),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.current_preedit.len(), 65_528);
        assert!(state.empty_preedit_repeat_active);
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

        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activated = true;
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
            state.empty_preedit_repeat_active = true;
        }
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
        assert!(ctx.text_inputs[&guest_id].empty_preedit_repeat_active);
        let mut key_serials = Vec::new();
        for messages in ctx.host_to_client_queue.chunks_exact(3) {
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
        assert!(ctx.text_inputs[&guest_id].empty_preedit_repeat_active);

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
        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "the duplicate fallback must be disarmed when the physical press owns the edit"
        );
    }

    #[test]
    fn held_backspace_fallback_requires_guest_keyboard_for_seat() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .empty_preedit_repeat_active = true;
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
            state.host_activated = false;
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;

        assert_eq!(handler.on_enable(&mut ctx), Action::Drop);
        assert!(!ctx.text_inputs[&guest_id].host_activated);
        assert!(ctx.client_to_host_queue.is_empty());

        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);
        assert!(ctx.text_inputs[&guest_id].host_activated);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 0);
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
            state.host_activated = false;
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
                empty_preedit_repeat_active: false,
                host_activated: false,
            },
        );
        let mut handler = TextInputV3Handler;

        ctx.last_sender_id = first_id;
        handler.on_enable(&mut ctx);
        handler.on_commit(&mut ctx);
        ctx.last_sender_id = second_id;
        handler.on_enable(&mut ctx);
        handler.on_commit(&mut ctx);

        assert!(ctx.text_inputs[&first_id].committed_enabled);
        assert!(ctx.text_inputs[&first_id].host_activated);
        assert!(!ctx.text_inputs[&second_id].committed_enabled);
        assert!(!ctx.text_inputs[&second_id].host_activated);
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
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activated = true;
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
        assert!(!state.host_activated);
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
        invalidate_for_keyboard_focus(state);

        assert_eq!(state.guest_commit_serial, 17);
        assert!(state.host_activated);
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
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activated = false;
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
            state.host_activated = false;
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
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 1), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 1), 0);
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
        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "an empty preedit without a physical Backspace must not arm fallback"
        );

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
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        // A stale route can survive a malformed/reordered bind, but without a
        // host keyboard there is no physical key or compositor time domain from
        // which a synthetic pair may safely be generated.
        ctx.keyboard_to_seat.insert(40, 0);
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .empty_preedit_repeat_active = true;
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
        state.empty_preedit_repeat_active = true;
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        handler.on_preedit_string(&mut ctx, 1, &String::new(), &"확정".to_string());

        assert!(
            !ctx.text_inputs[&guest_id].empty_preedit_repeat_active,
            "a non-empty commit must not leave the synthetic repeat armed"
        );
    }

    #[test]
    fn test_activation_state_machine() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
        ctx.text_inputs
            .get_mut(&guest_id)
            .expect("text input fixture")
            .active_surface = None;
        ctx.text_inputs.get_mut(&guest_id).unwrap().host_activated = false;

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
