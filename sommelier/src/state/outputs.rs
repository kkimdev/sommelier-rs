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

//! Host output state: one owner for every bound `wl_output`.
//!
//! [`OutputRegistry`] keeps one record per live host output: the global it
//! was bound from, its committed and pending [`OutputState`], and its
//! host-only `zaura_output`. It also owns the output each surface has
//! entered, and it alone decides which output placement may use. Handlers
//! translate wire events into its methods.
//!
//! [`OutputState::work_area`] turns one output's metadata into logical
//! screen coordinates. [`is_valid_transform`] and [`transformed_extent`] are
//! the only statements of the `wl_output.transform` enum rules; surfaces'
//! buffer transforms use them too.

use super::HostId;
use crate::geometry::Rect;
use crate::protocols::wayland::wl_output;
use crate::protocols::Iface;
use std::collections::BTreeMap;

/// Whether `transform` is a `wl_output.transform` enum value.
pub(crate) fn is_valid_transform(transform: i32) -> bool {
    (wl_output::TRANSFORM_NORMAL..=wl_output::TRANSFORM_FLIPPED_270).contains(&transform)
}

/// Width and height of a `width` x `height` extent after `transform`.
///
/// The odd enum values are the 90 and 270 degree rotations, flipped or not,
/// and swap the axes; the even values keep them. `transform` must satisfy
/// [`is_valid_transform`]: callers validate it on input, and other values
/// give a meaningless result.
pub(crate) fn transformed_extent(transform: i32, width: i32, height: i32) -> (i32, i32) {
    if transform & 1 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

/// The one interface whose bind may cross its global's removal, as when a
/// dock flaps (see `handler::registry::admit_guest_bind`). A `wl_output` up
/// to the advertised v3 has no request that creates objects, so an inert
/// one, a local-only guest object, needs nothing but its own destructor.
pub(crate) const INERT_BIND_INTERFACE: Iface = Iface::WlOutput;

/// Units per 1.0 of an Aura scale factor: `zaura_output` sends both scale
/// factors in thousandths.
const AURA_SCALE_UNITS: u64 = 1000;

/// Host output geometry used for compositor-owned window layout requests.
///
/// `wl_output.mode` reports pixel dimensions while Aura window bounds use
/// logical screen coordinates. Aura's device and current output scales
/// convert the former into the latter; `wl_output.scale` rounds fractional
/// scales upward and cannot describe those coordinates. Output insets remove
/// shelf/non-work-area margins when they are known.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputState {
    /// Logical screen-space origin from `wl_output.geometry`.
    pub origin_x: i32,
    pub origin_y: i32,
    /// Current `wl_output.mode` size: device pixels in the panel's native
    /// orientation (Exo sends `bounds_in_native`).
    pub mode_width: i32,
    pub mode_height: i32,
    /// Current `zaura_output.scale`, in thousandths (`AURA_SCALE_UNITS`).
    /// Preferred-only scale entries do not affect the current output geometry.
    pub aura_current_scale: Option<u32>,
    /// `zaura_output.device_scale_factor`, in thousandths. Both Aura factors
    /// must arrive before this output can authorize placement.
    pub aura_device_scale_factor: Option<u32>,
    /// Panel transform from `wl_output.geometry`, the rotation that maps the
    /// native-orientation `wl_output.mode` onto the logical screen.
    ///
    /// Exo sends `display.panel_rotation()` here: the active rotation plus
    /// the panel's mounting orientation. `zaura_output.logical_transform` is
    /// the active rotation alone, so it would leave a portrait-native panel
    /// in the wrong orientation; it is not used.
    pub panel_transform: i32,
    /// `zaura_output.insets`: work-area margins in logical screen
    /// coordinates, applied after rotation and scaling.
    pub insets_top: i32,
    pub insets_left: i32,
    pub insets_bottom: i32,
    pub insets_right: i32,
}

impl OutputState {
    /// The output's effective scale from DIPs to its pixels: the display
    /// zoom (the current `zaura_output.scale`) times the device scale
    /// factor, as Exo scales logical coordinates, in units of
    /// `AURA_SCALE_UNITS` squared.
    ///
    /// # Returns
    ///
    /// `None` until both Aura factors are known.
    fn aura_scale(self) -> Option<u64> {
        u64::from(self.aura_current_scale?).checked_mul(u64::from(self.aura_device_scale_factor?))
    }

