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

//! The rest of the host compositor, around the Exo model: one host display
//! that the DST ([`super::sched`]) and the fuzzer (`proxy::fuzz`) both
//! drive.
//!
//! The Exo model ([`super::exo`]) plays the seat's keyboard and the text
//! inputs, and leaves the display to its caller. [`Display`] plays it: it
//! holds the host's objects as the server end of the connection
//! ([`wire_peer`]), which holds every request the proxy sends to the
//! protocol, advertises its globals and holds each bind to what it
//! advertised, answers each `wl_display.sync` with
//! `wl_callback.done` and `delete_id`, once the model has kept a snapshot
//! of its trackers for it ([`Exo::snapshot_trackers`]), and each destructor
//! with its `delete_id`, as libwayland-server does. It hands every request
//! to the model and takes the model's events after each one, before it
//! answers a later sync, so that the sync's `done` follows every event of
//! earlier requests (E11). It gives each keyboard the default keymap, as
//! Exo does before anything else, so that the proxy knows which keys
//! repeat, and the seat's focus as it moves; plays the seat's pointers and
//! touches, whose presses land where their caller says; and creates the
//! objects a compositor creates of its own, as weston does: a data offer
//! for each data device, and a buffer for each asynchronous dma-buf
//! create. It destroys a destroyed surface's pending frame callbacks
//! without their `done`, as weston's surface destructor does, and a
//! caller can make it end live objects on its own ([`Display::end`]),
//! whose requests that crossed the `delete_id` then change nothing.

use std::collections::BTreeMap;

use super::exo::Exo;
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1;
use crate::protocols::wayland::{
    wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_display, wl_keyboard,
    wl_pointer, wl_registry, wl_seat, wl_surface, wl_touch,
};
use crate::protocols::AnyRequest;
use crate::test_support::wire_peer::{self, Peer, Role};
use crate::wire::{Outgoing, WL_DISPLAY_ID};

/// `BTN_LEFT`, the primary button.
pub(crate) const BTN_LEFT: u32 = 0x110;

/// The globals Exo advertises that the DST's proxy binds, at their newest:
/// Exo advertises v14 of the text-input extension.
pub(super) fn exo_globals() -> Vec<(String, u32)> {
    [
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
        (
            zcr_text_input_extension_v1::NAME,
            zcr_text_input_extension_v1::VERSION,
        ),
    ]
    .map(|(interface, version)| (interface.to_owned(), version))
    .to_vec()
}

/// The host's display, globals, keyboards' focus, pointers and touches.
pub(crate) struct Display {
    /// The globals it advertises, by name from 1: interface and version.
    globals: Vec<(String, u32)>,
    /// The host's objects.
    objects: Peer,
    /// Each surface's pending frame callbacks, which never get `done`.
    frame_callbacks: BTreeMap<u32, Vec<u32>>,
    /// The surface each pointer is on.
    pointers: BTreeMap<u32, u32>,
    /// The last serial an input event of a pointer or touch took. Exo
    /// numbers its own events; the two never meet.
    serial: u32,
    /// The frame callbacks it destroyed with their surface.
    cancelled: u64,
    /// The interfaces whose destroyed objects it never sends a
    /// `delete_id` ([`Display::unanswered`]).
    unanswered: &'static [&'static str],
    /// Whether its last global is an output it removes as soon as it
    /// advertises it, and whether it has ([`Display::unplugging_last`]).
    unplugging: bool,
    unplugged: bool,
}

impl Display {
    /// A display that advertises `globals`, by name from 1.
    pub(crate) fn new(globals: Vec<(String, u32)>) -> Self {
        Self {
            globals,
            objects: Peer::new(Role::Server),
            frame_callbacks: BTreeMap::new(),
            pointers: BTreeMap::new(),
            serial: 1 << 20,
            cancelled: 0,
            unanswered: &[],
            unplugging: false,
            unplugged: false,
        }
    }

    /// A display whose last global goes away as soon as it advertises it,
    /// as an output unplugged while a client starts: the first registry
    /// gets it and its `global_remove`, so a bind of it crosses the
    /// removal, and later ones never get it.
    pub(crate) fn unplugging_last(self) -> Self {
        Self {
            unplugging: true,
            ..self
        }
    }

