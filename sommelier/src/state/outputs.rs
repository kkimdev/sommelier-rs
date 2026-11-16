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
//! [`OutputRegistry`] keeps one record per bound host output: the global it
//! was bound from, whether it is still live, its committed and pending
//! [`OutputState`], and its host-only `zaura_output`. It also owns which
//! outputs each surface has entered, and it alone decides which output
//! placement may use. Handlers translate wire events into its methods.
//!
//! [`OutputState::work_area`] turns one output's metadata into logical
//! screen coordinates. [`is_valid_transform`] and [`transformed_extent`] are
//! the only statements of the `wl_output.transform` enum rules; surfaces'
//! buffer transforms use them too.

use std::collections::{HashMap, HashSet};

/// `wl_output.transform.normal`, the lowest enum value.
const TRANSFORM_NORMAL: i32 = 0;
/// `wl_output.transform.flipped_270`, the highest enum value.
const TRANSFORM_FLIPPED_270: i32 = 7;

/// Whether `transform` is a `wl_output.transform` enum value.
pub(crate) fn is_valid_transform(transform: i32) -> bool {
    (TRANSFORM_NORMAL..=TRANSFORM_FLIPPED_270).contains(&transform)
}

/// Width and height of a `width` x `height` extent after `transform`.
///
/// The odd enum values are the 90 and 270 degree rotations, flipped or not,
/// and swap the axes; the even values keep them.
pub(crate) fn transformed_extent(transform: i32, width: i32, height: i32) -> (i32, i32) {
    if transform & 1 == 1 {
        (height, width)
    } else {
        (width, height)
    }
}

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
    /// Current `zaura_output.scale`, in thousandths. Preferred-only scale
    /// entries do not affect the current output geometry.
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
    /// The output's work area, `(x, y, width, height)` in logical screen
    /// coordinates.
    ///
    /// The native pixel mode is rotated by the panel transform, divided by
    /// both Aura scale factors (floored), and shrunk by the logical insets.
    ///
    /// # Returns
    ///
    /// `None` until both Aura factors are known, and for an empty or
    /// overflowing area.
    pub fn work_area(self) -> Option<(i32, i32, i32, i32)> {
        let scale = u64::from(self.aura_current_scale?)
            .checked_mul(u64::from(self.aura_device_scale_factor?))?;
        let (mode_width, mode_height) =
            transformed_extent(self.panel_transform, self.mode_width, self.mode_height);
        // Legacy Aura factors are quantized to thousandths, so retain their
        // precision until the final division and conservatively floor DP
        // extents. Checked conversion rejects malformed or overflowing modes.
        let logical_extent = |pixels: i32| {
            let extent = u64::try_from(pixels)
                .ok()?
                .checked_mul(1_000_000)?
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
        Some((x, y, width, height))
    }
}

/// `wl_output.done` exists since this version. Older outputs have no event
/// group barrier, so each event applies as it arrives.
const WL_OUTPUT_DONE_SINCE: u32 = 2;

/// The registry advertisement that an output object was bound from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlobalBinding {
    /// Global name from `wl_registry.global`.
    pub(crate) name: u32,
    /// Advertisement generation of that name. A delayed `global_remove` of
    /// an older advertisement must not retire a binding of its replacement.
    pub(crate) generation: u64,
}

/// Whether an output may still feed compositor-owned placement.
#[derive(Debug)]
enum Liveness {
    /// Bound from a global that is still advertised.
    Live {
        /// Metadata published by the last complete event group.
        committed: OutputState,
        /// Metadata collected since then, published by `wl_output.done`.
        pending: Option<OutputState>,
    },
    /// Its global disappeared. The Wayland object stays valid until the
    /// guest releases it, but its metadata is gone and never used again.
    Inactive,
}

/// One bound host `wl_output`.
#[derive(Debug)]
struct OutputRecord {
    global: GlobalBinding,
    /// Version 2+ outputs batch events until `wl_output.done`.
    batches_until_done: bool,
    /// Host-only `zaura_output` extending this output, once created.
    aura_output: Option<u32>,
    liveness: Liveness,
}

