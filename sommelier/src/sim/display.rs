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

//! The rest of the host compositor, around the Exo model.
//!
//! The Exo model ([`super::exo`]) plays the seat's keyboard and the text
//! inputs, and leaves the display to its caller. [`Display`] plays it: it
//! advertises the globals, answers each `wl_display.sync` with
//! `wl_callback.done` and `delete_id`, once the model has kept a snapshot
//! of its trackers for it ([`Exo::snapshot_trackers`]), and each destructor
//! with its
//! `delete_id`. It hands every request to the model and takes the model's
//! events after each one, before it answers a later sync, so that the
//! sync's `done` follows every event of earlier requests (E11). It also
//! plays the seat's pointer and touch, whose presses name their point in a
//! field by where they land ([`keymap::position`]), and gives each keyboard
//! the default keymap, as Exo does before anything else, so that the proxy
//! knows which keys repeat.

use std::collections::{BTreeMap, VecDeque};

use super::editor::Target;
use super::exo::Exo;
use super::keymap;
use super::surfaces::Surfaces;
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::text_input_extension_unstable_v1::{
    zcr_extended_text_input_v1, zcr_text_input_extension_v1,
};
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::wayland::{
    wl_callback, wl_compositor, wl_display, wl_keyboard, wl_pointer, wl_registry, wl_seat,
    wl_surface, wl_touch,
};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::test_support::decode;
use crate::wire::{Outgoing, WL_DISPLAY_ID};

/// `BTN_LEFT`, the primary button.
const BTN_LEFT: u32 = 0x110;

/// The host's display, globals, pointer and touch.
pub(super) struct Display {
    /// The globals it advertises, by name from 1: interface and version.
    globals: Vec<(&'static str, u32)>,
    /// The interface of each host object, by ID, to decode what it sends.
    objects: BTreeMap<u32, String>,
    /// Which field each surface is, shared with the world.
    surfaces: Surfaces,
    /// The fields whose windows open again, in order, each with whether
    /// it had the seat's focus, which the new surface then takes (F2).
    reopening: VecDeque<(usize, bool)>,
    /// The new surfaces that take the seat's focus once mapped.
    mapping: Vec<u32>,
    /// The surface the pointer is on.
    pointer_on: Option<u32>,
    /// The last serial an input event of the pointer or touch took. Exo
    /// numbers its own events; the two never meet.
    serial: u32,
}

impl Display {
    /// A display whose surfaces `surfaces` names.
    pub fn new(surfaces: Surfaces) -> Self {
        let globals = vec![
            (wl_compositor::NAME, wl_compositor::VERSION),
            (wl_seat::NAME, wl_seat::VERSION),
            (
                zwp_text_input_manager_v1::NAME,
                zwp_text_input_manager_v1::VERSION,
            ),
            (
                zcr_keyboard_extension_v1::NAME,
                zcr_keyboard_extension_v1::VERSION,
            ),
            // Exo advertises v14, which the proxy binds at its newest.
            (
                zcr_text_input_extension_v1::NAME,
                zcr_text_input_extension_v1::VERSION,
            ),
        ];
        Self {
            globals,
            objects: BTreeMap::from([(WL_DISPLAY_ID, wl_display::NAME.to_owned())]),
            surfaces,
            reopening: VecDeque::new(),
            mapping: Vec::new(),
            pointer_on: None,
            serial: 1 << 20,
        }
    }

    /// The window of `field` opens again (F2): the next surface the proxy
    /// creates is its; if it had the seat's focus, `refocus`, the new one
    /// takes it once mapped, as a window manager gives it to a new window.
    ///
    /// Exo maps a window at the commit that gives a surface with a shell
    /// role its first buffer; the display has neither roles nor buffers, so
    /// the new surface's first commit stands for that one.
    pub fn reopen(&mut self, field: usize, refocus: bool) {
        self.reopening.push_back((field, refocus));
    }

