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

//! What Exo makes of the requests the proxy sends, and what it sends back.
//!
//! Exo, the ChromeOS compositor, applies a text-input request against state
//! the proxy cannot see: it drops a content type while no input method is
//! attached, and spends a pending surrounding-text support on the next
//! content type. A generic compositor exposes none of that, so the DST
//! holds the guest's committed fields to this model of each host
//! `zwp_text_input_v1`, and the fuzzer's compositor plays the rest of the
//! host itself around it.
//!
//! The simulation also gives the model the seat's keyboard, the user's
//! keys and the answers of the input method engine ([`super::engine`]), and
//! takes what the model hands the engine. Between the engine and the text
//! input it serves stands `InputMethodAsh`
//! (`ui/base/ime/ash/input_method_ash.cc`, IMA), whose rules the model
//! follows: it buffers the engine's text for a key until the engine's
//! callback for it, sends a handled press before that text and an
//! unhandled one after, confirms the composition before an unhandled press
//! and when the text input it serves changes or detaches, and drops the
//! buffer on a reset. A key goes raw while IMA serves no text input. The
//! extension's version is the one the proxy binds. Answers that belong to
//! the display, a sync's `done` and the `delete_id` of a destroyed object,
//! are left to whoever plays it, as is the keyboard's focus in the fuzzer.
//! The model is a test oracle, not a reimplementation: it keeps only the
//! state those read.
//!
//! Each rule cites Chromium's `components/exo/text_input.cc` (`TextInput`)
//! or `components/exo/wayland/zwp_text_input_manager.cc` (the request
//! handlers) at Chromium revision 137f56b0d966 (2026-11-17), IMA's rules
//! cite the functions of `input_method_ash.cc` at Chromium `main`
//! (2026-11-18), the tracker's rules cite the functions of
//! `ui/base/ime/surrounding_text_tracker.cc` at Chromium `main`
//! (2026-11-19; last changed in a89e336612fb), and the key path cites
//! `components/exo/keyboard.cc` (`Keyboard::OnKeyEvent`) and
//! `components/exo/wayland/serial_tracker.cc`.
//! Exo has one seat, and so does the model. Not modelled:
//! - Exo's forced `TEXT_INPUT_TYPE_NULL` for a field without surrounding
//!   text under an xkb input method (`ShouldUseNullInputType`), which
//!   depends on the user's input method, not on the requests;
//! - the fallbacks for unknown types, modes and flags, and for surrounding
//!   text whose offsets do not convert to UTF-16, none of which the proxy
//!   sends;
//! - `zwp_text_input_v1.modifiers_map`, which Exo sends on every
//!   `activate` (`zwp_text_input_manager.cc` 569-577) and the proxy
//!   ignores;
//! - the virtual keyboard, focus reason, grammar and autocorrect state;
//! - IMA's character composer and password fields, the engine's focus and
//!   blur on a type change, and a single-character commit sent as a key
//!   (`NeedInsertChar`), which the Korean engine never makes;
//! - `preedit_styling` and key repeats, which Exo never sends a client;
//!   the keys held at a `wl_keyboard.enter`, which the model never lists;
//!   and the release of a keyboard or its extension. The keymap is the
//!   display's ([`super::display`]).
//!
//! The engine gets the autorepeats of a held key ([`Exo::autorepeat`]),
//! and IMA confirms before each one it did not handle (E13).
//!
//! IMA drops a handled release: it reaches the extension as a `peek_key`
//! only, as its press did, and no `wl_keyboard.key` follows.
//!
//! Where the model is stricter than Exo: it keeps the surrounding text's
//! cursor and anchor as sent, where Exo keeps the range between them, and
//! the cursor rectangle as sent, where `gfx::Rect` clamps a negative width
//! or height to 0.

use super::clock::Micros;
use super::engine::{EngineIn, EngineOut, Request, TextAction};
use super::hangul::Key;
use super::keymap;
use crate::protocols::keyboard_extension_unstable_v1::{
    zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
};
use crate::protocols::text_input_extension_unstable_v1::{
    zcr_extended_text_input_v1 as extended, zcr_text_input_extension_v1,
};
use crate::protocols::text_input_unstable_v1::{zwp_text_input_manager_v1, zwp_text_input_v1};
use crate::protocols::wayland::{wl_keyboard, wl_registry, wl_seat};
use crate::protocols::AnyRequest;
use crate::wire::{IndexedString, Outgoing};
use std::collections::{BTreeMap, VecDeque};
use std::ops::Range;

/// What one Exo `TextInput` holds for its field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Field {
    /// Whether an input method is attached (`input_method_` is set).
    pub attached: bool,
    pub content_type: ContentType,
    pub surrounding_text_supported: bool,
    /// The last `set_surrounding_text`: text, cursor and anchor.
    pub surrounding_text: (String, u32, u32),
    pub cursor_rectangle: (i32, i32, i32, i32),
}

/// A content type as `SetTypeModeFlags` takes it, in the extension's wire
/// values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContentType {
    pub input_type: u32,
    pub input_mode: u32,
    pub flags: u32,
    pub learning: bool,
    pub inline_composition: bool,
}

/// The content type `text_input_set_content_type` makes of a v1 `hint`
/// and `purpose`, with each `ui::` value written as the extension's value
/// of the same name.
pub(super) fn v1_content_type(hint: u32, purpose: u32) -> ContentType {
    use zwp_text_input_v1 as v1;
    let hint_flags = [
        (
            v1::CONTENT_HINT_AUTO_COMPLETION,
            extended::INPUT_FLAGS_AUTOCOMPLETE_ON,
        ),
        (
            v1::CONTENT_HINT_AUTO_CORRECTION,
            extended::INPUT_FLAGS_AUTOCORRECT_ON,
        ),
        (
            v1::CONTENT_HINT_AUTO_CAPITALIZATION,
            extended::INPUT_FLAGS_AUTOCAPITALIZE_SENTENCES,
        ),
        (
            v1::CONTENT_HINT_LOWERCASE,
            extended::INPUT_FLAGS_AUTOCAPITALIZE_NONE,
        ),
        (
            v1::CONTENT_HINT_UPPERCASE,
            extended::INPUT_FLAGS_AUTOCAPITALIZE_CHARACTERS,
        ),
        (
            v1::CONTENT_HINT_TITLECASE,
            extended::INPUT_FLAGS_AUTOCAPITALIZE_WORDS,
        ),
        (
            v1::CONTENT_HINT_HIDDEN_TEXT,
            extended::INPUT_FLAGS_AUTOCOMPLETE_OFF | extended::INPUT_FLAGS_AUTOCORRECT_OFF,
        ),
    ];
    let flags = hint_flags
        .iter()
        .filter(|&&(bit, _)| hint & bit != 0)
        .fold(extended::INPUT_FLAGS_NONE, |flags, &(_, flag)| flags | flag);
    let (input_type, input_mode) = match purpose {
        v1::CONTENT_PURPOSE_DIGITS => (extended::INPUT_TYPE_NUMBER, extended::INPUT_MODE_DECIMAL),
        v1::CONTENT_PURPOSE_NUMBER => (extended::INPUT_TYPE_NUMBER, extended::INPUT_MODE_NUMERIC),
        v1::CONTENT_PURPOSE_PHONE => (extended::INPUT_TYPE_TELEPHONE, extended::INPUT_MODE_TEL),
        v1::CONTENT_PURPOSE_URL => (extended::INPUT_TYPE_URL, extended::INPUT_MODE_URL),
        v1::CONTENT_PURPOSE_EMAIL => (extended::INPUT_TYPE_EMAIL, extended::INPUT_MODE_EMAIL),
        v1::CONTENT_PURPOSE_PASSWORD => {
            (extended::INPUT_TYPE_PASSWORD, extended::INPUT_MODE_DEFAULT)
        }
        v1::CONTENT_PURPOSE_DATE => (extended::INPUT_TYPE_DATE, extended::INPUT_MODE_DEFAULT),
        v1::CONTENT_PURPOSE_TIME => (extended::INPUT_TYPE_TIME, extended::INPUT_MODE_DEFAULT),
        v1::CONTENT_PURPOSE_DATETIME => {
            (extended::INPUT_TYPE_DATE_TIME, extended::INPUT_MODE_DEFAULT)
        }
        _ => (extended::INPUT_TYPE_TEXT, extended::INPUT_MODE_DEFAULT),
    };
    ContentType {
        input_type,
        input_mode,
        flags,
        learning: hint & v1::CONTENT_HINT_SENSITIVE_DATA == 0
            && purpose != v1::CONTENT_PURPOSE_PASSWORD,
        inline_composition: true,
    }
}

/// One host `zwp_text_input_v1`.
struct TextInput {
    /// `surface_`: the surface of the last `activate`, until a `deactivate`
    /// or the surface loses the focus or is destroyed.
    surface: Option<u32>,
    field: Field,
    /// The delegate's `pending_surrounding_text_supported_`.
    pending_support: bool,
    /// What Exo predicts the field holds (E4). [`Field::surrounding_text`]
    /// is the last report as sent, for the fuzzer to compare.
    text: Tracker,
    /// When it registered as the seat's observer: at its first `activate`
    /// after a `Deactivate`, which unregisters it (E12).
    registered: Option<u64>,
}

/// Exo's text before a `set_preedit_region` or a
/// `delete_surrounding_text` it sent, in which the oracle measures the
/// proxy's translation of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RegionTruth {
    /// Its tracker's predicted text and cursor when it sent the event, with
    /// its selection and the composition it held there, if any.
    pub text: String,
    pub cursor: usize,
    pub selection: Selection,
    pub composition: Option<Range<usize>>,
}

impl RegionTruth {
    /// What `tracker` predicts before an event, which composes nothing.
    fn of(tracker: &Tracker) -> Self {
        let predicted = &tracker.predicted;
        Self {
            text: predicted.text.clone(),
            cursor: predicted.cursor(),
            selection: predicted.selection,
            composition: predicted.composition.clone(),
        }
    }
}

/// What became of a region the engine asked for, which the engine learns
/// from the field (D6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RegionOutcome {
    /// Exo composed this text.
    Composed(String),
    /// IMA refused the range at once, as it lay past the text, and the
    /// field composes what it did.
    Refused,
}

/// A `gfx::Range` selection in bytes, which keeps its direction: `end` is
/// where the cursor is, and it may lie before `start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Selection {
    pub start: usize,
    pub end: usize,
}

impl Selection {
    /// An empty selection, a cursor at `at`.
    fn caret(at: usize) -> Self {
        Self { start: at, end: at }
    }

    /// The bytes it covers, whatever its direction (`GetMin`, `GetMax`).
    fn range(self) -> Range<usize> {
        self.start.min(self.end)..self.start.max(self.end)
    }
}

/// What `SurroundingTextTracker` (`ui/base/ime/surrounding_text_tracker.cc`)
/// predicts a field holds: a `State`, whose `utf16_offset` is 0, as the
/// proxy never sends an offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Predicted {
    /// The text, with the composition in it.
    pub text: String,
    pub selection: Selection,
    /// Where the composition is in `text`, if there is one.
    pub composition: Option<Range<usize>>,
}

impl Predicted {
    fn composition(&self) -> &str {
        self.composition
            .clone()
            .map_or("", |range| &self.text[range])
    }

    /// Ends the composition before an edit of `range`, as `OnInsertText`
    /// and `OnExtendSelectionAndDelete` do: where the two meet
    /// ([`intersects`]), the edit takes the composition too; otherwise the
    /// composition goes, and `range` moves with the text after it. Returns
    /// the range to edit.
    fn take_composition(&mut self, range: Range<usize>) -> Range<usize> {
        let Some(composition) = self.composition.take() else {
            return range;
        };
        if intersects(&composition, &range) {
            return range.start.min(composition.start)..range.end.max(composition.end);
        }
        self.text.replace_range(composition.clone(), "");
        match range.start > composition.start {
            true => range.start - composition.len()..range.end - composition.len(),
            false => range,
        }
    }

    /// Where the cursor is: the selection's `end`, from which Exo measures
    /// a preedit region (`zwp_text_input_manager.cc`
    /// `SetCompositionFromExistingText`).
    fn cursor(&self) -> usize {
        self.selection.end
    }
}

/// `gfx::Range::Intersects` of forward ranges: they overlap, or one holds
/// the other, where an empty range at a non-empty one's end is not held.
fn intersects(a: &Range<usize>, b: &Range<usize>) -> bool {
    let holds = |outer: &Range<usize>, inner: &Range<usize>| {
        outer.start <= inner.start
            && inner.end <= outer.end
            && (inner.end != outer.end || inner.is_empty() == outer.is_empty())
    };
    a.start.max(b.start) < a.end.min(b.end) || holds(a, b) || holds(b, a)
}

/// `SurroundingTextTracker`: the prediction, and the states that the input
/// method's edits since the last report led to, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Tracker {
    pub predicted: Predicted,
    pub expected: Vec<Predicted>,
    /// A report it could not match dropped the composition, and nothing
    /// composed since (E4).
    pub dropped: bool,
    /// While the composition is a region composed from existing text for a
    /// text input without an extension, which its client never sees, and
    /// nothing has changed it since: the composition the client still
    /// shows, the one that stood before the first such region, since
    /// nothing tells the client to drop it.
    pub hidden: Option<String>,
}

impl Tracker {
    /// `Update`, for a report of the field: one that names a state an edit
    /// led to is the client catching up, which keeps the prediction, since
    /// the edits after that state lead to it; any other replaces the
    /// prediction, and with it the composition.
    ///
    /// The report's selection is the range between its cursor and anchor,
    /// with no direction (`SetSurroundingTextImpl`'s `minmax`), which only
    /// a forward state's selection equals.
    fn update(&mut self, text: String, cursor: usize, anchor: usize) {
        let selection = Selection {
            start: cursor.min(anchor),
            end: cursor.max(anchor),
        };
        let named = |state: &Predicted| state.text == text && state.selection == selection;
        match self.expected.iter().position(named) {
            Some(state) => drop(self.expected.drain(..state)),
            None => {
                self.dropped |= self.predicted.composition.is_some();
                self.hidden = None;
                self.predicted = Predicted {
                    text,
                    selection,
                    composition: None,
                };
                self.expected = vec![self.predicted.clone()];
            }
        }
    }

