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
//! The simulation also gives the model the seat's keyboards, the user's
//! keys and the answers of the input method engine ([`super::engine`]), and
//! takes what the model hands the engine. Exo then sends a key raw while no
//! text input is attached, or with the engine's edits once it answers, and
//! confirms the composition it tracks when a text input detaches. A host
//! behaviour no source settles is drawn per case ([`Host`]). Answers that
//! belong to the display, a sync's `done` and the `delete_id` of a
//! destroyed object, are left to whoever plays it. The model is a test
//! oracle, not a reimplementation: it keeps only the state those read.
//!
//! Each rule cites Chromium's `components/exo/text_input.cc` (`TextInput`)
//! or `components/exo/wayland/zwp_text_input_manager.cc` (the request
//! handlers) at Chromium revision 137f56b0d966 (2026-11-17), and the key
//! path cites `components/exo/keyboard.cc` (`Keyboard::OnKeyEvent`) and
//! `components/exo/wayland/serial_tracker.cc`. Exo has one seat, and so
//! does the model. Not modelled:
//! - Exo's forced `TEXT_INPUT_TYPE_NULL` for a field without surrounding
//!   text under an xkb input method (`ShouldUseNullInputType`), which
//!   depends on the user's input method, not on the requests;
//! - the fallbacks for unknown types, modes and flags, and for surrounding
//!   text whose offsets do not convert to UTF-16, none of which the proxy
//!   sends;
//! - `zwp_text_input_v1.modifiers_map`, which Exo sends on every
//!   `activate` (`zwp_text_input_manager.cc` 569-577) and the proxy
//!   ignores;
//! - which of several attached text inputs the input method serves, and
//!   the virtual keyboard, focus reason, grammar and autocorrect state;
//! - `preedit_styling` and `cursor_position` around the engine's edits, the
//!   keymap, key repeats, which Exo never sends, and the release of a
//!   keyboard or its extension.
//!
//! Where the model is stricter than Exo: it keeps the surrounding text's
//! cursor and anchor as sent, where Exo keeps the range between them, and
//! the cursor rectangle as sent, where `gfx::Rect` clamps a negative width
//! or height to 0.

use super::clock::Micros;
use super::editor::{TextModel, Txn};
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
use crate::protocols::wayland::{wl_keyboard, wl_seat};
use crate::protocols::AnyRequest;
use crate::wire::{IndexedString, Outgoing};
use std::collections::{BTreeMap, VecDeque};

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
    /// What `SurroundingTextTracker` predicts the text is (E4): the last
    /// surrounding text with the input method's edits since; its preedit is
    /// the composition Exo tracks (`composition_`).
    text: TextModel,
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
            text: TextModel::default(),
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

/// What a case draws about the host: the extension's version, and each
/// behaviour that only a real Chromebook can settle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Host {
    /// The `zcr_text_input_extension_v1` version, if the host has one.
    pub extension: Option<u32>,
    /// E6 (U3): a detach confirms the composition.
    pub detach_confirms: bool,
    /// E8 (U2): a key's edits follow its press's events, rather than
    /// precede them.
    pub text_after_key: bool,
    /// E9 (U4): the engine commits the composition Exo tracks with
    /// `confirm_preedit`, where the extension has it.
    pub confirm_commits: bool,
}

impl Host {
    pub fn draw(choices: &mut impl Choose) -> Self {
        Self {
            extension: [None, Some(7), Some(10), Some(11)][choices.pick(4) as usize],
            detach_confirms: choices.pick(2) == 1,
            text_after_key: choices.pick(2) == 1,
            confirm_commits: choices.pick(2) == 1,
        }
    }
}

/// A key handed to the engine that it has not answered.
struct PendingKey {
    id: u64,
    key: Key,
    /// The key's time, in milliseconds.
    time: u32,
    /// Whether Exo reset the engine after handing it the key.
    late: bool,
}