/// Single owner of the connection's host output state. All IDs are host IDs.
///
/// A record exists from the guest's bind until its release; an output that
/// was released, or never bound, is unknown and therefore never live. Its
/// global's removal makes a record inactive without removing it, because the
/// object and its `zaura_output` live until release.
#[derive(Debug, Default)]
pub(crate) struct OutputRegistry {
    /// Bound outputs in bind order. Placement scans this order so that its
    /// choice is deterministic when several outputs qualify.
    order: Vec<u32>,
    records: HashMap<u32, OutputRecord>,
    /// Host `zaura_output` → the host `wl_output` it extends.
    aura_outputs: HashMap<u32, u32>,
    /// Host `wl_surface` → live outputs it has entered. A surface can span
    /// outputs; an entry is removed once its set would be empty.
    surface_outputs: HashMap<u32, HashSet<u32>>,
}

impl OutputRegistry {
    /// Register an output whose host bind has been queued.
    ///
    /// # Arguments
    ///
    /// * `output` - the new host `wl_output` ID.
    /// * `version` - the bound protocol version; it decides whether events
    ///   wait for `wl_output.done`.
    /// * `global` - the advertisement it was bound from.
    pub(crate) fn bind(&mut self, output: u32, version: u32, global: GlobalBinding) {
        debug_assert!(
            !self.records.contains_key(&output),
            "host output {output} is bound twice"
        );
        self.order.push(output);
        self.records.insert(
            output,
            OutputRecord {
                global,
                batches_until_done: version >= WL_OUTPUT_DONE_SINCE,
                aura_output: None,
                liveness: Liveness::Live {
                    committed: OutputState::default(),
                    pending: None,
                },
            },
        );
    }

    /// Forget an output that the guest released.
    ///
    /// # Returns
    ///
    /// The output's `zaura_output`, which the caller must tear down before
    /// the `wl_output` itself.
    pub(crate) fn release(&mut self, output: u32) -> Option<u32> {
        let record = self.records.remove(&output)?;
        self.order.retain(|&candidate| candidate != output);
        self.forget_surface_associations(output);
        let aura_output = record.aura_output?;
        self.aura_outputs.remove(&aura_output);
        Some(aura_output)
    }

    /// Make every output bound from `global` inactive after its
    /// `wl_registry.global_remove`.
    pub(crate) fn deactivate(&mut self, global: GlobalBinding) {
        let mut retired = Vec::new();
        for (&output, record) in &mut self.records {
            if record.global == global {
                record.liveness = Liveness::Inactive;
                retired.push(output);
            }
        }
        for output in retired {
            self.forget_surface_associations(output);
        }
    }

    /// Whether `output` is bound, unreleased, and still advertised.
    pub(crate) fn is_live(&self, output: u32) -> bool {
        self.records
            .get(&output)
            .is_some_and(|record| matches!(record.liveness, Liveness::Live { .. }))
    }

    /// Live outputs, in bind order.
    pub(crate) fn live_outputs(&self) -> Vec<u32> {
        self.order
            .iter()
            .copied()
            .filter(|&output| self.is_live(output))
            .collect()
    }

    /// The `zaura_output` already created for `output`, if any.
    pub(crate) fn aura_output(&self, output: u32) -> Option<u32> {
        self.records.get(&output)?.aura_output
    }

    /// Record the `zaura_output` created for a live `output`.
    pub(crate) fn attach_aura_output(&mut self, output: u32, aura_output: u32) {
        let record = self
            .records
            .get_mut(&output)
            .expect("a zaura_output is created only for a bound output");
        debug_assert!(
            record.aura_output.is_none(),
            "output {output} has a zaura_output"
        );
        record.aura_output = Some(aura_output);
        self.aura_outputs.insert(aura_output, output);
    }

    /// The live output that an event from `aura_output` describes.
    ///
    /// # Returns
    ///
    /// `None` for an unknown `zaura_output` or one whose output is no longer
    /// live: its event is stale and must not change placement metadata.
    pub(crate) fn aura_event_target(&self, aura_output: u32) -> Option<u32> {
        let output = *self.aura_outputs.get(&aura_output)?;
        self.is_live(output).then_some(output)
    }

    /// Apply one metadata event to a live output and publish it unless the
    /// output batches events until `wl_output.done`.
    ///
    /// # Returns
    ///
    /// Whether `output` is live; events for other outputs change nothing.
    pub(crate) fn update(&mut self, output: u32, apply: impl FnOnce(&mut OutputState)) -> bool {
        let Some(record) = self.records.get_mut(&output) else {
            return false;
        };
        let Liveness::Live { committed, pending } = &mut record.liveness else {
            return false;
        };
        apply(pending.get_or_insert(*committed));
        if !record.batches_until_done {
            Self::publish(committed, pending);
        }
        true
    }

