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
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::protocols::wayland::wl_keyboard;
#[cfg(test)]
use crate::state::GuestKeyOwner;
use crate::state::{
    reconcile, ActivationStep, Context, CursorRect, GuestKeyDelivery, GuestKeyEvent, HostApplied,
    HostContentType, HostId, HostTarget, InternalCallback, Link, SeatFocusChange, TextInputState,
};
use crate::wire::{queue_or_drop, Action, Outgoing};

/// Queue `zwp_text_input_v3.preedit_string`, or drop host text that does not
/// fit a message (see [`queue_or_drop`]).
///
/// # Returns
///
/// Whether the event was queued.
fn push_preedit_string_to(
    queue: &mut Vec<Outgoing>,
    guest_id: u32,
    text: &str,
    cursor_begin: i32,
    cursor_end: i32,
) -> bool {
    queue_or_drop(
        queue,
        zwp_text_input_v3::evt::preedit_string(guest_id, Some(text), cursor_begin, cursor_end),
        format_args!("the preedit of text input {guest_id}"),
    )
}

/// Queue `zwp_text_input_v3.commit_string`, or drop host text that does not
/// fit a message (see [`queue_or_drop`]).
///
/// # Returns
///
/// Whether the event was queued.
fn push_commit_string_to(queue: &mut Vec<Outgoing>, guest_id: u32, text: &str) -> bool {
    queue_or_drop(
        queue,
        zwp_text_input_v3::evt::commit_string(guest_id, Some(text)),
        format_args!("the commit of text input {guest_id}"),
    )
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
    // The unknown version of a test fixture is the largest, so it allows
    // every request.
    version >= required
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
    for state in [
        wl_keyboard::KEY_STATE_PRESSED,
        wl_keyboard::KEY_STATE_RELEASED,
    ] {
        ctx.synthetic_keyboard_serial = ctx.synthetic_keyboard_serial.wrapping_add(1).max(1);
        ctx.host_to_client_queue.push(wl_keyboard::evt::key(
            held.guest_keyboard_id,
            ctx.synthetic_keyboard_serial,
            held.time,
            held.key,
            state,
        ));
    }
    true
}

/// Map a v3 content type to the host's v1 `(hint, purpose)` and the
/// extension's `(input_type, input_mode, input_flags, learning_mode)`.
pub(crate) fn map_v3_content_type(hint: u32, purpose: u32) -> (u32, u32, u32, u32, u32, u32) {
    use zcr_extended_text_input_v1 as ext;
    use zwp_text_input_v1 as v1;
    use zwp_text_input_v3 as v3;

    // zwp_text_input_v1 has no PIN purpose and its values after PASSWORD are
    // shifted by one compared with v3.
    let mut v1_hint = hint;
    let v1_purpose = match purpose {
        v3::CONTENT_PURPOSE_NORMAL..=v3::CONTENT_PURPOSE_PASSWORD => purpose,
        v3::CONTENT_PURPOSE_PIN => {
            v1_hint |= v1::CONTENT_HINT_SENSITIVE_DATA;
            v1::CONTENT_PURPOSE_DIGITS
        }
        v3::CONTENT_PURPOSE_DATE => v1::CONTENT_PURPOSE_DATE,
        v3::CONTENT_PURPOSE_TIME => v1::CONTENT_PURPOSE_TIME,
        v3::CONTENT_PURPOSE_DATETIME => v1::CONTENT_PURPOSE_DATETIME,
        v3::CONTENT_PURPOSE_TERMINAL => v1::CONTENT_PURPOSE_TERMINAL,
        _ => v1::CONTENT_PURPOSE_NORMAL, // Unknown values degrade to NORMAL.
    };

    // Chrome's extension uses TextInputType/TextInputMode enums rather than
    // Wayland's content-purpose enum.
    let multiline = hint & v3::CONTENT_HINT_MULTILINE != 0;
    let input_type = match (purpose, multiline) {
        (
            v3::CONTENT_PURPOSE_NORMAL | v3::CONTENT_PURPOSE_ALPHA | v3::CONTENT_PURPOSE_NAME,
            true,
        ) => ext::INPUT_TYPE_TEXT_AREA,
        (v3::CONTENT_PURPOSE_DIGITS | v3::CONTENT_PURPOSE_NUMBER | v3::CONTENT_PURPOSE_PIN, _) => {
            ext::INPUT_TYPE_NUMBER
        }
        (v3::CONTENT_PURPOSE_PHONE, _) => ext::INPUT_TYPE_TELEPHONE,
        (v3::CONTENT_PURPOSE_URL, _) => ext::INPUT_TYPE_URL,
        (v3::CONTENT_PURPOSE_EMAIL, _) => ext::INPUT_TYPE_EMAIL,
        (v3::CONTENT_PURPOSE_PASSWORD, _) => ext::INPUT_TYPE_PASSWORD,
        (v3::CONTENT_PURPOSE_DATE, _) => ext::INPUT_TYPE_DATE,
        (v3::CONTENT_PURPOSE_TIME, _) => ext::INPUT_TYPE_TIME,
        (v3::CONTENT_PURPOSE_DATETIME, _) => ext::INPUT_TYPE_DATE_TIME,
        _ => ext::INPUT_TYPE_TEXT,
    };
    let input_mode = match purpose {
        v3::CONTENT_PURPOSE_DIGITS | v3::CONTENT_PURPOSE_PIN => ext::INPUT_MODE_NUMERIC,
        v3::CONTENT_PURPOSE_NUMBER => ext::INPUT_MODE_DECIMAL,
        v3::CONTENT_PURPOSE_PHONE => ext::INPUT_MODE_TEL,
        v3::CONTENT_PURPOSE_URL => ext::INPUT_MODE_URL,
        v3::CONTENT_PURPOSE_EMAIL => ext::INPUT_MODE_EMAIL,
        _ => ext::INPUT_MODE_DEFAULT,
    };

    let mut input_flags = ext::INPUT_FLAGS_NONE;
    if hint & v3::CONTENT_HINT_COMPLETION != 0 {
        input_flags |= ext::INPUT_FLAGS_AUTOCOMPLETE_ON;
    }
    if hint & v3::CONTENT_HINT_SPELLCHECK != 0 {
        // text-input-v3 calls bit 1 `spellcheck`. Chrome's extension keeps
        // spellchecking distinct from its separate autocorrect flag.
        input_flags |= ext::INPUT_FLAGS_SPELLCHECK_ON;
    }
    if hint & v3::CONTENT_HINT_AUTO_CAPITALIZATION != 0 {
        input_flags |= ext::INPUT_FLAGS_AUTOCAPITALIZE_SENTENCES;
    }
    if hint & v3::CONTENT_HINT_LOWERCASE != 0 {
        input_flags |= ext::INPUT_FLAGS_AUTOCAPITALIZE_NONE;
    }
    if hint & v3::CONTENT_HINT_UPPERCASE != 0 {
        input_flags |= ext::INPUT_FLAGS_AUTOCAPITALIZE_CHARACTERS;
    }
    if hint & v3::CONTENT_HINT_TITLECASE != 0 {
        input_flags |= ext::INPUT_FLAGS_AUTOCAPITALIZE_WORDS;
    }
    let secret = hint & (v3::CONTENT_HINT_HIDDEN_TEXT | v3::CONTENT_HINT_SENSITIVE_DATA) != 0
        || matches!(
            purpose,
            v3::CONTENT_PURPOSE_PASSWORD | v3::CONTENT_PURPOSE_PIN
        );
    if secret {
        input_flags |= ext::INPUT_FLAGS_HAS_BEEN_PASSWORD;
    }

    let learning_mode = if secret {
        ext::LEARNING_MODE_DISABLED
    } else {
        ext::LEARNING_MODE_ENABLED
    };
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
        if plan.had_preedit
            && text.is_empty()
            && !commit.is_empty()
            && !push_commit_string_to(&mut transaction, guest_id, commit)
        {
            return Action::Drop;
        }

        if !push_preedit_string_to(&mut transaction, guest_id, text, cursor_begin, cursor_end) {
            return Action::Drop;
        }
        transaction.push(zwp_text_input_v3::evt::done(guest_id, plan.done_serial));

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

        log::trace!(
            ">>> on_commit_string: serial={}, text={:?}, guest_id={}",
            serial,
            text,
            guest_id
        );

        let mut transaction = Vec::new();
        if plan.had_preedit && !push_preedit_string_to(&mut transaction, guest_id, "", 0, 0) {
            return Action::Drop;
        }

        // v1 requires delete_surrounding_text and cursor_position to be
        // applied as part of the following commit_string. Preserve that
        // transaction boundary when translating to v3.
        for &(before, after) in &plan.deletes {
            transaction.push(zwp_text_input_v3::evt::delete_surrounding_text(
                guest_id, before, after,
            ));
        }

        if !push_commit_string_to(&mut transaction, guest_id, text) {
            return Action::Drop;
        }
        transaction.push(zwp_text_input_v3::evt::done(guest_id, plan.done_serial));

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
                        crate::handler::text_input::end_backspace_repeat_for_seat(ctx, guest_seat);
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

    /// Exo attached the input method. While active this answers the
    /// proxy's own `activate` ([`HostApplied`]), and otherwise belongs to a
    /// generation being drained, so it changes nothing.
    fn on_enter(&mut self, ctx: &mut Context, surface: u32) -> Action {
        log::debug!(
            ">>> on_enter: host_v1_id={}, surface={surface}",
            ctx.last_sender_id
        );
        Action::Drop
    }

    /// Exo detached the input method. While active Exo did so on its own
    /// and is now deactivated ([`HostApplied`]): the generation ends, its
    /// composition goes, a preedit the guest shows is cleared, and
    /// reconciling activates again if the guest still wants input. A
    /// `leave` that answers the proxy's `deactivate` arrives while draining
    /// and is ignored.
    fn on_leave(&mut self, ctx: &mut Context) -> Action {
        let host_v1_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_v1_id) else {
            return Action::Drop;
        };
        let Some(state) = ctx
            .text_inputs
            .get_mut(&guest_id)
            .filter(|state| state.host_v1_id == host_v1_id)
        else {
            return Action::Drop;
        };
        let Some(had_preedit) = state.end_on_host_detach() else {
            log::debug!(">>> on_leave: host_v1_id={host_v1_id} while not active");
            return Action::Drop;
        };
        log::info!("Exo detached the input method of text input {guest_id} (host {host_v1_id})");
        // The composition went with the input method, as when its process
        // restarts: the guest would otherwise keep showing it.
        if had_preedit {
            let serial = state.guest_commit_serial();
            let mut transaction = Vec::new();
            if push_preedit_string_to(&mut transaction, guest_id, "", 0, 0) {
                transaction.push(zwp_text_input_v3::evt::done(guest_id, serial));
                ctx.host_to_client_queue.extend(transaction);
            }
        }
        reconcile_host(ctx, guest_id, None);
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
        transaction.push(zwp_text_input_v3::evt::delete_surrounding_text(
            guest_id,
            before_length,
            after_length,
        ));
        if !push_preedit_string_to(
            &mut transaction,
            guest_id,
            &preedit_text,
            cursor_begin,
            cursor_end,
        ) {
            return Action::Drop;
        }
        transaction.push(zwp_text_input_v3::evt::done(guest_id, plan.done_serial));

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
                ctx.host_to_client_queue
                    .push(zwp_text_input_v3::evt::done(guest_id, plan.done_serial));
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
        if !push_preedit_string_to(&mut transaction, guest_id, "", 0, 0) {
            return Action::Drop;
        }

        if !push_commit_string_to(&mut transaction, guest_id, &plan.preedit_text) {
            return Action::Drop;
        }
        transaction.push(zwp_text_input_v3::evt::done(guest_id, plan.done_serial));

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
        // A new object takes its parent's version: the guest text input its
        // guest manager's, and each host object its host manager's.
        let guest_manager_version = ctx
            .shadow_table
            .guest_object_version_or_unknown(ctx.last_sender_id);
        let host_manager_version = ctx
            .shadow_table
            .host_object_version_or_unknown(host_manager_id);
        let host_v1_id = ctx.shadow_table.create_bridged(
            id,
            "zwp_text_input_v3",
            guest_manager_version,
            "zwp_text_input_v1",
            host_manager_version,
        );
        ctx.client_to_host_queue
            .push(zwp_text_input_manager_v1::req::create_text_input(
                host_manager_id,
                host_v1_id,
            ));

        let host_ext_id = ctx
            .host_text_input_extension_v1_id
            .map(|host_ext_manager_id| {
                let host_ext_version = ctx
                    .shadow_table
                    .host_object_version_or_unknown(host_ext_manager_id);
                let host_ext_id = ctx
                    .shadow_table
                    .create_host_only("zcr_extended_text_input_v1", host_ext_version);
                ctx.client_to_host_queue.push(
                    zcr_text_input_extension_v1::req::get_extended_text_input(
                        host_ext_manager_id,
                        host_ext_id,
                        host_v1_id,
                    ),
                );
                host_ext_id
            });

        let active_surface = ctx.keyboard_focus.surface_for_seat(seat);

        ctx.text_inputs.insert(
            id,
            crate::state::TextInputState::new(host_v1_id, host_ext_id, seat, active_surface),
        );

        // A text-input object created after keyboard focus was established
        // still needs the current enter event; otherwise the client may wait
        // indefinitely for a focus transition before enabling IME.
        if let Some(surface) = active_surface {
            ctx.host_to_client_queue
                .push(zwp_text_input_v3::evt::enter(id, surface));
        }

        Action::Drop
    }
}