/// Exo's seat and its text inputs, as far as the model goes.
#[derive(Default)]
pub(crate) struct Exo {
    host: Host,
    /// The seat's focused surface.
    focus: Option<u32>,
    /// By host `zwp_text_input_v1` ID.
    text_inputs: BTreeMap<u32, TextInput>,
    /// The text input of each host `zcr_extended_text_input_v1`.
    extensions: BTreeMap<u32, u32>,
    /// The events Exo sent and no one has taken yet, in order.
    events: Vec<Outgoing>,
    /// Each `wl_keyboard`, with its `zcr_extended_keyboard_v1` if it has
    /// one.
    keyboards: BTreeMap<u32, Option<u32>>,
    /// The last serial an event took.
    serial: u32,
    /// The serial `MaybeNextKeySerial` keeps for keys: a `peek_key` takes
    /// it, and the `wl_keyboard.key` that uses it frees it.
    key_serial: Option<u32>,
    /// What Exo handed the engine and no one has taken yet, in order.
    to_engine: Vec<EngineIn>,
    /// The keys the engine has yet to answer, in order.
    in_engine: VecDeque<PendingKey>,
    /// The ID of the next key Exo hands the engine.
    next_key: u64,
    /// How many answers came after a reset that followed their key.
    late_answers: u64,
}

impl Exo {
    pub(super) fn new(host: Host) -> Self {
        Self {
            host,
            ..Self::default()
        }
    }

    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// Move the seat's focus, as Alt-Tab or a click on another window does:
    /// each keyboard leaves the old surface and enters the new one, and the
    /// text inputs follow as in [`Exo::set_focus`]. Whether the keyboards
    /// or the text inputs hear of it first is drawn per move (E12).
    pub(super) fn move_focus(&mut self, focus: Option<u32>, choices: &mut impl Choose) {
        let lost = self.focus;
        if lost == focus {
            return;
        }
        let keyboards_first = choices.pick(2) == 0;
        if !keyboards_first {
            self.set_focus(focus);
        }
        for keyboard in self.keyboards.keys().copied().collect::<Vec<_>>() {
            if let Some(surface) = lost {
                let serial = self.next_serial();
                self.events
                    .push(wl_keyboard::evt::leave(keyboard, serial, surface));
            }
            if let Some(surface) = focus {
                self.keyboard_enter(keyboard, surface);
            }
        }
        if keyboards_first {
            self.set_focus(focus);
        }
    }

    /// The user presses and releases `key` at `now`. While a text input is
    /// attached, Exo hands the key to the engine and sends its events with
    /// the engine's answer ([`Exo::answer`]). Otherwise the input method
    /// has no client and passes the key, which reaches the keyboards at
    /// once (E7).
    pub(super) fn key(&mut self, now: Micros, key: Key) {
        let time = millis(now);
        if self.attached().is_none() {
            self.key_events(key, time, wl_keyboard::KEY_STATE_PRESSED, false);
            self.key_events(key, time, wl_keyboard::KEY_STATE_RELEASED, false);
            return;
        }
        let id = self.next_key;
        self.next_key += 1;
        self.in_engine.push_back(PendingKey {
            id,
            key,
            time,
            late: false,
        });
        self.to_engine.push(EngineIn::Key { key, id });
    }

    /// The engine's answer to a key. Exo applies the edits to the text
    /// input attached now; with none, `InputMethodAsh` has no client and
    /// they are lost. The key's events go out regardless: its `peek_key`,
    /// and its `wl_keyboard.key` unless the engine consumed it (E7), with
    /// the edits before them or after the press as the case drew (E8).
    pub(super) fn answer(&mut self, out: &EngineOut) {
        let key = self.in_engine.pop_front().expect("a key the engine has");
        assert_eq!(key.id, out.id, "the engine answers in order");
        self.late_answers += u64::from(key.late);
        if !self.host.text_after_key {
            self.edit(&out.actions);
        }
        let consumed = out.consumed;
        self.key_events(key.key, key.time, wl_keyboard::KEY_STATE_PRESSED, consumed);
        if self.host.text_after_key {
            self.edit(&out.actions);
        }
        self.key_events(key.key, key.time, wl_keyboard::KEY_STATE_RELEASED, consumed);
    }

