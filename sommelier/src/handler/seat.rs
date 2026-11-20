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

use crate::handler::Handlers;
use crate::protocols::wayland::wl_seat;
use crate::state::{Context, GuestId, PostDispatch, RequestSender};
use crate::wire::Action;

impl wl_seat::WlSeatHandler for Handlers {
    fn on_get_keyboard(ctx: &mut Context, sender: RequestSender, id: GuestId) -> Action {
        let seat = sender.guest;
        // The keyboard's record is keyed by its host ID, which the forward
        // maps. Its zcr_extended_keyboard_v1 is bound at its first enter
        // (`keyboard::ensure_extended_keyboard_bound`).
        ctx.defer_until_forwarded(PostDispatch::KeyboardCreated { keyboard: id, seat });
        Action::Forward
    }

    fn on_release(_ctx: &mut Context, _sender: RequestSender) -> Action {
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
    use crate::protocols::wayland::wl_keyboard;
    use crate::protocols::wayland::{wl_compositor, wl_display};
    use crate::proxy::harness::{Object, Session, TextInput};
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
            Self::with(Session::new()).await
        }

        /// [`Self::new`] in `s`.
        async fn with(mut s: Session) -> Self {
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

    /// The seat's release leaves its keyboard's routing and key state
    /// alone: a key the extended keyboard peeked and the keyboard pressed
    /// before the release is released to the guest after it, and the
    /// keyboard's focus, which the seat's text input shares, still ends
    /// with a `leave` for both.
    #[tokio::test]
    async fn release_keeps_child_keyboard_routing_state() {
        use crate::protocols::keyboard_extension_unstable_v1::{
            zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
        };
        use crate::protocols::wayland::wl_registry;
        use crate::protocols::AnyRequest;
        use zcr_extended_keyboard_v1::HANDLED_STATE_HANDLED as HANDLED;
        let mut s = Session::new();
        // The proxy binds the keyboard extension for itself, and gives the
        // keyboard an extended keyboard at its first enter.
        s.advertise(
            zcr_keyboard_extension_v1::NAME,
            zcr_keyboard_extension_v1::VERSION,
        )
        .await;
        let bind = s.host_received();
        assert!(
            matches!(
                &bind[..],
                [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { .. }))]
            ),
            "{bind:?}"
        );
        let mut f = Focused::with(s).await;
        let host = f.s.host_received();
        let [(
            _,
            AnyRequest::ZcrKeyboardExtensionV1(
                zcr_keyboard_extension_v1::Request::GetExtendedKeyboard {
                    id: extended,
                    keyboard,
                },
            ),
        )] = host[..]
        else {
            panic!("the keyboard got no extended keyboard: {host:?}");
        };
        assert_eq!(keyboard, f.keyboard.host);
        let backspace = 14;
        let key = |serial, state| {
            let key = wl_keyboard::Event::Key {
                serial,
                time: serial,
                key: backspace,
                state,
            };
            (f.keyboard.id, key.into())
        };
        let ack = |serial| {
            let ack = zcr_extended_keyboard_v1::Request::AckKey {
                serial,
                handled: HANDLED,
            };
            (extended, ack.into())
        };
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        f.s.host([
            zcr_extended_keyboard_v1::evt::peek_key(extended, 3, 3, backspace, pressed),
            wl_keyboard::evt::key(f.keyboard.host, 3, 3, backspace, pressed),
        ])
        .await;
        assert_eq!(f.s.guest_received(), [key(3, pressed)]);
        assert_eq!(f.s.host_received(), [ack(3)]);

        f.s.guest([wl_seat::req::release(f.seat.id)]).await;
        assert_eq!(
            f.s.host_received(),
            [(f.seat.host, wl_seat::Request::Release {}.into())]
        );
        assert_eq!(f.s.guest_received(), []);

        let released = wl_keyboard::KEY_STATE_RELEASED;
        f.s.host([
            zcr_extended_keyboard_v1::evt::peek_key(extended, 4, 4, backspace, released),
            wl_keyboard::evt::key(f.keyboard.host, 4, 4, backspace, released),
        ])
        .await;
        assert_eq!(f.s.guest_received(), [key(4, released)]);
        assert_eq!(f.s.host_received(), [ack(4)]);
        f.keyboard_leave().await;
        assert_eq!(f.s.host_received(), []);
    }

    #[tokio::test]
    async fn release_does_not_end_child_text_input_focus() {
        let mut f = Focused::new().await;
        f.s.enable(f.text_input, 1).await;
        f.preedit("한").await;

        f.s.guest([wl_seat::req::release(f.seat.id)]).await;
        assert_eq!(
            f.s.host_received(),
            [(f.seat.host, wl_seat::Request::Release {}.into())]
        );
        assert_eq!(f.s.guest_received(), [], "the text input keeps its focus");

        // Its composition goes on, and the keyboard's leave, which the
        // keyboard still routes to its seat, ends its focus, so the field
        // commits its preedit before the keyboard's `leave` and its own.
        f.preedit("한글").await;
        let leave = wl_keyboard::evt::leave(f.keyboard.host, 2, f.surface.host);
        f.s.host([leave]).await;
        let (id, surface) = (f.text_input.id, f.surface.id);
        let clear = zwp_text_input_v3::Event::PreeditString {
            text: Some("".into()),
            cursor_begin: 0,
            cursor_end: 0,
        };
        let commit = zwp_text_input_v3::Event::CommitString {
            text: Some("한글".into()),
        };
        assert_eq!(
            f.s.guest_received(),
            [
                (id, clear.into()),
                (id, commit.into()),
                (id, zwp_text_input_v3::Event::Done { serial: 1 }.into()),
                (
                    f.keyboard.id,
                    wl_keyboard::Event::Leave { serial: 2, surface }.into()
                ),
                (id, zwp_text_input_v3::Event::Leave { surface }.into()),
            ]
        );
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

    #[tokio::test]
    async fn seat_release_does_not_break_live_child_keyboard_press_release_pair() {
        // wl_seat.release destroys only the seat proxy. ChromiumOS' C
        // implementation does not destroy already-created wl_keyboard
        // children from sl_host_seat_removed(), so a child can still receive
        // the release matching a press that arrived before the parent seat
        // was released.
        let mut s = Session::new();
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let keyboard = s.keyboard_of(seat.id).await;
        let key = 30;
        let event = |serial, state| {
            let event = wl_keyboard::Event::Key {
                serial,
                time: 100 + serial,
                key,
                state,
            };
            (keyboard.id, event.into())
        };

        // The child press was forwarded while the seat was live. Its release
        // must remain paired even if the parent seat is released first.
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        s.host([wl_keyboard::evt::key(keyboard.host, 1, 101, key, pressed)])
            .await;
        assert_eq!(s.guest_received(), [event(1, pressed)]);

        // Release the parent seat without releasing its child keyboard.
        s.guest([wl_seat::req::release(seat.id)]).await;
        assert_eq!(
            s.host_received(),
            [(seat.host, wl_seat::Request::Release {}.into())]
        );

        // This is the behavior observed in ChromiumOS' proxy: the child
        // keyboard remains live and its matching release reaches the guest.
        let released = wl_keyboard::KEY_STATE_RELEASED;
        s.host([wl_keyboard::evt::key(keyboard.host, 2, 102, key, released)])
            .await;
        assert_eq!(
            s.guest_received(),
            [event(2, released)],
            "a live child keyboard must forward the release after seat release"
        );
        assert_eq!(s.host_received(), []);
    }
}