    /// A display that never answers the destroy of an object of
    /// `interfaces` with its `delete_id`, so that the proxy never uses its
    /// ID again. libwayland-server answers every destructor; the DST's
    /// display leaves out `zcr_extended_keyboard_v1`'s, as its own model
    /// did before it shared this one: answering it shifts every DST
    /// schedule, and the shrunk regressions (`sim::regressions`) replay
    /// theirs.
    pub(super) fn unanswered(self, interfaces: &'static [&'static str]) -> Self {
        Self {
            unanswered: interfaces,
            ..self
        }
    }

    /// The host's objects.
    pub(crate) fn objects(&self) -> &Peer {
        &self.objects
    }

    /// How many frame callbacks it destroyed with their surface.
    pub(crate) fn cancelled_frame_callbacks(&self) -> u64 {
        self.cancelled
    }

    /// The proxy sends `request` from host object `sender`: `exo` applies
    /// it, and the events that follow are the keymap of a new keyboard,
    /// Exo's, then the display's answer. A request from or naming an
    /// object the display ended on its own changes nothing.
    ///
    /// # Panics
    ///
    /// If the request breaks the protocol ([`wire_peer`]), or binds a
    /// global as the display did not advertise it.
    pub(crate) fn request(
        &mut self,
        sender: u32,
        request: &AnyRequest,
        exo: &mut Exo,
    ) -> Vec<Outgoing<u32>> {
        if let AnyRequest::WlRegistry(wl_registry::Request::Bind { name, id }) = request {
            self.check_bind(*name, &id.0, id.1);
        }
        let interface = self
            .objects
            .interface(sender)
            .unwrap_or_default()
            .to_owned();
        let crossed = std::iter::once(sender)
            .chain(request.object_args().into_iter().map(|(id, ..)| id))
            .any(|id| self.objects.deleted(id));
        let owed = self
            .objects
            .request(sender, request)
            .unwrap_or_else(|violation| {
                panic!("the proxy sent the host {sender} {request:?}: {violation}")
            });
        if !crossed {
            exo.request(sender, request);
        }
        if request.is_destructor() {
            self.forget(exo);
        }
        let mut events = Vec::new();
        if let AnyRequest::WlSeat(wl_seat::Request::GetKeyboard { id }) = *request {
            events.push(keymap(id));
        }
        events.extend(exo.take_events());
        match *request {
            AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) => {
                exo.snapshot_trackers(callback);
                events.push(wl_callback::evt::done(callback, 0));
                events.push(wl_display::evt::delete_id(WL_DISPLAY_ID, callback));
            }
            AnyRequest::WlDisplay(wl_display::Request::GetRegistry { registry }) => {
                let advertised = self.globals.len() - usize::from(self.unplugged);
                for (name, (interface, version)) in (1..).zip(&self.globals[..advertised]) {
                    let global = wl_registry::evt::global(registry, name, interface, *version);
                    events.push(global.expect("a global fits a message"));
                }
                if self.unplugging && !std::mem::replace(&mut self.unplugged, true) {
                    let name = u32::try_from(advertised).expect("a few globals");
                    events.push(wl_registry::evt::global_remove(registry, name));
                }
            }
            AnyRequest::WlSurface(wl_surface::Request::Frame { callback }) => {
                self.frame_callbacks
                    .entry(sender)
                    .or_default()
                    .push(callback);
            }
            AnyRequest::WlDataDeviceManager(wl_data_device_manager::Request::GetDataDevice {
                id,
                ..
            }) => {
                let offer = self.objects.free_server_id();
                events.push(wl_data_device::evt::data_offer(id, offer));
            }
            AnyRequest::ZwpLinuxBufferParamsV1(zwp_linux_buffer_params_v1::Request::Create {
                ..
            }) => {
                let buffer = self.objects.free_server_id();
                events.push(zwp_linux_buffer_params_v1::evt::created(sender, buffer));
            }
            _ => {}
        }
        // A destroyed surface's pending frame callbacks go first, without
        // `done`, as in weston's surface destructor, even if the host had
        // ended the surface on its own.
        if request.is_destructor() {
            let callbacks = self.frame_callbacks.remove(&sender).unwrap_or_default();
            self.cancelled += callbacks.len() as u64;
            events.extend(callbacks.into_iter().map(delete_id));
        }
        if let Some(ended) = owed.filter(|_| !self.unanswered.contains(&interface.as_str())) {
            events.push(delete_id(ended));
        }
        self.sent(&events);
        events
    }

    /// Check a bind of global `name` as `interface` at `version`: the
    /// display advertised it so, at that version or newer. A bind of the
    /// global it removed, which crossed the removal, is one too.
    ///
    /// # Panics
    ///
    /// If it did not.
    fn check_bind(&self, name: u32, interface: &str, version: u32) {
        let index = usize::try_from(name)
            .ok()
            .and_then(|name| name.checked_sub(1));
        let advertised = index.and_then(|index| self.globals.get(index));
        assert!(
            advertised.is_some_and(|(advertised, newest)| {
                advertised == interface && (1..=*newest).contains(&version)
            }),
            "the proxy bound global {name} as {interface} v{version}; the host advertised \
             {advertised:?}"
        );
    }

    /// The seat's focus moves to `surface`, as a click on its window does:
    /// Exo moves it ([`Exo::move_focus`]), and the seat's other keyboards,
    /// which Exo does not model, follow it.
    pub(crate) fn focus(&mut self, surface: u32, exo: &mut Exo) -> Vec<Outgoing<u32>> {
        let lost = exo.focus();
        exo.move_focus(Some(surface));
        let mut events = exo.take_events();
        let others = self.objects.live(wl_keyboard::NAME);
        for keyboard in others.into_iter().filter(|&id| Some(id) != exo.keyboard()) {
            if let Some(left) = lost {
                let serial = self.next_serial();
                events.push(wl_keyboard::evt::leave(keyboard, serial, left));
            }
            let serial = self.next_serial();
            let enter = wl_keyboard::evt::enter(keyboard, serial, surface, &[]);
            events.push(enter.expect("an enter with no keys fits a message"));
        }
        self.sent(&events);
        events
    }

    /// The live pointers, in ID order.
    pub(crate) fn pointers(&self) -> Vec<u32> {
        self.objects.live(wl_pointer::NAME)
    }

    /// The live touches, in ID order.
    pub(crate) fn touches(&self) -> Vec<u32> {
        self.objects.live(wl_touch::NAME)
    }

    /// `pointer` presses and releases the primary button at `x` on
    /// `surface`, entering it first if it is elsewhere, and moving there
    /// if not.
    pub(crate) fn click(
        &mut self,
        pointer: u32,
        surface: u32,
        x: i32,
        time: u32,
    ) -> Vec<Outgoing<u32>> {
        let mut events = Vec::new();
        if self.pointers.get(&pointer) != Some(&surface) {
            if let Some(left) = self.pointers.insert(pointer, surface) {
                let serial = self.next_serial();
                events.push(wl_pointer::evt::leave(pointer, serial, left));
            }
            let serial = self.next_serial();
            events.push(wl_pointer::evt::enter(pointer, serial, surface, x, 0));
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
        self.sent(&events);
        events
    }

    /// A finger of `touch` touches `surface` at `x` and lifts.
    pub(crate) fn tap(
        &mut self,
        touch: u32,
        surface: u32,
        x: i32,
        time: u32,
    ) -> Vec<Outgoing<u32>> {
        let down = wl_touch::evt::down(touch, self.next_serial(), time, surface, 0, x, 0);
        let up = wl_touch::evt::up(touch, self.next_serial(), time, 0);
        let frame = || wl_touch::evt::frame(touch);
        let events = vec![down, frame(), up, frame()];
        self.sent(&events);
        events
    }

    /// End every live object of `interface` the proxy created, as a host
    /// may on its own: each surface's frame callbacks first, as a
    /// compositor destroys a surface. Returns how many it ended of
    /// `interface`, and their `delete_id`s.
    pub(crate) fn end(&mut self, interface: &str, exo: &mut Exo) -> (usize, Vec<Outgoing<u32>>) {
        let ended: Vec<u32> = self
            .objects
            .live(interface)
            .into_iter()
            .filter(|&id| id < wire_peer::SERVER_ID_START)
            .collect();
        let mut events = Vec::new();
        for &id in &ended {
            let callbacks = self.frame_callbacks.remove(&id).unwrap_or_default();
            events.extend(callbacks.into_iter().chain([id]).map(delete_id));
        }
        self.sent(&events);
        self.forget(exo);
        (ended.len(), events)
    }

    /// `exo` and the pointers forget the objects that ended. A destroyed
    /// surface loses the seat's and the pointers' focus, with no `leave`;
    /// Exo deactivates its text inputs.
    fn forget(&mut self, exo: &mut Exo) {
        let objects = &self.objects;
        let live = |id: u32| objects.live_interface(id).is_some();
        exo.forget(live);
        self.pointers
            .retain(|&pointer, &mut surface| live(pointer) && live(surface));
    }

    /// The display sent `events`.
    fn sent(&mut self, events: &[Outgoing<u32>]) {
        for event in events {
            self.objects.send(event);
        }
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }
}

