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
//! A model plays one guest application at the wire level. It takes the v3
//! events the proxy sends, decoded, and what else the application learns
//! from the proxy: the keyboard focus, the keys the input method passed
//! (each mapped back to a [`Key`]) and the pointer presses and touches
//! (each mapped to a [`Target`]). It returns the requests the application
//! sends in reply, encoded. It holds one [`TextModel`] per field, applies
//! transactions through [`TextModel::apply_txn`] as far as the client's
//! rules allow, and logs every part of an event it drops, with the reason.
//!
//! A model also checks the v3 protocol as events arrive, and panics on a
//! violation, which fails the case:
//!
//! - every event comes from the model's text input;
//! - `enter` names a field's surface while none is entered, and `leave`
//!   the entered one;
//! - the other events arrive only between `enter` and `leave`;
//! - a `done` serial is at most the number of commits the model made;
//! - a delete fits the text ([`TextModel::delete_range`]).
//!
//! The models never hold a selection, as [`TextModel`] has none, so where
//! a client counts from the selection it counts from the cursor here.
//!
//! The facts each model implements are those of the design's section 2.5,
//! C1 to C9, each cited to the client's source where it is implemented.

pub(super) mod chromium;
pub(super) mod gtk;

use super::clock::Micros;
use super::editor::{Target, TextModel};
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
    /// What it dropped, and why.
    pub reason: DropReason,
}

/// What a client dropped, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum DropReason {
    /// Chromium: a delete while a composition is shown (C3).
    DeleteWhileComposing {
        /// The bytes before the cursor.
        before: u32,
        /// The bytes after the cursor.
        after: u32,
    },
    /// Chromium: a commit within 100 ms of a reset that ended a
    /// composition (C4).
    CommitInGuard(String),
    /// Chromium: a preedit within 100 ms of a reset that ended a
    /// composition (C4).
    PreeditInGuard(String),
    /// GTK: an event while no field has the keyboard focus, so that no
    /// input method context is current.
    NoFocusedField,
}

/// A `zwp_text_input_v3` client: one application with one text input on
/// one seat and a field on each of its surfaces.
pub(super) trait Client {
    /// The fields, in the order the model was given them.
    fn fields(&self) -> &[Field];

    /// What the client dropped, in order.
    fn drops(&self) -> &[Dropped];

    /// The keyboard focus moved to `surface`, or off the application's
    /// surfaces (`wl_keyboard.enter` and `leave`).
    fn keyboard_focus(&mut self, now: Micros, surface: Option<u32>) -> Vec<Outgoing>;

    /// A `zwp_text_input_v3` event from `sender`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing>;

    /// A key the input method passed reached the keyboard focus.
    fn key(&mut self, now: Micros, key: Key) -> Vec<Outgoing>;

    /// A primary button press or a touch-down at `at` on `surface`.
    fn press(&mut self, now: Micros, surface: u32, at: Target) -> Vec<Outgoing>;

    /// The release that ends the last press; `dragged` if the pointer
    /// moved past the drag threshold in between.
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
}

impl Protocol {
    fn new(text_input: u32) -> Self {
        Self {
            text_input,
            entered: None,
        }
    }

    /// Checks `event` from `sender` for a client with `fields` that made
    /// `commits` commits, and follows its `enter` and `leave`.
    ///
    /// # Panics
    ///
    /// On a violation of the protocol.
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
                if let Event::Done { serial } = *event {
                    assert!(
                        serial <= commits,
                        "protocol violation: done({serial}) after {commits} commits"
                    );
                }
            }
        }
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

/// The text of a v3 string event; empty if null.
///
/// # Panics
///
/// If it is not UTF-8, which the proxy never forwards.
fn utf8(text: Option<IndexedString>) -> String {
    match text {
        None => String::new(),
        Some(IndexedString::Utf8(text)) => text,
        Some(IndexedString::NotUtf8(bytes)) => {
            panic!("protocol violation: a string that is not UTF-8: {bytes:?}")
        }
    }
}

/// Checks that `delete_surrounding_text(before, after)` fits `model`.
///
/// # Panics
///
/// If it does not.
fn check_delete(model: &TextModel, before: u32, after: u32) {
    let fits = model.delete_range(before as usize, after as usize);
    assert!(
        fits.is_some(),
        "protocol violation: delete ({before}, {after}) does not fit {model:?}"
    );
}

/// Where a field draws its caret, as `x, y, width, height`: one 8-pixel
/// cell per character, the preedit included, so that every edit moves it.
fn caret_rect(model: &TextModel) -> [i32; 4] {
    let cells = model.text[..model.cursor].chars().count() + model.preedit.chars().count();
    let x = i32::try_from(cells * 8).expect("a short text");
    [x, 0, 1, 16]
}

/// `set_surrounding_text` of `model`'s text, with no selection.
fn surrounding_text(text_input: u32, model: &TextModel) -> Outgoing {
    let cursor = i32::try_from(model.cursor).expect("a short text");
    req::set_surrounding_text(text_input, &model.text, cursor, cursor)
        .expect("the surrounding text fits a message")
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
    //! What the models' tests share: the objects of a scripted case and
    //! a decoder for what a model sends.

    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;
    use crate::test_support::decoded;

    /// The text input.
    pub const TEXT_INPUT: u32 = 10;
    /// The surfaces of field 0 and field 1.
    pub const SURFACES: [u32; 2] = [20, 21];

    /// A field on surface `SURFACES[index]` holding `text`, the cursor at
    /// its end.
    pub fn field(index: usize, text: &str) -> Field {
        Field {
            surface: SURFACES[index],
            model: TextModel::new(text),
        }
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

    /// A `preedit_string` event of `text`, the cursor at its end.
    pub fn preedit(text: &str) -> Event {
        let end = i32::try_from(text.len()).expect("a short preedit");
        Event::PreeditString {
            text: Some(text.into()),
            cursor_begin: end,
            cursor_end: end,
        }
    }

    /// A `commit_string` event of `text`.
    pub fn commit(text: &str) -> Event {
        Event::CommitString {
            text: Some(text.to_owned()),
        }
    }

    /// A `delete_surrounding_text` event of `before` and `after` bytes.
    pub fn delete(before: u32, after: u32) -> Event {
        Event::DeleteSurroundingText {
            before_length: before,
            after_length: after,
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
}
