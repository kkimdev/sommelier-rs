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

//! A fuzzed guest that types into text fields.
//!
//! Random requests rarely enable a focused text input, let alone switch
//! fields, so the text-input focus and host activation paths, and the drain
//! barrier that can defer an activation, are reached this way instead.

use super::{exo, request, Guest, Message};
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
use crate::protocols::text_input_unstable_v3::{zwp_text_input_manager_v3, zwp_text_input_v3};
use crate::protocols::wayland::{wl_compositor, wl_seat, wl_surface};
use crate::wire::{MessageBuilder, WireMessage};

impl Guest<'_> {
    /// What a client typing into text fields sends: a text input on a seat
    /// with a keyboard, then a few rounds that each commit another of its
    /// surfaces, which the compositor then focuses, wait for the focus with
    /// a roundtrip, as a client waits for `enter`, and edit the field in a
    /// few transactions. Returns nothing if a global is missing.
    ///
    /// Records in the guest's view what its typing left ([`Typed`]).
    pub(super) fn type_in_fields(&mut self) -> Vec<Message> {
        let find = |interface: &str| {
            self.globals
                .iter()
                .find(|global| global.interface == interface)
        };
        let (Some(compositor), Some(seat), Some(manager)) = (
            find("wl_compositor"),
            find("wl_seat"),
            find("zwp_text_input_manager_v3"),
        ) else {
            return Vec::new();
        };
        let (bind_compositor, compositor, compositor_version) = self.bind_global(compositor);
        let (bind_seat, seat, seat_version) = self.bind_global(seat);
        let (bind_manager, manager, manager_version) = self.bind_global(manager);
        let keyboard = self.create("wl_keyboard", seat_version);
        let text_input = self.create("zwp_text_input_v3", manager_version);
        let surfaces: Vec<u32> = (0..2 + self.rng.below(2))
            .map(|_| self.create("wl_surface", compositor_version))
            .collect();
        let mut messages = vec![
            bind_compositor,
            bind_seat,
            bind_manager,
            request(seat, wl_seat::REQ_GET_KEYBOARD, &[keyboard]),
            request(
                manager,
                zwp_text_input_manager_v3::REQ_GET_TEXT_INPUT,
                &[text_input, seat],
            ),
        ];
        messages.extend(
            surfaces
                .iter()
                .map(|&surface| request(compositor, wl_compositor::REQ_CREATE_SURFACE, &[surface])),
        );
        let mut focus = None;
        let mut plan = Plan::default();
        for _ in 0..1 + self.rng.below(4) {
            let others: Vec<u32> = surfaces
                .iter()
                .copied()
                .filter(|&surface| Some(surface) != focus)
                .collect();
            let surface = *self.rng.pick(&others);
            focus = Some(surface);
            messages.push(request(surface, wl_surface::REQ_COMMIT, &[]));
            // The compositor answers the commit with the focus change, and
            // the proxy then deactivates the field, which the new focus
            // disables. The roundtrip reached the host before that barrier.
            plan.release();
            plan.disable();
            messages.push(self.roundtrip());
            messages.extend(self.settle(&mut plan));
            for _ in 0..1 + self.rng.below(3) {
                messages.extend(self.edit_field(text_input, &mut plan));
            }
        }
        // The closing sync follows.
        plan.release();
        self.view.typed = Some(Typed {
            field: plan.field,
            deferred_activations: plan.deferred_activations,
        });
        messages
    }

    /// One transaction on `text_input`, ending with its `commit`, that
    /// enables the `plan`'s field (always if it is disabled), changes it,
    /// or disables it.
    fn edit_field(&mut self, text_input: u32, plan: &mut Plan) -> Vec<Message> {
        use zwp_text_input_v3 as v3;
        let mut messages = Vec::new();
        let (enable, disable) = match self.rng.below(4) {
            _ if plan.field.is_none() => (true, false),
            0 => (true, false),
            1 => (false, true),
            _ => (false, false),
        };
        if disable {
            messages.push(request(text_input, v3::REQ_DISABLE, &[]));
            plan.disable();
        } else {
            if enable {
                plan.enable();
            }
            let field = plan.field.as_mut().expect("an enabled field");
            if enable {
                messages.push(request(text_input, v3::REQ_ENABLE, &[]));
            }
            // A field that gets its surrounding text in a later commit
            // changes whether it supports it.
            if self.rng.one_in(2) {
                let text: String = (0..self.rng.below(16))
                    .map(|_| char::from(b'a' + self.rng.below(26) as u8))
                    .collect();
                let cursor = self.rng.below(text.len() + 1) as u32;
                let anchor = self.rng.below(text.len() + 1) as u32;
                let mut payload = MessageBuilder::new();
                payload.write_string(&text);
                payload.write_u32(cursor);
                payload.write_u32(anchor);
                let mut message = request(text_input, v3::REQ_SET_SURROUNDING_TEXT, &[]);
                message.payload = payload.into_payload();
                messages.push(message);
                field.surrounding_text = Some((text, cursor, anchor));
            }
            // An enabled field without one has the default content type.
            if !self.rng.one_in(4) {
                let message = self.message(text_input, v3::REQ_SET_CONTENT_TYPE);
                let mut args = WireMessage::new(text_input, message.opcode, &message.payload);
                let mut word = || args.read_u32().expect("a content type");
                field.content_type = (word(), word());
                messages.push(message);
            }
            if self.rng.one_in(3) {
                let message = self.message(text_input, v3::REQ_SET_CURSOR_RECTANGLE);
                let mut args = WireMessage::new(text_input, message.opcode, &message.payload);
                let mut int = || args.read_i32().expect("a rectangle");
                field.cursor_rectangle = Some((int(), int(), int(), int()));
                messages.push(message);
            }
        }
        messages.push(request(text_input, v3::REQ_COMMIT, &[]));
        if disable {
            messages.extend(self.settle(plan));
        }
        messages
    }

    /// Now and then, a roundtrip after a focus change or a disable. Its
    /// sync releases the drain barrier's answer, which the compositor holds,
    /// so the proxy ends the field's host activation before the next enable
    /// arrives and activates that at once. Without it, the barrier's answer
    /// stays held past that enable, whose activation the proxy defers.
    fn settle(&mut self, plan: &mut Plan) -> Option<Message> {
        self.rng.one_in(2).then(|| {
            plan.release();
            self.roundtrip()
        })
    }
}

