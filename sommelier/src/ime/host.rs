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

//! The guest → host direction: the host state a text input's committed
//! state calls for ([`HostTarget`]), the requests that bring the
//! [`LinkState`] to it ([`reconcile`]), and their
//! encoding.

use super::state::{ContentType, LinkState, TextInput};
use super::Env;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as ext;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::wire::Outgoing;

/// A content type as the host applies it: the v3 content type, and the
/// surrounding-text support told with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostContentType {
    pub(super) content_type: ContentType,
    pub(super) surrounding_text_support: bool,
}

/// The host state a text input should have (design 2, "Reconcile").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HostTarget {
    /// The host surface of an enabled field, if the host knows it: the
    /// text input wants input there.
    pub(super) surface: Option<u32>,
    /// The host seat that can carry an `activate` or `deactivate`.
    pub(super) seat: Option<u32>,
    pub(super) content_type: HostContentType,
}

/// How the link's activation changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivationStep {
    Activate {
        seat: u32,
        surface: u32,
    },
    /// `deactivate`, behind a drain barrier.
    Deactivate {
        seat: u32,
    },
    /// R1: no live seat can carry a `deactivate`, so the link ends
    /// locally. Exo keeps its input method attached until its own focus
    /// moves (HANDOVER covers the cost).
    Abandon,
}

/// The host requests that bring a link to its target, in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HostSteps {
    pub(super) activation: Option<ActivationStep>,
    pub(super) content_type: Option<HostContentType>,
}

/// The host state text input `t` should have.
pub(super) fn target(t: &TextInput, env: &impl Env) -> HostTarget {
    let field = t.field();
    let committed = field.map(|(_, committed)| committed);
    HostTarget {
        surface: field
            .filter(|(_, committed)| committed.enabled)
            .and_then(|(surface, _)| env.host_surface(surface)),
        seat: env.host_seat(t.seat()),
        content_type: HostContentType {
            content_type: committed
                .map(|committed| committed.content_type)
                .unwrap_or_default(),
            surrounding_text_support: committed.is_some_and(|c| c.surrounding_text_support),
        },
    }
}

/// The requests that make `link` equal `target`, and no others: a content
/// type the host would drop or already has is not sent (INVARIANTS "The
/// host gets each content type once it can apply it, and only then"). A
/// draining link waits for its barrier. Pure, and idempotent once its
/// steps are applied.
pub(super) fn reconcile(target: &HostTarget, link: LinkState) -> HostSteps {
    let activation = match (target.surface, link) {
        (Some(surface), LinkState::Detached) => target
            .seat
            .map(|seat| ActivationStep::Activate { seat, surface }),
        (None, LinkState::Attached { .. }) => Some(match target.seat {
            Some(seat) => ActivationStep::Deactivate { seat },
            None => ActivationStep::Abandon,
        }),
        _ => None,
    };
    // What the attached input method has after that step, or `None` if no
    // input method is attached to apply a content type.
    let attached = match (activation, link) {
        (Some(ActivationStep::Activate { .. }), _) => Some(None),
        (None, LinkState::Attached { content_type }) => Some(content_type),
        _ => None,
    };
    HostSteps {
        activation,
        content_type: attached
            .filter(|&applied| applied != Some(target.content_type))
            .map(|_| target.content_type),
    }
}

/// The host requests that set `content_type` on host extended text input
/// `ext`, with its surrounding-text support (INVARIANTS "`set_input_type`
/// replaces v1 `set_content_type`"). Exo applies the pending support to
/// the next content type and then resets it, so it goes first.
pub(super) fn content_type_requests(ext: u32, content_type: HostContentType) -> [Outgoing; 2] {
    let (hint, purpose) = content_type.content_type;
    let (input_type, input_mode, input_flags, learning_mode) = map_v3_content_type(hint, purpose);
    let support = match content_type.surrounding_text_support {
        true => ext::SURROUNDING_TEXT_SUPPORT_SUPPORTED,
        false => ext::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED,
    };
    [
        ext::req::set_surrounding_text_support(ext, support),
        // A text-input-v3 client supports inline preedit by definition.
        ext::req::set_input_type(
            ext,
            input_type,
            input_mode,
            input_flags,
            learning_mode,
            ext::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        ),
    ]
}

/// Map a v3 content type to the extension's `(input_type, input_mode,
/// input_flags, learning_mode)`.
pub(crate) fn map_v3_content_type(hint: u32, purpose: u32) -> (u32, u32, u32, u32) {
    use zwp_text_input_v3 as v3;

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
    (input_type, input_mode, input_flags, learning_mode)
}
