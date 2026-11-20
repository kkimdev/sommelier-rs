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

//! Host `wl_output` events, their host-only `zaura_output` extension, and
//! the guest's `zxdg_output_v1` extension.
//!
//! The events feed [`crate::state::OutputRegistry`], which owns every
//! output fact; the end of an output also releases its Aura child.
//! `zxdg_output_v1` passes through: on a fractionally scaled display only
//! its logical size describes the output in surface coordinates, which the
//! integer `wl_output.scale` cannot.

use crate::handler::aura_objects;
use crate::handler::Handlers;
use crate::protocols::aura_shell::zaura_output::{self, ZauraOutputHandler};
use crate::protocols::wayland::wl_output;
use crate::protocols::xdg_output_unstable_v1::zxdg_output_manager_v1::ZxdgOutputManagerV1Handler;
use crate::protocols::Iface;
use crate::state::{
    is_valid_transform, Context, EventSender, GuestId, HostId, RequestObject, RequestSender,
};
use crate::wire::Action;

/// Host `wl_output` events feed [`crate::state::OutputRegistry`] and are
/// forwarded unchanged. Events from a released output never get here: the
/// proxy suppresses host events for objects awaiting `delete_id`. Events
/// from an output whose global was removed change nothing.
impl crate::protocols::wayland::wl_output::WlOutputHandler for Handlers {
    fn on_geometry(
        ctx: &mut Context,
        sender: EventSender,
        x: i32,
        y: i32,
        _physical_width: i32,
        _physical_height: i32,
        _subpixel: i32,
        _make: &String,
        _model: &String,
        transform: i32,
    ) -> Action {
        let host_output_id = sender.host;
        let valid_transform = is_valid_transform(transform);
        let live = ctx.outputs.update(host_output_id, |output| {
            output.origin_x = x;
            output.origin_y = y;
            if valid_transform {
                output.panel_transform = transform;
            }
        });
        if live && !valid_transform {
            log::warn!(
                "Ignoring invalid wl_output.transform={} for host output {}",
                transform,
                host_output_id
            );
        }
        Action::Forward
    }

    fn on_mode(
        ctx: &mut Context,
        sender: EventSender,
        flags: u32,
        width: i32,
        height: i32,
        _refresh: i32,
    ) -> Action {
        ctx.outputs.update(sender.host, |output| {
            if flags & wl_output::MODE_CURRENT != 0
                || output.mode_width <= 0
                || output.mode_height <= 0
            {
                output.mode_width = width;
                output.mode_height = height;
            }
        });
        Action::Forward
    }

    fn on_done(ctx: &mut Context, sender: EventSender) -> Action {
        ctx.outputs.commit(sender.host);
        Action::Forward
    }

    fn on_release(ctx: &mut Context, sender: RequestSender) -> Action {
        let guest_output_id = sender.guest;
        let Some(host_output_id) = sender.host_id() else {
            // Only an inert output, whose bind crossed its global's removal,
            // gets here without a host object. Nothing reaches the host; the
            // generated dispatcher completes the destructor locally.
            return Action::Drop;
        };
        // Ending the output releases its Aura child (see `forget_output`),
        // which goes first (see `aura_objects::release`). Returning Forward
        // would put wl_output.release ahead of the queued Aura release, so
        // queue the translated destructor explicitly.
        ctx.guest_destroyed(guest_output_id);
        ctx.send_host(wl_output::req::release(host_output_id));
        Action::Drop
    }
}

/// Forget host output `host_output_id`, which ended (see
/// `crate::state::lifecycle`), and release its Aura child.
pub(crate) fn forget_output(ctx: &mut Context, host_output_id: HostId) {
    if let Some(zaura_output_host_id) = ctx.outputs.release(host_output_id) {
        aura_objects::release(ctx, aura_objects::AuraChild::OUTPUT, zaura_output_host_id);
    }
}

/// `zaura_output` is host-only, so none of its events reach the guest. Its
/// events for an unknown or no longer live output are stale and dropped
/// (see [`crate::state::OutputRegistry::aura_event_target`]).
impl ZauraOutputHandler for Handlers {
    fn on_insets(
        ctx: &mut Context,
        sender: EventSender,
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    ) -> Action {
        let Some(wl_output_id) = ctx.outputs.aura_event_target(sender.host) else {
            return Action::Drop;
        };
        if top < 0 || left < 0 || bottom < 0 || right < 0 {
            // Negative work-area insets are malformed. Clamping them to zero
            // would silently expand the usable area beyond the compositor's
            // advertised output and could place windows outside the work area.
            log::warn!(
                "Ignoring invalid negative zaura_output.insets for host output {}: ({}, {}, {}, {})",
                wl_output_id,
                top,
                left,
                bottom,
                right
            );
            return Action::Drop;
        }
        ctx.outputs.update(wl_output_id, |output| {
            output.insets_top = top;
            output.insets_left = left;
            output.insets_bottom = bottom;
            output.insets_right = right;
        });
        Action::Drop
    }