/// `wl_display.delete_id` of `id`.
fn delete_id(id: u32) -> Outgoing<u32> {
    wl_display::evt::delete_id(WL_DISPLAY_ID, id)
}

/// `wl_keyboard.keymap` of the default keymap for `keyboard`, compiled
/// once from the pinned XKB defaults.
fn keymap(keyboard: u32) -> Outgoing<u32> {
    static KEYMAP: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let bytes = KEYMAP.get_or_init(crate::proxy::harness::default_keymap);
    let file = crate::test_support::shm_pool_file(bytes);
    let size = u32::try_from(bytes.len()).expect("a keymap that fits");
    let format = wl_keyboard::KEYMAP_FORMAT_XKB_V1;
    wl_keyboard::evt::keymap(keyboard, format, file.into(), size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
    use crate::protocols::wayland::{wl_data_offer, wl_output};
    use crate::protocols::AnyEvent;
    use crate::test_support::wire_peer::SERVER_ID_START;

    /// A client of a display: it sends requests and gets the display's
    /// answers, decoded and held to the protocol as it receives them.
    struct Client {
        objects: Peer,
        display: Display,
        exo: Exo,
    }

    impl Client {
        /// A client of a display that advertises `globals`, with registry
        /// 2, which binds global `n` as object `n + 2`, at its version.
        fn new(globals: &[(&str, u32)]) -> Self {
            let globals = globals
                .iter()
                .map(|&(interface, version)| (interface.to_owned(), version))
                .collect();
            Self::of(Display::new(globals))
        }

        /// [`Client::new`], of `display`.
        fn of(display: Display) -> Self {
            let mut client = Self {
                objects: Peer::new(Role::Client),
                display,
                exo: Exo::default(),
            };
            let advertised = client.send(1, wl_display::Request::GetRegistry { registry: 2 });
            for (_, global) in advertised {
                let AnyEvent::WlRegistry(wl_registry::Event::Global {
                    name,
                    interface,
                    version,
                }) = global
                else {
                    panic!("not a global: {global:?}");
                };
                let bind = wl_registry::Request::Bind {
                    name,
                    id: (interface, version, name + 2),
                };
                assert_eq!(client.send(2, bind), []);
            }
            client
        }

        /// The client sends `request` from `sender`; returns the display's
        /// answer.
        fn send(&mut self, sender: u32, request: impl Into<AnyRequest>) -> Vec<(u32, AnyEvent)> {
            let request = request.into();
            self.objects
                .request(sender, &request)
                .expect("a client sends");
            let answer = self.display.request(sender, &request, &mut self.exo);
            self.receive(answer)
        }

        /// The client receives `events` from the display.
        fn receive(&mut self, events: Vec<Outgoing<u32>>) -> Vec<(u32, AnyEvent)> {
            events
                .into_iter()
                .map(|message| {
                    let (message, fds) = message.into_parts();
                    let event = self.objects.receive(&message, &mut fds.into());
                    event.unwrap_or_else(|violation| panic!("the display sent {violation}"))
                })
                .collect()
        }
    }

    fn delete_id(id: u32) -> (u32, AnyEvent) {
        (WL_DISPLAY_ID, wl_display::Event::DeleteId { id }.into())
    }

    #[test]
    fn a_registry_gets_the_globals_and_a_sync_and_a_destructor_their_answers() {
        let mut client = Client::new(&[]);
        let global = |name, interface: &str, version| {
            let global = wl_registry::Event::Global {
                name,
                interface: interface.to_owned(),
                version,
            };
            (5, global.into())
        };
        let mut client_of_two = Client::new(&[(wl_compositor::NAME, 6), (wl_seat::NAME, 9)]);
        let registry = wl_display::Request::GetRegistry { registry: 5 };
        assert_eq!(
            client_of_two.send(1, registry),
            [
                global(1, wl_compositor::NAME, 6),
                global(2, wl_seat::NAME, 9)
            ]
        );
        let surface = wl_compositor::Request::CreateSurface { id: 6 };
        assert_eq!(client_of_two.send(3, surface), []);
        let destroy = wl_surface::Request::Destroy {};
        assert_eq!(client_of_two.send(6, destroy), [delete_id(6)]);
        let sync = wl_display::Request::Sync { callback: 3 };
        let done = wl_callback::Event::Done { callback_data: 0 };
        assert_eq!(client.send(1, sync), [(3, done.into()), delete_id(3)]);
    }

    #[test]
    fn a_new_keyboard_gets_the_keymap_first_and_a_sync_follows_exos_events() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6), (wl_seat::NAME, 9)]);
        // A keyboard of a seat whose focus is on surface 5: it gets the
        // keymap first, Exo enters it at once, and the sync that follows
        // answers after.
        client.send(3, wl_compositor::Request::CreateSurface { id: 5 });
        client.exo.set_text_input_focus(Some(5));
        let mut events = client.send(4, wl_seat::Request::GetKeyboard { id: 6 });
        events.extend(client.send(1, wl_display::Request::Sync { callback: 7 }));
        let senders: Vec<u32> = events.iter().map(|(sender, _)| *sender).collect();
        assert_eq!(senders, [6, 6, 6, 7, WL_DISPLAY_ID]);
        let AnyEvent::WlKeyboard(wl_keyboard::Event::Keymap { size, .. }) = events[0].1 else {
            panic!("the keymap first, not {:?}", events[0].1);
        };
        assert_eq!(size as usize, crate::proxy::harness::default_keymap().len());
    }

    #[test]
    fn a_sync_keeps_a_snapshot_of_the_trackers_it_follows() {
        let mut client = Client::new(&[(zwp_text_input_manager_v1::NAME, 1)]);
        let report = |text: &str| zwp_text_input_v1::Request::SetSurroundingText {
            text: text.into(),
            cursor: 1,
            anchor: 1,
        };
        let create = zwp_text_input_manager_v1::Request::CreateTextInput { id: 4 };
        client.send(3, create);
        client.send(4, report("ab"));
        client.send(1, wl_display::Request::Sync { callback: 5 });
        client.send(4, report("cd"));
        client.send(1, wl_display::Request::Sync { callback: 6 });
        let synced: Vec<(u32, &str)> = client
            .exo
            .synced
            .iter()
            .map(|(callback, trackers)| (*callback, trackers[&4].predicted.text.as_str()))
            .collect();
        assert_eq!(synced, [(5, "ab"), (6, "cd")]);
    }

    #[test]
    fn a_destroyed_surface_takes_its_pending_frame_callbacks_without_done() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6)]);
        client.send(3, wl_compositor::Request::CreateSurface { id: 4 });
        client.send(4, wl_surface::Request::Frame { callback: 5 });
        client.send(4, wl_surface::Request::Frame { callback: 6 });
        let destroy = wl_surface::Request::Destroy {};
        assert_eq!(
            client.send(4, destroy),
            [delete_id(5), delete_id(6), delete_id(4)]
        );
        assert_eq!(client.display.cancelled_frame_callbacks(), 2);
    }

    #[test]
    fn a_host_that_ends_objects_on_its_own_ignores_the_requests_that_crossed_it() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6)]);
        client.send(3, wl_compositor::Request::CreateSurface { id: 4 });
        client.send(4, wl_surface::Request::Frame { callback: 5 });
        client.send(3, wl_compositor::Request::CreateSurface { id: 6 });
        client.exo.set_text_input_focus(Some(4));
        // A surface's frame callbacks end first, while it is still live.
        let ended_with_surfaces = [wl_surface::NAME, wl_callback::NAME];
        let may_end = move |_, interface: &str| ended_with_surfaces.contains(&interface);
        client.objects.server_may_end(may_end);
        let (ended, deletes) = client.display.end(wl_surface::NAME, &mut client.exo);
        assert_eq!(ended, 2);
        assert_eq!(
            client.receive(deletes),
            [delete_id(5), delete_id(4), delete_id(6)]
        );
        assert_eq!(client.exo.focus(), None, "the focus went with its surface");
        // A frame request crossed the delete_id: its callback goes with the
        // surface's destroy, which crossed it too, and the surface's ID is
        // free.
        assert_eq!(
            client.send(4, wl_surface::Request::Frame { callback: 7 }),
            []
        );
        assert_eq!(
            client.send(4, wl_surface::Request::Destroy {}),
            [delete_id(7)]
        );
        assert_eq!(
            client.send(3, wl_compositor::Request::CreateSurface { id: 4 }),
            []
        );
    }

    #[test]
    fn the_display_creates_objects_of_its_own_under_free_server_ids() {
        let mut client = Client::new(&[
            (wl_seat::NAME, 9),
            (wl_data_device_manager::NAME, 3),
            (zwp_linux_dmabuf_v1::NAME, 3),
        ]);
        let device = wl_data_device_manager::Request::GetDataDevice { id: 6, seat: 3 };
        let offer = |id| (6, wl_data_device::Event::DataOffer { id }.into());
        assert_eq!(client.send(4, device), [offer(SERVER_ID_START)]);
        let params = zwp_linux_dmabuf_v1::Request::CreateParams { params_id: 7 };
        client.send(5, params);
        let create = zwp_linux_buffer_params_v1::Request::Create {
            width: 1,
            height: 1,
            format: 0,
            flags: 0,
        };
        let created = |buffer| {
            (
                7,
                zwp_linux_buffer_params_v1::Event::Created { buffer }.into(),
            )
        };
        assert_eq!(client.send(7, create), [created(SERVER_ID_START + 1)]);
        // An offer the client destroys frees its ID at once.
        let destroy = wl_data_offer::Request::Destroy {};
        assert_eq!(client.send(SERVER_ID_START, destroy), []);
        let device = wl_data_device_manager::Request::GetDataDevice { id: 8, seat: 3 };
        let offer = |id| (8, wl_data_device::Event::DataOffer { id }.into());
        assert_eq!(client.send(4, device), [offer(SERVER_ID_START)]);
    }

    #[test]
    fn the_seats_other_keyboards_follow_its_focus() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6), (wl_seat::NAME, 9)]);
        client.send(3, wl_compositor::Request::CreateSurface { id: 5 });
        client.send(3, wl_compositor::Request::CreateSurface { id: 6 });
        client.send(4, wl_seat::Request::GetKeyboard { id: 7 });
        // Exo plays the last keyboard bound; the display the first.
        client.send(4, wl_seat::Request::GetKeyboard { id: 8 });
        let focus = client.display.focus(5, &mut client.exo);
        let entered: Vec<(u32, &str)> = client
            .receive(focus)
            .into_iter()
            .filter_map(|(keyboard, event)| match event {
                AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { surface: 5, .. }) => {
                    Some((keyboard, "enter"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(entered, [(8, "enter"), (7, "enter")]);
        let focus = client.display.focus(6, &mut client.exo);
        let first: Vec<(u32, AnyEvent)> = client.receive(focus).into_iter().skip(3).collect();
        assert!(
            matches!(
                &first[..],
                [
                    (
                        7,
                        AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { surface: 5, .. })
                    ),
                    (
                        7,
                        AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { surface: 6, .. })
                    ),
                ]
            ),
            "{first:?}"
        );
    }

    #[test]
    fn a_press_lands_where_its_caller_says() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6), (wl_seat::NAME, 9)]);
        client.send(3, wl_compositor::Request::CreateSurface { id: 5 });
        client.send(4, wl_seat::Request::GetPointer { id: 6 });
        client.send(4, wl_seat::Request::GetTouch { id: 7 });
        assert_eq!(client.display.pointers(), [6]);
        assert_eq!(client.display.touches(), [7]);
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
        let click = client.display.click(6, 5, 40, 9);
        let enter = wl_pointer::Event::Enter {
            serial: first + 1,
            surface: 5,
            surface_x: 40,
            surface_y: 0,
        };
        let expected: Vec<(u32, AnyEvent)> = vec![
            (6, enter.into()),
            (6, button(first + 2, pressed).into()),
            (6, button(first + 3, released).into()),
        ];
        assert_eq!(client.receive(click), expected);
        // On the same surface, the pointer moves there.
        let click = client.display.click(6, 5, 80, 9);
        let motion = wl_pointer::Event::Motion {
            time: 9,
            surface_x: 80,
            surface_y: 0,
        };
        let expected: Vec<(u32, AnyEvent)> = vec![
            (6, motion.into()),
            (6, button(first + 4, pressed).into()),
            (6, button(first + 5, released).into()),
        ];
        assert_eq!(client.receive(click), expected);
        let tap = client.display.tap(7, 5, 40, 9);
        let down = wl_touch::Event::Down {
            serial: first + 6,
            time: 9,
            surface: 5,
            id: 0,
            x: 40,
            y: 0,
        };
        let up = wl_touch::Event::Up {
            serial: first + 7,
            time: 9,
            id: 0,
        };
        let expected: Vec<(u32, AnyEvent)> = vec![
            (7, down.into()),
            (7, wl_touch::Event::Frame {}.into()),
            (7, up.into()),
            (7, wl_touch::Event::Frame {}.into()),
        ];
        assert_eq!(client.receive(tap), expected);
        // A destroyed surface takes the pointer's focus with it, so the next
        // click enters anew.
        client.send(5, wl_surface::Request::Destroy {});
        client.send(3, wl_compositor::Request::CreateSurface { id: 5 });
        let click = client.display.click(6, 5, 40, 9);
        assert!(matches!(
            client.receive(click)[0].1,
            AnyEvent::WlPointer(wl_pointer::Event::Enter { surface: 5, .. })
        ));
    }

    #[test]
    fn the_first_registry_sees_the_last_global_go_and_later_ones_never_see_it() {
        let globals = [(wl_compositor::NAME, 6), (wl_output::NAME, 4)];
        let globals = globals.map(|(interface, version)| (interface.to_owned(), version));
        let mut client = Client {
            objects: Peer::new(Role::Client),
            display: Display::new(globals.to_vec()).unplugging_last(),
            exo: Exo::default(),
        };
        let global = |registry, name, interface: &str, version| {
            let global = wl_registry::Event::Global {
                name,
                interface: interface.to_owned(),
                version,
            };
            (registry, global.into())
        };
        let remove = wl_registry::Event::GlobalRemove { name: 2 };
        assert_eq!(
            client.send(1, wl_display::Request::GetRegistry { registry: 2 }),
            [
                global(2, 1, wl_compositor::NAME, 6),
                global(2, 2, wl_output::NAME, 4),
                (2, remove.into()),
            ]
        );
        assert_eq!(
            client.send(1, wl_display::Request::GetRegistry { registry: 3 }),
            [global(3, 1, wl_compositor::NAME, 6)]
        );
    }

    #[test]
    #[should_panic(expected = "the proxy bound global 1 as wl_compositor v7; the host advertised")]
    fn a_bind_newer_than_the_global_is_refused() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6)]);
        let bind = wl_registry::Request::Bind {
            name: 1,
            id: (wl_compositor::NAME.to_owned(), 7, 4),
        };
        client.send(2, bind);
    }

    #[test]
    #[should_panic(expected = "the proxy bound global 1 as wl_seat v1; the host advertised")]
    fn a_bind_of_another_interface_is_refused() {
        let mut client = Client::new(&[(wl_compositor::NAME, 6)]);
        let bind = wl_registry::Request::Bind {
            name: 1,
            id: (wl_seat::NAME.to_owned(), 1, 4),
        };
        client.send(2, bind);
    }

    #[test]
    fn an_unanswered_destroy_keeps_its_id() {
        let globals = vec![(wl_compositor::NAME.to_owned(), 6)];
        let display = Display::new(globals).unanswered(&[wl_surface::NAME]);
        let mut client = Client::of(display);
        client.send(3, wl_compositor::Request::CreateSurface { id: 4 });
        assert_eq!(client.send(4, wl_surface::Request::Destroy {}), []);
        assert_eq!(client.display.objects().free_server_id(), SERVER_ID_START);
        let error = client
            .display
            .objects
            .request(3, &wl_compositor::Request::CreateSurface { id: 4 }.into())
            .expect_err("in use");
        assert!(error.contains("still in use"), "{error}");
    }
}
