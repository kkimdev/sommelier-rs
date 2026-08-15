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

use crate::protocols::wayland::wl_seat;
use crate::state::Context;
use crate::wire::Action;

pub struct SeatHandler;

impl wl_seat::WlSeatHandler for SeatHandler {
    fn on_get_keyboard(&mut self, ctx: &mut Context, id: u32) -> Action {
        let guest_seat_id = ctx.last_sender_id;
        ctx.keyboard_to_seat.insert(id, guest_seat_id);
        ctx.shadow_table
            .track_interface(id, "wl_keyboard".to_string());
        // zcr_keyboard_extension_v1.get_extended_keyboard is sent from
        // KeyboardHandler::on_enter (the first host→client event for this
        // keyboard), because we need the *host* keyboard ID which is only
        // known at that point. See keyboard.rs::bind_extended_keyboard.
        Action::Forward
    }

    fn on_release(&mut self, _ctx: &mut Context) -> Action {
        // `wl_seat.release` destroys only this parent object. Existing
        // `wl_keyboard`, `wl_pointer`, and `wl_touch` children have their own
        // protocol lifetimes and remain usable until their respective
        // destructors. ChromiumOS' `sl_destroy_host_seat` follows this rule;
        // clearing child routing or IME state here would drop later events
        // (including a key release paired with a press sent before this
        // request).
        Action::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_keyboard::WlKeyboardHandler;
    use crate::protocols::wayland::wl_seat::WlSeatHandler;
    use crate::state::{Context, GuestKeyOwner, HostId, TextInputState};

    #[test]
    fn release_keeps_child_keyboard_routing_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_seat = 10;
        let guest_keyboard = 20;
        let host_keyboard = 30;
        ctx.shadow_table.map_id(guest_seat, 11);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        ctx.active_surface_for_seat.insert(guest_seat, 40);
        ctx.keyboard_active_surfaces
            .insert(HostId(host_keyboard), 40);
        ctx.keyboard_pressed_keys
            .insert(HostId(host_keyboard), [14].into_iter().collect());
        ctx.last_sender_id = guest_seat;

        assert_eq!(SeatHandler.on_release(&mut ctx), Action::Forward);
        assert_eq!(ctx.active_surface_for_seat.get(&guest_seat), Some(&40));
        assert_eq!(ctx.keyboard_to_seat.get(&guest_keyboard), Some(&guest_seat));
        assert!(ctx
            .keyboard_active_surfaces
            .contains_key(&HostId(host_keyboard)));
        assert!(ctx
            .keyboard_pressed_keys
            .contains_key(&HostId(host_keyboard)));
    }

    #[test]
    fn release_does_not_end_child_text_input_focus() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_seat = 10;
        let host_seat = 11;
        let guest_surface = 40;
        let host_surface = 41;
        let guest_keyboard = 20;
        let host_keyboard = 30;
        let guest_text_input = 50;
        let host_text_input = 51;

        ctx.shadow_table.map_id(guest_seat, host_seat);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_surface, host_surface);
        ctx.shadow_table
            .track_interface(guest_surface, "wl_surface".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.shadow_table.map_id(guest_text_input, host_text_input);
        ctx.shadow_table.track_interface_with_version(
            guest_text_input,
            "zwp_text_input_v3".to_string(),
            1,
        );
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        ctx.keyboard_active_surfaces
            .insert(HostId(host_keyboard), guest_surface);
        ctx.active_surface_for_seat
            .insert(guest_seat, guest_surface);
        ctx.text_inputs.insert(
            guest_text_input,
            TextInputState {
                host_v1_id: 60,
                host_ext_id: None,
                guest_seat,
                active_surface: Some(guest_surface),
                pending_enabled: false,
                committed_enabled: false,
                enabled_dirty: false,
                pending_surrounding_text: None,
                committed_surrounding_text: None,
                surrounding_text_dirty: false,
                content_hint: 0,
                content_purpose: 0,
                committed_content_type: None,
                content_type_dirty: false,
                cursor_rect: None,
                cursor_rect_dirty: false,
                text_change_cause: 0,
                current_preedit: "한".to_string(),
                guest_commit_serial: 0,
                pending_preedit_cursor: None,
                pending_preedit_selection: None,
                pending_deletes: Vec::new(),
                pending_cursor_position: None,
                empty_preedit_repeat_active: false,
                host_activated: true,
            },
        );

        ctx.last_sender_id = guest_seat;
        assert_eq!(SeatHandler.on_release(&mut ctx), Action::Forward);
        assert!(ctx.host_to_client_queue.is_empty());
        assert_eq!(
            ctx.text_inputs[&guest_text_input].active_surface,
            Some(guest_surface)
        );
        assert!(ctx.keyboard_to_seat.contains_key(&guest_keyboard));
    }

    #[test]
    fn seat_release_does_not_break_live_child_keyboard_press_release_pair() {
        // wl_seat.release destroys only the seat proxy. ChromiumOS' C
        // implementation does not destroy already-created wl_keyboard
        // children from sl_host_seat_removed(), so a child can still receive
        // the release matching a press that arrived before the parent seat
        // was released.
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_seat = 10;
        let host_seat = 11;
        let guest_keyboard = 20;
        let host_keyboard = 30;
        let key = 30;

        ctx.shadow_table.map_id(guest_seat, host_seat);
        ctx.shadow_table
            .track_interface(guest_seat, "wl_seat".to_string());
        ctx.shadow_table.map_id(guest_keyboard, host_keyboard);
        ctx.shadow_table
            .track_interface(guest_keyboard, "wl_keyboard".to_string());
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);

        // The child press was forwarded while the seat was live. Its release
        // must remain paired even if the parent seat is released first.
        ctx.last_sender_id = host_keyboard;
        let mut keyboard = crate::handler::keyboard::KeyboardHandler::new();
        assert_eq!(
            WlKeyboardHandler::on_key(
                &mut keyboard,
                &mut ctx,
                1,
                100,
                key,
                crate::handler::keyboard::WL_KEY_PRESSED,
            ),
            Action::Forward
        );
        assert_eq!(
            ctx.guest_key_owner(HostId(host_keyboard), key),
            Some(GuestKeyOwner::Physical)
        );

        // Release the parent seat without releasing its child keyboard.
        ctx.last_sender_id = guest_seat;
        assert_eq!(SeatHandler.on_release(&mut ctx), Action::Forward);

        // This is the behavior observed in ChromiumOS' proxy: the child
        // keyboard remains live and its matching release reaches the guest.
        ctx.last_sender_id = host_keyboard;
        assert_eq!(
            WlKeyboardHandler::on_key(
                &mut keyboard,
                &mut ctx,
                2,
                101,
                key,
                crate::handler::keyboard::WL_KEY_RELEASED,
            ),
            Action::Forward,
            "a live child keyboard must forward the release after seat release"
        );
    }
}
