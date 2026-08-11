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
use crate::state::Context;
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
) {
    queue.push((builder.build_message(sender_id, opcode), Vec::new()));
}

fn push_done(ctx: &mut Context, guest_id: u32, serial: u32) {
    let mut builder = MessageBuilder::new();
    builder.write_u32(serial);
    push_msg(&mut ctx.host_to_client_queue, guest_id, 5, builder);
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

fn synthesize_backspace_key_pair(ctx: &mut Context, guest_seat: u32) -> bool {
    let Some(guest_keyboard_id) = ctx
        .keyboard_to_seat
        .iter()
        .find_map(|(&keyboard, &seat)| (seat == guest_seat).then_some(keyboard))
    else {
        log::warn!(
            "Cannot synthesize held Backspace: no guest keyboard for seat {}",
            guest_seat
        );
        return false;
    };
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    for state in [1, 0] {
        ctx.synthetic_keyboard_serial = ctx.synthetic_keyboard_serial.wrapping_add(1).max(1);
        let mut builder = MessageBuilder::new();
        builder.write_u32(ctx.synthetic_keyboard_serial);
        builder.write_u32(time);
        builder.write_u32(crate::handler::keyboard::EVDEV_KEY_BACKSPACE);
        builder.write_u32(state);
        push_msg(
            &mut ctx.host_to_client_queue,
            guest_keyboard_id,
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
        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
            return Action::Drop;
        };
        if !state.host_activated {
            log::debug!(
                "Ignoring stale preedit_string for inactive text input {}",
                guest_id
            );
            return Action::Drop;
        }

        let pending_selection = state.pending_preedit_selection.take();
        let pending_cursor = state.pending_preedit_cursor.take();
        let (cursor_begin, cursor_end) =
            resolve_preedit_cursor(text, pending_selection, pending_cursor);
        let had_preedit = !state.current_preedit.is_empty();
        if text.is_empty() {
            state.empty_preedit_repeat_active |= had_preedit;
        } else {
            state.empty_preedit_repeat_active = false;
        }
        state.current_preedit = text.clone();
        let done_serial = state.guest_commit_serial;
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

        // v3 preedit_string (opcode 2).
        let mut builder = MessageBuilder::new();
        builder.write_string(text);
        builder.write_i32(cursor_begin);
        builder.write_i32(cursor_end);
        push_msg(&mut ctx.host_to_client_queue, guest_id, 2, builder);

        // Exo allocates v1 event serials independently. text-input-v3 instead
        // requires the number of the latest guest commit request.
        push_done(ctx, guest_id, done_serial);
        Action::Drop
    }

    fn on_commit_string(&mut self, ctx: &mut Context, serial: u32, text: &String) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
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
        let pending_deletes = std::mem::take(&mut state.pending_deletes);
        let pending_cursor_position = state.pending_cursor_position.take();
        state.pending_preedit_cursor = None;
        state.pending_preedit_selection = None;
        state.current_preedit.clear();
        if !text.is_empty() {
            state.empty_preedit_repeat_active = false;
        }
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

        if had_preedit {
            let mut builder = MessageBuilder::new();
            builder.write_string("");
            builder.write_i32(0);
            builder.write_i32(0);
            push_msg(&mut ctx.host_to_client_queue, guest_id, 2, builder);
        }

        // v1 requires delete_surrounding_text and cursor_position to be
        // applied as part of the following commit_string. Preserve that
        // transaction boundary when translating to v3.
        for (before, after) in pending_deletes {
            let mut builder = MessageBuilder::new();
            builder.write_u32(before);
            builder.write_u32(after);
            push_msg(&mut ctx.host_to_client_queue, guest_id, 4, builder);
        }

        let mut builder = MessageBuilder::new();
        builder.write_string(text);
        push_msg(&mut ctx.host_to_client_queue, guest_id, 3, builder);

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

        push_done(ctx, guest_id, done_serial);
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
            ">>> on_keysym: host_id={}, guest_id={}, serial={}, sym=0x{:x} ({:?}), state={}",
            host_id,
            guest_id,
            serial,
            sym,
            sym_char,
            state
        );

        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let found_keycode = xkbcommon::xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .and_then(|keymap| {
            for keycode_raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                let keycode = keycode_raw.into();
                let syms = keymap.key_get_syms_by_level(keycode, 0, 0);
                if syms.iter().any(|s| s.raw() == sym) {
                    return Some(keycode_raw - 8);
                }
            }
            None
        });

        if let Some(keycode) = found_keycode {
            let guest_seat = ctx.text_inputs.get(&guest_id).map(|state| state.guest_seat);
            let keyboard_id = guest_seat.and_then(|seat| {
                ctx.keyboard_to_seat
                    .iter()
                    .find_map(|(&keyboard, &keyboard_seat)| {
                        (keyboard_seat == seat).then_some(keyboard)
                    })
            });
            if let Some(keyboard_id) = keyboard_id {
                log::debug!(
                    "  -> forwarding wl_keyboard.key: keyboard_id={}, serial={}, time={}, keycode={}, state={}",
                    keyboard_id, serial, time, keycode, state
                );
                // Send wl_keyboard::key (opcode 3).
                let mut builder = MessageBuilder::new();
                builder.write_u32(serial); // serial
                builder.write_u32(time); // time
                builder.write_u32(keycode); // key
                builder.write_u32(state); // state (0: released, 1: pressed)
                push_msg(&mut ctx.host_to_client_queue, keyboard_id, 3, builder);
            } else {
                log::warn!("  -> no wl_keyboard found to forward keysym to");
            }
        } else {
            log::warn!(
                "  -> could not find keycode for sym=0x{:x} ({:?})",
                sym,
                sym_char
            );
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
            .iter_mut()
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
        let pending_selection = state.pending_preedit_selection.take();
        let pending_cursor = state.pending_preedit_cursor.take().or(Some(default_cursor));
        let (cursor_begin, cursor_end) =
            resolve_preedit_cursor(&preedit_text, pending_selection, pending_cursor);
        state.current_preedit = preedit_text.clone();

        let mut builder = MessageBuilder::new();
        builder.write_u32(before_length);
        builder.write_u32(after_length);
        push_msg(&mut ctx.host_to_client_queue, guest_id, 4, builder);

        let mut builder = MessageBuilder::new();
        builder.write_string(&preedit_text);
        builder.write_i32(cursor_begin);
        builder.write_i32(cursor_end);
        push_msg(&mut ctx.host_to_client_queue, guest_id, 2, builder);

        push_done(ctx, guest_id, done_serial);

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
        let backspace_pressed = ctx
            .peek_pressed_keys
            .contains(&crate::handler::keyboard::EVDEV_KEY_BACKSPACE);
        log::trace!(
            ">>> on_confirm_preedit: host_ext_id={}, selection_behavior={}",
            host_ext_id,
            _selection_behavior
        );
        let Some((&guest_id, state)) = ctx
            .text_inputs
            .iter_mut()
            .find(|(_, s)| s.host_ext_id == Some(host_ext_id))
        else {
            return Action::Drop;
        };
        if !state.host_activated {
            return Action::Drop;
        }
        let preedit_text = state.current_preedit.clone();
        if preedit_text.is_empty() {
            if backspace_pressed || state.empty_preedit_repeat_active {
                let guest_seat = state.guest_seat;
                state.empty_preedit_repeat_active = true;
                if synthesize_backspace_key_pair(ctx, guest_seat) {
                    log::debug!(
                        "Translating empty confirm_preedit during held Backspace to synthetic key pair"
                    );
                } else if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                    state.empty_preedit_repeat_active = false;
                }
                return Action::Drop;
            }
            state.empty_preedit_repeat_active = false;
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
        let done_serial = state.guest_commit_serial;
        state.current_preedit.clear();
        state.empty_preedit_repeat_active = false;

        log::debug!(
            "  -> sending v3 preedit_string(\"\") + commit_string({:?}) + done({})",
            preedit_text,
            done_serial
        );
        let mut builder = MessageBuilder::new();
        builder.write_string("");
        builder.write_i32(0); // cursor_begin
        builder.write_i32(0); // cursor_end
        push_msg(&mut ctx.host_to_client_queue, guest_id, 2, builder);

        let mut builder = MessageBuilder::new();
        builder.write_string(&preedit_text);
        push_msg(&mut ctx.host_to_client_queue, guest_id, 3, builder);

        push_done(ctx, guest_id, done_serial);
        Action::Drop
    }
}

