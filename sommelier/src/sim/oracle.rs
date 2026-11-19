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

//! The two oracles that judge a case once it has settled.
//!
//! **Tier 1, exact, every case.** [`SpecText`] reads what reached the
//! proxy, in order ([`Seen`]), and applies the specification of the
//! design's section 2 (`docs/IME_REDESIGN.md`) to an ideal client per
//! field. From the inputs alone it decides what the proxy must ask of the
//! host: an `activate` or a `deactivate` and its barrier where the field's
//! wish and the link differ, and a `reset` and its fence for a press that
//! ends a composition. Each step's decisions must be the proxy's own.
//! [`Shadow`] is the same ideal client fed what the proxy sent the guest.
//! The two must agree ([`exact`]) in text, cursor, preedit and preedit
//! cursor, whatever the timing, since neither lags. The guest's preedit
//! must also be what Exo composes, and Exo must hold the field the guest
//! last committed ([`V3Field`]).
//!
//! **Tier 2, user intent, settled cases only.** The client model's fields
//! and focus must equal those of [`Reference`], which applied the user's
//! actions through an ideal input method ([`intended`]). A failure the
//! proxy is known to cause is a [`KnownIssue`] ([`classify`]): the client
//! must then show exactly what the issue predicts.
//!
//! `SpecText` tracks whether the guest's text is ahead of what it last
//! reported, which drops Exo's absolute deletes (H9), and the bytes the
//! preedit regions took since, which with the preedit Exo holds give
//! Exo's text, which Exo measures a region in (X1). Exo sends no
//! `preedit_styling` (H4).

use std::collections::BTreeMap;

use super::client::Client;
use super::editor::{Reference, Target, TextModel, Txn, Typing, UserAction};
use super::exo::{self, Exo, RegionTruth};
use super::hangul::Key;
use super::keymap;
use super::sched::Detach;
use super::surfaces::Surfaces;
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1 as peek;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as extended;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1 as v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3 as v3;
use crate::protocols::wayland::{wl_callback, wl_display, wl_keyboard, wl_pointer, wl_touch};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::wire::IndexedString;

/// A message at the proxy, in the order it saw or sent them.
#[derive(Clone, Copy, Debug)]
pub(super) enum Seen<'a> {
    /// A request of the guest's text input reached the proxy.
    Guest(&'a v3::Request),
    /// An event from host object `sender` reached the proxy.
    Host {
        sender: u32,
        event: &'a AnyEvent,
        origin: Origin,
        /// What Exo did of it, if it is a `set_preedit_region`.
        region: Option<&'a RegionTruth>,
    },
    /// The proxy sent the host a request.
    ToHost(&'a AnyRequest),
    /// The proxy sent the guest an event.
    ToGuest(&'a AnyEvent),
    /// The proxy sent the guest a button press or a touch-down, which
    /// lands at `at` on the guest's surface `surface`.
    GuestPress { surface: u32, at: Target },
    /// The guest destroyed the surface of field `field` (F2).
    SurfaceDestroyed { field: usize },
    /// The proxy has sent all it sends for the message that reached it.
    StepEnd,
}

/// What engine text a host event carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Origin {
    /// An answer the engine made before a reset that reached it after the
    /// key (E5's outcome (c), P3).
    pub late: bool,
    /// How many resets Exo had handed the engine when it sent the event.
    pub resets: u64,
    /// The key whose press, not an autorepeat, the event answers.
    pub press: Option<Key>,
    /// The proxy had sent Exo a surrounding text, by the time the event
    /// reached it, that Exo had yet to get when it sent the event: on its
    /// way to Exo then, or sent while the event was on its way to the
    /// proxy. Either way the proxy's committed text is newer than Exo's.
    pub crossed: bool,
}

/// Which oracle a check belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tier {
    /// Tier 1: the specification, exactly.
    Exact,
    /// Tier 2: the user's intent.
    Intent,
}

/// A failed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub tier: Tier,
    /// What was checked: `"text"`, `"decision"`, `"composition"`,
    /// `"field state"` or `"focus"`.
    pub check: &'static str,
    /// The field it failed on, if the check is of one.
    pub field: Option<usize>,
    /// What was expected and what was found.
    pub detail: String,
}

/// What a guest committed of a text-input-v3 field, and what Exo then
/// holds for it ([`V3Field::on_host`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct V3Field {
    /// `(hint, purpose)`.
    pub content_type: (u32, u32),
    pub surrounding_text: Option<(String, u32, u32)>,
    pub cursor_rectangle: Option<(i32, i32, i32, i32)>,
}

impl V3Field {
    /// What Exo should hold for this field once the proxy has sent it to
    /// a host whose `zcr_extended_text_input_v1` has `extension_version`
    /// (0 for none): the proxy's own mapping of the content type, through
    /// the extension from v2 on and through v1 `set_content_type` before;
    /// inline composition, which every v3 client supports; surrounding
    /// text, supported when the field has it or when the extension cannot
    /// say otherwise (before v9); and the cursor rectangle, empty when it
    /// has none.
    pub(crate) fn on_host(&self, extension_version: u32) -> exo::Field {
        let (hint, purpose) = self.content_type;
        let (v1_hint, v1_purpose, input_type, input_mode, flags, learning_mode) =
            crate::ime::map_v3_content_type(hint, purpose);
        let content_type = if extension_version >= extended::REQ_DEPRECATED_SET_INPUT_TYPE_SINCE {
            exo::ContentType {
                input_type,
                input_mode,
                flags,
                learning: learning_mode == extended::LEARNING_MODE_ENABLED,
                inline_composition: true,
            }
        } else {
            exo::v1_content_type(v1_hint, v1_purpose)
        };
        exo::Field {
            attached: true,
            content_type,
            surrounding_text_supported: self.surrounding_text.is_some()
                || extension_version < extended::REQ_SET_SURROUNDING_TEXT_SUPPORT_SINCE,
            surrounding_text: host_surrounding_text(self.surrounding_text.as_ref()),
            cursor_rectangle: self.cursor_rectangle.unwrap_or_default(),
        }
    }
}

/// The surrounding text the proxy reports to the host for a commit that
/// states `stated`: the text, cursor and anchor as the guest stated them,
/// or `("", 0, 0)` for a cleared one (an enable or a disable). The one
/// place the oracle says what a report to the host holds, per step
/// ([`SpecText`]) and at quiescence ([`V3Field::on_host`]).
pub(crate) fn host_surrounding_text(stated: Option<&(String, u32, u32)>) -> (String, u32, u32) {
    stated.cloned().unwrap_or_default()
}

/// Whether `request` to the host reports a content type or the support for
/// surrounding text.
fn is_content_type(request: &AnyRequest) -> bool {
    content_type_matches(request, (0, 0), false).is_some()
}

/// What a content-type request to the host must hold for a field whose
/// committed content type is `(hint, purpose)` and which states a
/// surrounding text or not (`surrounding`), if `request` is one: `Some` of
/// whether it does.
fn content_type_matches(
    request: &AnyRequest,
    (hint, purpose): (u32, u32),
    surrounding: bool,
) -> Option<bool> {
    let (v1_hint, v1_purpose, input_type, input_mode, flags, learning_mode) =
        crate::ime::map_v3_content_type(hint, purpose);
    Some(match *request {
        AnyRequest::ZwpTextInputV1(v1::Request::SetContentType { hint, purpose }) => {
            (hint, purpose) == (v1_hint, v1_purpose)
        }
        AnyRequest::ZcrExtendedTextInputV1(extended::Request::DeprecatedSetInputType {
            input_type: t,
            input_mode: m,
            input_flags: f,
            learning_mode: l,
        }) => (t, m, f, l) == (input_type, input_mode, flags, learning_mode),
        AnyRequest::ZcrExtendedTextInputV1(extended::Request::SetInputType {
            input_type: t,
            input_mode: m,
            input_flags: f,
            learning_mode: l,
            inline_composition_support: i,
        }) => {
            let inline = extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED;
            (t, m, f, l, i) == (input_type, input_mode, flags, learning_mode, inline)
        }
        AnyRequest::ZcrExtendedTextInputV1(extended::Request::SetSurroundingTextSupport {
            support,
        }) => {
            let supported = extended::SURROUNDING_TEXT_SUPPORT_SUPPORTED;
            (support == supported) == surrounding
        }
        _ => return None,
    })
}

/// The v3 state a guest has sent since its last commit (G3 to G8).
#[derive(Clone, Debug, Default)]
struct Pending {
    enabled: Option<bool>,
    surrounding_text: Option<Option<(String, u32, u32)>>,
    content_type: Option<(u32, u32)>,
    cursor_rectangle: Option<Option<(i32, i32, i32, i32)>>,
}

/// The host link of the text input (section 3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Detached,
    Attached,
    /// Deactivated, until the `done` of this barrier.
    Draining(Option<u32>),
}

/// The reset fence (F4, W2): open from the press's `reset` until the
/// `done` of the sync after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fence {
    Closed,
    Open(Option<u32>),
}

/// What the proxy asks of the host in one step, as far as the IME's
/// decisions go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Decisions {
    /// `activate` on this host surface.
    activate: Option<u32>,
    deactivate: bool,
    reset: bool,
    /// `wl_display.sync`: the barrier and the fence.
    syncs: u32,
}

/// What a sync of the proxy's follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum After {
    Deactivate,
    Reset,
    #[default]
    Other,
}

/// What the specification takes Exo's text to be when it makes a region
/// (X1): the guest's text with the preedit Exo holds at the guest's
/// cursor, Exo's cursor in it; and whether the guest's text is ahead of its
/// report.
#[derive(Clone, Debug)]
struct RegionModel {
    text: String,
    cursor: usize,
    held: String,
    ahead: bool,
}

/// The preedit the guest shows, as the proxy tracks it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    text: String,
    cursor: (i32, i32),
    origin: Origin,
    /// Where Exo's cursor is in `text`, while Exo holds `text` at the
    /// guest's cursor (X1); `None` for a region's once the guest reported
    /// text without its bytes, which Exo cannot match (E4).
    host_cursor: Option<usize>,
}

/// A field as a client shows it: its model, and the cursor of its preedit,
/// `(0, 0)` without one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct View {
    pub model: TextModel,
    pub preedit_cursor: (i32, i32),
}

impl View {
    fn new(text: &str) -> Self {
        Self {
            model: TextModel::new(text),
            preedit_cursor: (0, 0),
        }
    }

    /// The client applies `txn` at `done`, with the preedit at `cursor`.
    fn apply(&mut self, txn: &Txn, cursor: (i32, i32)) {
        self.model.apply_txn(&clamped(&self.model, txn));
        self.preedit_cursor = if txn.preedit.is_empty() {
            (0, 0)
        } else {
            cursor
        };
    }
}

/// What a case counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// Presses that ended a composition (F4).
    pub press_commits: u64,
    /// Host composition events the fence dropped.
    pub fence_drops: u64,
    /// Host events of late engine answers that the guest got (P3).
    pub late_shown: u64,
    /// Keys that reached the guest while no text input was attached (E7).
    pub raw_keys: u64,
    /// Engine answers made before a reset that reached the engine after
    /// their key (E5's outcome (c)).
    pub late: u64,
    /// Engine answers whose text a reset dropped (E5's outcome (b)).
    pub dropped: u64,
    /// Exo's own detaches while attached (H2).
    pub host_detaches: u64,
    /// Exo's deletes the guest got (H9).
    pub deletes: u64,
    /// Exo's absolute deletes dropped while the guest's text was ahead of
    /// its report (H9), which guards against P4.
    pub deletes_ahead: u64,
    /// Exo's deletes dropped for their range (H9).
    pub deletes_misplaced: u64,
    /// Preedit regions the guest got (X1), and those of them over a region
    /// the guest had not reported.
    pub regions: u64,
    pub regions_over_regions: u64,
    /// Preedit regions the guest got over a preedit Exo holds whose bytes
    /// the committed text does not hold, a host preedit's (X1).
    pub regions_over_preedits: u64,
    /// Empty preedit regions that ended the composition Exo held, which the
    /// guest committed (X1).
    pub regions_ended: u64,
    /// Preedit regions dropped for their range or for not covering a
    /// region the guest had not reported (X1').
    pub regions_dropped: u64,
    /// Guest reports that crossed an edit on its way to the guest (P4).
    pub crossed_reports: u64,
    /// Preedit regions Exo made before it got a report the proxy had sent
    /// by the time the region reached it (KnownIssue::RegionCrossesReport).
    pub regions_crossing_reports: u64,
    /// Ranges IMA sent after the commit of the key it held them for
    /// (KnownIssue::StaleRegionAfterCommit).
    pub regions_stale: u64,
    /// Regions after which the guest shows another preedit than Exo
    /// composed, as Exo made them over a composition it no longer held
    /// (KnownIssue::RegionOverUnheldPreedit), or in text holding edits the
    /// guest had yet to report (KnownIssue::RegionWhileAhead).
    pub regions_unheld: u64,
    pub regions_ahead: u64,

    /// Focus moves that the text input heard of before the keyboard, bound
    /// after its activate, so that Exo detached while attached (H2).
    pub detaches_before_keyboard: u64,
    /// Empty confirmations that recovered a held key (X2'): its press and
    /// release reached the guest, or not, as `wl_keyboard.key` had already
    /// delivered it; and those that found no eligible held key.
    pub recovered_keys: u64,
    pub recoveries_skipped: u64,
    pub confirms_unheld: u64,
    /// Backspace leases armed, and ended while armed.
    pub leases: u64,
    pub leases_ended: u64,
    /// Autorepeats of a held key that Exo handed the engine (D7).
    pub repeats: u64,
    /// Field surfaces the guest destroyed, and those the text input was
    /// focused on (F2).
    pub surface_destroys: u64,
    pub focused_surface_destroys: u64,
    /// Recovered keys whose confirmation is Exo's before the press of
    /// another key (E13), not an autorepeat's: the guest gets a press of
    /// the held key that the user did not repeat.
    pub recoveries_before_a_press: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.press_commits += other.press_commits;
        self.fence_drops += other.fence_drops;
        self.late_shown += other.late_shown;
        self.raw_keys += other.raw_keys;
        self.late += other.late;
        self.dropped += other.dropped;
        self.host_detaches += other.host_detaches;
        self.deletes += other.deletes;
        self.deletes_ahead += other.deletes_ahead;
        self.deletes_misplaced += other.deletes_misplaced;
        self.regions += other.regions;
        self.regions_over_regions += other.regions_over_regions;
        self.regions_over_preedits += other.regions_over_preedits;
        self.regions_ended += other.regions_ended;
        self.regions_dropped += other.regions_dropped;
        self.crossed_reports += other.crossed_reports;
        self.regions_crossing_reports += other.regions_crossing_reports;
        self.regions_stale += other.regions_stale;
        self.regions_unheld += other.regions_unheld;
        self.regions_ahead += other.regions_ahead;
        self.detaches_before_keyboard += other.detaches_before_keyboard;
        self.recovered_keys += other.recovered_keys;
        self.recoveries_skipped += other.recoveries_skipped;
        self.confirms_unheld += other.confirms_unheld;
        self.leases += other.leases;
        self.leases_ended += other.leases_ended;
        self.repeats += other.repeats;
        self.surface_destroys += other.surface_destroys;
        self.focused_surface_destroys += other.focused_surface_destroys;
        self.recoveries_before_a_press += other.recoveries_before_a_press;
    }
}