    fn on_scale(ctx: &mut Context, sender: EventSender, flags: u32, scale: u32) -> Action {
        // Preferred scales describe choices, not the active DP geometry.
        if flags & zaura_output::SCALE_PROPERTY_CURRENT == 0 {
            return Action::Drop;
        }
        let Some(wl_output_id) = ctx.outputs.aura_event_target(sender.host) else {
            return Action::Drop;
        };
        if scale == 0 {
            log::warn!("Ignoring invalid zaura_output.scale=0");
            return Action::Drop;
        }
        ctx.outputs.update(wl_output_id, |output| {
            output.aura_current_scale = Some(scale);
        });
        Action::Drop
    }

    fn on_device_scale_factor(ctx: &mut Context, sender: EventSender, scale: u32) -> Action {
        let Some(wl_output_id) = ctx.outputs.aura_event_target(sender.host) else {
            return Action::Drop;
        };
        if scale == 0 {
            log::warn!("Ignoring invalid zaura_output.device_scale_factor=0");
            return Action::Drop;
        }
        ctx.outputs.update(wl_output_id, |output| {
            output.aura_device_scale_factor = Some(scale);
        });
        Action::Drop
    }

    /// Not used: see [`crate::state::OutputState::panel_transform`].
    fn on_logical_transform(_ctx: &mut Context, _sender: EventSender, _transform: i32) -> Action {
        Action::Drop
    }
}

/// `zxdg_output_manager_v1` is forwarded. The generated code translates
/// the IDs of its `zxdg_output_v1` children and their events, and checks
/// each event against the child's version, which is its manager's.
impl ZxdgOutputManagerV1Handler for Handlers {
    fn on_get_xdg_output(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        output: RequestObject,
    ) -> Action {
        if output.host.host().is_some() {
            return Action::Forward;
        }
        // An inert output (its bind crossed its global's removal) has no
        // host object to extend, so its xdg_output is inert too: a
        // local-only object whose destructor completes in the proxy. It
        // takes its manager's version.
        ctx.shadow_table
            .create_local(id, Iface::ZxdgOutputV1, sender.version);
        Action::Drop
    }
}