    /// The proxy sends `request` from host object `sender`: `exo` applies
    /// it, and the events that follow are Exo's, then the display's answer.
    pub fn request(&mut self, sender: u32, request: &AnyRequest, exo: &mut Exo) -> Vec<Outgoing> {
        if let Some((id, interface)) = request.new_object() {
            self.objects.insert(id, interface.to_owned());
        }
        exo.request(sender, request);
        match *request {
            // A destroyed surface loses the seat's and the pointer's
            // focus, with no leave; Exo deactivates its text inputs.
            AnyRequest::WlSurface(wl_surface::Request::Destroy {}) => {
                exo.forget(|id| id != sender);
                if self.pointer_on == Some(sender) {
                    self.pointer_on = None;
                }
            }
            AnyRequest::WlCompositor(wl_compositor::Request::CreateSurface { id }) => {
                match self.reopening.pop_front() {
                    Some((field, refocus)) => {
                        self.surfaces.add_host(Some(field), id);
                        if refocus {
                            self.mapping.push(id);
                        }
                    }
                    None => self.surfaces.add_host(None, id),
                }
            }
            AnyRequest::WlSurface(wl_surface::Request::Commit {}) => {
                if let Some(i) = self.mapping.iter().position(|&id| id == sender) {
                    self.mapping.remove(i);
                    exo.move_focus(Some(sender));
                }
            }
            _ => {}
        }
        let mut events = exo.take_events();
        if let AnyRequest::WlSeat(wl_seat::Request::GetKeyboard { id }) = *request {
            events.insert(0, keymap(id));
        }
        match *request {
            AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) => {
                exo.snapshot_trackers(callback);
                events.push(wl_callback::evt::done(callback, 0));
                events.push(self.delete(callback));
            }
            AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry }) => {
                for (name, &(interface, version)) in (1..).zip(&self.globals) {
                    let global = wl_registry::evt::global(registry, name, interface, version);
                    events.push(global.expect("a global fits a message"));
                }
            }
            _ if destructor(request) => events.push(self.delete(sender)),
            _ => {}
        }
        events
    }

    /// The pointer presses and releases the primary button at `at` on
    /// `surface`, entering it first if it is elsewhere.
    pub fn click(&mut self, surface: u32, at: Target, time: u32) -> Vec<Outgoing> {
        let pointer = self.object(wl_pointer::NAME);
        let x = keymap::position(at);
        let mut events = Vec::new();
        if self.pointer_on != Some(surface) {
            if let Some(left) = self.pointer_on.replace(surface) {
                events.push(wl_pointer::evt::leave(pointer, self.next_serial(), left));
            }
            events.push(wl_pointer::evt::enter(
                pointer,
                self.next_serial(),
                surface,
                x,
                0,
            ));
        } else {
            events.push(wl_pointer::evt::motion(pointer, time, x, 0));
        }
        for state in [
            wl_pointer::BUTTON_STATE_PRESSED,
            wl_pointer::BUTTON_STATE_RELEASED,
        ] {
            let serial = self.next_serial();
            events.push(wl_pointer::evt::button(
                pointer, serial, time, BTN_LEFT, state,
            ));
        }
        events
    }

    /// A finger touches `surface` at `at` and lifts.
    pub fn tap(&mut self, surface: u32, at: Target, time: u32) -> Vec<Outgoing> {
        let touch = self.object(wl_touch::NAME);
        let x = keymap::position(at);
        let down = wl_touch::evt::down(touch, self.next_serial(), time, surface, 0, x, 0);
        let up = wl_touch::evt::up(touch, self.next_serial(), time, 0);
        let frame = || wl_touch::evt::frame(touch);
        vec![down, frame(), up, frame()]
    }

    /// `event`, which the host sends, with its sender, decoded.
    ///
    /// # Panics
    ///
    /// If it does not decode as sent by an object the host has.
    pub fn decode(&self, (message, fds): &Outgoing) -> (u32, AnyEvent) {
        let fds = fds
            .iter()
            .map(|fd| fd.try_clone().expect("a duplicate descriptor"));
        let decoded = decode(message, &mut fds.collect(), |msg| {
            let interface = &self.objects[&msg.sender_id];
            AnyEvent::from_wire(interface, msg)
        });
        decoded.unwrap_or_else(|error| panic!("the host sent {message:?}: {error}"))
    }

    /// The host object of `interface`, of which a case has one.
    fn object(&self, interface: &str) -> u32 {
        let mut objects = self.objects.iter();
        let found = objects.find(|(_, name)| *name == interface);
        *found
            .unwrap_or_else(|| panic!("the proxy created no {interface}"))
            .0
    }

    /// `wl_display.delete_id` of `id`. The host keeps its interface, to
    /// decode the events sent before it, until the ID names a new object.
    fn delete(&self, id: u32) -> Outgoing {
        wl_display::evt::delete_id(WL_DISPLAY_ID, id)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }
}

