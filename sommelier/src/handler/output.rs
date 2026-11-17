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

//! Host `wl_output` events and their host-only `zaura_output` extension.
//!
//! The events feed [`crate::state::OutputRegistry`], which owns every
//! output fact; `wl_output.release` also releases the output's Aura child.

use crate::handler::aura_objects;
use crate::handler::display::queue_protocol_error;
use crate::protocols::aura_shell::zaura_output::ZauraOutputHandler;
use crate::protocols::wayland::wl_output::REQ_RELEASE as REQ_RELEASE_WL_OUTPUT;
use crate::state::{is_valid_transform, Context};
use crate::wire::Action;

pub struct OutputHandler;

/// `wl_output.mode.current` from the vendored protocol.
const WL_OUTPUT_MODE_CURRENT: u32 = 1;
/// `zaura_output.scale_property.current` from the vendored protocol.
const ZAURA_OUTPUT_SCALE_PROPERTY_CURRENT: u32 = 1;

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
            if flags & WL_OUTPUT_MODE_CURRENT != 0
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
            queue_protocol_error(
                ctx,
                guest_output_id,
                0,
                "wl_output.release has no host object mapping",
            );
            return Action::Drop;
        };
        if let Some(zaura_output_host_id) = ctx.outputs.release(host_output_id) {
            aura_objects::release(ctx, aura_objects::AuraChild::OUTPUT, zaura_output_host_id);
        }
        // The Aura child goes first (see `aura_objects::release`). Returning
        // Forward would put wl_output.release ahead of the queued Aura
        // release, so queue the translated destructor explicitly.
        ctx.shadow_table.mark_pending_destroy(guest_output_id);
        let release_message =
            crate::wire::MessageBuilder::new().build_message(host_output_id, REQ_RELEASE_WL_OUTPUT);
        ctx.client_to_host_queue.push((release_message, Vec::new()));
        Action::Drop
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
        if flags & ZAURA_OUTPUT_SCALE_PROPERTY_CURRENT == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::aura_shell::zaura_output::{
        REQ_RELEASE as REQ_RELEASE_AURA_OUTPUT, REQ_RELEASE_SINCE as REQ_RELEASE_AURA_OUTPUT_SINCE,
    };
    use crate::protocols::wayland::wl_output::WlOutputHandler;
    use crate::test_support::{msg_opcode, msg_sender, protocol_error_code, queued_headers};
    use crate::window_layout::Rect;

    const OUTPUT_HOST: u32 = 20;
    const AURA_OUTPUT_HOST: u32 = 21;

    /// Bind host output 20 at `version` with `committed` metadata, and give
    /// it the v38 Aura output 21.
    fn bind_output_with_aura(
        ctx: &mut Context,
        version: u32,
        committed: crate::state::OutputState,
    ) {
        ctx.outputs.bind(
            OUTPUT_HOST,
            version,
            crate::state::GlobalBinding {
                name: 7,
                generation: 1,
            },
        );
        ctx.outputs.update(OUTPUT_HOST, |state| *state = committed);
        ctx.outputs.commit(OUTPUT_HOST);
        ctx.outputs
            .remember_aura_output(OUTPUT_HOST, AURA_OUTPUT_HOST);
        ctx.shadow_table.track_host_interface_with_version(
            AURA_OUTPUT_HOST,
            "zaura_output".into(),
            38,
        );
    }

    fn committed_work_area(ctx: &Context) -> Option<Rect> {
        ctx.outputs
            .committed_for_test(OUTPUT_HOST)
            .expect("live output")
            .work_area()
    }

    #[test]
    fn fractional_aura_scale_defines_logical_work_area() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_output_with_aura(&mut ctx, 3, Default::default());
        let mut handler = OutputHandler;

        ctx.last_sender_id = OUTPUT_HOST;
        handler.on_mode(&mut ctx, 1, 3840, 2160, 60);
        ctx.last_sender_id = AURA_OUTPUT_HOST;
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 1000);
        handler.on_device_scale_factor(&mut ctx, 1250);
        ctx.last_sender_id = OUTPUT_HOST;
        WlOutputHandler::on_done(&mut handler, &mut ctx);

        assert_eq!(
            committed_work_area(&ctx),
            Some(Rect::new(0, 0, 3072, 1728)),
            "Aura bounds use fractional DP scale, not rounded wl_output.scale"
        );
    }

    #[test]
    fn aura_scale_updates_wait_for_done_and_ignore_preferred_or_zero_factors() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_output_with_aura(
            &mut ctx,
            3,
            crate::state::OutputState {
                mode_width: 3840,
                mode_height: 2160,
                ..Default::default()
            },
        );
        let mut handler = OutputHandler;

        ctx.last_sender_id = AURA_OUTPUT_HOST;
        assert_eq!(handler.on_device_scale_factor(&mut ctx, 0), Action::Drop);
        assert_eq!(
            ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 0),
            Action::Drop
        );
        assert!(!ctx.outputs.has_pending_for_test());
        assert_eq!(committed_work_area(&ctx), None);

        handler.on_device_scale_factor(&mut ctx, 1250);
        ctx.last_sender_id = OUTPUT_HOST;
        WlOutputHandler::on_done(&mut handler, &mut ctx);
        assert_eq!(committed_work_area(&ctx), None);

        ctx.last_sender_id = AURA_OUTPUT_HOST;
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 1000);
        assert_eq!(committed_work_area(&ctx), None);
        ctx.last_sender_id = OUTPUT_HOST;
        WlOutputHandler::on_done(&mut handler, &mut ctx);
        assert_eq!(committed_work_area(&ctx), Some(Rect::new(0, 0, 3072, 1728)));

        ctx.last_sender_id = AURA_OUTPUT_HOST;
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 2, 1200);
        handler.on_device_scale_factor(&mut ctx, 0);
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 0);
        assert!(!ctx.outputs.has_pending_for_test());
        assert_eq!(committed_work_area(&ctx), Some(Rect::new(0, 0, 3072, 1728)));

        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 3, 1200);
        assert_eq!(committed_work_area(&ctx), Some(Rect::new(0, 0, 3072, 1728)));
        ctx.last_sender_id = OUTPUT_HOST;
        WlOutputHandler::on_done(&mut handler, &mut ctx);
        assert_eq!(committed_work_area(&ctx), Some(Rect::new(0, 0, 2560, 1440)));
    }

    #[test]
    fn aura_scale_events_ignore_unknown_removed_and_released_outputs() {
        let global = crate::state::GlobalBinding {
            name: 7,
            generation: 1,
        };
        for retired in 0..3 {
            let mut ctx = Context::new_for_test(false, false, vec![]);
            if retired != 0 {
                bind_output_with_aura(&mut ctx, 3, Default::default());
                if retired == 1 {
                    assert_eq!(ctx.outputs.remove_global(global), [AURA_OUTPUT_HOST]);
                } else {
                    ctx.outputs.release(OUTPUT_HOST);
                }
            }
            ctx.last_sender_id = AURA_OUTPUT_HOST;
            let mut handler = OutputHandler;
            assert_eq!(handler.on_device_scale_factor(&mut ctx, 1250), Action::Drop);
            assert_eq!(
                ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 1000),
                Action::Drop
            );
            assert!(!ctx.outputs.has_pending_for_test());
            assert_eq!(ctx.outputs.committed_for_test(OUTPUT_HOST), None);
        }
    }

    #[test]
    fn aura_scale_events_publish_immediately_for_version_one_output() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_output_with_aura(&mut ctx, 1, Default::default());
        let mut handler = OutputHandler;
        ctx.last_sender_id = OUTPUT_HOST;
        handler.on_mode(&mut ctx, 1, 3840, 2160, 60);
        assert_eq!(committed_work_area(&ctx), None);
        ctx.last_sender_id = AURA_OUTPUT_HOST;
        handler.on_device_scale_factor(&mut ctx, 1250);
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 1000);
        assert_eq!(committed_work_area(&ctx), Some(Rect::new(0, 0, 3072, 1728)));
        assert!(!ctx.outputs.has_pending_for_test());
    }

    #[test]
    fn output_geometry_and_aura_insets_define_logical_work_area() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_output_with_aura(&mut ctx, 3, Default::default());
        let mut handler = OutputHandler;

        ctx.last_sender_id = OUTPUT_HOST;
        assert_eq!(
            handler.on_geometry(
                &mut ctx,
                1920,
                16,
                600,
                340,
                0,
                &"make".to_string(),
                &"model".to_string(),
                0,
            ),
            Action::Forward
        );
        handler.on_mode(&mut ctx, 1, 3840, 2160, 60);
        ctx.last_sender_id = AURA_OUTPUT_HOST;
        assert_eq!(handler.on_insets(&mut ctx, 24, 8, 48, 16), Action::Drop);
        ZauraOutputHandler::on_scale(&mut handler, &mut ctx, 1, 1000);
        handler.on_device_scale_factor(&mut ctx, 2000);
        assert_eq!(
            ctx.outputs
                .committed_for_test(OUTPUT_HOST)
                .map(|state| state.mode_width),
            Some(0)
        );
        ctx.last_sender_id = OUTPUT_HOST;
        assert_eq!(
            WlOutputHandler::on_done(&mut handler, &mut ctx),
            Action::Forward
        );

        assert_eq!(
            committed_work_area(&ctx),
            Some(Rect::new(1928, 40, 1896, 1008))
        );
    }

    #[test]
    fn negative_aura_insets_are_rejected_without_mutating_output_state() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let insets = crate::state::OutputState {
            insets_top: 1,
            insets_left: 2,
            insets_bottom: 3,
            insets_right: 4,
            ..Default::default()
        };
        bind_output_with_aura(&mut ctx, 3, insets);
        ctx.last_sender_id = AURA_OUTPUT_HOST;

        assert_eq!(
            ZauraOutputHandler::on_insets(&mut OutputHandler, &mut ctx, -1, 2, 3, 4),
            Action::Drop
        );
        assert_eq!(ctx.outputs.committed_for_test(OUTPUT_HOST), Some(insets));
        assert!(!ctx.outputs.has_pending_for_test());
    }

    #[test]
    fn removed_output_ignores_late_wl_output_and_aura_events() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        bind_output_with_aura(&mut ctx, 3, Default::default());
        let global = crate::state::GlobalBinding {
            name: 7,
            generation: 1,
        };
        assert_eq!(ctx.outputs.remove_global(global), [AURA_OUTPUT_HOST]);
        let mut handler = OutputHandler;

        // The guest still owns a valid wl_output, so its events are
        // forwarded, but they must not repopulate retired metadata.
        ctx.last_sender_id = OUTPUT_HOST;
        assert_eq!(
            handler.on_mode(&mut ctx, 1, 3840, 2160, 60),
            Action::Forward
        );
        assert_eq!(
            WlOutputHandler::on_done(&mut handler, &mut ctx),
            Action::Forward
        );
        ctx.last_sender_id = AURA_OUTPUT_HOST;
        assert_eq!(
            ZauraOutputHandler::on_insets(&mut handler, &mut ctx, 10, 20, 30, 40),
            Action::Drop
        );
        assert!(!ctx.outputs.has_pending_for_test());
        assert_eq!(ctx.outputs.committed_for_test(OUTPUT_HOST), None);
        assert_eq!(
            ctx.outputs.release(OUTPUT_HOST),
            None,
            "the removal already handed over the Aura child"
        );
    }

    /// Send one Exo display-metrics group: `wl_output.geometry` (panel
    /// transform) and `mode` (native panel pixels), the Aura factors and
    /// `logical_transform`, then `wl_output.done`.
    fn send_rotated_output_metrics(
        ctx: &mut Context,
        mode: (i32, i32),
        panel_transform: i32,
        logical_transform: i32,
    ) {
        let mut handler = OutputHandler;
        ctx.last_sender_id = OUTPUT_HOST;
        handler.on_geometry(
            ctx,
            0,
            0,
            0,
            0,
            0,
            &"make".to_string(),
            &"model".to_string(),
            panel_transform,
        );
        handler.on_mode(ctx, 1, mode.0, mode.1, 60);
        ctx.last_sender_id = AURA_OUTPUT_HOST;
        ZauraOutputHandler::on_scale(&mut handler, ctx, 1, 1000);
        handler.on_device_scale_factor(ctx, 1000);
        ZauraOutputHandler::on_logical_transform(&mut handler, ctx, logical_transform);
        ctx.last_sender_id = OUTPUT_HOST;
        WlOutputHandler::on_done(&mut handler, ctx);
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
            let mut ctx = Context::new_for_test(false, false, vec![]);
            bind_output_with_aura(&mut ctx, 3, Default::default());
            send_rotated_output_metrics(&mut ctx, mode, panel, logical);
            assert_eq!(
                committed_work_area(&ctx),
                Some(expected),
                "mode {mode:?}, panel {panel}, logical {logical}"
            );
        }
    }

    #[test]
    fn releasing_output_removes_it_from_placement_selection() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let output_guest = 10;
        ctx.shadow_table.map_id(output_guest, OUTPUT_HOST);
        ctx.shadow_table
            .track_interface_with_version(output_guest, "wl_output".into(), 3);
        bind_output_with_aura(
            &mut ctx,
            3,
            crate::state::OutputState {
                mode_width: 1920,
                mode_height: 1080,
                aura_current_scale: Some(1000),
                aura_device_scale_factor: Some(1000),
                ..Default::default()
            },
        );
        assert!(ctx.outputs.primary().is_some());
        let mut handler = OutputHandler;
        // wl_output.release is a client request, so the sender is the guest
        // object ID even though the queued destructor targets the host ID.
        ctx.last_sender_id = output_guest;
        assert_eq!(
            WlOutputHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(msg_sender(&ctx.client_to_host_queue[0].0), AURA_OUTPUT_HOST);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[0].0),
            REQ_RELEASE_AURA_OUTPUT
        );
        assert_eq!(msg_sender(&ctx.client_to_host_queue[1].0), OUTPUT_HOST);
        assert_eq!(
            msg_opcode(&ctx.client_to_host_queue[1].0),
            REQ_RELEASE_WL_OUTPUT
        );
        assert!(ctx.outputs.primary().is_none());
        assert!(ctx.outputs.is_empty_for_test());
        assert!(ctx.shadow_table.is_pending_destroy_guest(output_guest));
        assert!(ctx
            .shadow_table
            .is_pending_destroy_host_only(AURA_OUTPUT_HOST));
    }

    /// Before `zaura_output.release` the Aura output has no destructor: it is
    /// retired without a request, and only the wl_output is released.
    #[test]
    fn aura_output_without_release_is_retired_silently() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let output_guest = 10;
        ctx.shadow_table.map_id(output_guest, OUTPUT_HOST);
        ctx.shadow_table
            .track_interface_with_version(output_guest, "wl_output".into(), 3);
        bind_output_with_aura(&mut ctx, 3, Default::default());
        ctx.shadow_table.track_host_interface_with_version(
            AURA_OUTPUT_HOST,
            "zaura_output".into(),
            REQ_RELEASE_AURA_OUTPUT_SINCE - 1,
        );
        ctx.last_sender_id = output_guest;
        assert_eq!(
            WlOutputHandler::on_release(&mut OutputHandler, &mut ctx),
            Action::Drop
        );
        let queued = queued_headers(&ctx.client_to_host_queue);
        assert_eq!(queued, vec![(OUTPUT_HOST, REQ_RELEASE_WL_OUTPUT)]);
        assert!(!ctx
            .shadow_table
            .is_pending_destroy_host_only(AURA_OUTPUT_HOST));
        assert_eq!(ctx.shadow_table.get_host_interface(AURA_OUTPUT_HOST), None);
        assert!(!ctx.shadow_table.is_host_id_available(AURA_OUTPUT_HOST));
    }

    #[test]
    fn released_outputs_leave_no_record() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = OutputHandler;
        // Output 20 lost its global before release; output 30 did not.
        for (output_guest, output_host) in [(10, 20), (11, 30)] {
            ctx.shadow_table.map_id(output_guest, output_host);
            ctx.shadow_table
                .track_interface_with_version(output_guest, "wl_output".into(), 3);
            ctx.outputs.bind_for_test(output_host, Default::default());
        }
        let removed_aura_outputs = ctx.outputs.remove_global(crate::state::GlobalBinding {
            name: 20,
            generation: 1,
        });
        assert!(removed_aura_outputs.is_empty());
        for output_guest in [10, 11] {
            ctx.last_sender_id = output_guest;
            assert_eq!(
                WlOutputHandler::on_release(&mut handler, &mut ctx),
                Action::Drop
            );
        }
        assert!(
            ctx.outputs.is_empty_for_test(),
            "a released output must leave no record behind"
        );
    }

    #[test]
    fn releasing_unmapped_output_reports_a_protocol_error() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 10;

        assert_eq!(
            WlOutputHandler::on_release(&mut OutputHandler, &mut ctx),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(ctx.host_to_client_queue.len(), 1);
        assert_eq!(protocol_error_code(&ctx), 0);
    }
}
