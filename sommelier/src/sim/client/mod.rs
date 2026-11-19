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

//! Models of the `zwp_text_input_v3` clients the simulation types into.
//!
//! A model plays one guest application at the wire level. It takes the
//! decoded v3 events the proxy sends and the rest of what the application
//! learns from it: the keyboard focus, the keys the input method passed
//! (as [`Key`]s) and presses (as [`Target`]s). It returns the requests the
//! application sends, encoded. It holds a [`TextModel`] per field, changes
//! it through [`TextModel::apply_txn`] as far as the client's rules allow,
//! and logs every part of an event it drops, with the reason.
//!
//! A model panics on output no correct proxy sends, which fails the case:
//!
//! - an event from another object, or naming a surface it lacks;
//! - a string that is not UTF-8;
//! - a `done` serial past the commits it made;
//! - a delete that fits none of the surrounding texts it sent since the
//!   text input last entered ([`TextModel::delete_range`]); a delete that
//!   fits one is applied as the client applies it, clamped to its current
//!   text;
//! - `enter` while entered, `leave` of a surface not entered, and other
//!   events outside `enter` and `leave`. The protocol does not forbid
//!   these, but the proxy never sends them: it sends `leave` before the
//!   next `enter` and composes only for an entered field.
//!
//! The models never hold a selection, as [`TextModel`] has none, so where
//! a client counts from the selection it counts from the cursor here.
//!
//! Each fact of the design's section 2.5 (C1 to C9) is cited to the
//! client's source where it is implemented.

pub(super) mod chromium;
pub(super) mod gtk;

use super::clock::Micros;
use super::editor::{Target, TextModel, Txn};
use super::hangul::Key;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{self, req, Event};
use crate::wire::{IndexedString, Outgoing};

/// A text field on a surface of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Field {
    /// The guest `wl_surface` the field is on.
    pub surface: u32,
    /// What the field shows.
    pub model: TextModel,
}

/// A part of an event that a client dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Dropped {
    /// When it dropped it.
    pub at: Micros,
    /// What it dropped.
    pub part: Part,
    /// Why.
    pub reason: DropReason,
}

/// A part of a v3 transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Part {
    /// `preedit_string`.
    Preedit(String),
    /// `commit_string`.
    Commit(String),
    /// `delete_surrounding_text`.
    Delete {
        /// The bytes before the cursor.
        before: u32,
        /// The bytes after the cursor.
        after: u32,
    },
}

/// Why a client dropped a part of a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    /// Chromium: a delete while a composition is shown (C3).
    DeleteWhileComposing,
    /// Chromium: a delete whose range splits a character of the text it
    /// predicts.
    DeleteSplitsCharacter,
    /// Chromium: a commit or preedit within 100 ms of a reset that ended
    /// a composition in the field's window (C4).
    InGuard,
    /// Chromium: the transaction's window has no keyboard focus, so its
    /// input method has no text input client.
    NoTextInputClient,
    /// Chromium: held when the text input reset or enabled.
    DiscardedByReset,
    /// Chromium: held when the text input disabled.
    DiscardedByDisable,
    /// GTK: no field has the keyboard focus, so no input method context
    /// is current.
    NoFocusedField,
}

/// How a press reached the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PressKind {
    /// A primary `wl_pointer.button` press.
    Button,
    /// A `wl_touch.down`.
    Touch,
}

/// A `zwp_text_input_v3` client: one application with one text input on
/// one seat and a field on each of its surfaces.
pub(super) trait Client {
    /// The fields, in the order the model was given them.
    fn fields(&self) -> &[Field];

    /// The field with the keyboard focus.
    fn focused_field(&self) -> Option<usize>;

    /// What the client dropped, in order.
    fn drops(&self) -> &[Dropped];

    /// The keyboard focus moved to `surface`, or off the application's
    /// surfaces (`wl_keyboard.enter` and `leave`).
    fn keyboard_focus(&mut self, now: Micros, surface: Option<u32>) -> Vec<Outgoing>;

    /// A `zwp_text_input_v3` event from `sender`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing>;

    /// A key the input method passed reached the keyboard focus.
    fn key(&mut self, now: Micros, key: Key) -> Vec<Outgoing>;

    /// A press at `at` on `surface`.
    fn press(&mut self, now: Micros, surface: u32, at: Target, kind: PressKind) -> Vec<Outgoing>;

    /// The release or lift that ends the last press; `dragged` if it moved
    /// past the drag threshold in between.
    fn release(&mut self, now: Micros, dragged: bool) -> Vec<Outgoing>;
}

/// The content type of every field: plain text.
const CONTENT_TYPE: (u32, u32) = (
    zwp_text_input_v3::CONTENT_HINT_NONE,
    zwp_text_input_v3::CONTENT_PURPOSE_NORMAL,
);

/// The v3 protocol as one client sees it, checked as events arrive.
#[derive(Debug)]
struct Protocol {
    /// The client's text input.
    text_input: u32,
    /// The field whose surface the text input entered.
    entered: Option<usize>,
    /// The surrounding texts the client sent since the text input last
    /// entered, as text and cursor. A delete the proxy measured against
    /// one can cross the client's next `enable` in flight.
    sent: Vec<(String, usize)>,
}