/// A key the host holds, as the keyboard concern learns of it.
#[derive(Clone, Copy, Debug, Default)]
struct Held {
    /// The order of its `peek_key` press among all.
    sequence: u64,
    /// `wl_keyboard.key` delivered its press to the guest.
    forwarded: bool,
    /// Backspace only: the text input holds its IME repeat lease, or the
    /// lease was cancelled for the rest of the press.
    leased: bool,
    cancelled: bool,
}

/// What the specification says the proxy does: the decisions it asks of
/// the host, and what an ideal client then shows. The case has one guest
/// text input on one seat, which follows the keyboard focus.
pub(super) struct SpecText {
    /// Which field each host surface is.
    surfaces: Surfaces,
    views: Vec<View>,
    /// The field the keyboard focuses, which the text input follows (F1).
    focus: Option<usize>,
    shift: bool,
    /// The surface the pointer is on, and where.
    pointer: Option<(u32, i32)>,
    /// The committed state, if the field is enabled.
    committed: Option<V3Field>,
    pending: Pending,
    link: Link,
    fence: Fence,
    shown: Option<Shown>,
    /// The guest's text may hold edits it has not reported (`Ahead`).
    ahead: bool,
    /// The surrounding text the guest last committed crossed an edit on
    /// its way to the guest (P4), so Exo's text may hold edits the proxy
    /// takes the guest to have reported; and whether the one pending did.
    stale_report: bool,
    pending_report_crossed: bool,
    /// A region the guest shows other than Exo composed, for a known
    /// reason, since the guest last reported: later regions then differ
    /// for that reason ([`SpecText::judge_region`]).
    diverged: Option<&'static str>,
    /// The edits that reached the guest so far, and when the pending
    /// surrounding text was stated: a commit that applies it after an edit
    /// crossed that edit too (P4).
    edits: u64,
    edits_at_report: u64,
    /// The bytes before and after the committed cursor that preedit
    /// regions took since the guest last reported (X1).
    taken: Option<(usize, usize)>,
    /// Exo's text as the last region found it, for
    /// [`SpecText::judge_region`].
    region_model: Option<RegionModel>,
    /// The last `preedit_cursor` since a terminal event (H5).
    assembly: Option<i32>,
    /// The keys the host holds, by keycode, and the peeks so far: the
    /// sequence of the latest since the focus last moved, and their count.
    held: BTreeMap<u32, Held>,
    latest_peek: Option<u64>,
    peeks: u64,

    /// The step's decisions: what the specification asks, and what the
    /// proxy sent, with the callback of each sync after its request.
    expected: Decisions,
    sent: Decisions,
    synced: (Option<u32>, Option<u32>),
    /// The request the proxy sent last in the step, as far as its syncs go.
    after: After,
    /// The transactions the guest must get in the step, each with its
    /// preedit cursor.
    txns: Vec<(Txn, (i32, i32))>,
    /// What reached the proxy in this step, for a report.
    input: String,
    /// The steps whose decisions were not the specification's.
    pub wrong_decisions: Vec<String>,
    /// The regions after which the guest shows another preedit than Exo
    /// composed, with the check that failed ([`SpecText::judge_region`]).
    pub wrong_regions: Vec<(&'static str, String)>,
    /// The surrounding text the proxy must report to the host in the step,
    /// if a commit stated one, and the reports it sent. An `Option`: a step
    /// handles one request, so at most one commit.
    report: Option<(String, u32, u32)>,
    reported: Vec<(String, u32, u32)>,
    /// The content-type requests the proxy sent in the step that are not
    /// the committed field's.
    typed: Vec<String>,
    /// The steps whose reports to the host were not the specification's.
    pub wrong_reports: Vec<String>,
    pub counts: Counts,
}

impl SpecText {
    /// Fields holding `texts`, on the host surfaces `surfaces` names.
    pub fn new(surfaces: Surfaces, texts: &[String]) -> Self {
        Self {
            surfaces,
            views: texts.iter().map(|text| View::new(text)).collect(),
            focus: None,
            shift: false,
            pointer: None,
            committed: None,
            pending: Pending::default(),
            link: Link::Detached,
            fence: Fence::Closed,
            shown: None,
            ahead: false,
            stale_report: false,
            pending_report_crossed: false,
            diverged: None,
            edits: 0,
            edits_at_report: 0,
            taken: None,
            region_model: None,
            assembly: None,
            held: BTreeMap::new(),
            latest_peek: None,
            peeks: 0,
            expected: Decisions::default(),
            sent: Decisions::default(),
            synced: (None, None),
            after: After::Other,
            txns: Vec::new(),
            input: String::new(),
            wrong_decisions: Vec::new(),
            wrong_regions: Vec::new(),
            report: None,
            reported: Vec::new(),
            typed: Vec::new(),
            wrong_reports: Vec::new(),
            counts: Counts::default(),
        }
    }

    /// The guest's next surrounding text crossed an edit on its way to the
    /// guest (P4), if `crossed`.
    pub fn report_crossed(&mut self, crossed: bool) {
        self.pending_report_crossed = crossed;
    }

    /// Follows one message at the proxy.
    pub fn see(&mut self, seen: Seen<'_>) {
        match seen {
            Seen::Guest(request) => {
                self.input = format!("{request:?}");
                self.guest(request);
            }
            Seen::Host {
                sender,
                event,
                origin,
                region,
            } => {
                self.input = format!("{sender} {event:?}");
                self.host(sender, event, origin, region);
            }
            Seen::ToHost(request) => self.decision(request),
            Seen::SurfaceDestroyed { field } => {
                self.input = format!("the destroy of field {field}'s surface");
                self.surface_destroyed(field);
            }
            Seen::ToGuest(_) | Seen::GuestPress { .. } => {}
            Seen::StepEnd => self.step_end(),
        }
    }

    /// The gate: host composition events reach the guest only while the
    /// enabled, focused field's text input is attached and no fence is
    /// open.
    fn gate(&self) -> bool {
        self.link == Link::Attached
            && self.fence == Fence::Closed
            && self.focus.is_some()
            && self.committed.is_some()
    }

    /// G3 to G9: the guest's requests, which count only while the text
    /// input is focused.
    fn guest(&mut self, request: &v3::Request) {
        use v3::Request;
        if self.focus.is_none() {
            return;
        }
        let pending = &mut self.pending;
        match request {
            Request::Enable {} | Request::Disable {} => {
                *pending = Pending {
                    enabled: Some(matches!(request, Request::Enable {})),
                    surrounding_text: Some(None),
                    content_type: Some((0, 0)),
                    cursor_rectangle: Some(None),
                };
            }
            Request::SetSurroundingText {
                text,
                cursor,
                anchor,
            } => {
                let IndexedString::Utf8(text) = text else {
                    panic!("a client model sends UTF-8");
                };
                let at = |offset: &i32| u32::try_from(*offset).expect("an offset in the text");
                pending.surrounding_text = Some(Some((text.clone(), at(cursor), at(anchor))));
                self.edits_at_report = self.edits;
            }
            &Request::SetContentType { hint, purpose } => {
                pending.content_type = Some((hint, purpose));
            }
            &Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => pending.cursor_rectangle = Some(Some((x, y, width, height))),
            Request::Commit {} => self.commit(),
            _ => {}
        }
    }

    /// G9c: a commit applies the pending state. One that changes whether
    /// the field is enabled starts a new field, which forgets the shown
    /// preedit; one that leaves it enabled is answered with the shown
    /// preedit.
    fn commit(&mut self) {
        let Pending {
            enabled,
            surrounding_text,
            content_type,
            cursor_rectangle,
        } = std::mem::take(&mut self.pending);
        // The commit reports a stated surrounding text to the host, enabled
        // or not.
        if let Some(text) = &surrounding_text {
            self.report = Some(host_surrounding_text(text.as_ref()));
        }
        let was_enabled = self.committed.is_some();
        let enabled = enabled.unwrap_or(was_enabled);
        if enabled != was_enabled {
            self.end_lease();
            self.shown = None;
            self.committed = enabled.then(V3Field::default);
        }
        let Some(field) = &mut self.committed else {
            return;
        };
        if let Some(text) = surrounding_text {
            field.surrounding_text = text;
            self.ahead = false;
            self.diverged = None;
            let crossed = std::mem::take(&mut self.pending_report_crossed);
            self.stale_report = crossed || self.edits != self.edits_at_report;
            if self.taken.take().is_some() {
                if let Some(shown) = &mut self.shown {
                    shown.host_cursor = None;
                }
            }
        }
        if let Some(content_type) = content_type {
            field.content_type = content_type;
        }
        if let Some(rectangle) = cursor_rectangle {
            field.cursor_rectangle = rectangle;
        }
        let (preedit, cursor) = self
            .shown
            .as_ref()
            .map_or_else(Default::default, |shown| (shown.text.clone(), shown.cursor));
        let txn = Txn {
            preedit,
            ..Txn::default()
        };
        self.apply(&txn, cursor);
    }