/// Whether the extension `host_ext_id` takes `set_surrounding_text_support`.
fn host_takes_surrounding_text_support(ctx: &Context, host_ext_id: Option<u32>) -> bool {
    host_ext_id.is_some_and(|host_ext_id| {
        extension_version_allows(
            ctx.shadow_table.host_object_version_or_unknown(host_ext_id),
            zcr_extended_text_input_v1::REQ_SET_SURROUNDING_TEXT_SUPPORT_SINCE,
        )
    })
}

/// Push the host requests that set `content_type` on the text input
/// `(host_v1_id, host_ext_id)`, with its surrounding-text support if it has
/// one.
fn push_content_type(
    transaction: &mut Vec<Outgoing>,
    ctx: &Context,
    (host_v1_id, host_ext_id): (u32, Option<u32>),
    content_type: HostContentType,
) {
    let (hint, purpose) = content_type.content_type;
    let (v1_hint, v1_purpose, input_type, input_mode, input_flags, learning_mode) =
        map_v3_content_type(hint, purpose);
    let host_ext = host_ext_id.map(|host_ext_id| {
        (
            host_ext_id,
            ctx.shadow_table.host_object_version_or_unknown(host_ext_id),
        )
    });
    match host_ext {
        // The extension XML says v8's set_input_type is "used in place of"
        // v1 set_content_type. Exo applies the pending surrounding-text
        // support to whichever of the two comes first and then resets it to
        // supported, so sending both would always report the surrounding
        // text as supported.
        Some((host_ext_id, version))
            if extension_version_allows(
                version,
                zcr_extended_text_input_v1::REQ_SET_INPUT_TYPE_SINCE,
            ) =>
        {
            if let Some(supported) = content_type.surrounding_text_support {
                transaction.push(
                    zcr_extended_text_input_v1::req::set_surrounding_text_support(
                        host_ext_id,
                        if supported {
                            zcr_extended_text_input_v1::SURROUNDING_TEXT_SUPPORT_SUPPORTED
                        } else {
                            zcr_extended_text_input_v1::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED
                        },
                    ),
                );
            }
            // A text-input-v3 client supports inline preedit by definition.
            transaction.push(zcr_extended_text_input_v1::req::set_input_type(
                host_ext_id,
                input_type,
                input_mode,
                input_flags,
                learning_mode,
                zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            ));
        }
        // Before v8 the XML replaces nothing, so the four-argument request
        // follows v1 set_content_type; v8's set_input_type would be a
        // protocol error on v1-v7.
        host_ext => {
            debug_assert_eq!(content_type.surrounding_text_support, None);
            transaction.push(zwp_text_input_v1::req::set_content_type(
                host_v1_id, v1_hint, v1_purpose,
            ));
            if let Some((host_ext_id, _)) = host_ext.filter(|&(_, version)| {
                extension_version_allows(
                    version,
                    zcr_extended_text_input_v1::REQ_DEPRECATED_SET_INPUT_TYPE_SINCE,
                )
            }) {
                transaction.push(zcr_extended_text_input_v1::req::deprecated_set_input_type(
                    host_ext_id,
                    input_type,
                    input_mode,
                    input_flags,
                    learning_mode,
                ));
            }
        }
    }
}

/// The host state the text input `state` should have.
fn host_target(ctx: &Context, state: &TextInputState) -> HostTarget {
    let surface_known = state
        .active_surface
        .and_then(|surface| ctx.shadow_table.get_host_id(surface))
        .is_some();
    HostTarget {
        active: state.committed.enabled && surface_known,
        seat_live: live_host_seat(ctx, state).is_some(),
        content_type: HostContentType {
            content_type: state.committed.content_type,
            surrounding_text_support: host_takes_surrounding_text_support(ctx, state.host_ext_id)
                .then_some(state.committed.surrounding_text.is_some()),
        },
    }
}

/// The host seat that can carry the text input `state`'s `activate` or
/// `deactivate`: the host object of its guest seat, while that ID names a
/// live `wl_seat`.
///
/// A seat can end while its text inputs live on, and the guest may then
/// reuse its ID for any object, which the host would reject as a seat. A
/// seat awaiting its `delete_id` carries nothing either, and neither does
/// an ID with no host object (ID 0 is a protocol error).
fn live_host_seat(ctx: &Context, state: &TextInputState) -> Option<u32> {
    let seat = ctx
        .shadow_table
        .request_object(state.guest_seat, "wl_seat")?;
    match seat {
        Link::Host(host_seat) => Some(host_seat),
        // The proxy implements no wl_seat itself.
        Link::Local => None,
    }
}

/// What a guest commit sends the host besides its reconciled state: the
/// surrounding text, already encoded, and the cursor rectangle it states.
/// Exo keeps both with or without an input method and edits its copy of
/// the surrounding text as the input method commits, so each statement is
/// forwarded rather than reconciled.
#[derive(Default)]
pub(crate) struct GuestCommit {
    surrounding_text: Option<Outgoing>,
    cursor_rect: Option<CursorRect>,
}

/// Queue the host requests that bring the text input `guest_id` to the
/// state its guest committed ([`reconcile`]), and record what the host
/// applies.
///
/// Called wherever that state or the host's ability to apply it changes: a
/// guest commit, which passes its own requests in `commit`, a focus change,
/// a drain barrier's completion, a seat's bind and the object's destroy. The
/// requests go in one order: the activation step, the surrounding text,
/// the content type, the cursor rectangle and, after a guest commit or a
/// content type, `commit_state` with the guest's commit count.
pub(crate) fn reconcile_host(ctx: &mut Context, guest_id: u32, commit: Option<GuestCommit>) {
    let Some(state) = ctx.text_inputs.get(&guest_id) else {
        return;
    };
    let target = host_target(ctx, state);
    let steps = reconcile(&target, &state.host);
    let host_ids = (state.host_v1_id, state.host_ext_id);
    let serial = state.guest_commit_serial();
    let host_seat = live_host_seat(ctx, state);
    let host_surface = state
        .active_surface
        .and_then(|surface| ctx.shadow_table.get_host_id(surface));
    let (host, guest_seat) = (state.host, state.guest_seat);

    match steps.activation {
        Some(ActivationStep::Activate) => {
            let (Some(host_seat), Some(host_surface)) = (host_seat, host_surface) else {
                unreachable!("an activation target has a live seat and a known surface");
            };
            log::info!(
                "Activating text input {guest_id} (host {}) on host surface {host_surface}",
                host_ids.0
            );
            ctx.client_to_host_queue
                .push(zwp_text_input_v1::req::activate(
                    host_ids.0,
                    host_seat,
                    host_surface,
                ));
            let activated = ctx
                .text_inputs
                .get_mut(&guest_id)
                .is_some_and(|state| state.host.activate());
            debug_assert!(activated, "reconcile activates only an inactive host");
        }
        // Deactivate behind a drain barrier
        // ([`InternalCallback::TextInputDrained`]). The ordered host stream
        // is the proof: deactivate first, then sync. Draining rejects stale
        // host events until the callback's `done`.
        Some(ActivationStep::Deactivate) => {
            let host_seat = host_seat.expect("a deactivation target has a live seat");
            log::info!("Deactivating text input {guest_id} (host {})", host_ids.0);
            ctx.client_to_host_queue
                .push(zwp_text_input_v1::req::deactivate(host_ids.0, host_seat));
            let callback = queue_internal_sync(
                ctx,
                InternalCallback::TextInputDrained {
                    text_input: guest_id,
                    host_text_input: host_ids.0,
                },
            );
            let draining = ctx
                .text_inputs
                .get_mut(&guest_id)
                .is_some_and(|state| state.host.begin_draining(callback));
            debug_assert!(draining, "reconcile deactivates only an active host");
        }
        Some(ActivationStep::Abandon) => {
            log::debug!("Text input {guest_id} lost its host seat while active");
            if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
                state.host.end_without_barrier();
            }
        }
        // An activation the target needs and the host cannot take yet.
        None if target.active && !matches!(host, HostApplied::Active { .. }) => {
            if matches!(host, HostApplied::Draining { .. }) {
                log::debug!(
                    "Deferring text input {guest_id} activation until the previous generation drains"
                );
            } else {
                // Warn once per text input, however many seats it loses:
                // every reconcile until the guest binds a seat under the ID
                // would repeat it.
                let first = ctx
                    .text_inputs
                    .get_mut(&guest_id)
                    .is_some_and(|state| !std::mem::replace(&mut state.warned_seat_missing, true));
                let level = if first {
                    log::Level::Warn
                } else {
                    log::Level::Debug
                };
                log::log!(
                    level,
                    "Cannot activate text input {guest_id}: guest seat {guest_seat} has no live host seat"
                );
            }
        }
        None => {}
    }

    let guest_commit = commit.is_some();
    let GuestCommit {
        surrounding_text,
        cursor_rect,
    } = commit.unwrap_or_default();
    let mut transaction = Vec::from_iter(surrounding_text);
    if let Some(content_type) = steps.content_type {
        push_content_type(&mut transaction, ctx, host_ids, content_type);
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.host.apply_content_type(content_type);
        }
    }
    if let Some((x, y, width, height)) = cursor_rect {
        transaction.push(zwp_text_input_v1::req::set_cursor_rectangle(
            host_ids.0, x, y, width, height,
        ));
    }
    if guest_commit || steps.content_type.is_some() {
        transaction.push(zwp_text_input_v1::req::commit_state(host_ids.0, serial));
    }
    ctx.client_to_host_queue.extend(transaction);
}

/// Complete one host activation barrier
/// ([`InternalCallback::TextInputDrained`]) and reconcile the newest guest
/// state.
pub(crate) fn complete_host_activation_barrier(
    ctx: &mut Context,
    callback_id: HostId,
    guest_id: u32,
    host_v1_id: u32,
) {
    let completed = ctx.text_inputs.get_mut(&guest_id).is_some_and(|state| {
        state.host_v1_id == host_v1_id && state.host.complete_draining(callback_id)
    });
    if completed {
        reconcile_host(ctx, guest_id, None);
    }
}