    /// What Exo handed the engine since the last call, in order.
    pub(super) fn take_engine(&mut self) -> Vec<EngineIn> {
        std::mem::take(&mut self.to_engine)
    }

    /// How many of the engine's answers came after a reset that followed
    /// their key: the window a press's reset cannot close (E5).
    pub(super) fn late_answers(&self) -> u64 {
        self.late_answers
    }

    /// Move the seat's focus for the text inputs, for a compositor that
    /// tells the keyboards itself. `TextInput::OnSurfaceFocused` attaches
    /// the text inputs of the surface that gains it and deactivates those
    /// of the surface that loses it.
    pub(crate) fn set_focus(&mut self, focus: Option<u32>) {
        let lost = std::mem::replace(&mut self.focus, focus);
        if lost == focus {
            return;
        }
        for id in self.ids() {
            let surface = self.text_inputs[&id].surface;
            if surface.is_some() && surface == focus {
                self.attach(id);
            } else if surface.is_some() && surface == lost {
                self.deactivate(id, true);
            }
        }
    }

    /// Forget the surfaces and text inputs that are no longer `live`,
    /// destroyed by the proxy or by the compositor. A destroyed surface
    /// loses the focus, and `TextInput::OnSurfaceDestroying` deactivates
    /// its text inputs. An extension whose text input is gone has no
    /// delegate, and Exo ignores its requests.
    pub(crate) fn forget(&mut self, live: impl Fn(u32) -> bool) {
        if self.focus.is_some_and(|surface| !live(surface)) {
            self.set_focus(None);
        }
        self.text_inputs.retain(|&id, _| live(id));
        for id in self.ids() {
            if self.text_inputs[&id]
                .surface
                .is_some_and(|surface| !live(surface))
            {
                self.deactivate(id, true);
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
            AnyRequest::ZcrExtendedTextInputV1(request) => self.extension_request(sender, request),
            AnyRequest::ZwpTextInputV1(request) => self.text_input_request(sender, request),
            AnyRequest::WlSeat(wl_seat::Request::GetKeyboard { id }) => {
                self.keyboards.insert(*id, None);
                // A new keyboard learns the seat's focus at once.
                if let Some(surface) = self.focus {
                    self.keyboard_enter(*id, surface);
                }
            }
            AnyRequest::ZcrKeyboardExtensionV1(
                zcr_keyboard_extension_v1::Request::GetExtendedKeyboard { id, keyboard },
            ) => {
                self.keyboards.insert(*keyboard, Some(*id));
            }
            _ => {}
        }
    }

    /// The input method goes away, as when its process restarts:
    /// `TextInput::OnInputMethodDestroyed` deactivates each text input it
    /// was attached to, and nothing attaches it again until the next
    /// `activate`. Returns whether one was attached.
    pub(crate) fn destroy_input_method(&mut self) -> bool {
        let mut detached = false;
        for id in self.ids() {
            if self.text_inputs[&id].field.attached {
                self.deactivate(id, false);
                detached = true;
            }
        }
        detached
    }

    /// The events Exo sent since the last call, in order.
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
            Request::Deactivate { .. } => self.deactivate(sender, true),
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
                text_input.text.text.clone_from(&text);
                text_input.text.cursor = *cursor as usize;
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
        self.detach(id, true);
        self.text_input(id).surface = Some(surface);
        if self.focus == Some(surface) {
            self.attach(id);
        }
    }

    /// `TextInput::Deactivate`; the input method may confirm unless it is
    /// gone.
    fn deactivate(&mut self, id: u32, confirm: bool) {
        self.detach(id, confirm);
        self.text_input(id).surface = None;
    }