    /// The output's work area in logical screen coordinates.
    ///
    /// The native pixel mode is rotated by the panel transform, divided by
    /// both Aura scale factors (floored), and shrunk by the logical insets.
    ///
    /// # Returns
    ///
    /// `None` until both Aura factors are known, and for an empty or
    /// overflowing area.
    pub fn work_area(self) -> Option<Rect> {
        let scale = self.aura_scale()?;
        let (mode_width, mode_height) =
            transformed_extent(self.panel_transform, self.mode_width, self.mode_height);
        // Legacy Aura factors are quantized to thousandths, so retain their
        // precision until the final division and conservatively floor DP
        // extents. `scale` is the product of two factors, so its unit is
        // squared. Checked conversion rejects malformed or overflowing modes.
        let logical_extent = |pixels: i32| {
            let extent = u64::try_from(pixels)
                .ok()?
                .checked_mul(AURA_SCALE_UNITS * AURA_SCALE_UNITS)?
                .checked_div(scale)?;
            i32::try_from(extent).ok()
        };
        let width = logical_extent(mode_width)?;
        let height = logical_extent(mode_height)?;
        let x = self.origin_x.checked_add(self.insets_left)?;
        let y = self.origin_y.checked_add(self.insets_top)?;
        let width = width
            .checked_sub(self.insets_left)?
            .checked_sub(self.insets_right)?;
        let height = height
            .checked_sub(self.insets_top)?
            .checked_sub(self.insets_bottom)?;
        if width <= 0 || height <= 0 {
            return None;
        }
        Some(Rect::new(x, y, width, height))
    }
}

/// One live host `wl_output`.
#[derive(Debug)]
struct OutputRecord {
    /// The host `wl_output` ID.
    output: HostId,
    /// The name of the global it was bound from.
    global: u32,
    /// Outputs with `wl_output.done` batch events until it arrives. Older
    /// outputs have no event group barrier, so each event applies at once.
    batches_until_done: bool,
    /// Host-only `zaura_output` extending this output, once created.
    aura_output: Option<HostId>,
    /// Metadata published by the last complete event group.
    committed: OutputState,
    /// Metadata collected since then, published by `wl_output.done`.
    pending: Option<OutputState>,
}

impl OutputRecord {
    fn publish(&mut self) {
        if let Some(state) = self.pending.take() {
            self.committed = state;
        }
    }

    /// The output and its committed metadata if placement may use them:
    /// its work area is known.
    fn usable(&self) -> Option<(HostId, OutputState)> {
        self.committed
            .work_area()
            .map(|_| (self.output, self.committed))
    }
}

/// Single owner of the connection's host output state. All IDs are host IDs.
///
/// A record exists from the guest's bind until its release or its global's
/// removal, whichever comes first; any other output is unknown and never
/// live. The guest's `wl_output` outlives a removed global until the guest
/// releases it, but nothing about it is needed again: its events change
/// nothing, and its release forwards just the `wl_output` destructor.
#[derive(Debug, Default)]
pub(crate) struct OutputRegistry {
    /// Live outputs in bind order, so placement's choice is deterministic
    /// when several outputs qualify. A connection has a handful of outputs,
    /// so lookups scan.
    records: Vec<OutputRecord>,
    /// Host `wl_surface` → the live output it entered last. Exo shows each
    /// surface on one display, and moving it sends `leave` for the old
    /// display before `enter` for the new one.
    surface_outputs: BTreeMap<HostId, HostId>,
}

impl OutputRegistry {
    /// Register an output whose host bind has been queued.
    ///
    /// # Arguments
    ///
    /// * `output` - the new host `wl_output` ID.
    /// * `version` - the bound protocol version; it decides whether events
    ///   wait for `wl_output.done`.
    /// * `global` - the name of the global it was bound from.
    pub(crate) fn bind(&mut self, output: HostId, version: u32, global: u32) {
        // Host IDs are not reused before the release that removes the
        // record, so a second bind is a bookkeeping bug. Release builds keep
        // the existing record rather than listing the output twice.
        let already_bound = self.is_live(output);
        debug_assert!(!already_bound, "host output {output} is bound twice");
        if already_bound {
            return;
        }
        self.records.push(OutputRecord {
            output,
            global,
            batches_until_done: version >= wl_output::EVT_DONE_SINCE,
            aura_output: None,
            committed: OutputState::default(),
            pending: None,
        });
    }

    /// Forget an output that the guest released.
    ///
    /// # Returns
    ///
    /// The output's `zaura_output`, which the caller must tear down before
    /// the `wl_output` itself.
    pub(crate) fn release(&mut self, output: HostId) -> Option<HostId> {
        let index = self
            .records
            .iter()
            .position(|record| record.output == output)?;
        let record = self.records.remove(index);
        self.surface_outputs.retain(|_, entered| *entered != output);
        record.aura_output
    }