    /// `OnSetCompositionText`: `text` replaces the composition, or else
    /// the selection, even with no text, and the selection becomes
    /// `selection`, which is within `text` and keeps its direction.
    fn set_composition(&mut self, text: &str, selection: Selection) {
        self.dropped = false;
        self.hidden = None;
        let predicted = &mut self.predicted;
        let replaced = predicted
            .composition
            .take()
            .unwrap_or(predicted.selection.range());
        let start = replaced.start;
        predicted.text.replace_range(replaced, text);
        predicted.selection = Selection {
            start: start + selection.start,
            end: start + selection.end,
        };
        predicted.composition = (!text.is_empty()).then_some(start..start + text.len());
        self.expected.push(self.predicted.clone());
    }

    /// `OnExtendSelectionAndDelete`: the text in `range`, where it lies in
    /// the prediction, goes, with a composition it meets; a composition
    /// apart from it goes too. The cursor, with no selection, goes to the
    /// start of what went.
    fn delete(&mut self, range: Range<usize>) {
        let predicted = &mut self.predicted;
        let text = &predicted.text;
        if range.end > text.len()
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            return;
        }
        let range = predicted.take_composition(range);
        predicted.text.replace_range(range.clone(), "");
        predicted.selection = Selection::caret(range.start);
        self.hidden = None;
        self.expected.push(self.predicted.clone());
    }

    /// `OnSetCompositionFromExistingText`: the text in `range` becomes the
    /// composition, which the client never sees if `hidden`; returns it.
    fn compose_existing(&mut self, range: Range<usize>, hidden: bool) -> String {
        self.dropped = false;
        let shown = self.hidden.take();
        self.hidden = (hidden && !range.is_empty())
            .then(|| shown.unwrap_or_else(|| self.predicted.composition().to_owned()));
        let predicted = &mut self.predicted;
        let composed = predicted.text[range.clone()].to_owned();
        predicted.composition = (!range.is_empty()).then_some(range);
        self.expected.push(self.predicted.clone());
        composed
    }

    /// `OnInsertText`, with the cursor after the text, as
    /// `TextInput::InsertText` always asks: `text` replaces the selection,
    /// and the composition as well where they meet; a composition apart
    /// from the selection goes.
    fn insert(&mut self, text: &str) {
        self.hidden = None;
        let range = self
            .predicted
            .take_composition(self.predicted.selection.range());
        self.predicted.text.replace_range(range.clone(), text);
        self.predicted.selection = Selection::caret(range.start + text.len());
        self.expected.push(self.predicted.clone());
    }

    /// `OnConfirmCompositionText`: the composition stays, as text, and the
    /// state that leaves is one more an edit led to.
    fn confirm(&mut self) {
        self.hidden = None;
        self.predicted.composition = None;
        self.expected.push(self.predicted.clone());
    }

    /// `CancelComposition`: the composition stays, as text, and the
    /// history empties, the prediction too, so that the next report
    /// replaces the prediction as `ResetInternal` does, whatever it names,
    /// and an edit before it leaves only the state it led to.
    fn cancel(&mut self) {
        self.hidden = None;
        self.predicted.composition = None;
        self.expected.clear();
    }
}

impl TextInput {
    /// A text input as `text_input.h` initializes it.
    fn new() -> Self {
        Self {
            surface: None,
            field: Field {
                attached: false,
                content_type: ContentType {
                    input_type: extended::INPUT_TYPE_NONE,
                    input_mode: extended::INPUT_MODE_DEFAULT,
                    flags: extended::INPUT_FLAGS_NONE,
                    learning: true,
                    inline_composition: true,
                },
                surrounding_text_supported: true,
                surrounding_text: (String::new(), 0, 0),
                cursor_rectangle: (0, 0, 0, 0),
            },
            pending_support: true,
            text: Tracker::default(),
            registered: None,
        }
    }

    /// A content type from `set_input_type`, `deprecated_set_input_type`
    /// or v1 `set_content_type`. Each handler takes the pending support and
    /// resets it to supported before `SetTypeModeFlags`, which returns
    /// early, dropping the type, while no input method is attached.
    /// Returns whether it changed the type, mode, flags, learning, inline
    /// composition or surrounding-text support (`changed`).
    fn set_type_mode_flags(&mut self, content_type: ContentType) -> bool {
        let supported = std::mem::replace(&mut self.pending_support, true);
        if !self.field.attached {
            return false;
        }
        let changed = self.field.content_type != content_type
            || self.field.surrounding_text_supported != supported;
        self.field.content_type = content_type;
        self.field.surrounding_text_supported = supported;
        changed
    }
}

/// The seat's `wl_keyboard`. A case has one, as a client binds one.
#[derive(Clone, Copy)]
struct Keyboard {
    id: u32,
    /// When it registered as the seat's observer, which is when it was
    /// created (E12).
    registered: u64,
    /// Its `zcr_extended_keyboard_v1`, which the proxy binds at v2, the
    /// first with `peek_key`.
    extended: Option<u32>,
}

/// What `InputMethodAsh` holds between the engine and the text input it
/// serves.
#[derive(Default)]
struct Ima {
    /// The text input it serves: the last to attach
    /// (`SetFocusedTextInputClient`), until it detaches.
    client: Option<u32>,
    /// `pending_commit_`: the text the engine committed for the key in
    /// flight, which goes out with the key's callback.
    commit: Option<String>,
    /// `pending_composition_`: the composition the engine showed for the
    /// key in flight.
    composition: Option<String>,
    /// The composing range a request set while a key was in flight, in
    /// UTF-16 units before and after the cursor, which goes out after it.
    region: Option<(usize, usize)>,
    /// `composing_text_`: whether IMA takes the engine to be composing.
    composing: bool,
}

/// What became of a key's text, the verdict of [`Exo::answer`] on a part of
/// the engine's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// Nothing out of the ordinary: the text went out with the key, or the
    /// key made none.
    Normal,
    /// The key's text was made before a reset that IMA made after Exo
    /// handed the engine the key, and went out with its callback anyway:
    /// the late answer of E5.
    Late,
    /// A reset fell between the key's text and its callback, which dropped
    /// the text: the key went out with none.
    Dropped,
}

/// A key or a request handed to the engine that it has not answered.
struct PendingKey {
    id: u64,
    /// The key and whether it was pressed, or `None` for a request, whose
    /// text goes out at once, as no key is in flight, and which has no key
    /// events.
    key: Option<(Key, bool)>,
    /// The press is an autorepeat, which no key event follows (E7).
    repeat: bool,
    /// The input method it went through is gone, so its answer goes
    /// nowhere.
    orphaned: bool,
    /// The key's time, in milliseconds.
    time: u32,
    /// Whether the engine sent the key's text.
    answered: bool,
    /// Whether IMA reset the engine after Exo handed it the key and before
    /// its text came.
    late: bool,
    /// Whether IMA dropped the key's buffered text, after the text came and
    /// before the callback.
    dropped: bool,
}

/// Exo's seat and its text inputs, as far as the model goes.
#[derive(Default)]
pub(crate) struct Exo {
    /// The version the proxy bound `zcr_text_input_extension_v1` at.
    extension_version: Option<u32>,
    /// The seat's focused surface.
    focus: Option<u32>,
    /// By host `zwp_text_input_v1` ID.
    text_inputs: BTreeMap<u32, TextInput>,
    /// The text input of each host `zcr_extended_text_input_v1`.
    extensions: BTreeMap<u32, u32>,
    /// The events Exo sent and no one has taken yet, in order.
    events: Vec<Outgoing>,
    /// What Exo did of each `set_preedit_region` in `events`, by index.
    region_truths: Vec<(usize, RegionTruth)>,
    keyboard: Option<Keyboard>,
    /// The modifiers the keyboard last sent.
    modifiers: u32,
    /// The keys pressed, by keycode, and whether the input method consumed
    /// each press (`Keyboard::pressed_keys_`): its release goes the same
    /// way.
    pressed: BTreeMap<u32, bool>,
    /// The last serial an event took.
    serial: u32,
    /// The serial `MaybeNextKeySerial` keeps for keys: a `peek_key` takes
    /// it, and the `wl_keyboard.key` that uses it frees it.
    key_serial: Option<u32>,
    ima: Ima,
    /// Whether the case's input method is a Latin layout, which has no
    /// engine: IMA then dispatches every key at once, with no callback, no
    /// confirmation and no lag (`DispatchKeyEvent` without `GetEngine()`).
    pub(super) latin: bool,
    /// What Exo handed the engine and no one has taken yet, in order.
    to_engine: Vec<EngineIn>,
    /// The keys the engine has yet to answer, in order.
    in_engine: VecDeque<PendingKey>,
    /// The ID of the next key Exo hands the engine.
    next_key: u64,
    /// What became of the last region the engine asked for, since the
    /// engine last learned it.
    region_outcome: Option<RegionOutcome>,
    /// The seat's observers registered so far (E12).
    observers: u64,
    /// Each text input's tracker when the display answered a
    /// `wl_display.sync`, with the sync's callback, in order
    /// ([`Exo::snapshot_trackers`]).
    pub(super) synced: Vec<(u32, BTreeMap<u32, Tracker>)>,
}

impl Exo {
    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// Move the seat's focus, as Alt-Tab or a click on another window does:
    /// the keyboard leaves the old surface and enters the new one, and then
    /// the text inputs follow as in [`Exo::set_text_input_focus`]. The seat
    /// tells its observers in the order they registered (E12): a keyboard
    /// bound after a text input's `activate` hears second
    /// ([`Exo::text_inputs_hear_first`]).
    pub(super) fn move_focus(&mut self, focus: Option<u32>) {
        let lost = self.focus;
        if lost == focus {
            return;
        }
        let text_inputs_first = self.text_inputs_hear_first();
        if text_inputs_first {
            self.set_text_input_focus(focus);
        }
        if let Some(Keyboard { id, .. }) = self.keyboard {
            if let Some(surface) = lost {
                let serial = self.next_serial();
                self.events
                    .push(wl_keyboard::evt::leave(id, serial, surface));
            }
            if let Some(surface) = focus {
                self.keyboard_enter(id, surface);
            }
        }
        if !text_inputs_first {
            self.set_text_input_focus(focus);
        }
    }

    /// Whether a text input registered as the seat's observer before the
    /// keyboard, so that it hears of a focus move first: a keyboard bound
    /// after the text input's `activate` (E12).
    pub(super) fn text_inputs_hear_first(&self) -> bool {
        let Some(keyboard) = self.keyboard else {
            return false;
        };
        let registered = |ti: &TextInput| ti.registered.is_some_and(|r| r < keyboard.registered);
        self.text_inputs.values().any(registered)
    }

    /// The user presses or releases `key` at `now`, as `state` says. While
    /// the input method serves a text input and has an engine, Exo hands
    /// the key to the engine and sends its events with the engine's
    /// callback ([`Exo::answer`]). Otherwise the input method has no client
    /// or no engine ([`Exo::latin`]) and passes the key, which reaches the
    /// keyboard at once (E7).
    pub(super) fn key(&mut self, now: Micros, key: Key, state: u32) {
        let pressed = state == wl_keyboard::KEY_STATE_PRESSED;
        let time = millis(now);
        if self.ima.client.is_none() || self.latin {
            let consumed = if pressed {
                // `Keyboard::OnKeyEvent` passes a press only of a key it
                // does not hold.
                let code = keymap::keycode(key).0;
                if self.pressed.contains_key(&code) {
                    return;
                }
                self.pressed.insert(code, false);
                false
            } else {
                let Some(consumed) = self.release(key) else {
                    return;
                };
                consumed
            };
            self.key_events(key, time, state, consumed);
            return;
        }
        let id = self.next_key;
        self.next_key += 1;
        self.in_engine.push_back(PendingKey {
            id,
            key: Some((key, pressed)),
            repeat: false,
            orphaned: false,
            time,
            answered: false,
            late: false,
            dropped: false,
        });
        self.to_engine.push(EngineIn::Key { key, pressed, id });
    }

