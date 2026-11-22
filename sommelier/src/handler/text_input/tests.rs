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

use super::*;
use crate::ime::map_v3_content_type;
use crate::keys::GuestKeyOwner;
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1;
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_manager_v3;
use crate::protocols::wayland::wl_display;
use crate::protocols::wayland::wl_keyboard;
use crate::protocols::wayland::{
    wl_callback, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_touch,
};
use crate::protocols::Iface;
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::{Object, Session};
use crate::state::{GuestId, HostId, Singleton};
use crate::test_support::respell;
use crate::test_support::TestConnection;
use crate::wire::WL_DISPLAY_ID;
use crate::wire::{Outgoing, WireId};
use std::os::fd::OwnedFd;
use zcr_extended_text_input_v1::{evt as ext, Request as Ext};
use zcr_keyboard_extension_v1::Request as KeyboardExtension;
use zcr_text_input_extension_v1::Request as ExtManager;
use zwp_text_input_manager_v1::Request as V1Manager;
use zwp_text_input_v1::{evt as v1, Request as V1};
use zwp_text_input_v3::{req as v3, Event as V3};

/// A guest `zwp_text_input_v3` created for a seat whose keyboard has
/// given its focus to a surface, through a whole proxy [`Session`].
struct Field {
    s: Session,
    /// The guest text input.
    ti: u32,
    /// The host `zwp_text_input_v1` behind it.
    host: u32,
    /// The host `zcr_extended_text_input_v1` behind it.
    host_ext: u32,
    seat: Object,
    keyboard: Object,
    surface: Object,
    /// The guest's `zwp_text_input_manager_v3`.
    manager: u32,
    /// The host `zcr_extended_keyboard_v1` of the keyboard, 0 without
    /// one.
    extended_keyboard: u32,
}

/// What the host offers a [`Field`] besides its seat, keyboard,
/// text-input manager and the newest text-input extension.
#[derive(Clone, Copy)]
struct Host {
    /// Whether the host advertises the keyboard extension, which
    /// reports each key before the input method takes it (`peek_key`)
    /// and takes the proxy's `ack_key`, and sends the keyboard the
    /// default keymap, whose keys repeat.
    keyboard_extension_with_keymap: bool,
}

/// A guest text input and the host objects behind it, which a
/// [`Field`] drives one at a time.
#[derive(Clone, Copy)]
struct TextInput {
    ti: u32,
    host: u32,
    host_ext: u32,
}

/// What a commit that enables or disables a field sends the host
/// besides its edits ([`Field::reset`]).
#[derive(Default)]
struct Reset {
    /// The activation step, which goes first.
    step: Vec<(u32, AnyRequest)>,
    /// The content type the host lacks, after the surrounding text.
    content_type: Vec<(u32, AnyRequest)>,
}

impl Field {
    /// A disabled text input with what `host` offers.
    async fn offered(host: Host) -> Self {
        let mut s = Session::new();
        // Offset the guest's IDs from the host's, so that a request
        // that names a guest object where its host object belongs
        // fails.
        for _ in 0..100 {
            s.new_id();
        }
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let keyboard = s.new_id();
        let keyboard = Object {
            id: keyboard,
            host: s
                .create(wl_seat::req::get_keyboard(seat.id, keyboard))
                .await,
        };
        let surface = s.surface().await;
        let manager = s
            .bind(
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
            )
            .await;
        let version = zcr_text_input_extension_v1::VERSION;
        s.advertise("zcr_text_input_extension_v1", version).await;
        // The proxy binds it for itself.
        s.host_received();
        let mut f = Self {
            s,
            ti: 0,
            host: 0,
            host_ext: 0,
            seat,
            keyboard,
            surface,
            manager,
            extended_keyboard: 0,
        };
        let keyboard_extension = match host.keyboard_extension_with_keymap {
            true => Some(f.offer_keyboard_extension_with_keymap().await),
            false => None,
        };
        assert_eq!(f.enter_surface().await, []);
        if let Some(extension) = keyboard_extension {
            // The keyboard's first enter gets it its extended keyboard.
            let host = f.host_received().await;
            let [(
                sender,
                AnyRequest::ZcrKeyboardExtensionV1(KeyboardExtension::GetExtendedKeyboard {
                    id,
                    keyboard,
                }),
            )] = host[..]
            else {
                panic!("the host got no extended keyboard: {host:?}");
            };
            assert_eq!((sender, keyboard), (extension, f.keyboard.host));
            f.extended_keyboard = id;
        }
        let ti = f.s.new_id();
        f.create_text_input(ti).await;
        // The offset still keeps every guest ID above every host ID.
        let host = [
            f.seat.host,
            f.keyboard.host,
            f.surface.host,
            f.host,
            f.host_ext,
            f.extended_keyboard,
        ];
        let guest = [f.seat.id, f.keyboard.id, f.surface.id, f.manager, f.ti];
        assert!(
            host.iter().max() < guest.iter().min(),
            "{host:?}, {guest:?}"
        );
        f
    }

    /// A disabled text input.
    async fn disabled() -> Self {
        Self::offered(Host {
            keyboard_extension_with_keymap: false,
        })
        .await
    }

    /// A text input the guest enabled, which the host activated.
    async fn enabled() -> Self {
        Self::enable(Self::disabled().await).await
    }

    /// `f` once the guest enabled it and the host activated it.
    async fn enable(mut f: Self) -> Self {
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.host_received().await, f.enable_requests(1));
        f.answered(1);
        f
    }

    /// A text input the host activated, whose guest then committed the
    /// surrounding text `text` with `cursor` and `anchor`, so that each
    /// `done` is numbered 2.
    async fn with_surrounding_text(text: &str, cursor: i32, anchor: i32) -> Self {
        Self::report(Self::enabled().await, text, cursor, anchor).await
    }

    /// `f`, enabled, once its guest committed the surrounding text
    /// `text` with `cursor` and `anchor`.
    async fn report(mut f: Self, text: &str, cursor: i32, anchor: i32) -> Self {
        f.s.guest([
            v3::set_surrounding_text(f.ti, text, cursor, anchor).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        let mut expected = vec![f.surrounding_text(text, cursor as u32, anchor as u32)];
        expected.extend(f.content_type((0, 0), true));
        expected.push(f.commit_state(2));
        assert_eq!(f.host_received().await, expected);
        f.answered(2);
        f
    }

    /// A text input the guest enabled with the surrounding text "가",
    /// which the host activated, on a keyboard whose keys the host
    /// reports to the proxy before its input method takes them
    /// ([`Host::keyboard_extension_with_keymap`]).
    async fn typing() -> Self {
        let mut f = Self::offered(Host {
            keyboard_extension_with_keymap: true,
        })
        .await;
        f.start_typing().await;
        f
    }

    /// The guest enables the field with the surrounding text "가",
    /// and the host activates it, as for [`Self::typing`].
    async fn start_typing(&mut self) {
        let f = self;
        f.s.guest([
            v3::enable(f.ti),
            v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
            v3::commit(f.ti),
        ])
        .await;
        // The default content type, spelled out: the traces' other
        // expectations take it from `map_v3_content_type`.
        let input_type = Ext::SetInputType {
            input_type: zcr_extended_text_input_v1::INPUT_TYPE_TEXT,
            input_mode: zcr_extended_text_input_v1::INPUT_MODE_DEFAULT,
            input_flags: zcr_extended_text_input_v1::INPUT_FLAGS_NONE,
            learning_mode: zcr_extended_text_input_v1::LEARNING_MODE_ENABLED,
            inline_composition_support:
                zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        };
        let expected = [
            f.activate(),
            f.surrounding_text("가", 3, 3),
            f.ext(Ext::SetSurroundingTextSupport { support: 1 }),
            f.ext(input_type),
            f.no_cursor_rectangle(),
            f.commit_state(1),
        ];
        assert_eq!(f.host_received().await, expected);
        f.answered(1);
    }

    /// The host advertises the keyboard extension, which the proxy
    /// binds, and sends the keyboard the default keymap, which the
    /// guest gets; returns the extension's host object.
    async fn offer_keyboard_extension_with_keymap(&mut self) -> u32 {
        self.s
            .advertise(
                "zcr_keyboard_extension_v1",
                zcr_keyboard_extension_v1::VERSION,
            )
            .await;
        let host = self.host_received().await;
        let [(
            _,
            AnyRequest::WlRegistry(wl_registry::Request::Bind {
                id: (_, _, extension),
                ..
            }),
        )] = host[..]
        else {
            panic!("the proxy did not bind the keyboard extension: {host:?}");
        };

        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let flags = xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS;
        let keymap = xkbcommon::xkb::Keymap::new_from_names(&context, "", "", "", "", None, flags)
            .expect("the default keymap");
        let mut keymap = keymap
            .get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        // The size counts the terminating NUL.
        keymap.push(0);
        let size = keymap.len() as u32;
        let file = crate::test_support::shm_pool_file(&keymap);
        let fd = OwnedFd::from(file.try_clone().expect("duplicate the keymap file"));
        let format = wl_keyboard::KEYMAP_FORMAT_XKB_V1;
        let event = wl_keyboard::evt::keymap(self.keyboard.host, format, fd, size);
        self.s.host([event]).await;
        let fd = OwnedFd::from(file);
        let keymap = wl_keyboard::Event::Keymap { format, fd, size };
        assert_eq!(self.s.guest_received(), [(self.keyboard.id, keymap.into())]);
        extension
    }

    /// The guest creates text input `ti` for the seat, which the
    /// keyboard focus enters at once.
    async fn create_text_input(&mut self, ti: u32) {
        self.ti = ti;
        self.s
            .guest([zwp_text_input_manager_v3::req::get_text_input(
                self.manager,
                self.ti,
                self.seat.id,
            )])
            .await;
        let host = self.host_received().await;
        let (_, AnyRequest::ZwpTextInputManagerV1(V1Manager::CreateTextInput { id })) = host[0]
        else {
            panic!("the host got no text input: {host:?}");
        };
        self.host = id;
        self.host_ext = match host[1..] {
            [(
                _,
                AnyRequest::ZcrTextInputExtensionV1(ExtManager::GetExtendedTextInput {
                    id,
                    text_input,
                }),
            )] if text_input == self.host => id,
            _ => panic!("the host got no extended text input: {host:?}"),
        };
        assert_eq!(
            self.s.guest_received(),
            self.events([V3::Enter {
                surface: self.surface.id
            }])
        );
    }

    /// The text input the field drives.
    fn text_input(&self) -> TextInput {
        let Self {
            ti, host, host_ext, ..
        } = *self;
        TextInput { ti, host, host_ext }
    }

    /// Drive `text_input` from now on.
    fn select(&mut self, TextInput { ti, host, host_ext }: TextInput) {
        (self.ti, self.host, self.host_ext) = (ti, host, host_ext);
    }

    /// `events` of the guest text input.
    fn events(&self, events: impl IntoIterator<Item = V3>) -> Vec<(u32, AnyEvent)> {
        events
            .into_iter()
            .map(|event| (self.ti, event.into()))
            .collect()
    }

    /// The guest gets the `done` that answers its commit `serial`, and
    /// nothing else.
    fn answered(&mut self, serial: u32) {
        let events = self.events([V3::Done { serial }]);
        assert_eq!(self.s.guest_received(), events);
    }

    /// `request` of the host text input.
    fn v1(&self, request: V1) -> (u32, AnyRequest) {
        (self.host, request.into())
    }

    /// `request` of the host extension.
    fn ext(&self, request: Ext) -> (u32, AnyRequest) {
        (self.host_ext, request.into())
    }

    fn activate(&self) -> (u32, AnyRequest) {
        self.v1(V1::Activate {
            seat: self.seat.host,
            surface: self.surface.host,
        })
    }

    fn deactivate(&self) -> (u32, AnyRequest) {
        self.v1(V1::Deactivate {
            seat: self.seat.host,
        })
    }

    fn surrounding_text(&self, text: &str, cursor: u32, anchor: u32) -> (u32, AnyRequest) {
        self.v1(V1::SetSurroundingText {
            text: text.into(),
            cursor,
            anchor,
        })
    }

    /// The unset cursor rectangle that enable and disable leave.
    fn no_cursor_rectangle(&self) -> (u32, AnyRequest) {
        self.v1(V1::SetCursorRectangle {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        })
    }

    fn commit_state(&self, serial: u32) -> (u32, AnyRequest) {
        self.v1(V1::CommitState { serial })
    }

    /// The extension requests that set content type `(hint, purpose)`
    /// with surrounding-text `support`.
    fn content_type(&self, (hint, purpose): (u32, u32), support: bool) -> [(u32, AnyRequest); 2] {
        let (input_type, input_mode, input_flags, learning_mode) =
            map_v3_content_type(hint, purpose);
        [
            self.ext(Ext::SetSurroundingTextSupport {
                support: u32::from(support),
            }),
            self.ext(Ext::SetInputType {
                input_type,
                input_mode,
                input_flags,
                learning_mode,
                inline_composition_support:
                    zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            }),
        ]
    }

    /// What the host gets for the guest's commit `serial` that enables
    /// or disables the field: the activation step, the cleared
    /// surrounding text, the content type the host lacks, the cleared
    /// cursor rectangle and the commit.
    fn reset(&self, serial: u32, Reset { step, content_type }: Reset) -> Vec<(u32, AnyRequest)> {
        let mut requests = step;
        requests.push(self.surrounding_text("", 0, 0));
        requests.extend(content_type);
        requests.extend([self.no_cursor_rectangle(), self.commit_state(serial)]);
        requests
    }

    /// What the host gets when the guest's commit `serial` enables a
    /// field with no content type and the host activates it.
    fn enable_requests(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
        let content_type = self.content_type((0, 0), false);
        let step = vec![self.activate()];
        let content_type = content_type.into();
        self.reset(serial, Reset { step, content_type })
    }

    /// What the host gets when the guest's commit `serial` disables an
    /// active field, which the barrier `callback` drains.
    fn disable_requests(&self, callback: u32, serial: u32) -> Vec<(u32, AnyRequest)> {
        let step = vec![self.deactivate(), sync(callback)];
        self.reset(
            serial,
            Reset {
                step,
                ..Reset::default()
            },
        )
    }

    fn password_content_type(&self) -> [(u32, AnyRequest); 2] {
        self.content_type((0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD), false)
    }

    /// What the host gets when the proxy activates the password field
    /// again after the guest's commit `serial`.
    fn password_after_activate(&self, serial: u32) -> Vec<(u32, AnyRequest)> {
        let mut requests = vec![self.activate()];
        requests.extend(self.password_content_type());
        requests.push(self.commit_state(serial));
        requests
    }

    /// The guest releases its seat and the host deletes it.
    async fn release_seat(&mut self) {
        self.s.guest([wl_seat::req::release(self.seat.id)]).await;
        assert_eq!(
            self.host_received().await,
            [(self.seat.host, wl_seat::Request::Release {}.into())]
        );
        self.s
            .host([wl_display::evt::delete_id(WL_DISPLAY_ID, self.seat.host)])
            .await;
        assert_eq!(
            self.s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: self.seat.id }.into()
            )]
        );
    }

    /// The guest binds a new `wl_seat` under the ID of its ended seat;
    /// returns what the host received besides the bind.
    async fn bind_seat_again(&mut self) -> Vec<(u32, AnyRequest)> {
        let registry = self.s.registry().await;
        let name = self.s.advertise("wl_seat", wl_seat::VERSION).await;
        let [(_, AnyEvent::WlRegistry(wl_registry::Event::Global { version, .. }))] =
            self.s.guest_received()[..]
        else {
            panic!("the guest was not advertised the seat");
        };
        let bind = wl_registry::req::bind(registry.id, name, "wl_seat", version, self.seat.id);
        self.s.guest([bind.unwrap()]).await;
        let mut host = self.host_received().await;
        let (
            _,
            AnyRequest::WlRegistry(wl_registry::Request::Bind {
                id: (_, _, seat), ..
            }),
        ) = host.remove(0)
        else {
            panic!("the host got no bind: {host:?}");
        };
        self.seat.host = seat;
        host
    }

    /// The host's keyboard focus leaves the surface.
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
                (self.ti, V3::Leave { surface }.into()),
            ]
        );
    }

    /// The host gives the keyboard's focus to the surface; returns what
    /// the guest got after the keyboard's `enter` and the modifiers the
    /// proxy sends with it.
    async fn enter_surface(&mut self) -> Vec<(u32, AnyEvent)> {
        let enter = wl_keyboard::evt::enter(self.keyboard.host, 1, self.surface.host, &[]);
        self.s.host([enter.unwrap()]).await;
        let mut events = self.s.guest_received();
        let keyboard_events = events.drain(..2.min(events.len())).collect::<Vec<_>>();
        let enter = wl_keyboard::Event::Enter {
            serial: 1,
            surface: self.surface.id,
            keys: Vec::new(),
        };
        let modifiers = wl_keyboard::Event::Modifiers {
            serial: 1,
            mods_depressed: 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        assert_eq!(
            keyboard_events,
            [
                (self.keyboard.id, enter.into()),
                (self.keyboard.id, modifiers.into())
            ]
        );
        events
    }

    /// The host's keyboard focus enters a new surface, which the field
    /// follows from now on.
    async fn keyboard_enter_new_surface(&mut self) {
        self.surface = self.s.surface().await;
        self.keyboard_enter().await;
    }

    /// The host's keyboard focus enters the surface again.
    async fn keyboard_enter(&mut self) {
        let surface = self.surface.id;
        assert_eq!(
            self.enter_surface().await,
            self.events([V3::Enter { surface }])
        );
    }

    /// The guest gets a keyboard of the seat, to which the host gives
    /// the focus on the surface.
    async fn focus_keyboard_of_seat(&mut self) {
        let keyboard = self.s.new_id();
        let host = self
            .s
            .create(wl_seat::req::get_keyboard(self.seat.id, keyboard))
            .await;
        self.keyboard = Object { id: keyboard, host };
        assert_eq!(self.enter_surface().await, []);
    }

    /// The host's `button` event of `pointer`, of the left button in
    /// `state`.
    fn button(pointer: Object, state: u32) -> Outgoing<u32> {
        wl_pointer::evt::button(pointer.host, 4, 500, BTN_LEFT, state)
    }

    /// The `button` event `pointer` gets for [`Self::button`].
    fn guest_button(pointer: Object, state: u32) -> (u32, AnyEvent) {
        let button = wl_pointer::Event::Button {
            serial: 4,
            time: 500,
            button: BTN_LEFT,
            state,
        };
        (pointer.id, button.into())
    }

    /// A host `wl_touch.down` of `touch` on `surface`, of touch point
    /// `id`.
    fn down(touch: Object, surface: Object, id: i32) -> Outgoing<u32> {
        wl_touch::evt::down(touch.host, 6, 600, surface.host, id, 256, 512)
    }

    /// The `down` event `touch` gets for [`Self::down`].
    fn guest_down(touch: Object, surface: Object, id: i32) -> (u32, AnyEvent) {
        let down = wl_touch::Event::Down {
            serial: 6,
            time: 600,
            surface: surface.id,
            id,
            x: 256,
            y: 512,
        };
        (touch.id, down.into())
    }

    /// The host completes the drain barrier `callback`.
    async fn drain(&mut self, callback: u32) {
        self.s.host([wl_callback::evt::done(callback, 0)]).await;
    }

    /// The host answers the syncs the proxy sent with the recoveries
    /// of held keys that waited, the only requests it got: the `done`
    /// decides the one that still waits, if any, as an autorepeat's
    /// (`crate::keys::recovery`).
    /// What the host received, with each surrounding text's barrier,
    /// the `wl_display.sync` right after it, answered as Exo's display
    /// answers it, and left out: the IME core's Exo mirror waits for
    /// it, and no test of this file is about it but
    /// `a_report_reaches_the_host_with_its_barrier`.
    async fn host_received(&mut self) -> Vec<(u32, AnyRequest)> {
        let mut received = self.s.host_received();
        let mut glued = Vec::new();
        let mut i = 1;
        while i < received.len() {
            let report = matches!(
                received[i - 1].1,
                AnyRequest::ZwpTextInputV1(V1::SetSurroundingText { .. })
            );
            match received[i].1 {
                AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) if report => {
                    glued.push(callback);
                    received.remove(i);
                }
                _ => i += 1,
            }
        }
        for callback in glued {
            let done = wl_callback::evt::done(callback, 0);
            let delete = wl_display::evt::delete_id(WL_DISPLAY_ID, callback);
            self.s.host([done, delete]).await;
        }
        received
    }

    async fn answer_recovery_syncs(&mut self) {
        for (sender, request) in self.host_received().await {
            let AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) = request else {
                panic!("the host got {request:?} from {sender}, not a recovery's sync");
            };
            self.s.host([wl_callback::evt::done(callback, 0)]).await;
        }
    }

    /// The host input method's `preedit_string`.
    fn preedit(&self, text: &str, commit: &str) -> Outgoing<u32> {
        v1::preedit_string(self.host, 0, text, commit).unwrap()
    }

    /// The host input method's `commit_string`.
    fn commit(&self, text: &str) -> Outgoing<u32> {
        v1::commit_string(self.host, 0, text).unwrap()
    }

    /// The host input method's `delete_surrounding_text` of the bytes
    /// `index..index + length`, as Exo sends it.
    fn delete_range(&self, index: i32, length: u32) -> Outgoing<u32> {
        v1::delete_surrounding_text(self.host, index, length)
    }

    /// The host input method's `confirm_preedit`.
    fn confirm(&self) -> Outgoing<u32> {
        ext::confirm_preedit(self.host_ext, 1)
    }

    /// The extended keyboard's `peek_key` of `k`, which the host
    /// sends before its input method can take the key.
    fn peek(&self, k: Stroke) -> Outgoing<u32> {
        let peek_key = zcr_extended_keyboard_v1::evt::peek_key;
        peek_key(self.extended_keyboard, k.serial, k.time, k.key, k.state)
    }

    /// The host keyboard's `key` of `k`.
    fn key(&self, k: Stroke) -> Outgoing<u32> {
        wl_keyboard::evt::key(self.keyboard.host, k.serial, k.time, k.key, k.state)
    }

    /// The host input method's `keysym` `sym` of `k`, whose serial
    /// and time Exo sends in each other's arguments.
    fn keysym(&self, sym: u32, k: Stroke) -> Outgoing<u32> {
        v1::keysym(self.host, k.time, k.serial, sym, k.state, 0)
    }

    /// The `key` of `k` the guest keyboard gets.
    fn guest_key(&self, k: Stroke) -> (u32, AnyEvent) {
        key(self.keyboard.id, k.key, k.state, k.serial, k.time)
    }

    /// What the guest gets when an empty confirmation stands for the
    /// held key `held`, the key's latest peek: a press and a release,
    /// both with the serial and time of that peek, and `done`.
    fn recovered(&self, held: Stroke) -> Vec<(u32, AnyEvent)> {
        let press = Stroke {
            state: PRESSED,
            ..held
        };
        let release = Stroke {
            state: RELEASED,
            ..held
        };
        let mut events = vec![self.guest_key(press), self.guest_key(release)];
        events.extend(self.events([DONE]));
        events
    }

    /// The proxy's `ack_key` of key `serial`, which the guest
    /// `handled`.
    fn ack(&self, serial: u32, handled: bool) -> (u32, AnyRequest) {
        let handled = match handled {
            true => zcr_extended_keyboard_v1::HANDLED_STATE_HANDLED,
            false => zcr_extended_keyboard_v1::HANDLED_STATE_NOT_HANDLED,
        };
        let ack = zcr_extended_keyboard_v1::Request::AckKey { serial, handled };
        (self.extended_keyboard, ack.into())
    }

    /// The IME repeat lease of `key` on the host keyboard, which no
    /// peer sees until the key's next confirmation.
    fn repeat_owner(&mut self, key: u32) -> Option<u32> {
        let keyboard = HostId::for_test(self.keyboard.host);
        assert_eq!(key, BACKSPACE, "only Backspace is leased");
        let lease = self.s.ctx().keys.backspace_lease(keyboard);
        lease.map(|ti| ti.0.wire())
    }

    /// The owner the proxy assigned the guest's press of `key` on the
    /// host keyboard, which no peer sees.
    fn owner(&mut self, key: u32) -> Option<GuestKeyOwner> {
        let keyboard = HostId::for_test(self.keyboard.host);
        self.s.ctx().keys.owner(keyboard, key)
    }

    /// The proxy's state of the text input, for state no peer sees.
    fn state(&mut self) -> &crate::ime::TextInput {
        let ti = TiId(GuestId::for_test(self.ti));
        self.s.ctx().ime.get(ti).expect("the text input")
    }

    /// The host gives the held Backspace to its input method, which
    /// empties a preedit: the proxy leases Backspace's repeat to the
    /// text input. Returns the press.
    async fn lease_backspace_repeat(&mut self) -> Stroke {
        let press = Stroke {
            key: BACKSPACE,
            state: PRESSED,
            serial: 700,
            time: 1_234,
        };
        let events = [
            self.peek(press),
            self.preedit("가", ""),
            self.preedit("", ""),
        ];
        self.s.host(events).await;
        let expected = [preedit("가"), DONE, no_preedit(), DONE];
        assert_eq!(self.s.guest_received(), self.events(expected));
        assert_eq!(self.repeat_owner(BACKSPACE), Some(self.ti));
        press
    }

    /// Whether the proxy holds `key` of the host keyboard, which no peer
    /// sees until the key's next event.
    fn held(&mut self, key: u32) -> bool {
        let keyboard = HostId::for_test(self.keyboard.host);
        self.s.ctx().keys.held(keyboard, key)
    }
}

