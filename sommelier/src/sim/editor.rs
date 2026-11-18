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

//! A text field's content, and the reference editor that defines the text
//! the user intended.
//!
//! [`TextModel`] is what a `zwp_text_input_v3` client holds for a field.
//! The simulated clients and the reference editor share it, so they apply
//! a transaction and an unconsumed key the same way. [`Reference`] types
//! the user's actions into one model per field through an ideal input
//! method, which handles each key at once; the simulation's oracle compares
//! what the clients end with against it.

use std::ops::Range;

use super::hangul::{Composer, Key};

/// A text field's content. Offsets are UTF-8 byte offsets into `text`, as
/// `zwp_text_input_v3` counts them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct TextModel {
    /// The committed text.
    pub text: String,
    /// The cursor. Nothing generates a selection, so the model has none.
    pub cursor: usize,
    /// The composition, shown at the cursor and not part of `text`; empty
    /// if none.
    pub preedit: String,
}

/// The events of one `zwp_text_input_v3` transaction, which a client
/// applies together at `done`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Txn {
    /// `delete_surrounding_text`: the bytes before and after the cursor.
    pub delete: (usize, usize),
    /// `commit_string`; empty if none.
    pub commit: String,
    /// `preedit_string`; empty if none, which clears the preedit.
    pub preedit: String,
}

/// A point in a field that means the same text to the user and to a
/// client that lags behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Target {
    /// Before the first character.
    Start,
    /// After the last character.
    End,
    /// Just before the first occurrence of this digit (0 to 9), a marker
    /// seeded into the field's text; after the last character if the text
    /// has none. A generator must not type digits, so that each marker
    /// stays one place.
    BeforeMarker(u8),
}

/// What the user does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UserAction {
    /// Presses and releases a key, which goes to the focused field.
    Key(Key),
    /// Presses and releases the pointer at a point of a field's surface.
    Click {
        /// The field.
        field: usize,
        /// The point.
        at: Target,
    },
    /// Touches a point of a field's surface and lifts the finger.
    Tap {
        /// The field.
        field: usize,
        /// The point.
        at: Target,
    },
    /// Moves the keyboard focus to a field's surface, as Alt-Tab does.
    Focus(usize),
    /// Waits for the system to settle before the next action.
    Settle,
}

impl TextModel {
    /// A field holding `text`, with the cursor at its end.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
            preedit: String::new(),
        }
    }

    /// Applies a transaction in the order `zwp_text_input_v3.done` gives:
    /// the preedit goes, the surrounding text is deleted, the commit is
    /// inserted at the cursor, and the new preedit is shown there.
    ///
    /// # Panics
    ///
    /// If the delete does not fit the text: a client model checks it with
    /// [`TextModel::delete_range`] first.
    pub fn apply_txn(&mut self, txn: &Txn) {
        let (before, after) = txn.delete;
        let deleted = self
            .delete_range(before, after)
            .expect("the client checked the delete with delete_range");
        self.replace(deleted, "");
        self.replace(self.cursor..self.cursor, &txn.commit);
        self.preedit.clone_from(&txn.preedit);
    }

    /// The bytes `delete_surrounding_text(before, after)` names: `before`
    /// bytes before the cursor and `after` bytes after it. `None` if they
    /// reach past the text or split a character.
    pub fn delete_range(&self, before: usize, after: usize) -> Option<Range<usize>> {
        let range = self.cursor.checked_sub(before)?..self.cursor.checked_add(after)?;
        let fits = self.text.is_char_boundary(range.start) && self.text.is_char_boundary(range.end);
        fits.then_some(range)
    }

    /// Applies a key that the input method passed to the application, as a
    /// text field does: a character is inserted at the cursor, Enter inserts
    /// a line break, Backspace deletes the character before the cursor, and
    /// an arrow moves the cursor by one character. The preedit stays.
    pub fn insert_key(&mut self, key: Key) {
        let at = self.cursor;
        match key {
            Key::Char(c) => self.replace(at..at, c.encode_utf8(&mut [0; 4])),
            Key::Enter => self.replace(at..at, "\n"),
            Key::Backspace => self.replace(self.char_before()..at, ""),
            Key::Left => self.cursor = self.char_before(),
            Key::Right => self.cursor = self.char_after(),
        }
    }

    /// Moves the cursor to `target`. The preedit moves with the cursor.
    pub fn click(&mut self, target: Target) {
        let at = match target {
            Target::Start => Some(0),
            Target::End => None,
            Target::BeforeMarker(digit) => {
                assert!(digit < 10, "a marker is a digit, not {digit}");
                self.text.find(char::from(b'0' + digit))
            }
        };
        self.cursor = at.unwrap_or(self.text.len());
    }

    /// Where the character before the cursor starts; the cursor at the
    /// start of the text.
    fn char_before(&self) -> usize {
        let c = self.text[..self.cursor].chars().next_back();
        self.cursor - c.map_or(0, char::len_utf8)
    }

    /// Where the character after the cursor ends; the cursor at the end of
    /// the text.
    fn char_after(&self) -> usize {
        let c = self.text[self.cursor..].chars().next();
        self.cursor + c.map_or(0, char::len_utf8)
    }

    /// Replaces `range` with `text` and puts the cursor after it.
    fn replace(&mut self, range: Range<usize>, text: &str) {
        let at = range.start + text.len();
        self.text.replace_range(range, text);
        self.cursor = at;
    }
}