/// Reconcile the text inputs of the guest seat `guest_seat`, which a bind
/// just gave a host seat.
pub(crate) fn reconcile_seat(ctx: &mut Context, guest_seat: u32) {
    let text_inputs = ctx
        .text_inputs
        .iter()
        .filter_map(|(&guest_id, state)| (state.guest_seat == guest_seat).then_some(guest_id))
        .collect::<Vec<_>>();
    for guest_id in text_inputs {
        reconcile_host(ctx, guest_id, None);
    }
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

        let previous_surface = state.apply_focus(current_surface);

        // A stale local projection can already equal the authoritative target
        // even though the seat crossed a real focus boundary. Always
        // invalidate and reconcile host activation, but do not fabricate
        // duplicate guest leave/enter events in that case.
        if previous_surface != current_surface {
            if let Some(surface) = previous_surface {
                events.push(zwp_text_input_v3::evt::leave(guest_text_input_id, surface));
            }
            if let Some(surface) = current_surface {
                events.push(zwp_text_input_v3::evt::enter(guest_text_input_id, surface));
            }
        }
        text_inputs_to_update.push(guest_text_input_id);
    }

    ctx.host_to_client_queue.extend(events);
    for guest_text_input_id in text_inputs_to_update {
        reconcile_host(ctx, guest_text_input_id, None);
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

fn end_backspace_repeat_for_text_input(ctx: &mut Context, guest_seat: u32, guest_text_input: u32) {
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

/// Forget guest `zwp_text_input_v3` `guest_id`, which ended (see
/// `crate::state::lifecycle`), and destroy its host extension. Nothing is
/// sent to its v1 object once the host has deleted it (`host_deleted`).
pub(crate) fn forget_text_input(ctx: &mut Context, guest_id: u32, host_deleted: bool) {
    // Destruction is the final disabled transition. Route it through the
    // same activation reconciler as commit/focus changes so parent-seat
    // lifetime validation cannot drift between teardown paths.
    let guest_seat = ctx.text_inputs.get(&guest_id).map(|state| state.guest_seat);
    if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
        state.begin_destroy();
    }
    if let Some(guest_seat) = guest_seat {
        end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
    }
    if !host_deleted {
        reconcile_host(ctx, guest_id, None);
    } else if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
        state.host.end_without_barrier();
    }

    // zwp_text_input_v1 has no destructor, but its ChromeOS extension does.
    // Stop extension events and remove all local routing state.
    let host_ext_id = ctx
        .text_inputs
        .remove(&guest_id)
        .and_then(|state| state.host_ext_id);
    if let Some(host_ext_id) = host_ext_id {
        ctx.client_to_host_queue
            .push(zcr_extended_text_input_v1::req::destroy(host_ext_id));
        ctx.retire_destroyed_host_object(host_ext_id);
    }
}

