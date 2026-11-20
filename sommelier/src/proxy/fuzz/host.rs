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

//! The host of a fuzz case: the shared host display ([`Display`]) with the
//! Exo model behind it, and what a busy user and a slow host add to it:
//!
//! - each surface the proxy commits takes the seat's focus, as a click on
//!   its window would;
//! - in a case that lets it, it ends the live objects of one interface on
//!   its own as it answers one sync in two ([`super::HOST_DESTROYED`]);
//! - as it answers a sync while the focus is on a surface, the host's
//!   input method composes on each text input there one time in two, each
//!   of its pointers clicks there and each touch taps there two times in
//!   three, and one answer in six takes the input method away, as when its
//!   process restarts;
//! - it holds its answer to the sync after a `zwp_text_input_v1`
//!   deactivation, the proxy's drain barrier, until it answers anything
//!   else, so a transaction the guest sends meanwhile reaches the proxy
//!   first and the proxy defers the field's activation;
//! - it advertises one more `wl_output` and then removes it, so a bind of
//!   it crosses the removal ([`Display::unplugging_last`]).
//!
//! The scheduled cases ([`super::world`]) and the cases over real sockets
//! ([`super::sockets`]) both play it; each choice it makes comes from the
//! caller.

use std::collections::{BTreeMap, BTreeSet};

use super::Counts;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;
use crate::protocols::wayland::{wl_display, wl_surface};
use crate::protocols::AnyRequest;
use crate::sim::display::Display;
use crate::sim::exo::Exo;
use crate::sim::sched::Choose;
use crate::test_support::wire_peer::Peer;
use crate::test_support::{msg_opcode, msg_sender};
use crate::wire::{Outgoing, WL_DISPLAY_ID};

/// The host of a case; see the module documentation.
pub(super) struct Host {
    display: Display,
    exo: Exo,
    /// The interface whose live objects it ends on its own.
    destroys: Option<&'static str>,
    /// Answers held back, and whether the last request was a deactivation.
    held: Vec<Outgoing>,
    after_deactivate: bool,
    /// Whether the input method restarted in the answer to the sync of the
    /// last press, after it: it counts once the proxy's `reset` shows that
    /// the press ended a composition.
    restarted_after_press: bool,
    /// The seat each active host text input was activated with.
    activations: BTreeMap<u32, u32>,
    /// The host IDs it sent `delete_id` for, until an object has one again.
    deleted: BTreeSet<u32>,
    /// The serial of the input method's preedits.
    serial: u32,
    counts: Counts,
}

impl Host {
    /// A host that advertises `globals` and removes the last as soon as
    /// the proxy gets the registry ([`Display::unplugging_last`]).
    pub fn new(globals: Vec<(String, u32)>) -> Self {
        Self {
            display: Display::new(globals).unplugging_last(),
            exo: Exo::default(),
            destroys: None,
            held: Vec::new(),
            after_deactivate: false,
            restarted_after_press: false,
            activations: BTreeMap::new(),
            deleted: BTreeSet::new(),
            serial: 0,
            counts: Counts::default(),
        }
    }

    /// The host ends live objects of `interface` on its own, if any.
    pub fn ends(&mut self, interface: Option<&'static str>) {
        self.destroys = interface;
    }

    /// The host's objects.
    pub fn objects(&self) -> &Peer {
        self.display.objects()
    }

    /// What the host counted so far.
    pub fn counts(&self) -> Counts {
        Counts {
            frame_callbacks_cancelled: self.display.cancelled_frame_callbacks(),
            ..self.counts
        }
    }

