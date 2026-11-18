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

//! What Exo makes of the text-input requests the proxy sends.
//!
//! Exo, the ChromeOS compositor, applies a text-input request against state
//! the proxy cannot see: it drops a content type while no input method is
//! attached, and spends a pending surrounding-text support on the next
//! content type. A generic compositor exposes none of that, so the fake
//! compositor keeps this model of each host `zwp_text_input_v1` for the
//! fuzzer to compare with what the guest committed. It is a test oracle,
//! not a reimplementation: it keeps only the state that comparison reads.
//!
//! Each rule cites Chromium's `components/exo/text_input.cc` (`TextInput`)
//! or `components/exo/wayland/zwp_text_input_manager.cc` (the request
//! handlers) at Chromium revision 137f56b0d966 (2026-11-17). Exo has one
//! seat, and so does the model. Not modelled:
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
//!   the virtual keyboard, focus reason, grammar and autocorrect state.
//!
//! Where the model is stricter than Exo: it keeps the surrounding text's
//! cursor and anchor as sent, where Exo keeps the range between them, and
//! the cursor rectangle as sent, where `gfx::Rect` clamps a negative width
//! or height to 0.

use crate::protocols::text_input_extension_unstable_v1::{
    zcr_extended_text_input_v1 as extended, zcr_text_input_extension_v1,
};
use crate::protocols::text_input_unstable_v1::{zwp_text_input_manager_v1, zwp_text_input_v1};
use crate::wire::WireMessage;
use std::collections::BTreeMap;

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
    /// The events not yet sent: `enter` with its surface when an input
    /// method attaches, `leave` (`None`) when it detaches.
    events: Vec<Option<u32>>,
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
            events: Vec::new(),
        }
    }

    /// `TextInput::Activate`: nothing for the surface it already has;
    /// otherwise detach, take the surface, and attach if the seat's focus
    /// is on it.
    fn activate(&mut self, surface: u32, focus: Option<u32>) {
        if self.surface == Some(surface) {
            return;
        }
        self.detach();
        self.surface = Some(surface);
        if focus == Some(surface) {
            self.attach();
        }
    }

    /// `TextInput::Deactivate`.
    fn deactivate(&mut self) {
        self.detach();
        self.surface = None;
    }

    /// `AttachInputMethod` resets the type and mode to text, and keeps the
    /// flags, learning and surrounding-text support of the previous field.
    /// Its delegate's `Activated` sends `enter` with the surface of the
    /// last `activate` (`zwp_text_input_manager.cc:127`).
    fn attach(&mut self) {
        self.events.push(self.surface);
        self.field.attached = true;
        self.field.content_type.input_type = extended::INPUT_TYPE_TEXT;
        self.field.content_type.input_mode = extended::INPUT_MODE_TEXT;
    }

    /// `DetachInputMethod`, which does nothing without an input method.
    /// Its delegate's `Deactivated` sends `leave`
    /// (`zwp_text_input_manager.cc:132`).
    fn detach(&mut self) {
        if self.field.attached {
            self.events.push(None);
            self.field.attached = false;
            self.field.content_type.input_type = extended::INPUT_TYPE_NONE;
            self.field.content_type.input_mode = extended::INPUT_MODE_DEFAULT;
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

/// Exo's seat and its text inputs, as far as the model goes.
#[derive(Default)]
pub(crate) struct Exo {
    /// The seat's focused surface.
    focus: Option<u32>,
    /// By host `zwp_text_input_v1` ID.
    text_inputs: BTreeMap<u32, TextInput>,
    /// The text input of each host `zcr_extended_text_input_v1`.
    extensions: BTreeMap<u32, u32>,
}

impl Exo {
    pub(crate) fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// Move the seat's focus. `TextInput::OnSurfaceFocused` attaches the
    /// text inputs of the surface that gains it and deactivates those of
    /// the surface that loses it.
    pub(crate) fn set_focus(&mut self, focus: Option<u32>) {
        let lost = std::mem::replace(&mut self.focus, focus);
        if lost == focus {
            return;
        }
        for text_input in self.text_inputs.values_mut() {
            if text_input.surface.is_some() && text_input.surface == focus {
                text_input.attach();
            } else if text_input.surface.is_some() && text_input.surface == lost {
                text_input.deactivate();
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
        for text_input in self.text_inputs.values_mut() {
            if text_input.surface.is_some_and(|surface| !live(surface)) {
                text_input.deactivate();
            }
        }
    }

    /// Apply a request from the proxy that the compositor has accepted.
    pub(crate) fn request(&mut self, interface: &str, sender: u32, opcode: u16, payload: &[u8]) {
        let mut message = WireMessage::new(sender, opcode, payload);
        let mut word = || message.read_u32().expect("an accepted request");
        match (interface, opcode) {
            ("zwp_text_input_manager_v1", zwp_text_input_manager_v1::REQ_CREATE_TEXT_INPUT) => {
                self.text_inputs.insert(word(), TextInput::new());
            }
            (
                "zcr_text_input_extension_v1",
                zcr_text_input_extension_v1::REQ_GET_EXTENDED_TEXT_INPUT,
            ) => {
                let extension = word();
                self.extensions.insert(extension, word());
            }
            ("zcr_extended_text_input_v1", extended::REQ_DESTROY) => {
                self.extensions.remove(&sender);
            }
            ("zcr_extended_text_input_v1", extended::REQ_SET_SURROUNDING_TEXT_SUPPORT) => {
                let support = match word() {
                    extended::SURROUNDING_TEXT_SUPPORT_SUPPORTED => true,
                    extended::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED => false,
                    // Exo only logs an unknown value.
                    _ => return,
                };
                if let Some(text_input) = self.extended(sender) {
                    text_input.pending_support = support;
                }
            }
            (
                "zcr_extended_text_input_v1",
                extended::REQ_SET_INPUT_TYPE | extended::REQ_DEPRECATED_SET_INPUT_TYPE,
            ) => {
                let content_type = ContentType {
                    input_type: word(),
                    input_mode: word(),
                    flags: word(),
                    learning: word() == extended::LEARNING_MODE_ENABLED,
                    // The deprecated request is the new one with inline
                    // composition supported.
                    inline_composition: opcode == extended::REQ_DEPRECATED_SET_INPUT_TYPE
                        || word() == extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
                };
                if let Some(text_input) = self.extended(sender) {
                    text_input.set_type_mode_flags(content_type);
                }
            }
            ("zwp_text_input_v1", zwp_text_input_v1::REQ_SET_CONTENT_TYPE) => {
                let content_type = v1_content_type(word(), word());
                self.text_input(sender).set_type_mode_flags(content_type);
            }
            ("zwp_text_input_v1", zwp_text_input_v1::REQ_ACTIVATE) => {
                let _seat = word();
                let surface = word();
                let focus = self.focus;
                self.text_input(sender).activate(surface, focus);
            }
            ("zwp_text_input_v1", zwp_text_input_v1::REQ_DEACTIVATE) => {
                self.text_input(sender).deactivate();
            }
            // Exo keeps surrounding text and the cursor rectangle with or
            // without an input method.
            ("zwp_text_input_v1", zwp_text_input_v1::REQ_SET_SURROUNDING_TEXT) => {
                let text = message.read_string().expect("an accepted request");
                let mut word = || message.read_u32().expect("an accepted request");
                let surrounding_text = (text, word(), word());
                self.text_input(sender).field.surrounding_text = surrounding_text;
            }
            ("zwp_text_input_v1", zwp_text_input_v1::REQ_SET_CURSOR_RECTANGLE) => {
                let mut int = || message.read_i32().expect("an accepted request");
                let rectangle = (int(), int(), int(), int());
                self.text_input(sender).field.cursor_rectangle = rectangle;
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
        for text_input in self.text_inputs.values_mut() {
            if text_input.field.attached {
                text_input.deactivate();
                detached = true;
            }
        }
        detached
    }

    /// The `enter` (with its surface) and `leave` (`None`) events Exo sent
    /// since the last call, with the host text input of each.
    pub(crate) fn take_events(&mut self) -> Vec<(u32, Option<u32>)> {
        self.text_inputs
            .iter_mut()
            .flat_map(|(&id, text_input)| {
                std::mem::take(&mut text_input.events)
                    .into_iter()
                    .map(move |event| (id, event))
            })
            .collect()
    }

    /// The fields of the text inputs active on the focused surface.
    pub(crate) fn focused_fields(&self) -> Vec<Field> {
        self.text_inputs
            .values()
            .filter(|text_input| self.focus.is_some() && text_input.surface == self.focus)
            .map(|text_input| text_input.field.clone())
            .collect()
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