pub struct TextInputV3Handler;
impl zwp_text_input_v3::ZwpTextInputV3Handler for TextInputV3Handler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        // The v3 object is guest-facing but backed by a v1 host object whose
        // protocol has no destructor, so the host object stays for the
        // connection and the guest lifecycle completes locally. Ending the
        // object forgets its state (see `forget_text_input`).
        if ctx.guest_ended_host_kept(guest_id) {
            crate::handler::display::queue_delete_id(ctx, guest_id);
        } else {
            log::debug!(
                "Text input {} had already been retired before its destroy handler ran",
                guest_id
            );
        }
        Action::Drop
    }

    fn on_enable(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        log::info!(">>> v3 on_enable: guest_id={}", guest_id);
        if let Some(state) = ctx.text_inputs.get_mut(&guest_id) {
            state.pending = crate::state::V3Pending::reset(true);
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
            state.pending = crate::state::V3Pending::reset(false);
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
            state.pending.surrounding_text = Some(Some((text.clone(), cursor, anchor)));
        }
        Action::Drop
    }

    /// The host's text-input-v1 has no text-change cause to forward it to.
    fn on_set_text_change_cause(&mut self, _ctx: &mut Context, _cause: u32) -> Action {
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
            state.pending.content_type = Some((hint, purpose));
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
            state.pending.cursor_rect = Some((x, y, width, height));
        }
        Action::Drop
    }

    fn on_commit(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        let Some(state) = ctx.text_inputs.get_mut(&guest_id) else {
            return Action::Drop;
        };
        let serial = state.count_guest_commit();
        if !v3_is_focused(ctx, guest_id) {
            return Action::Drop;
        }
        let state = &ctx.text_inputs[&guest_id];
        // Exo has one seat and gives its input method to the text input
        // that activated last, without telling the one it replaced, which
        // then gets no input until it is deactivated and activated again.
        // So one text input on the connection is enabled at a time, whatever
        // guest seat it was created for.
        let enable_conflict = state.pending.enabled == Some(true)
            && ctx
                .text_inputs
                .iter()
                .any(|(&other_id, other)| other_id != guest_id && other.committed.enabled);
        log::trace!(
            ">>> v3 on_commit: guest_id={guest_id}, serial={serial}, host_v1_id={}",
            state.host_v1_id
        );

        // Encode the one request that can fail before changing any state,
        // so a commit that cannot reach the host changes nothing but the
        // commit count, and Exo and the bridge keep the same field. v3
        // enable and disable clear the surrounding text, and the clear
        // reaches the host too, or Exo would keep the old field's text.
        let mut surrounding_text = Vec::new();
        if let Some(text) = &state.pending.surrounding_text {
            let (text, cursor, anchor) =
                text.as_ref().map_or(("", 0, 0), |(text, cursor, anchor)| {
                    (text.as_str(), *cursor, *anchor)
                });
            if !queue_or_drop(
                &mut surrounding_text,
                zwp_text_input_v1::req::set_surrounding_text(
                    state.host_v1_id,
                    text,
                    cursor as u32,
                    anchor as u32,
                ),
                format_args!("the surrounding text of text input {guest_id}"),
            ) {
                return Action::Drop;
            }
        }

        if enable_conflict {
            log::warn!(
                "Ignoring text-input enable for guest {guest_id}: another text input is enabled"
            );
        }
        let guest_seat = state.guest_seat;
        let commit = ctx
            .text_inputs
            .get_mut(&guest_id)
            .expect("the focused text input was found above")
            .commit(enable_conflict);
        if commit.new_field {
            end_backspace_repeat_for_text_input(ctx, guest_seat, guest_id);
        }
        reconcile_host(
            ctx,
            guest_id,
            Some(GuestCommit {
                surrounding_text: surrounding_text.pop(),
                cursor_rect: commit.cursor_rect,
            }),
        );
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::keyboard::KeyboardHandler;
    use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1::ZcrExtendedKeyboardV1Handler;
    use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1::ZcrExtendedTextInputV1Handler;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1::ZwpTextInputV1Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3::ZwpTextInputManagerV3Handler;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::ZwpTextInputV3Handler;
    use crate::protocols::wayland::wl_display;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::{wl_callback, wl_compositor, wl_registry, wl_seat};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};
    use crate::wire::WireMessage;
    use crate::wire::WL_DISPLAY_ID;
    use zcr_extended_text_input_v1::{evt as ext, Request as Ext};
    use zcr_text_input_extension_v1::Request as ExtManager;
    use zwp_text_input_manager_v1::Request as V1Manager;
    use zwp_text_input_v1::{evt as v1, Request as V1};
    use zwp_text_input_v3::{req as v3, Event as V3};

    /// The opcode of the message at `idx` in a queue.
    fn msg_opcode(queue: &[crate::wire::Outgoing], idx: usize) -> u16 {
        crate::test_support::msg_opcode(&queue[idx].0)
    }

    /// The sender of the message at `idx` in a queue.
    fn msg_sender(queue: &[crate::wire::Outgoing], idx: usize) -> u32 {
        crate::test_support::msg_sender(&queue[idx].0)
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
        assert!(!push_commit_string_to(&mut queue, 7, &"x".repeat(4_084)));
        assert!(queue.is_empty());
    }

    #[test]
    fn oversized_surrounding_commit_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = guest_id;
        let oversized = "x".repeat(4_084);
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
        let mut expected = ctx.text_inputs[&guest_id].clone();
        assert_eq!(handler.on_commit(&mut ctx), Action::Drop);

        assert!(ctx.client_to_host_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        // The protocol counts the commit even though nothing reached the host.
        expected.set_guest_commits(1);
        assert_eq!(state, &expected);
        assert!(state.committed.surrounding_text.is_none());
        assert!(state.pending.surrounding_text.is_some());
        assert_eq!(state.committed.content_type, (0, 0));
        assert_eq!(state.pending.content_type, Some((0, 13)));
    }

    /// Helper: set up a context with a host→guest mapping for text input testing.
    fn setup_v1_ctx() -> (Context, u32, u32) {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_v1_id = 10u32;
        let guest_id = 20u32;
        let host_ext_id = 30u32;
        // Keep the fixture's historical guest seat ID (0), but provide the
        // real host mapping that production activation requests require.
        ctx.shadow_table.insert_paired_for_test(0, 2, "wl_seat");
        // The focused surface the host text input is active on.
        ctx.shadow_table
            .insert_paired_for_test(900, 901, "wl_surface");
        ctx.shadow_table.insert_bridged_for_test(
            guest_id,
            host_v1_id,
            "zwp_text_input_v3",
            "zwp_text_input_v1",
        );
        ctx.text_inputs.insert(guest_id, {
            // Most unit tests exercise the v3 transaction bridge without
            // modelling a keyboard enter. Use a stable synthetic focus
            // by default; tests covering the protocol's pre-enter/after-
            // leave behavior explicitly clear this field.
            let mut state =
                crate::state::TextInputState::new(host_v1_id, Some(host_ext_id), 0, Some(900));
            state.committed.enabled = true;
            state.host = HostApplied::Active { content_type: None };
            state
        });
        (ctx, host_v1_id, guest_id)
    }

    fn msg_done_serial(queue: &[crate::wire::Outgoing], idx: usize) -> u32 {
        let payload = &queue[idx].0[8..];
        u32::from_ne_bytes(payload[0..4].try_into().unwrap())
    }

    fn wire_message(queue: &[crate::wire::Outgoing], idx: usize) -> WireMessage<'_> {
        WireMessage::new(
            msg_sender(queue, idx),
            msg_opcode(queue, idx),
            &queue[idx].0[8..],
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
        ctx.shadow_table.insert_paired_for_test(
            guest_keyboard_id,
            host_keyboard_id.0,
            "wl_keyboard",
        );
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
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
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

        let oversized = "x".repeat(4_084);
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

        let oversized = "x".repeat(4_084);
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
    fn setting_preedit_region_clears_stale_backspace_repeat() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed.surrounding_text = Some(("가나다".to_string(), 9, 9));
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
        let oversized = "x".repeat(4_084);
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed.surrounding_text = Some((oversized, 4_084, 4_084));
            state.pending_preedit_cursor = Some(7);
            state.pending_preedit_selection = Some((1, 2));
            state.current_preedit = "old".to_string();
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        assert_eq!(
            ExtendedTextInputV1Handler.on_set_preedit_region(&mut ctx, -4_084, 4_084),
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
    fn oversized_confirm_preedit_does_not_partially_advance_state() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.last_sender_id = 30;
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.current_preedit = "x".repeat(4_084);
        }
        arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), guest_id);

        assert_eq!(
            ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 0),
            Action::Drop
        );

        assert!(ctx.host_to_client_queue.is_empty());
        let state = &ctx.text_inputs[&guest_id];
        assert_eq!(state.current_preedit.len(), 4_084);
        assert!(backspace_repeat_active_for_keyboard(&ctx, HostId(41)));
    }

    #[test]
    fn input_mode_switch_boundary_clears_stale_repeat_before_empty_confirmation() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        ctx.keyboard_to_seat.insert(40, 0);
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.committed.surrounding_text = Some(("english".to_string(), 7, 7));
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
            ctx.text_inputs[&guest_id].committed.surrounding_text,
            Some(("english".to_string(), 7, 7))
        );
    }

    #[test]
    fn held_backspace_repeats_over_committed_korean_without_preedit() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        let host_keyboard_id = 41;
        ctx.shadow_table
            .insert_paired_for_test(40, host_keyboard_id, "wl_keyboard");
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
            .committed
            .surrounding_text = Some(("가나다라".to_string(), 12, 12));
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
            ctx.shadow_table.insert_paired_for_test(
                guest_keyboard_id,
                host_keyboard_id.0,
                "wl_keyboard",
            );
            ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
            hold_repeatable_peek_key(&mut ctx, host_keyboard_id, key, 700, 100);
            ctx.last_sender_id = 30;

            assert_eq!(
                ExtendedTextInputV1Handler.on_confirm_preedit(&mut ctx, 1),
                Action::Drop
            );
            assert_eq!(ctx.host_to_client_queue.len(), 3);
            for (message, expected_state) in ctx.host_to_client_queue[..2].iter().zip([
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_RELEASED,
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
        ctx.shadow_table
            .insert_paired_for_test(40, host_keyboard_id.0, "wl_keyboard");
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
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard.0, "wl_keyboard");
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
        ctx.shadow_table
            .insert_paired_for_test(40, stale_host_keyboard.0, "wl_keyboard");
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, 41, "wl_keyboard");
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
            state.set_guest_commits(2);
            state.committed.surrounding_text = Some(("A가".to_string(), 4, 4));
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
            ctx.text_inputs[&guest_id].committed.surrounding_text,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
            state.committed.surrounding_text = Some(("A가".to_string(), 4, 4));
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
    fn child_objects_inherit_their_manager_versions() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_text_input_manager_v1_id = Some(2);
        ctx.host_text_input_extension_v1_id = Some(3);
        // Both text-input protocols are v1, so artificial manager versions
        // show where each child's version comes from.
        ctx.shadow_table
            .insert_host_only_for_test(2, "zwp_text_input_manager_v1", 4);
        ctx.shadow_table
            .insert_host_only_for_test(3, "zcr_text_input_extension_v1", 7);
        ctx.shadow_table
            .create_local(40, "zwp_text_input_manager_v3", 5);
        ctx.last_sender_id = 40;

        let mut handler = TextInputManagerV3Handler;
        assert_eq!(handler.on_get_text_input(&mut ctx, 20, 30), Action::Drop);
        let state = &ctx.text_inputs[&20];
        let child_id = state.host_ext_id.expect("extended text input child");
        assert_eq!(
            ctx.shadow_table.host_object_version(child_id),
            Some(7),
            "version guards must use the manager's negotiated host version"
        );
        assert_eq!(
            ctx.shadow_table.host_object_version(state.host_v1_id),
            Some(4)
        );
        assert_eq!(ctx.shadow_table.guest_object_version(20), Some(5));
    }

    #[test]
    fn guest_commit_serial_wraps_and_is_forwarded_unchanged() {
        let (mut ctx, _, guest_id) = setup_v1_ctx();
        {
            let state = ctx.text_inputs.get_mut(&guest_id).unwrap();
            state.set_guest_commits(u32::MAX);
            state.host = HostApplied::Inactive;
        }
        ctx.last_sender_id = guest_id;
        let mut handler = TextInputV3Handler;
        handler.on_commit(&mut ctx);

        assert_eq!(ctx.text_inputs[&guest_id].guest_commit_serial(), 0);
        let commit_state = ctx.client_to_host_queue.last().unwrap();
        assert_eq!(msg_done_serial(std::slice::from_ref(commit_state), 0), 0);
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, 888, "wl_keyboard");
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_REPEATED,
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
            wl_keyboard::KEY_STATE_REPEATED
        );
    }

    #[test]
    fn delayed_peek_and_keyboard_press_keep_keysym_pair_balanced() {
        let (mut ctx, host_v1_id, _guest_id) = setup_v1_ctx();
        let guest_keyboard_id = 999u32;
        let host_keyboard_id = 888u32;
        let host_extended_id = 777u32;
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
                0,
            ),
            Action::Drop
        );

        ctx.last_sender_id = host_extended_id;
        assert_eq!(
            keyboard.on_peek_key(&mut ctx, 1, 10, keycode, wl_keyboard::KEY_STATE_PRESSED,),
            Action::Drop
        );
        ctx.last_sender_id = host_keyboard_id;
        assert_eq!(
            keyboard.on_key(&mut ctx, 1, 10, keycode, wl_keyboard::KEY_STATE_PRESSED,),
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
                wl_keyboard::KEY_STATE_RELEASED,
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
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_RELEASED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        for (serial, state) in [
            (1, wl_keyboard::KEY_STATE_PRESSED),
            (2, wl_keyboard::KEY_STATE_RELEASED),
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
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "the completed generation's press serial must remain suppressed"
        );

        for (serial, state) in [
            (3, wl_keyboard::KEY_STATE_PRESSED),
            (4, wl_keyboard::KEY_STATE_RELEASED),
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
            wl_keyboard::KEY_STATE_PRESSED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, 888, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;
        let mut handler = TextInputV1Handler;

        for (serial, state) in [
            (u32::MAX - 1, wl_keyboard::KEY_STATE_PRESSED),
            (u32::MAX, wl_keyboard::KEY_STATE_RELEASED),
            (0, wl_keyboard::KEY_STATE_PRESSED),
            (1, wl_keyboard::KEY_STATE_RELEASED),
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
            wl_keyboard::KEY_STATE_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1000,
            0x8000_0010,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1010,
            0x8000_0011,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_RELEASED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(
            &mut ctx,
            0x8000_0012,
            1020,
            keycode,
            wl_keyboard::KEY_STATE_RELEASED,
        );

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1100,
            0x8000_0020,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
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
            wl_keyboard::KEY_STATE_PRESSED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
            wl_keyboard::KEY_STATE_PRESSED,
        );
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1000,
            u32::MAX - 2,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(&mut ctx, 0, 1010, keycode, wl_keyboard::KEY_STATE_RELEASED);

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            1005,
            u32::MAX - 1,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
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
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1010,
            u32::MAX,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_RELEASED,
            0,
        );
        text_input.on_keysym(
            &mut ctx,
            1110,
            2,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_RELEASED,
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
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_RELEASED,
                wl_keyboard::KEY_STATE_RELEASED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
        keyboard.on_peek_key(&mut ctx, 10, 100, keycode, wl_keyboard::KEY_STATE_PRESSED);
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            100,
            10,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(&mut ctx, 11, 110, keycode, wl_keyboard::KEY_STATE_RELEASED);
        keyboard.on_peek_key(&mut ctx, 20, 200, keycode, wl_keyboard::KEY_STATE_PRESSED);

        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            110,
            11,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_RELEASED,
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
            (20, wl_keyboard::KEY_STATE_PRESSED),
            (21, wl_keyboard::KEY_STATE_RELEASED),
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
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_RELEASED,
                wl_keyboard::KEY_STATE_PRESSED,
                wl_keyboard::KEY_STATE_RELEASED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
        keyboard.on_peek_key(&mut ctx, 10, 100, keycode, wl_keyboard::KEY_STATE_PRESSED);
        ctx.last_sender_id = host_v1_id;
        text_input.on_keysym(
            &mut ctx,
            100,
            10,
            xkbcommon::xkb::keysyms::KEY_a,
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        ctx.last_sender_id = host_extended_id;
        keyboard.on_peek_key(&mut ctx, 11, 110, keycode, wl_keyboard::KEY_STATE_RELEASED);
        keyboard.on_peek_key(&mut ctx, 20, 200, keycode, wl_keyboard::KEY_STATE_PRESSED);

        ctx.last_sender_id = host_v1_id;
        for (serial, state) in [
            (20, wl_keyboard::KEY_STATE_PRESSED),
            (21, wl_keyboard::KEY_STATE_RELEASED),
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
            wl_keyboard::KEY_STATE_RELEASED,
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
                .filter(|&&state| state == wl_keyboard::KEY_STATE_PRESSED)
                .count(),
            2
        );
        assert_eq!(
            states
                .iter()
                .filter(|&&state| state == wl_keyboard::KEY_STATE_RELEASED)
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
            ctx.shadow_table.insert_paired_for_test(
                guest_keyboard_id,
                host_keyboard_id,
                "wl_keyboard",
            );
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
                wl_keyboard::KEY_STATE_PRESSED,
            );
            ctx.last_sender_id = host_v1_id;
            text_input.on_keysym(
                &mut ctx,
                1000,
                u32::MAX - 2,
                xkbcommon::xkb::keysyms::KEY_a,
                wl_keyboard::KEY_STATE_PRESSED,
                0,
            );
            ctx.last_sender_id = host_extended_id;
            keyboard.on_peek_key(
                &mut ctx,
                u32::MAX - 1,
                1010,
                keycode,
                wl_keyboard::KEY_STATE_RELEASED,
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
                wl_keyboard::KEY_STATE_PRESSED,
                0,
            );
            assert_eq!(
                ctx.guest_key_owner(HostId(host_keyboard_id), keycode),
                Some(GuestKeyOwner::TextInputKeysym)
            );
            ctx.last_sender_id = host_extended_id;
            keyboard.on_peek_key(&mut ctx, 1, 1100, keycode, wl_keyboard::KEY_STATE_PRESSED);

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
                    wl_keyboard::KEY_STATE_RELEASED,
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
                    .filter(|&&state| state == wl_keyboard::KEY_STATE_PRESSED)
                    .count(),
                2
            );
            assert_eq!(
                states
                    .iter()
                    .filter(|&&state| state == wl_keyboard::KEY_STATE_RELEASED)
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        assert_eq!(
            handler.on_keysym(
                &mut ctx,
                123,
                456,
                xkbcommon::xkb::keysyms::KEY_a,
                wl_keyboard::KEY_STATE_REPEATED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
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
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
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
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, host_keyboard_id, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_RELEASED,
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
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard_id, 888, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard_id, 0);
        ctx.last_sender_id = host_v1_id;

        let mut handler = TextInputV1Handler;
        handler.on_keysym(
            &mut ctx,
            123,
            456,
            xkbcommon::xkb::keysyms::KEY_BackSpace,
            wl_keyboard::KEY_STATE_PRESSED,
            0,
        );
        handler.on_keysym(
            &mut ctx,
            124,
            457,
            xkbcommon::xkb::keysyms::KEY_BackSpace,
            wl_keyboard::KEY_STATE_RELEASED,
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
            ctx.shadow_table
                .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
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

    /// A guest `zwp_text_input_v3` created for a seat whose keyboard has
    /// given its focus to a surface, through a whole proxy [`Session`].
    struct Field {
        s: Session,
        /// The guest text input.
        ti: u32,
        /// The host `zwp_text_input_v1` behind it.
        host: u32,
        /// The host `zcr_extended_text_input_v1` behind it, 0 without one.
        host_ext: u32,
        seat: Object,
        keyboard: Object,
        surface: Object,
        /// The guest's `zwp_text_input_manager_v3`.
        manager: u32,
    }

    /// The text-input extension version the proxy binds.
    const EXTENSION: Option<u32> = Some(zcr_text_input_extension_v1::VERSION);

    /// A guest text input and the host objects behind it, which a
    /// [`Field`] drives one at a time.
    #[derive(Clone, Copy)]
    struct TextInput {
        ti: u32,
        host: u32,
        host_ext: u32,
    }

    /// What a commit that enables or disables a field sends the host
    /// besides its edits ([`Field::reset`]).
    #[derive(Default)]
    struct Reset {
        /// The activation step, which goes first.
        step: Vec<(u32, AnyRequest)>,
        /// The content type the host lacks, after the surrounding text.
        content_type: Vec<(u32, AnyRequest)>,
    }

    impl Field {
        /// A disabled text input whose host has the text-input extension at
        /// version `extension`, if any.
        async fn new(extension: Option<u32>) -> Self {
            let mut s = Session::new();
            let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
            let keyboard = s.new_id();
            let keyboard = Object {
                id: keyboard,
                host: s
                    .create(wl_seat::req::get_keyboard(seat.id, keyboard))
                    .await,
            };
            let surface = s.surface().await;
            let manager = s
                .bind(
                    "zwp_text_input_manager_v1",
                    zwp_text_input_manager_v1::VERSION,
                )
                .await;
            if let Some(version) = extension {
                s.advertise("zcr_text_input_extension_v1", version).await;
                // The proxy binds it for itself.
                s.host_received();
            }
            let mut f = Self {
                s,
                ti: 0,
                host: 0,
                host_ext: 0,
                seat,
                keyboard,
                surface,
                manager,
            };
            assert_eq!(f.enter_surface().await, []);
            let ti = f.s.new_id();
            f.create_text_input(ti).await;
            f
        }

        /// A disabled text input whose host has the newest extension.
        async fn disabled() -> Self {
            Self::new(EXTENSION).await
        }

        /// A text input the guest enabled, which the host activated.
        async fn enabled() -> Self {
            let mut f = Self::disabled().await;
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.enable_requests(1));
            f
        }

        /// The guest creates text input `ti` for the seat, which the
        /// keyboard focus enters at once.
        async fn create_text_input(&mut self, ti: u32) {
            self.ti = ti;
            self.s
                .guest([zwp_text_input_manager_v3::req::get_text_input(
                    self.manager,
                    self.ti,
                    self.seat.id,
                )])
                .await;
            let host = self.s.host_received();
            let (_, AnyRequest::ZwpTextInputManagerV1(V1Manager::CreateTextInput { id })) = host[0]
            else {
                panic!("the host got no text input: {host:?}");
            };
            self.host = id;
            self.host_ext = match host[1..] {
                [] => 0,
                [(
                    _,
                    AnyRequest::ZcrTextInputExtensionV1(ExtManager::GetExtendedTextInput {
                        id,
                        text_input,
                    }),
                )] if text_input == self.host => id,
                _ => panic!("the host got no extended text input: {host:?}"),
            };
            assert_eq!(
                self.s.guest_received(),
                self.events([V3::Enter {
                    surface: self.surface.id
                }])
            );
        }

        /// The text input the field drives.
        fn text_input(&self) -> TextInput {
            let Self {
                ti, host, host_ext, ..
            } = *self;
            TextInput { ti, host, host_ext }
        }

        /// Drive `text_input` from now on.
        fn select(&mut self, TextInput { ti, host, host_ext }: TextInput) {
            (self.ti, self.host, self.host_ext) = (ti, host, host_ext);
        }

        /// `events` of the guest text input.
        fn events(&self, events: impl IntoIterator<Item = V3>) -> Vec<(u32, AnyEvent)> {
            events
                .into_iter()
                .map(|event| (self.ti, event.into()))
                .collect()
        }

        /// `request` of the host text input.
        fn v1(&self, request: V1) -> (u32, AnyRequest) {
            (self.host, request.into())
        }

        /// `request` of the host extension.
        fn ext(&self, request: Ext) -> (u32, AnyRequest) {
            (self.host_ext, request.into())
        }

        fn activate(&self) -> (u32, AnyRequest) {
            self.v1(V1::Activate {
                seat: self.seat.host,
                surface: self.surface.host,
            })
        }

        fn deactivate(&self) -> (u32, AnyRequest) {
            self.v1(V1::Deactivate {
                seat: self.seat.host,
            })
        }

        fn surrounding_text(&self, text: &str, cursor: u32, anchor: u32) -> (u32, AnyRequest) {
            self.v1(V1::SetSurroundingText {
                text: text.into(),
                cursor,
                anchor,
            })
        }

        /// The unset cursor rectangle that enable and disable leave.
        fn no_cursor_rectangle(&self) -> (u32, AnyRequest) {
            self.v1(V1::SetCursorRectangle {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            })
        }

        fn commit_state(&self, serial: u32) -> (u32, AnyRequest) {
            self.v1(V1::CommitState { serial })
        }

        /// The extension requests that set content type `(hint, purpose)`
        /// with surrounding-text `support`.
        fn content_type(
            &self,
            (hint, purpose): (u32, u32),
            support: bool,
        ) -> [(u32, AnyRequest); 2] {
            let (_, _, input_type, input_mode, input_flags, learning_mode) =
                map_v3_content_type(hint, purpose);
            [
                self.ext(Ext::SetSurroundingTextSupport {
                    support: u32::from(support),
                }),
                self.ext(Ext::SetInputType {
                    input_type,
                    input_mode,
                    input_flags,
                    learning_mode,
                    inline_composition_support:
                        zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                }),
            ]
        }

        /// What the host gets for the guest's commit `serial` that enables
        /// or disables the field: the activation step, the cleared
        /// surrounding text, the content type the host lacks, the cleared
        /// cursor rectangle and the commit.
        fn reset(
            &self,
            serial: u32,
            Reset { step, content_type }: Reset,
        ) -> Vec<(u32, AnyRequest)> {
            let mut requests = step;
            requests.push(self.surrounding_text("", 0, 0));
            requests.extend(content_type);
            requests.extend([self.no_cursor_rectangle(), self.commit_state(serial)]);
            requests
        }

        /// What the host gets when the guest's commit `serial` enables a
        /// field with no content type and the host activates it.
        fn enable_requests(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
            let content_type = self.content_type((0, 0), false);
            let step = vec![self.activate()];
            let content_type = content_type.into();
            self.reset(serial, Reset { step, content_type })
        }

        /// What the host gets when the guest's commit `serial` disables an
        /// active field, which the barrier `callback` drains.
        fn disable_requests(&self, callback: u32, serial: u32) -> Vec<(u32, AnyRequest)> {
            let step = vec![self.deactivate(), sync(callback)];
            self.reset(
                serial,
                Reset {
                    step,
                    ..Reset::default()
                },
            )
        }

        fn password_content_type(&self) -> [(u32, AnyRequest); 2] {
            self.content_type((0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD), false)
        }

        /// What the host gets when the proxy activates the password field
        /// again after the guest's commit `serial`.
        fn password_after_activate(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
            let mut requests = vec![self.activate()];
            requests.extend(self.password_content_type());
            requests.push(self.commit_state(serial));
            requests
        }

        /// The guest releases its seat and the host deletes it.
        async fn release_seat(&mut self) {
            self.s.guest([wl_seat::req::release(self.seat.id)]).await;
            assert_eq!(
                self.s.host_received(),
                [(self.seat.host, wl_seat::Request::Release {}.into())]
            );
            self.s
                .host([wl_display::evt::delete_id(WL_DISPLAY_ID, self.seat.host)])
                .await;
            assert_eq!(
                self.s.guest_received(),
                [(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: self.seat.id }.into()
                )]
            );
        }

        /// The guest binds a new `wl_seat` under the ID of its ended seat;
        /// returns what the host received besides the bind.
        async fn bind_seat_again(&mut self) -> Vec<(u32, AnyRequest)> {
            let registry = self.s.registry().await;
            let name = self.s.advertise("wl_seat", wl_seat::VERSION).await;
            let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
                self.s.guest_received()[..]
            else {
                panic!("the guest was not advertised the seat");
            };
            let bind = wl_registry::req::bind(registry.id, name, "wl_seat", version, self.seat.id);
            self.s.guest([bind.unwrap()]).await;
            let mut host = self.s.host_received();
            let (
                _,
                AnyRequest::WlRegistry(wl_registry::Request::Bind {
                    id: (_, _, seat), ..
                }),
            ) = host.remove(0)
            else {
                panic!("the host got no bind: {host:?}");
            };
            self.seat.host = seat;
            host
        }

        /// The host's keyboard focus leaves the surface.
        async fn keyboard_leave(&mut self) {
            let leave = wl_keyboard::evt::leave(self.keyboard.host, 2, self.surface.host);
            self.s.host([leave]).await;
            let surface = self.surface.id;
            assert_eq!(
                self.s.guest_received(),
                [
                    (
                        self.keyboard.id,
                        wl_keyboard::Event::Leave { serial: 2, surface }.into()
                    ),
                    (self.ti, V3::Leave { surface }.into()),
                ]
            );
        }

        /// The host gives the keyboard's focus to the surface; returns what
        /// the guest got after the keyboard's `enter` and the modifiers the
        /// proxy sends with it.
        async fn enter_surface(&mut self) -> Vec<(u32, AnyEvent)> {
            let enter = wl_keyboard::evt::enter(self.keyboard.host, 1, self.surface.host, &[]);
            self.s.host([enter.unwrap()]).await;
            let mut events = self.s.guest_received();
            let keyboard_events = events.drain(..2.min(events.len())).collect::<Vec<_>>();
            let enter = wl_keyboard::Event::Enter {
                serial: 1,
                surface: self.surface.id,
                keys: Vec::new(),
            };
            let modifiers = wl_keyboard::Event::Modifiers {
                serial: 1,
                mods_depressed: 0,
                mods_latched: 0,
                mods_locked: 0,
                group: 0,
            };
            assert_eq!(
                keyboard_events,
                [
                    (self.keyboard.id, enter.into()),
                    (self.keyboard.id, modifiers.into())
                ]
            );
            events
        }

        /// The host's keyboard focus enters the surface again.
        async fn keyboard_enter(&mut self) {
            let surface = self.surface.id;
            assert_eq!(
                self.enter_surface().await,
                self.events([V3::Enter { surface }])
            );
        }

        /// The guest gets a keyboard of the seat, to which the host gives
        /// the focus on the surface.
        async fn focus_keyboard_of_seat(&mut self) {
            let keyboard = self.s.new_id();
            let host = self
                .s
                .create(wl_seat::req::get_keyboard(self.seat.id, keyboard))
                .await;
            self.keyboard = Object { id: keyboard, host };
            assert_eq!(self.enter_surface().await, []);
        }

        /// The host completes the drain barrier `callback`.
        async fn drain(&mut self, callback: u32) {
            self.s.host([wl_callback::evt::done(callback, 0)]).await;
        }

        /// The host input method's `preedit_string`.
        fn preedit(&self, text: &str, commit: &str) -> Outgoing {
            v1::preedit_string(self.host, 0, text, commit).unwrap()
        }

        /// The host input method's `commit_string`.
        fn commit(&self, text: &str) -> Outgoing {
            v1::commit_string(self.host, 0, text).unwrap()
        }
    }

    /// The `wl_display.sync` that creates drain barrier `callback`.
    fn sync(callback: u32) -> (u32, AnyRequest) {
        (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())
    }

    /// The callback of the one drain barrier among `requests`.
    fn barrier(requests: &[(u32, AnyRequest)]) -> u32 {
        let callbacks = requests
            .iter()
            .filter_map(|request| match request {
                (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) => {
                    Some(*callback)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [callback] = callbacks[..] else {
            panic!("no one drain barrier in {requests:?}");
        };
        callback
    }

    /// The guest's `preedit_string` of `text`, with the cursor at its end.
    fn preedit(text: &str) -> V3 {
        let end = text.len() as i32;
        preedit_at(text, end, end)
    }

    fn preedit_at(text: &str, cursor_begin: i32, cursor_end: i32) -> V3 {
        V3::PreeditString {
            text: Some(text.into()),
            cursor_begin,
            cursor_end,
        }
    }

    /// The guest's `preedit_string` that clears its preedit.
    fn no_preedit() -> V3 {
        preedit_at("", 0, 0)
    }

    fn commit(text: &str) -> V3 {
        V3::CommitString {
            text: Some(text.into()),
        }
    }
    #[tokio::test]
    async fn enable_is_not_activated_until_commit() {
        let mut f = Field::disabled().await;
        f.s.guest([v3::enable(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);

        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
    }

    /// A focus change deactivates the host behind a drain barrier: the
    /// host events sent before it reach no one, and the next activation,
    /// which follows the guest's new state, waits for it.
    #[tokio::test]
    async fn focus_generation_barrier_drops_old_events_before_reactivation() {
        let mut f = Field::enabled().await;
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        // Events the host sent before the barrier.
        f.s.host([f.preedit("old", ""), f.commit("old")]).await;
        assert_eq!(f.s.guest_received(), []);

        // The guest enables the field again while the barrier drains: its
        // state reaches the host first, without an activation.
        f.keyboard_enter().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "new", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [
                f.surrounding_text("new", 3, 3),
                f.no_cursor_rectangle(),
                f.commit_state(2),
            ]
        );

        // Then the activation, and the content type it resets.
        f.drain(callback).await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);

        f.s.host([f.preedit("new", "")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([preedit("new"), V3::Done { serial: 2 }])
        );
    }

    /// Exo drops a content type sent while no input method is attached:
    /// the disable sends none, neither does an enable while the
    /// deactivation drains, and the activation the barrier releases sends
    /// it once.
    #[tokio::test]
    async fn a_deferred_activation_replays_the_committed_content_type() {
        use zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD as PASSWORD;
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), f.reset(3, Reset::default()));

        f.drain(callback).await;
        let mut expected = vec![f.activate()];
        expected.extend(f.content_type((0, PASSWORD), false));
        expected.push(f.commit_state(3));
        assert_eq!(f.s.host_received(), expected);
        let (_, _, input_type, _, _, learning_mode) = map_v3_content_type(0, PASSWORD);
        assert_eq!(input_type, zcr_extended_text_input_v1::INPUT_TYPE_PASSWORD);
        assert_eq!(
            learning_mode,
            zcr_extended_text_input_v1::LEARNING_MODE_DISABLED
        );
    }

    /// A text input the guest enabled on a password field, which the host
    /// activated and attached its input method to.
    async fn attached_password_field() -> Field {
        let mut f = Field::disabled().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        let content_type = f.password_content_type().into();
        assert_eq!(
            f.s.host_received(),
            f.reset(
                1,
                Reset {
                    step: vec![f.activate()],
                    content_type
                }
            )
        );
        f.s.host([v1::enter(f.host, f.surface.host)]).await;
        f
    }

    #[tokio::test]
    async fn the_enter_answering_an_activate_resends_nothing() {
        let mut f = attached_password_field().await;
        assert_eq!(f.s.host_received(), []);

        // The content type sent after the activate is the one Exo applied.
        f.s.guest([
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);
    }

    /// Exo detached on its own, as when its input method is destroyed, and
    /// has no surface left to attach to: the proxy activates again and
    /// resends the content type the new attachment reset.
    #[tokio::test]
    async fn an_exo_detach_activates_again_with_the_content_type() {
        let mut f = attached_password_field().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The enter answering that activate resends nothing.
        f.s.host([v1::enter(f.host, f.surface.host)]).await;
        assert_eq!(f.s.host_received(), []);
        // The new generation takes host events.
        f.s.host([f.commit("가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("가"), V3::Done { serial: 1 }])
        );
    }

    #[tokio::test]
    async fn an_exo_detach_clears_the_guest_preedit() {
        let mut f = attached_password_field().await;

        // Without a preedit the guest is told nothing.
        f.s.host([v1::leave(f.host), v1::enter(f.host, f.surface.host)])
            .await;
        assert_eq!(f.s.guest_received(), []);
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The input method's process restarts mid-composition.
        f.s.host([f.preedit("가", "가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([preedit("가"), V3::Done { serial: 1 }])
        );
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([no_preedit(), V3::Done { serial: 1 }])
        );
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The composition went with it: a commit clears no preedit.
        f.s.host([f.commit("나")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("나"), V3::Done { serial: 1 }])
        );
    }

    /// Exo's text input hears of a focus change before the keyboard: its
    /// `leave` arrives first, and the proxy activates again on the surface
    /// Exo has just left, which Exo keeps without attaching.
    #[tokio::test]
    async fn an_exo_detach_before_the_keyboard_leave_activates_then_drains() {
        let mut f = attached_password_field().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), f.password_after_activate(1));

        // The keyboard leave then deactivates it behind a barrier.
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, [f.deactivate(), sync(callback)]);

        // Exo never attached, so it sends no leave, and the drain ends with
        // nothing more to send.
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
        // The host is inactive: it neither forwards nor takes its events.
        f.s.host([f.commit("가")]).await;
        assert_eq!(f.s.guest_received(), []);
    }

    /// Exo detached while the guest's seat cannot carry an `activate`:
    /// nothing is sent, not even a changed content type, until it can.
    #[tokio::test]
    async fn a_content_type_waits_for_exo_to_attach_again() {
        let mut f = attached_password_field().await;
        f.release_seat().await;
        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), []);

        f.s.guest([
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_NORMAL),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);
    }

    #[tokio::test]
    async fn the_leave_answering_a_deactivate_is_ignored() {
        let mut f = attached_password_field().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        f.s.host([v1::leave(f.host)]).await;
        assert_eq!(f.s.host_received(), []);
        // The barrier still drains the generation, and nothing activates.
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// The text input's seat ends, and the guest binds a new `wl_seat`
    /// under its ID, which gives the text input a host seat again.
    #[tokio::test]
    async fn an_activation_deferred_for_an_unmapped_seat_sends_the_content_type() {
        let mut f = Field::disabled().await;
        f.release_seat().await;

        // The seat has no host object: nothing can activate, which the
        // proxy warns of once.
        f.s.guest([
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), f.reset(1, Reset::default()));
        // Only the log shows the warning, so read the flag that makes it
        // once.
        assert!(f.s.ctx().text_inputs[&f.ti].warned_seat_missing);

        // Once it has one, the text input activates, and its content type
        // follows the activation.
        let host = f.bind_seat_again().await;
        let mut expected = vec![f.activate()];
        expected.extend(f.password_content_type());
        expected.push(f.commit_state(1));
        assert_eq!(host, expected);
    }

    /// One deactivation owns one barrier however the guest's state changes
    /// while it drains, and a field disabled by then is not activated.
    #[tokio::test]
    async fn activation_barrier_coalesces_changes_and_honors_final_disabled_target() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));

        f.keyboard_leave().await;
        f.keyboard_enter().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let mut expected = f.reset(3, Reset::default());
        expected.extend(f.reset(4, Reset::default()));
        assert_eq!(f.s.host_received(), expected);

        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// The guest destroys a text input while its deactivation drains and
    /// reuses its ID for a new one: the old barrier must not reconcile it.
    #[tokio::test]
    async fn stale_activation_callback_cannot_reconcile_reused_guest_id() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        assert_eq!(host, f.disable_requests(callback, 2));
        let old_ti = f.ti;
        f.s.guest([v3::destroy(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.ext(Ext::Destroy {})]);
        assert_eq!(
            f.s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: old_ti }.into()
            )]
        );

        f.create_text_input(old_ti).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));

        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// Exo has one seat and gives its input method to the text input that
    /// activated last, so a second text input, of the same or another
    /// guest seat, cannot take it from an enabled one.
    #[tokio::test]
    async fn second_text_input_enable_on_the_connection_is_ignored() {
        for other_seat in [false, true] {
            let mut f = Field::enabled().await;
            let first = f.text_input();
            if other_seat {
                f.seat = f.s.bind_paired("wl_seat", wl_seat::VERSION).await;
                f.focus_keyboard_of_seat().await;
            }
            let id = f.s.new_id();
            f.create_text_input(id).await;

            // A rejected enable still counts: its commit is the first.
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(
                f.s.host_received(),
                f.reset(1, Reset::default()),
                "another seat: {other_seat}"
            );

            // Its disable takes nothing from the first, which Exo still
            // gives its input method.
            f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
            assert_eq!(f.s.host_received(), f.reset(2, Reset::default()));
            f.select(first);
            f.s.host([f.commit("가")]).await;
            assert_eq!(
                f.s.guest_received(),
                f.events([commit("가"), V3::Done { serial: 1 }])
            );
        }
    }
    /// A disable invalidates the surrounding text, at the host too, and
    /// deactivates it, but only at its commit.
    #[tokio::test]
    async fn disable_commit_invalidates_surrounding_text_and_deactivates() {
        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), {
            let mut expected = vec![f.surrounding_text("가", 3, 3)];
            expected.extend(f.content_type((0, 0), true));
            expected.push(f.commit_state(2));
            expected
        });

        f.s.guest([v3::disable(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);
        f.s.guest([v3::commit(f.ti)]).await;
        let host = f.s.host_received();
        assert_eq!(host, f.disable_requests(barrier(&host), 3));
    }

    /// An enable starts a new field: it closes the composition the input
    /// method left, with its pending preedit cursor, selection and edits.
    #[tokio::test]
    async fn committed_enable_resets_previous_ime_event_transaction() {
        for preedit_first in [true, false] {
            let mut f = Field::enabled().await;
            f.s.host([
                f.preedit("이전", ""),
                v1::preedit_styling(f.host, 0, 3, 6),
                v1::preedit_cursor(f.host, 3),
                v1::delete_surrounding_text(f.host, -3, 3),
                v1::cursor_position(f.host, 1, 1),
            ])
            .await;
            let expected = [preedit("이전"), V3::Done { serial: 1 }];
            assert_eq!(f.s.guest_received(), f.events(expected));
            // No v3 event carries the cursor position, so read it.
            let cursor_position =
                |f: &mut Field| f.s.ctx().text_inputs[&f.ti].pending_cursor_position;
            assert_eq!(cursor_position(&mut f), Some((1, 1)));

            f.s.guest([v3::enable(f.ti)]).await;
            f.s.guest([v3::commit(f.ti)]).await;
            assert_eq!(cursor_position(&mut f), None);
            let content_type = f.content_type((0, 0), false).into();
            let expected = f.reset(
                2,
                Reset {
                    content_type,
                    ..Reset::default()
                },
            );
            assert_eq!(f.s.host_received(), expected);

            let done = V3::Done { serial: 2 };
            if preedit_first {
                // No cursor or selection for the next preedit.
                f.s.host([f.preedit("가나", "")]).await;
                assert_eq!(
                    f.s.guest_received(),
                    f.events([preedit_at("가나", 6, 6), done])
                );
            } else {
                // No preedit to clear and no edit to apply.
                f.s.host([f.commit("새")]).await;
                assert_eq!(f.s.guest_received(), f.events([commit("새"), done]));
            }
        }
    }

    /// Each commit forwards the surrounding text it states; offsets that
    /// are not character boundaries within the text state none.
    #[tokio::test]
    async fn invalid_utf8_surrounding_offsets_do_not_replace_pending_state() {
        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_surrounding_text(f.ti, "가나", 3, 6).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text("가나", 3, 6)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);

        for (serial, (cursor, anchor)) in (3..).zip([(1, 1), (3, 7), (-1, 0)]) {
            f.s.guest([
                v3::set_surrounding_text(f.ti, "가", cursor, anchor).unwrap(),
                v3::commit(f.ti),
            ])
            .await;
            assert_eq!(
                f.s.host_received(),
                [f.commit_state(serial)],
                "{cursor}, {anchor}"
            );
        }
    }

    /// The v3 content type maps to the host's, which is told with it that
    /// a v3 client supports inline composition, and whether the field has
    /// surrounding text.
    #[tokio::test]
    async fn content_type_mapping_preserves_pin_privacy_and_inline_composition() {
        use zcr_extended_text_input_v1::{
            INPUT_FLAGS_HAS_BEEN_PASSWORD as SECRET, INPUT_FLAGS_SPELLCHECK_ON, INPUT_MODE_NUMERIC,
            INPUT_TYPE_NUMBER, INPUT_TYPE_PASSWORD, INPUT_TYPE_TEXT_AREA, LEARNING_MODE_DISABLED,
        };
        use zwp_text_input_v1::{CONTENT_HINT_SENSITIVE_DATA, CONTENT_PURPOSE_DIGITS};
        use zwp_text_input_v3::{
            CONTENT_HINT_MULTILINE, CONTENT_HINT_SPELLCHECK, CONTENT_PURPOSE_PASSWORD,
            CONTENT_PURPOSE_PIN,
        };
        // A text field, a terminal and a password field.
        use zcr_extended_text_input_v1::{
            INPUT_FLAGS_NONE, INPUT_MODE_DEFAULT, INPUT_TYPE_TEXT, LEARNING_MODE_ENABLED,
        };
        use zwp_text_input_v1 as v1_enum;
        use zwp_text_input_v3::CONTENT_PURPOSE_TERMINAL;
        let text = |v1_purpose| {
            let flags = INPUT_FLAGS_NONE;
            (
                0,
                v1_purpose,
                INPUT_TYPE_TEXT,
                INPUT_MODE_DEFAULT,
                flags,
                LEARNING_MODE_ENABLED,
            )
        };
        assert_eq!(
            map_v3_content_type(0, 0),
            text(v1_enum::CONTENT_PURPOSE_NORMAL)
        );
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_TERMINAL),
            text(v1_enum::CONTENT_PURPOSE_TERMINAL)
        );
        let (mode, purpose) = (INPUT_MODE_DEFAULT, v1_enum::CONTENT_PURPOSE_PASSWORD);
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_PASSWORD),
            (
                0,
                purpose,
                INPUT_TYPE_PASSWORD,
                mode,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        // v1 has no PIN purpose: digits, sensitive and never learned.
        assert_eq!(
            map_v3_content_type(0, CONTENT_PURPOSE_PIN),
            (
                CONTENT_HINT_SENSITIVE_DATA,
                CONTENT_PURPOSE_DIGITS,
                INPUT_TYPE_NUMBER,
                INPUT_MODE_NUMERIC,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_SENSITIVE_DATA, CONTENT_PURPOSE_PASSWORD),
            (
                0x80,
                8,
                INPUT_TYPE_PASSWORD,
                0,
                SECRET,
                LEARNING_MODE_DISABLED
            )
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_MULTILINE, 0).2,
            INPUT_TYPE_TEXT_AREA
        );
        assert_eq!(
            map_v3_content_type(CONTENT_HINT_SPELLCHECK, 0).4,
            INPUT_FLAGS_SPELLCHECK_ON,
            "v3 spellcheck must map to Chrome spellcheck_on"
        );

        let mut f = Field::enabled().await;
        f.s.guest([
            v3::set_content_type(f.ti, 0, CONTENT_PURPOSE_PIN),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [
                f.ext(Ext::SetSurroundingTextSupport {
                    support: zcr_extended_text_input_v1::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED
                }),
                f.ext(Ext::SetInputType {
                    input_type: INPUT_TYPE_NUMBER,
                    input_mode: INPUT_MODE_NUMERIC,
                    input_flags: SECRET,
                    learning_mode: LEARNING_MODE_DISABLED,
                    inline_composition_support:
                        zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                }),
                f.commit_state(2),
            ]
        );
    }

    /// GTK resends the unchanged terminal content type after IME events.
    /// Before v8, replaying `set_content_type` and
    /// `deprecated_set_input_type` resets Exo's active Korean preedit before
    /// the next key arrives, so a content type the host has, also one
    /// changed and changed back before the commit, is not resent.
    #[tokio::test]
    async fn repeated_content_type_does_not_reset_active_host_composition() {
        use zwp_text_input_v3::{CONTENT_PURPOSE_PIN as PIN, CONTENT_PURPOSE_TERMINAL as TERMINAL};
        for purposes in [&[TERMINAL][..], &[PIN, TERMINAL]] {
            let mut f = Field::enabled().await;
            f.s.guest([v3::set_content_type(f.ti, 0, TERMINAL), v3::commit(f.ti)])
                .await;
            let mut expected = Vec::from(f.content_type((0, TERMINAL), false));
            expected.push(f.commit_state(2));
            assert_eq!(f.s.host_received(), expected);

            let mut requests = purposes
                .iter()
                .map(|&purpose| v3::set_content_type(f.ti, 0, purpose))
                .collect::<Vec<_>>();
            requests.push(v3::commit(f.ti));
            f.s.guest(requests).await;
            assert_eq!(f.s.host_received(), [f.commit_state(3)], "{purposes:?}");
        }
    }

    #[tokio::test]
    async fn surrounding_text_support_follows_the_field_and_resends_only_on_change() {
        let mut f = Field::enabled().await;
        let edits = |f: &Field, serial, support| {
            let mut requests = Vec::from(f.content_type((0, 0), support));
            requests.push(f.commit_state(serial));
            requests
        };

        // Chromium enables with a content type and no surrounding text,
        // which the host is told unsupported ([`Field::enabled`]), and sends
        // the text in a later commit, with no new content type. Exo learns
        // the support only with a content type, so it is resent.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abc", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text("abc", 3, 3)];
        expected.extend(edits(&f, 2, true));
        assert_eq!(f.s.host_received(), expected);

        // While the support holds, the content type is not resent: before
        // v8 a resend changes Exo's type twice, which can reset its
        // composition.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abcd", 4, 4).unwrap(),
            v3::set_content_type(f.ti, 0, 0),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("abcd", 4, 4), f.commit_state(3)]
        );

        // A disable and enable reset the editor state: the content type is
        // replayed with the support of the field's new, empty state.
        f.s.guest([
            v3::disable(f.ti),
            v3::enable(f.ti),
            v3::set_content_type(f.ti, 0, 0),
            v3::commit(f.ti),
        ])
        .await;
        let content_type = f.content_type((0, 0), false).into();
        assert_eq!(
            f.s.host_received(),
            f.reset(
                4,
                Reset {
                    content_type,
                    ..Reset::default()
                }
            )
        );
    }

    #[tokio::test]
    async fn a_host_without_surrounding_text_support_gets_no_resend() {
        // v8 has set_input_type but not set_surrounding_text_support.
        let mut f = Field::new(Some(8)).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        let [_, set_input_type] = f.content_type((0, 0), false);
        let expected = f.reset(
            1,
            Reset {
                step: vec![f.activate()],
                content_type: vec![set_input_type],
            },
        );
        assert_eq!(f.s.host_received(), expected);

        // The host was told no support, so gaining surrounding text changes
        // nothing it knows.
        f.s.guest([
            v3::set_surrounding_text(f.ti, "abc", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(
            f.s.host_received(),
            [f.surrounding_text("abc", 3, 3), f.commit_state(2)]
        );
    }

    /// The host requests of a content type for each extension version.
    #[tokio::test]
    async fn content_type_takes_one_host_request_per_extension_version() {
        use zwp_text_input_v3::CONTENT_PURPOSE_PIN as PIN;
        let (hint, purpose, input_type, input_mode, input_flags, learning_mode) =
            map_v3_content_type(0, PIN);
        for version in [1, 7, 8, 9] {
            let mut f = Field::new(Some(version)).await;
            f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
            f.s.host_received();
            f.s.guest([v3::set_content_type(f.ti, 0, PIN), v3::commit(f.ti)])
                .await;
            let set_content_type = f.v1(V1::SetContentType { hint, purpose });
            let deprecated_set_input_type = f.ext(Ext::DeprecatedSetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
            });
            let [support, set_input_type] = f.content_type((0, PIN), false);
            let expected = match version {
                // v1 has no extension input-type request.
                1 => vec![set_content_type],
                // v2-v7 refine the v1 content type with
                // deprecated_set_input_type.
                7 => vec![set_content_type, deprecated_set_input_type],
                // v8's set_input_type is used in place of v1
                // set_content_type.
                8 => vec![set_input_type],
                // Exo applies surrounding-text support to the next
                // set_content_type or set_input_type and then resets it, so
                // nothing may come between.
                9 => vec![support, set_input_type],
                version => panic!("uncovered version {version}"),
            };
            let mut expected = expected;
            expected.push(f.commit_state(2));
            assert_eq!(f.s.host_received(), expected, "version {version}");
        }
    }

    #[tokio::test]
    async fn destroy_drops_v3_request_and_cleans_host_extension_routing() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::destroy(f.ti)]).await;
        let host = f.s.host_received();
        // The destroy is the final disable: the host extension goes after
        // the generation's barrier.
        let callback = barrier(&host);
        assert_eq!(
            host,
            [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
        );
        assert_eq!(
            f.s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: f.ti }.into()
            )],
            "the guest-local destruction is acknowledged by wl_display"
        );

        // The host keeps the v1 object, which has no destructor, and the
        // extension's ID until its delete_id: a new text input takes
        // neither.
        let (old_host, old_host_ext) = (f.host, f.host_ext);
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        assert!(![old_host, old_host_ext].contains(&f.host));
        assert!(![old_host, old_host_ext].contains(&f.host_ext));
        f.drain(callback).await;
        assert_eq!(f.s.host_received(), []);
    }

    /// A destroyed text input whose seat ended, or awaits its
    /// `delete_id`, sends no `deactivate`: no seat can carry it.
    #[tokio::test]
    async fn destroy_after_seat_release_does_not_deactivate_destroyed_host_seat() {
        for deleted in [false, true] {
            let mut f = Field::enabled().await;
            if deleted {
                f.release_seat().await;
            } else {
                f.s.guest([wl_seat::req::release(f.seat.id)]).await;
                let release = (f.seat.host, wl_seat::Request::Release {}.into());
                assert_eq!(f.s.host_received(), [release]);
            }

            f.s.guest([v3::destroy(f.ti)]).await;
            assert_eq!(
                f.s.host_received(),
                [f.ext(Ext::Destroy {})],
                "deleted: {deleted}"
            );
            assert_eq!(
                f.s.guest_received(),
                [(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: f.ti }.into()
                )]
            );
        }
    }

    /// The active text input's seat ended, and the guest reused its ID for
    /// a surface, which the host would reject as a seat.
    #[tokio::test]
    async fn a_seat_id_reused_for_another_object_is_not_sent_as_a_seat() {
        let mut f = Field::enabled().await;
        f.release_seat().await;
        let compositor = f.s.bind("wl_compositor", wl_compositor::VERSION).await;
        f.s.create(wl_compositor::req::create_surface(compositor, f.seat.id))
            .await;

        // Neither disabling the field nor enabling it again sends the ID.
        for request in [v3::disable(f.ti), v3::enable(f.ti)] {
            f.s.guest([request, v3::commit(f.ti)]).await;
            let host = f.s.host_received();
            assert!(
                !host.iter().any(|(sender, request)| *sender == f.host
                    && matches!(
                        request,
                        AnyRequest::ZwpTextInputV1(V1::Activate { .. } | V1::Deactivate { .. })
                    )),
                "{host:?}"
            );
        }
    }

    #[tokio::test]
    async fn text_input_creation_works_without_optional_chromeos_extension() {
        let mut f = Field::new(None).await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        let content_type = f.v1(V1::SetContentType {
            hint: 0,
            purpose: 0,
        });
        let expected = f.reset(
            1,
            Reset {
                step: vec![f.activate()],
                content_type: vec![content_type],
            },
        );
        assert_eq!(f.s.host_received(), expected);
    }

    /// A leave is a hard boundary: what the guest requests until the next
    /// enter does not reach the host. Its commits still count
    /// (`v3_done_serial_counts_commits_made_while_unfocused`).
    #[tokio::test]
    async fn v3_requests_after_leave_are_ignored_until_next_enter() {
        let mut f = Field::disabled().await;
        f.keyboard_leave().await;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "stale", 0, 0).unwrap(),
            v3::set_text_change_cause(f.ti, 1),
            v3::set_content_type(f.ti, 3, 4),
            v3::set_cursor_rectangle(f.ti, 1, 2, 3, 4),
            v3::disable(f.ti),
            v3::commit(f.ti),
        ])
        .await;
        assert_eq!(f.s.host_received(), []);
        // The next enter resets the pending state, so no peer can see it;
        // read it to pin that the leave bounds what the requests change.
        assert_eq!(
            f.s.ctx().text_inputs[&f.ti].pending,
            crate::state::V3Pending::default(),
            "inactive v3 requests must not mutate double-buffered state"
        );
    }

    /// text-input-v3 numbers `done` by every commit on the object, so the
    /// commits a client makes while unfocused count as well.
    #[tokio::test]
    async fn v3_done_serial_counts_commits_made_while_unfocused() {
        let mut f = Field::enabled().await;

        // Focus leaves; the client disables and commits while unfocused.
        f.keyboard_leave().await;
        let host = f.s.host_received();
        let callback = barrier(&host);
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), []);

        // Focus enters again; the client enables and commits.
        f.keyboard_enter().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        f.drain(callback).await;
        f.s.host_received();

        // The IME commits text; `done` carries the client's commit count.
        f.s.host([f.commit("가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("가"), V3::Done { serial: 3 }])
        );
    }
    /// A sibling text input's rejected enable, or its destroy, ends no
    /// repeat lease of the enabled one.
    #[test]
    fn a_sibling_text_input_keeps_the_active_repeat_lease() {
        for destroy in [false, true] {
            let (mut ctx, _, active_id) = setup_v1_ctx();
            let sibling_id = 21;
            ctx.shadow_table
                .insert_paired_for_test(sibling_id, 11, "zwp_text_input_v3");
            ctx.text_inputs.insert(
                sibling_id,
                crate::state::TextInputState::new(11, None, 0, Some(900)),
            );
            arm_backspace_repeat_for_test(&mut ctx, 40, HostId(41), active_id);

            ctx.last_sender_id = sibling_id;
            if destroy {
                TextInputV3Handler.on_destroy(&mut ctx);
            } else {
                TextInputV3Handler.on_enable(&mut ctx);
                TextInputV3Handler.on_commit(&mut ctx);
                assert!(!ctx.text_inputs[&sibling_id].committed.enabled);
            }
            assert_eq!(
                ctx.key_generations
                    .ime_repeat_owner(HostId(41), crate::handler::keyboard::EVDEV_KEY_BACKSPACE),
                Some(active_id),
                "destroy: {destroy}"
            );
        }
    }

    /// The `done` of the guest's first commit.
    const DONE: V3 = V3::Done { serial: 1 };

    fn delete(before_length: u32, after_length: u32) -> V3 {
        V3::DeleteSurroundingText {
            before_length,
            after_length,
        }
    }

    /// The host input method's events reach the guest as text-input-v3
    /// transactions, each closed by a `done` that numbers the guest's
    /// commits, whatever serial the host sent.
    #[tokio::test]
    async fn host_ime_events_become_v3_transactions() {
        type Events = fn(&Field) -> Vec<Outgoing>;
        let korean: Events = |f| {
            vec![
                f.preedit("ㄱ", "ㄱ"),
                f.preedit("가", "가"),
                f.preedit("간", "간"),
                f.commit("가"),
                f.preedit("나", "나"),
                f.preedit("낟", "낟"),
                f.commit("나"),
                f.preedit("다", "다"),
            ]
        };
        let cases: Vec<(&str, Events, Vec<V3>)> = vec![
            (
                "a preedit",
                |f| vec![v1::preedit_string(f.host, 42, "こんにちは", "").unwrap()],
                vec![preedit("こんにちは"), DONE],
            ),
            (
                "a live preedit keeps its fallback",
                |f| vec![f.preedit("가", ""), f.preedit("간", "reset fallback")],
                vec![preedit("가"), DONE, preedit("간"), DONE],
            ),
            (
                "a preedit reset commits its fallback",
                |f| vec![f.preedit("미완성", ""), f.preedit("", "확정")],
                vec![preedit("미완성"), DONE, commit("확정"), no_preedit(), DONE],
            ),
            (
                "a reset without a preedit commits nothing",
                |f| vec![f.preedit("", "stale fallback")],
                vec![no_preedit(), DONE],
            ),
            (
                "a commit clears the preedit",
                |f| vec![f.preedit("미완성", ""), f.commit("確定")],
                vec![preedit("미완성"), DONE, no_preedit(), commit("確定"), DONE],
            ),
            (
                "Korean composition commits each syllable once",
                korean,
                vec![
                    preedit("ㄱ"),
                    DONE,
                    preedit("가"),
                    DONE,
                    preedit("간"),
                    DONE,
                    no_preedit(),
                    commit("가"),
                    DONE,
                    preedit("나"),
                    DONE,
                    preedit("낟"),
                    DONE,
                    no_preedit(),
                    commit("나"),
                    DONE,
                    preedit("다"),
                    DONE,
                ],
            ),
            (
                "a delete waits for its commit",
                |f| vec![v1::delete_surrounding_text(f.host, -3, 5)],
                vec![],
            ),
            (
                "a delete spans the cursor",
                |f| vec![v1::delete_surrounding_text(f.host, -3, 5), f.commit("")],
                vec![delete(3, 2), commit(""), DONE],
            ),
            (
                "a delete apart from the cursor would delete the text between",
                |f| vec![v1::delete_surrounding_text(f.host, -5, 3), f.commit("")],
                vec![commit(""), DONE],
            ),
            (
                "deletes accumulate until the commit",
                |f| {
                    vec![
                        v1::delete_surrounding_text(f.host, -3, 3),
                        v1::delete_surrounding_text(f.host, 0, 2),
                        f.commit("한"),
                    ]
                },
                vec![delete(3, 0), delete(0, 2), commit("한"), DONE],
            ),
            (
                "a preedit cursor is a byte offset",
                |f| vec![v1::preedit_cursor(f.host, 3), f.preedit("가나", "")],
                vec![preedit_at("가나", 3, 3), DONE],
            ),
            (
                "a selection is a byte range, which the cursor does not move",
                |f| {
                    vec![
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 6),
                        f.preedit("가나", ""),
                    ]
                },
                vec![preedit_at("가나", 0, 3), DONE],
            ),
            (
                "a cursor inside a character falls back to the end",
                |f| vec![v1::preedit_cursor(f.host, 1), f.preedit("가", "")],
                vec![preedit("가"), DONE],
            ),
            (
                "a selection inside a character falls back to the end",
                |f| vec![v1::preedit_styling(f.host, 1, 1, 6), f.preedit("가", "")],
                vec![preedit("가"), DONE],
            ),
            (
                "a confirm without a preedit or a held key",
                |f| vec![ext::confirm_preedit(f.host_ext, 1)],
                vec![],
            ),
            (
                "a confirm of an emptied preedit without a held key",
                |f| {
                    vec![
                        f.preedit("가", ""),
                        f.preedit("", ""),
                        ext::confirm_preedit(f.host_ext, 0),
                    ]
                },
                vec![preedit("가"), DONE, no_preedit(), DONE],
            ),
            (
                // Its selection and cursor go with it, while the edits stay
                // for the next commit.
                "a confirm commits the preedit",
                |f| {
                    vec![
                        f.preedit("나", ""),
                        v1::preedit_styling(f.host, 0, 3, 6),
                        v1::preedit_cursor(f.host, 3),
                        v1::delete_surrounding_text(f.host, -3, 3),
                        ext::confirm_preedit(f.host_ext, 0),
                        f.preedit("다라", ""),
                        f.commit("다"),
                        f.commit("라"),
                    ]
                },
                vec![
                    preedit("나"),
                    DONE,
                    no_preedit(),
                    commit("나"),
                    DONE,
                    preedit_at("다라", 6, 6),
                    DONE,
                    no_preedit(),
                    delete(3, 0),
                    commit("다"),
                    DONE,
                    commit("라"),
                    DONE,
                ],
            ),
        ];
        for (case, events, expected) in cases {
            let mut f = Field::enabled().await;
            let events = events(&f);
            f.s.host(events).await;
            assert_eq!(f.s.guest_received(), f.events(expected), "{case}");
        }
    }

    /// A preedit region makes committed surrounding text around the cursor
    /// the preedit; text the guest has not committed, or a region apart
    /// from the cursor, cannot be.
    #[tokio::test]
    async fn on_set_preedit_region_translates_correctly() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::set_surrounding_text(f.ti, "가나다", 6, 6).unwrap()])
            .await;
        f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
        assert_eq!(f.s.guest_received(), []);

        f.s.guest([v3::commit(f.ti)]).await;
        let mut expected = vec![f.surrounding_text("가나다", 6, 6)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.s.host_received(), expected);
        f.s.host([ext::set_preedit_region(f.host_ext, -6, 3)]).await;
        assert_eq!(f.s.guest_received(), []);

        f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
        let done = || V3::Done { serial: 2 };
        let expected = [delete(3, 0), preedit_at("나", 3, 3), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        // The region is the preedit now.
        f.s.host([f.commit("나")]).await;
        let expected = [no_preedit(), commit("나"), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));

        // A pending preedit cursor, or selection, places the region's.
        f.s.host([
            v1::preedit_cursor(f.host, 0),
            ext::set_preedit_region(f.host_ext, -3, 3),
        ])
        .await;
        let expected = [delete(3, 0), preedit_at("나", 0, 0), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
        f.s.host([
            v1::preedit_styling(f.host, 0, 3, 6),
            ext::set_preedit_region(f.host_ext, -6, 6),
        ])
        .await;
        let expected = [delete(6, 0), preedit_at("가나", 0, 3), done()];
        assert_eq!(f.s.guest_received(), f.events(expected));
    }

    /// Each commit_state carries the guest's commit count, which numbers
    /// every `done` until the next commit, however many host events with
    /// whatever serials it answers.
    #[tokio::test]
    async fn guest_commit_count_is_sent_to_v1_and_used_by_v3_done() {
        let mut f = Field::enabled().await;
        f.s.guest([v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), [f.commit_state(2)]);

        f.s.host([
            v1::preedit_string(f.host, 2942, "한", "").unwrap(),
            v1::preedit_string(f.host, 2944, "한글", "").unwrap(),
            v1::commit_string(f.host, 2945, "한글").unwrap(),
        ])
        .await;
        let done = || V3::Done { serial: 2 };
        assert_eq!(
            f.s.guest_received(),
            f.events([
                preedit("한"),
                done(),
                preedit("한글"),
                done(),
                no_preedit(),
                commit("한글"),
                done(),
            ])
        );
    }

    /// Events of a host text input that is not active belong to no guest
    /// generation: none reaches the guest or edits its next transaction.
    #[tokio::test]
    async fn inactive_host_ime_events_are_ignored() {
        let mut f = Field::disabled().await;
        f.s.host([
            f.preedit("가", ""),
            v1::delete_surrounding_text(f.host, -3, 3),
            f.commit("가"),
            v1::keysym(
                f.host,
                0,
                0,
                u32::from(b'a'),
                wl_keyboard::KEY_STATE_PRESSED,
                0,
            ),
        ])
        .await;
        assert_eq!(f.s.guest_received(), []);

        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.s.host_received(), f.enable_requests(1));
        f.s.host([f.commit("나")]).await;
        assert_eq!(f.s.guest_received(), f.events([commit("나"), DONE]));
    }
}