    /// `AttachInputMethod` resets the type and mode to text, and keeps the
    /// flags, learning and surrounding-text support of the previous field.
    /// Its delegate's `Activated` sends `enter` with the surface of the
    /// last `activate` (`zwp_text_input_manager.cc:127`).
    fn attach(&mut self, id: u32) {
        let text_input = self.text_input(id);
        let surface = text_input.surface.expect("an activated text input");
        text_input.field.attached = true;
        text_input.field.content_type.input_type = extended::INPUT_TYPE_TEXT;
        text_input.field.content_type.input_mode = extended::INPUT_MODE_TEXT;
        self.events.push(zwp_text_input_v1::evt::enter(id, surface));
    }

    /// `DetachInputMethod`, which does nothing without an input method.
    /// First the input method confirms the composition Exo tracks, if it
    /// may and the case drew that (E6), as a commit of it that confirms
    /// where it can. Then the delegate's `Deactivated` sends `leave`
    /// (`zwp_text_input_manager.cc:132`), and the engine drops its own
    /// composition.
    fn detach(&mut self, id: u32, confirm: bool) {
        let text_input = self.text_input(id);
        if !text_input.field.attached {
            return;
        }
        let composition = text_input.text.preedit.clone();
        if confirm && self.host.detach_confirms && !composition.is_empty() {
            self.commit(id, &composition, true);
        }
        let text_input = self.text_input(id);
        text_input.text.preedit.clear();
        text_input.field.attached = false;
        text_input.field.content_type.input_type = extended::INPUT_TYPE_NONE;
        text_input.field.content_type.input_mode = extended::INPUT_MODE_DEFAULT;
        self.events.push(zwp_text_input_v1::evt::leave(id));
        self.reset_engine();
    }

    /// `TextInput::Reset`: through the input method it has, cancel the
    /// composition Exo tracks, and send nothing (E5).
    fn reset(&mut self, id: u32) {
        let text_input = self.text_input(id);
        if text_input.field.attached {
            text_input.text.preedit.clear();
            self.reset_engine();
        }
    }

    /// Tell the engine to drop its composition. It does so once it reaches
    /// the notice, after every key handed over before it.
    fn reset_engine(&mut self) {
        self.to_engine.push(EngineIn::Reset);
        for key in &mut self.in_engine {
            key.late = true;
        }
    }

    /// Apply the engine's edits to the text input it serves, sending each
    /// as Exo does: a composition as `preedit_cursor` at its end and a
    /// `preedit_string` that is its own commit text, or an empty
    /// `preedit_string` to end it; a commit as in [`Exo::commit`]; a delete
    /// as `delete_surrounding_text` alone, with an absolute byte index into
    /// the predicted text (E10).
    fn edit(&mut self, actions: &[TextAction]) {
        let Some(id) = self.attached() else {
            return;
        };
        for action in actions {
            match action {
                TextAction::Commit(text) => self.commit(id, text, self.host.confirm_commits),
                TextAction::Preedit(text) => {
                    if !text.is_empty() {
                        let cursor = i32::try_from(text.len()).expect("a short preedit");
                        self.events
                            .push(zwp_text_input_v1::evt::preedit_cursor(id, cursor));
                    }
                    let serial = self.next_serial();
                    let event = zwp_text_input_v1::evt::preedit_string(id, serial, text, text);
                    self.events.push(event.expect("a preedit fits a message"));
                    self.text_input(id).text.preedit.clone_from(text);
                }
                &TextAction::Delete { before, after } => {
                    let text = &mut self.text_input(id).text;
                    assert!(text.preedit.is_empty(), "a delete while composing");
                    let range = text
                        .delete_range(before, after)
                        .expect("a delete of predicted text");
                    text.apply_txn(&Txn {
                        delete: (before, after),
                        ..Txn::default()
                    });
                    let index = i32::try_from(range.start).expect("a short text");
                    let length = u32::try_from(range.len()).expect("a short text");
                    self.events
                        .push(zwp_text_input_v1::evt::delete_surrounding_text(
                            id, index, length,
                        ));
                }
            }
        }
    }