    /// What reached the proxy from the host.
    fn host(&mut self, sender: u32, event: &AnyEvent, origin: Origin, truth: Option<&RegionTruth>) {
        match event {
            AnyEvent::WlKeyboard(event) => self.keyboard(event),
            &AnyEvent::ZcrExtendedKeyboardV1(peek::Event::PeekKey { key, state, .. }) => {
                self.peek(key, state);
            }
            AnyEvent::WlPointer(wl_pointer::Event::Enter {
                surface, surface_x, ..
            }) => self.pointer = Some((*surface, *surface_x)),
            AnyEvent::WlPointer(wl_pointer::Event::Motion { surface_x, .. }) => {
                if let Some((_, x)) = &mut self.pointer {
                    *x = *surface_x;
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Leave { .. }) => self.pointer = None,
            AnyEvent::WlPointer(wl_pointer::Event::Button { state, .. })
                if *state == wl_pointer::BUTTON_STATE_PRESSED =>
            {
                // A press on a surface the guest destroyed lands nowhere:
                // the proxy drops an enter that names it, and forgets the
                // pointer's focus on it at the destroy. Exo sent the press
                // before the host's `delete_id`, so the proxy has not
                // reused the surface's ID yet.
                let (surface, x) = self.pointer.expect("a press on a surface");
                if !self.surfaces.host_destroyed(surface) {
                    self.press(surface, x);
                }
            }
            // The proxy drops a touch on a surface the guest destroyed.
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, .. })
                if self.surfaces.host_destroyed(*surface) => {}
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, x, .. }) => self.press(*surface, *x),
            AnyEvent::WlCallback(wl_callback::Event::Done { .. }) => {
                if self.link == Link::Draining(Some(sender)) {
                    self.link = Link::Detached; // W1
                }
                if self.fence == Fence::Open(Some(sender)) {
                    self.fence = Fence::Closed; // W2
                }
            }
            AnyEvent::ZwpTextInputV1(event) => self.text_input(event, origin),
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::ConfirmPreedit { .. }) => {
                // X2 confirms the shown preedit; X2' recovers a held key.
                self.composition_event(origin, |spec| match spec.commit_shown() {
                    Some(txn) => Some((txn, (0, 0))),
                    None => spec.recover_held_key(origin),
                });
            }
            &AnyEvent::ZcrExtendedTextInputV1(extended::Event::SetPreeditRegion {
                index,
                length,
            }) => self.composition_event(origin, |spec| {
                let edit = spec.region(index, length, origin);
                spec.judge_region(truth, edit.as_ref(), origin);
                edit
            }),
            _ => {}
        }
    }

    /// The keyboard: its focus, which resets the text input (F1), Shift,
    /// and its keys. The ideal client types every press the host sends.
    /// The keyboard concern may keep a press from the guest only by a rule
    /// of its own, and no case reaches one: Exo sends a key's press on
    /// `wl_keyboard` only once, and only if the input method did not
    /// consume it, so no press repeats a key held, leased or recovered
    /// before it, and Exo sends no keysym (H10).
    fn keyboard(&mut self, event: &wl_keyboard::Event) {
        match *event {
            // The proxy drops an enter or a leave that names a surface the
            // guest destroyed.
            wl_keyboard::Event::Enter { surface, .. }
            | wl_keyboard::Event::Leave { surface, .. }
                if self.surfaces.host_destroyed(surface) => {}
            wl_keyboard::Event::Enter { surface, .. } => {
                // The keys an enter lists are held, but had no peek, so none
                // is a key to recover; Exo's model lists none.
                self.held.clear();
                self.move_focus(Some(self.field(surface)));
            }
            wl_keyboard::Event::Leave { .. } => self.move_focus(None),
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                self.shift = mods_depressed & keymap::SHIFT != 0;
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                let typed = keymap::key(key, self.shift);
                let pressed = state == wl_keyboard::KEY_STATE_PRESSED;
                if pressed {
                    self.press_key(key);
                    if let Some(held) = self.held.get_mut(&key) {
                        held.forwarded = true;
                    }
                }
                if let (true, Some(field), Some(key)) = (pressed, self.focus, typed) {
                    self.counts.raw_keys += u64::from(self.link != Link::Attached);
                    self.views[field].model.insert_key(key);
                }
            }
            _ => {}
        }
    }

    /// F5: a `peek_key` of the host's `key` in `state`. A press is the
    /// latest peek, and a release ends the key's press.
    fn peek(&mut self, key: u32, state: u32) {
        if state != wl_keyboard::KEY_STATE_PRESSED {
            self.held.remove(&key);
            return;
        }
        self.press_key(key);
        self.peeks += 1;
        let sequence = self.peeks;
        self.held.insert(
            key,
            Held {
                sequence,
                ..Held::default()
            },
        );
        self.latest_peek = Some(sequence);
    }

    /// A press of any key but Backspace cancels a held Backspace's repeat
    /// for the rest of its press, lease or not.
    fn press_key(&mut self, key: u32) {
        let backspace = keymap::keycode(Key::Backspace).0;
        if key == backspace {
            return;
        }
        if let Some(held) = self.held.get_mut(&backspace) {
            self.counts.leases_ended += u64::from(held.leased);
            held.cancelled = true;
            held.leased = false;
        }
    }

    /// The text input's Backspace lease ends: an armed lease is cancelled
    /// for the rest of the press.
    fn end_lease(&mut self) {
        let backspace = keymap::keycode(Key::Backspace).0;
        if let Some(held) = self.held.get_mut(&backspace).filter(|held| held.leased) {
            self.counts.leases_ended += 1;
            held.cancelled = true;
            held.leased = false;
        }
    }

    /// The text input takes the lease of a held Backspace, unless its
    /// repeat was cancelled.
    fn arm_lease(&mut self) {
        let backspace = keymap::keycode(Key::Backspace).0;
        if let Some(held) = self.held.get_mut(&backspace).filter(|held| !held.cancelled) {
            self.counts.leases += u64::from(!held.leased);
            held.leased = true;
        }
    }

    /// X2': an empty confirmation stands for the eligible held key, the
    /// one with the latest peek, if it is the latest peek since the focus
    /// moved and not a cancelled Backspace. The guest gets its press and
    /// release, unless `wl_keyboard.key` delivered it already, and then a
    /// bare `done`. Recovering Backspace arms its lease; skipping it, or
    /// recovering another key, ends it, and skipping another key leaves it.
    /// With no eligible key, nothing goes out, and the lease ends.
    /// `origin` says which key's press, if any, the confirmation is Exo's
    /// confirmation before (E13).
    fn recover_held_key(&mut self, origin: Origin) -> Option<(Txn, (i32, i32))> {
        let backspace = keymap::keycode(Key::Backspace).0;
        let latest = self.held.iter().max_by_key(|(_, held)| held.sequence);
        let eligible = latest.filter(|&(&key, held)| {
            self.latest_peek
                .is_none_or(|sequence| sequence == held.sequence)
                && !(key == backspace && held.cancelled)
        });
        let Some((&key, &held)) = eligible else {
            self.counts.confirms_unheld += 1;
            self.end_lease();
            return None;
        };
        if held.forwarded {
            self.counts.recoveries_skipped += 1;
            if key == backspace {
                self.end_lease();
            }
        } else {
            self.counts.recovered_keys += 1;
            let before = origin
                .press
                .is_some_and(|press| keymap::keycode(press).0 != key);
            self.counts.recoveries_before_a_press += u64::from(before);
            let field = self.focus.expect("a recovery in the focused field");
            if let Some(typed) = keymap::key(key, self.shift) {
                self.views[field].model.insert_key(typed);
            }
            match key == backspace {
                true => self.arm_lease(),
                false => self.end_lease(),
            }
        }
        Some((Txn::default(), (0, 0)))
    }

    /// The commit of the shown preedit, if the guest shows one, which ends
    /// the composition (X2, F1, H2).
    fn commit_shown(&mut self) -> Option<Txn> {
        let shown = self.shown.take()?;
        self.end_lease();
        self.edited();
        Some(Txn {
            commit: shown.text,
            ..Txn::default()
        })
    }

    /// F1: a focus change resets the text input. The field it leaves
    /// commits the preedit the guest shows, before the text input's
    /// `leave`, as mutter and ibus do at a focus-out, and as Exo's detach
    /// would (E6) if its confirmation reached the proxy before the
    /// keyboard's `leave` (E12).
    fn move_focus(&mut self, focus: Option<usize>) {
        if focus == self.focus {
            return;
        }
        if let Some(txn) = self.commit_shown() {
            self.apply(&txn, (0, 0));
        }
        self.reset_focus(focus);
    }

    /// The focus moves to `focus`, and the field it leaves ends its
    /// composition, which the guest drops at the text input's `leave`. The
    /// keyboard ends the seat's lease and forgets its latest peek.
    fn reset_focus(&mut self, focus: Option<usize>) {
        self.end_lease();
        self.latest_peek = None;
        if let Some(left) = self.focus {
            self.views[left].model.preedit.clear();
            self.views[left].preedit_cursor = (0, 0);
        }
        self.focus = focus;
        self.committed = None;
        self.pending = Pending::default();
        self.shown = None;
        self.ahead = false;
        self.taken = None;
    }

    /// F4: a press on the surface of the enabled, focused field while the
    /// gate is open and the guest shows a preedit commits it and resets the
    /// host behind a fence; the press then moves the cursor of the field it
    /// lands in.
    fn press(&mut self, surface: u32, x: i32) {
        let field = self.field(surface);
        if self.gate() && self.focus == Some(field) {
            if let Some(shown) = self.shown.take() {
                self.counts.press_commits += 1;
                self.end_lease();
                self.edited();
                self.assembly = None;
                self.expected.reset = true;
                let txn = Txn {
                    commit: shown.text,
                    ..Txn::default()
                };
                self.apply(&txn, (0, 0));
            }
        }
        let at = keymap::target(x).expect("a press at a target");
        self.views[field].model.click(at);
    }

    /// H2, H5, H7 and H8.
    fn text_input(&mut self, event: &v1::Event, origin: Origin) {
        match event {
            v1::Event::Leave {} if self.link == Link::Attached => {
                // H2: Exo detached on its own. Its detach confirmed the
                // composition it tracked before its `leave` (E6), so a
                // preedit the guest still shows is one Exo no longer
                // tracked (E4), below v11 of the extension, which the guest
                // commits.
                self.counts.host_detaches += 1;
                if let Some(txn) = self.commit_shown() {
                    self.apply(&txn, (0, 0));
                }
                self.leave_attached(Link::Detached);
            }
            &v1::Event::PreeditCursor { index } if self.link == Link::Attached => {
                self.assembly = Some(index);
            }
            v1::Event::PreeditString { text, commit, .. } => {
                let IndexedString::Utf8(text) = text else {
                    return self.composition_event(origin, |_| None);
                };
                self.composition_event(origin, |spec| {
                    let shown = spec.shown.take();
                    match text.is_empty() && shown.is_some() && commit.is_empty() {
                        true => spec.arm_lease(),
                        false => spec.end_lease(),
                    }
                    if text.is_empty() {
                        let commit = shown.map_or_else(String::new, |_| commit.clone());
                        // The reset commit is an edit that ends the
                        // composition, which gives the bytes regions took
                        // back as text.
                        if !commit.is_empty() {
                            spec.edited();
                        }
                        let txn = Txn {
                            commit,
                            ..Txn::default()
                        };
                        return Some((txn, (0, 0)));
                    }
                    let cursor = preedit_cursor(text, spec.assembly);
                    // Exo's cursor is where it sent the preedit's, or at
                    // its end.
                    let host_cursor = spec
                        .assembly
                        .and_then(|at| usize::try_from(at).ok())
                        .filter(|&at| text.is_char_boundary(at))
                        .unwrap_or(text.len());
                    spec.shown = Some(Shown {
                        text: text.clone(),
                        cursor,
                        origin,
                        host_cursor: Some(host_cursor),
                    });
                    let txn = Txn {
                        preedit: text.clone(),
                        ..Txn::default()
                    };
                    Some((txn, cursor))
                });
            }
            v1::Event::CommitString { text, .. } => {
                self.composition_event(origin, |spec| {
                    spec.shown = None;
                    spec.taken = None;
                    if !text.is_empty() {
                        spec.forward_edit();
                    }
                    spec.end_lease();
                    let txn = Txn {
                        commit: text.clone(),
                        ..Txn::default()
                    };
                    Some((txn, (0, 0)))
                });
            }
            &v1::Event::DeleteSurroundingText { index, length } => {
                self.composition_event(origin, |spec| spec.delete(index, length));
            }
            _ => {}
        }
    }

    /// An edit that ends the composition reached the guest, which has yet
    /// to report it: the bytes regions took are text again.
    fn edited(&mut self) {
        self.forward_edit();
        self.taken = None;
    }

    /// An edit of the input method reached the guest, which has yet to
    /// report it.
    fn forward_edit(&mut self) {
        self.ahead = true;
        self.edits += 1;
    }

    /// The committed surrounding text and cursor, without a selection, if
    /// the guest stated one.
    fn surrounding(&self) -> Option<(String, usize)> {
        let (text, cursor, anchor) = self.committed.as_ref()?.surrounding_text.clone()?;
        (cursor == anchor).then_some((text, cursor as usize))
    }

    /// H9: Exo's delete of `length` bytes from `index` of the committed
    /// surrounding text, or from the cursor if `index` is negative, which
    /// must hold the cursor and lie on character boundaries; an absolute
    /// one only while the guest's text is not ahead of its report.
    fn delete(&mut self, index: i32, length: u32) -> Option<(Txn, (i32, i32))> {
        let (text, cursor) = self.surrounding()?;
        if index >= 0 && self.ahead {
            self.counts.deletes_ahead += 1;
            return None;
        }
        let start = match index {
            ..0 => cursor as i64 + i64::from(index),
            _ => i64::from(index),
        };
        let Some((before, after)) = around(&text, cursor, start, length) else {
            self.counts.deletes_misplaced += 1;
            return None;
        };
        if (before, after) == (0, 0) {
            return None;
        }
        self.counts.deletes += 1;
        self.shown = None;
        self.end_lease();
        self.edited();
        let txn = Txn {
            delete: (before, after),
            ..Txn::default()
        };
        Some((txn, (0, 0)))
    }

    /// X1: the bytes from `index` before Exo's cursor, `length` long,
    /// become the preedit, at its cursor (H5) or where Exo's cursor was.
    /// Exo's text is the guest's, which is the committed text without the
    /// bytes regions took since the guest last reported, with the preedit
    /// Exo holds at the guest's cursor, Exo's cursor in it. The region
    /// must cover that preedit, and deletes only the rest.
    fn region(&mut self, index: i32, length: u32, origin: Origin) -> Option<(Txn, (i32, i32))> {
        let (text, cursor) = self.surrounding()?;
        let (taken_before, taken_after) = self.taken.unwrap_or_default();
        let held = self.shown.as_ref().and_then(|shown| {
            let at = shown.host_cursor?;
            Some((shown.text.clone(), at))
        });
        let (held, at) = held.unwrap_or_default();
        let start = cursor.checked_sub(taken_before)?;
        let end = cursor + taken_after;
        let exo_text = [text.get(..start)?, &held, text.get(end..)?].concat();
        let exo_cursor = start + at;
        self.region_model = Some(RegionModel {
            text: exo_text.clone(),
            cursor: exo_cursor,
            held: held.clone(),
            ahead: self.ahead || self.stale_report,
        });
        let region_start = exo_cursor as i64 + i64::from(index);
        let region = around(&exo_text, exo_cursor, region_start, length);
        let delete = region.and_then(|(before, after)| {
            Some((
                start.checked_sub(exo_cursor - before)?,
                (exo_cursor + after).checked_sub(start + held.len())?,
            ))
        });
        // A report Exo has yet to get moves what the proxy measures from.
        self.counts.regions_crossing_reports += u64::from(origin.crossed);
        // An empty region at Exo's cursor after the preedit it holds ends
        // the composition and leaves that preedit as text: the guest
        // commits it, as at X2.
        if region == Some((0, 0)) && !held.is_empty() && at == held.len() {
            self.counts.regions_ended += 1;
            self.shown = None;
            self.end_lease();
            self.edited();
            let txn = Txn {
                commit: held,
                ..Txn::default()
            };
            return Some((txn, (0, 0)));
        }
        let (Some((before, after)), Some(delete)) = (region, delete) else {
            self.counts.regions_dropped += 1;
            return None;
        };
        let preedit = exo_text[exo_cursor - before..exo_cursor + after].to_owned();
        // The committed text holds no byte of the preedit Exo holds.
        let over = self.taken.is_none() && !held.is_empty();
        self.counts.regions_over_preedits += u64::from(over);
        let at = self.assembly.unwrap_or(before as i32);
        let preedit_cursor = preedit_cursor(&preedit, Some(at));
        self.shown = None;
        self.end_lease();
        if !preedit.is_empty() {
            self.counts.regions += 1;
            self.counts.regions_over_regions += u64::from(self.taken.is_some());
            self.forward_edit();
            let taken = (taken_before + delete.0, taken_after + delete.1);
            self.taken = (taken != (0, 0)).then_some(taken);
            self.shown = Some(Shown {
                text: preedit.clone(),
                cursor: preedit_cursor,
                origin,
                host_cursor: Some(before),
            });
        }
        let txn = Txn {
            delete,
            preedit,
            ..Txn::default()
        };
        Some((txn, preedit_cursor))
    }

    /// X1, judged by what Exo did of the region (`truth`): the preedit the
    /// guest shows after `edit`, which the specification gives it, must be
    /// the text Exo composed. A region the specification drops is judged by
    /// the composition check. Where Exo made the region in another text
    /// than the one the specification takes Exo to hold, for a reason the
    /// proxy cannot know, the failure says which, so that only its known
    /// issue explains it: a report Exo could not match dropped the
    /// composition the specification takes Exo to hold (E4,
    /// `RegionOverUnheldPreedit`); a report the proxy committed had yet
    /// to reach Exo (`RegionCrossesReport`); or, Exo holding the composition
    /// the specification takes it to, Exo's text holds edits of the input
    /// method the guest has yet to report (P4, `RegionWhileAhead`). Until the guest
    /// reports, later regions differ for the same reason. Any other
    /// difference is the specification's.
    fn judge_region(
        &mut self,
        truth: Option<&RegionTruth>,
        edit: Option<&(Txn, (i32, i32))>,
        origin: Origin,
    ) {
        let model = self.region_model.take();
        let (Some(truth), Some((txn, _)), Some(model)) = (truth, edit, model) else {
            return;
        };
        if txn.preedit == truth.composed {
            return;
        }
        let check = if let Some(check) = self.diverged {
            check
        } else if truth.dropped && truth.held.is_empty() && !model.held.is_empty() {
            "region over an unheld preedit"
        } else if origin.crossed {
            "region crossing a report"
        } else if model.held != truth.held
            || (&model.text, model.cursor) == (&truth.text, truth.cursor)
        {
            "region"
        } else if model.ahead {
            "region while ahead"
        } else {
            "region"
        };
        self.counts.regions_unheld += u64::from(check == "region over an unheld preedit");
        self.counts.regions_ahead += u64::from(check == "region while ahead");
        // The guest shows other text than Exo holds until it reports.
        self.diverged = (check != "region").then_some(check);
        let detail = format!(
            "after {}: the guest shows {:?}; Exo composed {:?} in {:?} at {} over {:?}, which the \
             proxy took for {:?} at {} over {:?}",
            self.input,
            txn.preedit,
            truth.composed,
            truth.text,
            truth.cursor,
            truth.held,
            model.text,
            model.cursor,
            model.held
        );
        if self.wrong_regions.len() < 5 {
            self.wrong_regions.push((check, detail));
        }
    }

    /// A terminal host composition event, which takes the assembly: through
    /// the gate, `txn` says what the guest gets, if anything, and with
    /// which preedit cursor; without it, the event is dropped.
    fn composition_event(
        &mut self,
        origin: Origin,
        txn: impl FnOnce(&mut Self) -> Option<(Txn, (i32, i32))>,
    ) {
        if !self.gate() {
            self.assembly = None;
            self.counts.fence_drops += u64::from(self.link == Link::Attached);
            return;
        }
        let edit = txn(self);
        self.assembly = None;
        if let Some((txn, cursor)) = edit {
            self.counts.late_shown += u64::from(origin.late);
            self.apply(&txn, cursor);
        }
    }

    /// Leaves `Attached` for `link`, which ends the composition.
    fn leave_attached(&mut self, link: Link) {
        self.link = link;
        self.shown = None;
        self.taken = None;
        self.assembly = None;
    }

    /// A request the proxy sent the host, as far as the IME decides it.
    fn decision(&mut self, request: &AnyRequest) {
        if let AnyRequest::ZwpTextInputV1(v1::Request::SetSurroundingText {
            text: IndexedString::Utf8(text),
            cursor,
            anchor,
        }) = request
        {
            self.reported.push((text.clone(), *cursor, *anchor));
        }
        // The step's commit came before the proxy sent anything.
        let committed = self.committed.as_ref();
        let matches = committed.and_then(|field| {
            let surrounding = field.surrounding_text.is_some();
            content_type_matches(request, field.content_type, surrounding)
        });
        let typed = is_content_type(request);
        if typed && matches != Some(true) {
            self.typed
                .push(format!("{request:?} for the committed {committed:?}"));
        }
        let sent = &mut self.sent;
        let after = std::mem::take(&mut self.after);
        match *request {
            AnyRequest::ZwpTextInputV1(v1::Request::Activate { surface, .. }) => {
                sent.activate = Some(surface);
            }
            AnyRequest::ZwpTextInputV1(v1::Request::Deactivate { .. }) => {
                sent.deactivate = true;
                self.after = After::Deactivate;
            }
            AnyRequest::ZwpTextInputV1(v1::Request::Reset {}) => {
                sent.reset = true;
                self.after = After::Reset;
            }
            // Only the sync right after a deactivate or a reset is the
            // proxy's own barrier or fence.
            AnyRequest::WlDisplay(wl_display::Request::Sync { callback }) => match after {
                After::Deactivate => {
                    sent.syncs += 1;
                    self.synced.0 = Some(callback);
                }
                After::Reset => {
                    sent.syncs += 1;
                    self.synced.1 = Some(callback);
                }
                After::Other => {}
            },
            _ => {}
        }
    }

    /// The end of a step: the reconcile the specification runs after every
    /// input (2, Reconcile; R2). The field wants input while it is enabled
    /// and focused. A link that differs from that wish is deactivated
    /// behind a barrier, or activated on the focused surface; a draining
    /// one waits for its barrier. The proxy's decisions must be these.
    fn step_end(&mut self) {
        let wants = self.focus.is_some() && self.committed.is_some();
        match self.link {
            Link::Attached if !wants => {
                self.expected.deactivate = true;
                self.leave_attached(Link::Draining(self.synced.0));
            }
            Link::Detached if wants => {
                let field = self.focus.expect("a focused field");
                self.expected.activate = self.surfaces.host(field);
                self.link = Link::Attached;
            }
            _ => {}
        }
        if self.expected.reset {
            self.fence = Fence::Open(self.synced.1);
        }
        let expected = &mut self.expected;
        expected.syncs = u32::from(expected.deactivate) + u32::from(expected.reset);
        if self.sent != *expected && self.wrong_decisions.len() < 5 {
            self.wrong_decisions.push(format!(
                "after {}: the proxy sent {:?}, the specification asks {:?}",
                self.input, self.sent, expected
            ));
        }
        self.expected = Decisions::default();
        self.sent = Decisions::default();
        self.synced = (None, None);
        self.check_reports();
    }

    /// The reports the proxy sent the host in the step must be what the
    /// guest committed in it: the surrounding text a commit stated, once,
    /// and every content type and surrounding-text support as the committed
    /// field's.
    fn check_reports(&mut self) {
        let reported = std::mem::take(&mut self.reported);
        let expected: Vec<_> = self.report.take().into_iter().collect();
        let mut wrong = Vec::new();
        if reported != expected {
            wrong.push(format!(
                "the surrounding text {reported:?}, the specification asks {expected:?}"
            ));
        }
        wrong.append(&mut self.typed);
        if !wrong.is_empty() && self.wrong_reports.len() < 5 {
            let wrong = wrong.join("; ");
            self.wrong_reports
                .push(format!("after {}: the proxy reported {wrong}", self.input));
        }
    }

    /// The guest gets `txn`, which it applies to the focused field.
    fn apply(&mut self, txn: &Txn, cursor: (i32, i32)) {
        let field = self.focus.expect("a transaction for the focused field");
        self.views[field].apply(txn, cursor);
        let cursor = self.views[field].preedit_cursor;
        self.txns.push((txn.clone(), cursor));
    }

    fn field(&self, surface: u32) -> usize {
        let field = self.surfaces.field_of_host(surface);
        field.unwrap_or_else(|| panic!("host surface {surface} has no field"))
    }

    /// F2: the guest destroyed field `field`'s surface. If the text input
    /// is focused on it, it leaves it while the surface is still valid, and
    /// its focus is projected again from the seat's keyboard focus, which
    /// the destroyed surface had and which no surface has now. Its field
    /// is reset as at any focus change, but commits nothing: the guest
    /// destroyed the window it would go into.
    fn surface_destroyed(&mut self, field: usize) {
        self.counts.surface_destroys += 1;
        if self.focus == Some(field) {
            self.counts.focused_surface_destroys += 1;
            self.held.clear();
            self.reset_focus(None);
        }
    }
}