/// What the guest's typing into text fields left.
pub(super) struct Typed {
    /// The field its text input last committed, if it is enabled.
    pub field: Option<V3Field>,
    /// The activations its plan deferred: enables the proxy received while
    /// it drained the field's previous activation (see [`Plan`]).
    pub deferred_activations: u64,
}

/// The guest's field and, as the guest's plan determines it, the proxy's
/// host activation of it. A disable or a new focus makes the proxy
/// deactivate an active field behind a drain barrier, whose answer the
/// compositor holds until it next answers a request: here a focus change,
/// a settling roundtrip or the closing sync. An enable sent before that
/// reaches the proxy while it drains, so the proxy activates the field only
/// once that answer arrives. This counts the path, whatever the proxy sends
/// the host meanwhile.
#[derive(Default)]
struct Plan {
    field: Option<V3Field>,
    host: Host,
    deferred_activations: u64,
}

/// The proxy's host activation of the field.
#[derive(Clone, Copy, Default, PartialEq)]
enum Host {
    #[default]
    Inactive,
    Active,
    Draining,
}

impl Plan {
    /// Enable a new field, which the proxy activates at once unless it is
    /// draining the previous activation.
    fn enable(&mut self) {
        self.field = Some(V3Field::default());
        if self.host == Host::Inactive {
            self.host = Host::Active;
        }
    }

    /// Disable the field; an active one starts draining.
    fn disable(&mut self) {
        self.field = None;
        if self.host == Host::Active {
            self.host = Host::Draining;
        }
    }

    /// The held answer to the drain barrier, if any, arrives: the proxy
    /// activates a field enabled meanwhile, a deferred activation.
    fn release(&mut self) {
        if self.host == Host::Draining {
            self.host = if self.field.is_some() {
                self.deferred_activations += 1;
                Host::Active
            } else {
                Host::Inactive
            };
        }
    }
}

/// A text-input-v3 field as the guest committed it.
#[derive(Clone, Default)]
pub(super) struct V3Field {
    /// `(hint, purpose)`.
    content_type: (u32, u32),
    surrounding_text: Option<(String, u32, u32)>,
    cursor_rectangle: Option<(i32, i32, i32, i32)>,
}

impl V3Field {
    /// What Exo should hold for this field once the proxy has sent it to
    /// a host whose `zcr_extended_text_input_v1` has `extension_version`:
    /// the proxy's own mapping of the content type, through the extension
    /// from v2 on and through v1 `set_content_type` before; inline
    /// composition, which every v3 client supports; surrounding text,
    /// supported when the field has it or when the extension cannot say
    /// otherwise (before v9); and the cursor rectangle, empty when it has
    /// none.
    pub(super) fn on_host(&self, extension_version: u32) -> exo::Field {
        use zcr_extended_text_input_v1 as extended;
        let (hint, purpose) = self.content_type;
        let (v1_hint, v1_purpose, input_type, input_mode, flags, learning_mode) =
            crate::handler::text_input::map_v3_content_type(hint, purpose);
        let content_type = if extension_version >= extended::REQ_DEPRECATED_SET_INPUT_TYPE_SINCE {
            exo::ContentType {
                input_type,
                input_mode,
                flags,
                learning: learning_mode == extended::LEARNING_MODE_ENABLED,
                inline_composition: true,
            }
        } else {
            exo::v1_content_type(v1_hint, v1_purpose)
        };
        exo::Field {
            attached: true,
            content_type,
            surrounding_text_supported: self.surrounding_text.is_some()
                || extension_version < extended::REQ_SET_SURROUNDING_TEXT_SUPPORT_SINCE,
            surrounding_text: self.surrounding_text.clone().unwrap_or_default(),
            cursor_rectangle: self.cursor_rectangle.unwrap_or_default(),
        }
    }
}