    /// The input method commits `text`, which ends the composition. It
    /// confirms instead if `confirm`, `text` is the composition Exo tracks,
    /// and the extension has `confirm_preedit` (v11): a confirmation
    /// commits only that composition (E9).
    fn commit(&mut self, id: u32, text: &str, confirm: bool) {
        let tracked = !text.is_empty() && self.text_input(id).text.preedit == text;
        let confirms = confirm && tracked && self.host.extension >= Some(11);
        let extension = self.extension_of(id).filter(|_| confirms);
        let commit = Txn {
            commit: text.to_owned(),
            ..Txn::default()
        };
        self.text_input(id).text.apply_txn(&commit);
        match extension {
            Some(extension) => self.events.push(extended::evt::confirm_preedit(
                extension,
                extended::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_AFTER_PREEDIT,
            )),
            None => {
                let serial = self.next_serial();
                let event = zwp_text_input_v1::evt::commit_string(id, serial, text);
                self.events.push(event.expect("a commit fits a message"));
            }
        }
    }

    /// One transition of `key` for each keyboard of the focused surface:
    /// a `peek_key` on its extension, and, unless the input method consumed
    /// the key, a `wl_keyboard.key` with the same serial. A run of consumed
    /// transitions shares one serial, which the next key sent frees. Shift
    /// goes on before a shifted key's press and off after its release.
    fn key_events(&mut self, key: Key, time: u32, state: u32, consumed: bool) {
        if self.focus.is_none() {
            return;
        }
        let (code, shift) = keymap::keycode(key);
        if shift && state == wl_keyboard::KEY_STATE_PRESSED {
            self.modifiers(keymap::SHIFT);
        }
        let serial = match self.key_serial {
            Some(serial) => serial,
            None => self.next_serial(),
        };
        self.key_serial = Some(serial);
        for (&keyboard, &extended) in &self.keyboards {
            if let Some(extended) = extended {
                self.events.push(zcr_extended_keyboard_v1::evt::peek_key(
                    extended, serial, time, code, state,
                ));
            }
            if !consumed {
                self.events
                    .push(wl_keyboard::evt::key(keyboard, serial, time, code, state));
            }
        }
        if !consumed {
            self.key_serial = None;
        }
        if shift && state == wl_keyboard::KEY_STATE_RELEASED {
            self.modifiers(0);
        }
    }

    /// `wl_keyboard.modifiers` with `depressed` to each keyboard.
    fn modifiers(&mut self, depressed: u32) {
        for keyboard in self.keyboards.keys().copied().collect::<Vec<_>>() {
            let serial = self.next_serial();
            self.events.push(wl_keyboard::evt::modifiers(
                keyboard, serial, depressed, 0, 0, 0,
            ));
        }
    }

