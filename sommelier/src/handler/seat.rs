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
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
    use crate::protocols::wayland::wl_keyboard::{self, WlKeyboardHandler};
    use crate::protocols::wayland::wl_seat::WlSeatHandler;
    use crate::protocols::wayland::{wl_compositor, wl_display};
    use crate::proxy::harness::{Object, Session, TextInput};
    use crate::state::{Context, GuestKeyOwner, HostId};
    use crate::wire::WL_DISPLAY_ID;

    /// A guest seat, a keyboard of it whose focus the host gives to a
    /// surface, and a text input of the seat, which gets the `enter`.
    struct Focused {
        s: Session,
        seat: Object,
        keyboard: Object,
        surface: Object,
        text_input: TextInput,
    }

    impl Focused {
        async fn new() -> Self {
            let mut s = Session::new();
            let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
            let keyboard = s.keyboard_of(seat.id).await;
            let surface = s.surface().await;
            s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
                .await;
            assert_eq!(s.guest_received().len(), 2, "the enter and its modifiers");
            let text_input = s.text_input(seat.id).await;
            let enter = zwp_text_input_v3::Event::Enter {
                surface: surface.id,
            };
            assert_eq!(s.guest_received(), [(text_input.id, enter.into())]);
            Self {
                s,
                seat,
                keyboard,
                surface,
                text_input,
            }
        }

        /// The host's keyboard focus leaves the surface; the guest gets
        /// the keyboard's `leave` and the text input's.
        async fn keyboard_leave(&mut self) {
            let leave = wl_keyboard::evt::leave(self.keyboard.host, 2, self.surface.host);
            self.s.host([leave]).await;
            let surface = self.surface.id;
            assert_eq!(
                self.s.guest_received(),
                [
                    (
                        self.keyboard.id,
                        wl_keyboard::Event::Leave { serial: 2, surface }.into()
                    ),
                    (
                        self.text_input.id,
                        zwp_text_input_v3::Event::Leave { surface }.into()
                    ),
                ]
            );
        }

        /// The host's input method sends the preedit `text`, which the
        /// guest gets.
        async fn preedit(&mut self, text: &str) {
            let host = self.text_input.host;
            self.s
                .host([zwp_text_input_v1::evt::preedit_string(host, 0, text, "").unwrap()])
                .await;
            let end = text.len() as i32;
            let preedit = zwp_text_input_v3::Event::PreeditString {
                text: Some(text.into()),
                cursor_begin: end,
                cursor_end: end,
            };
            let done = zwp_text_input_v3::Event::Done { serial: 1 };
            let id = self.text_input.id;
            assert_eq!(
                self.s.guest_received(),
                [(id, preedit.into()), (id, done.into())]
            );
        }
    }

    #[test]
    fn release_keeps_child_keyboard_routing_state() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let guest_seat = 10;
        let guest_keyboard = 20;
        let host_keyboard = 30;
        ctx.shadow_table
            .insert_paired_for_test(guest_seat, 11, "wl_seat");
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
        ctx.keyboard_to_seat.insert(guest_keyboard, guest_seat);
        ctx.keyboard_focus
            .set_for_test(HostId(host_keyboard), guest_seat, 40, 41);
        ctx.key_generations
            .observe_physical_state(HostId(host_keyboard), 14, 1);
        ctx.last_sender_id = guest_seat;

        assert_eq!(SeatHandler.on_release(&mut ctx), Action::Forward);
        assert_eq!(ctx.keyboard_focus.surface_for_seat(guest_seat), Some(40));
        assert_eq!(ctx.keyboard_to_seat.get(&guest_keyboard), Some(&guest_seat));
        assert!(ctx
            .keyboard_focus
            .focus_for_keyboard(HostId(host_keyboard))
            .is_some());
        assert!(ctx
            .key_generations
            .physically_held(HostId(host_keyboard), 14));
    }

    #[tokio::test]
    async fn release_does_not_end_child_text_input_focus() {
        let mut f = Focused::new().await;
        f.s.enable(f.text_input).await;
        f.preedit("한").await;

        f.s.guest([wl_seat::req::release(f.seat.id)]).await;
        assert_eq!(
            f.s.host_received(),
            [(f.seat.host, wl_seat::Request::Release {}.into())]
        );
        assert_eq!(f.s.guest_received(), [], "the text input keeps its focus");

        // Its composition goes on, and the keyboard's leave, which the
        // keyboard still routes to its seat, ends its focus.
        f.preedit("한글").await;
        f.keyboard_leave().await;
        assert_eq!(f.s.host_received(), []);
    }

    #[tokio::test]
    async fn children_keep_an_ended_seat_whose_id_the_guest_reuses() {
        // A focused keyboard and a text input outlive their seat, which the
        // guest releases and whose ID it then reuses for a surface.
        let mut f = Focused::new().await;
        f.s.guest([wl_seat::req::release(f.seat.id)]).await;
        assert_eq!(
            f.s.host_received(),
            [(f.seat.host, wl_seat::Request::Release {}.into())]
        );
        f.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, f.seat.host)])
            .await;
        let delete_id = wl_display::Event::DeleteId { id: f.seat.id };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
        let compositor = f.s.bind("wl_compositor", wl_compositor::VERSION).await;
        f.s.create(wl_compositor::req::create_surface(compositor, f.seat.id))
            .await;

        // The session's check that no state outlives its object holds, and
        // the keyboard's leave still reaches the seat's text input.
        f.keyboard_leave().await;
        assert_eq!(f.s.host_received(), []);
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

        ctx.shadow_table
            .insert_paired_for_test(guest_seat, host_seat, "wl_seat");
        ctx.shadow_table
            .insert_paired_for_test(guest_keyboard, host_keyboard, "wl_keyboard");
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
                wl_keyboard::KEY_STATE_PRESSED,
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
                wl_keyboard::KEY_STATE_RELEASED,
            ),
            Action::Forward,
            "a live child keyboard must forward the release after seat release"
        );
    }
}