/// A key event of the host keyboard.
#[derive(Clone, Copy, Debug)]
struct Stroke {
    key: u32,
    state: u32,
    serial: u32,
    time: u32,
}

/// The evdev code of the space bar.
const SPACE: u32 = 57;
/// The evdev code of the left mouse button.
const BTN_LEFT: u32 = 0x110;

/// The `wl_display.sync` that creates drain barrier `callback`.
fn sync(callback: u32) -> (u32, AnyRequest) {
    (WL_DISPLAY_ID, wl_display::Request::Sync { callback }.into())
}

/// The callback of the one drain barrier among `requests`.
fn barrier(requests: &[(u32, AnyRequest)]) -> u32 {
    let callbacks = requests
        .iter()
        .filter_map(|request| match request {
            (_, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) => Some(*callback),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [callback] = callbacks[..] else {
        panic!("no one drain barrier in {requests:?}");
    };
    callback
}

/// The guest's `preedit_string` of `text`, with the cursor at its end.
fn preedit(text: &str) -> V3 {
    let end = text.len() as i32;
    preedit_at(text, end, end)
}

fn preedit_at(text: &str, cursor_begin: i32, cursor_end: i32) -> V3 {
    V3::PreeditString {
        text: Some(text.into()),
        cursor_begin,
        cursor_end,
    }
}

/// The guest's `preedit_string` that clears its preedit.
fn no_preedit() -> V3 {
    preedit_at("", 0, 0)
}

fn commit(text: &str) -> V3 {
    V3::CommitString {
        text: Some(text.into()),
    }
}

/// Exo drops a content type sent while no input method is attached:
/// the disable sends none, neither does an enable while the
/// deactivation drains, and the activation the barrier releases sends
/// it once.
#[tokio::test]
async fn a_deferred_activation_replays_the_committed_content_type() {
    use zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD as PASSWORD;
    let mut f = Field::enabled().await;
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, f.disable_requests(callback, 2));

    f.s.guest([
        v3::enable(f.ti),
        v3::set_content_type(f.ti, 0, PASSWORD),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(f.host_received().await, f.reset(3, Reset::default()));
    f.answered(3);

    f.drain(callback).await;
    let mut expected = vec![f.activate()];
    expected.extend(f.content_type((0, PASSWORD), false));
    expected.push(f.commit_state(3));
    assert_eq!(f.host_received().await, expected);
    let (input_type, _, _, learning_mode) = map_v3_content_type(0, PASSWORD);
    assert_eq!(input_type, zcr_extended_text_input_v1::INPUT_TYPE_PASSWORD);
    assert_eq!(
        learning_mode,
        zcr_extended_text_input_v1::LEARNING_MODE_DISABLED
    );
}

/// A text input the guest enabled on a password field, which the host
/// activated and attached its input method to.
async fn attached_password_field() -> Field {
    let mut f = Field::disabled().await;
    f.s.guest([
        v3::enable(f.ti),
        v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
        v3::commit(f.ti),
    ])
    .await;
    let content_type = f.password_content_type().into();
    assert_eq!(
        f.host_received().await,
        f.reset(
            1,
            Reset {
                step: vec![f.activate()],
                content_type
            }
        )
    );
    f.answered(1);
    f.s.host([v1::enter(f.host, f.surface.host)]).await;
    f
}

#[tokio::test]
async fn the_enter_answering_an_activate_resends_nothing() {
    let mut f = attached_password_field().await;
    assert_eq!(f.host_received().await, []);

    // The content type sent after the activate is the one Exo applied.
    f.s.guest([
        v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(f.host_received().await, [f.commit_state(2)]);
    f.answered(2);
}

/// Exo detached on its own, as when its input method is destroyed, and
/// has no surface left to attach to: the proxy activates again and
/// resends the content type the new attachment reset.
#[tokio::test]
async fn an_exo_detach_activates_again_with_the_content_type() {
    let mut f = attached_password_field().await;
    f.s.host([v1::leave(f.host)]).await;
    assert_eq!(f.host_received().await, f.password_after_activate(1));

    // The enter answering that activate resends nothing.
    f.s.host([v1::enter(f.host, f.surface.host)]).await;
    assert_eq!(f.host_received().await, []);
    // The new generation takes host events.
    f.s.host([f.commit("가")]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([commit("가"), V3::Done { serial: 1 }])
    );
}

/// Exo detaches while the guest shows a preedit that its detach did
/// not confirm (E6): the guest commits it.
#[tokio::test]
async fn an_exo_detach_commits_the_guest_preedit() {
    let mut f = attached_password_field().await;

    // Without a preedit the guest is told nothing.
    f.s.host([v1::leave(f.host), v1::enter(f.host, f.surface.host)])
        .await;
    assert_eq!(f.s.guest_received(), []);
    assert_eq!(f.host_received().await, f.password_after_activate(1));

    // The input method's process restarts mid-composition.
    f.s.host([f.preedit("가", "가")]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([preedit("가"), V3::Done { serial: 1 }])
    );
    f.s.host([v1::leave(f.host)]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }])
    );
    assert_eq!(f.host_received().await, f.password_after_activate(1));

    // The composition went with it: a commit clears no preedit.
    f.s.host([f.commit("나")]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([commit("나"), V3::Done { serial: 1 }])
    );
}

/// The focus leaves a field while the guest shows a preedit: the guest
/// commits it before the keyboard's `leave` and the text input's (F1),
/// while the field still has the focus. Exo's detach confirms the
/// composition too (E6), but the keyboard hears of the focus change
/// first (E12), so its confirmation arrives while the link drains, and
/// is dropped.
#[tokio::test]
async fn a_focus_loss_commits_the_shown_preedit_before_leave() {
    let mut f = Field::enabled().await;
    f.s.host([f.preedit("가", "가")]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([preedit("가"), V3::Done { serial: 1 }])
    );
    let leave = wl_keyboard::evt::leave(f.keyboard.host, 2, f.surface.host);
    f.s.host([leave]).await;
    let surface = f.surface.id;
    let leave = wl_keyboard::Event::Leave { serial: 2, surface };
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push((f.keyboard.id, leave.into()));
    expected.extend(f.events([V3::Leave { surface }]));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(callback)]);

    f.s.host([f.confirm(), v1::leave(f.host)]).await;
    assert_eq!(f.s.guest_received(), []);
    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
}

/// Exo's text input hears of a focus change before the keyboard: its
/// `leave` arrives first, and the proxy activates again on the surface
/// Exo has just left, which Exo keeps without attaching.
#[tokio::test]
async fn an_exo_detach_before_the_keyboard_leave_activates_then_drains() {
    let mut f = attached_password_field().await;
    f.s.host([v1::leave(f.host)]).await;
    assert_eq!(f.host_received().await, f.password_after_activate(1));

    // The keyboard leave then deactivates it behind a barrier.
    f.keyboard_leave().await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(callback)]);

    // Exo never attached, so it sends no leave, and the drain ends with
    // nothing more to send.
    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
    // The host is inactive: it neither forwards nor takes its events.
    f.s.host([f.commit("가")]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// Exo detached while the guest's seat cannot carry an `activate`:
/// nothing is sent, not even a changed content type, until it can.
#[tokio::test]
async fn a_content_type_waits_for_exo_to_attach_again() {
    let mut f = attached_password_field().await;
    f.release_seat().await;
    f.s.host([v1::leave(f.host)]).await;
    assert_eq!(f.host_received().await, []);

    f.s.guest([
        v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_NORMAL),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(f.host_received().await, [f.commit_state(2)]);
    f.answered(2);
}

#[tokio::test]
async fn the_leave_answering_a_deactivate_is_ignored() {
    let mut f = attached_password_field().await;
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, f.disable_requests(callback, 2));

    f.s.host([v1::leave(f.host)]).await;
    assert_eq!(f.host_received().await, []);
    // The barrier still drains the generation: an enable waits for it.
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.reset(3, Reset::default()));
    f.answered(3);
    f.drain(callback).await;
    let mut expected = vec![f.activate()];
    expected.extend(f.content_type((0, 0), false));
    expected.push(f.commit_state(3));
    assert_eq!(f.host_received().await, expected);
}

/// The text input's seat ends, and the guest binds a new `wl_seat`
/// under its ID, which gives the text input a host seat again.
#[tokio::test]
async fn an_activation_deferred_for_an_unmapped_seat_sends_the_content_type() {
    let mut f = Field::disabled().await;
    f.release_seat().await;

    // The seat has no host object: nothing can activate, which the
    // proxy warns of once.
    f.s.guest([
        v3::enable(f.ti),
        v3::set_content_type(f.ti, 0, zwp_text_input_v3::CONTENT_PURPOSE_PASSWORD),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(f.host_received().await, f.reset(1, Reset::default()));
    f.answered(1);
    // Only the log shows the warning, so read the flag that makes it
    // once.
    assert!(f.state().warned_for_test());

    // Once it has one, the text input activates, and its content type
    // follows the activation.
    let host = f.bind_seat_again().await;
    let mut expected = vec![f.activate()];
    expected.extend(f.password_content_type());
    expected.push(f.commit_state(1));
    assert_eq!(host, expected);
}

/// One deactivation owns one barrier however the guest's state changes
/// while it drains, and a field disabled by then is not activated.
#[tokio::test]
async fn activation_barrier_coalesces_changes_and_honors_final_disabled_target() {
    let mut f = Field::enabled().await;
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, f.disable_requests(callback, 2));

    // The guest enables the field on two other surfaces in turn, then
    // disables it.
    for serial in [3, 4] {
        f.keyboard_leave().await;
        f.keyboard_enter_new_surface().await;
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.host_received().await, f.reset(serial, Reset::default()));
        f.answered(serial);
    }
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.reset(5, Reset::default()));

    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
}

