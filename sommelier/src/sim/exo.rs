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
//! content type. A generic compositor exposes none of that, so the fuzzer's
//! compositor keeps this model of each host `zwp_text_input_v1` to compare
//! with what the guest committed, and plays the rest of the host itself.
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
//! extension's version is the one the proxy binds; a case draws the one
//! the host advertises ([`draw_extension_version`]). Answers that belong to
//! the display, a sync's `done` and the `delete_id` of a destroyed object,
//! are left to whoever plays it, as is the keyboard's focus in the fuzzer.
//! The model is a test oracle, not a reimplementation: it keeps only the
//! state those read.
//!
//! Each rule cites Chromium's `components/exo/text_input.cc` (`TextInput`)
//! or `components/exo/wayland/zwp_text_input_manager.cc` (the request
//! handlers) at Chromium revision 137f56b0d966 (2026-11-17), IMA's rules
//! cite the functions of `input_method_ash.cc` at Chromium `main`
//! (2026-11-18), and the key path cites `components/exo/keyboard.cc`
//! (`Keyboard::OnKeyEvent`) and `components/exo/wayland/serial_tracker.cc`.
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
//! - IMA's character composer and password fields, its reset when the
//!   content type changes (`OnTextInputTypeChanged`), the engine's focus
//!   and blur, and a single-character commit sent as a key
//!   (`NeedInsertChar`), which the Korean engine never makes;
//! - `preedit_styling`, the keymap and key repeats, which Exo never sends,
//!   and the release of a keyboard or its extension.
//!
//! Where the model is stricter than Exo: it keeps the surrounding text's
//! cursor and anchor as sent, where Exo keeps the range between them, and
//! the cursor rectangle as sent, where `gfx::Rect` clamps a negative width
//! or height to 0.

use super::clock::Micros;
use super::engine::{EngineIn, EngineOut, TextAction};
use super::hangul::Key;
use super::keymap;
use super::sched::Choose;
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
pub(crate) struct Field {
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
pub(crate) struct ContentType {
    pub input_type: u32,
    pub input_mode: u32,
    pub flags: u32,
    pub learning: bool,
    pub inline_composition: bool,
}

/// The content type `text_input_set_content_type` makes of a v1 `hint`
/// and `purpose`, with each `ui::` value written as the extension's value
/// of the same name.
pub(crate) fn v1_content_type(hint: u32, purpose: u32) -> ContentType {
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
}

/// What `SurroundingTextTracker` (`ui/base/ime/surrounding_text_tracker.cc`)
/// predicts a field holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Predicted {
    /// The text, with the composition in it.
    text: String,
    cursor: usize,
    /// Where the composition is in `text`, if there is one.
    composition: Option<Range<usize>>,
}

impl Predicted {
    fn composition(&self) -> &str {
        self.composition
            .clone()
            .map_or("", |range| &self.text[range])
    }

    /// Puts `text` in place of the composition, or at the cursor, with the
    /// cursor after it; returns where it is.
    fn replace(&mut self, text: &str) -> Range<usize> {
        let range = self.composition.take().unwrap_or(self.cursor..self.cursor);
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        range.start..self.cursor
    }
}

/// `SurroundingTextTracker`: the prediction, and the states that the input
/// method's edits since the last report led to, oldest first.
#[derive(Default)]
struct Tracker {
    predicted: Predicted,
    expected: Vec<Predicted>,
}

impl Tracker {
    /// `Update`, for a report of the field: one that names a state an edit
    /// led to is the client catching up, which keeps the prediction, since
    /// the edits after that state lead to it; any other replaces the
    /// prediction, and with it the composition.
    fn update(&mut self, text: String, cursor: usize, anchor: usize) {
        let named = |state: &Predicted| {
            state.text == text && state.cursor == cursor && state.cursor == anchor
        };
        match self.expected.iter().position(named) {
            Some(state) => drop(self.expected.drain(..state)),
            None => {
                self.predicted = Predicted {
                    text,
                    cursor,
                    composition: None,
                };
                self.expected = vec![self.predicted.clone()];
            }
        }
    }

    /// `OnSetCompositionText`.
    fn set_composition(&mut self, text: &str) {
        let range = self.predicted.replace(text);
        self.predicted.composition = (!range.is_empty()).then_some(range);
        self.expected.push(self.predicted.clone());
    }

    /// `OnInsertText`.
    fn insert(&mut self, text: &str) {
        self.predicted.replace(text);
        self.expected.push(self.predicted.clone());
    }

