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
use crate::protocols::AnyRequest;
use crate::wire::{IndexedString, Outgoing};
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
    /// The events Exo sent and no one has taken yet, in order.
    events: Vec<Outgoing>,
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
        for id in self.ids() {
            let surface = self.text_inputs[&id].surface;
            if surface.is_some() && surface == focus {
                self.attach(id);
            } else if surface.is_some() && surface == lost {
                self.deactivate(id);
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
            AnyRequest::ZcrExtendedTextInputV1(request) => self.extension_request(sender, request),
            AnyRequest::ZwpTextInputV1(request) => self.text_input_request(sender, request),
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
                self.deactivate(id);
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
            Request::Deactivate { .. } => self.deactivate(sender),
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
                self.text_input(sender).field.surrounding_text = (text, *cursor, *anchor);
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
    /// Its delegate's `Deactivated` sends `leave`
    /// (`zwp_text_input_manager.cc:132`).
    fn detach(&mut self, id: u32) {
        let text_input = self.text_input(id);
        if !text_input.field.attached {
            return;
        }
        text_input.field.attached = false;
        text_input.field.content_type.input_type = extended::INPUT_TYPE_NONE;
        text_input.field.content_type.input_mode = extended::INPUT_MODE_DEFAULT;
        self.events.push(zwp_text_input_v1::evt::leave(id));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::AnyEvent;
    use crate::test_support::decoded;

    const SEAT: u32 = 2;
    const TI_MANAGER: u32 = 3;
    const EXT_MANAGER: u32 = 4;
    const TI: u32 = 10;
    const EXT: u32 = 11;
    const A: u32 = 20;
    const B: u32 = 21;

    /// Exo whose seat focuses surface `A`, with text input `TI` and its
    /// extension `EXT`, not yet activated.
    fn exo() -> Exo {
        let mut exo = Exo::default();
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
            _ => panic!("no test object has ID {id}"),
        }
    }

    fn enter(surface: u32) -> (u32, AnyEvent) {
        (TI, zwp_text_input_v1::Event::Enter { surface }.into())
    }

    fn leave() -> (u32, AnyEvent) {
        (TI, zwp_text_input_v1::Event::Leave {}.into())
    }

    fn field(exo: &Exo) -> &Field {
        &exo.text_inputs[&TI].field
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
}