/// A multi-monitor fixture for tests: displays docked, described and
/// removed by the host over a whole session, in the order Exo sends their
/// events.
#[cfg(test)]
pub(crate) mod fixture {
    use crate::protocols::aura_shell::{zaura_output, zaura_shell};
    use crate::protocols::wayland::{wl_output, wl_registry, wl_surface};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};

    /// What Exo reports for one display.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Display {
        /// `wl_output.geometry` position in logical screen coordinates.
        pub(crate) origin: (i32, i32),
        /// `wl_output.mode`: native panel pixels.
        pub(crate) mode: (i32, i32),
        /// `wl_output.geometry` panel transform.
        pub(crate) transform: i32,
        /// `zaura_output.device_scale_factor`, in thousandths.
        pub(crate) device_scale: u32,
        /// The bottom `zaura_output.insets`: the shelf.
        pub(crate) shelf: i32,
    }

    /// The built-in 2560x1600 panel at 2x, with a 48 DIP shelf: a
    /// 1280x752 work area at the origin.
    pub(crate) const PANEL: Display = Display {
        origin: (0, 0),
        mode: (2560, 1600),
        transform: 0,
        device_scale: 2000,
        shelf: 48,
    };

    /// A 1920x1080 external monitor at 1x, left of the panel: its work
    /// area starts at x = -1920.
    pub(crate) const EXTERNAL: Display = Display {
        origin: (-1920, 0),
        mode: (1920, 1080),
        transform: 0,
        device_scale: 1000,
        shelf: 0,
    };

    /// A guest `wl_output`, its global's name and the host-only
    /// `zaura_output` the proxy gave its host object.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct Docked {
        pub(crate) name: u32,
        pub(crate) output: Object,
        pub(crate) aura_output: u32,
    }

    /// The host advertises a `wl_output` global, which the guest binds at
    /// `version`; the proxy gives it a `zaura_output` from `shell`, the host
    /// `zaura_shell` it bound. The host has sent no metadata yet.
    pub(crate) async fn dock(s: &mut Session, shell: u32, version: u32) -> Docked {
        let registry = s.registry().await;
        let name = s.advertise(wl_output::NAME, wl_output::VERSION).await;
        advertised(s, registry.id, name, version);
        bind(s, shell, registry.id, name, version).await
    }

    /// [`dock`] through `registry`, another guest registry, on which the
    /// host advertises global `name`.
    pub(crate) async fn dock_on(
        s: &mut Session,
        shell: u32,
        registry: Object,
        name: u32,
        version: u32,
    ) -> Docked {
        let global = |registry| {
            wl_registry::evt::global(registry, name, wl_output::NAME, wl_output::VERSION).unwrap()
        };
        s.host([global(registry.host)]).await;
        advertised(s, registry.id, name, version);
        bind(s, shell, registry.id, name, version).await
    }

    /// The guest gets output global `name` on `registry`, at `version` or
    /// newer.
    fn advertised(s: &mut Session, registry: u32, name: u32, version: u32) {
        let guest = s.guest_received();
        let [(
            sender,
            AnyEvent::WlRegistry(wl_registry::Event::Global {
                name: advertised_name,
                ref interface,
                version: advertised,
            }),
        )] = guest[..]
        else {
            panic!("the output was not advertised: {guest:?}");
        };
        assert_eq!(
            (sender, advertised_name, interface.as_str()),
            (registry, name, wl_output::NAME)
        );
        assert!(version <= advertised, "{guest:?}");
    }

    /// The guest binds output global `name` through `registry`.
    async fn bind(s: &mut Session, shell: u32, registry: u32, name: u32, version: u32) -> Docked {
        let id = s.new_id();
        s.guest([wl_registry::req::bind(registry, name, wl_output::NAME, version, id).unwrap()])
            .await;
        let host = s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: bound, .. })), (
            aura_shell,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput {
                id: aura_output,
                output,
            }),
        )] = &host[..]
        else {
            panic!("the output got no Aura output: {host:?}");
        };
        assert_eq!((*aura_shell, *output), (shell, bound.2));
        Docked {
            name,
            output: Object { id, host: bound.2 },
            aura_output: *aura_output,
        }
    }

    /// The host sends one Exo display-metrics group for `docked`, a
    /// version 2 or newer output; the guest gets its output's events.
    pub(crate) async fn send_metrics(s: &mut Session, docked: Docked, display: Display) {
        let (output, aura) = (docked.output, docked.aura_output);
        let (x, y) = display.origin;
        let (width, height) = display.mode;
        let current = zaura_output::SCALE_PROPERTY_CURRENT;
        s.host([
            wl_output::evt::geometry(output.host, x, y, 0, 0, 0, "", "", display.transform)
                .unwrap(),
            wl_output::evt::mode(output.host, wl_output::MODE_CURRENT, width, height, 60),
            zaura_output::evt::scale(aura, current, 1000),
            zaura_output::evt::device_scale_factor(aura, display.device_scale),
            zaura_output::evt::insets(aura, 0, 0, display.shelf, 0),
            wl_output::evt::done(output.host),
        ])
        .await;
        let guest = s.guest_received();
        assert!(
            matches!(
                &guest[..],
                [.., (sender, AnyEvent::WlOutput(wl_output::Event::Done {}))]
                    if *sender == output.id
            ) && guest.iter().all(|(sender, _)| *sender == output.id),
            "the guest did not get its output's metadata: {guest:?}"
        );
    }

    /// Exo moving `surface` to `to`: `leave(from)`, then `enter(to)`,
    /// which the guest gets.
    pub(crate) async fn move_surface(
        s: &mut Session,
        surface: Object,
        from: Option<Docked>,
        to: Docked,
    ) {
        let mut events = Vec::new();
        let mut expected = Vec::new();
        if let Some(from) = from {
            events.push(wl_surface::evt::leave(surface.host, from.output.host));
            let leave = wl_surface::Event::Leave {
                output: from.output.id,
            };
            expected.push((surface.id, leave.into()));
        }
        events.push(wl_surface::evt::enter(surface.host, to.output.host));
        let enter = wl_surface::Event::Enter {
            output: to.output.id,
        };
        expected.push((surface.id, enter.into()));
        s.host(events).await;
        assert_eq!(s.guest_received(), expected);
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{dock_on, move_surface, send_metrics, Docked, EXTERNAL, PANEL};
    use crate::geometry::Rect;
    use crate::handler::window_placement::fixture::{Desk, Window};
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell};
    use crate::protocols::wayland::{wl_display, wl_output, wl_registry};
    use crate::proxy::harness::Object;
    use crate::wire::{Outgoing, WL_DISPLAY_ID};

    /// Placement on, with `<Alt>q` placing the focused window on the whole
    /// work area.
    fn fullscreen() -> PlacementConfig {
        PlacementConfig::active_for_test(&[("<Alt>q", "fullscreen")])
    }

    /// A [`Desk`] whose only output the test describes, with a window the
    /// keyboard focuses on the desk's surface, which has entered no output.
    /// Each method sends one host event of the output.
    struct Screen {
        d: Desk,
        w: Window,
        docked: Docked,
    }

    impl Screen {
        /// The output bound at `version`, of whose metadata the host has
        /// sent nothing.
        async fn new(version: u32) -> Self {
            Self::with_shell(version, zaura_shell::VERSION).await
        }

        /// [`Self::new`] under a host `zaura_shell` of `shell_version`,
        /// which the Aura output takes.
        async fn with_shell(version: u32, shell_version: u32) -> Self {
            let mut d = Desk::with_shell(fullscreen(), shell_version).await;
            let docked = d.dock(version).await;
            let w = Window::new(&mut d).await;
            d.focus().await;
            Self { d, w, docked }
        }

        /// The host sends `event` of the `wl_output`; returns whether the
        /// guest got it.
        async fn output_event(&mut self, event: Outgoing<u32>) -> bool {
            self.d.s.host([event]).await;
            assert_eq!(self.d.s.host_received(), []);
            let guest = self.d.s.guest_received();
            assert!(
                guest.len() <= 1 && guest.iter().all(|(id, _)| *id == self.docked.output.id),
                "{guest:?}"
            );
            !guest.is_empty()
        }

        /// The host sends `event` of the host-only `zaura_output`, which
        /// neither peer hears of.
        async fn aura_event(&mut self, event: Outgoing<u32>) {
            self.d.s.host([event]).await;
            assert_eq!(self.d.s.host_received(), []);
            assert_eq!(self.d.s.guest_received(), []);
        }

        /// `wl_output.geometry` at `origin`, with the panel `transform`.
        async fn geometry(&mut self, origin: (i32, i32), transform: i32) -> bool {
            let (x, y) = origin;
            let host = self.docked.output.host;
            let event = wl_output::evt::geometry(host, x, y, 0, 0, 0, "", "", transform);
            self.output_event(event.unwrap()).await
        }

        /// The current `wl_output.mode`.
        async fn mode(&mut self, width: i32, height: i32) -> bool {
            let host = self.docked.output.host;
            let event = wl_output::evt::mode(host, wl_output::MODE_CURRENT, width, height, 60);
            self.output_event(event).await
        }

        async fn done(&mut self) -> bool {
            let host = self.docked.output.host;
            self.output_event(wl_output::evt::done(host)).await
        }

        /// `zaura_output.scale` with `flags`.
        async fn scale(&mut self, flags: u32, scale: u32) {
            let aura = self.docked.aura_output;
            self.aura_event(zaura_output::evt::scale(aura, flags, scale))
                .await;
        }

        async fn device_scale(&mut self, factor: u32) {
            let aura = self.docked.aura_output;
            self.aura_event(zaura_output::evt::device_scale_factor(aura, factor))
                .await;
        }

        async fn insets(&mut self, top: i32, left: i32, bottom: i32, right: i32) {
            let aura = self.docked.aura_output;
            self.aura_event(zaura_output::evt::insets(aura, top, left, bottom, right))
                .await;
        }

        async fn logical_transform(&mut self, transform: i32) {
            let aura = self.docked.aura_output;
            self.aura_event(zaura_output::evt::logical_transform(aura, transform))
                .await;
        }

        /// The work area, as a placement on all of it shows, or `None` if
        /// no output can place the window.
        async fn work_area(&mut self) -> Option<Rect> {
            self.d.placed(&self.w).await
        }

        /// The host removes the output's global; the guest is told, and the
        /// proxy releases the Aura output.
        async fn remove(&mut self) {
            let registry = self.d.s.registry().await;
            let name = self.docked.name;
            self.d
                .s
                .host([wl_registry::evt::global_remove(registry.host, name)])
                .await;
            let removed = wl_registry::Event::GlobalRemove { name };
            assert_eq!(self.d.s.guest_received(), [(registry.id, removed.into())]);
            let aura = self.docked.aura_output;
            assert_eq!(
                self.d.s.host_received(),
                [(aura, zaura_output::Request::Release {}.into())]
            );
        }

        /// The guest releases the output; returns what the host got.
        async fn release(&mut self) -> Vec<(u32, crate::protocols::AnyRequest)> {
            let output = self.docked.output.id;
            self.d.s.guest([wl_output::req::release(output)]).await;
            assert_eq!(self.d.s.guest_received(), []);
            self.d.s.host_received()
        }

        /// The host sends a placeable 1920x1080 output's metadata at 1x.
        async fn placeable(&mut self) {
            self.mode(1920, 1080).await;
            self.scale(CURRENT, 1000).await;
            self.device_scale(1000).await;
            assert!(self.done().await);
        }
    }

    use zaura_output::{SCALE_PROPERTY_CURRENT as CURRENT, SCALE_PROPERTY_PREFERRED as PREFERRED};

    #[tokio::test]
    async fn fractional_aura_scale_defines_logical_work_area() {
        let mut output = Screen::new(3).await;
        output.mode(3840, 2160).await;
        output.scale(CURRENT, 1000).await;
        output.device_scale(1250).await;
        output.done().await;

        assert_eq!(
            output.work_area().await,
            Some(Rect::new(0, 0, 3072, 1728)),
            "Aura bounds use fractional DP scale, not rounded wl_output.scale"
        );
    }

    /// Aura's scales take effect at `done`; a preferred scale, or a zero
    /// factor, changes nothing, even at a later `done`.
    #[tokio::test]
    async fn aura_scale_updates_wait_for_done_and_ignore_preferred_or_zero_factors() {
        let mut output = Screen::new(3).await;
        output.mode(3840, 2160).await;
        output.done().await;

        output.device_scale(0).await;
        output.scale(CURRENT, 0).await;
        output.done().await;
        assert_eq!(output.work_area().await, None);

        output.device_scale(1250).await;
        output.done().await;
        assert_eq!(output.work_area().await, None);

        output.scale(CURRENT, 1000).await;
        assert_eq!(output.work_area().await, None);
        output.done().await;
        assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 3072, 1728)));

        output.scale(PREFERRED, 1200).await;
        output.device_scale(0).await;
        output.scale(CURRENT, 0).await;
        output.done().await;
        assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 3072, 1728)));

        output.scale(PREFERRED | CURRENT, 1200).await;
        assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 3072, 1728)));
        output.done().await;
        assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 2560, 1440)));
    }

    /// Aura scales of an output the proxy does not know, or no longer
    /// does, describe nothing: an output whose scales were all it lacked
    /// stays unplaceable after they come.
    #[tokio::test]
    async fn aura_scale_events_ignore_unknown_removed_and_released_outputs() {
        #[derive(Debug)]
        enum Retired {
            Unknown,
            Removed,
            Released,
        }
        for retired in [Retired::Unknown, Retired::Removed, Retired::Released] {
            let mut output = Screen::new(3).await;
            output.mode(3840, 2160).await;
            assert!(output.done().await);
            match retired {
                // The events come from an Aura output the proxy never had.
                Retired::Unknown => output.docked.aura_output = 0xff00_0042,
                Retired::Removed => output.remove().await,
                Retired::Released => {
                    let (aura, host) = (output.docked.aura_output, output.docked.output.host);
                    assert_eq!(
                        output.release().await,
                        [
                            (aura, zaura_output::Request::Release {}.into()),
                            (host, wl_output::Request::Release {}.into()),
                        ]
                    );
                }
            }
            output.device_scale(1250).await;
            output.scale(CURRENT, 1000).await;
            // The guest still has a removed output, but not a released one.
            assert_eq!(
                output.done().await,
                !matches!(retired, Retired::Released),
                "{retired:?}"
            );
            assert_eq!(output.work_area().await, None, "{retired:?}");
        }
    }

    #[tokio::test]
    async fn aura_scale_events_publish_immediately_for_version_one_output() {
        let mut output = Screen::new(1).await;
        assert!(output.mode(3840, 2160).await);
        assert_eq!(output.work_area().await, None);
        output.device_scale(1250).await;
        output.scale(CURRENT, 1000).await;
        assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 3072, 1728)));
    }

    #[tokio::test]
    async fn output_geometry_and_aura_insets_define_logical_work_area() {
        let mut output = Screen::new(3).await;

        assert!(output.geometry((1920, 16), 0).await);
        assert!(output.mode(3840, 2160).await);
        output.insets(24, 8, 48, 16).await;
        output.scale(CURRENT, 1000).await;
        output.device_scale(2000).await;
        assert_eq!(output.work_area().await, None, "nothing is committed yet");
        assert!(output.done().await);

        assert_eq!(
            output.work_area().await,
            Some(Rect::new(1928, 40, 1896, 1008))
        );
    }

    #[tokio::test]
    async fn negative_aura_insets_are_rejected_without_mutating_output_state() {
        let mut output = Screen::new(3).await;
        output.insets(1, 2, 3, 4).await;
        output.placeable().await;
        let inset = Some(Rect::new(2, 1, 1920 - 6, 1080 - 4));
        assert_eq!(output.work_area().await, inset);

        output.insets(-1, 2, 3, 4).await;
        assert!(output.done().await);
        assert_eq!(output.work_area().await, inset);
    }

    #[tokio::test]
    async fn removed_output_ignores_late_wl_output_and_aura_events() {
        let mut output = Screen::new(3).await;
        output.remove().await;

        // The guest still owns a valid wl_output, so its events are
        // forwarded, but they must not repopulate retired metadata.
        assert!(output.mode(3840, 2160).await);
        output.insets(10, 20, 30, 40).await;
        output.scale(CURRENT, 1000).await;
        output.device_scale(1000).await;
        assert!(output.done().await);
        assert_eq!(output.work_area().await, None);
        // The removal already handed over the Aura child.
        let host = output.docked.output.host;
        assert_eq!(
            output.release().await,
            [(host, wl_output::Request::Release {}.into())]
        );
    }

    #[tokio::test]
    async fn work_area_rotates_native_mode_by_the_panel_transform() {
        // (native mode, panel transform, logical transform, work area). The
        // transforms are the wire values Exo sends: its `OutputTransform`
        // inverts the rotation, so ROTATE_90 is `transform.270` (3).
        for (mode, panel, logical, expected) in [
            // A landscape panel at rotation 90: both transforms agree.
            ((1920, 1080), 3, 3, Rect::new(0, 0, 1080, 1920)),
            // A portrait-native panel (`kRightUp`) at rotation 0: its panel
            // rotation is ROTATE_90 (wire value 3) and its logical rotation
            // ROTATE_0 (0), and the screen is landscape. The logical
            // transform alone left it portrait.
            ((1600, 2560), 3, 0, Rect::new(0, 0, 2560, 1600)),
            // The same panel at rotation 90: panel ROTATE_180 (wire value 2),
            // logical ROTATE_90 (wire value 3).
            ((1600, 2560), 2, 3, Rect::new(0, 0, 1600, 2560)),
        ] {
            // One Exo display-metrics group.
            let mut output = Screen::new(3).await;
            output.geometry((0, 0), panel).await;
            output.mode(mode.0, mode.1).await;
            output.scale(CURRENT, 1000).await;
            output.device_scale(1000).await;
            output.logical_transform(logical).await;
            output.done().await;
            assert_eq!(
                output.work_area().await,
                Some(expected),
                "mode {mode:?}, panel {panel}, logical {logical}"
            );
        }
    }

    /// Releasing a wl_output forgets it, so placement no longer picks it,
    /// and releases its Aura output first. Before `zaura_output.release` the
    /// Aura output has no destructor: it is retired without a request, and
    /// its ID stays the host's.
    #[tokio::test]
    async fn releasing_output_removes_it_from_placement_selection() {
        for (shell_version, aura_release) in [
            (zaura_output::REQ_RELEASE_SINCE, true),
            (zaura_output::REQ_RELEASE_SINCE - 1, false),
        ] {
            let mut output = Screen::with_shell(3, shell_version).await;
            output.placeable().await;
            assert_eq!(output.work_area().await, Some(Rect::new(0, 0, 1920, 1080)));

            let Docked {
                output: wl_output,
                aura_output,
                ..
            } = output.docked;
            let mut expected = vec![(wl_output.host, wl_output::Request::Release {}.into())];
            if aura_release {
                expected.insert(0, (aura_output, zaura_output::Request::Release {}.into()));
            }
            assert_eq!(output.release().await, expected, "v{shell_version}");
            assert_eq!(output.work_area().await, None, "v{shell_version}");

            // The host has not deleted the Aura output, so an object the
            // proxy creates meanwhile takes another ID.
            let s = &mut output.d.s;
            let callback = s.new_id();
            let sync = wl_display::req::sync(WL_DISPLAY_ID, callback);
            assert_ne!(s.create(sync).await, aura_output, "v{shell_version}");

            // The guest's output waits for the host's delete_id.
            s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, wl_output.host)])
                .await;
            let deleted = wl_display::Event::DeleteId { id: wl_output.id };
            assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        }
    }

    /// An output released after its global's removal, and one released
    /// while its global lives, leave no record: the session's check that
    /// no state outlives its object holds after each release.
    #[tokio::test]
    async fn released_outputs_leave_no_record() {
        let mut d = Desk::bare(fullscreen()).await;
        let removed = d.dock(3).await;
        let live = d.dock(3).await;
        let registry = d.s.registry().await;
        d.s.host([wl_registry::evt::global_remove(registry.host, removed.name)])
            .await;
        let global_remove = wl_registry::Event::GlobalRemove { name: removed.name };
        assert_eq!(d.s.guest_received(), [(registry.id, global_remove.into())]);
        let aura_release = || zaura_output::Request::Release {}.into();
        assert_eq!(d.s.host_received(), [(removed.aura_output, aura_release())]);

        let release = || wl_output::Request::Release {}.into();
        d.s.guest([wl_output::req::release(removed.output.id)])
            .await;
        assert_eq!(d.s.host_received(), [(removed.output.host, release())]);
        d.s.guest([wl_output::req::release(live.output.id)]).await;
        assert_eq!(
            d.s.host_received(),
            [
                (live.aura_output, aura_release()),
                (live.output.host, release()),
            ]
        );
        assert_eq!(d.s.guest_received(), []);
    }

    mod session {
        use crate::protocols::wayland::{wl_display, wl_output, wl_registry};
        use crate::protocols::xdg_output_unstable_v1::{zxdg_output_manager_v1, zxdg_output_v1};
        use crate::proxy::harness::{Object, Session};
        use crate::wire::WL_DISPLAY_ID;

        /// How many IDs the guest skips before its first object. The proxy
        /// allocates host IDs densely from the bottom, so without the gap
        /// each guest object could get its own ID on the host too, and a
        /// forward that kept the guest ID would pass for a translated one.
        const GUEST_ID_LEAD: u32 = 10;

        /// A session whose guest IDs run [`GUEST_ID_LEAD`] ahead of the
        /// host's.
        fn session() -> Session {
            let mut s = Session::new();
            for _ in 0..GUEST_ID_LEAD {
                s.new_id();
            }
            s
        }

        /// The host advertises global `interface` at `version`, which the
        /// guest is told of; returns its name.
        async fn advertise(s: &mut Session, interface: &str, version: u32) -> u32 {
            let name = s.advertise(interface, version).await;
            let registry = s.registry().await;
            let global = wl_registry::Event::Global {
                name,
                interface: interface.to_string(),
                version,
            };
            assert_eq!(s.guest_received(), [(registry.id, global.into())]);
            assert_eq!(s.host_received(), []);
            name
        }

        /// The guest binds global `interface` at `version`, which the host
        /// advertises first; returns the guest object and its host object.
        async fn bind(s: &mut Session, interface: &str, version: u32) -> Object {
            let name = advertise(s, interface, version).await;
            let registry = s.registry().await;
            let id = s.new_id();
            let bind = wl_registry::req::bind(registry.id, name, interface, version, id).unwrap();
            let host = s.create(bind).await;
            assert_ne!(host, id);
            Object { id, host }
        }

        /// A `wl_output` the guest binds after the host removed its global,
        /// as when the bind and the removal cross: an inert output.
        async fn inert_output(s: &mut Session) -> u32 {
            let name = advertise(s, "wl_output", 3).await;
            let registry = s.registry().await;
            s.host([wl_registry::evt::global_remove(registry.host, name)])
                .await;
            let removed = wl_registry::Event::GlobalRemove { name };
            assert_eq!(s.guest_received(), [(registry.id, removed.into())]);
            let output = s.new_id();
            s.guest([wl_registry::req::bind(registry.id, name, "wl_output", 3, output).unwrap()])
                .await;
            assert_eq!(s.host_received(), []);
            output
        }

        /// A bound output's release reaches the host, which acknowledges it;
        /// the proxy sends no `delete_id` of its own.
        #[tokio::test]
        async fn bound_output_release_reaches_the_host_without_a_local_delete_id() {
            let mut s = session();
            let output = bind(&mut s, "wl_output", 3).await;

            s.guest([wl_output::req::release(output.id)]).await;
            assert_eq!(
                s.host_received(),
                [(output.host, wl_output::Request::Release {}.into())]
            );
            assert_eq!(s.guest_received(), []);

            s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, output.host)])
                .await;
            let deleted = wl_display::Event::DeleteId { id: output.id };
            assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        }

        /// An inert output (its bind crossed its global's removal) has no
        /// host object, so its release completes in the proxy with one
        /// `delete_id`, which frees its ID.
        #[tokio::test]
        async fn inert_output_release_completes_locally() {
            let mut s = session();
            let output = inert_output(&mut s).await;

            s.guest([wl_output::req::release(output)]).await;
            assert_eq!(s.host_received(), []);
            let deleted = wl_display::Event::DeleteId { id: output };
            assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);

            // The ID is free for the guest's next object.
            s.create(wl_display::req::sync(WL_DISPLAY_ID, output)).await;
        }

        /// The xdg_output of a live output gets a host object; its events
        /// reach the guest translated, at its manager's version.
        #[tokio::test]
        async fn xdg_output_passes_through_with_translated_ids_and_versions() {
            let mut s = session();
            let manager = bind(&mut s, "zxdg_output_manager_v1", 1).await;
            let output = bind(&mut s, "wl_output", 3).await;
            let xdg_output = s.new_id();

            let get = zxdg_output_manager_v1::req::get_xdg_output;
            s.guest([get(manager.id, xdg_output, output.id)]).await;
            let forwarded = s.host_received();
            let [(_, forward)] = &forwarded[..] else {
                panic!("the host did not get one request: {forwarded:?}");
            };
            let host_xdg_output = forward.new_object().expect("an xdg_output").0;
            assert_ne!(host_xdg_output, xdg_output);
            let get = zxdg_output_manager_v1::Request::GetXdgOutput {
                id: host_xdg_output,
                output: output.host,
            };
            assert_eq!(forwarded, [(manager.host, get.into())]);

            // A 2560x1600 panel at 1.6x is 1600x1000 in surface coordinates.
            s.host([zxdg_output_v1::evt::logical_size(
                host_xdg_output,
                1600,
                1000,
            )])
            .await;
            let size = zxdg_output_v1::Event::LogicalSize {
                width: 1600,
                height: 1000,
            };
            assert_eq!(s.guest_received(), [(xdg_output, size.into())]);

            // `description` is a v2 event, which a v1 xdg_output never gets:
            // a host that sends it breaks the protocol.
            let description = zxdg_output_v1::evt::description(host_xdg_output, "Built-in display");
            s.host_fatal([description.unwrap()]).await;
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
            );
        }

        /// The xdg_output of an inert output is inert too: nothing reaches
        /// the host, and its destroy completes in the proxy with one
        /// `delete_id`.
        #[tokio::test]
        async fn xdg_output_of_an_inert_output_is_inert() {
            let mut s = session();
            let manager = bind(&mut s, "zxdg_output_manager_v1", 3).await;
            let output = inert_output(&mut s).await;
            let xdg_output = s.new_id();

            let get = zxdg_output_manager_v1::req::get_xdg_output;
            s.guest([get(manager.id, xdg_output, output)]).await;
            // No event can show an inert object's version.
            let table = &s.ctx().shadow_table;
            let xdg_output = crate::state::GuestId::for_test(xdg_output);
            assert_eq!(
                table.get_interface(xdg_output),
                Some(crate::protocols::Iface::ZxdgOutputV1)
            );
            assert_eq!(table.guest_object_version(xdg_output), Some(3));

            s.guest([zxdg_output_v1::req::destroy(crate::wire::WireId::wire(
                xdg_output,
            ))])
            .await;
            assert_eq!(s.host_received(), []);
            let deleted = wl_display::Event::DeleteId {
                id: crate::wire::WireId::wire(xdg_output),
            };
            assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        }
    }

    /// Dock the panel and an external monitor, move a window onto the
    /// external one, undock it, and redock it under a new global name, as
    /// Exo does when a dock is unplugged and plugged back in.
    #[tokio::test]
    async fn hotplug_moves_placement_between_outputs() {
        let mut d = Desk::bare(fullscreen()).await;
        let panel = d.dock(3).await;
        send_metrics(&mut d.s, panel, PANEL).await;
        let external = d.dock(3).await;
        send_metrics(&mut d.s, external, EXTERNAL).await;
        let w = Window::new(&mut d).await;
        d.focus().await;
        let surface = d.surface;
        let panel_area = Some(Rect::new(0, 0, 1280, 752));
        let external_area = Some(Rect::new(-1920, 0, 1920, 1080));
        assert_eq!(d.placed(&w).await, panel_area);
        move_surface(&mut d.s, surface, None, external).await;
        assert_eq!(d.placed(&w).await, external_area);

        // Undock: Exo moves the window back, then removes the global.
        move_surface(&mut d.s, surface, Some(external), panel).await;
        let registry = d.s.registry().await;
        d.s.host([wl_registry::evt::global_remove(
            registry.host,
            external.name,
        )])
        .await;
        let removed = wl_registry::Event::GlobalRemove {
            name: external.name,
        };
        assert_eq!(d.s.guest_received(), [(registry.id, removed.into())]);
        assert_eq!(
            d.s.host_received(),
            [(
                external.aura_output,
                zaura_output::Request::Release {}.into()
            )]
        );
        assert_eq!(d.placed(&w).await, panel_area);

        // Redock under a new name: a new binding, metadata and placement.
        let redocked = d.dock(3).await;
        assert_ne!(redocked.output.host, external.output.host);
        send_metrics(&mut d.s, redocked, EXTERNAL).await;
        move_surface(&mut d.s, surface, Some(panel), redocked).await;
        assert_eq!(d.placed(&w).await, external_area);
    }

    /// One output bound through two registries is two host objects, each
    /// with its own metadata; its removal forgets both at once.
    #[tokio::test]
    async fn an_output_bound_through_two_registries_is_two_outputs() {
        let mut d = Desk::bare(fullscreen()).await;
        let first = d.dock(3).await;
        let id = d.s.new_id();
        let host =
            d.s.create(wl_display::req::get_registry(WL_DISPLAY_ID, id))
                .await;
        let registries = [d.s.registry().await, Object { id, host }];
        let shell = d.shell;
        let second = dock_on(&mut d.s, shell, registries[1], first.name, 3).await;
        assert_ne!(first.output.host, second.output.host);
        assert_ne!(first.aura_output, second.aura_output);

        send_metrics(&mut d.s, first, PANEL).await;
        send_metrics(&mut d.s, second, EXTERNAL).await;
        let w = Window::new(&mut d).await;
        d.focus().await;
        let surface = d.surface;
        move_surface(&mut d.s, surface, None, first).await;
        assert_eq!(d.placed(&w).await, Some(Rect::new(0, 0, 1280, 752)));
        move_surface(&mut d.s, surface, Some(first), second).await;
        assert_eq!(d.placed(&w).await, Some(Rect::new(-1920, 0, 1920, 1080)));

        let release = || zaura_output::Request::Release {}.into();
        for (registry, released) in registries.into_iter().zip([
            vec![
                (first.aura_output, release()),
                (second.aura_output, release()),
            ],
            vec![],
        ]) {
            let name = first.name;
            d.s.host([wl_registry::evt::global_remove(registry.host, name)])
                .await;
            let removed = wl_registry::Event::GlobalRemove { name };
            assert_eq!(d.s.guest_received(), [(registry.id, removed.into())]);
            assert_eq!(d.s.host_received(), released, "already released");
        }
        assert_eq!(d.placed(&w).await, None);
    }
}