    /// Publish the metadata collected for `output` at `wl_output.done`.
    pub(crate) fn commit(&mut self, output: u32) {
        if let Some(OutputRecord {
            liveness: Liveness::Live { committed, pending },
            ..
        }) = self.records.get_mut(&output)
        {
            Self::publish(committed, pending);
        }
    }

    fn publish(committed: &mut OutputState, pending: &mut Option<OutputState>) {
        if let Some(state) = pending.take() {
            *committed = state;
        }
    }

    /// The committed metadata of `output` if placement may use it: the
    /// output is live and its work area is known.
    pub(crate) fn usable(&self, output: u32) -> Option<OutputState> {
        match self.records.get(&output)?.liveness {
            Liveness::Live { committed, .. } => committed.work_area().map(|_| committed),
            Liveness::Inactive => None,
        }
    }

    /// The first usable output, in bind order.
    pub(crate) fn primary(&self) -> Option<(u32, OutputState)> {
        self.first_usable(|_| true)
    }

    /// The output to use for placing `surface`.
    ///
    /// The first usable output, in bind order, among those the surface has
    /// entered. Only a surface with no output association (created before
    /// its first enter event, or whose outputs all went away) falls back to
    /// [`Self::primary`]. If the surface has entered outputs but none is
    /// usable yet, this returns `None`: another output would move the window
    /// to a different monitor.
    pub(crate) fn for_surface(&self, surface: u32) -> Option<(u32, OutputState)> {
        match self.surface_outputs.get(&surface) {
            Some(entered) => self.first_usable(|output| entered.contains(&output)),
            None => self.primary(),
        }
    }

    fn first_usable(&self, eligible: impl Fn(u32) -> bool) -> Option<(u32, OutputState)> {
        self.order
            .iter()
            .copied()
            .filter(|&output| eligible(output))
            .find_map(|output| self.usable(output).map(|state| (output, state)))
    }

    /// Record `wl_surface.enter`. An output that is not live is ignored: its
    /// events may still be in flight after a global removal or a release,
    /// and must not steer placement.
    pub(crate) fn surface_entered(&mut self, surface: u32, output: u32) {
        if self.is_live(output) {
            self.surface_outputs
                .entry(surface)
                .or_default()
                .insert(output);
        }
    }

    /// Record `wl_surface.leave`.
    pub(crate) fn surface_left(&mut self, surface: u32, output: u32) {
        let Some(outputs) = self.surface_outputs.get_mut(&surface) else {
            return;
        };
        outputs.remove(&output);
        if outputs.is_empty() {
            self.surface_outputs.remove(&surface);
        }
    }

    /// Forget a destroyed surface's output associations.
    pub(crate) fn surface_destroyed(&mut self, surface: u32) {
        self.surface_outputs.remove(&surface);
    }

    fn forget_surface_associations(&mut self, output: u32) {
        self.surface_outputs.retain(|_, outputs| {
            outputs.remove(&output);
            !outputs.is_empty()
        });
    }
}

/// Test-only setup and inspection. Production code changes outputs only
/// through the wire-event methods above.
#[cfg(test)]
impl OutputRegistry {
    /// Bind a version-3 output from its own one-generation global and
    /// commit `committed` as its metadata.
    pub(crate) fn bind_for_test(&mut self, output: u32, committed: OutputState) {
        self.bind(
            output,
            3,
            GlobalBinding {
                name: output,
                generation: 1,
            },
        );
        self.update(output, |state| *state = committed);
        self.commit(output);
    }

    /// The committed metadata of a live output, usable or not.
    pub(crate) fn committed_for_test(&self, output: u32) -> Option<OutputState> {
        match self.records.get(&output)?.liveness {
            Liveness::Live { committed, .. } => Some(committed),
            Liveness::Inactive => None,
        }
    }

    /// Whether any output has uncommitted metadata.
    pub(crate) fn has_pending_for_test(&self) -> bool {
        self.records.values().any(|record| {
            matches!(
                record.liveness,
                Liveness::Live {
                    pending: Some(_),
                    ..
                }
            )
        })
    }

    /// The outputs `surface` has entered.
    pub(crate) fn entered_for_test(&self, surface: u32) -> Option<&HashSet<u32>> {
        self.surface_outputs.get(&surface)
    }