/// Exo has one seat and gives its input method to the text input that
/// activated last, so a second text input, of the same or another
/// guest seat, cannot take it from an enabled one.
#[tokio::test]
async fn second_text_input_enable_on_the_connection_is_ignored() {
    for other_seat in [false, true] {
        let mut f = Field::enabled().await;
        let first = f.text_input();
        if other_seat {
            f.seat = f.s.bind_paired("wl_seat", wl_seat::VERSION).await;
            f.focus_keyboard_of_seat().await;
        }
        let id = f.s.new_id();
        f.create_text_input(id).await;

        // A rejected enable still counts: its commit is the first.
        f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(
            f.host_received().await,
            f.reset(1, Reset::default()),
            "another seat: {other_seat}"
        );

        // Its disable takes nothing from the first, which Exo still
        // gives its input method.
        f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
        assert_eq!(f.host_received().await, f.reset(2, Reset::default()));
        f.select(first);
        f.s.host([f.commit("가")]).await;
        assert_eq!(
            f.s.guest_received(),
            f.events([commit("가"), V3::Done { serial: 1 }])
        );
    }
}

/// The text inputs of a seat follow its focus in ascending ID order,
/// whatever order the guest created them in, so what the proxy sends
/// never depends on how it hashes them. With eight, an order that
/// depends on hashing is all but certain to differ; two would match it
/// half the time.
#[tokio::test]
async fn the_text_inputs_of_a_seat_follow_its_focus_in_id_order() {
    let mut f = Field::enabled().await;
    let first = f.text_input();
    let mut ids = vec![first.ti];
    ids.extend((1..8).map(|_| f.s.new_id()));
    for &id in ids[1..].iter().rev() {
        f.create_text_input(id).await;
    }
    f.select(first);
    let (keyboard, surface) = (f.keyboard, f.surface.id);

    let leave = wl_keyboard::evt::leave(keyboard.host, 2, f.surface.host);
    f.s.host([leave]).await;
    let mut expected: Vec<(u32, AnyEvent)> = vec![(
        keyboard.id,
        wl_keyboard::Event::Leave { serial: 2, surface }.into(),
    )];
    expected.extend(ids.iter().map(|&ti| (ti, V3::Leave { surface }.into())));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(callback)]);

    let entered: Vec<(u32, AnyEvent)> = ids
        .iter()
        .map(|&ti| (ti, V3::Enter { surface }.into()))
        .collect();
    assert_eq!(f.enter_surface().await, entered);
    assert_eq!(f.host_received().await, []);
}

