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
//! can end it first ([`crate::handler::text_input::end_composition_on_press`]),
//! and gives some clients wheel steps for touchpad scrolling
//! ([`WheelSteps`]).

use crate::handler::Handlers;
use std::collections::BTreeMap;

use crate::client_kind::WheelClient;
use crate::protocols::wayland::wl_pointer;
use crate::state::{Context, EventObject, EventSender, HostId};
use crate::wire::Action;

/// The scroll distance of one wheel step, in 24.8 fixed-point surface
/// units: Exo's own for one wheel notch (`kWaylandScrollUnit`, 10, in
/// `WaylandPointerDelegate::OnPointerScroll`), so a touchpad scroll as far
/// as one notch scrolls is one step.
const WHEEL_STEP: i64 = 10 * 256;

/// Wheel steps synthesized from touchpad scrolling, for a connection
/// whose client gets them ([`WheelClient`], which `crate::client_kind`
/// decides): Exo sends a touchpad scroll as `wl_pointer.axis` alone, and
/// Wine scrolls only on `axis_discrete`. Each host pointer that scrolled
/// has a [`WheelStepper`], from its first `axis_source` once the client
/// is known to get steps, until its `enter`, `leave` or end.
#[derive(Debug)]
pub(crate) struct WheelSteps {
    client: WheelClient,
    pointers: BTreeMap<HostId, WheelStepper>,
}

// A source is known only on a pointer that has `axis_discrete`: the
// dispatcher rejects an event newer than its object.
const _: () = assert!(wl_pointer::EVT_AXIS_SOURCE_SINCE >= wl_pointer::EVT_AXIS_DISCRETE_SINCE);
// The steps are `axis_discrete` events, which a pointer from v8 on, which
// gets `axis_value120` instead, no longer takes: no guest seat is that new.
const _: () =
    assert!(crate::handler::registry::WL_SEAT_VERSION < wl_pointer::EVT_AXIS_VALUE120_SINCE);

impl WheelSteps {
    pub(crate) fn new(client: WheelClient) -> Self {
        Self {
            client,
            pointers: BTreeMap::new(),
        }
    }

    /// The pointers it follows; every batch checks they are live.
    #[cfg(test)]
    pub(crate) fn pointers_for_test(&self) -> impl Iterator<Item = HostId> + '_ {
        self.pointers.keys().copied()
    }
}

/// One pointer's wheel steps: the source of its current frame and, per
/// axis (`AXIS_VERTICAL_SCROLL`, `AXIS_HORIZONTAL_SCROLL`), the finger or
/// continuous distance not yet sent as a step. Pure arithmetic: the
/// handler feeds it the pointer's events.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct WheelStepper {
    source: Option<u32>,
    remainder: [i64; 2],
}

impl WheelStepper {
    /// `wl_pointer.axis_source`: the source of this frame's axes.
    fn source(&mut self, source: u32) {
        self.source = Some(source);
    }

    /// `wl_pointer.frame`: the next frame names its source anew.
    fn frame(&mut self) {
        self.source = None;
    }

    /// `wl_pointer.axis_stop`: `axis` starts from nothing again.
    fn stop(&mut self, axis: u32) {
        if let Some(remainder) = self.remainder.get_mut(axis as usize) {
            *remainder = 0;
        }
    }

    /// `wl_pointer.axis` of `value` on `axis`: the [`WHEEL_STEP`]s it
    /// crosses, signed like the scroll, if this frame's source is a finger
    /// or continuous and it crosses one. The rest carries over.
    fn axis(&mut self, axis: u32, value: i32) -> Option<i32> {
        let continuous = matches!(
            self.source,
            Some(wl_pointer::AXIS_SOURCE_FINGER | wl_pointer::AXIS_SOURCE_CONTINUOUS)
        );
        let remainder = self
            .remainder
            .get_mut(axis as usize)
            .filter(|_| continuous)?;
        let distance = *remainder + i64::from(value);
        let steps = distance / WHEEL_STEP;
        *remainder = distance - steps * WHEEL_STEP;
        // |steps| <= |value| / WHEEL_STEP + 1, so it fits.
        (steps != 0).then_some(steps as i32)
    }
}

/// `pointer`'s stepper, if it scrolled since it was last restarted.
fn stepper(ctx: &mut Context, pointer: HostId) -> Option<&mut WheelStepper> {
    ctx.wheel_steps.pointers.get_mut(&pointer)
}

/// Start `pointer`'s wheel steps again.
fn restart_steps(ctx: &mut Context, pointer: HostId) {
    ctx.wheel_steps.pointers.remove(&pointer);
}

impl wl_pointer::WlPointerHandler for Handlers {
    fn on_enter(
        ctx: &mut Context,
        sender: EventSender,
        _serial: u32,
        surface: EventObject,
        _surface_x: i32,
        _surface_y: i32,
    ) -> Action {
        // The generated dispatcher calls this only for a host surface
        // paired with a live guest surface (`ShadowTable::event_object`):
        // it drops an `enter` or `leave` that names a surface the guest
        // destroyed, whose end already took the pointer off it.
        ctx.pointer_focus.enter(sender.host, surface.guest);
        restart_steps(ctx, sender.host);
        Action::Forward
    }