    /// `OnConfirmCompositionText` and `CancelComposition`: the composition
    /// stays, as text.
    fn end_composition(&mut self) {
        self.predicted.composition = None;
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
        }
    }

    /// A content type from `set_input_type`, `deprecated_set_input_type`
    /// or v1 `set_content_type`. Each handler takes the pending support and
    /// resets it to supported before `SetTypeModeFlags`, which returns
    /// early, dropping the type, while no input method is attached.
    fn set_type_mode_flags(&mut self, content_type: ContentType) {
        let supported = std::mem::replace(&mut self.pending_support, true);
        if !self.field.attached {
            return;
        }
        self.field.content_type = content_type;
        self.field.surrounding_text_supported = supported;
    }
}

/// The `zcr_text_input_extension_v1` version a case's host advertises, if
/// any. Exo advertises v14, which the proxy binds at v11, its newest, so 11
/// comes first and stands for Exo; the older versions and none test that
/// the proxy copes with other hosts, one for each way it sends a content
/// type or a confirmation.
pub(super) fn draw_extension_version(choices: &mut impl Choose) -> Option<u32> {
    [Some(11), None, Some(7), Some(10)][choices.pick(4) as usize]
}

/// The seat's `wl_keyboard`. A case has one, as a client binds one.
#[derive(Clone, Copy)]
struct Keyboard {
    id: u32,
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
    /// `composing_text_`: whether IMA takes the engine to be composing.
    composing: bool,
}

/// A key handed to the engine that it has not answered.
struct PendingKey {
    id: u64,
    key: Key,
    pressed: bool,
    /// The key's time, in milliseconds.
    time: u32,
    /// Whether the engine sent the key's text.
    answered: bool,
    /// Whether IMA reset after Exo handed the engine the key and before its
    /// text came.
    late: bool,
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
    /// What Exo handed the engine and no one has taken yet, in order.
    to_engine: Vec<EngineIn>,
    /// The keys the engine has yet to answer, in order.
    in_engine: VecDeque<PendingKey>,
    /// The ID of the next key Exo hands the engine.
    next_key: u64,
}

