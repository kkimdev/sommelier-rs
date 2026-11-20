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

use std::collections::BTreeMap;

use crate::protocols::wayland::wl_pointer;
use crate::state::{Context, HostId};
use crate::wire::Action;

/// The scroll distance of one wheel step, in 24.8 fixed-point surface
/// units: Exo's own for one wheel notch (`kWaylandScrollUnit`, 10, in
/// `WaylandPointerDelegate::OnPointerScroll`), so a touchpad scroll as far
/// as one notch scrolls is one step.
const WHEEL_STEP: i64 = 10 * 256;

/// Wheel steps synthesized from touchpad scrolling, for a connection
/// [`crate::client_kind`] chose: Exo sends a touchpad scroll as
/// `wl_pointer.axis` alone, and Wine scrolls only on `axis_discrete`.
/// Each host pointer keeps its frame's `axis_source` and, per axis, the
/// finger or continuous distance not yet sent as a step. The remainder
/// starts again at `axis_stop`, `enter` and `leave`.
#[derive(Debug, Default)]
pub(crate) struct WheelSteps(BTreeMap<HostId, Scroll>);

// A source is known only on a pointer that has `axis_discrete`: the
// dispatcher rejects an event newer than its object.
const _: () = assert!(wl_pointer::EVT_AXIS_SOURCE_SINCE >= wl_pointer::EVT_AXIS_DISCRETE_SINCE);
// The steps are `axis_discrete` events, which a pointer from v8 on, which
// gets `axis_value120` instead, no longer takes: no guest seat is that new.
const _: () =
    assert!(crate::handler::registry::WL_SEAT_VERSION < wl_pointer::EVT_AXIS_VALUE120_SINCE);

#[derive(Debug, Default)]
struct Scroll {
    source: Option<u32>,
    /// Per axis (`AXIS_VERTICAL_SCROLL`, `AXIS_HORIZONTAL_SCROLL`).
    remainder: [i64; 2],
}

impl WheelSteps {
    /// The pointers it follows; every batch checks they are live.
    #[cfg(test)]
    pub(crate) fn pointers_for_test(&self) -> impl Iterator<Item = HostId> + '_ {
        self.0.keys().copied()
    }
}

/// `pointer`'s scroll, if the connection has wheel steps and it scrolled.
fn scroll_of(ctx: &mut Context, pointer: HostId) -> Option<&mut Scroll> {
    ctx.wheel_steps.as_mut()?.0.get_mut(&pointer)
}

/// Start `pointer`'s wheel steps again, if the connection has them.
fn restart_steps(ctx: &mut Context, pointer: HostId) {
    if let Some(steps) = &mut ctx.wheel_steps {
        steps.0.remove(&pointer);
    }
}

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
        restart_steps(ctx, HostId::from_event_sender(ctx));
        Action::Forward
    }

    fn on_leave(&mut self, ctx: &mut Context, _serial: u32, surface: u32) -> Action {
        // The dispatcher resolved the surface, as for `enter`.
        if let Some(guest_surface) = ctx.shadow_table.get_guest_id(surface) {
            ctx.pointer_focus
                .leave(HostId::from_event_sender(ctx), guest_surface);
        }
        restart_steps(ctx, HostId::from_event_sender(ctx));
        Action::Forward
    }

    fn on_axis_source(&mut self, ctx: &mut Context, axis_source: u32) -> Action {
        let pointer = HostId::from_event_sender(ctx);
        if let Some(steps) = &mut ctx.wheel_steps {
            steps.0.entry(pointer).or_default().source = Some(axis_source);
        }
        Action::Forward
    }

    fn on_frame(&mut self, ctx: &mut Context) -> Action {
        let pointer = HostId::from_event_sender(ctx);
        if let Some(scroll) = scroll_of(ctx, pointer) {
            scroll.source = None;
        }
        Action::Forward
    }

    fn on_axis_stop(&mut self, ctx: &mut Context, _time: u32, axis: u32) -> Action {
        let pointer = HostId::from_event_sender(ctx);
        if let Some(remainder) =
            scroll_of(ctx, pointer).and_then(|s| s.remainder.get_mut(axis as usize))
        {
            *remainder = 0;
        }
        Action::Forward
    }

    /// A finger or continuous scroll that crosses [`WHEEL_STEP`]s reaches
    /// a client with [`WheelSteps`] as `axis_discrete`, then the `axis`,
    /// which the protocol says it precedes. A wheel's `axis` comes with
    /// the host's own `axis_discrete`, and passes as it is.
    fn on_axis(&mut self, ctx: &mut Context, time: u32, axis: u32, value: i32) -> Action {
        let pointer = HostId::from_event_sender(ctx);
        let Some(guest_pointer) = ctx.shadow_table.get_guest_id(pointer.0) else {
            return Action::Forward;
        };
        let Some(scroll) = scroll_of(ctx, pointer) else {
            return Action::Forward;
        };
        let continuous = matches!(
            scroll.source,
            Some(wl_pointer::AXIS_SOURCE_FINGER | wl_pointer::AXIS_SOURCE_CONTINUOUS)
        );
        let (true, Some(remainder)) = (continuous, scroll.remainder.get_mut(axis as usize)) else {
            return Action::Forward;
        };
        let distance = *remainder + i64::from(value);
        let steps = distance / WHEEL_STEP;
        *remainder = distance - steps * WHEEL_STEP;
        if steps == 0 {
            return Action::Forward;
        }
        // |steps| <= |value| / WHEEL_STEP + 1, so it fits.
        ctx.host_to_client_queue.extend([
            wl_pointer::evt::axis_discrete(guest_pointer, axis, steps as i32),
            wl_pointer::evt::axis(guest_pointer, time, axis, value),
        ]);
        Action::Drop
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
    use crate::protocols::wayland::{wl_pointer, wl_seat, wl_surface};
    use crate::protocols::AnyEvent;
    use crate::proxy::harness::{Object, Session};
    use crate::state::{Context, HostId};

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

    const VERTICAL: u32 = wl_pointer::AXIS_VERTICAL_SCROLL;
    const HORIZONTAL: u32 = wl_pointer::AXIS_HORIZONTAL_SCROLL;
    const FINGER: u32 = wl_pointer::AXIS_SOURCE_FINGER;

    /// A session whose client gets wheel steps or not, with a pointer on a
    /// surface; returns the pointer and the surface.
    async fn scrolling(wheel_steps: bool) -> (Session, Object, Object) {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        // What `start_client` decides from the client's process.
        ctx.wheel_steps = wheel_steps.then(Default::default);
        let mut s = Session::with_context(ctx);
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
        let (mut s, p, surface) = scrolling(true).await;
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
        for (wheel_steps, source) in [(false, FINGER), (true, wheel)] {
            let (mut s, p, _) = scrolling(wheel_steps).await;
            scroll(&mut s, p, source, VERTICAL, 25, None).await;
        }
    }
}