/// The cursor and anchor are byte offsets into the surrounding text,
/// which replacing an invalid sequence would move, so surrounding text
/// that is not UTF-8 cannot reach the host. It empties the field's
/// surrounding text instead, rather than leave the host's input method
/// editing text the guest no longer has, and the field keeps its
/// support for surrounding text, so the host's content type stays. The
/// session goes on, and the next valid text reaches the host.
#[tokio::test]
async fn surrounding_text_that_is_not_utf8_clears_the_hosts() {
    let mut f = Field::enabled().await;
    f.s.guest([
        v3::set_surrounding_text(f.ti, "가", 3, 3).unwrap(),
        v3::commit(f.ti),
    ])
    .await;
    let mut expected = vec![f.surrounding_text("가", 3, 3)];
    expected.extend(f.content_type((0, 0), true));
    expected.push(f.commit_state(2));
    assert_eq!(f.host_received().await, expected);
    f.answered(2);

    let latin1 = v3::set_surrounding_text(f.ti, "Cafe", 3, 3).unwrap();
    f.s.guest([
        v3::set_surrounding_text(f.ti, "나", 3, 3).unwrap(),
        respell(latin1, "Cafe", b"Caf\xe9"),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(
        f.host_received().await,
        [f.surrounding_text("", 0, 0), f.commit_state(3)]
    );
    f.answered(3);

    f.s.guest([
        v3::set_surrounding_text(f.ti, "Café", 5, 5).unwrap(),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(
        f.host_received().await,
        [f.surrounding_text("Café", 5, 5), f.commit_state(4)]
    );
    f.answered(4);
}

/// The v3 content type maps to the host's, which is told with it that
/// a v3 client supports inline composition, and whether the field has
/// surrounding text.
#[tokio::test]
async fn content_type_mapping_preserves_pin_privacy_and_inline_composition() {
    use zcr_extended_text_input_v1::{
        INPUT_FLAGS_HAS_BEEN_PASSWORD as SECRET, INPUT_FLAGS_SPELLCHECK_ON, INPUT_MODE_NUMERIC,
        INPUT_TYPE_NUMBER, INPUT_TYPE_PASSWORD, INPUT_TYPE_TEXT_AREA, LEARNING_MODE_DISABLED,
    };
    use zwp_text_input_v3::{
        CONTENT_HINT_MULTILINE, CONTENT_HINT_SENSITIVE_DATA, CONTENT_HINT_SPELLCHECK,
        CONTENT_PURPOSE_PASSWORD, CONTENT_PURPOSE_PIN,
    };
    // A text field, a terminal and a password field.
    use zcr_extended_text_input_v1::{
        INPUT_FLAGS_NONE, INPUT_MODE_DEFAULT, INPUT_TYPE_TEXT, LEARNING_MODE_ENABLED,
    };
    use zwp_text_input_v3::CONTENT_PURPOSE_TERMINAL;
    let text = (
        INPUT_TYPE_TEXT,
        INPUT_MODE_DEFAULT,
        INPUT_FLAGS_NONE,
        LEARNING_MODE_ENABLED,
    );
    assert_eq!(map_v3_content_type(0, 0), text);
    assert_eq!(map_v3_content_type(0, CONTENT_PURPOSE_TERMINAL), text);
    let password = (
        INPUT_TYPE_PASSWORD,
        INPUT_MODE_DEFAULT,
        SECRET,
        LEARNING_MODE_DISABLED,
    );
    assert_eq!(map_v3_content_type(0, CONTENT_PURPOSE_PASSWORD), password);
    // A PIN: digits, secret and never learned.
    assert_eq!(
        map_v3_content_type(0, CONTENT_PURPOSE_PIN),
        (
            INPUT_TYPE_NUMBER,
            INPUT_MODE_NUMERIC,
            SECRET,
            LEARNING_MODE_DISABLED
        )
    );
    assert_eq!(
        map_v3_content_type(CONTENT_HINT_SENSITIVE_DATA, CONTENT_PURPOSE_PASSWORD),
        password
    );
    assert_eq!(
        map_v3_content_type(CONTENT_HINT_MULTILINE, 0).0,
        INPUT_TYPE_TEXT_AREA
    );
    assert_eq!(
        map_v3_content_type(CONTENT_HINT_SPELLCHECK, 0).2,
        INPUT_FLAGS_SPELLCHECK_ON,
        "v3 spellcheck must map to Chrome spellcheck_on"
    );

    let mut f = Field::enabled().await;
    f.s.guest([
        v3::set_content_type(f.ti, 0, CONTENT_PURPOSE_PIN),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(
        f.host_received().await,
        [
            f.ext(Ext::SetSurroundingTextSupport {
                support: zcr_extended_text_input_v1::SURROUNDING_TEXT_SUPPORT_UNSUPPORTED
            }),
            f.ext(Ext::SetInputType {
                input_type: INPUT_TYPE_NUMBER,
                input_mode: INPUT_MODE_NUMERIC,
                input_flags: SECRET,
                learning_mode: LEARNING_MODE_DISABLED,
                inline_composition_support:
                    zcr_extended_text_input_v1::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
            }),
            f.commit_state(2),
        ]
    );
    f.answered(2);
}

#[tokio::test]
async fn surrounding_text_support_follows_the_field_and_resends_only_on_change() {
    let mut f = Field::enabled().await;
    let edits = |f: &Field, serial, support| {
        let mut requests = Vec::from(f.content_type((0, 0), support));
        requests.push(f.commit_state(serial));
        requests
    };

    // Chromium enables with a content type and no surrounding text,
    // which the host is told unsupported ([`Field::enabled`]), and sends
    // the text in a later commit, with no new content type. Exo learns
    // the support only with a content type, so it is resent.
    f.s.guest([
        v3::set_surrounding_text(f.ti, "abc", 3, 3).unwrap(),
        v3::commit(f.ti),
    ])
    .await;
    let mut expected = vec![f.surrounding_text("abc", 3, 3)];
    expected.extend(edits(&f, 2, true));
    assert_eq!(f.host_received().await, expected);
    f.answered(2);

    // While the support holds, the content type is not resent: before
    // v8 a resend changes Exo's type twice, which can reset its
    // composition.
    f.s.guest([
        v3::set_surrounding_text(f.ti, "abcd", 4, 4).unwrap(),
        v3::set_content_type(f.ti, 0, 0),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(
        f.host_received().await,
        [f.surrounding_text("abcd", 4, 4), f.commit_state(3)]
    );
    f.answered(3);

    // A disable and enable reset the editor state: the content type is
    // replayed with the support of the field's new, empty state.
    f.s.guest([
        v3::disable(f.ti),
        v3::enable(f.ti),
        v3::set_content_type(f.ti, 0, 0),
        v3::commit(f.ti),
    ])
    .await;
    let content_type = f.content_type((0, 0), false).into();
    assert_eq!(
        f.host_received().await,
        f.reset(
            4,
            Reset {
                content_type,
                ..Reset::default()
            }
        )
    );
    f.answered(4);
}

#[tokio::test]
async fn destroy_drops_v3_request_and_cleans_host_extension_routing() {
    let mut f = Field::enabled().await;
    f.s.host([f.preedit("가", "")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.guest([v3::destroy(f.ti)]).await;
    let host = f.host_received().await;
    // The destroy is the final disable: the host extension goes after
    // the generation's barrier.
    let callback = barrier(&host);
    assert_eq!(
        host,
        [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
    );
    assert_eq!(
        f.s.guest_received(),
        [(
            WL_DISPLAY_ID,
            wl_display::Event::DeleteId { id: f.ti }.into()
        )],
        "the guest-local destruction is acknowledged by wl_display"
    );

    // The host keeps the v1 object, which has no destructor, and the
    // extension's ID until its delete_id: a new text input takes
    // neither.
    let (old_host, old_host_ext) = (f.host, f.host_ext);
    let ti = f.s.new_id();
    f.create_text_input(ti).await;
    assert!(![old_host, old_host_ext].contains(&f.host));
    assert!(![old_host, old_host_ext].contains(&f.host_ext));
    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);

    // A late confirm of the destroyed extension reaches no one, not even
    // the new text input's composition.
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    f.answered(1);
    f.s.host([f.preedit("나", ""), ext::confirm_preedit(old_host_ext, 0)])
        .await;
    let expected = [preedit("나"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    // The host's delete_id frees the extension's ID, which the next
    // host object takes.
    f.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, old_host_ext)])
        .await;
    let ti = f.s.new_id();
    f.create_text_input(ti).await;
    assert_eq!(f.host, old_host_ext);
}

/// A destroyed text input whose seat ended, or awaits its
/// `delete_id`, sends no `deactivate`: no seat can carry it.
#[tokio::test]
async fn destroy_after_seat_release_does_not_deactivate_destroyed_host_seat() {
    for deleted in [false, true] {
        let mut f = Field::enabled().await;
        if deleted {
            f.release_seat().await;
        } else {
            f.s.guest([wl_seat::req::release(f.seat.id)]).await;
            let release = (f.seat.host, wl_seat::Request::Release {}.into());
            assert_eq!(f.host_received().await, [release]);
        }

        f.s.guest([v3::destroy(f.ti)]).await;
        assert_eq!(
            f.host_received().await,
            [f.ext(Ext::Destroy {})],
            "deleted: {deleted}"
        );
        assert_eq!(
            f.s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: f.ti }.into()
            )]
        );
    }
}

/// The active text input's seat ended, and the guest reused its ID for
/// a surface, which the host would reject as a seat.
#[tokio::test]
async fn a_seat_id_reused_for_another_object_is_not_sent_as_a_seat() {
    let mut f = Field::enabled().await;
    f.release_seat().await;
    let compositor = f.s.bind("wl_compositor", wl_compositor::VERSION).await;
    f.s.create(wl_compositor::req::create_surface(compositor, f.seat.id))
        .await;

    // Neither disabling the field nor enabling it again sends the ID:
    // each commit sends only its edits.
    for (serial, request) in [(2, v3::disable(f.ti)), (3, v3::enable(f.ti))] {
        f.s.guest([request, v3::commit(f.ti)]).await;
        assert_eq!(f.host_received().await, f.reset(serial, Reset::default()));
    }
    f.answered(3);
    // The disable ended the host's generation: its events reach no one.
    f.s.host([f.commit("가")]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// A host without the text-input extension at v11 gives a text input
/// no input method: the guest's text input is inert, sends nothing,
/// and its destructor still completes. One warning per process says
/// why.
#[tokio::test]
async fn a_text_input_without_a_v11_extension_has_no_input_method() {
    let since = zcr_extended_text_input_v1::EVT_CONFIRM_PREEDIT_SINCE;
    for extension in [None, Some(since - 1)] {
        let mut s = Session::new();
        let (seat, _) = s.keyboard().await;
        let version = zwp_text_input_manager_v1::VERSION;
        let manager = s.bind("zwp_text_input_manager_v1", version).await;
        if let Some(version) = extension {
            s.advertise("zcr_text_input_extension_v1", version).await;
            // The proxy binds it for itself.
            s.host_received();
        }
        let ti = s.new_id();
        s.guest([
            zwp_text_input_manager_v3::req::get_text_input(manager, ti, seat),
            v3::enable(ti),
            v3::set_content_type(ti, 0, 0),
            v3::commit(ti),
            v3::destroy(ti),
        ])
        .await;
        assert_eq!(s.host_received(), [], "{extension:?}");
        let delete_id = wl_display::Event::DeleteId { id: ti };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
    }
    let warned = crate::warn_once::WarnOnce::new();
    assert!(warn_no_input_method(&warned));
    assert!(!warn_no_input_method(&warned));
}

/// A dropped keysym is logged once, with the test's own flag.
#[test]
fn a_dropped_keysym_is_logged_once() {
    let once = crate::warn_once::WarnOnce::new();
    assert!(log_dropped_keysym(&once, 0x61, 1));
    assert!(!log_dropped_keysym(&once, 0x61, 1));
}

/// An extension whose version the proxy cannot find gives no input
/// method: the v11 check fails closed instead of passing.
#[tokio::test]
async fn an_extension_of_unknown_version_gives_no_input_method() {
    let mut s = Session::new();
    let (seat, _) = s.keyboard().await;
    let version = zwp_text_input_manager_v1::VERSION;
    let manager = s.bind("zwp_text_input_manager_v1", version).await;
    let version = zcr_text_input_extension_v1::VERSION;
    s.advertise(zcr_text_input_extension_v1::NAME, version)
        .await;
    // The proxy binds it for itself.
    let host = s.host_received();
    let [(
        _,
        AnyRequest::WlRegistry(wl_registry::Request::Bind {
            id: (_, _, extension),
            ..
        }),
    )] = host[..]
    else {
        panic!("the proxy did not bind the extension: {host:?}");
    };
    // With its version, the extension gives a text input an input method.
    let ti = s.new_id();
    let get_text_input = zwp_text_input_manager_v3::req::get_text_input;
    s.guest([get_text_input(manager, ti, seat)]).await;
    let host = s.host_received();
    assert!(
        matches!(
            host[..],
            [
                (
                    _,
                    AnyRequest::ZwpTextInputManagerV1(V1Manager::CreateTextInput { .. })
                ),
                (
                    _,
                    AnyRequest::ZcrTextInputExtensionV1(ExtManager::GetExtendedTextInput { .. })
                ),
            ]
        ),
        "{host:?}"
    );

    // Injects a fault no peer can cause: no peer can make the proxy lose
    // a bound extension's version; retiring its object stands in for
    // that bookkeeping fault.
    s.ctx()
        .shadow_table
        .retire_host(HostId::for_test(extension));
    let ti = s.new_id();
    s.guest([
        get_text_input(manager, ti, seat),
        v3::enable(ti),
        v3::set_content_type(ti, 0, 0),
        v3::commit(ti),
        v3::destroy(ti),
    ])
    .await;
    assert_eq!(s.host_received(), []);
    let delete_id = wl_display::Event::DeleteId { id: ti };
    assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
}

/// A leave is a hard boundary: what the guest requests until the next
/// enter does not reach the host. Its commits still count
/// (`v3_done_serial_counts_commits_made_while_unfocused`).
#[tokio::test]
async fn v3_requests_after_leave_are_ignored_until_next_enter() {
    let mut f = Field::disabled().await;
    f.keyboard_leave().await;
    f.s.guest([
        v3::enable(f.ti),
        v3::set_surrounding_text(f.ti, "stale", 0, 0).unwrap(),
        v3::set_text_change_cause(f.ti, 1),
        v3::set_content_type(f.ti, 3, 4),
        v3::set_cursor_rectangle(f.ti, 1, 2, 3, 4),
        v3::disable(f.ti),
        v3::commit(f.ti),
    ])
    .await;
    assert_eq!(f.host_received().await, []);
    // The next enter resets the pending state, so no peer can see it;
    // read it to pin that the leave bounds what the requests change.
    assert_eq!(
        f.state().pending_for_test(),
        &crate::ime::V3Pending::default(),
        "inactive v3 requests must not mutate double-buffered state"
    );
}

/// text-input-v3 numbers `done` by every commit on the object, so the
/// commits a client makes while unfocused count as well.
#[tokio::test]
async fn v3_done_serial_counts_commits_made_while_unfocused() {
    let mut f = Field::enabled().await;

    // Focus leaves; the client disables and commits while unfocused.
    f.keyboard_leave().await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, []);

    // Focus enters again; the client enables and commits.
    f.keyboard_enter().await;
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    f.answered(3);
    f.drain(callback).await;
    f.host_received().await;

    // The IME commits text; `done` carries the client's commit count.
    f.s.host([f.commit("가")]).await;
    assert_eq!(
        f.s.guest_received(),
        f.events([commit("가"), V3::Done { serial: 3 }])
    );
}

/// A text-input-v3 client that, like Chromium's
/// (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`), sends its
/// surrounding text and cursor rectangle only while the latest `done`
/// names its latest commit (`DoneSerialEqualsCommitCount`), and on each
/// `done` shows the preedit sent since the previous `done`, or none, as
/// the protocol says.
struct DoneCountingClient {
    /// Its `commit` requests.
    commits: u32,
    /// The serial of the latest `done`.
    done_serial: u32,
    /// The `preedit_string` since the latest `done`.
    pending_preedit: V3,
    /// The preedit it shows.
    preedit: V3,
}

impl DoneCountingClient {
    fn new() -> Self {
        Self {
            commits: 0,
            done_serial: 0,
            pending_preedit: no_preedit(),
            preedit: no_preedit(),
        }
    }

    /// Its next `commit` of text input `ti`.
    fn commit(&mut self, ti: u32) -> Outgoing<u32> {
        self.commits += 1;
        v3::commit(ti)
    }

    /// Apply `events` of its text input.
    fn receive(&mut self, events: Vec<(u32, AnyEvent)>) {
        for (_, event) in events {
            match event {
                AnyEvent::ZwpTextInputV3(event @ V3::PreeditString { .. }) => {
                    self.pending_preedit = event;
                }
                AnyEvent::ZwpTextInputV3(V3::Done { serial }) => {
                    self.done_serial = serial;
                    self.preedit = std::mem::replace(&mut self.pending_preedit, no_preedit());
                }
                _ => {}
            }
        }
    }

    /// Whether it may send its state.
    fn may_send(&self) -> bool {
        self.done_serial == self.commits
    }
}

/// Every commit that leaves the field enabled gets a `done`, as mutter
/// and KWin send, so a client that waits for one before it reports a
/// caret move or an edit, as Chromium does, reports each at once
/// rather than after the input method's next event. A commit that
/// disables the field gets none, as from mutter.
#[tokio::test]
async fn every_commit_of_an_enabled_field_gets_a_done() {
    let mut f = Field::disabled().await;
    let mut client = DoneCountingClient::new();
    f.s.guest([v3::enable(f.ti), client.commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    let events = f.s.guest_received();
    assert_eq!(events, f.events([V3::Done { serial: 1 }]));
    client.receive(events);
    assert!(client.may_send());

    // The caret moves.
    let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
    f.s.guest([rect, client.commit(f.ti)]).await;
    let rect = f.v1(V1::SetCursorRectangle {
        x: 10,
        y: 20,
        width: 1,
        height: 16,
    });
    assert_eq!(f.host_received().await, [rect, f.commit_state(2)]);
    let events = f.s.guest_received();
    assert_eq!(events, f.events([V3::Done { serial: 2 }]));
    client.receive(events);
    assert!(client.may_send());

    f.s.guest([v3::disable(f.ti), client.commit(f.ti)]).await;
    let host = f.host_received().await;
    assert_eq!(host, f.disable_requests(barrier(&host), 3));
    assert_eq!(f.s.guest_received(), []);
}

/// The `done` that answers a commit carries the preedit the guest
/// shows, with its cursor, as mutter's and KWin's do: a `done` applies
/// the preedit sent since the previous one, or none, so without it the
/// guest would drop its composition. Chromium commits after every
/// preedit it applies.
#[tokio::test]
async fn a_commit_during_composition_keeps_the_preedit() {
    let mut f = Field::disabled().await;
    let mut client = DoneCountingClient::new();
    f.s.guest([v3::enable(f.ti), client.commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    client.receive(f.s.guest_received());

    f.s.host([v1::preedit_cursor(f.host, 3), f.preedit("가나", "가나")])
        .await;
    let events = f.s.guest_received();
    let composing = || preedit_at("가나", 3, 3);
    let expected = [composing(), V3::Done { serial: 1 }];
    assert_eq!(events, f.events(expected));
    client.receive(events);
    assert_eq!(client.preedit, composing());

    let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
    f.s.guest([rect, client.commit(f.ti)]).await;
    f.host_received().await;
    let events = f.s.guest_received();
    let expected = [composing(), V3::Done { serial: 2 }];
    assert_eq!(events, f.events(expected));
    client.receive(events);
    assert!(client.may_send());
    assert_eq!(client.preedit, composing());

    // The composition goes on.
    f.s.host([f.preedit("가낙", "가낙")]).await;
    let expected = [preedit("가낙"), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A pointer press on the surface of a composition ends it before the
/// press reaches the guest, as mutter
/// (`meta_wayland_text_input_handle_event`) and KWin
/// (`InputMethodEventFilter::pointerButton`) do: the guest commits the
/// preedit it shows, and Exo, told to `reset`, drops the composition,
/// so the next key starts a new syllable instead of joining the old one.
#[tokio::test]
async fn a_press_on_the_composing_surface_commits_the_preedit() {
    let mut f = Field::enabled().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    // The reset goes behind a fence: Exo's events sent before it
    // handled the reset belong to the dropped composition.
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);
    f.drain(fence).await;

    // The composition ended: the release changes nothing, and the
    // input method's next commit replaces no preedit.
    let released = wl_pointer::BUTTON_STATE_RELEASED;
    f.s.host([Field::button(pointer, released)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_button(pointer, released)]
    );
    f.s.host([f.commit("나")]).await;
    let expected = [commit("나"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A press that ends a composition is re-sent in its place in its host
/// batch: the guest gets the commit transaction, then the press, then
/// the rest of the batch in order and unchanged.
#[tokio::test]
async fn a_press_keeps_its_place_in_its_batch() {
    let mut f = Field::enabled().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([
        Field::button(pointer, pressed),
        wl_pointer::evt::frame(pointer.host),
        wl_pointer::evt::motion(pointer.host, 510, 300, 520),
        wl_pointer::evt::frame(pointer.host),
    ])
    .await;
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    let frame = || -> (u32, AnyEvent) { (pointer.id, wl_pointer::Event::Frame {}.into()) };
    let motion = wl_pointer::Event::Motion {
        time: 510,
        surface_x: 300,
        surface_y: 520,
    };
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.extend([
        Field::guest_button(pointer, pressed),
        frame(),
        (pointer.id, motion.into()),
        frame(),
    ]);
    assert_eq!(f.s.guest_received(), expected);
    f.drain(fence).await;
}

/// The events Exo sent before it handled the press's `reset` still
/// arrive, and its commits belong to the composition the press
/// committed: the input method's own commit of the syllable, as when
/// Space ends it just before the click, or of the syllable it became
/// after the preedit the press committed. The guest gets neither, so
/// no syllable reaches it twice. Commits after the reset's fence reach
/// the guest.
#[tokio::test]
async fn a_commit_in_flight_at_a_press_is_dropped() {
    let mut f = Field::enabled().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("각", "각")]).await;
    let expected = [preedit("각"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("각"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);

    // In flight: the input method's commit of the syllable, with its
    // cursor, and of the syllable it became.
    let cursor = v1::cursor_position(f.host, 3, 3);
    f.s.host([cursor, f.commit("각"), f.commit("갂")]).await;
    assert_eq!(f.s.guest_received(), []);

    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    f.drain(fence).await;
    f.s.host([f.commit("나")]).await;
    let expected = [commit("나"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A preedit Exo sent before it handled the press's `reset` belongs to
/// the composition the reset dropped: the guest never shows it, so a
/// second press commits nothing, and the guest does not end up with
/// `가간`. Preedits after the reset's fence reach the guest.
#[tokio::test]
async fn a_preedit_in_flight_at_a_press_is_dropped() {
    let mut f = Field::enabled().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);

    // In flight: the syllable the next jamo made, with its cursor.
    f.s.host([v1::preedit_cursor(f.host, 3), f.preedit("간", "간")])
        .await;
    assert_eq!(f.s.guest_received(), []);
    f.s.host([Field::button(pointer, pressed)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_button(pointer, pressed)]
    );

    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    // The dropped preedit's cursor went with it.
    f.drain(fence).await;
    f.s.host([f.preedit("나다", "나다")]).await;
    let expected = [preedit("나다"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// Through the field "나", the guest shows the preedit "가", and a press
/// commits it behind a reset fence. Returns the pointer and the fence.
async fn press_after_na_ga(f: &mut Field) -> (Object, u32) {
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 2 }]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    (pointer, fence)
}

/// A delete Exo sent before it handled the press's `reset` was made
/// against the composition the press committed: the guest never gets
/// it. Once the fence is done and the guest has reported what the press
/// did, Exo's deletes reach it again.
#[tokio::test]
async fn a_delete_in_flight_at_a_press_is_dropped() {
    let mut f = Field::with_surrounding_text("나", 3, 3).await;
    let (_, fence) = press_after_na_ga(&mut f).await;
    f.s.host([f.delete_range(0, 3)]).await;
    assert_eq!(f.s.guest_received(), []);
    f.drain(fence).await;
    f.s.guest([
        v3::set_surrounding_text(f.ti, "나가", 6, 6).unwrap(),
        v3::commit(f.ti),
    ])
    .await;
    let expected = [f.surrounding_text("나가", 6, 6), f.commit_state(3)];
    assert_eq!(f.host_received().await, expected);
    f.answered(3);
    f.s.host([f.delete_range(3, 3)]).await;
    let expected = [delete(3, 0), V3::Done { serial: 3 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A confirmation Exo sent before it handled the press's `reset`
/// confirms the composition the press committed. With nothing composed
/// it would stand for the held Space, which the guest would then get
/// as a key the input method never gave it.
#[tokio::test]
async fn a_confirm_in_flight_at_a_press_is_dropped() {
    let mut f = Field::typing().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    f.s.host([f.peek(space), f.preedit("각", "각")]).await;
    assert_eq!(f.s.guest_received(), f.events([preedit("각"), DONE]));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("각"), DONE]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);

    f.s.host([f.confirm()]).await;
    assert_eq!(f.s.guest_received(), []);
    f.drain(fence).await;
}

/// The field shows the preedit "가", and a press commits it behind a
/// reset fence. Returns the fence.
async fn press_during_ga(f: &mut Field) -> u32 {
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    f.s.host([f.preedit("가", "가")]).await;
    assert_eq!(f.s.guest_received(), f.events([preedit("가"), DONE]));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("가"), DONE]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    fence
}

/// A focus change while a reset fence is pending deactivates behind a
/// drain barrier, which the host answers after the fence. The fence
/// then ends, and the next generation's composition reaches the guest.
#[tokio::test]
async fn a_reset_fence_ends_before_the_next_generation() {
    let mut f = Field::enabled().await;
    let fence = press_during_ga(&mut f).await;
    f.keyboard_leave().await;
    let host = f.host_received().await;
    let drain = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(drain)]);
    f.drain(fence).await;
    f.drain(drain).await;

    f.keyboard_enter().await;
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(2));
    f.answered(2);
    f.s.host([f.preedit("나", "나")]).await;
    let expected = [preedit("나"), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A text input destroyed while its reset fence is pending takes the
/// fence with it: the fence's return later changes nothing, not even
/// for a new text input that reuses the guest ID.
#[tokio::test]
async fn a_reset_fence_ends_with_its_text_input() {
    let mut f = Field::enabled().await;
    let fence = press_during_ga(&mut f).await;
    f.s.guest([v3::destroy(f.ti)]).await;
    let host = f.host_received().await;
    let drain = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(drain), f.ext(Ext::Destroy {})]);
    let delete_id = wl_display::Event::DeleteId { id: f.ti };
    assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);

    let ti = f.ti;
    f.create_text_input(ti).await;
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    f.answered(1);
    f.drain(fence).await;
    f.drain(drain).await;
    assert_eq!(f.s.guest_received(), []);
    f.s.host([f.preedit("나", "나")]).await;
    let expected = [preedit("나"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// Only a press on the surface of a composition ends it: a press with
/// no composition, a press on another surface and a release change
/// nothing, and the composition goes on.
#[tokio::test]
async fn a_press_elsewhere_or_without_a_composition_changes_nothing() {
    let mut f = Field::enabled().await;
    let pointer = f.s.pointer_on(f.seat.id, f.surface).await;
    let (pressed, released) = (
        wl_pointer::BUTTON_STATE_PRESSED,
        wl_pointer::BUTTON_STATE_RELEASED,
    );

    // No composition.
    f.s.host([Field::button(pointer, pressed)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_button(pointer, pressed)]
    );
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    // A release.
    f.s.host([Field::button(pointer, released)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_button(pointer, released)]
    );

    // A press on another surface, or after the pointer left.
    let other = f.s.surface().await;
    let other_pointer = f.s.pointer_on(f.seat.id, other).await;
    f.s.host([Field::button(other_pointer, pressed)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_button(other_pointer, pressed)]
    );
    let leave = wl_pointer::evt::leave(pointer.host, 5, f.surface.host);
    f.s.host([leave, Field::button(pointer, pressed)]).await;
    let leave = wl_pointer::Event::Leave {
        serial: 5,
        surface: f.surface.id,
    };
    assert_eq!(
        f.s.guest_received(),
        [
            (pointer.id, leave.into()),
            Field::guest_button(pointer, pressed)
        ]
    );

    // The host was told nothing, and the composition goes on.
    assert_eq!(f.host_received().await, []);
    f.s.host([f.commit("각")]).await;
    let expected = [no_preedit(), commit("각"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A touch-down on the surface of a composition ends it as a pointer
/// press does, before the guest gets the down, as mutter
/// (`meta_wayland_text_input_handle_event` on a touch-begin) and KWin
/// (`InputMethodEventFilter::touchDown`) do.
#[tokio::test]
async fn a_touch_down_on_the_composing_surface_commits_the_preedit() {
    let mut f = Field::enabled().await;
    let touch = f.s.touch(f.seat.id).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    f.s.host([Field::down(touch, f.surface, 1)]).await;
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_down(touch, f.surface, 1));
    assert_eq!(f.s.guest_received(), expected);
    f.drain(fence).await;

    // The composition ended: the input method's next commit replaces
    // no preedit.
    f.s.host([f.commit("나")]).await;
    let expected = [commit("나"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A touch-down that ends a composition is re-sent in its place in its
/// host batch: the guest gets the commit transaction, then the down,
/// then the rest of the batch in order and unchanged, a second finger's
/// down during the reset fence included.
#[tokio::test]
async fn a_touch_down_keeps_its_place_in_its_batch() {
    let mut f = Field::enabled().await;
    let touch = f.s.touch(f.seat.id).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    let host_frame = || wl_touch::evt::frame(touch.host);
    f.s.host([
        Field::down(touch, f.surface, 1),
        host_frame(),
        wl_touch::evt::motion(touch.host, 610, 1, 300, 520),
        Field::down(touch, f.surface, 2),
        host_frame(),
        wl_touch::evt::up(touch.host, 7, 620, 1),
        host_frame(),
    ])
    .await;
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    let frame = || -> (u32, AnyEvent) { (touch.id, wl_touch::Event::Frame {}.into()) };
    let motion = wl_touch::Event::Motion {
        time: 610,
        id: 1,
        x: 300,
        y: 520,
    };
    let up = wl_touch::Event::Up {
        serial: 7,
        time: 620,
        id: 1,
    };
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.extend([
        Field::guest_down(touch, f.surface, 1),
        frame(),
        (touch.id, motion.into()),
        Field::guest_down(touch, f.surface, 2),
        frame(),
        (touch.id, up.into()),
        frame(),
    ]);
    assert_eq!(f.s.guest_received(), expected);
    f.drain(fence).await;
}

/// Only a touch-down on the surface of a composition ends it: one with
/// no composition or on another surface changes nothing, and the
/// composition goes on.
#[tokio::test]
async fn a_touch_down_elsewhere_or_without_a_composition_changes_nothing() {
    let mut f = Field::enabled().await;
    let touch = f.s.touch(f.seat.id).await;

    // No composition.
    f.s.host([Field::down(touch, f.surface, 1)]).await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_down(touch, f.surface, 1)]
    );
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));

    // A second finger, on another surface.
    let other = f.s.surface().await;
    f.s.host([Field::down(touch, other, 2)]).await;
    assert_eq!(f.s.guest_received(), [Field::guest_down(touch, other, 2)]);

    // The host was told nothing, and the composition goes on.
    assert_eq!(f.host_received().await, []);
    f.s.host([f.commit("각")]).await;
    let expected = [no_preedit(), commit("각"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A second finger's touch-down while the reset fence of the first
/// finger's is pending ends nothing: the preedit Exo sent before it
/// handled the `reset` never reached the guest, so the host gets no
/// second `reset` and the guest no second commit.
#[tokio::test]
async fn a_touch_down_during_a_reset_fence_sends_no_second_reset() {
    let mut f = Field::enabled().await;
    let touch = f.s.touch(f.seat.id).await;
    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([Field::down(touch, f.surface, 1)]).await;
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_down(touch, f.surface, 1));
    assert_eq!(f.s.guest_received(), expected);

    // In flight: the syllable the next jamo made. Then a second
    // finger touches down while the first stays down.
    f.s.host([f.preedit("간", "간"), Field::down(touch, f.surface, 2)])
        .await;
    assert_eq!(
        f.s.guest_received(),
        [Field::guest_down(touch, f.surface, 2)]
    );
    assert_eq!(f.host_received().await, []);
    f.drain(fence).await;
}

/// A `leave` names the surface it leaves. One that names a surface
/// other than the pointer's, as from a host that delivers a `leave`
/// late, leaves the pointer where it is, so a press there still ends
/// the composition.
#[tokio::test]
async fn a_leave_of_another_surface_keeps_the_pointer_on_its_surface() {
    let mut f = Field::enabled().await;
    let other = f.s.surface().await;
    let pointer = f.s.pointer_on(f.seat.id, other).await;
    let leave = wl_pointer::evt::leave(pointer.host, 2, other.host);
    let enter = wl_pointer::evt::enter(pointer.host, 3, f.surface.host, 0, 0);
    let late_leave = wl_pointer::evt::leave(pointer.host, 4, other.host);
    f.s.host([leave, enter, late_leave]).await;
    let leave = |serial| wl_pointer::Event::Leave {
        serial,
        surface: other.id,
    };
    let enter = wl_pointer::Event::Enter {
        serial: 3,
        surface: f.surface.id,
        surface_x: 0,
        surface_y: 0,
    };
    assert_eq!(
        f.s.guest_received(),
        [
            (pointer.id, leave(2).into()),
            (pointer.id, enter.into()),
            (pointer.id, leave(4).into()),
        ]
    );

    f.s.host([f.preedit("가", "가")]).await;
    let expected = [preedit("가"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    let pressed = wl_pointer::BUTTON_STATE_PRESSED;
    f.s.host([Field::button(pointer, pressed)]).await;
    let mut expected = f.events([no_preedit(), commit("가"), V3::Done { serial: 1 }]);
    expected.push(Field::guest_button(pointer, pressed));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let fence = barrier(&host);
    assert_eq!(host, [f.v1(V1::Reset {}), sync(fence)]);
    f.drain(fence).await;
}

/// An input method that replaces text deletes, then commits, each a
/// transaction of its own. A client that waits for `done`, as Chromium
/// does, reports its text after the first, before it has applied the
/// second; the `done` that answers that report lets it report the text
/// after the second at once, rather than at the input method's next
/// event, so the next delete is measured against the text the guest
/// shows. The `done` of the next commit repeats no preedit once its
/// clear reached the guest.
#[tokio::test]
async fn a_client_that_waits_for_done_reports_the_text_an_edit_leaves() {
    let mut f = Field::disabled().await;
    let mut client = DoneCountingClient::new();
    f.s.guest([
        v3::enable(f.ti),
        v3::set_surrounding_text(f.ti, "see you tom", 11, 11).unwrap(),
        client.commit(f.ti),
    ])
    .await;
    f.host_received().await;
    client.receive(f.s.guest_received());
    assert!(client.may_send());
    let report = |f: &Field, text: &str| {
        let end = text.len() as i32;
        v3::set_surrounding_text(f.ti, text, end, end).unwrap()
    };
    let done = |serial| V3::Done { serial };

    // The user accepts the suggestion "tomorrow".
    f.s.host([f.delete_range(8, 3), f.commit("tomorrow")]).await;
    let mut events = f.s.guest_received();
    let expected = [delete(3, 0), done(1), commit("tomorrow"), done(1)];
    assert_eq!(events, f.events(expected));
    let commit_transaction = events.split_off(2);
    client.receive(events);
    assert!(client.may_send());
    f.s.guest([report(&f, "see you "), client.commit(f.ti)])
        .await;
    let expected = [f.surrounding_text("see you ", 8, 8), f.commit_state(2)];
    assert_eq!(f.host_received().await, expected);
    client.receive(commit_transaction);
    assert!(!client.may_send());
    let events = f.s.guest_received();
    assert_eq!(events, f.events([done(2)]));
    client.receive(events);
    assert!(client.may_send());
    f.s.guest([report(&f, "see you tomorrow"), client.commit(f.ti)])
        .await;
    let expected = [
        f.surrounding_text("see you tomorrow", 16, 16),
        f.commit_state(3),
    ];
    assert_eq!(f.host_received().await, expected);
    f.answered(3);

    // The input method composes, and the caret moves.
    f.s.host([f.preedit("x", "x")]).await;
    client.receive(f.s.guest_received());
    let rect = v3::set_cursor_rectangle(f.ti, 10, 20, 1, 16);
    f.s.guest([rect, client.commit(f.ti)]).await;
    f.host_received().await;
    let events = f.s.guest_received();
    assert_eq!(events, f.events([preedit("x"), done(4)]));
    client.receive(events);

    // The input method clears its composition and takes the suggestion
    // back, which is measured in Exo's text.
    f.s.host([f.preedit("", ""), f.delete_range(8, 8)]).await;
    let events = f.s.guest_received();
    let expected = [no_preedit(), done(4), delete(8, 0), done(4)];
    assert_eq!(events, f.events(expected));
    client.receive(events);
    assert!(client.may_send());
    f.s.guest([report(&f, "see you "), client.commit(f.ti)])
        .await;
    f.host_received().await;
    f.answered(5);
}

/// The `done` of the guest's first commit.
const DONE: V3 = V3::Done { serial: 1 };

fn delete(before_length: u32, after_length: u32) -> V3 {
    V3::DeleteSurroundingText {
        before_length,
        after_length,
    }
}

/// The host input method's events reach the guest as text-input-v3
/// transactions, each closed by a `done` that numbers the guest's
/// commits, whatever serial the host sent.
#[tokio::test]
async fn host_ime_events_become_v3_transactions() {
    type Events = fn(&Field) -> Vec<Outgoing<u32>>;
    let korean: Events = |f| {
        vec![
            f.preedit("ㄱ", "ㄱ"),
            f.preedit("가", "가"),
            f.preedit("간", "간"),
            f.commit("가"),
            f.preedit("나", "나"),
            f.preedit("낟", "낟"),
            f.commit("나"),
            f.preedit("다", "다"),
        ]
    };
    let cases: Vec<(&str, Events, Vec<V3>)> = vec![
        (
            "a preedit",
            |f| vec![v1::preedit_string(f.host, 42, "こんにちは", "").unwrap()],
            vec![preedit("こんにちは"), DONE],
        ),
        (
            // GTK emits `commit` for an empty commit_string too
            // (`text_input_commit_apply`, `gtk/gtkimcontextwayland.c`
            // 281-282, GTK 92348cd4), and GTK4's `GtkText` and
            // `GtkTextView` delete the selection before they insert it
            // (`gtk_text_enter_text`, `gtk/gtktext.c` 4592-4593;
            // `gtk_text_view_commit_text`, `gtk/gtktextview.c` 8779).
            "an empty commit sends no commit_string",
            |f| vec![f.commit(""), f.preedit("미완성", ""), f.commit("")],
            vec![DONE, preedit("미완성"), DONE, no_preedit(), DONE],
        ),
        (
            "a commit clears the preedit",
            |f| vec![f.preedit("미완성", ""), f.commit("確定")],
            vec![preedit("미완성"), DONE, no_preedit(), commit("確定"), DONE],
        ),
        (
            "Korean composition commits each syllable once",
            korean,
            vec![
                preedit("ㄱ"),
                DONE,
                preedit("가"),
                DONE,
                preedit("간"),
                DONE,
                no_preedit(),
                commit("가"),
                DONE,
                preedit("나"),
                DONE,
                preedit("낟"),
                DONE,
                no_preedit(),
                commit("나"),
                DONE,
                preedit("다"),
                DONE,
            ],
        ),
        (
            // Its preedit_cursor and preedit_styling are byte offsets
            // into it, which replacing an invalid sequence would move.
            "a preedit that is not UTF-8 is ignored",
            |f| {
                let latin1 = respell(f.preedit("Cafe", ""), "Cafe", b"Caf\xe9");
                vec![f.preedit("가", ""), latin1, f.preedit("가나", "")]
            },
            vec![preedit("가"), DONE, preedit("가나"), DONE],
        ),
        (
            "an ignored preedit takes its cursor and styling with it",
            |f| {
                let latin1 = respell(f.preedit("Cafe", ""), "Cafe", b"Caf\xe9");
                vec![
                    v1::preedit_styling(f.host, 0, 3, 6),
                    v1::preedit_cursor(f.host, 3),
                    latin1,
                    f.preedit("가나", ""),
                ]
            },
            vec![preedit("가나"), DONE],
        ),
        (
            "a preedit region without surrounding text takes the cursor and styling with it",
            |f| {
                vec![
                    v1::preedit_styling(f.host, 0, 3, 6),
                    v1::preedit_cursor(f.host, 3),
                    ext::set_preedit_region(f.host_ext, -3, 3),
                    f.preedit("가나", ""),
                ]
            },
            vec![preedit("가나"), DONE],
        ),
        (
            // Nothing the proxy uses indexes committed text.
            "a commit that is not UTF-8 commits replacement characters",
            |f| vec![respell(f.commit("Cafe"), "Cafe", b"Caf\xe9")],
            vec![commit("Caf\u{fffd}"), DONE],
        ),
        (
            // Each 0xff becomes the three bytes of U+FFFD, too long for
            // the guest's message. The cursor position it takes with it
            // reaches no guest event here;
            // `oversized_host_text_does_not_partially_advance_state`
            // shows it in the next edit.
            "a commit too long once replaced is dropped",
            |f| {
                let text = "x".repeat(4_000);
                let latin1 = respell(f.commit(&text), &text, &[0xff; 4_000]);
                vec![v1::cursor_position(f.host, 1, 1), latin1, f.commit("나")]
            },
            vec![commit("나"), DONE],
        ),
        (
            "a preedit cursor is a byte offset",
            |f| vec![v1::preedit_cursor(f.host, 3), f.preedit("가나", "")],
            vec![preedit_at("가나", 3, 3), DONE],
        ),
        (
            "a selection is a byte range, which the cursor does not move",
            |f| {
                vec![
                    v1::preedit_styling(f.host, 0, 3, 6),
                    v1::preedit_cursor(f.host, 6),
                    f.preedit("가나", ""),
                ]
            },
            vec![preedit_at("가나", 0, 3), DONE],
        ),
        (
            "a cursor inside a character falls back to the end",
            |f| vec![v1::preedit_cursor(f.host, 1), f.preedit("가", "")],
            vec![preedit("가"), DONE],
        ),
        (
            "a selection inside a character falls back to the end",
            |f| vec![v1::preedit_styling(f.host, 1, 1, 6), f.preedit("가", "")],
            vec![preedit("가"), DONE],
        ),
        (
            "a confirm without a preedit or a held key",
            |f| vec![ext::confirm_preedit(f.host_ext, 1)],
            vec![],
        ),
        (
            "a confirm of an emptied preedit without a held key",
            |f| {
                vec![
                    f.preedit("가", ""),
                    f.preedit("", ""),
                    ext::confirm_preedit(f.host_ext, 0),
                ]
            },
            vec![preedit("가"), DONE, no_preedit(), DONE],
        ),
        (
            // Its selection and cursor go with it.
            "a confirm commits the preedit",
            |f| {
                vec![
                    f.preedit("나", ""),
                    v1::preedit_styling(f.host, 0, 3, 6),
                    v1::preedit_cursor(f.host, 3),
                    ext::confirm_preedit(f.host_ext, 0),
                    f.preedit("다라", ""),
                    f.commit("다"),
                    f.commit("라"),
                ]
            },
            vec![
                preedit("나"),
                DONE,
                no_preedit(),
                commit("나"),
                DONE,
                preedit_at("다라", 6, 6),
                DONE,
                no_preedit(),
                commit("다"),
                DONE,
                commit("라"),
                DONE,
            ],
        ),
    ];
    for (case, events, expected) in cases {
        let mut f = Field::enabled().await;
        let events = events(&f);
        f.s.host(events).await;
        assert_eq!(f.s.guest_received(), f.events(expected), "{case}");
    }
}

/// Exo's `delete_surrounding_text(index, length)` deletes the bytes
/// `index..index + length` of the surrounding text, counted from its
/// start, and no `commit_string` closes it. The guest gets it at once,
/// in bytes before and after its cursor, measured in Exo's text, edits
/// of the input method the guest has yet to report included. A delete
/// is dropped where v3 cannot carry it to every client (a range the
/// cursor is not in, a selection) and where Exo could not have sent it
/// (a range its text does not have, a negative index).
#[tokio::test]
async fn exo_deletes_become_v3_deletes_around_the_cursor() {
    type Events = fn(&Field) -> Vec<Outgoing<u32>>;
    /// The guest's surrounding text, cursor and anchor, if it has any.
    type Surrounding = Option<(&'static str, i32, i32)>;
    /// The `done` after the guest's second commit, which stated the
    /// surrounding text.
    const DONE_2: V3 = V3::Done { serial: 2 };
    let cases: Vec<(&str, Surrounding, Events, Vec<V3>)> = vec![
        (
            // The input method replaces "tom" with the suggestion.
            "an accepted suggestion deletes, then commits",
            Some(("see you tom", 11, 11)),
            |f| vec![f.delete_range(8, 3), f.commit("tomorrow")],
            vec![delete(3, 0), DONE_2, commit("tomorrow"), DONE_2],
        ),
        (
            // A long press on "e" offers its diacritics.
            "a chosen diacritic deletes, then commits",
            Some(("e", 1, 1)),
            |f| vec![f.delete_range(0, 1), f.commit("é")],
            vec![delete(1, 0), DONE_2, commit("é"), DONE_2],
        ),
        (
            // "하세요" is three syllables of three bytes each.
            "a delete alone counts UTF-8 bytes",
            Some(("안녕하세요", 15, 15)),
            |f| vec![f.delete_range(6, 9)],
            vec![delete(9, 0), DONE_2],
        ),
        (
            "a delete around the cursor",
            Some(("가나다", 3, 3)),
            |f| vec![f.delete_range(0, 6)],
            vec![delete(3, 3), DONE_2],
        ),
        (
            // Exo deletes the selection "cd" and one byte either side,
            // which v3 clients count from different places.
            "a delete with a selection is dropped",
            Some(("abcdef", 4, 2)),
            |f| vec![f.delete_range(1, 4), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            "a delete with a selection whose cursor is its start is dropped",
            Some(("abcdef", 2, 4)),
            |f| vec![f.delete_range(1, 4), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            "a range apart from the cursor is dropped, not kept",
            Some(("가나다", 9, 9)),
            |f| vec![f.delete_range(0, 3), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            "a range past the text is dropped",
            Some(("가", 3, 3)),
            |f| vec![f.delete_range(0, 6), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            "a range inside a character is dropped",
            Some(("가나", 6, 6)),
            |f| vec![f.delete_range(1, 5), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            "a delete without surrounding text is dropped",
            None,
            |f| vec![f.delete_range(0, 3), f.commit("x")],
            vec![commit("x"), DONE],
        ),
        (
            // Only the v1 XML's reading, relative to the cursor, could
            // give one a meaning; Exo's index is absolute.
            "a negative index is dropped",
            Some(("abc", 3, 3)),
            |f| vec![f.delete_range(-1, 1), f.commit("x")],
            vec![commit("x"), DONE_2],
        ),
        (
            // Exo sends one when the input method deletes nothing.
            "an empty delete is not forwarded, and edits nothing",
            Some(("abc", 3, 3)),
            |f| vec![f.delete_range(3, 0), f.delete_range(2, 1)],
            vec![delete(1, 0), DONE_2],
        ),
        (
            // It sends no commit_string either (`host_ime_events_become_v3_transactions`).
            "an empty commit without a preedit edits nothing",
            Some(("abc", 3, 3)),
            |f| vec![f.commit(""), f.delete_range(2, 1)],
            vec![DONE_2, delete(1, 0), DONE_2],
        ),
        (
            // The guest gets only the preedit's clear, as from an empty
            // preedit, so it has no edit to report.
            "an empty commit over a preedit edits nothing",
            Some(("abc", 3, 3)),
            |f| vec![f.preedit("x", "x"), f.commit(""), f.delete_range(2, 1)],
            vec![
                preedit("x"),
                DONE_2,
                no_preedit(),
                DONE_2,
                delete(1, 0),
                DONE_2,
            ],
        ),
        (
            // It commits the preedit, an edit, as a confirmation does
            // (`ime::tests::an_empty_region_over_a_held_preedit_commits_it`), so
            // "c" of Exo's "abcx|" is apart from the cursor.
            "an empty preedit region over a preedit commits it",
            Some(("abc", 3, 3)),
            |f| {
                vec![
                    f.preedit("x", "x"),
                    ext::set_preedit_region(f.host_ext, 0, 0),
                    f.delete_range(2, 1),
                ]
            },
            vec![preedit("x"), DONE_2, no_preedit(), commit("x"), DONE_2],
        ),
        (
            // The guest gets only a `done`, so it has no edit to report.
            "an empty preedit region edits nothing",
            Some(("abc", 3, 3)),
            |f| {
                vec![
                    ext::set_preedit_region(f.host_ext, 0, 0),
                    f.delete_range(2, 1),
                ]
            },
            vec![DONE_2, delete(1, 0), DONE_2],
        ),
        (
            // Exo deletes "cX" of its "abcX|def", which the guest has.
            "a delete after a commit the guest has not reported",
            Some(("abcdef", 3, 3)),
            |f| vec![f.commit("X"), f.delete_range(2, 2)],
            vec![commit("X"), DONE_2, delete(2, 0), DONE_2],
        ),
        (
            // Exo deletes "bd" of its "ab|def".
            "a delete after a delete the guest has not reported",
            Some(("abcdef", 3, 3)),
            |f| vec![f.delete_range(2, 1), f.delete_range(1, 2)],
            vec![delete(1, 0), DONE_2, delete(1, 1), DONE_2],
        ),
        (
            // Exo deletes "b" of its "ab|cd".
            "a delete after a confirmation the guest has not reported",
            Some(("acd", 1, 1)),
            |f| vec![f.preedit("b", "b"), f.confirm(), f.delete_range(1, 1)],
            vec![
                preedit("b"),
                DONE_2,
                no_preedit(),
                commit("b"),
                DONE_2,
                delete(1, 0),
                DONE_2,
            ],
        ),
        (
            // The region "나" becomes a preedit and is cleared, so Exo
            // deletes "다" of its "가|다".
            "a delete after a preedit region the guest has not reported",
            Some(("가나다", 6, 6)),
            |f| {
                vec![
                    ext::set_preedit_region(f.host_ext, -3, 3),
                    f.preedit("", ""),
                    f.delete_range(3, 3),
                ]
            },
            vec![
                delete(3, 0),
                preedit_at("나", 3, 3),
                DONE_2,
                no_preedit(),
                DONE_2,
                delete(0, 3),
                DONE_2,
            ],
        ),
    ];
    for (case, surrounding_text, events, expected) in cases {
        let mut f = match surrounding_text {
            Some((text, cursor, anchor)) => {
                Field::with_surrounding_text(text, cursor, anchor).await
            }
            None => Field::enabled().await,
        };
        let events = events(&f);
        f.s.host(events).await;
        assert_eq!(f.s.guest_received(), f.events(expected), "{case}");
    }
}

/// Each surrounding text reaches the host with a `wl_display.sync`
/// right after it, whose `done` places the report among Exo's events
/// for the IME core's Exo mirror; the guest never sees it. The mirror
/// measures Exo's deletes in Exo's text.
#[tokio::test]
async fn a_report_reaches_the_host_with_its_barrier() {
    let mut f = Field::enabled().await;
    let report = v3::set_surrounding_text(f.ti, "ab", 2, 2).unwrap();
    f.s.guest([report, v3::commit(f.ti)]).await;
    let host = f.s.host_received();
    let Some((
        report,
        (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })),
    )) = host.first().zip(host.get(1))
    else {
        panic!("no report and barrier in {host:?}");
    };
    let callback = *callback;
    assert_eq!(*report, f.surrounding_text("ab", 2, 2));
    f.answered(2);
    let delete_id = wl_display::evt::delete_id(WL_DISPLAY_ID, callback);
    f.s.host([wl_callback::evt::done(callback, 0), delete_id])
        .await;
    assert_eq!(f.s.guest_received(), []);
    // Exo deletes "b" of its "ab|".
    f.s.host([f.delete_range(1, 1)]).await;
    let expected = [delete(1, 0), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A key the guest gets may edit its text where Exo does not see, so
/// Exo's deletes are dropped until the guest states its text: a commit
/// with no surrounding text states it is unchanged.
#[tokio::test]
async fn a_forwarded_key_holds_back_exos_delete_until_the_guest_commits() {
    let mut f = Field::with_surrounding_text("abc", 3, 3).await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 50,
        time: 1_000,
    };
    f.s.host([f.key(space)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(space)]);
    f.host_received().await;
    f.s.guest([v3::commit(f.ti)]).await;
    f.host_received().await;
    f.answered(3);
    f.s.host([f.delete_range(2, 1)]).await;
    let expected = [delete(1, 0), V3::Done { serial: 3 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    // Another key, and the delete after it is dropped.
    let a = Stroke {
        key: 30,
        serial: 51,
        ..space
    };
    f.s.host([f.key(a)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(a)]);
    f.host_received().await;
    f.s.host([f.delete_range(1, 1)]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// #85: a region over the shown region deletes while the guest shows a
/// preedit, which Chromium drops (C3), so the guest is out of step with
/// Exo; the next, growing region is dropped, and the input method's
/// next commit doubles the text until the guest's report puts it in
/// step again.
#[tokio::test]
async fn a_region_over_a_region_drops() {
    let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
    let done = || V3::Done { serial: 2 };
    let region = |f: &Field, index, length| ext::set_preedit_region(f.host_ext, index, length);
    f.s.host([region(&f, -1, 1)]).await;
    let expected = [delete(1, 0), preedit_at("d", 1, 1), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([region(&f, -2, 2)]).await;
    let expected = [no_preedit(), delete(1, 0), preedit_at("cd", 2, 2), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([region(&f, -3, 3)]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// Mirror mode measures a region over a host preedit with Exo's cursor
/// inside it from that cursor, the preedit at the guest's.
#[tokio::test]
async fn a_region_over_a_host_preedit_with_exos_cursor_inside_it() {
    let mut f = Field::with_surrounding_text("a1", 2, 2).await;
    let done = || V3::Done { serial: 2 };
    f.s.host([v1::preedit_cursor(f.host, 3), f.preedit("가나", "가나")])
        .await;
    let expected = [preedit_at("가나", 3, 3), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
    // Exo's "a1가|나": "1가나".
    f.s.host([ext::set_preedit_region(f.host_ext, -4, 7)]).await;
    let expected = [
        no_preedit(),
        delete(1, 0),
        preedit_at("1가나", 4, 4),
        done(),
    ];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// At v11 Exo confirms before each press its input method does not
/// handle, so a field that never reports, or a held key, makes many
/// history states, which the mirror keeps as fingerprints: it stays
/// exact, and measures Exo's delete.
#[tokio::test]
async fn many_empty_confirmations_keep_the_mirror_exact() {
    let mut f = Field::with_surrounding_text("abc", 3, 3).await;
    for _ in 0..65 {
        f.s.host([f.confirm()]).await;
    }
    f.answer_recovery_syncs().await;
    f.s.guest_received();
    f.s.host([f.delete_range(2, 1)]).await;
    let expected = [delete(1, 0), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A modifier the guest gets edits no text, so it does not hold back
/// Exo's next delete: one after a commit the guest has yet to report,
/// which the mirror measures in Exo's text.
#[tokio::test]
async fn a_modifier_key_does_not_hold_back_exos_delete() {
    let mut f = Field::typing().await;
    f.s.host([f.commit("X")]).await;
    let expected = [commit("X"), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
    let shift = Stroke {
        key: 42,
        state: PRESSED,
        serial: 60,
        time: 1_000,
    };
    f.s.host([f.peek(shift), f.key(shift)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(shift)]);
    assert_eq!(f.host_received().await, [f.ack(60, true)]);
    // Exo's "가X|".
    f.s.host([f.delete_range(0, 4)]).await;
    let expected = [delete(4, 0), V3::Done { serial: 1 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// An empty preedit reaches the guest only to clear the one it shows.
/// Over none, the transaction is its `done` alone, since clients tell
/// an empty preedit from none:
/// - GTK emits `preedit-start` for an empty preedit over none
///   (`text_input_preedit_apply`, `gtk/gtkimcontextwayland.c` 241-244,
///   GTK 92348cd4), on which GTK4's `GtkText` deletes the selection
///   (`gtk_text_preedit_start_cb`, `gtk/gtktext.c` 4491), as does
///   `GtkTextView`.
/// - Chromium applies one that differs from the preedit it last
///   applied, as one with a hidden cursor does
///   (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc` 466,
///   revision ffb4f13519), and an empty composition over none deletes
///   the selection (`UpdateCompositionIfTextSelected`,
///   `ui/base/ime/linux/input_method_auralinux.cc` 325-328).
///
/// Either way the user's selected text would go.
#[tokio::test]
async fn an_empty_preedit_reaches_only_a_guest_that_shows_one() {
    let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
    let done = || V3::Done { serial: 2 };
    f.s.host([
        f.preedit("", ""),
        f.preedit("", "stale fallback"),
        v1::preedit_cursor(f.host, -1),
        f.preedit("", ""),
    ])
    .await;
    assert_eq!(f.s.guest_received(), f.events([done(), done(), done()]));

    f.s.host([f.preedit("가", ""), f.preedit("", "")]).await;
    let expected = [preedit("가"), done(), no_preedit(), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// An empty preedit, which clears the one the guest shows, always has
/// the cursor `(0, 0)`, even after the host hid the cursor. Chromium
/// keeps the last preedit it applied, cursor included, and applies a
/// `done`'s preedit only if it differs, an absent one counting as
/// `("", 0, 0)` (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`
/// 466, `zwp_text_input_v3.h` 139-140, revision ffb4f13519). After
/// `("", -1, -1)`, the next `done` without a preedit would apply an
/// empty composition again, which deletes the selection
/// (`UpdateCompositionIfTextSelected`,
/// `ui/base/ime/linux/input_method_auralinux.cc` 325-328).
#[tokio::test]
async fn an_empty_preedit_has_no_hidden_cursor() {
    let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
    let done = || V3::Done { serial: 2 };
    let expected = f.events([preedit("가"), done(), no_preedit(), done()]);
    f.s.host([
        f.preedit("가", "가"),
        v1::preedit_cursor(f.host, -1),
        f.preedit("", ""),
    ])
    .await;
    assert_eq!(f.s.guest_received(), expected);
}

/// An empty preedit region deletes nothing, so it sends no
/// `delete_surrounding_text`, only the empty preedit that clears a
/// shown one ([`an_empty_preedit_reaches_only_a_guest_that_shows_one`],
/// [`an_empty_preedit_has_no_hidden_cursor`]); one over a preedit Exo
/// holds commits it
/// (`ime::tests::an_empty_region_over_a_held_preedit_commits_it`). Chromium
/// applies an empty delete too (`ZwpTextInputV3Impl::OnDone`,
/// `ui/ozone/platform/wayland/host/zwp_text_input_v3.cc` 440, revision
/// ffb4f13519), and while nothing is composed it deletes the selection
/// (`InputMethodAuraLinux::OnDeleteSurroundingText`,
/// `ui/base/ime/linux/input_method_auralinux.cc` 518): the user's
/// selected text would go. With a selection, which no guest in step
/// with Exo has, the region is dropped altogether.
#[tokio::test]
async fn an_empty_preedit_region_sends_no_delete() {
    let mut f = Field::with_surrounding_text("abcdef", 4, 4).await;
    f.s.host([ext::set_preedit_region(f.host_ext, 0, 0)]).await;
    let done = || V3::Done { serial: 2 };
    assert_eq!(f.s.guest_received(), f.events([done()]));
    let mut f = Field::with_surrounding_text("abcdef", 4, 2).await;
    f.s.host([ext::set_preedit_region(f.host_ext, 0, 0)]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// A preedit region makes text around the cursor the preedit; a region
/// apart from the cursor cannot be.
#[tokio::test]
async fn on_set_preedit_region_translates_correctly() {
    // A region apart from the cursor takes the pending cursor with it.
    let mut f = Field::with_surrounding_text("가나다", 6, 6).await;
    f.s.host([
        v1::preedit_cursor(f.host, 0),
        ext::set_preedit_region(f.host_ext, -6, 3),
    ])
    .await;
    assert_eq!(f.s.guest_received(), []);

    let mut f = Field::with_surrounding_text("가나다", 6, 6).await;
    f.s.host([ext::set_preedit_region(f.host_ext, -3, 3)]).await;
    let done = || V3::Done { serial: 2 };
    let expected = [delete(3, 0), preedit_at("나", 3, 3), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
    // The region is the preedit now.
    f.s.host([f.commit("나")]).await;
    let expected = [no_preedit(), commit("나"), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));

    // A pending preedit cursor, or selection, places the region's.
    f.s.host([
        v1::preedit_cursor(f.host, 0),
        ext::set_preedit_region(f.host_ext, -3, 3),
    ])
    .await;
    let expected = [delete(3, 0), preedit_at("나", 0, 0), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));
    // A region over the shown preedit clears it before its delete:
    // SDL3 applies each event as it arrives and turns the delete into
    // a Backspace (`text_input_delete_surrounding_text`,
    // `src/video/wayland/SDL_waylandevents.c` 2683-2710, main
    // 1fc62260fc81), which its `testime` ignores while a preedit shows
    // (`test/testime.c` 1084-1086). The guest's text is "가|다" now,
    // so of the region "가나" it deletes only "가".
    f.s.host([
        v1::preedit_styling(f.host, 0, 3, 6),
        ext::set_preedit_region(f.host_ext, -6, 6),
    ])
    .await;
    let expected = [no_preedit(), delete(3, 0), preedit_at("가나", 0, 3), done()];
    assert_eq!(f.s.guest_received(), f.events(expected));

    // The `done` of the guest's next commit repeats the region's
    // preedit ([`answer_guest_commit`]).
    f.s.guest([v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, [f.commit_state(3)]);
    let expected = [preedit_at("가나", 0, 3), V3::Done { serial: 3 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// H10: Exo sends no keysym for a physical key, and no keysym release
/// at all, so a keysym reaches the guest as nothing, rather than as a
/// press no release would end. It leaves no state behind either: the
/// key's own press, which follows, reaches the guest.
#[tokio::test]
async fn a_keysym_reaches_no_peer() {
    use xkbcommon::xkb::keysyms::KEY_space;
    let mut f = Field::typing().await;
    let press = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    let release = Stroke {
        state: RELEASED,
        serial: 701,
        time: 1_300,
        ..press
    };
    f.s.host([f.keysym(KEY_space, press), f.keysym(KEY_space, release)])
        .await;
    assert_eq!(f.s.guest_received(), []);

    let press = Stroke {
        serial: 702,
        time: 1_400,
        ..press
    };
    f.s.host([f.peek(press), f.key(press)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(press)]);
    assert_eq!(f.host_received().await, [f.ack(702, true)]);
}

/// The key an empty confirmation recovers reaches the guest as a
/// press and a release that both carry the serial and time of the
/// held key's latest `peek_key`: the serial Exo gave the key event its
/// input method consumed.
#[tokio::test]
async fn a_recovered_key_carries_the_latest_peek_serial() {
    let mut f = Field::typing().await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
        .await;
    let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));

    for _ in 0..2 {
        f.s.host([f.confirm()]).await;
        f.answer_recovery_syncs().await;
        assert_eq!(f.s.guest_received(), f.recovered(space));
    }
}

/// At extension v11 Exo confirms before every press its input method
/// does not handle (E13), and sends the press's peek and key after the
/// confirmation (E8), the modifiers it carries first. While Backspace,
/// which the input method took, is held, that confirmation is the new
/// press's, not a repeat of Backspace: the guest gets the new key and
/// no Backspace (DST seed 456, shrunk). The press ends the Backspace
/// lease.
#[tokio::test]
async fn a_confirmation_before_an_unhandled_press_recovers_no_held_key() {
    for shifted in [false, true] {
        let mut f = Field::typing().await;
        f.lease_backspace_repeat().await;
        let space = Stroke {
            key: SPACE,
            state: PRESSED,
            serial: 701,
            time: 1_300,
        };
        let mut events = vec![f.confirm()];
        let mut expected = Vec::new();
        if shifted {
            let shift = wl_keyboard::evt::modifiers(f.keyboard.host, 701, 1, 0, 0, 0);
            events.push(shift);
            let shift = wl_keyboard::Event::Modifiers {
                serial: 701,
                mods_depressed: 1,
                mods_latched: 0,
                mods_locked: 0,
                group: 0,
            };
            expected.push((f.keyboard.id, shift.into()));
        }
        events.extend([f.peek(space), f.key(space)]);
        f.s.host(events).await;
        expected.push(f.guest_key(space));
        assert_eq!(f.s.guest_received(), expected, "shifted: {shifted}");
        let host = f.host_received().await;
        let callback = barrier(&host);
        assert_eq!(host, [sync(callback), f.ack(701, true)]);
        f.s.host([wl_callback::evt::done(callback, 0)]).await;
        assert_eq!(f.s.guest_received(), [], "shifted: {shifted}");
        assert_eq!(f.repeat_owner(BACKSPACE), None);
    }
}

/// An empty confirmation while Backspace, which the input method took,
/// is held, then the peek of a press the input method handles and
/// that press's text: the confirmation was Backspace's autorepeat. The
/// guest gets Backspace before the new press's text.
#[tokio::test]
async fn a_confirmation_before_a_handled_press_recovers_the_held_key() {
    let mut f = Field::typing().await;
    let backspace = f.lease_backspace_repeat().await;
    let r = Stroke {
        key: 19,
        state: PRESSED,
        serial: 701,
        time: 1_300,
    };
    f.s.host([f.confirm(), f.peek(r), f.preedit("ㄱ", "")])
        .await;
    let mut expected = f.recovered(backspace);
    expected.extend(f.events([preedit("ㄱ"), DONE]));
    assert_eq!(f.s.guest_received(), expected);
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), []);
}

/// A host event the guest gets that decides a waiting recovery reaches
/// the guest after what the recovery sends, the held key's pair and the
/// transaction that closes the confirmation: the press of a key no peek
/// announced, and the modifiers after the peek of a new press.
#[tokio::test]
async fn a_recovery_reaches_the_guest_before_the_event_that_decides_it() {
    let mut f = Field::typing().await;
    let backspace = f.lease_backspace_repeat().await;
    let a = Stroke {
        key: 30,
        state: PRESSED,
        serial: 701,
        time: 1_300,
    };
    f.s.host([f.confirm(), f.key(a)]).await;
    let mut expected = f.recovered(backspace);
    expected.push(f.guest_key(a));
    assert_eq!(f.s.guest_received(), expected);
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [sync(callback), f.ack(701, true)]);
    f.s.host([wl_callback::evt::done(callback, 0)]).await;
    assert_eq!(f.s.guest_received(), []);

    let mut f = Field::typing().await;
    let backspace = f.lease_backspace_repeat().await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 702,
        time: 1_400,
    };
    let shift = wl_keyboard::evt::modifiers(f.keyboard.host, 702, 1, 0, 0, 0);
    f.s.host([f.confirm(), f.peek(space), shift]).await;
    let mut expected = f.recovered(backspace);
    let shift = wl_keyboard::Event::Modifiers {
        serial: 702,
        mods_depressed: 1,
        mods_latched: 0,
        mods_locked: 0,
        group: 0,
    };
    expected.push((f.keyboard.id, shift.into()));
    assert_eq!(f.s.guest_received(), expected);
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), []);
}

/// Exo confirms as it detaches the input method on its own (E6), as
/// when the input method's process restarts, then sends `leave` (H2).
/// That confirmation is the detach's, not a repeat of the held Space,
/// which the input method took. The key events in flight die with the
/// input method, so a held key's release may never come: Space is not
/// recovered again, but its next press is.
#[tokio::test]
async fn an_input_method_detach_ends_the_recovery_of_held_keys() {
    let mut f = Field::typing().await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
        .await;
    let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([f.confirm(), v1::leave(f.host)]).await;
    assert_eq!(f.s.guest_received(), []);
    let callback = barrier(&f.host_received().await);
    let answer = wl_callback::evt::done(callback, 0);
    f.s.host([answer, v1::enter(f.host, f.surface.host)]).await;
    assert_eq!(f.s.guest_received(), []);

    f.s.host([f.confirm()]).await;
    assert_eq!(f.host_received().await, [], "no recovery waits");
    assert_eq!(f.s.guest_received(), []);

    let release = Stroke {
        state: RELEASED,
        serial: 701,
        ..space
    };
    let again = Stroke {
        serial: 702,
        time: 1_400,
        ..space
    };
    f.s.host([f.peek(release), f.peek(again), f.confirm()])
        .await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(again));
}

/// A key held across Exo's detach (H2) may stay pressed in Exo's
/// keyboard, as its release died with the input method, and
/// `Keyboard::OnKeyEvent` handles a press only of a key it does not
/// hold: the key's next press gets no peek and no `wl_keyboard.key`,
/// only the input method's empty confirmation before it (E13). While
/// such a key is held, an empty confirmation is that press's as likely
/// as an autorepeat of the eligible held key, here Backspace, so it
/// recovers nothing, as a guessed Backspace would delete text (DST
/// seeds 13104 and 16710, shrunk). Once the old key's release comes,
/// the next confirmation recovers Backspace again.
#[tokio::test]
async fn a_confirmation_while_a_key_held_across_a_detach_recovers_nothing() {
    let mut f = Field::typing().await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
        .await;
    f.s.guest_received();
    f.s.host([f.confirm(), v1::leave(f.host)]).await;
    let callback = barrier(&f.host_received().await);
    let answer = wl_callback::evt::done(callback, 0);
    f.s.host([answer, v1::enter(f.host, f.surface.host)]).await;
    assert_eq!(f.s.guest_received(), []);

    let backspace = Stroke {
        key: BACKSPACE,
        serial: 701,
        time: 1_400,
        ..space
    };
    f.s.host([f.peek(backspace), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), [], "Space's press, unseen");

    let release = Stroke {
        state: RELEASED,
        serial: 702,
        time: 1_450,
        ..space
    };
    f.s.host([f.peek(release), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(backspace));
}

/// Exo's own detach (H2) ends only the recovery of the keys held
/// across it, which stay the guest's keys: the `wl_keyboard.key` of
/// `1` reaches the guest. Their releases died with the input method, so a
/// `peek_key` press of a key still held is its next press, which an
/// empty confirmation recovers.
#[tokio::test]
async fn a_key_held_across_a_detach_is_typed_and_recovered_again() {
    const KEY_1: u32 = 2;
    let mut f = Field::typing().await;
    let one = Stroke {
        key: KEY_1,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    let space = Stroke {
        key: SPACE,
        serial: 701,
        time: 1_240,
        ..one
    };
    f.s.host([f.peek(one), f.peek(space), f.confirm(), v1::leave(f.host)])
        .await;
    let callback = barrier(&f.host_received().await);
    let answer = wl_callback::evt::done(callback, 0);
    f.s.host([answer, v1::enter(f.host, f.surface.host)]).await;
    assert_eq!(f.s.guest_received(), []);

    let one_again = Stroke {
        serial: 702,
        time: 1_400,
        ..one
    };
    f.s.host([f.peek(one_again), f.key(one_again)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(one_again)]);
    assert_eq!(f.host_received().await, [f.ack(702, true)]);

    let space_again = Stroke {
        serial: 703,
        time: 1_500,
        ..space
    };
    f.s.host([f.peek(space_again), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(space_again));
}

/// A recovery that waits goes only to a text input that still passes
/// the gate: once the guest disabled or destroyed it, or the key's
/// keyboard, the host's next event recovers nothing.
#[tokio::test]
async fn a_waiting_recovery_ends_with_its_text_input_or_keyboard() {
    #[derive(Clone, Copy, Debug)]
    enum End {
        Disable,
        Destroy,
        ReleaseKeyboard,
    }
    for end in [End::Disable, End::Destroy, End::ReleaseKeyboard] {
        let mut f = Field::typing().await;
        f.lease_backspace_repeat().await;
        f.s.host([f.confirm()]).await;
        let callback = barrier(&f.host_received().await);
        let request = match end {
            End::Disable => v3::disable(f.ti),
            End::Destroy => v3::destroy(f.ti),
            End::ReleaseKeyboard => wl_keyboard::req::release(f.keyboard.id),
        };
        f.s.guest([request]).await;
        if !matches!(end, End::Destroy) {
            f.s.guest([v3::commit(f.ti)]).await;
        }
        f.host_received().await;
        f.s.guest_received();
        f.s.host([wl_callback::evt::done(callback, 0)]).await;
        assert_eq!(f.s.guest_received(), [], "{end:?}");
    }
}

// Traces of real IME and key bugs, step by step.

/// Exo can give a key's peek release the serial of its press. The
/// release still ends the press, so that a next press with that
/// serial starts a generation an empty confirmation recovers.
#[tokio::test]
async fn a_peek_release_with_the_press_serial_ends_the_press() {
    let mut f = Field::typing().await;
    let press = Stroke {
        key: BACKSPACE,
        state: PRESSED,
        serial: 18_857,
        time: 100,
    };
    let release = Stroke {
        state: RELEASED,
        time: 110,
        ..press
    };
    f.s.host([f.peek(press), f.peek(release)]).await;
    assert!(!f.held(BACKSPACE));

    let again = Stroke { time: 120, ..press };
    f.s.host([f.peek(again), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(again));
    assert_eq!(f.repeat_owner(BACKSPACE), Some(f.ti));
}

/// A sibling text input's rejected enable, or its destroy, whether it
/// ever committed or not, ends no repeat lease of the enabled one,
/// whose empty confirmations go on recovering the held Backspace.
#[tokio::test]
async fn a_sibling_text_input_keeps_the_active_repeat_lease() {
    let mut f = Field::typing().await;
    let backspace = f.lease_backspace_repeat().await;
    f.s.host([f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(backspace));

    // A sibling whose enable is rejected, then one that never commits.
    let field = f.text_input();
    for enables in [true, false] {
        let id = f.s.new_id();
        // Creating a text input makes the fixture drive it.
        f.create_text_input(id).await;
        let sibling = f.text_input();
        if enables {
            f.s.guest([v3::enable(sibling.ti), v3::commit(sibling.ti)])
                .await;
            assert_eq!(f.host_received().await, f.reset(1, Reset::default()));
        }
        f.s.guest([v3::destroy(sibling.ti)]).await;
        assert_eq!(f.host_received().await, [f.ext(Ext::Destroy {})]);
        let deleted = wl_display::Event::DeleteId { id: sibling.ti };
        assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
    }
    f.select(field);

    f.s.host([f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(backspace));
}

/// A text input's destroy ends its repeat lease, which a new text
/// input under its ID does not inherit; the next physical press can
/// lease the new one.
#[tokio::test]
async fn a_text_input_recreated_under_its_id_does_not_inherit_the_repeat_lease() {
    let mut f = Field::typing().await;
    f.lease_backspace_repeat().await;
    f.s.guest([v3::destroy(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(
        host,
        [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
    );
    let deleted = wl_display::Event::DeleteId { id: f.ti };
    assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
    assert_eq!(f.repeat_owner(BACKSPACE), None);

    let old_host = f.host;
    f.create_text_input(f.ti).await;
    assert_ne!(f.host, old_host);
    assert_eq!(f.repeat_owner(BACKSPACE), None);
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    f.answered(1);

    let release = Stroke {
        key: BACKSPACE,
        state: RELEASED,
        serial: 701,
        time: 1_300,
    };
    let press = Stroke {
        state: PRESSED,
        serial: 702,
        time: 1_400,
        ..release
    };
    f.s.host([
        f.peek(release),
        f.peek(press),
        f.preedit("나", ""),
        f.preedit("", ""),
    ])
    .await;
    let expected = [preedit("나"), DONE, no_preedit(), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    assert_eq!(f.repeat_owner(BACKSPACE), Some(f.ti));
}

/// Held Space after a Korean commit, then held Backspace over an
/// emptied preedit, each held twice. Every empty confirmation while
/// the key is held stands for one press and release, a delayed
/// `wl_keyboard.key` duplicates none, and no confirmation after the
/// release recovers it.
#[tokio::test]
async fn held_keys_repeat_through_confirmations_after_korean_input() {
    let mut f = Field::typing().await;

    // The input method commits the syllable with Space.
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_234,
    };
    f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
        .await;
    let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));

    f.s.host([f.confirm(), f.confirm(), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    let mut expected = f.recovered(space);
    expected.extend(f.recovered(space));
    expected.extend(f.recovered(space));
    assert_eq!(f.s.guest_received(), expected);

    let release = Stroke {
        state: RELEASED,
        serial: 701,
        time: 1_300,
        ..space
    };
    f.s.host([f.peek(release), f.confirm()]).await;
    assert_eq!(f.s.guest_received(), []);

    // A second hold starts a new generation, which repeats the same:
    // nothing of the first leaks across its release.
    let space = Stroke {
        serial: 702,
        time: 1_400,
        ..space
    };
    f.s.host([f.peek(space), f.preedit("나", ""), f.commit("나 ")])
        .await;
    let expected = [preedit("나"), DONE, no_preedit(), commit("나 "), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([f.confirm(), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    let mut expected = f.recovered(space);
    expected.extend(f.recovered(space));
    assert_eq!(f.s.guest_received(), expected);
    let release = Stroke {
        state: RELEASED,
        serial: 703,
        time: 1_500,
        ..space
    };
    f.s.host([f.peek(release)]).await;
    assert!(!f.held(SPACE));
    // No peer sees the peek a later confirmation could recover.
    let keyboard = HostId::for_test(f.keyboard.host);
    assert_eq!(f.s.ctx().keys.peek(keyboard, SPACE), None);

    // Backspace held over a preedit the input method empties.
    let backspace = Stroke {
        key: BACKSPACE,
        state: PRESSED,
        serial: 800,
        time: 1_600,
    };
    f.s.host([f.peek(backspace), f.preedit("가", ""), f.preedit("", "")])
        .await;
    let expected = [preedit("가"), DONE, no_preedit(), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([f.confirm(), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    let mut expected = f.recovered(backspace);
    expected.extend(f.recovered(backspace));
    assert_eq!(f.s.guest_received(), expected);
    f.s.host([f.key(backspace)]).await;
    assert_eq!(f.s.guest_received(), []);
    assert_eq!(f.host_received().await, [f.ack(800, false)]);

    // Its release reaches the proxy on both channels.
    let release = Stroke {
        state: RELEASED,
        serial: 801,
        time: 1_700,
        ..backspace
    };
    f.s.host([f.peek(release), f.key(release), f.confirm()])
        .await;
    assert_eq!(f.s.guest_received(), []);
    assert_eq!(f.host_received().await, [f.ack(801, false)]);

    let backspace = Stroke {
        serial: 810,
        time: 1_800,
        ..backspace
    };
    f.s.host([f.peek(backspace), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(backspace));
}

/// The keys a `wl_keyboard.enter` reports held are numbered in the
/// host's order, each at its first listing, and a peek of such a key
/// takes its number. So when the host lists S, then A, then S again,
/// and peeks S, then A, an empty confirmation stands for A, the key
/// numbered last and peeked last: the order follows the host, not the
/// keycodes. The guest holds A since the enter, so it gets no key
/// pair, only the `done` that closes the transaction. Numbered by
/// keycode, A would come first, the latest peek would not be the
/// newest key, and the confirmation would stand for nothing.
#[tokio::test]
async fn keys_held_at_an_enter_are_numbered_in_the_hosts_order() {
    const KEY_S: u32 = 31;
    const KEY_A: u32 = 30;
    let mut f = Field::offered(Host {
        keyboard_extension_with_keymap: true,
    })
    .await;
    f.keyboard_leave().await;
    let keys: Vec<u8> = [KEY_S, KEY_A, KEY_S]
        .iter()
        .flat_map(|key| key.to_ne_bytes())
        .collect();
    let enter = wl_keyboard::evt::enter(f.keyboard.host, 3, f.surface.host, &keys);
    f.s.host([enter.unwrap()]).await;
    let (keyboard, surface) = (f.keyboard.id, f.surface.id);
    let modifiers = wl_keyboard::Event::Modifiers {
        serial: 3,
        mods_depressed: 0,
        mods_latched: 0,
        mods_locked: 0,
        group: 0,
    };
    let mut expected: Vec<(u32, AnyEvent)> = vec![
        (
            keyboard,
            wl_keyboard::Event::Enter {
                serial: 3,
                surface,
                keys,
            }
            .into(),
        ),
        (keyboard, modifiers.into()),
    ];
    expected.extend(f.events([V3::Enter { surface }]));
    assert_eq!(f.s.guest_received(), expected);
    assert_eq!(f.host_received().await, []);
    f.start_typing().await;

    let s = Stroke {
        key: KEY_S,
        state: PRESSED,
        serial: 800,
        time: 2_000,
    };
    let a = Stroke {
        key: KEY_A,
        serial: 801,
        time: 2_001,
        ..s
    };
    f.s.host([f.peek(s), f.peek(a), f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.events([DONE]));
}

/// A text input destroyed while its deactivation drains sends nothing
/// more: its host's late events reach no one, and the proxy's drain
/// callback ends without reaching the guest, its host ID free for
/// another object only after the host's `delete_id`.
#[tokio::test]
async fn a_text_input_destroyed_while_draining_ends_its_barrier_unseen() {
    let mut f = Field::enabled().await;
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, f.disable_requests(callback, 2));
    f.s.guest([v3::destroy(f.ti)]).await;
    assert_eq!(f.host_received().await, [f.ext(Ext::Destroy {})]);
    let deleted = wl_display::Event::DeleteId { id: f.ti };
    assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

    f.s.host([f.preedit("stale", ""), f.commit("stale")]).await;
    assert_eq!(f.s.guest_received(), []);

    f.drain(callback).await;
    // No peer sees whether the proxy still waits on its own callback.
    assert_eq!(
        f.s.ctx().internal_callbacks.get(HostId::for_test(callback)),
        None
    );
    let ti = f.s.new_id();
    f.create_text_input(ti).await;
    assert!(![f.host, f.host_ext].contains(&callback));
    f.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, callback)])
        .await;
    assert_eq!(f.s.guest_received(), []);
    let ti = f.s.new_id();
    f.create_text_input(ti).await;
    assert_eq!(f.host, callback);
}

/// A focus change drains the old generation: the input method's
/// composition, commit and confirmation sent while it drains reach no
/// one, while the guest's enable on the new surface waits for the
/// drain and then composes.
#[tokio::test]
async fn a_focus_change_drops_the_old_composition_and_held_key() {
    let mut f = Field::typing().await;
    let space = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: 700,
        time: 1_000,
    };
    f.s.host([f.peek(space), f.preedit("가", ""), f.commit("가 ")])
        .await;
    let expected = [preedit("가"), DONE, no_preedit(), commit("가 "), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    f.s.host([f.confirm()]).await;
    f.answer_recovery_syncs().await;
    assert_eq!(f.s.guest_received(), f.recovered(space));

    f.keyboard_leave().await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(callback)]);
    f.keyboard_enter_new_surface().await;
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(
        f.host_received().await,
        [
            f.surrounding_text("", 0, 0),
            f.no_cursor_rectangle(),
            f.commit_state(2)
        ]
    );
    f.answered(2);

    f.s.host([
        f.preedit("오래된 조합", ""),
        f.commit("오래된 확정"),
        f.confirm(),
    ])
    .await;
    assert_eq!(f.s.guest_received(), []);

    f.drain(callback).await;
    // No peer sees whether the proxy still waits on its own callback.
    assert_eq!(
        f.s.ctx().internal_callbacks.get(HostId::for_test(callback)),
        None
    );
    let mut expected = vec![f.activate()];
    expected.extend(f.content_type((0, 0), false));
    expected.push(f.commit_state(2));
    assert_eq!(f.host_received().await, expected);
    f.s.host([f.preedit("새 조합", "")]).await;
    let expected = [preedit("새 조합"), V3::Done { serial: 2 }];
    assert_eq!(f.s.guest_received(), f.events(expected));
}

/// A text input recreated under the ID of one whose deactivation
/// drains is activated at once, and the old drain neither reconciles
/// nor reaches it, nor moves it off its own host object.
#[tokio::test]
async fn a_text_input_recreated_under_its_id_ignores_the_old_barrier() {
    let mut f = Field::enabled().await;
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, f.disable_requests(callback, 2));
    f.s.guest([v3::destroy(f.ti)]).await;
    assert_eq!(f.host_received().await, [f.ext(Ext::Destroy {})]);
    let deleted = wl_display::Event::DeleteId { id: f.ti };
    assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

    let old_host = f.host;
    f.create_text_input(f.ti).await;
    assert_ne!(f.host, old_host);
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    f.answered(1);

    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
    assert_eq!(f.s.guest_received(), []);
    f.s.host([f.preedit("새 객체", "")]).await;
    let expected = [preedit("새 객체"), DONE];
    assert_eq!(f.s.guest_received(), f.events(expected));
    // The replacement keeps its own host text input, which its disable
    // deactivates.
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    assert_eq!(host, f.disable_requests(barrier(&host), 2));
}

/// An enable or a disable the guest has not committed when the focus
/// leaves dies with that focus: its commit after the next enter
/// activates nothing.
#[tokio::test]
async fn an_uncommitted_enable_or_disable_dies_with_its_focus() {
    let mut f = Field::enabled().await;
    f.s.guest([v3::disable(f.ti)]).await;
    assert_eq!(f.host_received().await, []);
    f.keyboard_leave().await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    assert_eq!(host, [f.deactivate(), sync(callback)]);
    f.keyboard_enter_new_surface().await;
    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
    // No peer sees whether the proxy still waits on its own callback.
    assert_eq!(
        f.s.ctx().internal_callbacks.get(HostId::for_test(callback)),
        None
    );
    f.s.guest([v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, [f.commit_state(2)]);

    f.s.guest([v3::enable(f.ti)]).await;
    f.keyboard_leave().await;
    assert_eq!(f.host_received().await, []);
    f.keyboard_enter_new_surface().await;
    // No peer sees that the enter dropped the pending enable before
    // the commit below shows it.
    assert_eq!(f.state().pending_for_test().enabled, None);
    f.s.guest([v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, [f.commit_state(3)]);
}

/// A release delayed past a wrapped serial's next press ends only its
/// own generation: the new press stays held and owned, and gets its
/// own release, and every key gets one `ack_key`. Then a key
/// is held as the focus leaves, which drains the field: a confirmation
/// recovers nothing.
#[tokio::test]
async fn a_delayed_release_ends_only_its_generation() {
    let mut f = Field::typing().await;
    let old_press = Stroke {
        key: SPACE,
        state: PRESSED,
        serial: u32::MAX - 2,
        time: 100,
    };
    let old_release = Stroke {
        state: RELEASED,
        serial: u32::MAX,
        time: 110,
        ..old_press
    };
    let new_press = Stroke {
        serial: 1,
        time: 200,
        ..old_press
    };
    f.s.host([f.peek(old_press), f.key(old_press)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(old_press)]);
    assert_eq!(f.host_received().await, [f.ack(old_press.serial, true)]);
    f.s.host([f.peek(old_release), f.peek(new_press), f.key(new_press)])
        .await;
    assert_eq!(f.s.guest_received(), [f.guest_key(new_press)]);
    assert_eq!(f.host_received().await, [f.ack(new_press.serial, true)]);

    f.s.host([f.key(old_release)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(old_release)]);
    assert_eq!(f.host_received().await, [f.ack(old_release.serial, true)]);
    assert!(f.held(SPACE));
    assert_eq!(f.owner(SPACE), Some(GuestKeyOwner::Physical));

    let new_release = Stroke {
        state: RELEASED,
        serial: 3,
        time: 220,
        ..old_press
    };
    f.s.host([f.peek(new_release), f.key(new_release)]).await;
    assert_eq!(f.s.guest_received(), [f.guest_key(new_release)]);
    assert_eq!(f.host_received().await, [f.ack(new_release.serial, true)]);
    assert!(!f.held(SPACE));
    assert_eq!(f.owner(SPACE), None);

    let held = Stroke {
        serial: 4,
        time: 300,
        ..old_press
    };
    f.s.host([f.peek(held)]).await;
    f.keyboard_leave().await;
    let host = f.host_received().await;
    assert_eq!(host, [f.deactivate(), sync(barrier(&host))]);
    f.s.host([f.confirm()]).await;
    assert_eq!(f.s.guest_received(), []);
}

/// Each keyboard of a seat gets one `enter` and one `leave` for each
/// focus it is given, whatever order the host's events take, and the
/// seat's focus, which its text input follows, is the focus of the
/// keyboard that entered last, until a keyboard leaves it.
#[tokio::test]
async fn each_keyboard_focus_is_entered_and_left_once() {
    // Keyboard `a` has entered the surface with serial 1.
    let mut f = Field::disabled().await;
    let (a, surface) = (f.keyboard, f.surface);
    let id = f.s.new_id();
    let host = f.s.create(wl_seat::req::get_keyboard(f.seat.id, id)).await;
    let b = Object { id, host };
    let replacement = f.s.surface().await;
    let enter = |keyboard: Object, serial, surface: Object| {
        wl_keyboard::evt::enter(keyboard.host, serial, surface.host, &[]).unwrap()
    };
    let leave = |keyboard: Object, serial, surface: Object| {
        wl_keyboard::evt::leave(keyboard.host, serial, surface.host)
    };
    let entered = |keyboard: Object, serial, surface: Object| {
        let enter = wl_keyboard::Event::Enter {
            serial,
            surface: surface.id,
            keys: Vec::new(),
        };
        let modifiers = wl_keyboard::Event::Modifiers {
            serial,
            mods_depressed: 0,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        [(keyboard.id, enter.into()), (keyboard.id, modifiers.into())]
    };
    let left = |keyboard: Object, serial, surface: Object| {
        let leave = wl_keyboard::Event::Leave {
            serial,
            surface: surface.id,
        };
        (keyboard.id, leave.into())
    };

    // A second keyboard needs its own enter; a duplicate is dropped.
    f.s.host([enter(b, 2, surface), enter(b, 3, surface)]).await;
    assert_eq!(f.s.guest_received(), entered(b, 2, surface));
    // The first leave moves the seat's focus off the surface, which
    // the host leaves on every keyboard; `b`'s own leave balances its
    // enter.
    f.s.host([leave(a, 4, surface), leave(a, 5, surface)]).await;
    let mut expected = vec![left(a, 4, surface)];
    expected.extend(f.events([V3::Leave {
        surface: surface.id,
    }]));
    assert_eq!(f.s.guest_received(), expected);
    f.s.host([leave(b, 6, surface)]).await;
    assert_eq!(f.s.guest_received(), [left(b, 6, surface)]);

    // `b` takes the seat's focus elsewhere before `a` leaves: that
    // leave still balances `a`'s enter, once, and moves no focus.
    f.s.host([enter(a, 7, surface)]).await;
    let mut expected = Vec::from(entered(a, 7, surface));
    expected.extend(f.events([V3::Enter {
        surface: surface.id,
    }]));
    assert_eq!(f.s.guest_received(), expected);
    f.s.host([enter(b, 8, replacement)]).await;
    let mut expected = Vec::from(entered(b, 8, replacement));
    expected.extend(f.events([
        V3::Leave {
            surface: surface.id,
        },
        V3::Enter {
            surface: replacement.id,
        },
    ]));
    assert_eq!(f.s.guest_received(), expected);
    f.s.host([leave(a, 9, surface), leave(a, 10, surface)])
        .await;
    assert_eq!(f.s.guest_received(), [left(a, 9, surface)]);
}

const BACKSPACE: u32 = crate::keys::EVDEV_KEY_BACKSPACE;
const PRESSED: u32 = wl_keyboard::KEY_STATE_PRESSED;
const RELEASED: u32 = wl_keyboard::KEY_STATE_RELEASED;

/// Guest keyboard `keyboard`'s `key` event of `key` in `state`.
fn key(keyboard: u32, key: u32, state: u32, serial: u32, time: u32) -> (u32, AnyEvent) {
    let event = wl_keyboard::Event::Key {
        serial,
        time,
        key,
        state,
    };
    (keyboard, event.into())
}

/// A report and its barrier are one entry of the host queue, which a
/// submission never splits (`connection::OutgoingMessages`); the host
/// gets them as one stream either way.
#[tokio::test]
async fn a_reports_barrier_is_glued_into_its_entry() {
    let mut f = Field::enabled().await;
    // Dispatches the guest's report and commit as their requests would,
    // and reads state no peer can observe: how the host queue holds them
    // before the proxy sends them.
    let ti = TiId(GuestId::for_test(f.ti));
    let ctx = f.s.ctx();
    super::ime(
        ctx,
        Input::SurroundingText(ti, IndexedString::from("ab"), 2, 2),
    );
    super::ime(ctx, Input::Commit(ti));
    let queue: Vec<_> = ctx.take_effects().to_host.collect();
    let report = queue.iter().find(|message| {
        crate::test_support::msg_header(message.bytes()).opcode
            == zwp_text_input_v1::REQ_SET_SURROUNDING_TEXT
    });
    let report = report.expect("a report");
    let messages = crate::test_support::glued_messages(report.bytes());
    let [_, sync] = messages[..] else {
        panic!("{} messages in the report's entry", messages.len());
    };
    let header = crate::test_support::msg_header(sync);
    assert_eq!(
        (header.sender_id, header.opcode),
        (WL_DISPLAY_ID, wl_display::REQ_SYNC)
    );
}

/// Host text too long for a message is dropped whole: the guest gets
/// nothing, and the text input and its repeat lease stay as they were,
/// except that the event takes the pieces Exo sent before it, as every
/// edit does (B3). Otherwise the next one would apply them. Only host
/// text the proxy lengthened by replacing invalid UTF-8 fits the host's
/// message but not the guest's (`host_ime_events_become_v3_transactions`
/// sends such a commit); a confirmation commits a shown preedit, which
/// no host can make too long, so the test shows it one.
#[tokio::test]
async fn oversized_host_text_does_not_partially_advance_state() {
    // Each 0xff becomes the three bytes of U+FFFD.
    let latin1 = "x".repeat(4_000);
    type Event = fn(&Field, &str) -> Outgoing<u32>;
    let cases: [(&str, Event); 3] = [
        ("preedit", |f, text| {
            respell(f.preedit(text, ""), text, &[0xff; 4_000])
        }),
        ("commit", |f, text| {
            respell(f.commit(text), text, &[0xff; 4_000])
        }),
        ("confirm", |f, _| f.confirm()),
    ];
    for (case, event) in cases {
        let mut f = Field::typing().await;
        let press = f.lease_backspace_repeat().await;
        if case == "confirm" {
            // Injects a state no peer can cause: a shown preedit too long
            // to commit.
            let ti = TiId(GuestId::for_test(f.ti));
            let state = f.s.ctx().ime.get_mut(ti).expect("the text input");
            state.show_for_test(&"x".repeat(4_084));
        }
        // Exo sends the pieces of an edit before it.
        let selection = zwp_text_input_v1::PREEDIT_STYLE_SELECTION;
        f.s.host([
            v1::preedit_cursor(f.host, 1),
            v1::preedit_styling(f.host, 0, 1, selection),
            v1::cursor_position(f.host, 1, 1),
            event(&f, &latin1),
        ])
        .await;
        assert_eq!(f.s.guest_received(), [], "{case}");
        assert_eq!(f.host_received().await, [], "{case}");
        // The repeat stays leased: its tombstone takes Backspace's press.
        f.s.host([f.key(press)]).await;
        assert_eq!(
            f.host_received().await,
            [f.ack(press.serial, false)],
            "{case}"
        );
        assert_eq!(f.s.guest_received(), [], "{case}");
        // The dropped event took the pieces, which the next edit lacks.
        f.s.host([f.preedit("나", "")]).await;
        let expected = f.events([preedit("나"), DONE]);
        assert_eq!(f.s.guest_received(), expected, "{case}");
    }
}

#[test]
fn child_objects_inherit_their_manager_versions() {
    // Built by hand: the proxy binds no version above the generated
    // one, so no session can tell a child's inherited version from it.
    let mut ctx = TestConnection::default().context();
    // Both text-input protocols are v1, so artificial manager versions
    // show where each child's version comes from.
    let manager = ctx
        .shadow_table
        .create_host_only(Iface::ZwpTextInputManagerV1, 4);
    let extension = ctx
        .shadow_table
        .create_host_only(Iface::ZcrTextInputExtensionV1, 12);
    ctx.globals
        .bind_for_test(Singleton::TextInputManager, manager);
    ctx.globals
        .bind_for_test(Singleton::TextInputExtension, extension);
    ctx.shadow_table
        .create_local(GuestId::for_test(40), Iface::ZwpTextInputManagerV3, 5);
    let version = wl_seat::VERSION;
    ctx.shadow_table
        .create_paired(GuestId::for_test(30), Iface::WlSeat, version, version);

    let get_text_input = zwp_text_input_manager_v3::req::get_text_input(40, 20, 30);
    let (bytes, _) = get_text_input.into_parts();
    let header = crate::test_support::msg_header(&bytes);
    let mut fds = std::collections::VecDeque::new();
    let mut msg = crate::wire::WireMessage::new(
        header.sender_id,
        header.opcode,
        &bytes[crate::wire::MessageHeader::LEN..],
    )
    .with_fds(&mut fds);
    let forward = crate::test_support::dispatch_request(&mut ctx, &mut msg);
    assert!(matches!(forward, Ok(None)), "the request is consumed");
    let ti = TiId(GuestId::for_test(20));
    let host = ctx.ime.get(ti).expect("the text input").host();
    assert_eq!(
        ctx.shadow_table.host_object_version(host.ext.id),
        Some(12),
        "version guards must use the manager's negotiated host version"
    );
    assert_eq!(ctx.shadow_table.host_object_version(host.v1), Some(4));
    assert_eq!(ctx.shadow_table.guest_object_version(ti.0), Some(5));
}

/// A drain barrier completes only the generation it drains: the guest
/// destroyed the text input and reused its ID for one with another
/// host text input, enabled and not yet active, as it waits for a
/// drain of its own, which the old barrier must not end.
#[tokio::test]
async fn stale_activation_callback_cannot_reconcile_reused_guest_id() {
    let mut f = Field::enabled().await;
    f.s.guest([v3::destroy(f.ti)]).await;
    let host = f.host_received().await;
    let callback = barrier(&host);
    // The deactivate, its drain barrier, then the extension's destroy.
    assert_eq!(
        host,
        [f.deactivate(), sync(callback), f.ext(Ext::Destroy {})]
    );
    let deleted = wl_display::Event::DeleteId { id: f.ti };
    assert_eq!(f.s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

    // It activates, deactivates, and is enabled again while it drains.
    f.create_text_input(f.ti).await;
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(1));
    f.answered(1);
    f.s.guest([v3::disable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    let own = barrier(&host);
    assert_eq!(host, f.disable_requests(own, 2));
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    let host = f.host_received().await;
    assert!(
        !host.contains(&f.activate()),
        "not while it drains: {host:?}"
    );
    f.answered(3);

    f.drain(callback).await;
    assert_eq!(f.host_received().await, []);
    assert_eq!(f.s.guest_received(), []);
    // Its own barrier activates it.
    f.drain(own).await;
    let host = f.host_received().await;
    assert_eq!(host.first(), Some(&f.activate()), "{host:?}");
}

/// The guest's commit count wraps, and the host gets it as it is.
#[tokio::test]
async fn guest_commit_serial_wraps_and_is_forwarded_unchanged() {
    let mut f = Field::disabled().await;
    // Injects a state no peer can reach in a test's time: 2^32 - 1
    // commits.
    let ti = TiId(GuestId::for_test(f.ti));
    let state = f.s.ctx().ime.get_mut(ti).expect("the text input");
    state.set_commits_for_test(u32::MAX);
    f.s.guest([v3::enable(f.ti), v3::commit(f.ti)]).await;
    assert_eq!(f.host_received().await, f.enable_requests(0));
    f.answered(0);
}

/// Without a host v1 manager the text input has no input method,
/// like one without the extension.
#[tokio::test]
async fn text_input_creation_without_host_manager_does_not_create_broken_state() {
    let mut s = Session::new();
    let (seat, _) = s.keyboard().await;
    let version = zwp_text_input_manager_v1::VERSION;
    let manager = s.bind("zwp_text_input_manager_v1", version).await;
    let version = zcr_text_input_extension_v1::VERSION;
    s.advertise(zcr_text_input_extension_v1::NAME, version)
        .await;
    // The proxy binds it for itself.
    s.host_received();
    // Injects a fault no peer can cause: the proxy has no host manager
    // for the guest's, which it advertises only with one.
    s.ctx().globals.unbind_for_test(Singleton::TextInputManager);
    let ti = s.new_id();
    s.guest([
        zwp_text_input_manager_v3::req::get_text_input(manager, ti, seat),
        v3::enable(ti),
        v3::commit(ti),
        v3::destroy(ti),
    ])
    .await;
    assert_eq!(s.host_received(), []);
    let delete_id = wl_display::Event::DeleteId { id: ti };
    assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
}