impl Exo {
    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// Move the seat's focus, as Alt-Tab or a click on another window does:
    /// the keyboard leaves the old surface and enters the new one, and the
    /// text inputs follow as in [`Exo::set_text_input_focus`]. Whether the
    /// keyboard or the text inputs hear of it first is drawn per move
    /// (E12).
    pub(super) fn move_focus(&mut self, focus: Option<u32>, choices: &mut impl Choose) {
        let lost = self.focus;
        if lost == focus {
            return;
        }
        let keyboard_first = choices.pick(2) == 0;
        if !keyboard_first {
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
        if keyboard_first {
            self.set_text_input_focus(focus);
        }
    }

    /// The user presses or releases `key` at `now`, as `state` says. While
    /// the input method serves a text input, Exo hands the key to the
    /// engine and sends its events with the engine's callback
    /// ([`Exo::answer`]). Otherwise the input method has no client and
    /// passes the key, which reaches the keyboard at once (E7).
    pub(super) fn key(&mut self, now: Micros, key: Key, state: u32) {
        let pressed = state == wl_keyboard::KEY_STATE_PRESSED;
        let time = millis(now);
        if self.ima.client.is_none() {
            let consumed = if pressed {
                self.pressed.insert(keymap::keycode(key).0, false);
                false
            } else {
                self.release(key)
            };
            self.key_events(key, time, state, consumed);
            return;
        }
        let id = self.next_key;
        self.next_key += 1;
        self.in_engine.push_back(PendingKey {
            id,
            key,
            pressed,
            time,
            answered: false,
            late: false,
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
    /// Returns whether the callback sent text that the engine made before
    /// a reset that IMA made after Exo handed it the key: the late answer of
    /// E5.
    pub(super) fn answer(&mut self, out: &EngineOut) -> bool {
        match out {
            EngineOut::Text { id, actions } => {
                let key = self.in_engine.front_mut().expect("a key the engine has");
                assert_eq!(key.id, *id, "the engine answers in order");
                key.answered = true;
                if self.ima.client.is_some() {
                    self.buffer(actions);
                }
                false
            }
            &EngineOut::Done { id, handled } => {
                let key = self.in_engine.pop_front().expect("a key the engine has");
                assert_eq!(key.id, id, "the engine answers in order");
                let PendingKey { key: k, time, .. } = key;
                if !key.pressed {
                    let consumed = self.release(k);
                    self.key_events(k, time, wl_keyboard::KEY_STATE_RELEASED, consumed);
                    return false;
                }
                let code = keymap::keycode(k).0;
                self.pressed.insert(code, handled);
                let pressed = wl_keyboard::KEY_STATE_PRESSED;
                let sent = if handled {
                    self.key_events(k, time, pressed, true);
                    self.flush()
                } else {
                    if let Some(client) = self.ima.client {
                        self.confirm_composition(client);
                    }
                    let sent = self.flush();
                    self.key_events(k, time, pressed, false);
                    sent
                };
                key.late && sent
            }
        }
    }

    /// What Exo handed the engine since the last call, in order.
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
        // A text input that ends stops being served, and can be sent nothing.
        if self.ima.client.is_some_and(|id| !live(id)) {
            self.ima = Ima::default();
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
                self.keyboard = Some(Keyboard { id, extended: None });
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
    pub(crate) fn destroy_input_method(&mut self) -> bool {
        if let Some(client) = self.ima.client {
            self.confirm_composition(client);
        }
        self.ima = Ima::default();
        let mut detached = false;
        for id in self.ids() {
            if self.text_inputs[&id].field.attached {
                self.deactivate(id);
                detached = true;
            }
        }
        detached
    }

    /// The events Exo sent since the last call, in order. Whoever plays the
    /// display takes them after each input it hands the model, before it
    /// answers any later `wl_display.sync`, so that the sync's `done`
    /// follows every event of earlier requests (E11).
    pub(crate) fn take_events(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.events)
    }

    /// The fields of the text inputs active on the focused surface.
    pub(crate) fn focused_fields(&self) -> Vec<Field> {
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
        if let Some(text_input) = self.extended(sender) {
            text_input.set_type_mode_flags(content_type);
        }
    }

    fn text_input_request(&mut self, sender: u32, request: &zwp_text_input_v1::Request) {
        use zwp_text_input_v1::Request;
        match request {
            Request::SetContentType { hint, purpose } => {
                let content_type = v1_content_type(*hint, *purpose);
                self.text_input(sender).set_type_mode_flags(content_type);
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
        self.detach(id);
        self.text_input(id).surface = Some(surface);
        if self.focus == Some(surface) {
            self.attach(id);
        }
    }

    /// `TextInput::Deactivate`.
    fn deactivate(&mut self, id: u32) {
        self.detach(id);
        self.text_input(id).surface = None;
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

    /// `DetachInputMethod`, which does nothing without an input method. If
    /// the input method serves this text input, it stops, which confirms
    /// the composition first (E6). Then the delegate's `Deactivated` sends
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
        self.text_input(id).text.end_composition();
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
            self.ima.commit = None;
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
        commit.is_some() || composition.is_some()
    }

    /// `InputMethodAsh::ConfirmComposition(reset_engine = true)` for text
    /// input `id`, which IMA serves. With only a commit buffered, nothing
    /// happens. Otherwise a buffered composition is shown, the composition
    /// Exo tracks is confirmed if there is one, or always where the
    /// extension has `confirm_preedit` (`SupportsAlwaysConfirmComposition`),
    /// and IMA resets, keeping the buffered commit.
    fn confirm_composition(&mut self, id: u32) {
        if self.ima.commit.is_some() && self.ima.composition.is_none() {
            return;
        }
        if let Some(text) = self.ima.composition.take() {
            self.set_composition(id, &text);
        }
        let extension = self.confirming_extension(id);
        let composing = !self.text_input(id).text.predicted.composition().is_empty();
        if extension.is_some() || composing {
            self.confirm_composition_text(id, extension);
        }
        let commit = self.ima.commit.take();
        self.reset_context();
        self.ima.commit = commit;
    }

    /// `TextInput::ConfirmCompositionText(keep_selection = true)`: with
    /// `confirm_preedit` on `extension`, leaving the selection unchanged; or
    /// else, by the delegate's fallback, the cursor as `cursor_position`,
    /// then a `commit_string` of the composition.
    fn confirm_composition_text(&mut self, id: u32, extension: Option<u32>) {
        let text = &mut self.text_input(id).text;
        let composition = text.predicted.composition().to_owned();
        let cursor = i32::try_from(text.predicted.cursor).expect("a short text");
        text.end_composition();
        if let Some(extension) = extension {
            let unchanged = extended::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_UNCHANGED;
            self.events
                .push(extended::evt::confirm_preedit(extension, unchanged));
            return;
        }
        self.events
            .push(zwp_text_input_v1::evt::cursor_position(id, cursor, cursor));
        let serial = self.next_serial();
        let event = zwp_text_input_v1::evt::commit_string(id, serial, &composition);
        self.events.push(event.expect("a commit fits a message"));
    }

    /// `InputMethodAsh::ResetContext`, which does nothing without a text
    /// input to serve: the buffered text goes, and the engine resets if IMA
    /// took it to be composing.
    fn reset_context(&mut self) {
        if self.ima.client.is_none() {
            return;
        }
        self.ima.commit = None;
        self.ima.composition = None;
        for key in self.in_engine.iter_mut().filter(|key| !key.answered) {
            key.late = true;
        }
        if std::mem::take(&mut self.ima.composing) {
            self.to_engine.push(EngineIn::Reset);
        }
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
        self.text_input(id).text.set_composition(text);
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

    /// The release of `key`: whether the input method consumed its press.
    fn release(&mut self, key: Key) -> bool {
        let code = keymap::keycode(key).0;
        self.pressed.remove(&code).unwrap_or(false)
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
        (self.extension_version >= Some(11)).then_some(extension)
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
        let id = self.extensions.get(&extension)?;
        self.text_inputs.get_mut(id)
    }
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
    const V11: u32 = 11;
    const V10: u32 = 10;
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
            assert!(!exo.answer(&EngineOut::Text { id, actions }));
        }
        assert!(!exo.answer(&EngineOut::Done { id, handled }));
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
        let composing = Predicted {
            text: "a가ㄴb".to_owned(),
            cursor: 7,
            composition: Some(4..7),
        };
        assert_eq!(*predicted(&exo), composing);
        // A report of a state an edit led to is the client catching up.
        send(&mut exo, TI, surrounding("a가b", 4));
        assert_eq!(*predicted(&exo), composing);
        // A cancelled composition stays, as text.
        send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
        let cancelled = Predicted {
            composition: None,
            ..composing
        };
        assert_eq!(*predicted(&exo), cancelled);
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        // Any other report replaces the prediction, composition and all, so
        // a detach then has nothing to confirm.
        press(&mut exo, 'e', &[shows("ㄷ")], true);
        send(&mut exo, TI, surrounding("xyz", 3));
        let reported = Predicted {
            text: "xyz".to_owned(),
            cursor: 3,
            composition: None,
        };
        assert_eq!(*predicted(&exo), reported);
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

    /// The text and the handled callback of key `id`.
    fn answer(exo: &mut Exo, id: u64, actions: Vec<TextAction>) -> bool {
        assert!(!exo.answer(&EngineOut::Text { id, actions }));
        exo.answer(&EngineOut::Done { id, handled: true })
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
        assert!(answer(&mut exo, 1, vec![shows("가")]), "late");
        let [cursor, string] = preedit(3, "가");
        assert_eq!(events(&mut exo), [peek(1, 'k', DOWN), cursor, string]);
        // Between the text and the callback, the reset drops the text, and
        // the key goes out handled with none.
        let mut exo = active(V10);
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, k('k'), DOWN);
        exo.take_engine();
        let actions = vec![shows("가")];
        assert!(!exo.answer(&EngineOut::Text { id: 1, actions }));
        send(&mut exo, TI, reset());
        assert!(!exo.answer(&EngineOut::Done {
            id: 1,
            handled: true
        }));
        assert_eq!(events(&mut exo), [peek(1, 'k', DOWN)]);
        // With nothing composed, the engine is left alone.
        let mut exo = active(V10);
        send(&mut exo, TI, reset());
        assert_eq!(exo.take_engine(), []);
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
        assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        // The end of the input method confirms too.
        let mut exo = active(V11);
        assert!(exo.destroy_input_method());
        assert_eq!(events(&mut exo), [confirm(), leave()]);
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
        assert!(!exo.answer(&EngineOut::Text { id: 0, actions }));
        assert_eq!(events(&mut exo), [], "nothing before the callback");
        assert!(!exo.answer(&EngineOut::Done {
            id: 0,
            handled: true
        }));
        let [cursor, string] = preedit(2, "ㄱ");
        assert_eq!(events(&mut exo), [peek(1, 'r', DOWN), cursor, string]);
        // The release goes as its press did, consumed, with the same
        // serial, which only a key sent frees.
        exo.key(NOW, k('r'), UP);
        assert_eq!(exo.take_engine(), [r(false, 1)]);
        assert!(!exo.answer(&EngineOut::Done {
            id: 1,
            handled: false
        }));
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
    fn e12_a_focus_move_reaches_the_keyboard_and_text_inputs_in_the_drawn_order() {
        for text_inputs_first in [false, true] {
            let mut exo = active(V10);
            exo.move_focus(Some(B), &mut |_| u32::from(text_inputs_first));
            let keyboard_leave = wl_keyboard::Event::Leave {
                serial: 1,
                surface: A,
            };
            let keyboard_enter = wl_keyboard::Event::Enter {
                serial: 3,
                surface: B,
                keys: Vec::new(),
            };
            let mut expected = vec![
                (KEYBOARD, keyboard_leave.into()),
                modifiers(KEYBOARD, 2, 0),
                (KEYBOARD, keyboard_enter.into()),
            ];
            let at = if text_inputs_first { 0 } else { 3 };
            expected.insert(at, leave());
            assert_eq!(events(&mut exo), expected);
        }
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
    fn a_case_draws_the_extension_version() {
        let versions = [0, 1, 2, 3].map(|choice| draw_extension_version(&mut |_| choice));
        assert_eq!(versions, [Some(V11), None, Some(7), Some(V10)]);
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
