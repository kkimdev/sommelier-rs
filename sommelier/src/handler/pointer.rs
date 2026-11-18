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

//! `wl_pointer` events, forwarded as they come. The proxy follows each
//! pointer's surface so that a press on the surface of an IME composition
//! can end it first ([`crate::handler::text_input::end_composition_on_press`]).

use crate::protocols::wayland::wl_pointer;
use crate::state::{Context, HostId};
use crate::wire::Action;

pub struct PointerHandler;

impl wl_pointer::WlPointerHandler for PointerHandler {
    fn on_enter(
        &mut self,
        ctx: &mut Context,
        _serial: u32,
        surface: u32,
        _surface_x: i32,
        _surface_y: i32,
    ) -> Action {
        // The generated dispatcher calls this only for a host surface
        // paired with a live guest surface (`ShadowTable::event_object`):
        // it drops an `enter` or `leave` that names a surface the guest
        // destroyed, whose end already took the pointer off it.
        if let Some(guest_surface) = ctx.shadow_table.get_guest_id(surface) {
            ctx.pointer_focus
                .enter(HostId::from_event_sender(ctx), guest_surface);
        }
        Action::Forward
    }

    fn on_leave(&mut self, ctx: &mut Context, _serial: u32, surface: u32) -> Action {
        // The dispatcher resolved the surface, as for `enter`.
        if let Some(guest_surface) = ctx.shadow_table.get_guest_id(surface) {
            ctx.pointer_focus
                .leave(HostId::from_event_sender(ctx), guest_surface);
        }
        Action::Forward
    }

    /// A press on the surface of a composition ends it before the guest
    /// gets the press.
    fn on_button(
        &mut self,
        ctx: &mut Context,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) -> Action {
        if state != wl_pointer::BUTTON_STATE_PRESSED {
            return Action::Forward;
        }
        let pointer = HostId::from_event_sender(ctx);
        let (Some(surface), Some(guest_pointer)) = (
            ctx.pointer_focus.surface(pointer),
            ctx.shadow_table.get_guest_id(pointer.0),
        ) else {
            return Action::Forward;
        };
        crate::handler::text_input::end_composition_on_press(ctx, surface, || {
            wl_pointer::evt::button(guest_pointer, serial, time, button, state)
        })
    }
}

/// Forget host pointer `pointer`, which ended (see
/// `crate::state::lifecycle`).
pub(crate) fn forget_pointer(ctx: &mut Context, pointer: HostId) {
    ctx.pointer_focus.forget_pointer(pointer);
}

#[cfg(test)]
mod tests {
    use crate::protocols::wayland::{wl_pointer, wl_seat, wl_surface};
    use crate::proxy::harness::Session;
    use crate::state::HostId;

    /// A pointer's surface is forgotten when the surface ends, and the
    /// pointer when it ends; each batch of the session checks that no
    /// state outlives its object.
    #[tokio::test]
    async fn pointer_focus_ends_with_its_surface_or_pointer() {
        let mut s = Session::new();
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let surface = s.surface().await;
        let pointer = s.pointer_on(seat.id, surface).await;
        // The surface the pointer is on is no state a peer sees.
        assert_eq!(
            s.ctx().pointer_focus.surface(HostId(pointer.host)),
            Some(surface.id)
        );
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        let destroy = wl_surface::Request::Destroy {};
        assert_eq!(s.host_received(), [(surface.host, destroy.into())]);

        let surface = s.surface().await;
        let pointer = s.pointer_on(seat.id, surface).await;
        s.guest([wl_pointer::req::release(pointer.id)]).await;
        let release = wl_pointer::Request::Release {};
        assert_eq!(s.host_received(), [(pointer.host, release.into())]);
    }
}