    /// `keyboard` enters `surface`, after the modifiers, as Exo sends them.
    fn keyboard_enter(&mut self, keyboard: u32, surface: u32) {
        let serial = self.next_serial();
        self.events
            .push(wl_keyboard::evt::modifiers(keyboard, serial, 0, 0, 0, 0));
        let serial = self.next_serial();
        let enter = wl_keyboard::evt::enter(keyboard, serial, surface, &[]);
        self.events.push(enter.expect("an enter with no keys fits"));
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// The text input the input method serves.
    fn attached(&self) -> Option<u32> {
        let mut text_inputs = self.text_inputs.iter();
        text_inputs.find_map(|(&id, text_input)| text_input.field.attached.then_some(id))
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
    /// The time of every key, and its milliseconds.
    const NOW: Micros = Micros(7_000);
    const MS: u32 = 7;
    const DOWN: u32 = wl_keyboard::KEY_STATE_PRESSED;
    const UP: u32 = wl_keyboard::KEY_STATE_RELEASED;

    fn exo() -> Exo {
        exo_for(Host::default())
    }

    /// Exo for `host`, whose seat focuses surface `A`, with keyboard
    /// `KEYBOARD` and its extension `PEEK`, and text input `TI` and its
    /// extension `EXT`, not yet activated.
    fn exo_for(host: Host) -> Exo {
        let mut exo = Exo::new(host);
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
        exo.set_focus(Some(A));
        exo
    }

    /// [`exo_for`] with `TI` active on `A`, and its events taken.
    fn active(host: Host) -> Exo {
        let mut exo = exo_for(host);
        send(&mut exo, TI, activate(A));
        exo.take_events();
        exo
    }

    /// The user presses `key`, and the engine answers with `actions` at
    /// once; returns the events Exo sent.
    fn press(
        exo: &mut Exo,
        key: char,
        actions: &[TextAction],
        consumed: bool,
    ) -> Vec<(u32, AnyEvent)> {
        exo.key(NOW, Key::Char(key));
        let [EngineIn::Key { id, .. }] = exo.take_engine()[..] else {
            panic!("the key went to the engine");
        };
        let actions = actions.to_vec();
        exo.answer(&EngineOut {
            id,
            actions,
            consumed,
        });
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

    /// `preedit_cursor` and `preedit_string` showing `text`.
    fn preedit(serial: u32, text: &str) -> [(u32, AnyEvent); 2] {
        let index = text.len() as i32;
        let commit = text.to_owned();
        [
            (TI, zwp_text_input_v1::Event::PreeditCursor { index }.into()),
            (
                TI,
                zwp_text_input_v1::Event::PreeditString {
                    serial,
                    text: text.into(),
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

    fn confirm() -> (u32, AnyEvent) {
        let selection_behavior = extended::CONFIRM_PREEDIT_SELECTION_BEHAVIOR_AFTER_PREEDIT;
        (
            EXT,
            extended::Event::ConfirmPreedit { selection_behavior }.into(),
        )
    }

    fn peek(serial: u32, key: char, state: u32) -> (u32, AnyEvent) {
        let key = keymap::keycode(Key::Char(key)).0;
        let event = zcr_extended_keyboard_v1::Event::PeekKey {
            serial,
            time: MS,
            key,
            state,
        };
        (PEEK, event.into())
    }

    fn key(serial: u32, key: char, state: u32) -> (u32, AnyEvent) {
        let key = keymap::keycode(Key::Char(key)).0;
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

    fn predicted(exo: &Exo) -> &TextModel {
        &exo.text_inputs[&TI].text
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
        exo.set_focus(Some(B));
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
            |exo| exo.set_focus(Some(B)),
            |exo| exo.forget(|id| id != A),
            |exo| assert!(exo.destroy_input_method()),
        ];
        for detach in detaches {
            exo.set_focus(Some(A));
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
        exo.set_focus(Some(B));
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
    fn e4_exo_predicts_the_text_from_the_last_report_and_the_edits_since() {
        let mut exo = active(Host::default());
        send(&mut exo, TI, surrounding("ab", 1));
        press(&mut exo, 'r', &[commits("가")], true);
        let edited = TextModel {
            text: "a가b".to_owned(),
            cursor: 4,
            preedit: String::new(),
        };
        assert_eq!(*predicted(&exo), edited);
        // Without an input method too, the next report replaces it.
        send(&mut exo, TI, deactivate());
        send(&mut exo, TI, surrounding("xyz", 3));
        let rectangle = zwp_text_input_v1::Request::SetCursorRectangle {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        send(&mut exo, TI, rectangle);
        assert_eq!(*predicted(&exo), TextModel::new("xyz"));
        assert_eq!(field(&exo).surrounding_text, ("xyz".to_owned(), 3, 3));
        assert_eq!(field(&exo).cursor_rectangle, (1, 2, 3, 4));
    }

    #[test]
    fn e5_a_reset_cancels_the_composition_silently_but_not_a_key_in_the_engine() {
        let mut exo = active(Host::default());
        press(&mut exo, 'r', &[shows("ㄱ")], true);
        exo.key(NOW, Key::Char('k'));
        send(&mut exo, TI, zwp_text_input_v1::Request::Reset {});
        assert_eq!(events(&mut exo), []);
        assert_eq!(predicted(&exo).preedit, "");
        let k = EngineIn::Key {
            key: Key::Char('k'),
            id: 1,
        };
        assert_eq!(exo.take_engine(), [k, EngineIn::Reset]);
        // The answer the engine made before it reached the reset still
        // goes out.
        let actions = vec![shows("가")];
        exo.answer(&EngineOut {
            id: 1,
            actions,
            consumed: true,
        });
        let [cursor, string] = preedit(3, "가");
        let late = [cursor, string, peek(2, 'k', DOWN), peek(2, 'k', UP)];
        assert_eq!(events(&mut exo), late);
        assert_eq!(exo.late_answers(), 1);
    }

    #[test]
    fn e6_a_detach_confirms_the_tracked_composition_if_the_case_drew_it() {
        let confirms = |extension| Host {
            extension,
            detach_confirms: true,
            ..Host::default()
        };
        let silent = Host {
            extension: Some(11),
            ..Host::default()
        };
        let cases = [
            (silent, None),
            (confirms(Some(11)), Some(confirm())),
            (confirms(Some(10)), Some(commit(3, "가"))),
            (confirms(None), Some(commit(3, "가"))),
        ];
        for (host, confirmation) in cases {
            let mut exo = active(host);
            press(&mut exo, 'r', &[shows("가")], true);
            send(&mut exo, TI, deactivate());
            let expected: Vec<_> = confirmation.into_iter().chain([leave()]).collect();
            assert_eq!(events(&mut exo), expected, "{host:?}");
            assert_eq!(predicted(&exo).preedit, "");
            assert_eq!(exo.take_engine(), [EngineIn::Reset]);
        }
        // An input method that is gone confirms nothing.
        let mut exo = active(confirms(Some(11)));
        press(&mut exo, 'r', &[shows("가")], true);
        assert!(exo.destroy_input_method());
        assert_eq!(events(&mut exo), [leave()]);
    }

    #[test]
    fn e7_a_key_reaches_the_keyboard_raw_while_no_text_input_is_attached() {
        let mut exo = exo();
        exo.key(NOW, Key::Char('r'));
        assert_eq!(exo.take_engine(), []);
        let raw = [
            peek(1, 'r', DOWN),
            key(1, 'r', DOWN),
            peek(2, 'r', UP),
            key(2, 'r', UP),
        ];
        assert_eq!(events(&mut exo), raw);
        // A shifted key comes with Shift held.
        exo.key(NOW, Key::Char('R'));
        let shifted = [
            modifiers(KEYBOARD, 3, keymap::SHIFT),
            peek(4, 'R', DOWN),
            key(4, 'R', DOWN),
            peek(5, 'R', UP),
            key(5, 'R', UP),
            modifiers(KEYBOARD, 6, 0),
        ];
        assert_eq!(events(&mut exo), shifted);
    }

    #[test]
    fn e7_a_key_the_engine_answers_gets_its_key_event_unless_consumed() {
        let mut exo = active(Host::default());
        exo.key(NOW, Key::Char('r'));
        assert_eq!(events(&mut exo), [], "nothing before the engine answers");
        let r = EngineIn::Key {
            key: Key::Char('r'),
            id: 0,
        };
        assert_eq!(exo.take_engine(), [r]);
        let actions = vec![shows("ㄱ")];
        exo.answer(&EngineOut {
            id: 0,
            actions,
            consumed: true,
        });
        let [cursor, string] = preedit(1, "ㄱ");
        let consumed = [cursor, string, peek(2, 'r', DOWN), peek(2, 'r', UP)];
        assert_eq!(events(&mut exo), consumed);
        // The next key sent takes the serial the consumed ones kept.
        let passed = [
            commit(3, "ㄱ"),
            peek(2, ' ', DOWN),
            key(2, ' ', DOWN),
            peek(4, ' ', UP),
            key(4, ' ', UP),
        ];
        assert_eq!(press(&mut exo, ' ', &[commits("ㄱ")], false), passed);
    }

    #[test]
    fn e8_a_keys_edits_precede_its_events_or_follow_its_press_as_drawn() {
        let mut exo = active(Host::default());
        let before = [
            commit(1, "가"),
            peek(2, ' ', DOWN),
            key(2, ' ', DOWN),
            peek(3, ' ', UP),
            key(3, ' ', UP),
        ];
        assert_eq!(press(&mut exo, ' ', &[commits("가")], false), before);
        let mut exo = active(Host {
            text_after_key: true,
            ..Host::default()
        });
        let after = [
            peek(1, ' ', DOWN),
            key(1, ' ', DOWN),
            commit(2, "가"),
            peek(3, ' ', UP),
            key(3, ' ', UP),
        ];
        assert_eq!(press(&mut exo, ' ', &[commits("가")], false), after);
    }

    #[test]
    fn e9_only_the_tracked_composition_is_confirmed() {
        let host = Host {
            extension: Some(11),
            confirm_commits: true,
            ..Host::default()
        };
        let mut exo = active(host);
        press(&mut exo, 'r', &[shows("각")], true);
        // 각 and ㅏ commit 가, which Exo never tracked.
        let split = press(&mut exo, 'k', &[commits("가"), shows("가")], true);
        assert_eq!(split[0], commit(3, "가"));
        assert_eq!(press(&mut exo, ' ', &[commits("가")], false)[0], confirm());
        // Before v11, or as drawn, the engine commits what it confirms.
        let commits_only = [
            Host {
                extension: Some(10),
                ..host
            },
            Host {
                confirm_commits: false,
                ..host
            },
        ];
        for host in commits_only {
            let mut exo = active(host);
            press(&mut exo, 'r', &[shows("가")], true);
            let events = press(&mut exo, ' ', &[commits("가")], false);
            assert_eq!(events[0], commit(3, "가"), "{host:?}");
        }
    }

    #[test]
    fn e10_a_delete_counts_bytes_from_the_start_of_the_predicted_text() {
        let mut exo = active(Host::default());
        send(&mut exo, TI, surrounding("ab", 2));
        press(&mut exo, 'r', &[commits("가")], true);
        let delete = TextAction::Delete {
            before: 3,
            after: 0,
        };
        // 가 starts 2 bytes into "ab가", and no commit comes with it.
        let deleted = zwp_text_input_v1::Event::DeleteSurroundingText {
            index: 2,
            length: 3,
        };
        let events = [(TI, deleted.into()), peek(2, 'r', DOWN), peek(2, 'r', UP)];
        assert_eq!(press(&mut exo, 'r', &[delete], true), events);
        assert_eq!(*predicted(&exo), TextModel::new("ab"));
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
    fn e12_a_focus_move_reaches_keyboards_and_text_inputs_in_the_drawn_order() {
        for text_inputs_first in [false, true] {
            let mut exo = active(Host::default());
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
    fn a_case_draws_the_extension_version_and_each_unknown() {
        assert_eq!(Host::draw(&mut |_| 0), Host::default());
        let last = Host {
            extension: Some(11),
            detach_confirms: true,
            text_after_key: true,
            confirm_commits: true,
        };
        assert_eq!(Host::draw(&mut |n| n - 1), last);
    }
}