/// The reference editor: what a correct system shows after the user's
/// actions, typed through an ideal input method that handles each key at
/// once.
///
/// - A key goes to the focused field. The composer's commit is inserted,
///   then the key itself if the composer passed it, and the composition is
///   shown as the field's preedit.
/// - A click or a tap commits the composition where it is, then focuses
///   the field it lands in and moves that field's cursor, as mutter and
///   KWin do on a press.
/// - A focus move to another field commits the composition in the field it
///   leaves, as Exo's confirm on detach intends. A focus move to the
///   focused field changes nothing.
///
/// Field 0 has the focus at the start.
///
/// This is the user's intent only for actions the system can follow in
/// order: each `Click` and `Tap` must have a `Settle` before and after it,
/// and each `Focus` a `Settle` before it. Otherwise Exo can hand a key to
/// the engine before the press's reset, and the proxy drops that key's
/// late result by design (INVARIANTS, "A pointer press ends the
/// composition on its surface").
#[derive(Clone, Debug)]
pub(super) struct Reference {
    /// One model per field.
    fields: Vec<TextModel>,
    /// The field with the keyboard focus.
    focus: usize,
    /// The input method's composition, which is in the focused field.
    composer: Composer,
}

impl Reference {
    /// An editor with these fields, field 0 focused.
    pub fn new(fields: Vec<TextModel>) -> Self {
        assert!(!fields.is_empty(), "the focus needs a field");
        Self {
            fields,
            focus: 0,
            composer: Composer::default(),
        }
    }

    /// Applies one user action.
    pub fn apply(&mut self, action: UserAction) {
        match action {
            UserAction::Key(key) => {
                let out = self.composer.key(key);
                let field = &mut self.fields[self.focus];
                field.apply_txn(&Txn {
                    commit: out.commit,
                    preedit: out.preedit,
                    ..Txn::default()
                });
                if !out.consumed {
                    field.insert_key(key);
                }
            }
            UserAction::Click { field, at } | UserAction::Tap { field, at } => {
                self.commit_composition();
                self.focus = field;
                self.fields[field].click(at);
            }
            UserAction::Focus(field) if field != self.focus => {
                self.commit_composition();
                self.focus = field;
            }
            UserAction::Focus(_) | UserAction::Settle => {}
        }
    }

    /// The fields' models.
    pub fn fields(&self) -> &[TextModel] {
        &self.fields
    }

