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

//! Keys as the US layout's evdev keycodes.
//!
//! Exo sends a key the input method passes as a `wl_keyboard.key` with the
//! key's evdev keycode, and a client maps it back. Shift is not a key of its
//! own here: a key that needs it, such as an uppercase letter, comes with
//! the Shift modifier set around it.

use super::hangul::Key;

/// The first evdev keycode of each letter row, and the row's letters.
const ROWS: [(u32, &str); 3] = [(16, "qwertyuiop"), (30, "asdfghjkl"), (44, "zxcvbnm")];
const KEY_BACKSPACE: u32 = 14;
const KEY_ENTER: u32 = 28;
const KEY_SPACE: u32 = 57;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;

/// Shift's bit in `wl_keyboard.modifiers`, as the default xkb keymap
/// numbers its modifiers.
pub(super) const SHIFT: u32 = 1;

/// The evdev keycode of `key`, and whether it needs Shift.
///
/// # Panics
///
/// For a character other than a letter or Space, which the simulation does
/// not type.
pub(super) fn keycode(key: Key) -> (u32, bool) {
    let code = match key {
        Key::Backspace => KEY_BACKSPACE,
        Key::Enter => KEY_ENTER,
        Key::Left => KEY_LEFT,
        Key::Right => KEY_RIGHT,
        Key::Char(' ') => KEY_SPACE,
        Key::Char(c) => {
            let lower = c.to_ascii_lowercase();
            let code = ROWS.iter().find_map(|&(first, row)| {
                let column = row.find(lower)?;
                Some(first + u32::try_from(column).expect("a short row"))
            });
            let code = code.unwrap_or_else(|| panic!("no key types {c:?}"));
            return (code, c.is_ascii_uppercase());
        }
    };
    (code, false)
}

/// The key that `keycode` types, with Shift held or not; `None` for a
/// keycode no [`Key`] has.
pub(super) fn key(keycode: u32, shift: bool) -> Option<Key> {
    let key = match keycode {
        KEY_BACKSPACE => Key::Backspace,
        KEY_ENTER => Key::Enter,
        KEY_LEFT => Key::Left,
        KEY_RIGHT => Key::Right,
        KEY_SPACE => Key::Char(' '),
        code => {
            let letter = ROWS.iter().find_map(|&(first, row)| {
                let column = usize::try_from(code.checked_sub(first)?).ok()?;
                row.chars().nth(column)
            })?;
            Key::Char(if shift {
                letter.to_ascii_uppercase()
            } else {
                letter
            })
        }
    };
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::hangul::keys;

    #[test]
    fn every_key_the_simulation_types_maps_back() {
        let typed = keys("abcdefghijklmnopqrstuvwxyz QWERTOP⌫⏎←→");
        for key in typed {
            let (code, shift) = keycode(key);
            assert_eq!(super::key(code, shift), Some(key), "keycode {code}");
        }
        assert_eq!(keycode(Key::Char('q')), (16, false));
        assert_eq!(keycode(Key::Char('M')), (50, true));
        assert_eq!(super::key(1, false), None, "KEY_ESC");
    }
}