impl Protocol {
    fn new(text_input: u32) -> Self {
        Self {
            text_input,
            entered: None,
            sent: Vec::new(),
        }
    }

    /// Checks `event` from `sender` for a client with `fields` that made
    /// `commits` commits, and follows its `enter` and `leave`.
    ///
    /// # Panics
    ///
    /// On output no correct proxy sends (see the module's documentation).
    fn check(&mut self, fields: &[Field], sender: u32, event: &Event, commits: u32) {
        assert_eq!(
            sender, self.text_input,
            "protocol violation: {event:?} from {sender}, not the text input"
        );
        match *event {
            Event::Enter { surface } => {
                assert_eq!(
                    self.entered, None,
                    "protocol violation: enter while entered"
                );
                self.entered = Some(field_on(fields, surface));
                self.sent.clear();
            }
            Event::Leave { surface } => {
                let field = self.entered.take();
                assert_eq!(
                    field.map(|field| fields[field].surface),
                    Some(surface),
                    "protocol violation: leave of a surface not entered"
                );
            }
            _ => {
                assert!(
                    self.entered.is_some(),
                    "protocol violation: {event:?} outside enter and leave"
                );
                match *event {
                    // The count wraps, as Chromium's and GTK's do.
                    Event::Done { serial } => assert!(
                        commits.wrapping_sub(serial) <= u32::MAX / 2,
                        "protocol violation: done({serial}) after {commits} commits"
                    ),
                    Event::DeleteSurroundingText {
                        before_length,
                        after_length,
                    } => self.check_delete(before_length, after_length),
                    _ => {}
                }
            }
        }
    }

    /// `set_surrounding_text(text, cursor, cursor)`, recorded as sent.
    fn surrounding_text(&mut self, text: &str, cursor: usize) -> Outgoing {
        self.sent.push((text.to_owned(), cursor));
        let at = i32::try_from(cursor).expect("a short text");
        req::set_surrounding_text(self.text_input, text, at, at)
            .expect("the surrounding text fits a message")
    }

    /// Checks that `delete_surrounding_text(before, after)` fits a
    /// surrounding text the client sent since the text input last entered.
    /// The proxy measures each delete against the text the client last
    /// committed, and a delete may cross a newer one, or an `enable`, in
    /// flight.
    fn check_delete(&self, before: u32, after: u32) {
        let fits = self.sent.iter().any(|(text, cursor)| {
            let sent = TextModel {
                text: text.clone(),
                cursor: *cursor,
                preedit: String::new(),
            };
            sent.delete_range(before as usize, after as usize).is_some()
        });
        assert!(
            fits,
            "protocol violation: delete ({before}, {after}) fits no text sent: {:?}",
            self.sent
        );
    }
}

/// The field on `surface`.
///
/// # Panics
///
/// If no field is on it: the proxy named a surface the client lacks.
fn field_on(fields: &[Field], surface: u32) -> usize {
    fields
        .iter()
        .position(|field| field.surface == surface)
        .unwrap_or_else(|| panic!("protocol violation: surface {surface} has no field"))
}

/// The text of a v3 string event; `None` if null.
///
/// # Panics
///
/// If it is not UTF-8, which the proxy never forwards.
fn utf8(text: Option<IndexedString>) -> Option<String> {
    match text? {
        IndexedString::Utf8(text) => Some(text),
        IndexedString::NotUtf8(bytes) => {
            panic!("protocol violation: a string that is not UTF-8: {bytes:?}")
        }
    }
}

/// The characters in the bytes `before` the cursor and `after` it, each
/// range clamped to `text` and shrunk to whole characters.
fn chars_around(text: &str, cursor: usize, before: u32, after: u32) -> (usize, usize) {
    let mut start = cursor.saturating_sub(before as usize);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = cursor.saturating_add(after as usize).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let count = |range: &str| range.chars().count();
    (count(&text[start..cursor]), count(&text[cursor..end]))
}

/// Deletes `before` characters before the cursor and `after` after it,
/// as many as there are, keeping the preedit.
fn delete_chars(model: &mut TextModel, before: usize, after: usize) {
    let bytes = |chars: &mut dyn Iterator<Item = char>, n| -> usize {
        chars.take(n).map(char::len_utf8).sum()
    };
    let before = bytes(&mut model.text[..model.cursor].chars().rev(), before);
    let after = bytes(&mut model.text[model.cursor..].chars(), after);
    model.apply_txn(&Txn {
        delete: (before, after),
        preedit: model.preedit.clone(),
        ..Txn::default()
    });
}

/// Ends the composition `model` shows, keeping its text.
fn confirm(model: &mut TextModel) {
    let commit = std::mem::take(&mut model.preedit);
    model.apply_txn(&Txn {
        commit,
        ..Txn::default()
    });
}

