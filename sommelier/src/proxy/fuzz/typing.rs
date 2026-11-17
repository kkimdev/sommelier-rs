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

use super::{request, Guest, Message};
use crate::protocols::text_input_unstable_v3::{zwp_text_input_manager_v3, zwp_text_input_v3};
use crate::protocols::wayland::{wl_compositor, wl_seat, wl_surface};
use crate::wire::MessageBuilder;

impl Guest<'_> {
    /// What a client typing into text fields sends: a text input on a seat
    /// with a keyboard, then a few rounds that each commit another of its
    /// surfaces, which the compositor then focuses, wait for the focus with
    /// a roundtrip, as a client waits for `enter`, and edit the field in a
    /// few transactions. Returns nothing if a global is missing.
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
        for _ in 0..1 + self.rng.below(4) {
            let others: Vec<u32> = surfaces
                .iter()
                .copied()
                .filter(|&surface| Some(surface) != focus)
                .collect();
            let surface = *self.rng.pick(&others);
            focus = Some(surface);
            messages.push(request(surface, wl_surface::REQ_COMMIT, &[]));
            messages.push(self.roundtrip());
            messages.extend(self.settle());
            let mut enabled = false;
            for _ in 0..1 + self.rng.below(3) {
                messages.extend(self.edit_field(text_input, &mut enabled));
            }
        }
        messages
    }

    /// One transaction on `text_input`, ending with its `commit`: enable
    /// the field (always if it is not `enabled`), change it, or disable it.
    fn edit_field(&mut self, text_input: u32, enabled: &mut bool) -> Vec<Message> {
        use zwp_text_input_v3 as v3;
        let mut messages = Vec::new();
        let (enable, disable) = match self.rng.below(4) {
            _ if !*enabled => (true, false),
            0 => (true, false),
            1 => (false, true),
            _ => (false, false),
        };
        if disable {
            messages.push(request(text_input, v3::REQ_DISABLE, &[]));
        } else {
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
            }
            // An enabled field without one has the default content type.
            if !self.rng.one_in(4) {
                messages.push(self.message(text_input, v3::REQ_SET_CONTENT_TYPE));
            }
            if self.rng.one_in(3) {
                messages.push(self.message(text_input, v3::REQ_SET_CURSOR_RECTANGLE));
            }
        }
        *enabled = !disable;
        messages.push(request(text_input, v3::REQ_COMMIT, &[]));
        if disable {
            messages.extend(self.settle());
        }
        messages
    }

    /// Now and then, a roundtrip after a focus change or a disable. Its
    /// sync releases the drain barrier's answer, which the compositor holds,
    /// so the proxy ends the field's host activation before the next enable
    /// arrives and activates that at once. Without it, the barrier's answer
    /// stays held past that enable, whose activation the proxy defers.
    fn settle(&mut self) -> Option<Message> {
        self.rng.one_in(2).then(|| self.roundtrip())
    }
}