    /// Forget every output bound from `global` after its
    /// `wl_registry.global_remove`.
    ///
    /// # Returns
    ///
    /// Their `zaura_output`s, which the caller must release now: nothing
    /// reads them again, and a `wl_output` older than version 3 has no
    /// release request that could free them later.
    pub(crate) fn remove_global(&mut self, global: u32) -> Vec<HostId> {
        let removed: Vec<HostId> = self
            .records
            .iter()
            .filter(|record| record.global == global)
            .map(|record| record.output)
            .collect();
        removed
            .into_iter()
            .filter_map(|output| self.release(output))
            .collect()
    }

    fn record(&self, output: HostId) -> Option<&OutputRecord> {
        self.records.iter().find(|record| record.output == output)
    }

    fn record_mut(&mut self, output: HostId) -> Option<&mut OutputRecord> {
        self.records
            .iter_mut()
            .find(|record| record.output == output)
    }

    /// Whether `output` is bound, unreleased, and still advertised.
    pub(crate) fn is_live(&self, output: HostId) -> bool {
        self.record(output).is_some()
    }

    /// Live outputs, in bind order.
    pub(crate) fn live_outputs(&self) -> Vec<HostId> {
        self.records.iter().map(|record| record.output).collect()
    }

    /// The `zaura_output` created for `output`, if any.
    pub(crate) fn aura_output(&self, output: HostId) -> Option<HostId> {
        self.record(output)?.aura_output
    }

    /// Record the `zaura_output` created for a live output that had none.
    /// Only a live output may get one (see [`Self::is_live`]).
    pub(crate) fn remember_aura_output(&mut self, output: HostId, aura_output: HostId) {
        debug_assert!(self.is_live(output) && self.aura_output(output).is_none());
        if let Some(record) = self.record_mut(output) {
            record.aura_output = Some(aura_output);
        }
    }

    /// The live output that an event from `aura_output` describes.
    ///
    /// # Returns
    ///
    /// `None` for an unknown `zaura_output` or one whose output is no longer
    /// live: its event is stale and must not change placement metadata.
    pub(crate) fn aura_event_target(&self, aura_output: HostId) -> Option<HostId> {
        self.records
            .iter()
            .find(|record| record.aura_output == Some(aura_output))
            .map(|record| record.output)
    }

    /// Apply one metadata event to a live output and publish it unless the
    /// output batches events until `wl_output.done`.
    ///
    /// # Returns
    ///
    /// Whether `output` is live; events for other outputs change nothing.
    pub(crate) fn update(&mut self, output: HostId, apply: impl FnOnce(&mut OutputState)) -> bool {
        let Some(record) = self.record_mut(output) else {
            return false;
        };
        apply(record.pending.get_or_insert(record.committed));
        if !record.batches_until_done {
            record.publish();
        }
        true
    }

    /// Publish the metadata collected for `output` at `wl_output.done`.
    pub(crate) fn commit(&mut self, output: HostId) {
        if let Some(record) = self.record_mut(output) {
            record.publish();
        }
    }

    /// The first usable output, in bind order.
    pub(crate) fn primary(&self) -> Option<(HostId, OutputState)> {
        self.records.iter().find_map(OutputRecord::usable)
    }

    /// The output to use for placing `surface`: the one it entered last.
    ///
    /// Only a surface on no output (before its first enter event, or after
    /// its output went away) falls back to [`Self::primary`]. If the
    /// surface's output is not usable yet, this returns `None`: another
    /// output would move the window to a different monitor.
    pub(crate) fn for_surface(&self, surface: HostId) -> Option<(HostId, OutputState)> {
        match self.surface_outputs.get(&surface) {
            Some(&output) => self.record(output)?.usable(),
            None => self.primary(),
        }
    }

    /// Record `wl_surface.enter`. An output that is not live is ignored: its
    /// events may still be in flight after a global removal or a release,
    /// and must not steer placement.
    pub(crate) fn surface_entered(&mut self, surface: HostId, output: HostId) {
        if self.is_live(output) {
            self.surface_outputs.insert(surface, output);
        }
    }

    /// Record `wl_surface.leave`. A leave for an output other than the one
    /// the surface entered last changes nothing, whichever order the two
    /// events arrive in.
    pub(crate) fn surface_left(&mut self, surface: HostId, output: HostId) {
        if self.surface_outputs.get(&surface) == Some(&output) {
            self.surface_outputs.remove(&surface);
        }
    }