/// `wl_keyboard.keymap` of the default keymap for `keyboard`, compiled
/// once from the pinned XKB defaults.
fn keymap(keyboard: u32) -> Outgoing {
    static KEYMAP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let bytes = KEYMAP.get_or_init(crate::proxy::harness::default_keymap);
    let file = crate::test_support::shm_pool_file(bytes);
    let size = u32::try_from(bytes.len()).expect("a keymap that fits");
    let format = wl_keyboard::KEYMAP_FORMAT_XKB_V1;
    wl_keyboard::evt::keymap(keyboard, format, file.into(), size)
}

/// Whether `request` destroys its sender, for each object a case has that
/// has a destructor: a case destroys a surface as it reopens a window
/// (F2), and releases the keyboard it bound again.
fn destructor(request: &AnyRequest) -> bool {
    matches!(
        request,
        AnyRequest::WlSurface(wl_surface::Request::Destroy {})
            | AnyRequest::WlSeat(wl_seat::Request::Release {})
            | AnyRequest::WlKeyboard(wl_keyboard::Request::Release {})
            | AnyRequest::WlPointer(wl_pointer::Request::Release {})
            | AnyRequest::WlTouch(wl_touch::Request::Release {})
            | AnyRequest::ZcrExtendedTextInputV1(zcr_extended_text_input_v1::Request::Destroy {})
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The events `display` answers each of `requests` with, decoded.
    fn answers(display: &mut Display, requests: &[(u32, AnyRequest)]) -> Vec<(u32, AnyEvent)> {
        let mut exo = Exo::default();
        let mut events = Vec::new();
        for (sender, request) in requests {
            let answer = display.request(*sender, request, &mut exo);
            events.extend(answer.iter().map(|event| display.decode(event)));
        }
        events
    }

    #[test]
    fn a_sync_and_a_destructor_get_their_answers() {
        let mut display = Display::new(Surfaces::new(&[30], &[]));
        let surface = AnyRequest::from(wl_compositor::Request::CreateSurface { id: 7 });
        let requests = [
            (1, wl_display::Request::GetRegistry { registry: 2 }.into()),
            (3, surface),
            (7, wl_surface::Request::Destroy {}.into()),
            (1, wl_display::Request::Sync { callback: 8 }.into()),
        ];
        let global = |name, interface: &str, version| {
            let global = wl_registry::Event::Global {
                name,
                interface: interface.to_owned(),
                version,
            };
            (2, global.into())
        };
        let delete_id = |id| (WL_DISPLAY_ID, wl_display::Event::DeleteId { id }.into());
        let expected: Vec<(u32, AnyEvent)> = vec![
            global(1, "wl_compositor", wl_compositor::VERSION),
            global(2, "wl_seat", wl_seat::VERSION),
            global(
                3,
                "zwp_text_input_manager_v1",
                zwp_text_input_manager_v1::VERSION,
            ),
            global(
                4,
                "zcr_keyboard_extension_v1",
                zcr_keyboard_extension_v1::VERSION,
            ),
            global(
                5,
                "zcr_text_input_extension_v1",
                zcr_text_input_extension_v1::VERSION,
            ),
            delete_id(7),
            (8, wl_callback::Event::Done { callback_data: 0 }.into()),
            delete_id(8),
        ];
        assert_eq!(answers(&mut display, &requests), expected);
        assert_eq!(display.surfaces.host(0), Some(7));
    }

    #[test]
    fn a_sync_keeps_a_snapshot_of_the_trackers_it_follows() {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
        let mut display = Display::new(Surfaces::new(&[30], &[]));
        let mut exo = Exo::default();
        let report = |text: &str| zwp_text_input_v1::Request::SetSurroundingText {
            text: text.into(),
            cursor: 1,
            anchor: 1,
        };
        let requests: [(u32, AnyRequest); 5] = [
            (
                3,
                zwp_text_input_manager_v1::Request::CreateTextInput { id: 5 }.into(),
            ),
            (5, report("ab").into()),
            (1, wl_display::Request::Sync { callback: 8 }.into()),
            (5, report("cd").into()),
            (1, wl_display::Request::Sync { callback: 9 }.into()),
        ];
        for (sender, request) in &requests {
            display.request(*sender, request, &mut exo);
        }
        let synced: Vec<(u32, &str)> = exo
            .synced
            .iter()
            .map(|(callback, trackers)| (*callback, trackers[&5].predicted.text.as_str()))
            .collect();
        assert_eq!(synced, [(8, "ab"), (9, "cd")]);
    }

    #[test]
    fn a_window_opens_again_on_a_new_surface_that_takes_its_focus() {
        let surfaces = Surfaces::new(&[30, 31], &[]);
        let mut display = Display::new(surfaces.clone());
        let mut exo = Exo::default();
        // The seat's focus after `request` from `sender`.
        let request = |display: &mut Display, exo: &mut Exo, sender, request: AnyRequest| {
            display.request(sender, &request, exo);
            exo.focus()
        };
        let create = |id| wl_compositor::Request::CreateSurface { id }.into();
        let destroy = || wl_surface::Request::Destroy {}.into();
        let commit = || wl_surface::Request::Commit {}.into();
        request(
            &mut display,
            &mut exo,
            4,
            wl_seat::Request::GetPointer { id: 5 }.into(),
        );
        request(&mut display, &mut exo, 3, create(7));
        request(&mut display, &mut exo, 3, create(8));
        assert_eq!((surfaces.host(0), surfaces.host(1)), (Some(7), Some(8)));
        exo.set_text_input_focus(Some(7));
        display.click(7, Target::Start, 9);
        // F2: the focused window closes and opens again; its new surface
        // takes the focus once mapped.
        display.reopen(0, true);
        assert_eq!(request(&mut display, &mut exo, 7, destroy()), None);
        assert_eq!(request(&mut display, &mut exo, 3, create(9)), None);
        assert_eq!(request(&mut display, &mut exo, 9, commit()), Some(9));
        assert_eq!(surfaces.host(0), Some(9));
        assert_eq!(surfaces.field_of_host(7), Some(0));
        // The pointer left the destroyed surface, so a click enters anew.
        let click = display.click(9, Target::Start, 9);
        let (_, first) = display.decode(&click[0]);
        assert!(matches!(
            first,
            AnyEvent::WlPointer(wl_pointer::Event::Enter { surface: 9, .. })
        ));
        // An unfocused one leaves the focus where it is.
        display.reopen(1, false);
        assert_eq!(request(&mut display, &mut exo, 8, destroy()), Some(9));
        assert_eq!(request(&mut display, &mut exo, 3, create(10)), Some(9));
        assert_eq!(request(&mut display, &mut exo, 10, commit()), Some(9));
        assert_eq!(surfaces.host(1), Some(10));
    }

    #[test]
    fn a_sync_follows_the_events_of_earlier_requests() {
        let mut display = Display::new(Surfaces::new(&[30], &[]));
        let mut exo = Exo::default();
        // A keyboard of a seat whose focus is on surface 7: it gets the
        // keymap first, Exo enters it at once, and the sync that follows
        // answers after.
        exo.set_text_input_focus(Some(7));
        let keyboard = wl_seat::Request::GetKeyboard { id: 5 };
        let mut events = display.request(4, &keyboard.into(), &mut exo);
        let sync = wl_display::Request::Sync { callback: 8 };
        events.extend(display.request(1, &sync.into(), &mut exo));
        let decoded: Vec<(u32, AnyEvent)> =
            events.iter().map(|event| display.decode(event)).collect();
        let senders: Vec<u32> = decoded.iter().map(|(sender, _)| *sender).collect();
        assert_eq!(senders, [5, 5, 5, 8, WL_DISPLAY_ID]);
        let AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { size, .. }) = decoded[0].1 else {
            panic!("the keymap first, not {:?}", decoded[0].1);
        };
        assert_eq!(size as usize, crate::proxy::harness::default_keymap().len());
    }

    #[test]
    fn a_press_names_its_target_by_where_it_lands() {
        let mut display = Display::new(Surfaces::new(&[30], &[]));
        let pointer = wl_seat::Request::GetPointer { id: 5 };
        let touch = wl_seat::Request::GetTouch { id: 6 };
        answers(&mut display, &[(4, pointer.into()), (4, touch.into())]);
        let x = keymap::position(Target::BeforeMarker(1));
        let click = |display: &mut Display, surface| {
            let events = display.click(surface, Target::BeforeMarker(1), 9);
            let events: Vec<_> = events.iter().map(|event| display.decode(event)).collect();
            events
                .into_iter()
                .map(|(_, event)| event)
                .collect::<Vec<_>>()
        };
        let button = |serial, state| wl_pointer::Event::Button {
            serial,
            time: 9,
            button: BTN_LEFT,
            state,
        };
        let (pressed, released) = (
            wl_pointer::BUTTON_STATE_PRESSED,
            wl_pointer::BUTTON_STATE_RELEASED,
        );
        let first = 1 << 20;
        let enter = wl_pointer::Event::Enter {
            serial: first + 1,
            surface: 20,
            surface_x: x,
            surface_y: 0,
        };
        let expected: Vec<AnyEvent> = vec![
            enter.into(),
            button(first + 2, pressed).into(),
            button(first + 3, released).into(),
        ];
        assert_eq!(click(&mut display, 20), expected);
        // On the same surface, the pointer moves there.
        let motion = wl_pointer::Event::Motion {
            time: 9,
            surface_x: x,
            surface_y: 0,
        };
        let expected: Vec<AnyEvent> = vec![
            motion.into(),
            button(first + 4, pressed).into(),
            button(first + 5, released).into(),
        ];
        assert_eq!(click(&mut display, 20), expected);
        let tap = display.tap(21, Target::Start, 9);
        let tap: Vec<_> = tap.iter().map(|event| display.decode(event)).collect();
        let down = wl_touch::Event::Down {
            serial: first + 6,
            time: 9,
            surface: 21,
            id: 0,
            x: 0,
            y: 0,
        };
        let up = wl_touch::Event::Up {
            serial: first + 7,
            time: 9,
            id: 0,
        };
        let frame = wl_touch::Event::Frame {};
        let expected: Vec<(u32, AnyEvent)> = vec![
            (6, down.into()),
            (6, frame.into()),
            (6, up.into()),
            (6, wl_touch::Event::Frame {}.into()),
        ];
        assert_eq!(tap, expected);
    }
}