pub struct TextInputManagerV3Handler;
impl zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler for TextInputManagerV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        ctx.shadow_table.remove_id(ctx.last_sender_id);
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
            .track_interface(id, "zwp_text_input_v3".to_string());
        ctx.shadow_table
            .track_host_interface(host_v1_id, "zwp_text_input_v1".to_string());
        if let Some(host_ext_id) = host_ext_id {
            ctx.shadow_table
                .track_host_interface(host_ext_id, "zcr_extended_text_input_v1".to_string());
        }

        let active_surface = ctx.active_surface_for_seat.get(&seat).copied();

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
    if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
        let host_seat = ctx.shadow_table.get_host_id(state.guest_seat).unwrap_or(0);
        let host_surface = state
            .active_surface
            .and_then(|s| ctx.shadow_table.get_host_id(s))
            .unwrap_or(0);
        let target_activated = state.committed_enabled && host_surface != 0;

        if target_activated == state.host_activated {
            return;
        }
        state.host_activated = target_activated;

        if target_activated {
            log::info!(
                "update_host_activation: activating text input v1 (guest_id={}, host_v1_id={}, host_surface={})",
                guest_id,
                state.host_v1_id,
                host_surface,
            );
            // activate: opcode 0
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_seat);
            builder.write_u32(host_surface);
            push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 0, builder);
        } else {
            log::info!(
                "update_host_activation: deactivating text input v1 (guest_id={}, host_v1_id={})",
                guest_id,
                state.host_v1_id
            );
            // deactivate: opcode 1
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_seat);
            push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 1, builder);
        }
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