    /// Forget a destroyed surface's output association.
    pub(crate) fn surface_destroyed(&mut self, surface: HostId) {
        self.surface_outputs.remove(&surface);
    }
}

/// Test-only setup and inspection. Production code changes outputs only
/// through the wire-event methods above.
#[cfg(test)]
impl OutputRegistry {
    /// Bind a version-3 output from a global of its own, named as it is,
    /// and commit `committed` as its metadata.
    pub(crate) fn bind_for_test(&mut self, output: HostId, committed: OutputState) {
        self.bind(output, 3, crate::wire::WireId::wire(output));
        self.update(output, |state| *state = committed);
        self.commit(output);
    }

    /// The output `surface` entered last.
    pub(crate) fn entered_for_test(&self, surface: HostId) -> Option<HostId> {
        self.surface_outputs.get(&surface).copied()
    }

    /// Whether the registry holds no output or surface entry.
    pub(crate) fn is_empty_for_test(&self) -> bool {
        self.records.is_empty() && self.surface_outputs.is_empty()
    }

    /// Every host wl_surface with an output, and that output.
    pub(super) fn surface_outputs_for_test(&self) -> impl Iterator<Item = (HostId, HostId)> + '_ {
        self.surface_outputs
            .iter()
            .map(|(&surface, &output)| (surface, output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host ID `id`.
    fn h(id: u32) -> HostId {
        HostId::for_test(id)
    }

    #[test]
    fn transform_rules_follow_the_wl_output_enum() {
        for transform in 0..=7 {
            assert!(is_valid_transform(transform));
            let swapped = matches!(transform, 1 | 3 | 5 | 7);
            assert_eq!(
                transformed_extent(transform, 4, 3),
                if swapped { (3, 4) } else { (4, 3) },
                "transform {transform}"
            );
        }
        assert!(!is_valid_transform(-1));
        assert!(!is_valid_transform(8));
    }

    #[test]
    fn output_work_area_converts_scale_and_insets() {
        let output = OutputState {
            origin_x: 1920,
            origin_y: 16,
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(2000),
            insets_top: 24,
            insets_left: 8,
            insets_bottom: 48,
            insets_right: 16,
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some(Rect::new(1928, 40, 1896, 1008)));
    }

    #[test]
    fn output_work_area_combines_aura_scales_before_rounding() {
        let output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1250),
            aura_device_scale_factor: Some(2000),
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 1536, 864)));
    }

    #[test]
    fn fractional_output_work_area_rotates_before_applying_dp_insets() {
        let output = OutputState {
            origin_x: 1920,
            origin_y: 16,
            mode_width: 3840,
            mode_height: 2160,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1250),
            panel_transform: 1,
            insets_top: 24,
            insets_left: 8,
            insets_bottom: 48,
            insets_right: 16,
        };
        assert_eq!(output.work_area(), Some(Rect::new(1928, 40, 1704, 3000)));
    }

    #[test]
    fn output_work_area_requires_valid_aura_factors() {
        let mut output = OutputState {
            mode_width: 3840,
            mode_height: 2160,
            ..Default::default()
        };
        for (current, device) in [
            (None, None),
            (Some(1000), None),
            (None, Some(1250)),
            (Some(0), Some(1250)),
            (Some(1000), Some(0)),
            (Some(u32::MAX), Some(u32::MAX)),
        ] {
            output.aura_current_scale = current;
            output.aura_device_scale_factor = device;
            assert_eq!(output.work_area(), None, "factors: {current:?}, {device:?}");
        }
        output.mode_width = i32::MAX;
        output.aura_current_scale = Some(1);
        output.aura_device_scale_factor = Some(1);
        assert_eq!(output.work_area(), None, "DP dimensions must fit i32");
    }

    #[test]
    fn output_work_area_rejects_invalid_dimensions() {
        let output = OutputState {
            mode_width: 100,
            mode_height: 100,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(2000),
            insets_top: 60,
            ..Default::default()
        };
        assert_eq!(output.work_area(), None);
    }

    #[test]
    fn output_work_area_swaps_rotated_dimensions() {
        let output = OutputState {
            mode_width: 1920,
            mode_height: 1080,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            panel_transform: 1,
            ..Default::default()
        };
        assert_eq!(output.work_area(), Some(Rect::new(0, 0, 1080, 1920)));
    }

    fn usable_state() -> OutputState {
        OutputState {
            mode_width: 1920,
            mode_height: 1080,
            aura_current_scale: Some(1000),
            aura_device_scale_factor: Some(1000),
            ..Default::default()
        }
    }

    #[test]
    fn surface_output_selection_never_jumps_to_another_output() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(h(20), usable_state());
        outputs.bind_for_test(h(30), usable_state());
        outputs.surface_entered(h(100), h(30));
        assert_eq!(outputs.for_surface(h(100)).map(|(id, _)| id), Some(h(30)));

        // The entered output's metadata is incomplete: the surface is on
        // output 30, so selecting output 20 would move it to another monitor.
        outputs.update(h(30), |state| state.aura_device_scale_factor = None);
        outputs.commit(h(30));
        assert_eq!(outputs.for_surface(h(100)), None);
        assert_eq!(outputs.primary().map(|(id, _)| id), Some(h(20)));

        // Only a surface with no output association falls back.
        assert_eq!(outputs.for_surface(h(200)).map(|(id, _)| id), Some(h(20)));
        outputs.surface_left(h(100), h(30));
        assert_eq!(outputs.entered_for_test(h(100)), None);
        assert_eq!(outputs.for_surface(h(100)).map(|(id, _)| id), Some(h(20)));
    }

    #[test]
    fn primary_output_follows_bind_order() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(h(30), usable_state());
        outputs.bind_for_test(h(20), usable_state());
        assert_eq!(outputs.primary().map(|(id, _)| id), Some(h(30)));
        assert_eq!(outputs.live_outputs(), [h(30), h(20)]);
    }

    #[test]
    fn events_batch_until_done_from_version_two() {
        for (version, published_at_once) in [(1, true), (2, false), (3, false)] {
            let mut outputs = OutputRegistry::default();
            outputs.bind(h(20), version, 1);
            assert!(outputs.update(h(20), |state| *state = usable_state()));
            assert_eq!(
                outputs.primary().is_some(),
                published_at_once,
                "version {version}"
            );
            outputs.commit(h(20));
            assert_eq!(outputs.primary(), Some((h(20), usable_state())));
        }
    }

    #[test]
    fn removal_forgets_only_the_outputs_of_its_global() {
        let mut outputs = OutputRegistry::default();
        outputs.bind(h(20), 3, 7);
        outputs.bind(h(30), 3, 8);
        outputs.surface_entered(h(100), h(20));
        outputs.surface_entered(h(101), h(30));

        assert!(outputs.remove_global(7).is_empty(), "no Aura output");
        assert!(!outputs.is_live(h(20)));
        assert!(outputs.is_live(h(30)), "another global's output stays");
        assert_eq!(outputs.entered_for_test(h(100)), None);
        assert_eq!(outputs.entered_for_test(h(101)), Some(h(30)));
        assert!(!outputs.update(h(20), |state| state.mode_width = 1));
        outputs.surface_entered(h(102), h(20));
        assert_eq!(outputs.entered_for_test(h(102)), None);
    }

    #[test]
    fn aura_output_events_reach_only_their_live_output() {
        let mut outputs = OutputRegistry::default();
        assert_eq!(outputs.aura_output(h(20)), None, "unbound");
        outputs.bind_for_test(h(20), OutputState::default());
        assert_eq!(outputs.aura_output(h(20)), None);
        outputs.remember_aura_output(h(20), h(21));
        assert_eq!(outputs.aura_output(h(20)), Some(h(21)));
        assert_eq!(outputs.aura_event_target(h(21)), Some(h(20)));
        assert_eq!(outputs.aura_event_target(h(22)), None);

        assert_eq!(outputs.remove_global(20), [h(21)]);
        assert_eq!(outputs.aura_event_target(h(21)), None);
        assert_eq!(outputs.aura_output(h(20)), None);
        assert!(!outputs.is_live(h(20)), "a removed output gets no new one");
    }

    #[test]
    fn release_returns_the_aura_output_and_forgets_everything() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(h(20), usable_state());
        outputs.bind_for_test(h(30), usable_state());
        outputs.remember_aura_output(h(20), h(21));
        outputs.surface_entered(h(100), h(20));

        assert_eq!(outputs.release(h(20)), Some(h(21)));
        assert_eq!(outputs.release(h(20)), None, "release is not repeated");
        assert_eq!(outputs.release(h(30)), None, "no Aura output was created");
        assert_eq!(outputs.aura_event_target(h(21)), None);
        assert!(outputs.primary().is_none());
        assert!(outputs.is_empty_for_test());
    }

    #[test]
    fn destroyed_surface_forgets_its_outputs() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(h(20), usable_state());
        outputs.surface_entered(h(100), h(20));
        outputs.surface_destroyed(h(100));
        assert_eq!(outputs.entered_for_test(h(100)), None);
    }
}
