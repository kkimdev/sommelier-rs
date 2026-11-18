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
use crate::protocols::aura_shell::zaura_output::{self, ZauraOutputHandler};
use crate::protocols::wayland::wl_output;
use crate::protocols::xdg_output_unstable_v1::zxdg_output_manager_v1::ZxdgOutputManagerV1Handler;
use crate::state::{is_valid_transform, Context};
use crate::wire::Action;

pub struct OutputHandler;

/// Host `wl_output` events feed [`crate::state::OutputRegistry`] and are
/// forwarded unchanged. Events from a released output never get here: the
/// proxy suppresses host events for objects awaiting `delete_id`. Events
/// from an output whose global was removed change nothing.
impl crate::protocols::wayland::wl_output::WlOutputHandler for OutputHandler {
    fn on_geometry(
        &mut self,
        ctx: &mut Context,
        x: i32,
        y: i32,
        _physical_width: i32,
        _physical_height: i32,
        _subpixel: i32,
        _make: &String,
        _model: &String,
        transform: i32,
    ) -> Action {
        let host_output_id = ctx.last_sender_id;
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
        &mut self,
        ctx: &mut Context,
        flags: u32,
        width: i32,
        height: i32,
        _refresh: i32,
    ) -> Action {
        ctx.outputs.update(ctx.last_sender_id, |output| {
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

    fn on_done(&mut self, ctx: &mut Context) -> Action {
        ctx.outputs.commit(ctx.last_sender_id);
        Action::Forward
    }

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        // This is a client→host request, so `last_sender_id` is the guest
        // wl_output ID. Translate it before touching host-side output state or
        // queueing the destructor. Keeping the direction explicit avoids
        // retiring an unrelated host object when the two namespaces differ.
        let guest_output_id = ctx.last_sender_id;
        let Some(host_output_id) = ctx.shadow_table.get_host_id(guest_output_id) else {
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
        ctx.client_to_host_queue
            .push(wl_output::req::release(host_output_id));
        Action::Drop
    }
}

/// Forget host output `host_output_id`, which ended (see
/// `crate::state::lifecycle`), and release its Aura child.
pub(crate) fn forget_output(ctx: &mut Context, host_output_id: u32) {
    if let Some(zaura_output_host_id) = ctx.outputs.release(host_output_id) {
        aura_objects::release(ctx, aura_objects::AuraChild::OUTPUT, zaura_output_host_id);
    }
}

/// `zaura_output` is host-only, so none of its events reach the guest. Its
/// events for an unknown or no longer live output are stale and dropped
/// (see [`crate::state::OutputRegistry::aura_event_target`]).
impl ZauraOutputHandler for OutputHandler {
    fn on_insets(
        &mut self,
        ctx: &mut Context,
        top: i32,
        left: i32,
        bottom: i32,
        right: i32,
    ) -> Action {
        let Some(wl_output_id) = ctx.outputs.aura_event_target(ctx.last_sender_id) else {
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

    fn on_scale(&mut self, ctx: &mut Context, flags: u32, scale: u32) -> Action {
        // Preferred scales describe choices, not the active DP geometry.
        if flags & zaura_output::SCALE_PROPERTY_CURRENT == 0 {
            return Action::Drop;
        }
        let Some(wl_output_id) = ctx.outputs.aura_event_target(ctx.last_sender_id) else {
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

    fn on_device_scale_factor(&mut self, ctx: &mut Context, scale: u32) -> Action {
        let Some(wl_output_id) = ctx.outputs.aura_event_target(ctx.last_sender_id) else {
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
    fn on_logical_transform(&mut self, _ctx: &mut Context, _transform: i32) -> Action {
        Action::Drop
    }
}

/// `zxdg_output_manager_v1` is forwarded. The generated code translates
/// the IDs of its `zxdg_output_v1` children and their events, and checks
/// each event against the child's version, which is its manager's.
impl ZxdgOutputManagerV1Handler for OutputHandler {
    fn on_get_xdg_output(&mut self, ctx: &mut Context, id: u32, output: u32) -> Action {
        if ctx.shadow_table.get_host_id(output).is_some() {
            return Action::Forward;
        }
        // An inert output (its bind crossed its global's removal) has no
        // host object to extend, so its xdg_output is inert too: a
        // local-only object whose destructor completes in the proxy.
        let version = ctx
            .shadow_table
            .guest_object_version_or_unknown(ctx.last_sender_id);
        ctx.shadow_table.create_local(id, "zxdg_output_v1", version);
        Action::Drop
    }
}

/// A multi-monitor fixture for tests: displays docked, described and
/// removed through the real registry and output handlers, in the order
/// Exo sends their events.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use crate::handler::registry::RegistryHandler;
    use crate::protocols::wayland::wl_output::WlOutputHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_surface::WlSurfaceHandler;

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

    /// A context with the ARC bounds policy, guest registries 1 and 2
    /// (hosts 1 and 2), and a v38 host `zaura_shell`, so every output gets
    /// a `zaura_output`.
    pub(crate) fn context() -> Context {
        let mut ctx = Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::arc_bounds_for_test(""),
        );
        for registry in [1, 2] {
            ctx.shadow_table
                .insert_paired_for_test(registry, registry, "wl_registry");
        }
        ctx.last_sender_id = 1;
        let shell = "zaura_shell".to_string();
        assert_eq!(
            RegistryHandler.on_global(&mut ctx, 99, &shell, 38),
            Action::Drop
        );
        ctx
    }

    /// Advertise `display` as `wl_output` global `name` on `registry`, bind
    /// it there as guest output `guest_output`, and send its metrics.
    ///
    /// # Returns
    ///
    /// The host `wl_output`.
    pub(crate) fn dock(
        ctx: &mut Context,
        registry: u32,
        name: u32,
        guest_output: u32,
        display: Display,
    ) -> u32 {
        ctx.last_sender_id = registry;
        let output = "wl_output".to_string();
        RegistryHandler.on_global(ctx, name, &output, 3);
        let bind = (output, 3, guest_output);
        assert_eq!(RegistryHandler.on_bind(ctx, name, &bind), Action::Drop);
        let host_output = ctx.shadow_table.get_host_id(guest_output).expect("bound");
        send_metrics(ctx, host_output, display);
        host_output
    }

    /// Send one Exo display-metrics group for `host_output`.
    pub(crate) fn send_metrics(ctx: &mut Context, host_output: u32, display: Display) {
        let mut handler = OutputHandler;
        let no_name = String::new();
        ctx.last_sender_id = host_output;
        let (x, y) = display.origin;
        let (width, height) = display.mode;
        handler.on_geometry(ctx, x, y, 0, 0, 0, &no_name, &no_name, display.transform);
        handler.on_mode(ctx, wl_output::MODE_CURRENT, width, height, 60);
        ctx.last_sender_id = ctx.outputs.aura_output(host_output).expect("Aura output");
        let current = zaura_output::SCALE_PROPERTY_CURRENT;
        ZauraOutputHandler::on_scale(&mut handler, ctx, current, 1000);
        handler.on_device_scale_factor(ctx, display.device_scale);
        handler.on_insets(ctx, 0, 0, display.shelf, 0);
        ctx.last_sender_id = host_output;
        WlOutputHandler::on_done(&mut handler, ctx);
    }

    /// Exo moving `host_surface` to `to`: `leave(from)`, then `enter(to)`.
    pub(crate) fn move_surface(ctx: &mut Context, host_surface: u32, from: Option<u32>, to: u32) {
        let mut handler = crate::handler::compositor::CompositorHandler;
        ctx.last_sender_id = host_surface;
        if let Some(from) = from {
            WlSurfaceHandler::on_leave(&mut handler, ctx, from);
        }
        WlSurfaceHandler::on_enter(&mut handler, ctx, to);
    }

    /// The work area placement would use for `host_surface`.
    pub(crate) fn placement_area(
        ctx: &Context,
        host_surface: u32,
    ) -> Option<(u32, crate::window_layout::Rect)> {
        let (output, state) = ctx.outputs.for_surface(host_surface)?;
        Some((output, state.work_area()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::aura_shell::zaura_output::{
        REQ_RELEASE as REQ_RELEASE_AURA_OUTPUT, REQ_RELEASE_SINCE as REQ_RELEASE_AURA_OUTPUT_SINCE,
    };
    use crate::protocols::wayland::wl_output::WlOutputHandler;
    use crate::state::{GlobalBinding, OutputState};
    use crate::test_support::{msg_sender, queued_headers};
    use crate::window_layout::Rect;

    const OUTPUT_HOST: u32 = 20;
    const AURA_OUTPUT_HOST: u32 = 21;
    /// The global [`OUTPUT_HOST`] is bound from.
    const GLOBAL: GlobalBinding = GlobalBinding {
        name: 7,
        generation: 1,
    };

    /// Host output [`OUTPUT_HOST`] and its Aura output [`AURA_OUTPUT_HOST`].
    /// Each method delivers one host event, as the proxy dispatches it.
    struct Output(Context);

    impl Output {
        /// The output bound at `version` with `committed` metadata, and an
        /// Aura output at the vendored version.
        fn new(version: u32, committed: OutputState) -> Self {
            Self::with_aura_version(version, committed, zaura_output::VERSION)
        }

        /// [`Self::new`] with an Aura output of `aura_version`.
        fn with_aura_version(version: u32, committed: OutputState, aura_version: u32) -> Self {
            let mut ctx = Context::new_for_test(false, false, vec![]);
            ctx.outputs.bind(OUTPUT_HOST, version, GLOBAL);
            ctx.outputs.update(OUTPUT_HOST, |state| *state = committed);
            ctx.outputs.commit(OUTPUT_HOST);
            ctx.outputs
                .remember_aura_output(OUTPUT_HOST, AURA_OUTPUT_HOST);
            ctx.shadow_table.insert_host_only_for_test(
                AURA_OUTPUT_HOST,
                "zaura_output",
                aura_version,
            );
            Self(ctx)
        }

        fn output(&mut self) -> &mut Context {
            self.0.last_sender_id = OUTPUT_HOST;
            &mut self.0
        }

        fn aura(&mut self) -> &mut Context {
            self.0.last_sender_id = AURA_OUTPUT_HOST;
            &mut self.0
        }

        /// `wl_output.geometry` at `origin`, with the panel `transform`.
        fn geometry(&mut self, origin: (i32, i32), transform: i32) -> Action {
            let name = String::new();
            let (x, y) = origin;
            OutputHandler.on_geometry(self.output(), x, y, 0, 0, 0, &name, &name, transform)
        }

        /// The current `wl_output.mode`.
        fn mode(&mut self, width: i32, height: i32) -> Action {
            OutputHandler.on_mode(self.output(), wl_output::MODE_CURRENT, width, height, 60)
        }

        fn done(&mut self) -> Action {
            WlOutputHandler::on_done(&mut OutputHandler, self.output())
        }

        /// `zaura_output.scale` with `flags`.
        fn scale(&mut self, flags: u32, scale: u32) -> Action {
            ZauraOutputHandler::on_scale(&mut OutputHandler, self.aura(), flags, scale)
        }

        fn device_scale(&mut self, factor: u32) -> Action {
            OutputHandler.on_device_scale_factor(self.aura(), factor)
        }

        fn insets(&mut self, top: i32, left: i32, bottom: i32, right: i32) -> Action {
            OutputHandler.on_insets(self.aura(), top, left, bottom, right)
        }

        fn logical_transform(&mut self, transform: i32) -> Action {
            ZauraOutputHandler::on_logical_transform(&mut OutputHandler, self.aura(), transform)
        }

        fn committed(&self) -> Option<OutputState> {
            self.0.outputs.committed_for_test(OUTPUT_HOST)
        }

        fn work_area(&self) -> Option<Rect> {
            self.committed().expect("live output").work_area()
        }

        fn has_pending(&self) -> bool {
            self.0.outputs.has_pending_for_test()
        }
    }

    use zaura_output::{SCALE_PROPERTY_CURRENT as CURRENT, SCALE_PROPERTY_PREFERRED as PREFERRED};

    #[test]
    fn fractional_aura_scale_defines_logical_work_area() {
        let mut output = Output::new(3, OutputState::default());
        output.mode(3840, 2160);
        output.scale(CURRENT, 1000);
        output.device_scale(1250);
        output.done();

        assert_eq!(
            output.work_area(),
            Some(Rect::new(0, 0, 3072, 1728)),
            "Aura bounds use fractional DP scale, not rounded wl_output.scale"
        );
    }

    #[test]
    fn aura_scale_updates_wait_for_done_and_ignore_preferred_or_zero_factors() {
        let mut output = Output::new(
            3,
            OutputState {
                mode_width: 3840,
                mode_height: 2160,
                ..Default::default()
            },
        );

        assert_eq!(output.device_scale(0), Action::Drop);
        assert_eq!(output.scale(CURRENT, 0), Action::Drop);
        assert!(!output.has_pending());
        assert_eq!(output.work_area(), None);

        output.device_scale(1250);
        output.done();
        assert_eq!(output.work_area(), None);

        output.scale(CURRENT, 1000);
        assert_eq!(output.work_area(), None);
        output.done();
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 3072, 1728)));

        output.scale(PREFERRED, 1200);
        output.device_scale(0);
        output.scale(CURRENT, 0);
        assert!(!output.has_pending());
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 3072, 1728)));

        output.scale(PREFERRED | CURRENT, 1200);
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 3072, 1728)));
        output.done();
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 2560, 1440)));
    }

    #[test]
    fn aura_scale_events_ignore_unknown_removed_and_released_outputs() {
        #[derive(Debug)]
        enum Retired {
            Unknown,
            Removed,
            Released,
        }
        for retired in [Retired::Unknown, Retired::Removed, Retired::Released] {
            let mut output = Output::new(3, OutputState::default());
            match retired {
                Retired::Unknown => output = Output(Context::new_for_test(false, false, vec![])),
                Retired::Removed => {
                    assert_eq!(output.0.outputs.remove_global(GLOBAL), [AURA_OUTPUT_HOST])
                }
                Retired::Released => assert_eq!(
                    output.0.outputs.release(OUTPUT_HOST),
                    Some(AURA_OUTPUT_HOST)
                ),
            }
            assert_eq!(output.device_scale(1250), Action::Drop, "{retired:?}");
            assert_eq!(output.scale(CURRENT, 1000), Action::Drop, "{retired:?}");
            assert!(!output.has_pending(), "{retired:?}");
            assert_eq!(output.committed(), None, "{retired:?}");
        }
    }

    #[test]
    fn aura_scale_events_publish_immediately_for_version_one_output() {
        let mut output = Output::new(1, OutputState::default());
        output.mode(3840, 2160);
        assert_eq!(output.work_area(), None);
        output.device_scale(1250);
        output.scale(CURRENT, 1000);
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 3072, 1728)));
        assert!(!output.has_pending());
    }

    #[test]
    fn output_geometry_and_aura_insets_define_logical_work_area() {
        let mut output = Output::new(3, OutputState::default());

        assert_eq!(output.geometry((1920, 16), 0), Action::Forward);
        assert_eq!(output.mode(3840, 2160), Action::Forward);
        assert_eq!(output.insets(24, 8, 48, 16), Action::Drop);
        output.scale(CURRENT, 1000);
        output.device_scale(2000);
        assert_eq!(output.committed().map(|state| state.mode_width), Some(0));
        assert_eq!(output.done(), Action::Forward);

        assert_eq!(output.work_area(), Some(Rect::new(1928, 40, 1896, 1008)));
    }

    #[test]
    fn negative_aura_insets_are_rejected_without_mutating_output_state() {
        let insets = OutputState {
            insets_top: 1,
            insets_left: 2,
            insets_bottom: 3,
            insets_right: 4,
            ..Default::default()
        };
        let mut output = Output::new(3, insets);

        assert_eq!(output.insets(-1, 2, 3, 4), Action::Drop);
        assert_eq!(output.committed(), Some(insets));
        assert!(!output.has_pending());
    }

    #[test]
    fn removed_output_ignores_late_wl_output_and_aura_events() {
        let mut output = Output::new(3, OutputState::default());
        assert_eq!(output.0.outputs.remove_global(GLOBAL), [AURA_OUTPUT_HOST]);

        // The guest still owns a valid wl_output, so its events are
        // forwarded, but they must not repopulate retired metadata.
        assert_eq!(output.mode(3840, 2160), Action::Forward);
        assert_eq!(output.done(), Action::Forward);
        assert_eq!(output.insets(10, 20, 30, 40), Action::Drop);
        assert!(!output.has_pending());
        assert_eq!(output.committed(), None);
        assert_eq!(
            output.0.outputs.release(OUTPUT_HOST),
            None,
            "the removal already handed over the Aura child"
        );
    }

    #[test]
    fn work_area_rotates_native_mode_by_the_panel_transform() {
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
            let mut output = Output::new(3, OutputState::default());
            output.geometry((0, 0), panel);
            output.mode(mode.0, mode.1);
            output.scale(CURRENT, 1000);
            output.device_scale(1000);
            output.logical_transform(logical);
            output.done();
            assert_eq!(
                output.work_area(),
                Some(expected),
                "mode {mode:?}, panel {panel}, logical {logical}"
            );
        }
    }

    /// Releasing a wl_output forgets it, so placement no longer picks it,
    /// and releases its Aura output first. Before `zaura_output.release` the
    /// Aura output has no destructor: it is retired without a request.
    #[test]
    fn releasing_output_removes_it_from_placement_selection() {
        for (aura_version, aura_release) in [
            (REQ_RELEASE_AURA_OUTPUT_SINCE, true),
            (REQ_RELEASE_AURA_OUTPUT_SINCE - 1, false),
        ] {
            let placeable = OutputState {
                mode_width: 1920,
                mode_height: 1080,
                aura_current_scale: Some(1000),
                aura_device_scale_factor: Some(1000),
                ..Default::default()
            };
            let Output(mut ctx) = Output::with_aura_version(3, placeable, aura_version);
            let output_guest = 10;
            ctx.shadow_table.insert_paired_with_version_for_test(
                output_guest,
                OUTPUT_HOST,
                "wl_output",
                3,
            );
            assert!(ctx.outputs.primary().is_some());
            // wl_output.release is a client request, so the sender is the
            // guest object ID even though the queued destructor targets the
            // host ID.
            ctx.last_sender_id = output_guest;
            assert_eq!(
                WlOutputHandler::on_release(&mut OutputHandler, &mut ctx),
                Action::Drop
            );
            let mut expected = vec![(OUTPUT_HOST, wl_output::REQ_RELEASE)];
            if aura_release {
                expected.insert(0, (AURA_OUTPUT_HOST, REQ_RELEASE_AURA_OUTPUT));
            }
            assert_eq!(queued_headers(&ctx.client_to_host_queue), expected);
            assert!(ctx.outputs.primary().is_none());
            assert!(ctx.outputs.is_empty_for_test());
            assert!(ctx.shadow_table.is_pending_destroy_guest(output_guest));
            assert!(ctx.shadow_table.is_retired_host(AURA_OUTPUT_HOST));
            assert_eq!(ctx.shadow_table.get_host_interface(AURA_OUTPUT_HOST), None);
            assert!(!ctx.shadow_table.is_host_id_available(AURA_OUTPUT_HOST));
        }
    }

    #[test]
    fn released_outputs_leave_no_record() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        // Output 20 lost its global before release; output 30 did not.
        for (output_guest, output_host) in [(10, 20), (11, 30)] {
            ctx.shadow_table.insert_paired_with_version_for_test(
                output_guest,
                output_host,
                "wl_output",
                3,
            );
            ctx.outputs.bind_for_test(output_host, Default::default());
        }
        let removed_aura_outputs = ctx.outputs.remove_global(GlobalBinding {
            name: 20,
            generation: 1,
        });
        assert!(removed_aura_outputs.is_empty());
        for output_guest in [10, 11] {
            ctx.last_sender_id = output_guest;
            assert_eq!(
                WlOutputHandler::on_release(&mut OutputHandler, &mut ctx),
                Action::Drop
            );
        }
        assert!(
            ctx.outputs.is_empty_for_test(),
            "a released output must leave no record behind"
        );
    }

    /// A bound output's release reaches the host, which acknowledges it; the
    /// proxy sends no `delete_id` of its own.
    #[test]
    fn bound_output_release_reaches_the_host_without_a_local_delete_id() {
        use crate::protocols::wayland::wl_output::{dispatch_request, REQ_RELEASE};
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, OUTPUT_HOST, "wl_output", 3);
        ctx.outputs.bind_for_test(OUTPUT_HOST, Default::default());

        ctx.last_sender_id = 10;
        let mut release = crate::wire::WireMessage::new(10, REQ_RELEASE, &[]);
        let dispatched = dispatch_request(&mut release, &mut OutputHandler, &mut ctx);
        assert!(matches!(dispatched, Ok(None)), "{dispatched:?}");
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(OUTPUT_HOST, wl_output::REQ_RELEASE)]
        );
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.shadow_table.is_pending_destroy_guest(10));
    }

    /// An inert output (its bind crossed its global's removal) has no host
    /// object, so its release completes in the proxy with one `delete_id`.
    #[test]
    fn inert_output_release_completes_locally() {
        use crate::protocols::wayland::wl_display::EVT_DELETE_ID;
        use crate::protocols::wayland::wl_output::{dispatch_request, REQ_RELEASE};
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.create_local(10, "wl_output", 3);

        let mut release = crate::wire::WireMessage::new(10, REQ_RELEASE, &[]);
        let dispatched = dispatch_request(&mut release, &mut OutputHandler, &mut ctx);
        assert!(matches!(dispatched, Ok(None)), "{dispatched:?}");
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(1, EVT_DELETE_ID)]
        );
        assert_eq!(ctx.shadow_table.get_interface(10), None);
        assert!(!ctx.is_fatal());
    }

    /// Forwards every `zxdg_output_v1` message, as the proxy's dispatcher
    /// does for an interface without a custom handler.
    struct ForwardXdgOutput;
    impl crate::protocols::xdg_output_unstable_v1::zxdg_output_v1::ZxdgOutputV1Handler
        for ForwardXdgOutput
    {
    }

    /// Guest `zxdg_output_manager_v1` 30 on host 31 at `version`.
    fn bind_xdg_output_manager(ctx: &mut Context, version: u32) {
        ctx.shadow_table.insert_paired_with_version_for_test(
            30,
            31,
            "zxdg_output_manager_v1",
            version,
        );
    }

    /// Dispatch `get_xdg_output(40, output_guest)` from manager 30.
    fn get_xdg_output(ctx: &mut Context, output_guest: u32) -> Option<Vec<u8>> {
        use crate::protocols::xdg_output_unstable_v1::zxdg_output_manager_v1::{
            dispatch_request, REQ_GET_XDG_OUTPUT,
        };
        let payload = [40u32.to_ne_bytes(), output_guest.to_ne_bytes()].concat();
        let mut request = crate::wire::WireMessage::new(30, REQ_GET_XDG_OUTPUT, &payload);
        ctx.last_sender_id = 30;
        let dispatched = dispatch_request(&mut request, &mut OutputHandler, ctx);
        dispatched
            .expect("valid request")
            .map(|(message, _)| message)
    }

    /// The xdg_output of a live output gets a host object; its events
    /// reach the guest translated, at its manager's version.
    #[test]
    fn xdg_output_passes_through_with_translated_ids_and_versions() {
        use crate::protocols::xdg_output_unstable_v1::zxdg_output_v1::{
            self, dispatch_event, EVT_DESCRIPTION, EVT_LOGICAL_SIZE,
        };
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_xdg_output_manager(&mut ctx, 1);
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, OUTPUT_HOST, "wl_output", 3);

        let request = get_xdg_output(&mut ctx, 10).expect("forwarded");
        let host_xdg_output = ctx.shadow_table.get_host_id(40).expect("host object");
        assert_eq!(msg_sender(&request), 31);
        assert_eq!(
            request[8..],
            [host_xdg_output.to_ne_bytes(), OUTPUT_HOST.to_ne_bytes()].concat()
        );

        // A 2560x1600 panel at 1.6x is 1600x1000 in surface coordinates.
        let size = [1600i32.to_ne_bytes(), 1000i32.to_ne_bytes()].concat();
        let mut event = crate::wire::WireMessage::new(host_xdg_output, EVT_LOGICAL_SIZE, &size);
        let (event, _) = dispatch_event(&mut event, &mut ForwardXdgOutput, &mut ctx)
            .expect("valid event")
            .expect("forwarded");
        assert_eq!(msg_sender(&event), 40);
        assert_eq!(event[8..], size);

        // `description` is a v2 event, which a v1 xdg_output never gets.
        let (description, _) =
            zxdg_output_v1::evt::description(host_xdg_output, "Built-in display").unwrap();
        let mut event =
            crate::wire::WireMessage::new(host_xdg_output, EVT_DESCRIPTION, &description[8..]);
        assert!(matches!(
            dispatch_event(&mut event, &mut ForwardXdgOutput, &mut ctx),
            Err(crate::wire::ProtocolError::UnsupportedVersion { .. })
        ));
    }

    /// The xdg_output of an inert output is inert too: nothing reaches the
    /// host, and its destroy completes in the proxy with one `delete_id`.
    #[test]
    fn xdg_output_of_an_inert_output_is_inert() {
        use crate::protocols::wayland::wl_display::EVT_DELETE_ID;
        use crate::protocols::xdg_output_unstable_v1::zxdg_output_v1::{
            dispatch_request, REQ_DESTROY,
        };
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_xdg_output_manager(&mut ctx, 3);
        ctx.shadow_table.create_local(10, "wl_output", 3);

        assert_eq!(get_xdg_output(&mut ctx, 10), None);
        assert_eq!(ctx.shadow_table.get_host_id(40), None);
        assert!(ctx
            .shadow_table
            .request_object(40, "zxdg_output_v1")
            .is_some());
        assert_eq!(ctx.shadow_table.guest_object_version(40), Some(3));

        let mut destroy = crate::wire::WireMessage::new(40, REQ_DESTROY, &[]);
        let dispatched = dispatch_request(&mut destroy, &mut ForwardXdgOutput, &mut ctx);
        assert!(matches!(dispatched, Ok(None)), "{dispatched:?}");
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(1, EVT_DELETE_ID)]
        );
    }

    /// Dock the panel and an external monitor, move a window onto the
    /// external one, undock it, and redock it under a new global name, as
    /// Exo does when a dock is unplugged and plugged back in.
    #[test]
    fn hotplug_moves_placement_between_outputs() {
        use super::fixture::{context, dock, move_surface, placement_area, EXTERNAL, PANEL};
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;
        let surface = 500;
        let mut ctx = context();
        let panel = dock(&mut ctx, 1, 10, 30, PANEL);
        let external = dock(&mut ctx, 1, 11, 31, EXTERNAL);
        let panel_area = Rect::new(0, 0, 1280, 752);
        let external_area = Rect::new(-1920, 0, 1920, 1080);
        assert_eq!(placement_area(&ctx, surface), Some((panel, panel_area)));
        move_surface(&mut ctx, surface, None, external);
        assert_eq!(
            placement_area(&ctx, surface),
            Some((external, external_area))
        );

        // Undock: Exo moves the window back, then removes the global.
        let external_aura = ctx.outputs.aura_output(external).expect("Aura output");
        move_surface(&mut ctx, surface, Some(external), panel);
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = 1;
        assert_eq!(
            RegistryHandler.on_global_remove(&mut ctx, 11),
            Action::Forward
        );
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(external_aura, REQ_RELEASE_AURA_OUTPUT)]
        );
        assert_eq!(ctx.outputs.live_outputs(), [panel]);
        assert_eq!(placement_area(&ctx, surface), Some((panel, panel_area)));

        // Redock under a new name: a new binding, metadata and placement.
        let redocked = dock(&mut ctx, 1, 12, 32, EXTERNAL);
        assert_ne!(redocked, external);
        move_surface(&mut ctx, surface, Some(panel), redocked);
        assert_eq!(
            placement_area(&ctx, surface),
            Some((redocked, external_area))
        );
    }

    /// One output bound through two registries is two host objects, each
    /// with its own metadata; its removal forgets both at once.
    #[test]
    fn an_output_bound_through_two_registries_is_two_outputs() {
        use super::fixture::{context, dock, send_metrics, EXTERNAL, PANEL};
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;
        let mut ctx = context();
        let first = dock(&mut ctx, 1, 10, 30, PANEL);
        let second = dock(&mut ctx, 2, 10, 31, PANEL);
        assert_ne!(first, second);
        let aura_outputs = [first, second].map(|output| ctx.outputs.aura_output(output));
        assert!(aura_outputs[0].is_some() && aura_outputs[0] != aura_outputs[1]);

        send_metrics(&mut ctx, second, EXTERNAL);
        let work_area = |ctx: &Context, output| {
            ctx.outputs
                .committed_for_test(output)
                .and_then(|state| state.work_area())
        };
        assert_eq!(work_area(&ctx, first), Some(Rect::new(0, 0, 1280, 752)));
        assert_eq!(
            work_area(&ctx, second),
            Some(Rect::new(-1920, 0, 1920, 1080))
        );

        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = 1;
        assert_eq!(
            RegistryHandler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(ctx.outputs.live_outputs().is_empty());
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            aura_outputs.map(|aura| (aura.unwrap(), REQ_RELEASE_AURA_OUTPUT))
        );
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = 2;
        assert_eq!(
            RegistryHandler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(ctx.client_to_host_queue.is_empty(), "already released");
    }
}