    /// Whether the registry holds no output, Aura output or surface entry.
    pub(crate) fn is_empty_for_test(&self) -> bool {
        self.order.is_empty()
            && self.records.is_empty()
            && self.aura_outputs.is_empty()
            && self.surface_outputs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(output.work_area(), Some((1928, 40, 1896, 1008)));
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
        assert_eq!(output.work_area(), Some((0, 0, 1536, 864)));
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
        assert_eq!(output.work_area(), Some((1928, 40, 1704, 3000)));
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
        assert_eq!(output.work_area(), Some((0, 0, 1080, 1920)));
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
        outputs.bind_for_test(20, usable_state());
        outputs.bind_for_test(30, usable_state());
        outputs.surface_entered(100, 30);
        assert_eq!(outputs.for_surface(100).map(|(id, _)| id), Some(30));

        // The entered output's metadata is incomplete: the surface is on
        // output 30, so selecting output 20 would move it to another monitor.
        outputs.update(30, |state| state.aura_device_scale_factor = None);
        outputs.commit(30);
        assert_eq!(outputs.for_surface(100), None);
        assert_eq!(outputs.primary().map(|(id, _)| id), Some(20));

        // Only a surface with no output association falls back.
        assert_eq!(outputs.for_surface(200).map(|(id, _)| id), Some(20));
        outputs.surface_left(100, 30);
        assert_eq!(outputs.entered_for_test(100), None);
        assert_eq!(outputs.for_surface(100).map(|(id, _)| id), Some(20));
    }

    #[test]
    fn primary_output_follows_bind_order() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(30, usable_state());
        outputs.bind_for_test(20, usable_state());
        assert_eq!(outputs.primary().map(|(id, _)| id), Some(30));
        assert_eq!(outputs.live_outputs(), [30, 20]);
    }

    #[test]
    fn events_batch_until_done_from_version_two() {
        for (version, published_at_once) in [(1, true), (2, false), (3, false)] {
            let mut outputs = OutputRegistry::default();
            let global = GlobalBinding {
                name: 1,
                generation: 1,
            };
            outputs.bind(20, version, global);
            assert!(outputs.update(20, |state| *state = usable_state()));
            assert_eq!(
                outputs.usable(20).is_some(),
                published_at_once,
                "version {version}"
            );
            outputs.commit(20);
            assert_eq!(outputs.usable(20), Some(usable_state()));
        }
    }

    #[test]
    fn deactivation_matches_the_exact_global_generation() {
        let mut outputs = OutputRegistry::default();
        let old = GlobalBinding {
            name: 7,
            generation: 1,
        };
        let replacement = GlobalBinding {
            name: 7,
            generation: 2,
        };
        outputs.bind(20, 3, old);
        outputs.bind(30, 3, replacement);
        outputs.surface_entered(100, 20);
        outputs.surface_entered(100, 30);

        outputs.deactivate(old);
        assert!(!outputs.is_live(20));
        assert!(
            outputs.is_live(30),
            "a delayed remove must spare the replacement"
        );
        assert_eq!(
            outputs.entered_for_test(100),
            Some(&[30].into_iter().collect())
        );
        assert!(!outputs.update(20, |state| state.mode_width = 1));
        outputs.surface_entered(101, 20);
        assert_eq!(outputs.entered_for_test(101), None);
    }

    #[test]
    fn aura_output_events_reach_only_their_live_output() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(20, OutputState::default());
        assert_eq!(outputs.aura_output(20), None);
        outputs.attach_aura_output(20, 21);
        assert_eq!(outputs.aura_output(20), Some(21));
        assert_eq!(outputs.aura_event_target(21), Some(20));
        assert_eq!(outputs.aura_event_target(22), None);

        outputs.deactivate(GlobalBinding {
            name: 20,
            generation: 1,
        });
        assert_eq!(outputs.aura_event_target(21), None);
        assert_eq!(outputs.aura_output(20), Some(21), "release still needs it");
    }

    #[test]
    fn release_returns_the_aura_output_and_forgets_everything() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(20, usable_state());
        outputs.bind_for_test(30, usable_state());
        outputs.attach_aura_output(20, 21);
        outputs.surface_entered(100, 20);

        assert_eq!(outputs.release(20), Some(21));
        assert_eq!(outputs.release(20), None, "release is not repeated");
        assert_eq!(outputs.release(30), None, "no Aura output was created");
        assert_eq!(outputs.aura_event_target(21), None);
        assert!(outputs.primary().is_none());
        assert!(outputs.is_empty_for_test());
    }

    #[test]
    fn destroyed_surface_forgets_its_outputs() {
        let mut outputs = OutputRegistry::default();
        outputs.bind_for_test(20, usable_state());
        outputs.surface_entered(100, 20);
        outputs.surface_destroyed(100);
        assert_eq!(outputs.entered_for_test(100), None);
    }
}