/// Where a field draws its caret, as `x, y, width, height`: one 8-pixel
/// cell per character, the preedit included, so that every edit moves it.
fn caret_rect(model: &TextModel) -> [i32; 4] {
    let cells = model.text[..model.cursor].chars().count() + model.preedit.chars().count();
    let x = i32::try_from(cells * 8).expect("a short text");
    [x, 0, 1, 16]
}

/// `set_cursor_rectangle` of `rect`.
fn cursor_rectangle(text_input: u32, [x, y, width, height]: [i32; 4]) -> Outgoing {
    req::set_cursor_rectangle(text_input, x, y, width, height)
}

/// `set_content_type` of [`CONTENT_TYPE`].
fn content_type(text_input: u32) -> Outgoing {
    req::set_content_type(text_input, CONTENT_TYPE.0, CONTENT_TYPE.1)
}

#[cfg(test)]
mod script {
    //! What the models' tests share: the objects of a scripted case, the
    //! events they send and the requests they expect.

    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;
    use crate::test_support::decoded;

    /// The text input.
    pub const TEXT_INPUT: u32 = 10;
    /// The surfaces of field 0 and field 1.
    pub const SURFACES: [u32; 2] = [20, 21];

    /// Fields holding `texts`, field `i` on `SURFACES[i]`, each cursor at
    /// the end.
    pub fn fields(texts: &[&str]) -> Vec<Field> {
        texts
            .iter()
            .zip(SURFACES)
            .map(|(text, surface)| Field {
                surface,
                model: TextModel::new(text),
            })
            .collect()
    }

    /// The requests `client` sends for each of `events` at `now`, each
    /// checked to come from the text input.
    pub fn send(
        client: &mut impl Client,
        now: Micros,
        events: impl IntoIterator<Item = Event>,
    ) -> Vec<Request> {
        let out = events
            .into_iter()
            .flat_map(|event| client.event(now, TEXT_INPUT, event))
            .collect::<Vec<_>>();
        requests(out)
    }

    /// The requests in `out`, each checked to come from the text input.
    pub fn requests(out: Vec<Outgoing>) -> Vec<Request> {
        decoded(&out, Request::from_wire)
            .into_iter()
            .map(|(sender, request)| {
                assert_eq!(sender, TEXT_INPUT, "{request:?}");
                request
            })
            .collect()
    }

    /// `enter` of field `index`'s surface.
    pub fn enter(index: usize) -> Event {
        Event::Enter {
            surface: SURFACES[index],
        }
    }

    /// `leave` of field `index`'s surface.
    pub fn leave(index: usize) -> Event {
        Event::Leave {
            surface: SURFACES[index],
        }
    }

    /// `done(serial)`.
    pub fn done(serial: u32) -> Event {
        Event::Done { serial }
    }

    /// `preedit_string` of `text`, the cursor at its end.
    pub fn preedit(text: &str) -> Event {
        let end = i32::try_from(text.len()).expect("a short preedit");
        Event::PreeditString {
            text: Some(text.into()),
            cursor_begin: end,
            cursor_end: end,
        }
    }

    /// `commit_string` of `text`.
    pub fn commit(text: &str) -> Event {
        Event::CommitString {
            text: Some(text.to_owned()),
        }
    }

    /// `delete_surrounding_text` of `before` and `after` bytes.
    pub fn delete(before: u32, after: u32) -> Event {
        Event::DeleteSurroundingText {
            before_length: before,
            after_length: after,
        }
    }

    /// `set_surrounding_text` of `text`, the cursor at byte `cursor` and
    /// no selection.
    pub fn surrounding(text: &str, cursor: i32) -> Request {
        Request::SetSurroundingText {
            text: text.into(),
            cursor,
            anchor: cursor,
        }
    }

    /// `set_cursor_rectangle` of the caret after `cells` characters.
    pub fn caret(cells: i32) -> Request {
        Request::SetCursorRectangle {
            x: cells * 8,
            y: 0,
            width: 1,
            height: 16,
        }
    }

    /// `set_content_type` of plain text.
    pub fn plain_text() -> Request {
        Request::SetContentType {
            hint: CONTENT_TYPE.0,
            purpose: CONTENT_TYPE.1,
        }
    }

    /// A model holding `text`, the cursor at `cursor` and `preedit` shown.
    pub fn model(text: &str, cursor: usize, preedit: &str) -> TextModel {
        TextModel {
            text: text.to_owned(),
            cursor,
            preedit: preedit.to_owned(),
        }
    }

    /// A drop at `at`.
    pub fn dropped(at: Micros, part: Part, reason: DropReason) -> Dropped {
        Dropped { at, part, reason }
    }

    #[test]
    fn a_delete_counts_whole_characters_and_clamps_to_the_text() {
        // "a가b" with the cursor after 가: one byte before splits it.
        assert_eq!(chars_around("a가b", 4, 1, 1), (0, 1));
        assert_eq!(chars_around("a가b", 4, 3, 9), (1, 1));
        assert_eq!(chars_around("a가b", 4, 9, 0), (2, 0));
        let mut field = model("a가b", 4, "ㄴ");
        delete_chars(&mut field, 1, 5);
        assert_eq!(field, model("a", 1, "ㄴ"));
    }
}