    /// A part of the engine's answer to a key. IMA buffers the key's text
    /// for the text input it serves, and drops it without one. At the
    /// callback (`ProcessKeyEventDone`), a handled press goes out as a
    /// consumed key, then the text; an unhandled press first confirms the
    /// composition ([`Exo::confirm_composition`]), then sends the text,
    /// then the key (E7, E8). A release goes the way its press went.
    ///
    /// The verdict ([`Answer`]) on what became of the key's text is given
    /// at the callback, once it is known; a text part always returns
    /// [`Answer::Normal`].
    pub(super) fn answer(&mut self, out: &EngineOut) -> Answer {
        match out {
            EngineOut::Text { id, actions } => {
                let key = self.in_engine.front_mut().expect("a key the engine has");
                assert_eq!(key.id, *id, "the engine answers in order");
                key.answered = true;
                if key.orphaned {
                    return Answer::Normal;
                }
                let (request, late) = (key.key.is_none(), key.late);
                let Some(client) = self.ima.client else {
                    return Answer::Normal;
                };
                if !request {
                    self.buffer(actions);
                    return Answer::Normal;
                }
                // While a key is in flight, IMA holds a request's text and
                // range for that key's callback, as it holds the key's own.
                let key_in_flight = self.in_engine.iter().any(|p| p.key.is_some());
                for action in actions {
                    self.apply(client, action, key_in_flight);
                }
                match late {
                    true => Answer::Late,
                    false => Answer::Normal,
                }
            }
            &EngineOut::Done { id, handled } => {
                let pending = self.in_engine.pop_front().expect("a key the engine has");
                assert_eq!(pending.id, id, "the engine answers in order");
                let PendingKey {
                    key,
                    repeat,
                    orphaned,
                    time,
                    late,
                    dropped,
                    ..
                } = pending;
                if orphaned {
                    return Answer::Normal;
                }
                let Some((k, pressed)) = key else {
                    return Answer::Normal;
                };
                if repeat {
                    // An unhandled autorepeat is confirmed before it is
                    // passed on (E13), and the keyboard then drops it (E7).
                    let shown = !handled
                        && self
                            .ima
                            .client
                            .is_some_and(|client| self.confirm_composition(client));
                    let sent = self.flush() || shown;
                    return match (dropped, late && sent) {
                        (true, _) => Answer::Dropped,
                        (false, true) => Answer::Late,
                        (false, false) => Answer::Normal,
                    };
                }
                if !pressed {
                    if let Some(consumed) = self.release(k) {
                        self.key_events(k, time, wl_keyboard::KEY_STATE_RELEASED, consumed);
                    }
                    return Answer::Normal;
                }
                // `Keyboard::OnKeyEvent` passes a press only of a key it does
                // not hold, as one pressed raw since the press went to the
                // engine.
                let code = keymap::keycode(k).0;
                let held = self.pressed.contains_key(&code);
                if !held {
                    self.pressed.insert(code, handled);
                }
                let pressed = wl_keyboard::KEY_STATE_PRESSED;
                let sent = if handled {
                    if !held {
                        self.key_events(k, time, pressed, true);
                    }
                    self.flush()
                } else {
                    let shown = self
                        .ima
                        .client
                        .is_some_and(|client| self.confirm_composition(client));
                    let sent = self.flush() || shown;
                    if !held {
                        self.key_events(k, time, pressed, false);
                    }
                    sent
                };
                if dropped {
                    Answer::Dropped
                } else if late && sent {
                    Answer::Late
                } else {
                    Answer::Normal
                }
            }
        }
    }

    /// The key whose press, not an autorepeat, `out` answers, if it does.
    pub(super) fn press_of(&self, out: &EngineOut) -> Option<Key> {
        let pending = self.in_engine.front()?;
        let id = match *out {
            EngineOut::Text { id, .. } | EngineOut::Done { id, .. } => id,
        };
        match pending.key {
            Some((key, true)) if pending.id == id && !pending.repeat => Some(key),
            _ => None,
        }
    }

    /// The engine repeats held `key` at `now` (D7). IMA hands it the
    /// autorepeat as a press while it serves a text input and has an
    /// engine; the keyboard drops every autorepeat (E7), so otherwise it
    /// goes nowhere. Returns whether the engine got it.
    pub(super) fn autorepeat(&mut self, now: Micros, key: Key) -> bool {
        if self.ima.client.is_none() || self.latin {
            return false;
        }
        let id = self.next_key;
        self.next_key += 1;
        self.in_engine.push_back(PendingKey {
            id,
            key: Some((key, true)),
            repeat: true,
            orphaned: false,
            time: millis(now),
            answered: false,
            late: false,
            dropped: false,
        });
        self.to_engine.push(EngineIn::Key {
            key,
            pressed: true,
            id,
        });
        true
    }

    /// The user makes `request` of the engine (D6). IMA hands it over with
    /// the text before the cursor of the text input it serves, as Exo
    /// predicts it, but only while it serves one and has an engine. A
    /// text input without an extension gets a reconversion too, which Exo
    /// composes but cannot show it.
    pub(super) fn ask(&mut self, request: Request) {
        let Some(client) = self.ima.client.filter(|_| !self.latin) else {
            return;
        };
        let predicted = &self.text_inputs[&client].text.predicted;
        let before = predicted.text[..predicted.cursor()].to_owned();
        let id = self.next_key;
        self.next_key += 1;
        self.in_engine.push_back(PendingKey {
            id,
            key: None,
            repeat: false,
            orphaned: false,
            time: 0,
            answered: false,
            late: false,
            dropped: false,
        });
        self.to_engine.push(EngineIn::Request {
            request,
            id,
            before,
        });
    }

    /// What Exo handed the engine since the last call, in order. Whoever
    /// plays the engine drains it after every input it hands the model
    /// ([`Exo::key`], a request, a focus move, [`Exo::destroy_input_method`])
    /// and sends each part to the engine at that time, so the engine's
    /// queue holds what Exo handed it in the order it did.
    pub(super) fn take_engine(&mut self) -> Vec<EngineIn> {
        std::mem::take(&mut self.to_engine)
    }

    /// Move the seat's focus for the text inputs only, for a compositor
    /// that sends the keyboard's `enter` and `leave` itself, as the
    /// fuzzer's does. `TextInput::OnSurfaceFocused` attaches the text
    /// inputs of the surface that gains it and deactivates those of the
    /// surface that loses it.
    pub(crate) fn set_text_input_focus(&mut self, focus: Option<u32>) {
        let lost = std::mem::replace(&mut self.focus, focus);
        if lost == focus {
            return;
        }
        for id in self.ids() {
            let surface = self.text_inputs[&id].surface;
            if surface.is_some() && surface == focus {
                self.attach(id);
            } else if surface.is_some() && surface == lost {
                self.deactivate(id);
            }
        }
    }

    /// Forget the objects that are no longer `live`, destroyed by the
    /// proxy or by the compositor. A destroyed surface loses the focus, and
    /// `TextInput::OnSurfaceDestroying` deactivates its text inputs. An
    /// extension whose text input is gone has no delegate, and Exo ignores
    /// its requests.
    pub(crate) fn forget(&mut self, live: impl Fn(u32) -> bool) {
        if self.focus.is_some_and(|surface| !live(surface)) {
            self.set_text_input_focus(None);
        }
        self.text_inputs.retain(|&id, _| live(id));
        // A text input that ends detaches as `DetachInputMethod` does, which
        // drops its type first: IMA stops serving it, can send it nothing,
        // and leaves the engine as it is ([`Exo::reset_context`]).
        if self.ima.client.is_some_and(|id| !live(id)) {
            self.ima.client = None;
        }
        let text_inputs = &self.text_inputs;
        let extended = |extension: u32, id: u32| live(extension) && text_inputs.contains_key(&id);
        self.extensions
            .retain(|&extension, &mut id| extended(extension, id));
        self.keyboard = self.keyboard.filter(|keyboard| live(keyboard.id));
        if let Some(keyboard) = &mut self.keyboard {
            keyboard.extended = keyboard.extended.filter(|&extended| live(extended));
        }
        for id in self.ids() {
            if self.text_inputs[&id]
                .surface
                .is_some_and(|surface| !live(surface))
            {
                self.deactivate(id);
            }
        }
    }

    /// Apply a request from the proxy, sent by host object `sender`, that
    /// the compositor has accepted.
    pub(crate) fn request(&mut self, sender: u32, request: &AnyRequest) {
        match request {
            AnyRequest::ZwpTextInputManagerV1(
                zwp_text_input_manager_v1::Request::CreateTextInput { id },
            ) => {
                self.text_inputs.insert(*id, TextInput::new());
            }
            AnyRequest::ZcrTextInputExtensionV1(
                zcr_text_input_extension_v1::Request::GetExtendedTextInput { id, text_input },
            ) => {
                self.extensions.insert(*id, *text_input);
            }
            AnyRequest::WlRegistry(wl_registry::Request::Bind {
                id: (interface, version, _),
                ..
            }) if interface == zcr_text_input_extension_v1::NAME => {
                self.extension_version = Some(*version);
            }
            AnyRequest::ZcrExtendedTextInputV1(request) => self.extension_request(sender, request),
            AnyRequest::ZwpTextInputV1(request) => self.text_input_request(sender, request),
            AnyRequest::WlSeat(wl_seat::Request::GetKeyboard { id }) => {
                let id = *id;
                self.observers += 1;
                let registered = self.observers;
                self.keyboard = Some(Keyboard {
                    id,
                    extended: None,
                    registered,
                });
                // A new keyboard learns the seat's focus at once.
                if let Some(surface) = self.focus {
                    self.keyboard_enter(id, surface);
                }
            }
            AnyRequest::ZcrKeyboardExtensionV1(
                zcr_keyboard_extension_v1::Request::GetExtendedKeyboard { id, keyboard },
            ) => {
                if let Some(bound) = self.keyboard.as_mut().filter(|bound| bound.id == *keyboard) {
                    bound.extended = Some(*id);
                }
            }
            _ => {}
        }
    }

    /// The input method goes away, as when its process restarts. Its
    /// destructor confirms the composition of the text input it serves
    /// (`~InputMethodAsh`), and then `TextInput::OnInputMethodDestroyed`
    /// deactivates each text input it was attached to; nothing attaches it
    /// again until the next `activate`. Returns whether one was attached.
    ///
    /// The new input method is there for that `activate` at once: the
    /// model leaves out a window in which an `activate` finds none, which
    /// would leave Exo unattached until the focus next moves. The engine
    /// stays: only the confirmation of an attached text input resets it.
    /// A key or request in flight never gets its callback, as the engine's
    /// callback held the old input method weakly.
    pub(crate) fn destroy_input_method(&mut self) -> bool {
        if let Some(client) = self.ima.client {
            self.confirm_composition(client);
        }
        self.ima = Ima::default();
        for pending in &mut self.in_engine {
            pending.orphaned = true;
        }
        let mut detached = false;
        for id in self.ids() {
            if self.text_inputs[&id].field.attached {
                self.deactivate(id);
                detached = true;
            }
        }
        detached
    }

    /// The display answers the `wl_display.sync` of `callback`: every
    /// request before it has changed the trackers, and nothing after it
    /// has, so a model of them that the requests and events up to its
    /// `done` drive must hold the same ([`Exo::synced`]).
    pub(super) fn snapshot_trackers(&mut self, callback: u32) {
        let trackers = self
            .text_inputs
            .iter()
            .map(|(&id, text_input)| (id, text_input.text.clone()))
            .collect();
        self.synced.push((callback, trackers));
    }

    /// The events Exo sent since the last call, in order. Whoever plays the
    /// display takes them after each input it hands the model, before it
    /// answers any later `wl_display.sync`, so that the sync's `done`
    /// follows every event of earlier requests (E11).
    pub(crate) fn take_events(&mut self) -> Vec<Outgoing> {
        self.region_truths.clear();
        std::mem::take(&mut self.events)
    }

    /// [`Exo::take_events`], each `set_preedit_region` with what Exo did
    /// of it.
    pub(super) fn take_events_with_truths(&mut self) -> Vec<(Outgoing, Option<RegionTruth>)> {
        let mut truths = std::mem::take(&mut self.region_truths)
            .into_iter()
            .peekable();
        let events = std::mem::take(&mut self.events);
        (0..)
            .zip(events)
            .map(|(i, event)| {
                let truth = truths.next_if(|(at, _)| *at == i).map(|(_, truth)| truth);
                (event, truth)
            })
            .collect()
    }

    /// The composition Exo tracks for the text input attached on the
    /// focused surface, if one is; empty if it composes nothing.
    pub(super) fn composition(&self) -> Option<String> {
        let id = *self.composing_text_inputs().first()?;
        let tracker = &self.text_inputs[&id].text;
        Some(tracker.predicted.composition().to_owned())
    }

    /// If that composition is a region its client never sees, as the text
    /// input has no extension, and which nothing has committed or changed
    /// since: the composition the client shows instead, the one that stood
    /// before the region.
    pub(super) fn composition_shown_instead(&self) -> Option<String> {
        let id = *self.composing_text_inputs().first()?;
        self.text_inputs[&id].text.hidden.clone()
    }

    /// Whether the tracker of the text input attached on the focused
    /// surface holds a state with the composition `composition` at the
    /// cursor of the report `(text, cursor, anchor)`, which that state is
    /// without it: a report that would only drop the composition.
    pub(super) fn holds_with_composition(
        &self,
        (text, cursor, anchor): &(String, u32, u32),
        composition: &str,
    ) -> bool {
        let Some(id) = self.composing_text_inputs().first().copied() else {
            return false;
        };
        let at = *cursor as usize;
        let holds = |state: &Predicted| {
            state.composition.as_ref().is_some_and(|range| {
                range.start == at
                    && state.text[range.clone()] == *composition
                    && text.get(..at) == Some(&state.text[..at])
                    && text.get(at..) == Some(&state.text[range.end..])
            })
        };
        cursor == anchor && self.text_inputs[&id].text.expected.iter().any(holds)
    }