    /// The field with the keyboard focus.
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Commits the composition, if any, into the focused field.
    fn commit_composition(&mut self) {
        let commit = self.composer.confirm();
        self.fields[self.focus].apply_txn(&Txn {
            commit,
            ..Txn::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::hangul::keys;

    /// A model holding `text`, with the cursor at `cursor`.
    fn model(text: &str, cursor: usize, preedit: &str) -> TextModel {
        TextModel {
            text: text.to_owned(),
            cursor,
            preedit: preedit.to_owned(),
        }
    }

    /// The fields after the reference editor applies `actions` to fields
    /// holding `texts`.
    fn intended(texts: &[&str], actions: &[UserAction]) -> Vec<TextModel> {
        reference(texts, actions).fields().to_vec()
    }

    /// The reference editor after it applies `actions` to fields holding
    /// `texts`.
    fn reference(texts: &[&str], actions: &[UserAction]) -> Reference {
        let mut editor = Reference::new(texts.iter().map(|t| TextModel::new(t)).collect());
        for &action in actions {
            editor.apply(action);
        }
        editor
    }

    /// The actions that type `script` (see [`keys`]).
    fn typing(script: &str) -> Vec<UserAction> {
        keys(script).into_iter().map(UserAction::Key).collect()
    }

    #[test]
    fn a_transaction_deletes_then_commits_then_shows_the_preedit() {
        let mut field = model("abcdef", 3, "x");
        field.apply_txn(&Txn {
            delete: (1, 1),
            commit: "Z".to_owned(),
            preedit: "y".to_owned(),
        });
        assert_eq!(field, model("abZef", 3, "y"));
    }

    #[test]
    fn a_transaction_counts_utf8_bytes() {
        let mut field = model("가나다", 6, "");
        field.apply_txn(&Txn {
            delete: (3, 0),
            commit: "라".to_owned(),
            preedit: "ㅁ".to_owned(),
        });
        assert_eq!(field, model("가라다", 6, "ㅁ"));
    }

    #[test]
    fn a_transaction_without_a_preedit_clears_it() {
        let mut field = model("ab", 1, "가");
        field.apply_txn(&Txn::default());
        assert_eq!(field, model("ab", 1, ""));
    }

    #[test]
    fn a_delete_must_fit_the_text_on_char_boundaries() {
        let field = model("a가b", 4, "");
        let table = [
            ((0, 0), Some(4..4)),
            ((3, 1), Some(1..5)),
            ((4, 1), Some(0..5)),
            // Past the start or the end.
            ((5, 0), None),
            ((0, 2), None),
            ((usize::MAX, usize::MAX), None),
            // Into 가, which is 3 bytes.
            ((1, 0), None),
            ((2, 1), None),
        ];
        for ((before, after), range) in table {
            assert_eq!(
                field.delete_range(before, after),
                range,
                "({before}, {after})"
            );
        }
    }

    #[test]
    #[should_panic(expected = "delete_range")]
    fn a_transaction_whose_delete_does_not_fit_panics() {
        model("가", 3, "").apply_txn(&Txn {
            delete: (1, 0),
            ..Txn::default()
        });
    }

    #[test]
    fn a_passed_key_edits_like_a_text_field() {
        let table = [
            (model("a가", 4, ""), Key::Char('x'), model("a가x", 5, "")),
            (model("a가", 4, ""), Key::Enter, model("a가\n", 5, "")),
            (model("a가", 4, ""), Key::Backspace, model("a", 1, "")),
            (model("a가", 0, ""), Key::Backspace, model("a가", 0, "")),
            (model("a가", 4, ""), Key::Left, model("a가", 1, "")),
            (model("a가", 0, ""), Key::Left, model("a가", 0, "")),
            (model("a가", 1, ""), Key::Right, model("a가", 4, "")),
            (model("a가", 4, ""), Key::Right, model("a가", 4, "")),
        ];
        for (mut field, key, after) in table {
            let before = field.clone();
            field.insert_key(key);
            assert_eq!(field, after, "{key:?} on {before:?}");
        }
    }

    #[test]
    fn a_click_moves_the_cursor_to_its_target() {
        let field = model("0ab1cd2", 2, "가");
        let table = [
            (Target::Start, 0),
            (Target::End, 7),
            (Target::BeforeMarker(0), 0),
            (Target::BeforeMarker(1), 3),
            (Target::BeforeMarker(2), 6),
            // A marker the text lacks resolves to the end.
            (Target::BeforeMarker(3), 7),
        ];
        for (target, cursor) in table {
            let mut clicked = field.clone();
            clicked.click(target);
            assert_eq!(clicked, model("0ab1cd2", cursor, "가"), "{target:?}");
        }
    }

    #[test]
    #[should_panic(expected = "a marker is a digit")]
    fn a_marker_that_is_not_a_digit_panics() {
        model("0ab1cd2", 0, "").click(Target::BeforeMarker(10));
    }

    #[test]
    fn keys_commit_and_compose_at_the_cursor_of_the_focused_field() {
        assert_eq!(
            intended(&["0ab1cd2", "x"], &typing("rkskdk")),
            [model("0ab1cd2가나", 13, "아"), model("x", 1, "")]
        );
        assert_eq!(
            intended(&[""], &typing("dkssudgktpdy.")),
            [model("안녕하세요.", 16, "")]
        );
    }

    #[test]
    fn a_passed_key_lands_after_the_commit() {
        let table = [
            ("rk ", model("가 ", 4, "")),
            ("rk⏎", model("가\n", 4, "")),
            ("rk←", model("가", 0, "")),
            ("rk←sk", model("가", 0, "나")),
            ("rk←sk→", model("나가", 6, "")),
        ];
        for (script, field) in table {
            assert_eq!(intended(&[""], &typing(script)), [field], "{script:?}");
        }
    }

    #[test]
    fn backspace_takes_back_jamo_then_deletes_syllables() {
        let table = [
            ("rkrk⌫", model("0가", 4, "ㄱ")),
            ("rkrk⌫⌫", model("0가", 4, "")),
            ("rkrk⌫⌫⌫", model("0", 1, "")),
            ("rkrk⌫⌫⌫⌫", model("", 0, "")),
            ("rkrk⌫⌫⌫⌫⌫", model("", 0, "")),
        ];
        for (script, field) in table {
            assert_eq!(intended(&["0"], &typing(script)), [field], "{script:?}");
        }
    }

    #[test]
    fn a_click_or_tap_commits_then_moves_the_cursor() {
        let target = Target::BeforeMarker(1);
        for press in [
            UserAction::Click {
                field: 0,
                at: target,
            },
            UserAction::Tap {
                field: 0,
                at: target,
            },
        ] {
            let actions = [typing("rk"), vec![press], typing("k")].concat();
            // The next jamo starts afresh at the target: it does not join 가.
            assert_eq!(
                intended(&["0ab1cd2"], &actions),
                [model("0ab1cd2가", 3, "ㅏ")],
                "{press:?}"
            );
        }
    }

    #[test]
    fn a_click_on_another_field_commits_in_the_focused_one() {
        let click = UserAction::Click {
            field: 1,
            at: Target::Start,
        };
        let actions = [typing("rk"), vec![click], typing("sk")].concat();
        let editor = reference(&["0", "1"], &actions);
        assert_eq!(editor.fields(), [model("0가", 4, ""), model("1", 0, "나")]);
        assert_eq!(editor.focus(), 1);
    }

    #[test]
    fn a_focus_move_commits_in_the_field_it_leaves() {
        let actions = [typing("rk"), vec![UserAction::Focus(1)], typing("sk")].concat();
        let editor = reference(&["0", "1"], &actions);
        assert_eq!(editor.fields(), [model("0가", 4, ""), model("1", 1, "나")]);
        assert_eq!(editor.focus(), 1);
    }

    #[test]
    fn focusing_the_focused_field_or_settling_keeps_the_composition() {
        for pause in [UserAction::Focus(0), UserAction::Settle] {
            let actions = [typing("rk"), vec![pause], typing("r")].concat();
            let editor = reference(&["0", "1"], &actions);
            let fields = [model("0", 1, "각"), model("1", 1, "")];
            assert_eq!(editor.fields(), fields, "{pause:?}");
            assert_eq!(editor.focus(), 0, "{pause:?}");
        }
    }
}