/// `txn` as a client applies it to `model`: its delete clamped to the
/// text and shrunk to whole characters. The proxy measures a delete
/// against the text the guest last reported, which an ideal client, which
/// does not lag or drop as the client model does, may not have.
fn clamped(model: &TextModel, txn: &Txn) -> Txn {
    let (text, cursor) = (&model.text, model.cursor);
    let (before, after) = txn.delete;
    let mut start = cursor.saturating_sub(before);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = cursor.saturating_add(after).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Txn {
        delete: (cursor - start, end - cursor),
        ..txn.clone()
    }
}

/// The bytes before and after `cursor` of the range of `text` from `start`,
/// `length` long, if it holds the cursor and lies on character boundaries.
fn around(text: &str, cursor: usize, start: i64, length: u32) -> Option<(usize, usize)> {
    let start = usize::try_from(start).ok()?;
    let end = start.checked_add(length as usize)?;
    let fits = start <= cursor
        && cursor <= end
        && end <= text.len()
        && text.is_char_boundary(start)
        && text.is_char_boundary(end);
    fits.then(|| (cursor - start, end - cursor))
}

/// The cursor of a preedit `text` whose `preedit_cursor` was `index`: there,
/// if it is a character boundary of the text, and otherwise at its end
/// (H7).
fn preedit_cursor(text: &str, index: Option<i32>) -> (i32, i32) {
    let end = i32::try_from(text.len()).expect("a short preedit");
    match index {
        Some(index) if index < 0 => (-1, -1),
        Some(index) if text.is_char_boundary(index as usize) => (index, index),
        _ => (end, end),
    }
}

/// An ideal client fed what the proxy sent the guest: it applies each
/// transaction at `done` to the entered field, drops the preedit at
/// `leave`, and types keys and takes presses as [`TextModel`] does.
pub(super) struct Shadow {
    /// Which field each guest surface is.
    surfaces: Surfaces,
    views: Vec<View>,
    /// The field the text input entered, and its surface.
    entered: Option<(usize, u32)>,
    /// The field the keyboard focuses.
    keyboard: Option<usize>,
    shift: bool,
    /// The transaction in progress, with its preedit cursor.
    txn: (Txn, (i32, i32)),
    /// The transactions the guest got in the step.
    txns: Vec<(Txn, (i32, i32))>,
}

impl Shadow {
    /// Fields holding `texts`, on the guest surfaces `surfaces` names.
    pub fn new(surfaces: Surfaces, texts: &[String]) -> Self {
        Self {
            surfaces,
            views: texts.iter().map(|text| View::new(text)).collect(),
            entered: None,
            keyboard: None,
            shift: false,
            txn: Default::default(),
            txns: Vec::new(),
        }
    }

    /// Follows one message at the proxy: what it sent the guest.
    pub fn see(&mut self, seen: Seen<'_>) {
        match seen {
            Seen::ToGuest(AnyEvent::ZwpTextInputV3(event)) => self.text_input(event),
            Seen::ToGuest(AnyEvent::WlKeyboard(event)) => match *event {
                wl_keyboard::Event::Enter { surface, .. } => {
                    self.keyboard = Some(self.field(surface));
                }
                wl_keyboard::Event::Leave { .. } => self.keyboard = None,
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    self.shift = mods_depressed & keymap::SHIFT != 0;
                }
                wl_keyboard::Event::Key { key, state, .. } => {
                    let typed = keymap::key(key, self.shift);
                    let pressed = state == wl_keyboard::KEY_STATE_PRESSED;
                    if let (true, Some(field), Some(key)) = (pressed, self.keyboard, typed) {
                        self.views[field].model.insert_key(key);
                    }
                }
                _ => {}
            },
            Seen::GuestPress { surface, at } => {
                let field = self.field(surface);
                self.views[field].model.click(at);
            }
            // The client's window with the keyboard focus is gone: a key
            // for it goes nowhere until the focus comes back.
            Seen::SurfaceDestroyed { field } if self.keyboard == Some(field) => {
                self.keyboard = None;
            }
            // The proxy leaves a surface the guest destroys in the step
            // that destroys it, while the surface is still valid (F2).
            Seen::StepEnd => {
                if let Some((_, surface)) = self.entered {
                    assert!(
                        !self.surfaces.guest_destroyed(surface),
                        "the text input stays entered on guest surface {surface}, which the guest destroyed"
                    );
                }
            }
            _ => {}
        }
    }

    fn text_input(&mut self, event: &v3::Event) {
        let (txn, cursor) = &mut self.txn;
        match event {
            &v3::Event::Enter { surface } => self.entered = Some((self.field(surface), surface)),
            v3::Event::Leave { .. } => {
                if let Some((field, _)) = self.entered.take() {
                    self.views[field].model.preedit.clear();
                    self.views[field].preedit_cursor = (0, 0);
                }
                self.txn = Default::default();
            }
            v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                txn.preedit = match text {
                    Some(IndexedString::Utf8(text)) => text.clone(),
                    _ => String::new(),
                };
                *cursor = (*cursor_begin, *cursor_end);
            }
            v3::Event::CommitString { text } => txn.commit = text.clone().unwrap_or_default(),
            &v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => txn.delete = (before_length as usize, after_length as usize),
            v3::Event::Done { .. } => {
                let (txn, cursor) = std::mem::take(&mut self.txn);
                let (field, _) = self.entered.expect("a done while entered");
                let view = &mut self.views[field];
                // The view keeps the cursor as sent, an empty preedit's
                // included, which the specification holds to (0, 0).
                view.model.apply_txn(&clamped(&view.model, &txn));
                view.preedit_cursor = cursor;
                self.txns.push((txn, cursor));
            }
        }
    }

    /// The field on guest surface `surface`, which the guest has not
    /// destroyed: the proxy names no surface after the guest's destroy
    /// reached it.
    fn field(&self, surface: u32) -> usize {
        assert!(
            !self.surfaces.guest_destroyed(surface),
            "the proxy named guest surface {surface}, which the guest destroyed"
        );
        let field = self.surfaces.field_of_guest(surface);
        field.unwrap_or_else(|| panic!("guest surface {surface} has no field"))
    }
}