    fn on_leave(
        ctx: &mut Context,
        sender: EventSender,
        _serial: u32,
        surface: EventObject,
    ) -> Action {
        // The dispatcher resolved the surface, as for `enter`.
        ctx.pointer_focus.leave(sender.host, surface.guest);
        restart_steps(ctx, sender.host);
        Action::Forward
    }

    /// The client's first scroll after its detection answered settles
    /// whether it gets wheel steps ([`WheelClient::settle`]).
    fn on_axis_source(ctx: &mut Context, sender: EventSender, axis_source: u32) -> Action {
        let steps = &mut ctx.wheel_steps;
        if steps.client.settle() {
            steps
                .pointers
                .entry(sender.host)
                .or_default()
                .source(axis_source);
        }
        Action::Forward
    }

    fn on_frame(ctx: &mut Context, sender: EventSender) -> Action {
        if let Some(stepper) = stepper(ctx, sender.host) {
            stepper.frame();
        }
        Action::Forward
    }

    fn on_axis_stop(ctx: &mut Context, sender: EventSender, _time: u32, axis: u32) -> Action {
        if let Some(stepper) = stepper(ctx, sender.host) {
            stepper.stop(axis);
        }
        Action::Forward
    }

    /// A finger or continuous scroll that crosses [`WHEEL_STEP`]s reaches
    /// a client with [`WheelSteps`] as `axis_discrete`, then the `axis`,
    /// which the protocol says it precedes. A wheel's `axis` comes with
    /// the host's own `axis_discrete`, and passes as it is.
    fn on_axis(ctx: &mut Context, sender: EventSender, time: u32, axis: u32, value: i32) -> Action {
        let Some(guest_pointer) = sender.guest_id() else {
            return Action::Forward;
        };
        let Some(steps) = stepper(ctx, sender.host).and_then(|stepper| stepper.axis(axis, value))
        else {
            return Action::Forward;
        };
        ctx.send_guest_all([
            wl_pointer::evt::axis_discrete(guest_pointer, axis, steps),
            wl_pointer::evt::axis(guest_pointer, time, axis, value),
        ]);
        Action::Drop
    }

    /// A press on the surface of a composition ends it before the guest
    /// gets the press.
    fn on_button(
        ctx: &mut Context,
        sender: EventSender,
        serial: u32,
        time: u32,
        button: u32,
        state: u32,
    ) -> Action {
        if state != wl_pointer::BUTTON_STATE_PRESSED {
            return Action::Forward;
        }
        let (Some(surface), Some(guest_pointer)) =
            (ctx.pointer_focus.surface(sender.host), sender.guest_id())
        else {
            return Action::Forward;
        };
        crate::handler::text_input::end_composition_on_press(
            ctx,
            surface,
            wl_pointer::evt::button(guest_pointer, serial, time, button, state),
        )
    }
}

/// Forget host pointer `pointer`, which ended (see
/// `crate::state::lifecycle`).
pub(crate) fn forget_pointer(ctx: &mut Context, pointer: HostId) {
    ctx.pointer_focus.forget_pointer(pointer);
    restart_steps(ctx, pointer);
}

#[cfg(test)]
mod tests {
    use super::WheelStepper;
    use crate::client_kind::WheelClient;
    use crate::protocols::wayland::{wl_pointer, wl_seat, wl_surface};
    use crate::protocols::AnyEvent;
    use crate::proxy::harness::{Object, Session};
    use crate::settings::Settings;
    use crate::state::{ConnectionFacts, Context};
    use std::rc::Rc;

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
            s.ctx().pointer_focus.surface(pointer.host_id()),
            Some(surface.guest_id())
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

    const VERTICAL: u32 = wl_pointer::AXIS_VERTICAL_SCROLL;
    const HORIZONTAL: u32 = wl_pointer::AXIS_HORIZONTAL_SCROLL;
    const FINGER: u32 = wl_pointer::AXIS_SOURCE_FINGER;

    /// A session whose client gets wheel steps as `wheel` says, with a
    /// pointer on a surface; returns the pointer and the surface.
    async fn scrolling(wheel: WheelClient) -> (Session, Object, Object) {
        let mut s = Session::with_context(Context::new(
            Rc::new(Settings::for_test()),
            ConnectionFacts {
                wheel,
                ..ConnectionFacts::for_test()
            },
        ));
        let seat = s.bind_paired("wl_seat", wl_seat::VERSION).await;
        let surface = s.surface().await;
        let pointer = s.pointer_on(seat.id, surface).await;
        (s, pointer, surface)
    }