    /// The host text inputs an input method is attached to on the focused
    /// surface: the ones whose preedits and presses matter.
    pub(crate) fn composing_text_inputs(&self) -> Vec<u32> {
        self.text_inputs
            .iter()
            .filter(|(_, text_input)| {
                text_input.field.attached
                    && self.focus.is_some()
                    && text_input.surface == self.focus
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// The fields of the text inputs active on the focused surface.
    pub(super) fn focused_fields(&self) -> Vec<Field> {
        self.text_inputs
            .values()
            .filter(|text_input| self.focus.is_some() && text_input.surface == self.focus)
            .map(|text_input| text_input.field.clone())
            .collect()
    }

    fn extension_request(&mut self, sender: u32, request: &extended::Request) {
        let content_type = match *request {
            extended::Request::Destroy {} => {
                self.extensions.remove(&sender);
                return;
            }
            extended::Request::SetSurroundingTextSupport { support } => {
                let support = match support {
                    extended::SURROUNDING_TEXT_SUPPORT_SUPPORTED => true,
                    extended::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED => false,
                    // Exo only logs an unknown value.
                    _ => return,
                };
                if let Some(text_input) = self.extended(sender) {
                    text_input.pending_support = support;
                }
                return;
            }
            extended::Request::SetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
                inline_composition_support,
            } => ContentType {
                input_type,
                input_mode,
                flags: input_flags,
                learning: learning_mode == extended::LEARNING_MODE_ENABLED,
                inline_composition: inline_composition_support
                    == extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            },
            // The new request with inline composition supported.
            extended::Request::DeprecatedSetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
            } => ContentType {
                input_type,
                input_mode,
                flags: input_flags,
                learning: learning_mode == extended::LEARNING_MODE_ENABLED,
                inline_composition: true,
            },
            _ => return,
        };
        if let Some(id) = self.extended_id(sender) {
            self.set_type_mode_flags(id, content_type);
        }
    }

    /// `SetTypeModeFlags` for text input `id`: if it changed anything and
    /// the input method serves it, `OnTextInputTypeChanged` runs
    /// `UpdateContextFocusState` and so `ResetContext`, which drops the
    /// buffers, marks the keys in flight late, and resets the engine if IMA
    /// took it to be composing; with the new type none, it returns early.
    fn set_type_mode_flags(&mut self, id: u32, content_type: ContentType) {
        let changed = self.text_input(id).set_type_mode_flags(content_type);
        if changed && self.ima.client == Some(id) {
            self.reset_context();
        }
    }

    fn text_input_request(&mut self, sender: u32, request: &zwp_text_input_v1::Request) {
        use zwp_text_input_v1::Request;
        match request {
            Request::SetContentType { hint, purpose } => {
                let content_type = v1_content_type(*hint, *purpose);
                self.set_type_mode_flags(sender, content_type);
            }
            Request::Activate { surface, .. } => self.activate(sender, *surface),
            Request::Deactivate { .. } => self.deactivate(sender),
            Request::Reset {} => self.reset(sender),
            // Exo keeps surrounding text and the cursor rectangle with or
            // without an input method.
            Request::SetSurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let text = match text {
                    IndexedString::Utf8(text) => text.clone(),
                    IndexedString::NotUtf8(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                };
                let text_input = self.text_input(sender);
                let (at, from) = (*cursor as usize, *anchor as usize);
                text_input.text.update(text.clone(), at, from);
                text_input.field.surrounding_text = (text, *cursor, *anchor);
            }
            Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                let rectangle = (*x, *y, *width, *height);
                self.text_input(sender).field.cursor_rectangle = rectangle;
            }
            _ => {}
        }
    }

    /// `TextInput::Activate`: nothing for the surface it already has;
    /// otherwise detach, take the surface, and attach if the seat's focus
    /// is on it.
    fn activate(&mut self, id: u32, surface: u32) {
        if self.text_input(id).surface == Some(surface) {
            return;
        }
        if self.text_input(id).registered.is_none() {
            self.observers += 1;
            self.text_input(id).registered = Some(self.observers);
        }
        self.detach(id);
        self.text_input(id).surface = Some(surface);
        if self.focus == Some(surface) {
            self.attach(id);
        }
    }

    /// `TextInput::Deactivate`.
    fn deactivate(&mut self, id: u32) {
        self.detach(id);
        let text_input = self.text_input(id);
        text_input.surface = None;
        text_input.registered = None;
    }

    /// `AttachInputMethod` resets the type and mode to text, and keeps the
    /// flags, learning and surrounding-text support of the previous field.
    /// The input method then serves this text input, and its delegate's
    /// `Activated` sends `enter` with the surface of the last `activate`
    /// (`zwp_text_input_manager.cc:127`).
    fn attach(&mut self, id: u32) {
        let text_input = self.text_input(id);
        let surface = text_input.surface.expect("an activated text input");
        text_input.field.attached = true;
        text_input.field.content_type.input_type = extended::INPUT_TYPE_TEXT;
        text_input.field.content_type.input_mode = extended::INPUT_MODE_TEXT;
        self.serve(Some(id));
        self.events.push(zwp_text_input_v1::evt::enter(id, surface));
    }

    /// `DetachInputMethod`, which does nothing without an input method. It
    /// drops the type before the input method stops serving this text
    /// input, which confirms the composition but, with the type none,
    /// leaves the engine as it is. Then the delegate's `Deactivated` sends
    /// `leave` (`zwp_text_input_manager.cc:132`).
    fn detach(&mut self, id: u32) {
        let text_input = self.text_input(id);
        if !text_input.field.attached {
            return;
        }
        text_input.field.attached = false;
        text_input.field.content_type.input_type = extended::INPUT_TYPE_NONE;
        text_input.field.content_type.input_mode = extended::INPUT_MODE_DEFAULT;
        if self.ima.client == Some(id) {
            self.serve(None);
        }
        self.events.push(zwp_text_input_v1::evt::leave(id));
    }

    /// `TextInput::Reset`: the composition Exo tracks is cancelled, and if
    /// the input method serves this text input, it resets
    /// (`CancelComposition`). Nothing goes to the client (E5).
    fn reset(&mut self, id: u32) {
        self.text_input(id).text.cancel();
        if self.ima.client == Some(id) {
            self.reset_context();
        }
    }

    /// The input method serves `client` from now on
    /// (`SetFocusedTextInputClient`, `DetachTextInputClient`). The one it
    /// served loses its buffered commit and has its composition confirmed
    /// (`OnWillChangeFocusedClient`), and the input method then resets
    /// (`OnDidChangeFocusedClient`).
    fn serve(&mut self, client: Option<u32>) {
        if let Some(served) = self.ima.client {
            if self.ima.commit.take().is_some() {
                self.mark_dropped();
            }
            self.confirm_composition(served);
        }
        self.ima.client = client;
        self.reset_context();
    }

    /// IMA buffers the engine's text for the key in flight
    /// (`CommitText`, `UpdateCompositionText`).
    fn buffer(&mut self, actions: &[TextAction]) {
        for action in actions {
            match action {
                TextAction::Commit(text) => {
                    self.ima
                        .commit
                        .get_or_insert_with(String::new)
                        .push_str(text);
                }
                TextAction::Preedit(text) => {
                    self.ima.composing |= !text.is_empty();
                    self.ima.composition = Some(text.clone());
                }
                TextAction::Delete { .. } | TextAction::Region { .. } => {
                    panic!("only a request deletes or reconverts, and no key is in flight then")
                }
            }
        }
    }

    /// IMA sends the buffered text to the text input it serves, the commit
    /// first (`MaybeProcessPendingInputMethodResult`). Returns whether it
    /// sent any.
    fn flush(&mut self) -> bool {
        let commit = self.ima.commit.take();
        let composition = self.ima.composition.take();
        let Some(id) = self.ima.client else {
            return false;
        };
        if let Some(text) = &commit {
            self.insert_text(id, text);
            self.ima.composing = false;
        }
        if let Some(text) = &composition {
            self.ima.composing = true;
            self.set_composition(id, text);
        }
        let region = self.ima.region.take();
        if let Some((before, after)) = region {
            self.ima.composing |= self.set_composition_from_existing_text(id, before, after);
        }
        commit.is_some() || composition.is_some() || region.is_some()
    }

    /// `InputMethodAsh::ConfirmComposition(reset_engine = true)` for text
    /// input `id`, which IMA serves. With only a commit buffered, nothing
    /// happens. Otherwise a buffered composition is shown, the composition
    /// Exo tracks is confirmed if there is one, or always where the
    /// extension has `confirm_preedit` (`SupportsAlwaysConfirmComposition`),
    /// and IMA resets, keeping the buffered commit. Returns whether it
    /// showed the buffered composition.
    fn confirm_composition(&mut self, id: u32) -> bool {
        if self.ima.commit.is_some() && self.ima.composition.is_none() {
            return false;
        }
        let shown = self.ima.composition.take();
        if let Some(text) = &shown {
            self.set_composition(id, text);
        }
        let extension = self.confirming_extension(id);
        let composing = !self.text_input(id).text.predicted.composition().is_empty();
        if extension.is_some() || composing {
            self.confirm_composition_text(id, extension);
        }
        let commit = self.ima.commit.take();
        self.reset_context();
        self.ima.commit = commit;
        shown.is_some()
    }

    /// `TextInput::ConfirmCompositionText(keep_selection = true)`: with
    /// `confirm_preedit` on `extension`, leaving the selection unchanged; or
    /// else, by the delegate's fallback, the selection as `cursor_position`
    /// (its `end` the index, its `start` the anchor), then a
    /// `commit_string` of the composition.
    fn confirm_composition_text(&mut self, id: u32, extension: Option<u32>) {
        let text = &mut self.text_input(id).text;
        let composition = text.predicted.composition().to_owned();
        let selection = text.predicted.selection;
        let [index, anchor] =
            [selection.end, selection.start].map(|at| i32::try_from(at).expect("a short text"));
        text.confirm();
        if let Some(extension) = extension {
            let unchanged = extended::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_UNCHANGED;
            self.events
                .push(extended::evt::confirm_preedit(extension, unchanged));
            return;
        }
        self.events
            .push(zwp_text_input_v1::evt::cursor_position(id, index, anchor));
        let serial = self.next_serial();
        let event = zwp_text_input_v1::evt::commit_string(id, serial, &composition);
        self.events.push(event.expect("a commit fits a message"));
    }

    /// `InputMethodAsh::ResetContext`, which does nothing without a text
    /// input to serve, or while its type is none
    /// (`IsPasswordOrNoneInputFieldFocused`), as after a detach: the
    /// buffered text goes, and the engine resets if IMA took it to be
    /// composing.
    fn reset_context(&mut self) {
        let Some(client) = self.ima.client else {
            return;
        };
        let served = self.text_inputs.get(&client);
        if served.is_none_or(|ti| ti.field.content_type.input_type == extended::INPUT_TYPE_NONE) {
            return;
        }
        let dropped = self.ima.commit.take().is_some()
            | self.ima.composition.take().is_some()
            | self.ima.region.take().is_some();
        if dropped {
            self.mark_dropped();
        }
        if std::mem::take(&mut self.ima.composing) {
            self.reset_engine();
        }
    }

    /// IMA resets the engine, which reaches the reset after the keys in
    /// flight, so their answers are late.
    fn reset_engine(&mut self) {
        for key in self.in_engine.iter_mut().filter(|key| !key.answered) {
            key.late = true;
        }
        self.to_engine.push(EngineIn::Reset);
    }

    /// IMA dropped the text it buffered for the key in flight, which is the
    /// first, as the engine answers in order.
    fn mark_dropped(&mut self) {
        if let Some(key) = self.in_engine.front_mut().filter(|key| key.answered) {
            key.dropped = true;
        }
    }

    /// IMA passes on `action` of a request's answer to text input `id`
    /// (`input_method_ash.cc`). A delete goes out at once, unless IMA takes
    /// the engine to compose, when it is dropped. A commit and a composing
    /// range wait for the callback of a key in flight (`key_in_flight`),
    /// and otherwise go out at once; a commit then leaves IMA's
    /// `composing_text_` as it is.
    fn apply(&mut self, id: u32, action: &TextAction, key_in_flight: bool) {
        match *action {
            TextAction::Commit(ref text) if key_in_flight => {
                self.ima
                    .commit
                    .get_or_insert_with(String::new)
                    .push_str(text);
            }
            TextAction::Commit(ref text) => self.insert_text(id, text),
            TextAction::Preedit(ref text) => {
                self.set_composition(id, text);
                self.ima.composing = !text.is_empty();
            }
            TextAction::Delete { .. } if self.ima.composing => {}
            TextAction::Delete { before, after } => self.delete_surrounding_text(id, before, after),
            TextAction::Region { before, after } if key_in_flight => {
                self.ima.region = Some((before, after));
            }
            TextAction::Region { before, after } => {
                let composed = self.set_composition_from_existing_text(id, before, after);
                if !composed {
                    self.region_outcome = Some(RegionOutcome::Refused);
                }
                self.ima.composing |= composed;
            }
        }
    }

    /// `TextInput::ExtendSelectionAndDelete`: the selection of Exo's
    /// prediction, with the UTF-16 units `before` it and `after` it,
    /// clamped to the text, which it
    /// deletes, as a `delete_surrounding_text` with the absolute byte
    /// index of their start and no `commit_string` (E10).
    fn delete_surrounding_text(&mut self, id: u32, before: usize, after: usize) {
        let text = &mut self.text_input(id).text;
        let truth = RegionTruth::of(text);
        let selection = text.predicted.selection.range();
        let (range, _) = utf16_span(&text.predicted.text, selection, before, after);
        let (index, length) = (range.start, range.len());
        text.delete(range);
        let index = i32::try_from(index).expect("a short text");
        let length = u32::try_from(length).expect("a short text");
        let event = zwp_text_input_v1::evt::delete_surrounding_text(id, index, length);
        self.region_truths.push((self.events.len(), truth));
        self.events.push(event);
    }

    /// `TextInput::SetCompositionFromExistingText`: the UTF-16 units
    /// `before` the cursor and `after` it become the composition, which the
    /// extension's `set_preedit_region` shows, in bytes from the cursor. A
    /// range past the text is refused, before anything is sent
    /// (`text_input.cc`), as IMA's `SetComposingRange` refuses it when it
    /// comes (`input_method_ash.cc`); the engine hears only of IMA's
    /// refusal, not of one when IMA sends a range it held for a key.
    fn set_composition_from_existing_text(&mut self, id: u32, before: usize, after: usize) -> bool {
        let text = &mut self.text_input(id).text;
        let cursor = text.predicted.cursor();
        let (range, within) = utf16_span(&text.predicted.text, cursor..cursor, before, after);
        if !within {
            return false;
        }
        let truth = RegionTruth::of(text);
        let hidden = self.extension_of(id).is_none();
        let composed = self
            .text_input(id)
            .text
            .compose_existing(range.clone(), hidden);
        if let Some(extension) = self.extension_of(id) {
            let index = -i32::try_from(cursor - range.start).expect("a short region");
            let length = u32::try_from(range.len()).expect("a short region");
            let event = extended::evt::set_preedit_region(extension, index, length);
            self.region_truths.push((self.events.len(), truth));
            self.events.push(event);
        }
        self.region_outcome = Some(RegionOutcome::Composed(composed));
        true
    }