/// Tier 1 at the end of each step: the guest got the transactions the
/// specification asks, in order, and shows the same in every field, cursor
/// and preedit cursor included. Returns the difference, if any, and starts
/// the next step.
pub(super) fn step(spec: &mut SpecText, shadow: &mut Shadow) -> Option<String> {
    let (expected, got) = (
        std::mem::take(&mut spec.txns),
        std::mem::take(&mut shadow.txns),
    );
    if expected != got {
        return Some(format!(
            "after {}: the guest got the transactions {got:?}, the specification asks {expected:?}",
            spec.input
        ));
    }
    (spec.views != shadow.views).then(|| {
        let views = |views: &[View]| views.iter().map(show).collect::<Vec<_>>().join(", ");
        format!(
            "after {}: spec [{}], shadow [{}]",
            spec.input,
            views(&spec.views),
            views(&shadow.views)
        )
    })
}

/// Tier 1 at quiescence:
///
/// - the steps that failed ([`step`]), the first few, `steps`;
/// - `shadow` against `spec`, per field;
/// - the proxy's decisions against the specification's;
/// - the guest's preedit against what Exo composes;
/// - Exo's field on the focused surface against what the guest last
///   committed, for a host whose text-input extension is `extension`.
///
/// Exo composes in two places that can disagree for a while, so the
/// preedit must match one of them: the engine's composition (`engine`), or
/// the one Exo tracks in the field's text, which a report Exo cannot match
/// to its prediction drops (E4). The engine can also drop the composition
/// the guest shows, by a reset Exo's client never hears of: a late answer
/// shows a composition the reset behind its key dropped (E5, P3), and IMA
/// resets the engine on its own, as at a content type change. Of such a
/// preedit, which `resets`, the engine resets so far, tells, Exo may
/// compose nothing at all.
/// And a host without the text-input extension may compose a reconverted
/// region that the guest never shows.
pub(super) fn exact(
    steps: &[String],
    spec: &SpecText,
    shadow: &Shadow,
    exo: &Exo,
    engine: &str,
    resets: u64,
    extension: Option<u32>,
) -> Vec<Failure> {
    let mut failures = Vec::new();
    let failure = |check, field, detail| Failure {
        tier: Tier::Exact,
        check,
        field,
        detail,
    };
    for (i, (spec, shadow)) in spec.views.iter().zip(&shadow.views).enumerate() {
        if spec != shadow {
            let detail = format!("spec:   {}\nshadow: {}", show(spec), show(shadow));
            failures.push(failure("text", Some(i), detail));
        }
    }
    if !steps.is_empty() {
        failures.push(failure("step", None, steps.join("\n")));
    }
    if !spec.wrong_decisions.is_empty() {
        failures.push(failure("decision", None, spec.wrong_decisions.join("\n")));
    }
    for (check, detail) in &spec.wrong_regions {
        failures.push(failure(check, spec.focus, detail.clone()));
    }
    if !spec.wrong_reports.is_empty() {
        failures.push(failure("report", None, spec.wrong_reports.join("\n")));
    }
    let recoveries = spec.counts.recoveries_before_a_press;
    if recoveries > 0 {
        let detail = format!(
            "{recoveries} held keys recovered for Exo's confirmation before another key's press"
        );
        failures.push(failure("recovery", spec.focus, detail));
    }
    if let (Some(field), Some(tracked)) = (spec.focus, exo.composition()) {
        let preedit = &spec.views[field].model.preedit;
        let dropped = spec.shown.as_ref().is_some_and(|shown| {
            (shown.origin.late || shown.origin.resets < resets) && engine.is_empty()
        });
        // Without the extension, Exo composes a reconverted region the
        // guest never sees: while it stands uncommitted, the guest still
        // shows the composition that stood before it.
        let tracked = exo.composition_shown_instead().unwrap_or(tracked);
        if *preedit != tracked && preedit != engine && !(dropped && tracked.is_empty()) {
            let detail = format!(
                "the guest shows {preedit:?}; Exo tracks {tracked:?}, its engine composes {engine:?}"
            );
            failures.push(failure("composition", Some(field), detail));
        }
    }
    let expected: Vec<exo::Field> = spec
        .committed
        .iter()
        .map(|field| field.on_host(extension.unwrap_or(0)))
        .collect();
    let held = exo.focused_fields();
    if held != expected {
        let detail = format!("committed: {expected:?}\nExo holds: {held:?}");
        failures.push(failure("field state", spec.focus, detail));
    }
    failures
}

/// Tier 2: each of `client`'s fields against `reference`'s, and the focus.
pub(super) fn intended(reference: &Reference, client: &dyn Client) -> Vec<Failure> {
    let mut failures = Vec::new();
    let failure = |check, field, detail| Failure {
        tier: Tier::Intent,
        check,
        field,
        detail,
    };
    let fields = client.fields().iter().map(|field| &field.model);
    for (i, (intended, got)) in reference.fields().iter().zip(fields).enumerate() {
        if intended != got {
            let detail = format!(
                "intended: {}\nclient:   {}\nclient drops: {:?}",
                show_model(intended),
                show_model(got),
                client.drops()
            );
            failures.push(failure("text", Some(i), detail));
        }
    }
    if client.focused_field() != Some(reference.focus()) {
        let detail = format!(
            "intended field {}, client {:?}",
            reference.focus(),
            client.focused_field()
        );
        failures.push(failure("focus", None, detail));
    }
    failures
}

/// A tier-2 failure the proxy is known to cause: counted, not failed. The
/// PR that fixes one deletes its variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum KnownIssue {
    /// P1: a focus move away from a field that composes loses the
    /// composition in a client that does not keep it itself (GTK).
    FocusWhileComposing,
    /// A restart of the input method while the guest shows a composition
    /// that Exo no longer tracks loses it, below v11 of the extension.
    RestartLosesUntrackedComposition,
    /// An empty confirmation before the press of a key the input method
    /// did not handle (E13) recovers the key held before it, as if it had
    /// repeated (X2').
    RecoveryBeforeAPress,
    /// A preedit region Exo made from text older than the guest's report
    /// the proxy already committed is measured in that newer report (X1).
    /// The report was on its way to Exo, or the proxy sent it while the
    /// region was on its way to the proxy.
    RegionCrossesReport,
    /// A range IMA held for a key and sent after that key's commit
    /// composes the committed text, which the guest shows as a preedit
    /// that Exo drops at its next report (E4).
    StaleRegionAfterCommit,
    /// A preedit region over a composition Exo dropped on a report it
    /// could not match (E4), which the proxy still takes Exo to hold, is
    /// measured as if Exo held it (X1).
    RegionOverUnheldPreedit,
    /// A preedit region Exo made in text that holds edits of the input
    /// method the guest has yet to report is measured in the text it last
    /// reported (X1, P4).
    RegionWhileAhead,
}

impl KnownIssue {
    /// The sets of issues a case can show, the smallest first.
    const SETS: [&'static [Self]; 3] = [
        &[Self::FocusWhileComposing],
        &[Self::RestartLosesUntrackedComposition],
        &[
            Self::FocusWhileComposing,
            Self::RestartLosesUntrackedComposition,
        ],
    ];

    /// Why the proxy fails the user here.
    pub fn reason(self) -> &'static str {
        match self {
            Self::FocusWhileComposing => {
                "the keyboard hears of the focus move first (E12), so the proxy resets the \
                 field (F1) and sends the guest only `leave`, at which GTK drops its preedit; \
                 Exo's detach then confirms the composition, but the link is draining and the \
                 proxy drops the confirmation"
            }
            Self::StaleRegionAfterCommit => {
                "IMA holds a reconversion's range while a key is in flight and sends it after \
                 that key's commit, which ended the region; Exo composes the committed text \
                 again, the proxy shows it (X1), and the guest's next report, which leaves the \
                 preedit out, makes Exo drop it (E4), so the guest shows a preedit nothing \
                 composes"
            }
            Self::RegionOverUnheldPreedit => {
                "a report Exo could not match to its prediction, as a GTK3 report of a tap's \
                 cursor that lags behind a late answer, dropped the composition Exo held (E4), \
                 which nothing tells the proxy; it still measures a region over the preedit the \
                 guest shows, which Exo no longer holds, so the guest shows other bytes than Exo \
                 composes (X1)"
            }
            Self::RegionWhileAhead => {
                "Exo makes a region in its prediction, which holds the input method's edits \
                 that the guest has yet to report, as a commit after a delete the guest's \
                 report crossed, or a press's commit; the proxy measures it in the text the \
                 guest last reported, which tracks only the bytes regions took, so the guest \
                 shows other bytes than Exo composes (X1, P4)"
            }
            Self::RegionCrossesReport => {
                "Exo makes a region in the text it had before a report of the guest's, which \
                 moved the cursor or left out the preedit Exo holds, reached it: the report was \
                 on its way to Exo, or the proxy sent it while the region was on its way back; \
                 the proxy has already committed that report and measures the region in it \
                 (X1), so the guest shows other bytes than Exo composes"
            }
            Self::RecoveryBeforeAPress => {
                "at v11 Exo confirms before every key the input method did not handle (E13), \
                 and that key's peek comes only after its confirmation (E8), so the proxy takes \
                 the confirmation for one of the key held before, which is still the latest \
                 peek, and sends the guest a press and release of it (X2'): a held Backspace \
                 deletes once more, a held Space types once more"
            }
            Self::RestartLosesUntrackedComposition => {
                "a report Exo could not match to its prediction dropped the composition Exo \
                 tracks (E4), and below v11 of the extension Exo's detach confirms only a \
                 tracked composition (E6), so it confirms nothing; at its `leave` the proxy \
                 clears the preedit the guest shows (H2)"
            }
        }
    }

    /// Where its fix is tracked.
    pub fn tracking(self) -> &'static str {
        match self {
            Self::FocusWhileComposing => "docs/IME_REDESIGN.md 1.3 P1; fixed by C5",
            Self::RestartLosesUntrackedComposition => {
                "docs/IME_REDESIGN.md 2.2 H2; C5 decides whether H2 commits the shown preedit"
            }
            Self::RecoveryBeforeAPress => {
                "docs/IME_REDESIGN.md 2.2 X2', with REVIEW2's held-key peek sequence"
            }
            Self::RegionCrossesReport => {
                "docs/IME_REDESIGN.md 2.2 X1; the core must measure a region against the text \
                 Exo had"
            }
            Self::StaleRegionAfterCommit => {
                "docs/IME_REDESIGN.md 2.2 X1; IMA's ordering, made lasting by the proxy's \
                 reports without the preedit"
            }
            Self::RegionOverUnheldPreedit => {
                "docs/IME_REDESIGN.md 2.2 X1 and HANDOVER; needs the proxy to tell which \
                 reports Exo matched"
            }
            Self::RegionWhileAhead => {
                "docs/IME_REDESIGN.md 2.2 X1 and HANDOVER; needs the proxy to track the text the \
                 guest gained, not only the bytes it lost"
            }
        }
    }
}

/// The known issues that explain a case's `failures`, if any do: tier 1
/// passed, and the client shows exactly what a set of issues predicts
/// together (`predicts`, asked of each set, the smallest first, and only
/// then); or every failure is of tier 1 and a known issue's trigger
/// explains it: a recovery check, of a key recovered for a confirmation
/// before another's press; a composition check, in a case with a region
/// that crossed a report or was stale (`counts`); and a region check that
/// says why Exo made the region in other text than the proxy knew
/// ([`SpecText::judge_region`]).
pub(super) fn classify(
    failures: &[Failure],
    counts: &Counts,
    mut predicts: impl FnMut(&[KnownIssue]) -> bool,
) -> Vec<KnownIssue> {
    // Tier 1 failures that a known issue's trigger explains.
    let mut known = Vec::new();
    let mut unexplained = Vec::new();
    for failure in failures {
        let issue = match (failure.tier, failure.check) {
            (Tier::Exact, "composition") if counts.regions_crossing_reports > 0 => {
                KnownIssue::RegionCrossesReport
            }
            (Tier::Exact, "composition") if counts.regions_stale > 0 => {
                KnownIssue::StaleRegionAfterCommit
            }
            (Tier::Exact, "composition") if counts.regions_unheld > 0 => {
                KnownIssue::RegionOverUnheldPreedit
            }
            (Tier::Exact, "composition") if counts.regions_ahead > 0 => {
                KnownIssue::RegionWhileAhead
            }
            (Tier::Exact, "region over an unheld preedit") => KnownIssue::RegionOverUnheldPreedit,
            (Tier::Exact, "region crossing a report") => KnownIssue::RegionCrossesReport,
            (Tier::Exact, "region while ahead") => KnownIssue::RegionWhileAhead,
            (Tier::Exact, "recovery") => KnownIssue::RecoveryBeforeAPress,
            _ => {
                unexplained.push(failure);
                continue;
            }
        };
        if !known.contains(&issue) {
            known.push(issue);
        }
    }
    if !known.is_empty() {
        return match unexplained.is_empty() {
            true => known,
            false => Vec::new(),
        };
    }
    let intent_only = !failures.is_empty() && failures.iter().all(|f| f.tier == Tier::Intent);
    let mut sets = KnownIssue::SETS.into_iter();
    match intent_only {
        true => sets.find(|set| predicts(set)).unwrap_or_default().to_vec(),
        false => Vec::new(),
    }
}

/// What the user did in a case, and what the host did of it that a known
/// issue's prediction needs.
pub(super) struct Played<'a> {
    /// The fields' initial texts.
    pub texts: &'a [String],
    pub typing: Typing,
    pub actions: &'a [UserAction],
    /// For each restart, click, tap and focus move, in order, what Exo's
    /// own detach did there.
    pub detaches: &'a [Detach],
}