    /// The host gets `request` from host object `sender` at `time`, in
    /// milliseconds, and answers, each of its choices from `choices`;
    /// returns what it sends now: nothing while it holds its answers back.
    pub fn answer(
        &mut self,
        sender: u32,
        request: &AnyRequest,
        time: u32,
        choices: &mut impl Choose,
    ) -> Vec<Outgoing> {
        self.observe(sender, request);
        let sync = matches!(
            request,
            AnyRequest::WlDisplay(wl_display::Request::Sync { .. })
        );
        let deactivate = matches!(
            request,
            AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Deactivate { .. })
        );
        let hold = sync && self.after_deactivate;
        self.after_deactivate = deactivate;
        let mut answer = Vec::new();
        if sync {
            self.as_it_syncs(time, choices, &mut answer);
        }
        answer.extend(self.display.request(sender, request, &mut self.exo));
        if let AnyRequest::WlSurface(wl_surface::Request::Commit {}) = request {
            let surface = self.display.objects().live_interface(sender);
            if surface.is_some() && self.exo.focus() != Some(sender) {
                answer.extend(self.display.focus(sender, &mut self.exo));
            }
        }
        self.after(&answer);
        let release = !hold && !answer.is_empty();
        self.held.append(&mut answer);
        match release {
            true => self.release(),
            false => Vec::new(),
        }
    }

    /// The answers it holds back, which it sends now.
    pub fn release(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.held)
    }

    /// Count what `request` shows, before the display has it.
    fn observe(&mut self, sender: u32, request: &AnyRequest) {
        let objects = self.display.objects();
        let crossed = objects.deleted(sender);
        if let Some((id, interface)) = request.new_object() {
            if self.deleted.remove(&id) {
                self.counts.host_ids_reused += 1;
            }
            if interface == "zaura_toplevel" {
                self.counts.aura_toplevels += 1;
            }
        }
        match *request {
            // The proxy's own answer to a press on a composition.
            AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Reset {}) if !crossed => {
                self.counts.compositions_ended_by_presses += 1;
                if std::mem::take(&mut self.restarted_after_press) {
                    self.counts.restarts_after_presses += 1;
                }
            }
            AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Activate { seat, .. })
                if !crossed =>
            {
                self.activations.insert(sender, seat);
            }
            AnyRequest::ZwpTextInputV1(zwp_text_input_v1::Request::Deactivate { seat }) => {
                // The proxy had not yet read the seat's delete_id.
                if objects.deleted(seat) {
                    self.counts.deactivates_of_destroyed_seats += 1;
                }
                self.activations.remove(&sender);
            }
            _ => {}
        }
    }

    /// What the host does as it answers a sync, before its `done`: it may
    /// end live objects of its interface, then composes and presses on the
    /// focused surface, then may restart the input method.
    fn as_it_syncs(&mut self, time: u32, choices: &mut impl Choose, answer: &mut Vec<Outgoing>) {
        if let Some(interface) = self.destroys.filter(|_| choices.pick(2) == 0) {
            let (ended, deletes) = self.display.end(interface, &mut self.exo);
            self.counts.live_objects_host_destroyed += ended as u64;
            answer.extend(deletes);
        }
        let pressed = self.presses(time, choices, answer);
        let restarted = choices.pick(6) == 0 && self.exo.destroy_input_method();
        if restarted {
            self.counts.input_method_restarts += 1;
        }
        // The proxy's `reset` answers the last press, if it ended a
        // composition.
        if pressed {
            self.restarted_after_press = restarted;
        }
    }

    /// The input method composes on each text input attached on the
    /// focused surface now and then, and then each pointer clicks there and
    /// each touch taps there, now and then: the press reaches the proxy
    /// while the preedit is live, and ends it
    /// (`handler::text_input::end_composition_on_press`). Returns whether
    /// anything pressed.
    fn presses(
        &mut self,
        time: u32,
        choices: &mut impl Choose,
        answer: &mut Vec<Outgoing>,
    ) -> bool {
        let Some(surface) = self.exo.focus() else {
            return false;
        };
        for text_input in self.exo.composing_text_inputs() {
            if choices.pick(2) == 0 {
                self.serial += 1;
                let serial = self.serial;
                let preedit =
                    zwp_text_input_v1::evt::preedit_string(text_input, serial, "나", "나");
                answer.push(preedit.expect("a short preedit fits a message"));
            }
        }
        let mut pressed = false;
        for pointer in self.display.pointers() {
            if choices.pick(3) != 0 {
                answer.extend(self.display.click(pointer, surface, 0, time));
                pressed = true;
            }
        }
        for touch in self.display.touches() {
            if choices.pick(3) != 0 {
                answer.extend(self.display.tap(touch, surface, 0, time));
                pressed = true;
            }
        }
        pressed
    }

    /// Count what `answer` shows, and end the activations whose seat
    /// ended while they were active.
    fn after(&mut self, answer: &[Outgoing]) {
        let objects = self.display.objects();
        for (event, _) in answer {
            let sender = msg_sender(event);
            let opcode = msg_opcode(event);
            if (sender, opcode) == (WL_DISPLAY_ID, wl_display::EVT_DELETE_ID) {
                let id = u32::from_ne_bytes(event[8..12].try_into().expect("an ID"));
                self.deleted.insert(id);
            }
            let text_input = objects.live_interface(sender) == Some(zwp_text_input_v1::NAME);
            if text_input && opcode == zwp_text_input_v1::EVT_LEAVE {
                self.counts.text_input_leaves += 1;
            }
        }
        // An activation whose seat ended while active, released by the
        // guest or ended by the host, ends with its seat.
        self.activations.retain(|&text_input, &mut seat| {
            let live = objects.live_interface(text_input).is_some();
            let seat_live = objects.live_interface(seat) == Some("wl_seat");
            if live && !seat_live {
                self.counts.seatless_activations += 1;
            }
            live && seat_live
        });
    }
}