    /// What became of the region the engine asked for last, if Exo composed
    /// or refused one since the last call, for the engine to learn.
    pub(super) fn take_region_outcome(&mut self) -> Option<RegionOutcome> {
        self.region_outcome.take()
    }

    /// `TextInput::InsertText`: a `commit_string`, which ends the
    /// composition.
    fn insert_text(&mut self, id: u32, text: &str) {
        let serial = self.next_serial();
        let event = zwp_text_input_v1::evt::commit_string(id, serial, text);
        self.events.push(event.expect("a commit fits a message"));
        self.text_input(id).text.insert(text);
    }

    /// `TextInput::SetCompositionText`: `preedit_cursor` at the end of the
    /// composition, then a `preedit_string` that is its own commit text.
    fn set_composition(&mut self, id: u32, text: &str) {
        let cursor = i32::try_from(text.len()).expect("a short composition");
        self.events
            .push(zwp_text_input_v1::evt::preedit_cursor(id, cursor));
        let serial = self.next_serial();
        let event = zwp_text_input_v1::evt::preedit_string(id, serial, text, text);
        self.events.push(event.expect("a preedit fits a message"));
        let selection = Selection::caret(text.len());
        self.text_input(id).text.set_composition(text, selection);
    }

    /// One transition of `key` on the keyboard, if it is on a surface: a
    /// `peek_key` on its extension, and, unless the input method consumed
    /// the key, a `wl_keyboard.key` with the same serial. A run of consumed
    /// transitions shares one serial, which the next key sent frees. Shift
    /// is not a key of its own here, so it sends no `peek_key` or `key`:
    /// its modifier goes on before a shifted key's press and off after its
    /// release.
    fn key_events(&mut self, key: Key, time: u32, state: u32, consumed: bool) {
        let Some(Keyboard {
            id: keyboard,
            extended,
            ..
        }) = self.keyboard
        else {
            return;
        };
        if self.focus.is_none() {
            return;
        }
        let (code, shift) = keymap::keycode(key);
        if shift && state == wl_keyboard::KEY_STATE_PRESSED {
            self.set_modifiers(keymap::SHIFT);
        }
        let serial = match self.key_serial {
            Some(serial) => serial,
            None => self.next_serial(),
        };
        self.key_serial = Some(serial);
        if let Some(extended) = extended {
            let peek = zcr_extended_keyboard_v1::evt::peek_key(extended, serial, time, code, state);
            self.events.push(peek);
        }
        if !consumed {
            self.events
                .push(wl_keyboard::evt::key(keyboard, serial, time, code, state));
            self.key_serial = None;
        }
        if shift && state == wl_keyboard::KEY_STATE_RELEASED {
            self.set_modifiers(0);
        }
    }

    /// The release of `key`: whether the input method consumed its press,
    /// or `None` if Exo tracked no such press, whose release
    /// `Keyboard::OnKeyEvent` ignores.
    fn release(&mut self, key: Key) -> Option<bool> {
        self.pressed.remove(&keymap::keycode(key).0)
    }

    /// `wl_keyboard.modifiers` with `depressed`, if they changed.
    fn set_modifiers(&mut self, depressed: u32) {
        if depressed == self.modifiers {
            return;
        }
        self.modifiers = depressed;
        if let Some(Keyboard { id, .. }) = self.keyboard {
            let serial = self.next_serial();
            self.events
                .push(wl_keyboard::evt::modifiers(id, serial, depressed, 0, 0, 0));
        }
    }

    /// `keyboard` enters `surface`, after the modifiers, as Exo sends them.
    fn keyboard_enter(&mut self, keyboard: u32, surface: u32) {
        let serial = self.next_serial();
        let modifiers = self.modifiers;
        self.events.push(wl_keyboard::evt::modifiers(
            keyboard, serial, modifiers, 0, 0, 0,
        ));
        let serial = self.next_serial();
        let enter = wl_keyboard::evt::enter(keyboard, serial, surface, &[]);
        self.events.push(enter.expect("an enter with no keys fits"));
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// The extension of text input `id` if it has `confirm_preedit` (v11),
    /// which the delegate then always confirms with
    /// (`SupportsConfirmPreedit`).
    fn confirming_extension(&self, id: u32) -> Option<u32> {
        let extension = self.extension_of(id)?;
        let since = extended::EVT_CONFIRM_PREEDIT_SINCE;
        (self.extension_version >= Some(since)).then_some(extension)
    }

    /// The extension of text input `id`, if it has one.
    fn extension_of(&self, id: u32) -> Option<u32> {
        let mut extensions = self.extensions.iter();
        extensions.find_map(|(&extension, &text_input)| (text_input == id).then_some(extension))
    }

    /// The IDs of the text inputs, in order.
    fn ids(&self) -> Vec<u32> {
        self.text_inputs.keys().copied().collect()
    }

    fn text_input(&mut self, id: u32) -> &mut TextInput {
        self.text_inputs.get_mut(&id).expect("a host text input")
    }

    /// The text input of `extension`, if it still has one.
    fn extended(&mut self, extension: u32) -> Option<&mut TextInput> {
        let id = self.extended_id(extension)?;
        self.text_inputs.get_mut(&id)
    }

    /// The ID of the text input of `extension`, if it still has one.
    fn extended_id(&self, extension: u32) -> Option<u32> {
        let id = *self.extensions.get(&extension)?;
        self.text_inputs.contains_key(&id).then_some(id)
    }
}

/// The bytes of `text` from `before` UTF-16 units before `range` to
/// `after` units after it, clamped to the text, and whether they lay
/// within it.
fn utf16_span(
    text: &str,
    range: Range<usize>,
    before: usize,
    after: usize,
) -> (Range<usize>, bool) {
    let mut start = range.start;
    let mut units = 0;
    for c in text[..range.start].chars().rev() {
        if units >= before {
            break;
        }
        units += c.len_utf16();
        start -= c.len_utf8();
    }
    let within_before = units >= before;
    let mut end = range.end;
    let mut units = 0;
    for c in text[range.end..].chars() {
        if units >= after {
            break;
        }
        units += c.len_utf16();
        end += c.len_utf8();
    }
    (start..end, within_before && units >= after)
}

/// `now` in milliseconds, as a Wayland input event carries it, wrapping as
/// its 32 bits do.
fn millis(now: Micros) -> u32 {
    (now.0 / 1_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::AnyEvent;
    use crate::sim::hangul::keys;
    use crate::test_support::decoded;

    const SEAT: u32 = 2;
    const TI_MANAGER: u32 = 3;
    const EXT_MANAGER: u32 = 4;
    const KEYBOARD_EXT_MANAGER: u32 = 5;
    const TI: u32 = 10;
    const EXT: u32 = 11;
    const A: u32 = 20;
    const B: u32 = 21;
    const KEYBOARD: u32 = 30;
    const PEEK: u32 = 31;
    const KEYBOARD2: u32 = 32;
    const TI2: u32 = 40;
    const REGISTRY: u32 = 6;
    /// Extension versions with `confirm_preedit` and without it.
    const V11: u32 = extended::EVT_CONFIRM_PREEDIT_SINCE;
    const V10: u32 = V11 - 1;
    /// The time of every key, and its milliseconds.
    const NOW: Micros = Micros(7_000);
    const MS: u32 = 7;
    const DOWN: u32 = wl_keyboard::KEY_STATE_PRESSED;
    const UP: u32 = wl_keyboard::KEY_STATE_RELEASED;

    fn exo() -> Exo {
        exo_for(V10)
    }

    /// Exo whose seat focuses surface `A`, with keyboard `KEYBOARD` and its
    /// extension `PEEK`, and text input `TI` and its extension `EXT`, bound
    /// at `version`, not yet activated.
    fn exo_for(version: u32) -> Exo {
        let mut exo = Exo::default();
        let interface = zcr_text_input_extension_v1::NAME.to_owned();
        let id = (interface, version, EXT_MANAGER);
        send(
            &mut exo,
            REGISTRY,
            wl_registry::Request::Bind { name: 1, id },
        );
        send(
            &mut exo,
            SEAT,
            wl_seat::Request::GetKeyboard { id: KEYBOARD },
        );
        let peek = zcr_keyboard_extension_v1::Request::GetExtendedKeyboard {
            id: PEEK,
            keyboard: KEYBOARD,
        };
        send(&mut exo, KEYBOARD_EXT_MANAGER, peek);
        let create = zwp_text_input_manager_v1::Request::CreateTextInput { id: TI };
        send(&mut exo, TI_MANAGER, create);
        let extend = zcr_text_input_extension_v1::Request::GetExtendedTextInput {
            id: EXT,
            text_input: TI,
        };
        send(&mut exo, EXT_MANAGER, extend);
        exo.set_text_input_focus(Some(A));
        exo
    }

    /// [`exo_for`] with `TI` active on `A`, and its events taken.
    fn active(version: u32) -> Exo {
        let mut exo = exo_for(version);
        send(&mut exo, TI, activate(A));
        exo.take_events();
        exo
    }

    /// The key a character of a script types (see [`keys`]).
    fn k(c: char) -> Key {
        keys(&c.to_string())[0]
    }

    /// The user presses `key`, and the engine answers at once with
    /// `actions` and whether it `handled` the key; returns the events Exo
    /// sent.
    fn press(
        exo: &mut Exo,
        key: char,
        actions: &[TextAction],
        handled: bool,
    ) -> Vec<(u32, AnyEvent)> {
        exo.key(NOW, k(key), DOWN);
        let [EngineIn::Key { id, .. }] = exo.take_engine()[..] else {
            panic!("the key went to the engine");
        };
        if !actions.is_empty() {
            let actions = actions.to_vec();
            assert_eq!(exo.answer(&EngineOut::Text { id, actions }), Answer::Normal);
        }
        assert_eq!(exo.answer(&EngineOut::Done { id, handled }), Answer::Normal);
        events(exo)
    }

    fn shows(text: &str) -> TextAction {
        TextAction::Preedit(text.to_owned())
    }

    fn commits(text: &str) -> TextAction {
        TextAction::Commit(text.to_owned())
    }

    fn surrounding(text: &str, cursor: u32) -> zwp_text_input_v1::Request {
        zwp_text_input_v1::Request::SetSurroundingText {
            text: text.into(),
            cursor,
            anchor: cursor,
        }
    }

    fn deactivate() -> zwp_text_input_v1::Request {
        zwp_text_input_v1::Request::Deactivate { seat: SEAT }
    }

    fn send(exo: &mut Exo, sender: u32, request: impl Into<AnyRequest>) {
        exo.request(sender, &request.into());
    }

    fn activate(surface: u32) -> zwp_text_input_v1::Request {
        zwp_text_input_v1::Request::Activate {
            seat: SEAT,
            surface,
        }
    }

    /// A content type of `input_type` with every other value set apart
    /// from the defaults.
    fn input_type(input_type: u32) -> extended::Request {
        extended::Request::SetInputType {
            input_type,
            input_mode: extended::INPUT_MODE_NUMERIC,
            input_flags: extended::INPUT_FLAGS_AUTOCORRECT_OFF,
            learning_mode: extended::LEARNING_MODE_DISABLED,
            inline_composition_support: extended::INLINE_COMPOSITION_SUPPORT_UNSUPPORTED,
        }
    }

    fn support(supported: bool) -> extended::Request {
        let support = if supported {
            extended::SURROUNDING_TEXT_SUPPORT_SUPPORTED
        } else {
            extended::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED
        };
        extended::Request::SetSurroundingTextSupport { support }
    }

    /// The events Exo sent since the last call, decoded.
    fn events(exo: &mut Exo) -> Vec<(u32, AnyEvent)> {
        decoded(&exo.take_events(), |msg| {
            AnyEvent::from_wire(interface(msg.sender_id), msg)
        })
    }

    /// The interface of each object the tests name.
    fn interface(id: u32) -> &'static str {
        match id {
            TI => "zwp_text_input_v1",
            EXT => "zcr_extended_text_input_v1",
            KEYBOARD | KEYBOARD2 => "wl_keyboard",
            TI2 => "zwp_text_input_v1",
            PEEK => "zcr_extended_keyboard_v1",
            _ => panic!("no test object has ID {id}"),
        }
    }

    fn enter(surface: u32) -> (u32, AnyEvent) {
        (TI, zwp_text_input_v1::Event::Enter { surface }.into())
    }

    fn leave() -> (u32, AnyEvent) {
        (TI, zwp_text_input_v1::Event::Leave {}.into())
    }

    /// `preedit_cursor` and `preedit_string` showing `text` on `TI`.
    fn preedit(serial: u32, text: &str) -> [(u32, AnyEvent); 2] {
        preedit_on(TI, serial, text)
    }

    fn preedit_on(id: u32, serial: u32, text: &str) -> [(u32, AnyEvent); 2] {
        let index = text.len() as i32;
        let commit = text.to_owned();
        let text = text.into();
        [
            (id, zwp_text_input_v1::Event::PreeditCursor { index }.into()),
            (
                id,
                zwp_text_input_v1::Event::PreeditString {
                    serial,
                    text,
                    commit,
                }
                .into(),
            ),
        ]
    }

    fn commit(serial: u32, text: &str) -> (u32, AnyEvent) {
        let text = text.to_owned();
        (
            TI,
            zwp_text_input_v1::Event::CommitString { serial, text }.into(),
        )
    }

    /// The cursor `zwp_text_input_v1.cursor_position` reports, with no
    /// selection.
    fn cursor(index: i32) -> (u32, AnyEvent) {
        let anchor = index;
        (
            TI,
            zwp_text_input_v1::Event::CursorPosition { index, anchor }.into(),
        )
    }

    fn confirm() -> (u32, AnyEvent) {
        let selection_behavior = extended::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_UNCHANGED;
        (
            EXT,
            extended::Event::ConfirmPreedit { selection_behavior }.into(),
        )
    }

    fn peek(serial: u32, key: char, state: u32) -> (u32, AnyEvent) {
        let key = keymap::keycode(k(key)).0;
        let event = zcr_extended_keyboard_v1::Event::PeekKey {
            serial,
            time: MS,
            key,
            state,
        };
        (PEEK, event.into())
    }

    fn key(serial: u32, key: char, state: u32) -> (u32, AnyEvent) {
        let key = keymap::keycode(k(key)).0;
        let event = wl_keyboard::Event::Key {
            serial,
            time: MS,
            key,
            state,
        };
        (KEYBOARD, event.into())
    }

    fn modifiers(keyboard: u32, serial: u32, mods_depressed: u32) -> (u32, AnyEvent) {
        let event = wl_keyboard::Event::Modifiers {
            serial,
            mods_depressed,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        (keyboard, event.into())
    }

    fn field(exo: &Exo) -> &Field {
        &exo.text_inputs[&TI].field
    }

    fn predicted(exo: &Exo) -> &Predicted {
        &exo.text_inputs[&TI].text.predicted
    }

    /// A prediction of `text`, with the cursor at `cursor` and the
    /// composition, if any, at `composition`.
    fn predicts(text: &str, cursor: usize, composition: Option<Range<usize>>) -> Predicted {
        let text = text.to_owned();
        Predicted {
            text,
            selection: Selection::caret(cursor),
            composition,
        }
    }

    #[test]
    fn a_hidden_region_stands_until_the_tracker_changes_its_composition() {
        type Change = fn(&mut Tracker);
        let changes: [(&str, Change); 5] = [
            ("cancel", Tracker::cancel),
            ("insert", |t| t.insert("x")),
            ("confirm", Tracker::confirm),
            ("delete", |t| t.delete(0..1)),
            ("a report it cannot match", |t| {
                t.update("other".to_owned(), 0, 0)
            }),
        ];
        for (change, apply) in changes {
            let mut tracker = Tracker::default();
            tracker.update("0a".to_owned(), 2, 2);
            tracker.compose_existing(1..2, true);
            assert!(tracker.hidden.is_some());
            apply(&mut tracker);
            assert!(tracker.hidden.is_none(), "{change}");
        }
    }

    #[test]
    fn e1_activate_moves_the_input_method_only_to_another_surface() {
        let mut exo = exo();
        send(&mut exo, TI, activate(A));
        assert_eq!(events(&mut exo), [enter(A)]);
        send(&mut exo, TI, activate(A));
        assert_eq!(events(&mut exo), []);
        // The seat does not focus B, so nothing attaches there until it
        // does.
        send(&mut exo, TI, activate(B));
        assert_eq!(events(&mut exo), [leave()]);
        exo.set_text_input_focus(Some(B));
        assert_eq!(events(&mut exo), [enter(B)]);
    }

    #[test]
    fn e2_attaching_sets_text_and_keeps_the_flags_learning_and_support() {
        let mut exo = exo();
        send(&mut exo, TI, activate(A));
        send(&mut exo, EXT, support(false));
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_PASSWORD));
        let typed = field(&exo).content_type;
        send(
            &mut exo,
            TI,
            zwp_text_input_v1::Request::Deactivate { seat: SEAT },
        );
        let detached = ContentType {
            input_type: extended::INPUT_TYPE_NONE,
            input_mode: extended::INPUT_MODE_DEFAULT,
            ..typed
        };
        assert_eq!(field(&exo).content_type, detached);
        send(&mut exo, TI, activate(A));
        let attached = ContentType {
            input_type: extended::INPUT_TYPE_TEXT,
            input_mode: extended::INPUT_MODE_TEXT,
            ..typed
        };
        assert_eq!(field(&exo).content_type, attached);
        assert!(!field(&exo).surrounding_text_supported);
        assert_eq!(events(&mut exo), [enter(A), leave(), enter(A)]);
    }