/// Whether `client` shows exactly what `issues` predict of `played`: the
/// user's intent, except that each issue loses the composition where it
/// strikes instead of committing it. P1 strikes at each focus move away
/// from a composing field that the keyboard heard of first; the lost
/// detach at each restart, and each focus move the text input heard of
/// first, whose detach confirmed nothing.
pub(super) fn issues_predict(issues: &[KnownIssue], played: &Played, client: &dyn Client) -> bool {
    let fields = played
        .texts
        .iter()
        .map(|text| TextModel::new(text))
        .collect();
    let mut reference = Reference::new(fields).typing(played.typing);
    let mut detaches = played.detaches.iter();
    for &action in played.actions {
        let detach = match action {
            UserAction::RestartInputMethod
            | UserAction::Click { .. }
            | UserAction::Tap { .. }
            | UserAction::Focus(_) => detaches.next().copied().unwrap_or_default(),
            _ => Detach::default(),
        };
        let lost = detach.before_keyboard
            && detach.untracked
            && issues.contains(&KnownIssue::RestartLosesUntrackedComposition);
        let p1 = !detach.before_keyboard
            && issues.contains(&KnownIssue::FocusWhileComposing)
            && reference.leaves_composition(action);
        reference.apply_losing(action, lost || p1);
    }
    intended(&reference, client).is_empty()
}

/// A field as `"text with | at the cursor" preedit "…" at (begin, end)`.
fn show(view: &View) -> String {
    format!("{} at {:?}", show_model(&view.model), view.preedit_cursor)
}

