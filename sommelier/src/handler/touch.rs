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

//! `wl_touch` events, forwarded as they come, except that a touch-down on
//! the surface of an IME composition ends it first
//! ([`crate::handler::text_input::end_composition_on_press`]).

use crate::handler::Handlers;
use crate::protocols::wayland::wl_touch;
use crate::state::{Context, HostId};
use crate::wire::Action;

impl wl_touch::WlTouchHandler for Handlers {
    /// A touch-down on the surface of a composition ends it before the
    /// guest gets the down, as a pointer press does. A down names its
    /// surface, so no focus is tracked.
    fn on_down(
        ctx: &mut Context,
        serial: u32,
        time: u32,
        surface: u32,
        id: i32,
        x: i32,
        y: i32,
    ) -> Action {
        // The generated dispatcher calls this only for a host surface
        // paired with a live guest surface (`ShadowTable::event_object`).
        let touch = HostId::from_event_sender(ctx);
        let (Some(guest_surface), Some(guest_touch)) = (
            ctx.shadow_table.get_guest_id(surface),
            ctx.shadow_table.get_guest_id(touch.0),
        ) else {
            return Action::Forward;
        };
        crate::handler::text_input::end_composition_on_press(
            ctx,
            guest_surface,
            wl_touch::evt::down(guest_touch, serial, time, guest_surface, id, x, y),
        )
    }
}