    #[test]
    fn e2_exo_detaches_on_focus_loss_surface_end_and_input_method_end() {
        let mut exo = exo();
        let detaches: [fn(&mut Exo); 3] = [
            |exo| exo.set_text_input_focus(Some(B)),
            |exo| exo.forget(|id| id != A),
            |exo| assert!(exo.destroy_input_method()),
        ];
        for detach in detaches {
            exo.set_text_input_focus(Some(A));
            send(&mut exo, TI, activate(A));
            assert_eq!(events(&mut exo), [enter(A)]);
            detach(&mut exo);
            assert_eq!(events(&mut exo), [leave()]);
            assert_eq!(
                field(&exo).content_type.input_type,
                extended::INPUT_TYPE_NONE
            );
        }
        // A surface that ends unfocused takes its activations with it, so
        // a new surface with its ID gets none.
        send(&mut exo, TI, activate(B));
        exo.forget(|id| id != B);
        exo.set_text_input_focus(Some(B));
        assert_eq!(events(&mut exo), []);
    }

    #[test]
    fn e3_a_content_type_spends_the_pending_support_and_needs_an_input_method() {
        let mut exo = exo();
        send(&mut exo, EXT, support(false));
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_PASSWORD));
        assert_eq!(
            field(&exo).content_type.input_type,
            extended::INPUT_TYPE_NONE
        );
        send(&mut exo, TI, activate(A));
        let content_type = zwp_text_input_v1::Request::SetContentType {
            hint: zwp_text_input_v1::CONTENT_HINT_NONE,
            purpose: zwp_text_input_v1::CONTENT_PURPOSE_URL,
        };
        send(&mut exo, TI, content_type);
        assert_eq!(
            field(&exo).content_type.input_type,
            extended::INPUT_TYPE_URL
        );
        assert!(field(&exo).surrounding_text_supported);
        send(&mut exo, EXT, support(false));
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_EMAIL));
        assert!(!field(&exo).surrounding_text_supported);
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_EMAIL));
        assert!(field(&exo).surrounding_text_supported);
    }

    #[test]
    fn e4_exo_predicts_the_text_from_reports_and_the_input_methods_edits() {
        let mut exo = active(V10);
        send(&mut exo, TI, surrounding("ab", 1));
        press(&mut exo, 'r', &[commits("가")], true);
        press(&mut exo, 's', &[shows("ㄴ")], true);
        assert_eq!(*predicted(&exo), predicts("a가ㄴb", 7, Some(4..7)));
        // A report of a state an edit led to is the client catching up.
        send(&mut exo, TI, surrounding("a가b", 4));
        assert_eq!(*predicted(&exo), predicts("a가ㄴb", 7, Some(4..7)));
        // A cancelled composition stays, as text.
        send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
        assert_eq!(*predicted(&exo), predicts("a가ㄴb", 7, None));
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        // Any other report replaces the prediction, composition and all, so
        // a detach then has nothing to confirm.
        press(&mut exo, 'e', &[shows("ㄷ")], true);
        send(&mut exo, TI, surrounding("xyz", 3));
        assert_eq!(*predicted(&exo), predicts("xyz", 3, None));
        events(&mut exo);
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [leave()]);
        // Without an input method, Exo keeps the reports too.
        send(&mut exo, TI, surrounding("uv", 0));
        let rectangle = zwp_text_input_v1::Request::SetCursorRectangle {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        send(&mut exo, TI, rectangle);
        assert_eq!(predicted(&exo).text, "uv");
        assert_eq!(field(&exo).surrounding_text, ("uv".to_owned(), 0, 0));
        assert_eq!(field(&exo).cursor_rectangle, (1, 2, 3, 4));
    }

    #[test]
    fn a_reset_forgets_the_states_an_edit_led_to_and_a_confirmation_keeps_them() {
        let typed = |unhandled: bool| {
            let mut exo = active(V10);
            send(&mut exo, TI, surrounding("ab", 1));
            press(&mut exo, 'r', &[commits("가")], true);
            press(&mut exo, 's', &[shows("ㄴ")], true);
            if unhandled {
                press(&mut exo, 'a', &[], false);
            } else {
                send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
            }
            exo
        };
        // A reset leaves no state to catch up to, so a report of an
        // earlier state replaces the prediction.
        let mut exo = typed(false);
        assert_eq!(exo.text_inputs[&TI].text.expected.len(), 0);
        send(&mut exo, TI, surrounding("a가b", 4));
        assert_eq!(*predicted(&exo), predicts("a가b", 4, None));
        // A confirmation adds a state, and the report is still catching up.
        let mut exo = typed(true);
        assert_eq!(exo.text_inputs[&TI].text.expected.len(), 4);
        send(&mut exo, TI, surrounding("a가b", 4));
        assert_eq!(*predicted(&exo), predicts("a가ㄴb", 7, None));
    }

    /// `SurroundingTextTracker::CancelComposition` clears
    /// `expected_updates_` and pushes nothing, so `Update` finds no state:
    /// even a report of the prediction resets it (`ResetInternal`), and
    /// one of the state before a later edit undoes that edit.
    #[test]
    fn cancel_empties_the_history() {
        let mut tracker = Tracker::default();
        tracker.update("ab".to_owned(), 1, 1);
        tracker.set_composition("x", Selection::caret(1));
        tracker.cancel();
        assert_eq!(tracker.expected, []);
        assert_eq!(tracker.predicted, predicts("axb", 2, None));
        tracker.update("axb".to_owned(), 2, 2);
        assert_eq!(tracker.expected, [predicts("axb", 2, None)]);
        // An edit after a cancel leaves a history of its own state only.
        tracker.cancel();
        tracker.insert("y");
        assert_eq!(tracker.expected, [predicts("axyb", 3, None)]);
        tracker.update("axb".to_owned(), 2, 2);
        assert_eq!(tracker.predicted, predicts("axb", 2, None));
        assert!(!tracker.dropped);
    }

    /// `SetSurroundingTextImpl` reports `minmax(cursor, anchor)`, and
    /// `SurroundingTextTracker::Update` compares selections with
    /// `gfx::Range::operator==`: a report matches a state with that range
    /// whichever way the client drew it, and resets with it.
    #[test]
    fn update_keeps_a_reported_selection_as_its_range() {
        let mut tracker = Tracker::default();
        tracker.update("abcd".to_owned(), 3, 1);
        let selected = Selection { start: 1, end: 3 };
        assert_eq!(tracker.predicted.selection, selected);
        tracker.set_composition("x", Selection::caret(1));
        assert_eq!(tracker.predicted.text, "axd");
        // A report of the state before the edit, drawn the other way, is the
        // client catching up.
        tracker.update("abcd".to_owned(), 1, 3);
        assert_eq!(tracker.expected.len(), 2);
        assert_eq!(tracker.predicted.composition(), "x");
    }

    /// `OnSetCompositionText` replaces the selection when nothing is
    /// composed, even with an empty composition, and sets the selection
    /// it is given, reversed or not, within the composition.
    #[test]
    fn set_composition_replaces_the_selection_and_keeps_a_directed_one() {
        let mut tracker = Tracker::default();
        tracker.update("abcd".to_owned(), 1, 3);
        tracker.set_composition("", Selection::caret(0));
        assert_eq!(tracker.predicted, predicts("ad", 1, None));
        tracker.set_composition("xy", Selection { start: 2, end: 0 });
        let reversed = Selection { start: 3, end: 1 };
        assert_eq!(tracker.predicted.selection, reversed);
        assert_eq!(tracker.predicted.composition(), "xy");
        // A report's range is forward, so it names no reversed state.
        tracker.update("axyd".to_owned(), 1, 3);
        assert_eq!(tracker.expected.len(), 1);
        assert!(tracker.dropped);
    }

    /// `OnInsertText` replaces the selection, with a composition it meets
    /// (`gfx::Range::Intersects`), and erases one apart from it, which the
    /// selection then follows.
    #[test]
    fn insert_replaces_the_selection_and_ends_the_composition() {
        let mut tracker = Tracker::default();
        tracker.update("abcd".to_owned(), 3, 1);
        tracker.insert("x");
        assert_eq!(tracker.predicted, predicts("axd", 2, None));
        tracker.update("abc".to_owned(), 3, 3);
        tracker.compose_existing(0..1, false);
        tracker.insert("x");
        assert_eq!(tracker.predicted, predicts("bcx", 3, None));
        tracker.update("abc".to_owned(), 1, 1);
        tracker.compose_existing(1..2, false);
        tracker.insert("x");
        assert_eq!(tracker.predicted, predicts("axc", 2, None));
    }

    /// `OnExtendSelectionAndDelete` widens the deleted range over a
    /// composition it meets, and otherwise erases the composition and
    /// moves the range with the text after it, where Exo's own
    /// `delete_surrounding_text` names only the range.
    #[test]
    fn delete_ends_the_composition_with_its_text() {
        let composed = |range: Range<usize>| {
            let mut tracker = Tracker::default();
            tracker.update("abcde".to_owned(), 3, 3);
            tracker.compose_existing(range, false);
            tracker
        };
        // Apart: the composition "a" goes, and so does "d", after it.
        let mut tracker = composed(0..1);
        tracker.delete(3..4);
        assert_eq!(tracker.predicted, predicts("bce", 2, None));
        // Apart, after the range, and with nothing deleted.
        let mut tracker = composed(3..4);
        tracker.delete(1..2);
        assert_eq!(tracker.predicted, predicts("ace", 1, None));
        let mut tracker = composed(1..3);
        tracker.delete(3..3);
        assert_eq!(tracker.predicted, predicts("ade", 1, None));
        // Met: the range takes the whole composition.
        let mut tracker = composed(1..3);
        tracker.delete(2..4);
        assert_eq!(tracker.predicted, predicts("ae", 1, None));
        assert_eq!(tracker.expected.len(), 3);
    }

    #[test]
    fn intersects_follows_gfx_range() {
        // Overlap; an empty range inside or at the start; none at the end.
        assert!(intersects(&(0..2), &(1..3)));
        assert!(intersects(&(1..3), &(1..1)));
        assert!(intersects(&(1..3), &(2..2)));
        assert!(!intersects(&(1..3), &(3..3)));
        assert!(!intersects(&(1..3), &(3..5)));
        assert!(!intersects(&(1..3), &(0..1)));
    }

    /// The text and the handled callback of key `id`.
    fn answer(exo: &mut Exo, id: u64, actions: Vec<TextAction>) -> Answer {
        assert_eq!(exo.answer(&EngineOut::Text { id, actions }), Answer::Normal);
        exo.answer(&EngineOut::Done { id, handled: true })
    }

    #[test]
    fn e5_a_reset_that_leaves_the_engine_alone_makes_no_answer_late() {
        // IMA composed nothing, so its reset does not reset the engine, and
        // the key in flight is answered from the composition it had.
        let mut exo = active(V10);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
        assert_eq!(exo.take_engine(), []);
        assert_eq!(answer(&mut exo, 0, vec![shows("ㅏ")]), Answer::Normal);
    }

    #[test]
    fn e5_a_reset_is_silent_and_drops_the_text_of_a_key_only_once_it_came() {
        let reset = || zwp_text_input_v1::Request::Reset {};
        // Before the engine sends a key's text, the reset cancels nothing
        // of it: the text goes out late, with the key.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        send(&mut exo, TI, reset());
        assert_eq!(events(&mut exo), []);
        assert_eq!(predicted(&exo).composition, None);
        // IMA was composing, so the engine resets too.
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        assert_eq!(answer(&mut exo, 1, vec![shows("가")]), Answer::Late);
        let [cursor, string] = preedit(3, "가");
        assert_eq!(events(&mut exo), [peek(1, 'k', DOWN), cursor, string]);
        // Between the text and the callback, the reset drops the text, and
        // the key goes out handled with none.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        let actions = vec![shows("가")];
        assert_eq!(
            exo.answer(&EngineOut::Text { id: 1, actions }),
            Answer::Normal
        );
        send(&mut exo, TI, reset());
        let done = EngineOut::Done {
            id: 1,
            handled: true,
        };
        assert_eq!(exo.answer(&done), Answer::Dropped);
        assert_eq!(events(&mut exo), [peek(1, 'k', DOWN)]);
        // With nothing composed, the engine is left alone.
        let mut exo = active(V10);
        send(&mut exo, TI, reset());
        assert_eq!(exo.take_engine(), []);
    }

    #[test]
    fn an_unhandled_press_shows_the_late_text_that_its_confirmation_shows() {
        // The reset, of an engine that composed, falls before the key's
        // text, which comes after it and is shown by the confirmation, not
        // by the flush.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, k('a'), DOWN);
        exo.take_engine();
        send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
        let actions = vec![shows("ㅎ")];
        assert_eq!(
            exo.answer(&EngineOut::Text { id: 1, actions }),
            Answer::Normal
        );
        let late = EngineOut::Done {
            id: 1,
            handled: false,
        };
        assert_eq!(exo.answer(&late), Answer::Late);
        let done = EngineOut::Done {
            id: 0,
            handled: false,
        };
        // A key whose text came before the reset is not late.
        let mut exo = active(V10);
        exo.key(NOW, k('a'), DOWN);
        exo.take_engine();
        let actions = vec![shows("ㅎ")];
        assert_eq!(
            exo.answer(&EngineOut::Text { id: 0, actions }),
            Answer::Normal
        );
        assert_eq!(exo.answer(&done), Answer::Normal);
    }

    #[test]
    fn a_change_of_the_served_text_input_drops_the_text_of_a_key_in_flight() {
        let mut exo = active(V11);
        let create = zwp_text_input_manager_v1::Request::CreateTextInput { id: TI2 };
        send(&mut exo, TI_MANAGER, create);
        exo.key(NOW, k('r'), DOWN);
        exo.take_engine();
        let actions = vec![commits("가")];
        assert_eq!(
            exo.answer(&EngineOut::Text { id: 0, actions }),
            Answer::Normal
        );
        send(&mut exo, TI2, activate(A));
        let done = EngineOut::Done {
            id: 0,
            handled: true,
        };
        assert_eq!(exo.answer(&done), Answer::Dropped);
    }

    #[test]
    fn e6_a_detach_confirms_the_composition_before_the_leave() {
        // From v11 of the extension, always, so often with nothing composed.
        let mut exo = active(V11);
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [confirm(), leave()]);
        // Before it, only a composition, as the cursor and a commit.
        let mut exo = active(V10);
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [leave()]);
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [cursor(3), commit(3, "가"), leave()]);
        assert_eq!(exo.take_engine(), []);
        // The end of the input method confirms too.
        let mut exo = active(V11);
        assert!(exo.destroy_input_method());
        assert_eq!(events(&mut exo), [confirm(), leave()]);
    }

    #[test]
    fn a_detach_confirms_but_leaves_the_engine_to_the_next_attach() {
        // The detach drops the type first, so IMA's reset returns early:
        // the engine is not reset, a key in flight is not late, and IMA
        // still takes the engine to be composing.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [cursor(3), commit(3, "가"), leave()]);
        assert_eq!(exo.take_engine(), []);
        assert!(!exo.in_engine[0].late);
        assert!(exo.ima.composing);
        // The next attach resets it, as real IMA's does.
        send(&mut exo, TI, activate(A));
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        assert!(exo.in_engine[0].late);
    }

    #[test]
    fn a_restart_of_the_input_method_resets_the_engine_only_through_its_confirmation() {
        // Detached by a deactivate, nothing confirms: the engine is not
        // reset, and the key in flight never gets its callback.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        send(&mut exo, TI, deactivate());
        events(&mut exo);
        assert!(!exo.destroy_input_method());
        assert_eq!(exo.take_engine(), []);
        send(&mut exo, TI, activate(A));
        assert_eq!(events(&mut exo), [enter(A)]);
        assert_eq!(exo.take_engine(), []);
        let id = exo.in_engine[0].id;
        let text = EngineOut::Text {
            id,
            actions: vec![shows("가")],
        };
        exo.answer(&text);
        exo.answer(&EngineOut::Done { id, handled: true });
        assert_eq!(events(&mut exo), []);
        // Attached, the restart confirms, which resets the engine once.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        assert!(exo.destroy_input_method());
        assert_eq!(events(&mut exo), [cursor(3), commit(3, "가"), leave()]);
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
    }

    /// The engine's answer to request `id`: its text `actions`, then its
    /// callback; returns the verdict on the text.
    fn answer_request(exo: &mut Exo, id: u64, actions: Vec<TextAction>) -> Answer {
        let verdict = exo.answer(&EngineOut::Text { id, actions });
        let done = EngineOut::Done { id, handled: true };
        assert_eq!(exo.answer(&done), Answer::Normal);
        verdict
    }

    #[test]
    fn e10_a_suggestion_deletes_from_an_absolute_index_then_commits() {
        let mut exo = active(V11);
        send(&mut exo, TI, surrounding("0ab가1", 6));
        exo.ask(Request::AcceptSuggestion);
        let request = EngineIn::Request {
            request: Request::AcceptSuggestion,
            id: 0,
            before: "0ab가".to_owned(),
        };
        assert_eq!(exo.take_engine(), [request]);
        // Three UTF-16 units, five bytes.
        let delete = TextAction::Delete {
            before: 3,
            after: 0,
        };
        let actions = vec![delete, commits("ab가~")];
        // No key is in flight, so IMA sends the text at once.
        assert_eq!(answer_request(&mut exo, 0, actions), Answer::Normal);
        let deleted = zwp_text_input_v1::Event::DeleteSurroundingText {
            index: 1,
            length: 5,
        };
        assert_eq!(events(&mut exo), [(TI, deleted.into()), commit(1, "ab가~")]);
        assert_eq!(*predicted(&exo), predicts("0ab가~1", 7, None));
    }

    #[test]
    fn a_request_answered_while_a_key_is_in_flight_waits_for_it_but_its_delete() {
        let mut exo = active(V11);
        send(&mut exo, TI, surrounding("0ab", 3));
        exo.ask(Request::AcceptSuggestion);
        exo.key(NOW, k('r'), DOWN);
        exo.take_engine();
        let delete = TextAction::Delete {
            before: 2,
            after: 0,
        };
        answer_request(&mut exo, 0, vec![delete, commits("ab~")]);
        let deleted = zwp_text_input_v1::Event::DeleteSurroundingText {
            index: 1,
            length: 2,
        };
        assert_eq!(events(&mut exo), [(TI, deleted.into())]);
        // The commit goes out after the key's peek, with its text.
        let text = EngineOut::Text {
            id: 1,
            actions: vec![shows("ㄱ")],
        };
        exo.answer(&text);
        exo.answer(&EngineOut::Done {
            id: 1,
            handled: true,
        });
        let mut expected = vec![peek(1, 'r', DOWN), commit(2, "ab~")];
        expected.extend(preedit(3, "ㄱ"));
        assert_eq!(events(&mut exo), expected);
        // A range held for a key goes out after the key's commit, which
        // makes it stale, and counts.
        exo.ask(Request::Reconvert);
        exo.key(NOW, k(' '), DOWN);
        exo.take_engine();
        let region = TextAction::Region {
            before: 1,
            after: 0,
        };
        answer_request(&mut exo, 2, vec![region]);
        assert_eq!(events(&mut exo), []);
        let text = EngineOut::Text {
            id: 3,
            actions: vec![commits("ㄱ")],
        };
        exo.answer(&text);
        exo.answer(&EngineOut::Done {
            id: 3,
            handled: true,
        });
        events(&mut exo);
        // While IMA takes the engine to compose, a delete is dropped.
        exo.ask(Request::AcceptSuggestion);
        exo.take_engine();
        let delete = TextAction::Delete {
            before: 1,
            after: 0,
        };
        answer_request(&mut exo, 4, vec![delete]);
        assert_eq!(events(&mut exo), []);
    }

    #[test]
    fn a_reconversion_composes_existing_text_through_the_extension() {
        let mut exo = active(V10);
        send(&mut exo, TI, surrounding("0a가", 5));
        exo.ask(Request::Reconvert);
        exo.take_engine();
        let region = TextAction::Region {
            before: 1,
            after: 0,
        };
        answer_request(&mut exo, 0, vec![region]);
        let event = extended::Event::SetPreeditRegion {
            index: -3,
            length: 3,
        };
        assert_eq!(events(&mut exo), [(EXT, event.into())]);
        assert_eq!(*predicted(&exo), predicts("0a가", 5, Some(2..5)));
        assert_eq!(
            exo.take_region_outcome(),
            Some(RegionOutcome::Composed("가".to_owned()))
        );
        assert!(exo.ima.composing);
        // One past the text is refused before anything is sent, which the
        // engine hears.
        exo.ask(Request::Reconvert);
        exo.take_engine();
        let region = TextAction::Region {
            before: 4,
            after: 0,
        };
        answer_request(&mut exo, 1, vec![region]);
        assert_eq!(events(&mut exo), []);
        assert_eq!(exo.take_region_outcome(), Some(RegionOutcome::Refused));
        assert_eq!(*predicted(&exo), predicts("0a가", 5, Some(2..5)));
    }

    #[test]
    fn the_engine_gets_no_request_without_a_text_input_and_a_hidden_region_without_an_extension() {
        let mut exo = exo();
        exo.ask(Request::AcceptSuggestion);
        assert_eq!(exo.take_engine(), [], "no text input attached");
        exo.extensions.clear();
        send(&mut exo, TI, activate(A));
        send(&mut exo, TI, surrounding("0a", 2));
        events(&mut exo);
        exo.take_engine();
        // Without an extension, Exo composes the region, and sends nothing.
        exo.ask(Request::Reconvert);
        assert_eq!(exo.take_engine().len(), 1);
        let region = TextAction::Region {
            before: 1,
            after: 0,
        };
        answer_request(&mut exo, 0, vec![region]);
        assert_eq!(events(&mut exo), []);
        assert_eq!(*predicted(&exo), predicts("0a", 2, Some(1..2)));
        assert_eq!(
            exo.take_region_outcome(),
            Some(RegionOutcome::Composed("a".to_owned()))
        );
        assert_eq!(exo.composition_shown_instead().as_deref(), Some(""));
        // Its commit ends it.
        exo.ask(Request::AcceptSuggestion);
        exo.take_engine();
        answer_request(&mut exo, 1, vec![commits("a")]);
        assert_eq!(exo.composition_shown_instead(), None);
        // A region over a composition the client shows leaves it shown,
        // through the regions after it too.
        exo.ask(Request::AcceptSuggestion);
        exo.take_engine();
        answer_request(&mut exo, 2, vec![shows("b")]);
        assert_eq!(exo.composition_shown_instead(), None);
        for (id, before) in [(3, 2), (4, 1)] {
            exo.ask(Request::Reconvert);
            exo.take_engine();
            answer_request(&mut exo, id, vec![TextAction::Region { before, after: 0 }]);
            assert_eq!(exo.composition_shown_instead().as_deref(), Some("b"));
        }
        assert_eq!(exo.composition().as_deref(), Some("b"));
    }

    #[test]
    fn e13_an_autorepeat_goes_to_the_engine_and_no_further() {
        let mut exo = active(V11);
        let repeat = |exo: &mut Exo, actions: &[TextAction], handled| {
            assert!(exo.autorepeat(NOW, k(' ')));
            let [EngineIn::Key {
                id, pressed: true, ..
            }] = exo.take_engine()[..]
            else {
                panic!("the repeat went to the engine as a press");
            };
            if !actions.is_empty() {
                let actions = actions.to_vec();
                exo.answer(&EngineOut::Text { id, actions });
            }
            exo.answer(&EngineOut::Done { id, handled });
            events(exo)
        };
        // Unhandled, it is confirmed (E13), and the keyboard drops it (E7).
        assert_eq!(repeat(&mut exo, &[], false), [confirm()]);
        // Handled, only its text goes out.
        assert_eq!(repeat(&mut exo, &[shows("ㄱ")], true), preedit(1, "ㄱ"));
        // With no text input attached, the engine gets none.
        send(&mut exo, TI, deactivate());
        events(&mut exo);
        assert!(!exo.autorepeat(NOW, k(' ')));
        assert_eq!(exo.take_engine(), []);
    }

    #[test]
    fn a_text_input_that_ends_leaves_the_engine_alone_too() {
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        exo.forget(|id| id != TI);
        assert_eq!(exo.take_engine(), []);
        assert!(!exo.in_engine[0].late);
        assert!(exo.ima.composing);
        assert_eq!(exo.ima.client, None);
    }

    #[test]
    fn a_latin_layout_has_no_engine_and_its_keys_go_out_at_once() {
        let mut exo = active(V11);
        exo.latin = true;
        exo.key(NOW, k('r'), DOWN);
        exo.key(NOW, k('r'), UP);
        assert_eq!(exo.take_engine(), []);
        // No `confirm_preedit` before an unhandled press, no callback.
        let raw = [
            peek(1, 'r', DOWN),
            key(1, 'r', DOWN),
            peek(2, 'r', UP),
            key(2, 'r', UP),
        ];
        assert_eq!(events(&mut exo), raw);
        assert!(exo.in_engine.is_empty());
    }

    #[test]
    fn a_release_whose_press_exo_never_tracked_sends_nothing() {
        // `Keyboard::OnKeyEvent` ignores it, with no input method to hand
        // it to, with a Latin layout, and through the engine.
        let mut exo = exo();
        exo.key(NOW, k('r'), UP);
        assert_eq!(events(&mut exo), []);
        let mut exo = active(V10);
        exo.latin = true;
        exo.key(NOW, k('r'), UP);
        assert_eq!(events(&mut exo), []);
        let mut exo = active(V10);
        exo.latin = false;
        exo.key(NOW, k('r'), UP);
        exo.take_engine();
        let done = EngineOut::Done {
            id: 0,
            handled: false,
        };
        assert_eq!(exo.answer(&done), Answer::Normal);
        assert_eq!(events(&mut exo), []);
    }

    #[test]
    fn a_changed_content_type_resets_the_context_of_the_text_input_it_serves() {
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        // A new type drops the buffers, marks the key late and resets the
        // engine, which IMA took to be composing.
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_NUMBER));
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        assert!(exo.in_engine[0].late);
        // The same type changes nothing.
        exo.in_engine[0].late = false;
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_NUMBER));
        assert_eq!(exo.take_engine(), []);
        assert!(!exo.in_engine[0].late);
        // Nor does it without an input method, which drops the type.
        let mut exo = exo_for(V10);
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_NUMBER));
        assert!(exo.take_engine().is_empty());
    }

    #[test]
    fn a_changed_surrounding_text_support_or_a_type_of_none_resets_as_the_type_does() {
        let composing = || {
            let mut exo = active(V10);
            press(&mut exo, 'r', &[shows("ㄱ")], true);
            exo.key(NOW, k('k'), DOWN);
            exo.take_engine();
            exo
        };
        // The support alone counts, once a request spends it.
        let mut exo = composing();
        send(&mut exo, EXT, support(false));
        assert_eq!(exo.take_engine(), []);
        let same = extended::Request::SetInputType {
            input_type: extended::INPUT_TYPE_TEXT,
            input_mode: extended::INPUT_MODE_TEXT,
            input_flags: extended::INPUT_FLAGS_NONE,
            learning_mode: extended::LEARNING_MODE_ENABLED,
            inline_composition_support: extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        };
        send(&mut exo, EXT, same);
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        // With the type none, `ResetContext` returns early.
        let mut exo = composing();
        send(&mut exo, EXT, input_type(extended::INPUT_TYPE_NONE));
        assert_eq!(exo.take_engine(), []);
        assert!(!exo.in_engine[0].late);
    }

    #[test]
    fn e7_a_key_reaches_the_keyboard_raw_while_no_text_input_is_attached() {
        let mut exo = exo();
        exo.key(NOW, k('r'), DOWN);
        exo.key(NOW, k('r'), UP);
        assert_eq!(exo.take_engine(), []);
        let raw = [
            peek(1, 'r', DOWN),
            key(1, 'r', DOWN),
            peek(2, 'r', UP),
            key(2, 'r', UP),
        ];
        assert_eq!(events(&mut exo), raw);
        // A shifted key comes with Shift held, which changes only once.
        for (c, state) in [('R', DOWN), ('Q', DOWN), ('R', UP), ('Q', UP)] {
            exo.key(NOW, k(c), state);
        }
        let shifted = [
            modifiers(KEYBOARD, 3, keymap::SHIFT),
            peek(4, 'R', DOWN),
            key(4, 'R', DOWN),
            peek(5, 'Q', DOWN),
            key(5, 'Q', DOWN),
            peek(6, 'R', UP),
            key(6, 'R', UP),
            modifiers(KEYBOARD, 7, 0),
            peek(8, 'Q', UP),
            key(8, 'Q', UP),
        ];
        assert_eq!(events(&mut exo), shifted);
    }

    #[test]
    fn e7_a_press_of_a_held_key_goes_nowhere() {
        // `Keyboard::OnKeyEvent` passes a press only of a key it does not
        // hold, so a second press before the release sends nothing.
        let mut exo = exo();
        exo.key(NOW, k('r'), DOWN);
        exo.key(NOW, k('r'), DOWN);
        exo.key(NOW, k('r'), UP);
        let raw = [
            peek(1, 'r', DOWN),
            key(1, 'r', DOWN),
            peek(2, 'r', UP),
            key(2, 'r', UP),
        ];
        assert_eq!(events(&mut exo), raw);
        // Nor does the engine's answer to a press of a key pressed raw
        // since, though IMA still confirms before it.
        let mut exo = active(V11);
        exo.key(NOW, k(' '), DOWN);
        let [EngineIn::Key { id, .. }] = exo.take_engine()[..] else {
            panic!("the key went to the engine");
        };
        send(&mut exo, TI, deactivate());
        exo.key(NOW, k(' '), DOWN);
        events(&mut exo);
        exo.answer(&EngineOut::Done { id, handled: false });
        assert_eq!(events(&mut exo), []);
    }

    #[test]
    fn e7_a_key_through_the_engine_goes_out_with_its_callback() {
        let mut exo = active(V10);
        exo.key(NOW, k('r'), DOWN);
        let r = |pressed, id| EngineIn::Key {
            key: k('r'),
            pressed,
            id,
        };
        assert_eq!(exo.take_engine(), [r(true, 0)]);
        let actions = vec![shows("ㄱ")];
        assert_eq!(
            exo.answer(&EngineOut::Text { id: 0, actions }),
            Answer::Normal
        );
        assert_eq!(events(&mut exo), [], "nothing before the callback");
        assert_eq!(
            exo.answer(&EngineOut::Done {
                id: 0,
                handled: true
            }),
            Answer::Normal
        );
        let [cursor, string] = preedit(2, "ㄱ");
        assert_eq!(events(&mut exo), [peek(1, 'r', DOWN), cursor, string]);
        // The release goes as its press did, consumed, with the same
        // serial, which only a key sent frees.
        exo.key(NOW, k('r'), UP);
        assert_eq!(exo.take_engine(), [r(false, 1)]);
        assert_eq!(
            exo.answer(&EngineOut::Done {
                id: 1,
                handled: false
            }),
            Answer::Normal
        );
        assert_eq!(events(&mut exo), [peek(1, 'r', UP)]);
        let enter = press(&mut exo, '⏎', &[commits("ㄱ")], false);
        let passed = [commit(3, "ㄱ"), peek(1, '⏎', DOWN), key(1, '⏎', DOWN)];
        assert_eq!(enter, passed);
    }

    #[test]
    fn e8_a_handled_press_precedes_its_text_and_an_unhandled_one_follows_it() {
        let mut exo = active(V10);
        let [cursor, string] = preedit(2, "ㄱ");
        let handled = [peek(1, 'r', DOWN), cursor, string];
        assert_eq!(press(&mut exo, 'r', &[shows("ㄱ")], true), handled);
        let unhandled = [commit(3, "ㄱ"), peek(1, '⏎', DOWN), key(1, '⏎', DOWN)];
        assert_eq!(press(&mut exo, '⏎', &[commits("ㄱ")], false), unhandled);
    }

    #[test]
    fn an_unhandled_press_confirms_the_composition_before_it_goes_out() {
        // From v11, always, so usually as an empty confirmation.
        let mut exo = active(V11);
        let empty = [confirm(), peek(1, 'a', DOWN), key(1, 'a', DOWN)];
        assert_eq!(press(&mut exo, 'a', &[], false), empty);
        // Before it, only a composition, as the cursor and a commit.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("가")], true);
        let confirmed = [
            cursor(3),
            commit(3, "가"),
            peek(1, 'a', DOWN),
            key(1, 'a', DOWN),
        ];
        assert_eq!(press(&mut exo, 'a', &[], false), confirmed);
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        // Not when the engine buffered only a commit, which ends the
        // composition itself.
        let mut exo = active(V11);
        press(&mut exo, 'r', &[shows("가")], true);
        let committed = [commit(3, "가"), peek(1, '⏎', DOWN), key(1, '⏎', DOWN)];
        assert_eq!(press(&mut exo, '⏎', &[commits("가")], false), committed);
    }

    #[test]
    fn e9_the_engines_commit_is_a_commit_even_of_the_composition() {
        let mut exo = active(V11);
        press(&mut exo, 'r', &[shows("가")], true);
        let space = [peek(1, ' ', DOWN), commit(3, "가 ")];
        assert_eq!(press(&mut exo, ' ', &[commits("가 ")], true), space);
    }

    #[test]
    fn the_input_method_serves_the_text_input_that_attached_last() {
        let mut exo = active(V11);
        let create = zwp_text_input_manager_v1::Request::CreateTextInput { id: TI2 };
        send(&mut exo, TI_MANAGER, create);
        send(&mut exo, TI2, activate(A));
        // Moving to TI2 confirms TI's composition.
        let enter2 = zwp_text_input_v1::Event::Enter { surface: A };
        assert_eq!(events(&mut exo), [confirm(), (TI2, enter2.into())]);
        let [cursor, string] = preedit_on(TI2, 2, "ㄱ");
        let typed = [peek(1, 'r', DOWN), cursor, string];
        assert_eq!(press(&mut exo, 'r', &[shows("ㄱ")], true), typed);
        // TI keeps its activation, but its detach confirms nothing.
        send(&mut exo, TI, deactivate());
        assert_eq!(events(&mut exo), [leave()]);
    }

    #[test]
    fn e11_each_request_sends_its_events_at_once_in_order() {
        let mut exo = exo();
        send(&mut exo, TI, activate(A));
        send(&mut exo, TI, deactivate());
        send(&mut exo, TI, activate(A));
        assert_eq!(events(&mut exo), [enter(A), leave(), enter(A)]);
    }

    #[test]
    fn e12_a_focus_move_reaches_the_keyboard_before_the_text_inputs() {
        // The keyboard registered as the seat's observer before the text
        // input's first activate, so it hears of the focus first.
        let mut exo = active(V10);
        exo.move_focus(Some(B));
        let keyboard_leave = wl_keyboard::Event::Leave {
            serial: 1,
            surface: A,
        };
        let keyboard_enter = wl_keyboard::Event::Enter {
            serial: 3,
            surface: B,
            keys: Vec::new(),
        };
        let expected = vec![
            (KEYBOARD, keyboard_leave.into()),
            modifiers(KEYBOARD, 2, 0),
            (KEYBOARD, keyboard_enter.into()),
            leave(),
        ];
        assert_eq!(events(&mut exo), expected);
        assert!(!exo.text_inputs_hear_first());
    }

    #[test]
    fn e12_a_keyboard_bound_after_the_activate_hears_of_a_focus_move_second() {
        let mut exo = active(V10);
        send(
            &mut exo,
            SEAT,
            wl_seat::Request::GetKeyboard { id: KEYBOARD2 },
        );
        events(&mut exo);
        assert!(exo.text_inputs_hear_first());
        exo.move_focus(Some(B));
        let got = events(&mut exo);
        let leave_at = |sender| {
            got.iter().position(|(s, e)| {
                *s == sender
                    && matches!(
                        e,
                        AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. })
                            | AnyEvent::ZwpTextInputV1(zwp_text_input_v1::Event::Leave {})
                    )
            })
        };
        assert!(leave_at(TI) < leave_at(KEYBOARD2), "{got:?}");
        // The deactivate unregistered the text input; its next activate
        // registers it after the keyboard.
        send(&mut exo, TI, activate(B));
        assert!(!exo.text_inputs_hear_first());
    }

    #[test]
    fn a_keyboard_learns_the_seats_focus_at_once() {
        let mut exo = exo();
        send(
            &mut exo,
            SEAT,
            wl_seat::Request::GetKeyboard { id: KEYBOARD2 },
        );
        let enter = wl_keyboard::Event::Enter {
            serial: 2,
            surface: A,
            keys: Vec::new(),
        };
        let expected = [modifiers(KEYBOARD2, 1, 0), (KEYBOARD2, enter.into())];
        assert_eq!(events(&mut exo), expected);
    }

    #[test]
    fn exo_forgets_the_keyboard_and_extensions_that_end() {
        let mut exo = exo();
        exo.forget(|id| id != PEEK && id != EXT);
        send(&mut exo, EXT, support(false));
        assert!(exo.text_inputs[&TI].pending_support, "an ended extension");
        exo.key(NOW, k('r'), DOWN);
        assert_eq!(events(&mut exo), [key(1, 'r', DOWN)]);
        exo.forget(|id| id != KEYBOARD);
        exo.key(NOW, k('r'), UP);
        assert_eq!(events(&mut exo), []);
    }
}