/// A model as `"text with | at the cursor" preedit "…"`.
fn show_model(model: &TextModel) -> String {
    let (before, after) = model.text.split_at(model.cursor);
    format!(
        "{:?} preedit {:?}",
        format!("{before}|{after}"),
        model.preedit
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1 as manager;
    use crate::sim::client::{gtk::Gtk, Field};
    use crate::sim::clock::Micros;
    use crate::sim::hangul::Key;

    /// The host surfaces of field 0 and field 1, and their guest ones.
    const HOST: [u32; 2] = [20, 21];
    const GUEST: [u32; 2] = [30, 31];
    /// The callbacks of the proxy's syncs.
    const SYNC: u32 = 40;

    /// The surfaces of the first `fields` fields.
    fn both(fields: usize) -> Surfaces {
        Surfaces::new(&GUEST[..fields], &HOST[..fields])
    }

    fn texts(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    /// A step: `event` reaches the proxy from the host, which sends
    /// `decisions`.
    fn host(spec: &mut SpecText, event: impl Into<AnyEvent>, decisions: &[AnyRequest]) {
        let event = event.into();
        spec.see(Seen::Host {
            sender: SYNC,
            event: &event,
            origin: Origin::default(),
            region: None,
        });
        decisions
            .iter()
            .for_each(|request| spec.see(Seen::ToHost(request)));
        spec.see(Seen::StepEnd);
    }

    /// Steps of the guest's `requests`, the last answered with `decisions`.
    fn guest(spec: &mut SpecText, requests: &[v3::Request], decisions: &[AnyRequest]) {
        for (i, request) in requests.iter().enumerate() {
            spec.see(Seen::Guest(request));
            if i + 1 == requests.len() {
                decisions
                    .iter()
                    .for_each(|request| spec.see(Seen::ToHost(request)));
            }
            spec.see(Seen::StepEnd);
        }
    }

    fn activate(field: usize) -> AnyRequest {
        v1::Request::Activate {
            seat: 1,
            surface: HOST[field],
        }
        .into()
    }

    fn deactivate() -> AnyRequest {
        v1::Request::Deactivate { seat: 1 }.into()
    }

    fn reset() -> AnyRequest {
        v1::Request::Reset {}.into()
    }

    fn sync() -> AnyRequest {
        wl_display::Request::Sync { callback: SYNC }.into()
    }

    fn enter(field: usize) -> wl_keyboard::Event {
        wl_keyboard::Event::Enter {
            serial: 1,
            surface: HOST[field],
            keys: Vec::new(),
        }
    }

    fn enable() -> [v3::Request; 2] {
        [v3::Request::Enable {}, v3::Request::Commit {}]
    }

    /// The spec of fields holding `fields`, the keyboard on field 0, its
    /// text input enabled and attached.
    fn attached(fields: &[&str]) -> SpecText {
        let mut spec = SpecText::new(both(2), &texts(fields));
        host(&mut spec, enter(0), &[]);
        guest(&mut spec, &enable(), &[activate(0), reported("", 0)]);
        spec
    }

    /// The proxy's report of `text`, with the cursor at `cursor`, to the
    /// host.
    fn reported(text: &str, cursor: u32) -> AnyRequest {
        v1::Request::SetSurroundingText {
            text: IndexedString::Utf8(text.to_owned()),
            cursor,
            anchor: cursor,
        }
        .into()
    }

    /// Exo's `preedit_cursor` and `preedit_string` of `text`.
    fn preedit(spec: &mut SpecText, text: &str) {
        let index = i32::try_from(text.len()).expect("a short preedit");
        host(spec, v1::Event::PreeditCursor { index }, &[]);
        let event = v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8(text.to_owned()),
            commit: text.to_owned(),
        };
        host(spec, event, &[]);
    }

    fn commit(text: &str) -> v1::Event {
        v1::Event::CommitString {
            serial: 0,
            text: text.to_owned(),
        }
    }

    fn tap(field: usize, at: Target) -> wl_touch::Event {
        wl_touch::Event::Down {
            serial: 0,
            time: 0,
            surface: HOST[field],
            id: 0,
            x: keymap::position(at),
            y: 0,
        }
    }

    fn view(text: &str, cursor: usize, preedit: &str) -> View {
        let preedit_cursor = match preedit.len() {
            0 => (0, 0),
            end => (end as i32, end as i32),
        };
        View {
            model: TextModel {
                text: text.to_owned(),
                cursor,
                preedit: preedit.to_owned(),
            },
            preedit_cursor,
        }
    }

    #[test]
    fn composition_events_reach_the_guest_only_through_the_gate() {
        // Not attached: dropped, but not by a fence.
        let mut spec = SpecText::new(both(1), &texts(&["0"]));
        host(&mut spec, enter(0), &[]);
        preedit(&mut spec, "가");
        assert_eq!(spec.views, [view("0", 1, "")]);
        // H7 shows a preedit at its cursor (H5); H8 ends it.
        let mut spec = attached(&["0", "1"]);
        preedit(&mut spec, "가");
        assert_eq!(spec.views[0], view("0", 1, "가"));
        host(&mut spec, commit("각"), &[]);
        assert_eq!(spec.views[0], view("0각", 4, ""));
        // X2 commits the shown preedit; X2' with none changes nothing.
        preedit(&mut spec, "ㄴ");
        let confirm = || extended::Event::ConfirmPreedit {
            selection_behavior: 1,
        };
        host(&mut spec, confirm(), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.views[0], view("0각ㄴ", 7, ""));
        assert_eq!(
            (spec.counts.fence_drops, &spec.wrong_decisions),
            (0, &vec![])
        );
    }

    #[test]
    fn a_preedit_cursor_is_where_exo_put_it_or_at_the_end() {
        assert_eq!(preedit_cursor("가나", Some(3)), (3, 3));
        assert_eq!(
            preedit_cursor("가나", Some(1)),
            (6, 6),
            "inside a character"
        );
        assert_eq!(preedit_cursor("가나", None), (6, 6));
        assert_eq!(preedit_cursor("가", Some(-1)), (-1, -1));
        // A terminal event takes the assembly, even a dropped one.
        let mut spec = attached(&["0"]);
        host(&mut spec, v1::Event::PreeditCursor { index: 0 }, &[]);
        host(&mut spec, commit(""), &[]);
        let event = v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8("가".to_owned()),
            commit: String::new(),
        };
        host(&mut spec, event, &[]);
        assert_eq!(spec.views[0].preedit_cursor, (3, 3));
    }

    #[test]
    fn a_press_commits_the_preedit_and_resets_behind_a_fence() {
        let mut spec = attached(&["0a", "1"]);
        preedit(&mut spec, "가");
        // F4 commits, then the press moves the cursor; the proxy must reset.
        host(&mut spec, tap(0, Target::Start), &[reset(), sync()]);
        assert_eq!(spec.views[0], view("0a가", 0, ""));
        assert_eq!(
            (spec.counts.press_commits, &spec.wrong_decisions),
            (1, &vec![])
        );
        // W2: the fence drops what Exo sent before the reset.
        preedit(&mut spec, "각");
        assert_eq!(spec.views[0], view("0a가", 0, ""));
        assert_eq!(spec.counts.fence_drops, 1);
        host(
            &mut spec,
            wl_callback::Event::Done { callback_data: 0 },
            &[],
        );
        preedit(&mut spec, "ㄴ");
        assert_eq!(spec.views[0], view("0a가", 0, "ㄴ"));
        // A press on the other field commits nothing; one that resets
        // anyway is a wrong decision.
        host(&mut spec, tap(1, Target::Start), &[reset(), sync()]);
        assert_eq!(spec.views[1], view("1", 0, ""));
        assert_eq!(spec.wrong_decisions.len(), 1);
    }

    #[test]
    fn the_link_follows_the_fields_wish_through_a_barrier() {
        let mut spec = SpecText::new(both(2), &texts(&["0", "1"]));
        host(&mut spec, enter(0), &[]);
        // An enabled, focused field must be activated on its surface.
        guest(&mut spec, &enable(), &[activate(1)]);
        assert_eq!(spec.wrong_decisions.len(), 1, "the wrong surface");
        let mut spec = attached(&["0", "1"]);
        // F1: the keyboard moves, so the field resets and must deactivate.
        host(&mut spec, enter(1), &[deactivate(), sync()]);
        // Enabled again while draining, it waits for the barrier (R2), then
        // activates.
        guest(&mut spec, &enable(), &[]);
        host(
            &mut spec,
            wl_callback::Event::Done { callback_data: 0 },
            &[activate(1)],
        );
        // H2: Exo detaches on its own, so the field activates again.
        host(&mut spec, v1::Event::Leave {}, &[activate(1)]);
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
        // A spurious reset, or a missing deactivate, is wrong.
        guest(&mut spec, &[v3::Request::Commit {}], &[reset(), sync()]);
        guest(
            &mut spec,
            &[v3::Request::Disable {}, v3::Request::Commit {}],
            &[],
        );
        assert_eq!(spec.wrong_decisions.len(), 2);
    }

    #[test]
    fn only_a_sync_after_a_deactivate_or_a_reset_is_a_decision() {
        let mut spec = attached(&["0"]);
        // A sync of the proxy's for something else is no IME decision.
        guest(&mut spec, &[v3::Request::Commit {}], &[sync()]);
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
        // A deactivate without its barrier is a wrong one.
        let disable = [v3::Request::Disable {}, v3::Request::Commit {}];
        guest(&mut spec, &disable, &[deactivate()]);
        assert_eq!(spec.wrong_decisions.len(), 1);
    }

    #[test]
    fn each_step_must_give_the_guest_the_transactions_the_specification_asks() {
        let mut spec = attached(&["0"]);
        let mut shadow = Shadow::new(both(1), &texts(&["0"]));
        let to_guest = |shadow: &mut Shadow, events: Vec<AnyEvent>| {
            events
                .iter()
                .for_each(|event| shadow.see(Seen::ToGuest(event)));
        };
        to_guest(
            &mut shadow,
            vec![v3::Event::Enter { surface: GUEST[0] }.into()],
        );
        // The answer to the enable: a bare done.
        to_guest(&mut shadow, vec![v3::Event::Done { serial: 1 }.into()]);
        assert_eq!(step(&mut spec, &mut shadow), None);
        preedit(&mut spec, "가");
        let shown = |cursor| -> Vec<AnyEvent> {
            vec![
                v3::Event::PreeditString {
                    text: Some(IndexedString::Utf8("가".to_owned())),
                    cursor_begin: cursor,
                    cursor_end: cursor,
                }
                .into(),
                v3::Event::Done { serial: 1 }.into(),
            ]
        };
        // A bogus preedit before the real one heals by the end, but not
        // within the step.
        let mut bogus = shadow.views.clone();
        to_guest(&mut shadow, shown(0));
        to_guest(&mut shadow, shown(3));
        assert!(step(&mut spec, &mut shadow).is_some_and(|f| f.contains("transactions")));
        assert_eq!(step(&mut spec, &mut shadow), None);
        // The same transactions, but a view that differs, as from a key.
        bogus[0].model.text.push('x');
        shadow.views = bogus;
        assert!(step(&mut spec, &mut shadow).is_some_and(|f| f.contains("spec [")));
    }

    #[test]
    fn exos_detach_or_a_focus_change_commits_the_preedit() {
        // H2: Exo's own detach commits the preedit it left unconfirmed,
        // and counts.
        let mut spec = attached(&["0", "1"]);
        preedit(&mut spec, "가");
        host(&mut spec, v1::Event::Leave {}, &[activate(0)]);
        assert_eq!(spec.views[0], view("0가", 4, ""));
        assert_eq!(spec.counts.host_detaches, 1);
        // F1: the keyboard leaves; the guest commits the preedit before the
        // text input's leave.
        let mut spec = attached(&["0", "1"]);
        preedit(&mut spec, "가");
        let leave = wl_keyboard::Event::Leave {
            serial: 2,
            surface: HOST[0],
        };
        host(&mut spec, leave, &[deactivate(), sync()]);
        assert_eq!(spec.views[0], view("0가", 4, ""));
        // Draining, the gate is shut.
        preedit(&mut spec, "각");
        host(&mut spec, enter(0), &[]);
        assert_eq!(spec.views[0], view("0가", 4, ""));
        // A leave that answers the proxy's deactivate is not Exo's own
        // (H2').
        host(&mut spec, v1::Event::Leave {}, &[]);
        assert_eq!(spec.counts.host_detaches, 0);
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
    }

    /// A step: the guest destroys field `field`'s surface, and the proxy
    /// sends `decisions`.
    fn destroy(spec: &mut SpecText, field: usize, decisions: &[AnyRequest]) {
        spec.surfaces.destroy(GUEST[field]);
        spec.see(Seen::SurfaceDestroyed { field });
        decisions
            .iter()
            .for_each(|request| spec.see(Seen::ToHost(request)));
        spec.see(Seen::StepEnd);
    }

    #[test]
    fn a_destroyed_surface_loses_the_focus_and_its_input_goes_nowhere() {
        // F2: the text input leaves the focused surface the guest destroys
        // and the field detaches, dropping the preedit; the seat focuses
        // no surface now.
        let pointer = || wl_pointer::Event::Enter {
            serial: 2,
            surface: HOST[0],
            surface_x: keymap::position(Target::Start),
            surface_y: 0,
        };
        let press = || wl_pointer::Event::Button {
            serial: 3,
            time: 0,
            button: 0x110,
            state: wl_pointer::BUTTON_STATE_PRESSED,
        };
        let mut spec = attached(&["0", "1"]);
        host(&mut spec, pointer(), &[]);
        preedit(&mut spec, "가");
        destroy(&mut spec, 0, &[deactivate(), sync()]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        assert_eq!(spec.focus, None);
        assert_eq!(
            (
                spec.counts.surface_destroys,
                spec.counts.focused_surface_destroys
            ),
            (1, 1)
        );
        host(
            &mut spec,
            wl_callback::Event::Done { callback_data: 0 },
            &[],
        );
        // A press Exo sent before it learned of the destroy lands nowhere,
        // as the proxy forgot the pointer's focus on the surface.
        host(&mut spec, press(), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        // The proxy drops the input that names the destroyed surface.
        host(&mut spec, enter(0), &[]);
        assert_eq!(spec.focus, None);
        host(&mut spec, tap(0, Target::Start), &[]);
        host(&mut spec, pointer(), &[]);
        host(&mut spec, press(), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        // Exo focuses the field's new surface, and the field attaches anew.
        spec.surfaces.add_host(Some(0), 22);
        let enter = wl_keyboard::Event::Enter {
            serial: 3,
            surface: 22,
            keys: Vec::new(),
        };
        host(&mut spec, enter, &[]);
        assert_eq!(spec.focus, Some(0));
        let activate = v1::Request::Activate {
            seat: 1,
            surface: 22,
        };
        guest(&mut spec, &enable(), &[activate.into(), reported("0", 1)]);
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
        // Destroying an unfocused surface changes no focus.
        destroy(&mut spec, 1, &[]);
        assert_eq!(spec.focus, Some(0));
        assert_eq!(
            (
                spec.counts.surface_destroys,
                spec.counts.focused_surface_destroys
            ),
            (2, 1)
        );
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
        // A destroy the proxy does not leave is a wrong step.
        let mut spec = attached(&["0", "1"]);
        destroy(&mut spec, 0, &[]);
        assert_ne!(spec.wrong_decisions, Vec::<String>::new());
    }

    /// The guest reports `text` with the cursor at `cursor`, and commits.
    fn report(spec: &mut SpecText, text: &str, cursor: i32) {
        let surrounding = v3::Request::SetSurroundingText {
            text: IndexedString::Utf8(text.to_owned()),
            cursor,
            anchor: cursor,
        };
        let report = reported(text, cursor as u32);
        guest(spec, &[surrounding, v3::Request::Commit {}], &[report]);
    }

    #[test]
    fn every_report_to_the_host_must_be_what_the_guest_committed_in_the_step() {
        let mut spec = attached(&["0ab"]);
        assert_eq!(spec.wrong_reports, Vec::<String>::new());
        let surrounding = |cursor| v3::Request::SetSurroundingText {
            text: IndexedString::Utf8("0ab".to_owned()),
            cursor,
            anchor: cursor,
        };
        let commit = || v3::Request::Commit {};
        // A report with another cursor, a missing report, and one no commit
        // stated.
        guest(
            &mut spec,
            &[surrounding(3), commit()],
            &[reported("0ab", 0)],
        );
        guest(&mut spec, &[surrounding(2), commit()], &[]);
        host(&mut spec, enter(0), &[reported("0ab", 2)]);
        // A content type that is not the committed one.
        let typed = v1::Request::SetContentType {
            hint: 0,
            purpose: v1::CONTENT_PURPOSE_PASSWORD,
        };
        guest(&mut spec, &[commit()], &[typed.into()]);
        assert_eq!(spec.wrong_reports.len(), 4, "{:?}", spec.wrong_reports);
    }

    #[test]
    fn exos_delete_reaches_the_guest_only_from_a_reported_text_around_the_cursor() {
        let delete = |index, length| v1::Event::DeleteSurroundingText { index, length };
        let mut spec = attached(&["0ab"]);
        // No surrounding text: dropped.
        host(&mut spec, delete(1, 2), &[]);
        assert_eq!(spec.views[0], view("0ab", 3, ""));
        // Absolute, of bytes 1 to 3.
        report(&mut spec, "0ab", 3);
        host(&mut spec, delete(1, 2), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        // The guest has not reported it: an absolute delete is dropped, one
        // from the cursor is not.
        host(&mut spec, delete(0, 1), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        host(&mut spec, delete(-1, 1), &[]);
        assert_eq!(spec.views[0], view("", 0, ""));
        // Reported, an empty range, or one past the text, is dropped.
        report(&mut spec, "0", 1);
        host(&mut spec, delete(1, 0), &[]);
        host(&mut spec, delete(1, 1), &[]);
        let counts = (
            spec.counts.deletes,
            spec.counts.deletes_ahead,
            spec.counts.deletes_misplaced,
        );
        assert_eq!(counts, (2, 1, 1));
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
    }

    #[test]
    fn a_region_over_a_region_deletes_only_what_the_guest_has() {
        let region = |index, length| extended::Event::SetPreeditRegion { index, length };
        let mut spec = attached(&["0ab"]);
        report(&mut spec, "0ab", 3);
        host(&mut spec, region(-1, 1), &[]);
        assert_eq!(spec.views[0], view("0a", 2, "b"));
        // The guest still has "0a": the committed text's "b" is gone.
        host(&mut spec, region(-2, 2), &[]);
        assert_eq!(spec.views[0], view("0", 1, "ab"));
        // One that does not cover the bytes regions took is dropped.
        host(&mut spec, region(-1, 1), &[]);
        assert_eq!(spec.views[0], view("0", 1, "ab"));
        let counts = |spec: &SpecText| {
            let c = spec.counts;
            (
                c.regions,
                c.regions_over_regions,
                c.regions_dropped,
                c.regions_over_preedits,
            )
        };
        assert_eq!(counts(&spec), (2, 1, 1, 0));
        // Once the guest reports its text, which lacks the region's bytes,
        // Exo drops its composition (E4): a region is measured from the
        // guest's cursor.
        report(&mut spec, "0", 1);
        host(&mut spec, region(-1, 1), &[]);
        assert_eq!(spec.views[0], view("", 0, "0"));
        assert_eq!(counts(&spec), (3, 1, 1, 0));
        // A commit gives the bytes back.
        host(&mut spec, commit("0"), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        // Exo holds a host preedit at the guest's cursor, its own cursor
        // after it: a region over it takes it with the bytes before it.
        report(&mut spec, "0", 1);
        preedit(&mut spec, "b");
        host(&mut spec, region(-2, 2), &[]);
        assert_eq!(spec.views[0], view("", 0, "0b"));
        assert_eq!(counts(&spec), (4, 1, 1, 1));
        // One that does not cover it is dropped.
        host(&mut spec, region(-1, 1), &[]);
        assert_eq!(spec.views[0], view("", 0, "0b"));
        assert_eq!(counts(&spec), (4, 1, 2, 1));
    }

    /// The host's `peek_key` of `key`, pressed or released.
    fn peek_key(key: Key, state: u32) -> peek::Event {
        peek::Event::PeekKey {
            serial: 1,
            time: 0,
            key: keymap::keycode(key).0,
            state,
        }
    }

    fn confirm() -> extended::Event {
        extended::Event::ConfirmPreedit {
            selection_behavior: 1,
        }
    }

    const DOWN: u32 = wl_keyboard::KEY_STATE_PRESSED;
    const UP: u32 = wl_keyboard::KEY_STATE_RELEASED;

    #[test]
    fn an_empty_confirmation_recovers_the_latest_held_key_unless_delivered() {
        let mut spec = attached(&["0"]);
        spec.txns.clear();
        let bare = || vec![(Txn::default(), (0, 0))];
        // Nothing held: nothing goes out.
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.txns, []);
        // A held Space the input method consumed: its press, then a done.
        host(&mut spec, peek_key(Key::Char(' '), DOWN), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(
            (&spec.views[0], std::mem::take(&mut spec.txns)),
            (&view("0 ", 2, ""), bare())
        );
        // One that wl_keyboard delivered: only the done.
        host(&mut spec, peek_key(Key::Char(' '), UP), &[]);
        host(&mut spec, peek_key(Key::Char('a'), DOWN), &[]);
        let key = wl_keyboard::Event::Key {
            serial: 1,
            time: 0,
            key: keymap::keycode(Key::Char('a')).0,
            state: DOWN,
        };
        host(&mut spec, key, &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(
            (&spec.views[0], std::mem::take(&mut spec.txns)),
            (&view("0 a", 3, ""), bare())
        );
        // A key held but not the latest peek: nothing.
        host(&mut spec, peek_key(Key::Char('s'), DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('s'), UP), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.txns, []);
        let c = spec.counts;
        let counts = (c.recovered_keys, c.recoveries_skipped, c.confirms_unheld);
        assert_eq!(counts, (1, 1, 2));
        // A held Space recovered for Exo's confirmation before Enter's press
        // stands for no repeat.
        host(&mut spec, peek_key(Key::Char(' '), DOWN), &[]);
        let before_enter = Origin {
            press: Some(Key::Enter),
            ..Origin::default()
        };
        let event = confirm().into();
        spec.see(Seen::Host {
            sender: SYNC,
            event: &event,
            origin: before_enter,
            region: None,
        });
        spec.see(Seen::StepEnd);
        assert_eq!(spec.views[0], view("0 a ", 4, ""));
        assert_eq!(spec.counts.recoveries_before_a_press, 1);
    }

    #[test]
    fn a_held_backspace_leases_its_repeat_until_an_edit_or_another_key() {
        let backspace = |state| peek_key(Key::Backspace, state);
        let cleared = || v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8(String::new()),
            commit: String::new(),
        };
        let mut spec = attached(&["0ab"]);
        preedit(&mut spec, "가");
        // Backspace takes back the composition, which arms the lease, and
        // its repeats are recovered.
        host(&mut spec, backspace(DOWN), &[]);
        host(&mut spec, cleared(), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.views[0], view("0a", 2, ""));
        assert_eq!((spec.counts.leases, spec.counts.leases_ended), (1, 0));
        // An edit ends it for the rest of the press.
        host(&mut spec, commit("x"), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.views[0], view("0ax", 3, ""));
        assert_eq!(spec.counts.leases_ended, 1);
        // So does another key's press, lease or not.
        host(&mut spec, backspace(UP), &[]);
        host(&mut spec, backspace(DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('a'), DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('a'), UP), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.views[0], view("0ax", 3, ""));
        assert_eq!(spec.counts.recovered_keys, 1);
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
    }

    #[test]
    fn a_guest_commit_is_answered_with_the_shown_preedit_and_records_the_field() {
        let mut spec = attached(&["0"]);
        preedit(&mut spec, "가");
        let surrounding = v3::Request::SetSurroundingText {
            text: IndexedString::Utf8("0".to_owned()),
            cursor: 1,
            anchor: 1,
        };
        guest(&mut spec, &[surrounding, v3::Request::Commit {}], &[]);
        assert_eq!(spec.views[0], view("0", 1, "가"));
        let field = V3Field {
            surrounding_text: Some(("0".to_owned(), 1, 1)),
            ..V3Field::default()
        };
        assert_eq!(spec.committed, Some(field));
        // A disable is a new field, which forgets the shown preedit; the
        // guest keeps showing it until the answer to the next enable, a
        // `done` with no preedit, clears it.
        let disable = [v3::Request::Disable {}, v3::Request::Commit {}];
        guest(&mut spec, &disable, &[deactivate(), sync()]);
        assert_eq!((&spec.committed, &spec.shown), (&None, &None));
        assert_eq!(spec.views[0], view("0", 1, "가"));
        guest(&mut spec, &enable(), &[]);
        assert_eq!(spec.views[0], view("0", 1, ""));
        assert_eq!(spec.wrong_decisions, Vec::<String>::new());
    }

    #[test]
    fn the_guest_types_every_key_the_host_presses() {
        let mut spec = attached(&["0"]);
        let key = |state| wl_keyboard::Event::Key {
            serial: 0,
            time: 0,
            key: keymap::keycode(Key::Char('a')).0,
            state,
        };
        host(&mut spec, key(wl_keyboard::KEY_STATE_PRESSED), &[]);
        host(&mut spec, key(wl_keyboard::KEY_STATE_RELEASED), &[]);
        host(&mut spec, key(wl_keyboard::KEY_STATE_PRESSED), &[]);
        assert_eq!(spec.views[0], view("0aa", 3, ""));
        assert_eq!(spec.counts.raw_keys, 0);
    }

    #[test]
    #[should_panic(expected = "the proxy named guest surface 30, which the guest destroyed")]
    fn the_shadow_fails_on_an_event_naming_a_destroyed_surface() {
        let surfaces = both(2);
        let mut shadow = Shadow::new(surfaces.clone(), &texts(&["0", "1"]));
        surfaces.destroy(GUEST[0]);
        shadow.see(Seen::SurfaceDestroyed { field: 0 });
        shadow.see(Seen::StepEnd);
        let enter = wl_keyboard::Event::Enter {
            serial: 1,
            surface: GUEST[0],
            keys: Vec::new(),
        };
        shadow.see(Seen::ToGuest(&enter.into()));
    }

    #[test]
    #[should_panic(expected = "the text input stays entered on guest surface 30")]
    fn the_shadow_fails_on_a_text_input_entered_on_a_destroyed_surface() {
        let surfaces = both(2);
        let mut shadow = Shadow::new(surfaces.clone(), &texts(&["0", "1"]));
        let enter = v3::Event::Enter { surface: GUEST[0] }.into();
        shadow.see(Seen::ToGuest(&enter));
        surfaces.destroy(GUEST[0]);
        shadow.see(Seen::SurfaceDestroyed { field: 0 });
        shadow.see(Seen::StepEnd);
    }

    #[test]
    fn the_shadow_applies_what_the_guest_got() {
        let mut shadow = Shadow::new(both(2), &texts(&["0", "1"]));
        let events: Vec<AnyEvent> = vec![
            v3::Event::Enter { surface: GUEST[0] }.into(),
            v3::Event::PreeditString {
                text: Some(IndexedString::Utf8("가".to_owned())),
                cursor_begin: 0,
                cursor_end: 3,
            }
            .into(),
            v3::Event::CommitString {
                text: Some("a".to_owned()),
            }
            .into(),
        ];
        events
            .iter()
            .for_each(|event| shadow.see(Seen::ToGuest(event)));
        // Nothing applies before `done`.
        assert_eq!(shadow.views[0], view("0", 1, ""));
        shadow.see(Seen::ToGuest(&v3::Event::Done { serial: 0 }.into()));
        let mut shown = view("0a", 2, "가");
        shown.preedit_cursor = (0, 3);
        assert_eq!(shadow.views[0], shown);
        // A press moves the cursor; `leave` drops the preedit.
        shadow.see(Seen::GuestPress {
            surface: GUEST[0],
            at: Target::Start,
        });
        shadow.see(Seen::ToGuest(
            &v3::Event::Leave { surface: GUEST[0] }.into(),
        ));
        assert_eq!(shadow.views[0], view("0a", 0, ""));
    }

    #[test]
    fn a_delete_that_does_not_fit_is_clamped_to_whole_characters() {
        let mut shadow = Shadow::new(both(1), &texts(&["a가"]));
        let delete = |before_length, after_length| -> [AnyEvent; 2] {
            [
                v3::Event::DeleteSurroundingText {
                    before_length,
                    after_length,
                }
                .into(),
                v3::Event::Done { serial: 0 }.into(),
            ]
        };
        let enter: AnyEvent = v3::Event::Enter { surface: GUEST[0] }.into();
        shadow.see(Seen::ToGuest(&enter));
        // Into 가, and past the end: nothing.
        for event in delete(1, 2) {
            shadow.see(Seen::ToGuest(&event));
        }
        assert_eq!(shadow.views[0], view("a가", 4, ""));
        // Past the start: what there is.
        for event in delete(9, 0) {
            shadow.see(Seen::ToGuest(&event));
        }
        assert_eq!(shadow.views[0], view("", 0, ""));
        let mut spec = attached(&["a가"]);
        // The specification's ideal client clamps the same way.
        let txn = Txn {
            delete: (2, 0),
            ..Txn::default()
        };
        spec.views[0].apply(&txn, (0, 0));
        assert_eq!(spec.views[0], view("a가", 4, ""));
    }

    #[test]
    fn exos_composition_must_be_the_engines_or_the_one_it_tracks() {
        // Exo attached a text input on field 0, and tracks no composition.
        let mut exo = Exo::default();
        exo.set_text_input_focus(Some(HOST[0]));
        exo.request(3, &manager::Request::CreateTextInput { id: 10 }.into());
        exo.request(10, &activate(0));
        let checks = |spec: &SpecText, engine: &str, resets| -> Vec<&'static str> {
            let shadow = Shadow {
                views: spec.views.clone(),
                ..Shadow::new(both(1), &[])
            };
            let failures = exact(&[], spec, &shadow, &exo, engine, resets, None);
            let checks = failures.iter().map(|failure| failure.check);
            checks.filter(|&check| check != "field state").collect()
        };
        let mut spec = attached(&["0"]);
        // The guest shows nothing, which Exo tracks.
        assert_eq!(checks(&spec, "가", 0), Vec::<&str>::new());
        // The guest shows what the engine composes.
        preedit(&mut spec, "가");
        assert_eq!(checks(&spec, "가", 0), Vec::<&str>::new());
        // Nothing composes it.
        assert_eq!(checks(&spec, "", 0), ["composition"]);
        // Unless the engine reset since it made it, without Exo's client
        // hearing of it.
        assert_eq!(checks(&spec, "", 1), Vec::<&str>::new());
        assert_eq!(checks(&spec, "각", 1), ["composition"]);
        // Or the guest got it late.
        let late = v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8("각".to_owned()),
            commit: "각".to_owned(),
        };
        let origin = Origin {
            late: true,
            ..Origin::default()
        };
        spec.see(Seen::Host {
            sender: 10,
            event: &late.into(),
            origin,
            region: None,
        });
        assert_eq!(checks(&spec, "", 0), Vec::<&str>::new());
        assert_eq!(spec.counts.late_shown, 1);
    }

    #[test]
    fn a_hidden_region_excuses_a_composition_only_while_it_stands() {
        use crate::sim::engine::{EngineOut, Request, TextAction};
        // Exo, with no extension, composes a region of "0a" that the guest
        // never sees.
        let mut exo = Exo::default();
        exo.set_text_input_focus(Some(HOST[0]));
        exo.request(3, &manager::Request::CreateTextInput { id: 10 }.into());
        exo.request(10, &activate(0));
        exo.request(10, &reported("0a", 2));
        exo.take_events();
        exo.take_engine();
        exo.ask(Request::Reconvert);
        exo.take_engine();
        let region = TextAction::Region {
            before: 1,
            after: 0,
        };
        exo.answer(&EngineOut::Text {
            id: 0,
            actions: vec![region],
        });
        exo.answer(&EngineOut::Done {
            id: 0,
            handled: true,
        });
        let spec = attached(&["0a"]);
        let checks = |exo: &Exo, engine: &str| -> Vec<&'static str> {
            let shadow = Shadow {
                views: spec.views.clone(),
                ..Shadow::new(both(1), &[])
            };
            let failures = exact(&[], &spec, &shadow, exo, engine, 0, None);
            let checks = failures.iter().map(|failure| failure.check);
            checks.filter(|&check| check == "composition").collect()
        };
        assert_eq!(checks(&exo, ""), Vec::<&str>::new());
        // Once a key composes over it, the guest must show that.
        exo.key(Micros(0), Key::Char('r'), wl_keyboard::KEY_STATE_PRESSED);
        exo.take_engine();
        exo.answer(&EngineOut::Text {
            id: 1,
            actions: vec![TextAction::Preedit("ㄱ".to_owned())],
        });
        exo.answer(&EngineOut::Done {
            id: 1,
            handled: true,
        });
        assert_eq!(checks(&exo, "ㄱ"), ["composition"]);
    }

    /// GTK4 showing `texts`, the keyboard on field `focus`.
    fn gtk(texts: [&str; 2], focus: usize) -> Gtk {
        let fields = texts
            .iter()
            .zip(GUEST)
            .map(|(text, surface)| Field::new(surface, TextModel::new(text)))
            .collect();
        let mut gtk = Gtk::new(true, 10, fields);
        gtk.keyboard_focus(Micros(0), Some(GUEST[focus]));
        gtk
    }

    #[test]
    fn the_intent_is_the_fields_and_the_focus() {
        let gtk = gtk(["가", "1"], 1);
        let fields = texts(&["가", "1"])
            .iter()
            .map(|text| TextModel::new(text))
            .collect();
        let mut reference = Reference::new(fields);
        reference.apply(UserAction::Focus(1));
        assert_eq!(intended(&reference, &gtk), []);
        reference.apply(UserAction::Key(Key::Char('a')));
        reference.apply(UserAction::Focus(0));
        let failures = intended(&reference, &gtk);
        let checks: Vec<_> = failures.iter().map(|f| (f.check, f.field)).collect();
        assert_eq!(checks, [("text", Some(1)), ("focus", None)]);
    }

    #[test]
    fn a_region_made_while_a_report_was_on_its_way_explains_a_composition() {
        let composition = Failure {
            tier: Tier::Exact,
            check: "composition",
            field: Some(0),
            detail: String::new(),
        };
        let crossing = Counts {
            regions_crossing_reports: 1,
            ..Counts::default()
        };
        let one = std::slice::from_ref(&composition);
        assert_eq!(
            classify(one, &crossing, |_| false),
            [KnownIssue::RegionCrossesReport]
        );
        assert_eq!(classify(one, &Counts::default(), |_| true), []);
        // The scheduler marks such a region, which SpecText counts.
        let mut spec = attached(&["0ab"]);
        report(&mut spec, "0ab", 3);
        let region = extended::Event::SetPreeditRegion {
            index: -1,
            length: 1,
        };
        let origin = Origin {
            crossed: true,
            ..Origin::default()
        };
        spec.see(Seen::Host {
            sender: SYNC,
            event: &region.into(),
            origin,
            region: None,
        });
        spec.see(Seen::StepEnd);
        assert_eq!(spec.counts.regions_crossing_reports, 1);
        let issue = KnownIssue::RegionCrossesReport;
        assert!(!issue.reason().is_empty() && issue.tracking().contains("X1"));
        // So does a range IMA sent after the commit of the key it held it
        // for.
        let stale = Counts {
            regions_stale: 1,
            ..Counts::default()
        };
        let issue = KnownIssue::StaleRegionAfterCommit;
        assert_eq!(classify(one, &stale, |_| false), [issue]);
        assert!(!issue.reason().is_empty() && issue.tracking().contains("X1"));
    }

    #[test]
    fn a_known_issue_must_predict_exactly_what_the_client_shows() {
        use KnownIssue::{FocusWhileComposing as P1, RestartLosesUntrackedComposition as Lost};
        // The user typed 가 in field 0 and moved to field 1: P1 loses it.
        let actions = [
            UserAction::Key(Key::Char('r')),
            UserAction::Key(Key::Char('k')),
            UserAction::Focus(1),
        ];
        let texts = texts(&["0", "1"]);
        let played = Played {
            texts: &texts,
            typing: Typing::default(),
            actions: &actions,
            detaches: &[],
        };
        let predicts =
            |issues: &[KnownIssue], client: &Gtk| issues_predict(issues, &played, client);
        assert!(predicts(&[P1], &gtk(["0", "1"], 1)));
        assert!(!predicts(&[], &gtk(["0", "1"], 1)), "no issue keeps it");
        assert!(
            !predicts(&[P1], &gtk(["0가", "1"], 1)),
            "it kept the syllable"
        );
        assert!(
            !predicts(&[P1], &gtk(["0", "1x"], 1)),
            "and lost something else"
        );
        // A restart loses 가 only where Exo's detach confirmed nothing.
        let actions = [
            UserAction::Key(Key::Char('r')),
            UserAction::Key(Key::Char('k')),
            UserAction::RestartInputMethod,
            UserAction::Focus(1),
        ];
        let detach = |untracked| Detach {
            before_keyboard: true,
            untracked,
        };
        let restarted = [detach(true), Detach::default()];
        let confirmed = [detach(false), Detach::default()];
        let first = [detach(true)];
        let first_confirmed = [detach(false)];
        let lost = |detaches| Played {
            detaches,
            actions: &actions,
            ..played
        };
        let client = gtk(["0", "1"], 1);
        assert!(issues_predict(&[Lost], &lost(&restarted), &client));
        assert!(!issues_predict(&[Lost], &lost(&confirmed), &client));
        assert!(
            !issues_predict(&[P1], &lost(&restarted), &client),
            "not P1's"
        );
        // So does a focus move the text input hears of first, where P1
        // cannot strike; one it confirms keeps the syllable.
        let actions = [
            UserAction::Key(Key::Char('r')),
            UserAction::Key(Key::Char('k')),
            UserAction::Focus(1),
        ];
        let moved = |detaches| Played {
            detaches,
            actions: &actions,
            ..played
        };
        assert!(issues_predict(&[Lost], &moved(&first), &client));
        assert!(!issues_predict(&[P1], &moved(&first), &client));
        let kept = gtk(["0가", "1"], 1);
        assert!(issues_predict(&[], &moved(&first_confirmed), &kept));
        let lost = Failure {
            tier: Tier::Intent,
            check: "text",
            field: Some(0),
            detail: String::new(),
        };
        let one = std::slice::from_ref(&lost);
        let none = Counts::default();
        assert_eq!(classify(one, &none, |_| true), [P1]);
        assert_eq!(classify(one, &none, |issues| issues == [Lost]), [Lost]);
        assert_eq!(classify(one, &none, |issues| issues.len() == 2), [P1, Lost]);
        assert_eq!(classify(one, &none, |_| false), []);
        assert_eq!(classify(&[], &none, |_| true), []);
        let exact = Failure {
            tier: Tier::Exact,
            ..lost.clone()
        };
        assert_eq!(classify(&[lost.clone(), exact], &none, |_| true), []);
        // No issue explains a composition that Exo composes elsewhere
        // without a region that crossed a report or was stale.
        let composition = Failure {
            tier: Tier::Exact,
            check: "composition",
            ..lost.clone()
        };
        let one = std::slice::from_ref(&composition);
        assert_eq!(classify(one, &none, |_| true), []);
        // A key recovered for a confirmation before another's press.
        let recovery = Failure {
            tier: Tier::Exact,
            check: "recovery",
            ..lost.clone()
        };
        let before = KnownIssue::RecoveryBeforeAPress;
        assert_eq!(
            classify(std::slice::from_ref(&recovery), &none, |_| false),
            [before]
        );
        assert_eq!(
            classify(&[recovery.clone(), composition], &none, |_| false),
            []
        );
        assert_eq!(classify(&[recovery, lost], &none, |_| true), []);
        assert!(!P1.reason().is_empty() && P1.tracking().contains("C5"));
        assert!(!Lost.reason().is_empty() && Lost.tracking().contains("H2"));
    }
}