pub(crate) fn end_backspace_repeat(ctx: &mut Context) {
    for state in ctx.text_inputs.values_mut() {
        state.empty_preedit_repeat_active = false;
    }
}

pub struct TextInputV3Handler;
impl zwp_text_input_v3::ZwpTextInputV3Handler for TextInputV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        let Some(state) = ctx.text_inputs.remove(&guest_id) else {
            ctx.shadow_table.remove_id(guest_id);
            return Action::Drop;
        };

        if state.host_activated {
            let host_seat = ctx.shadow_table.get_host_id(state.guest_seat).unwrap_or(0);
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_seat);
            push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 1, builder);
        }

        // zwp_text_input_v1 has no destructor, but its ChromeOS extension does.
        // Stop extension events and remove all local routing state.
        if let Some(host_ext_id) = state.host_ext_id {
            push_msg(
                &mut ctx.client_to_host_queue,
                host_ext_id,
                0,
                MessageBuilder::new(),
            );
            ctx.shadow_table.remove_host_interface(host_ext_id);
        }
        ctx.shadow_table.remove_id(guest_id);
        Action::Drop
    }

    fn on_enable(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
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
            state.content_type_dirty = true;
            state.cursor_rect = None;
            state.cursor_rect_dirty = true;
            state.text_change_cause = 0;
        }
        Action::Drop
    }

    fn on_disable(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        log::info!(">>> v3 on_disable: guest_id={}", guest_id);
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.pending_enabled = false;
            state.enabled_dirty = true;
            state.pending_surrounding_text = None;
            state.surrounding_text_dirty = true;
            state.content_hint = 0;
            state.content_purpose = 0;
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
        log::trace!(
            ">>> v3 on_set_content_type: guest_id={}, hint={}, purpose={}",
            guest_id,
            hint,
            purpose
        );
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.content_hint = hint;
            state.content_purpose = purpose;
            state.content_type_dirty = true;
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
        let enable_conflict = ctx.text_inputs.get(&guest_id).is_some_and(|state| {
            state.enabled_dirty
                && state.pending_enabled
                && ctx.text_inputs.iter().any(|(&other_id, other)| {
                    other_id != guest_id
                        && other.guest_seat == state.guest_seat
                        && other.committed_enabled
                })
        });
        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
            return Action::Drop;
        };
        state.guest_commit_serial = state.guest_commit_serial.wrapping_add(1);
        let reset_state = state.enabled_dirty;
        if reset_state {
            if enable_conflict {
                log::warn!(
                    "Ignoring text-input enable for guest {}: another input is enabled on seat {}",
                    guest_id,
                    state.guest_seat
                );
                state.pending_enabled = state.committed_enabled;
            } else {
                state.committed_enabled = state.pending_enabled;
            }
            state.enabled_dirty = false;
        }
        if reset_state {
            state.current_preedit.clear();
            state.pending_preedit_cursor = None;
            state.pending_preedit_selection = None;
            state.pending_deletes.clear();
            state.pending_cursor_position = None;
            state.empty_preedit_repeat_active = false;
        }
        let enabled = state.committed_enabled;
        let host_v1_id = state.host_v1_id;
        let commit_serial = state.guest_commit_serial;
        log::trace!(
            ">>> v3 on_commit: guest_id={}, serial={}, enabled={}, host_v1_id={}",
            guest_id,
            commit_serial,
            enabled,
            host_v1_id
        );

        update_host_activation(ctx, guest_id);

        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
            return Action::Drop;
        };
        if state.surrounding_text_dirty {
            state.surrounding_text_dirty = false;
            state.committed_surrounding_text = state.pending_surrounding_text.clone();
            if let Some((text, cursor, anchor)) = &state.committed_surrounding_text {
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
                push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 5, builder);
            }
        }

        if state.content_type_dirty {
            let hint = state.content_hint;
            let purpose = state.content_purpose;
            state.content_type_dirty = false;
            let (v1_hint, v1_purpose, input_type, input_mode, input_flags, learning_mode) =
                map_v3_content_type(hint, purpose);

            if let Some(host_ext_id) = state.host_ext_id {
                // Tell Exo whether this v3 client supplied surrounding text.
                // This must precede set_content_type/set_input_type to take
                // effect for the new input.
                let mut builder = MessageBuilder::new();
                builder.write_u32(u32::from(state.committed_surrounding_text.is_some()));
                push_msg(&mut ctx.client_to_host_queue, host_ext_id, 7, builder);
            }

            // set_content_type: opcode 6 (on zwp_text_input_v1)
            let mut builder = MessageBuilder::new();
            builder.write_u32(v1_hint);
            builder.write_u32(v1_purpose);
            push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 6, builder);

            if let Some(host_ext_id) = state.host_ext_id {
                let mut builder = MessageBuilder::new();
                builder.write_u32(input_type);
                builder.write_u32(input_mode);
                builder.write_u32(input_flags);
                builder.write_u32(learning_mode);
                // A text-input-v3 client supports inline preedit by definition.
                builder.write_u32(1);
                push_msg(&mut ctx.client_to_host_queue, host_ext_id, 6, builder);
            }
        }

        if state.cursor_rect_dirty {
            state.cursor_rect_dirty = false;
            let (x, y, w, h) = state.cursor_rect.unwrap_or((0, 0, 0, 0));
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
            push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 7, builder);
        }

        state.text_change_cause = 0;
        log::debug!("  -> sending v1 commit_state(serial={})", commit_serial);
        // commit_state: opcode 9
        let mut builder = MessageBuilder::new();
        builder.write_u32(commit_serial);
        push_msg(&mut ctx.client_to_host_queue, state.host_v1_id, 9, builder);
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;
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

    /// Helper: set up a context with a host→guest mapping for text input testing.
    fn setup_v1_ctx() -> (Context, u32, u32) {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_v1_id = 10u32;
        let guest_id = 20u32;
        let host_ext_id = 30u32;
        ctx.shadow_table.map_id(guest_id, host_v1_id);
        ctx.text_inputs.insert(
            guest_id,
            crate::state::TextInputState {
                host_v1_id,
                host_ext_id: Some(host_ext_id),
                guest_seat: 0,
                active_surface: None,
                pending_enabled: true,
                committed_enabled: true,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
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
        end_backspace_repeat(&mut ctx);
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
        ctx.keyboard_to_seat.insert(40, 0);
        ctx.peek_pressed_keys
            .insert(crate::handler::keyboard::EVDEV_KEY_BACKSPACE);
        ctx.text_inputs
            .get_mut(&guest_id)
            .unwrap()
            .committed_surrounding_text = Some(("가나다라".to_string(), 12, 12));
        ctx.last_sender_id = 30;
        let mut handler = ExtendedTextInputV1Handler;

        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 6);
        assert!(ctx.text_inputs[&guest_id].empty_preedit_repeat_active);
        for message in &ctx.host_to_client_queue {
            assert_eq!(
                msg_sender(std::slice::from_ref(message), 0),
                40,
                "each repeat must target the active guest keyboard"
            );
            assert_eq!(
                msg_opcode(std::slice::from_ref(message), 0),
                3,
                "each repeat must be a wl_keyboard.key event"
            );
        }
    }

    #[test]
    fn emptied_preedit_enables_held_backspace_fallback_for_committed_text() {
        let (mut ctx, host_v1_id, guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 40;
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
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
            vec![3, 3]
        );
        for message in &ctx.host_to_client_queue {
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
        assert_ne!(release.read_u32().unwrap(), press_serial);
        assert_eq!(release.read_u32().unwrap(), press_time);
        assert_eq!(release.read_u32().unwrap(), 14);
        assert_eq!(release.read_u32().unwrap(), 0);
        assert_eq!(
            ctx.text_inputs[&guest_id].committed_surrounding_text,
            Some(("A가".to_string(), 4, 4))
        );

        ctx.host_to_client_queue.clear();
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 2);

        end_backspace_repeat(&mut ctx);
        ctx.peek_pressed_keys
            .remove(&crate::handler::keyboard::EVDEV_KEY_BACKSPACE);
        ctx.host_to_client_queue.clear();
        assert_eq!(handler.on_confirm_preedit(&mut ctx, 1), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn held_backspace_fallback_requires_guest_keyboard_for_seat() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        ctx.peek_pressed_keys
            .insert(crate::handler::keyboard::EVDEV_KEY_BACKSPACE);
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
        assert!(ctx.shadow_table.get_host_interface(30).is_none());
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 0), 10);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 0), 1);
        assert_eq!(msg_sender(&ctx.client_to_host_queue, 1), 30);
        assert_eq!(msg_opcode(&ctx.client_to_host_queue, 1), 0);
    }

    #[test]
    fn text_input_creation_works_without_optional_chromeos_extension() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_text_input_manager_v1_id = Some(50);
        ctx.active_surface_for_seat.insert(7, 99);
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
    fn on_keysym_forwards_serial_and_time_to_wl_keyboard() {
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
        assert_eq!(serial, 123);
        assert_eq!(time, 456);
    }

    #[test]
    fn test_activation_state_machine() {
        let (mut ctx, _host_v1_id, guest_id) = setup_v1_ctx();
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
}