    /// Host pointer `p` scrolls `units` on `axis` from `source` in one
    /// frame, and the guest gets it with `steps`.
    async fn scroll(
        s: &mut Session,
        p: Object,
        source: u32,
        axis: u32,
        units: i32,
        steps: Option<i32>,
    ) {
        s.host([
            wl_pointer::evt::axis_source(p.host, source),
            wl_pointer::evt::axis(p.host, 7, axis, units * 256),
            wl_pointer::evt::frame(p.host),
        ])
        .await;
        let source = wl_pointer::Event::AxisSource {
            axis_source: source,
        };
        let discrete = steps.map(|discrete| wl_pointer::Event::AxisDiscrete { axis, discrete });
        let value = units * 256;
        let axis = wl_pointer::Event::Axis {
            time: 7,
            axis,
            value,
        };
        let expected: Vec<(u32, AnyEvent)> = [source]
            .into_iter()
            .chain(discrete)
            .chain([axis, wl_pointer::Event::Frame {}])
            .map(|event| (p.id, event.into()))
            .collect();
        assert_eq!(s.guest_received(), expected, "{units}");
    }

    /// Touchpad scrolling reaches a client with wheel steps as one
    /// `axis_discrete` per 10 units crossed, before the `axis`; the rest
    /// carries over, per axis, until an `axis_stop`, `leave` or `enter`.
    #[tokio::test]
    async fn a_wine_client_gets_wheel_steps_from_touchpad_scrolling() {
        let (mut s, p, surface) = scrolling(WheelClient::Yes).await;
        scroll(&mut s, p, FINGER, VERTICAL, 6, None).await;
        scroll(&mut s, p, FINGER, VERTICAL, 9, Some(1)).await; // 15: 5 left
        scroll(&mut s, p, FINGER, VERTICAL, 25, Some(3)).await; // 30: none left
        scroll(&mut s, p, FINGER, HORIZONTAL, -12, Some(-1)).await; // -2 left
        scroll(&mut s, p, FINGER, VERTICAL, 8, None).await; // 8 left

        s.host([
            wl_pointer::evt::axis_stop(p.host, 7, HORIZONTAL),
            wl_pointer::evt::frame(p.host),
        ])
        .await;
        let stop = wl_pointer::Event::AxisStop {
            time: 7,
            axis: HORIZONTAL,
        };
        let frame = wl_pointer::Event::Frame {};
        assert_eq!(
            s.guest_received(),
            [(p.id, stop.into()), (p.id, frame.into())]
        );
        scroll(&mut s, p, FINGER, HORIZONTAL, -9, None).await; // -11 had it kept -2

        s.host([
            wl_pointer::evt::leave(p.host, 2, surface.host),
            wl_pointer::evt::enter(p.host, 3, surface.host, 0, 0),
        ])
        .await;
        assert_eq!(s.guest_received().len(), 2);
        scroll(&mut s, p, FINGER, VERTICAL, 8, None).await; // 16 had it kept 8
    }

    /// A client without wheel steps, and a wheel, get the scroll as the
    /// host sent it.
    #[tokio::test]
    async fn wheel_steps_leave_other_clients_and_wheels_alone() {
        let wheel = wl_pointer::AXIS_SOURCE_WHEEL;
        for (client, source) in [(WheelClient::No, FINGER), (WheelClient::Yes, wheel)] {
            let (mut s, p, _) = scrolling(client).await;
            scroll(&mut s, p, source, VERTICAL, 25, None).await;
        }
    }

    /// A client whose process is still being read scrolls as the host
    /// sent it, and nothing waits for the read: its first scroll after the
    /// answer settles whether it gets steps.
    #[tokio::test]
    async fn a_pending_detection_leaves_scrolls_alone_until_it_answers() {
        let (answer, wine) = std::sync::mpsc::sync_channel(1);
        let (mut s, p, _) = scrolling(WheelClient::Pending(wine)).await;
        scroll(&mut s, p, FINGER, VERTICAL, 25, None).await;
        answer.send(true).expect("the session waits");
        scroll(&mut s, p, FINGER, VERTICAL, 25, Some(2)).await;
    }

    /// The stepper is the arithmetic alone: a finger or continuous axis
    /// crosses whole steps, signed like the scroll, and keeps the rest per
    /// axis until a stop; a wheel's axis, or one with no source this
    /// frame, crosses none.
    #[test]
    fn a_stepper_carries_the_rest_over_per_axis() {
        let unit = 256;
        let mut stepper = WheelStepper::default();
        assert_eq!(stepper.axis(VERTICAL, 25 * unit), None, "no source");
        stepper.source(wl_pointer::AXIS_SOURCE_CONTINUOUS);
        assert_eq!(stepper.axis(VERTICAL, 25 * unit), Some(2));
        assert_eq!(stepper.axis(HORIZONTAL, -15 * unit), Some(-1));
        assert_eq!(stepper.axis(VERTICAL, 5 * unit), Some(1), "5 were left");
        assert_eq!(stepper.axis(HORIZONTAL, -4 * unit), None, "-9 now");
        stepper.stop(HORIZONTAL);
        assert_eq!(stepper.axis(HORIZONTAL, -4 * unit), None, "-4 now");
        assert_eq!(stepper.axis(HORIZONTAL, -6 * unit), Some(-1));
        stepper.frame();
        assert_eq!(stepper.axis(VERTICAL, 25 * unit), None, "a new frame");
        stepper.source(wl_pointer::AXIS_SOURCE_WHEEL);
        assert_eq!(stepper.axis(VERTICAL, 25 * unit), None, "a wheel");
    }
}
