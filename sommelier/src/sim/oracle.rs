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
//! design's section 2 (`docs/IME.md`) to an ideal client per
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
//! actions through an ideal input method ([`intended`]).
//!
//! Exo's deletes (H9) and preedit regions (X1) reach the guest as
//! `SpecText` measures them in Exo's real text ([`RegionTruth`]), or not at
//! all: the proxy drops what its Exo mirror cannot measure
//! ([`SpecText::resolve`]). Exo sends no `preedit_styling`
//! (H4).

use std::collections::BTreeMap;

use super::client::Client;
use super::editor::{Reference, Target, TextModel, Txn, UserAction};
use super::exo::{self, Exo, RegionTruth};
use super::hangul::Key;
use super::keymap;
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
pub(super) struct V3Field {
    /// `(hint, purpose)`.
    pub content_type: (u32, u32),
    pub surrounding_text: Option<(String, u32, u32)>,
    pub cursor_rectangle: Option<(i32, i32, i32, i32)>,
}

impl V3Field {
    /// What Exo should hold for this field once the proxy has sent it: the
    /// proxy's own mapping of the content type; inline composition, which
    /// every v3 client supports; surrounding text, supported when the
    /// field has it; and the cursor rectangle, empty when it has none.
    pub(super) fn on_host(&self) -> exo::Field {
        let (hint, purpose) = self.content_type;
        let (input_type, input_mode, flags, learning_mode) =
            crate::ime::map_v3_content_type(hint, purpose);
        exo::Field {
            attached: true,
            content_type: exo::ContentType {
                input_type,
                input_mode,
                flags,
                learning: learning_mode == extended::LEARNING_MODE_ENABLED,
                inline_composition: true,
            },
            surrounding_text_supported: self.surrounding_text.is_some(),
            surrounding_text: self.surrounding_text.clone().unwrap_or_default(),
            cursor_rectangle: self.cursor_rectangle.unwrap_or_default(),
        }
    }
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
    let (input_type, input_mode, flags, learning_mode) =
        crate::ime::map_v3_content_type(hint, purpose);
    Some(match *request {
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

/// The preedit the guest shows, as the proxy tracks it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    text: String,
    cursor: (i32, i32),
    origin: Origin,
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
    /// Exo's deletes the guest got (H9), and those Exo's text measures to
    /// no transaction, for their range.
    pub deletes: u64,
    pub deletes_misplaced: u64,
    /// Preedit regions the guest got (X1), and those of them over a
    /// preedit the guest showed.
    pub regions: u64,
    pub regions_over_preedits: u64,
    /// Deletes and regions the proxy dropped as its Exo mirror could not
    /// measure them, by cause. The proxy counts them; the world reads them
    /// at the end of the case.
    pub drops: crate::ime::Drops,
    /// Deletes and regions the proxy dropped although the guest held Exo's
    /// text, without its composition, which it showed, cursor included:
    /// the drops a sharper mirror could avoid.
    pub drops_truth_in_step: u64,
    /// Guest reports that crossed an edit on its way to the guest (P4).
    pub crossed_reports: u64,

    /// Focus moves that the text input heard of before the keyboard, bound
    /// after its activate, so that Exo detached while attached (H2).
    pub detaches_before_keyboard: u64,
    /// Empty confirmations that recovered a held key (X2'): its press and
    /// release reached the guest, or not, as `wl_keyboard.key` had already
    /// delivered it; and those that found no eligible held key.
    pub recovered_keys: u64,
    pub recoveries_skipped: u64,
    pub confirms_unheld: u64,
    /// Empty confirmations that found an eligible held key and turned out
    /// to be Exo's before a new press (E13) or before its detach (E6),
    /// which recovered nothing.
    pub confirms_before_a_press: u64,
    pub confirms_before_a_detach: u64,
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
        self.deletes_misplaced += other.deletes_misplaced;
        self.regions += other.regions;
        self.regions_over_preedits += other.regions_over_preedits;
        self.drops += other.drops;
        self.drops_truth_in_step += other.drops_truth_in_step;
        self.crossed_reports += other.crossed_reports;
        self.detaches_before_keyboard += other.detaches_before_keyboard;
        self.recovered_keys += other.recovered_keys;
        self.recoveries_skipped += other.recoveries_skipped;
        self.confirms_unheld += other.confirms_unheld;
        self.confirms_before_a_press += other.confirms_before_a_press;
        self.confirms_before_a_detach += other.confirms_before_a_detach;
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
    /// The input method detached on its own while the key was held (H2):
    /// it is not recovered again.
    stale: bool,
}

/// X2': the recovery of held key `key` for an empty confirmation, which
/// waits for the host's next event. `press` is the key of a `peek_key`
/// press that came since, of a key not held before; `origin` is the
/// confirmation's.
#[derive(Clone, Copy, Debug)]
struct Waiting {
    key: u32,
    origin: Origin,
    press: Option<u32>,
}

/// The transaction of a delete or a region measured in Exo's text (H9,
/// X1), which the guest gets only if the proxy sends it.
#[derive(Clone, Debug)]
struct Measured {
    txn: Txn,
    cursor: (i32, i32),
    origin: Origin,
    /// The preedit the guest shows after it.
    shown: Option<Shown>,
    /// A delete or a region.
    kind: &'static str,
}

/// The step's delete or region (H9, X1), which the guest gets as Exo's
/// text measures it, or not at all ([`SpecText::resolve`]).
#[derive(Debug)]
struct Measuring {
    /// The transactions the guest got in the step before it.
    txns: usize,
    /// What Exo's text measures, if the guest can be in step with Exo;
    /// `None` for none.
    measured: Option<Measured>,
    region: bool,
    /// What the guest gets if the proxy cannot measure it: an empty
    /// region over a preedit the guest shows commits it, as Exo ends its
    /// composition as text all the same.
    fallback: Option<Txn>,
    /// The guest held Exo's text, without its composition, which it
    /// showed, cursor included.
    truth_in_step: bool,
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
    /// The last `preedit_cursor` since a terminal event (H5).
    assembly: Option<i32>,
    /// The step's delete or region ([`Measuring`]).
    measuring: Option<Measuring>,
    /// The guest got none of a region Exo composed, so it shows another
    /// preedit than Exo composes until Exo's next preedit, commit or
    /// confirmation, or a focus change.
    pub region_dropped: bool,
    /// The keys the host holds, by keycode, and the peeks so far: the
    /// sequence of the latest since the focus last moved, and their count.
    held: BTreeMap<u32, Held>,
    latest_peek: Option<u64>,
    peeks: u64,
    /// X2': an empty confirmation's recovery that waits for the host's
    /// next event ([`SpecText::decide_recovery`]).
    recovery: Option<Waiting>,

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
    /// The surrounding text the proxy must report to the host in the step,
    /// if a commit stated one, and the reports it sent. An `Option`: a step
    /// handles one request, so at most one commit.
    report: Option<(String, u32, u32)>,
    reported: Vec<(String, u32, u32)>,
    /// The report the proxy withheld in the step, with the preedit the
    /// guest shows, for the caller to hold to Exo's tracker
    /// ([`SpecText::take_withheld`]); and the preedit the guest showed when
    /// the proxy withheld the field's latest report, if it did, so that
    /// Exo holds that report with the preedit as its composition.
    withheld: Option<((String, u32, u32), String)>,
    pub report_withheld: Option<String>,
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
            assembly: None,
            measuring: None,
            region_dropped: false,
            held: BTreeMap::new(),
            latest_peek: None,
            peeks: 0,
            recovery: None,
            expected: Decisions::default(),
            sent: Decisions::default(),
            synced: (None, None),
            after: After::Other,
            txns: Vec::new(),
            input: String::new(),
            wrong_decisions: Vec::new(),
            report: None,
            reported: Vec::new(),
            withheld: None,
            report_withheld: None,
            typed: Vec::new(),
            wrong_reports: Vec::new(),
            counts: Counts::default(),
        }
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
            self.report = Some(text.clone().unwrap_or_default());
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
        self.decide_recovery(event);
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
            AnyEvent::ZwpTextInputV1(event) => self.text_input(event, origin, truth),
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::ConfirmPreedit { .. }) => {
                self.region_dropped = false;
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
                let measured = spec.measure_region(truth, index, length, origin);
                spec.measure(truth, measured, true, length);
                None
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
    /// moved and not a cancelled Backspace. With no eligible key, nothing
    /// goes out, and the lease ends. With one, the recovery waits for the
    /// host's next event ([`SpecText::decide_recovery`]): Exo confirms
    /// before every press its input method does not handle (E13), and sends
    /// the press's peek and key after the confirmation (E8). `origin` says
    /// which key's press, if any, the confirmation is Exo's confirmation
    /// before.
    fn recover_held_key(&mut self, origin: Origin) -> Option<(Txn, (i32, i32))> {
        let backspace = keymap::keycode(Key::Backspace).0;
        let latest = self.held.iter().max_by_key(|(_, held)| held.sequence);
        let eligible = latest.filter(|&(&key, held)| {
            self.latest_peek
                .is_none_or(|sequence| sequence == held.sequence)
                && !(key == backspace && held.cancelled)
                && !held.stale
        });
        let Some((&key, _)) = eligible else {
            self.counts.confirms_unheld += 1;
            self.end_lease();
            return None;
        };
        self.recovery = Some(Waiting {
            key,
            origin,
            press: None,
        });
        None
    }

    /// X2': the host's `event` decides the recovery that waits, if any,
    /// before it is handled. Before any peek, the keyboard's modifiers
    /// leave it waiting, and the peek of a press of a key not held leaves
    /// it waiting on that press. That press's `wl_keyboard.key` shows that
    /// the confirmation was Exo's before it, and the text input's `leave`
    /// that it was Exo's as it detached (E6): nothing is recovered, and the
    /// lease ends. Any other event recovers the key.
    fn decide_recovery(&mut self, event: &AnyEvent) {
        let Some(waiting) = &mut self.recovery else {
            return;
        };
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        match (event, waiting.press) {
            (AnyEvent::WlKeyboard(wl_keyboard::Event::Modifiers { .. }), None) => {}
            (&AnyEvent::ZcrExtendedKeyboardV1(peek::Event::PeekKey { key, state, .. }), None)
                if state == pressed && !self.held.contains_key(&key) =>
            {
                waiting.press = Some(key);
            }
            (&AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. }), Some(press))
                if state == pressed && key == press =>
            {
                self.recovery = None;
                self.counts.confirms_before_a_press += 1;
                self.end_lease();
            }
            (AnyEvent::ZwpTextInputV1(v1::Event::Leave {}), _) => {
                self.recovery = None;
                self.counts.confirms_before_a_detach += 1;
                self.end_lease();
            }
            _ => self.recover_waiting(),
        }
    }

    /// X2': the recovery that waits was an autorepeat's. If the text input
    /// still passes the gate and the key is held, the guest gets its press
    /// and release, unless `wl_keyboard.key` delivered it already, and then
    /// what it shows, again. Recovering Backspace arms its lease; skipping
    /// it, or recovering another key, ends it, and skipping another key
    /// leaves it. Otherwise nothing goes out, and the lease ends.
    fn recover_waiting(&mut self) {
        let Some(Waiting { key, origin, .. }) = self.recovery.take() else {
            return;
        };
        let backspace = keymap::keycode(Key::Backspace).0;
        let held = self.held.get(&key).copied();
        let (Some(held), true) = (held, self.gate()) else {
            self.end_lease();
            return;
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

    /// The commit of the shown preedit, if the guest shows one, which ends
    /// the composition (X2, F1, H2).
    fn commit_shown(&mut self) -> Option<Txn> {
        let shown = self.shown.take()?;
        self.end_lease();
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
        self.region_dropped = false;
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
    fn text_input(&mut self, event: &v1::Event, origin: Origin, truth: Option<&RegionTruth>) {
        match event {
            v1::Event::Leave {} if self.link == Link::Attached => {
                // H2: Exo detached on its own. The keys in flight died with
                // the input method, so no key held now is recovered again.
                // Its detach confirmed before its `leave` (E6), so a preedit
                // the guest still shows is one that confirmation did not
                // commit, which the guest commits.
                self.counts.host_detaches += 1;
                for held in self.held.values_mut() {
                    held.stale = true;
                }
                if let Some(txn) = self.commit_shown() {
                    self.apply(&txn, (0, 0));
                }
                self.leave_attached(Link::Detached);
            }
            &v1::Event::PreeditCursor { index } if self.link == Link::Attached => {
                self.assembly = Some(index);
            }
            v1::Event::PreeditString { text, .. } => {
                self.region_dropped = false;
                let IndexedString::Utf8(text) = text else {
                    return self.composition_event(origin, |_| None);
                };
                self.composition_event(origin, |spec| {
                    let shown = spec.shown.take();
                    match text.is_empty() && shown.is_some() {
                        true => spec.arm_lease(),
                        false => spec.end_lease(),
                    }
                    if text.is_empty() {
                        return Some((Txn::default(), (0, 0)));
                    }
                    let cursor = preedit_cursor(text, spec.assembly);
                    spec.shown = Some(Shown {
                        text: text.clone(),
                        cursor,
                        origin,
                    });
                    let txn = Txn {
                        preedit: text.clone(),
                        ..Txn::default()
                    };
                    Some((txn, cursor))
                });
            }
            v1::Event::CommitString { text, .. } => {
                self.region_dropped = false;
                self.composition_event(origin, |spec| {
                    spec.shown = None;
                    spec.end_lease();
                    let txn = Txn {
                        commit: text.clone(),
                        ..Txn::default()
                    };
                    Some((txn, (0, 0)))
                });
            }
            &v1::Event::DeleteSurroundingText { index, length } => {
                self.composition_event(origin, |spec| {
                    let measured = spec.measure_delete(truth, index, length, origin);
                    spec.measure(truth, measured, false, length);
                    None
                });
            }
            _ => {}
        }
    }

    /// H9 and X1: the step's delete or region, which Exo's text before it
    /// (`truth`) measures to `measured`, the transaction the guest gets if
    /// the proxy sends it, which its Exo mirror measures.
    /// Exo edited its text, unless the event is an empty delete, so the
    /// Backspace lease ends whatever reaches the guest.
    fn measure(
        &mut self,
        truth: Option<&RegionTruth>,
        measured: Option<Measured>,
        region: bool,
        length: u32,
    ) {
        if region || length > 0 {
            self.end_lease();
        }
        let fallback = self.shown.as_ref().filter(|_| region && length == 0);
        let fallback = fallback.map(|shown| Txn {
            commit: shown.text.clone(),
            ..Txn::default()
        });
        self.measuring = Some(Measuring {
            txns: self.txns.len(),
            measured,
            region,
            fallback,
            truth_in_step: truth.is_some_and(|truth| self.truth_in_step(truth)),
        });
    }

    /// Whether the guest, as an ideal client shows the focused field, holds
    /// Exo's text before an event (`truth`) without its composition, which
    /// it shows as its preedit, with its cursor where Exo's is: in step, as
    /// a perfect mirror would find it.
    fn truth_in_step(&self, truth: &RegionTruth) -> bool {
        let (Some(field), Some(held)) = (self.focus, Self::held(truth)) else {
            return false;
        };
        let model = &self.views[field].model;
        let text = [&truth.text[..held.start], &truth.text[held.end..]].concat();
        model.text == text && model.cursor == held.start && model.preedit == truth.text[held]
    }

    /// The guest got the transactions `got` in the step of a delete or a
    /// region (`measuring`), after the step's earlier ones: either the one
    /// Exo's text measures, which the specification then applies, or a
    /// drop: none, or the commit of the shown preedit for an empty region,
    /// which the specification then applies. Anything else fails the step.
    fn resolve(&mut self, measuring: Measuring, got: &[(Txn, (i32, i32))]) {
        let Some(after) = got.get(measuring.txns..) else {
            return;
        };
        match (measuring.measured, measuring.fallback) {
            (Some(measured), _) if after == [(measured.txn.clone(), measured.cursor)] => {
                self.region_dropped &= !measuring.region;
                self.accept(measured);
            }
            // Required: a guest that gets anything else fails the step.
            (measured, Some(fallback)) => {
                let dropped = measured.is_some() && measuring.truth_in_step;
                self.counts.drops_truth_in_step += u64::from(dropped);
                self.shown = None;
                self.apply(&fallback, (0, 0));
            }
            (measured, None) if after.is_empty() => {
                let dropped = measured.is_some() && measuring.truth_in_step;
                self.counts.drops_truth_in_step += u64::from(dropped);
                self.region_dropped |= measuring.region;
            }
            _ => {}
        }
    }

    /// Where a guest in step with Exo's text before the event, `truth`, has
    /// its preedit: the composition, which it shows, with the rest of Exo's
    /// text and its cursor at the composition's start; or else, with no
    /// selection, empty at Exo's cursor, the guest's. `None` with a
    /// selection, in which no guest is in step.
    ///
    /// Whether the guest is in step is the proxy's to tell, and an ideal
    /// client's text is no guide: a client model's can differ, as GTK's
    /// Backspace leaves a syllable's first jamo (C14). The specification
    /// holds the proxy only to a measurement in Exo's text, if it makes
    /// one, or else to a drop.
    fn held(truth: &RegionTruth) -> Option<std::ops::Range<usize>> {
        match &truth.composition {
            Some(composition) => Some(composition.clone()),
            None => {
                let selection = truth.selection;
                (selection.start == selection.end).then_some(selection.end..selection.end)
            }
        }
    }

    /// X1: the bytes from `index` past Exo's cursor, `length` long, in
    /// Exo's text (`truth`), become the preedit, if they cover the preedit
    /// Exo holds; an empty region leaves that preedit as text, which the
    /// guest commits. `None` if no guest can be in step, or Exo's text
    /// measures no transaction.
    fn measure_region(
        &self,
        truth: Option<&RegionTruth>,
        index: i32,
        length: u32,
        origin: Origin,
    ) -> Option<Measured> {
        let (truth, held) = truth.and_then(|truth| Some((truth, Self::held(truth)?)))?;
        let measured = (|| {
            let start = usize::try_from(truth.cursor as i64 + i64::from(index)).ok()?;
            let end = start + length as usize;
            let text = truth.text.get(start..end)?;
            if text.is_empty() {
                let commit = truth.text[held.clone()].to_owned();
                let txn = Txn {
                    commit,
                    ..Txn::default()
                };
                return Some((txn, (0, 0), None, "region"));
            }
            if held.start < start || end < held.end {
                return None;
            }
            let before = truth.cursor.clamp(start, end) - start;
            let cursor = preedit_cursor(text, Some(self.assembly.unwrap_or(before as i32)));
            let shown = Shown {
                text: text.to_owned(),
                cursor,
                origin,
            };
            let txn = Txn {
                delete: (held.start - start, end - held.end),
                preedit: text.to_owned(),
                ..Txn::default()
            };
            Some((txn, cursor, Some(shown), "region"))
        })();
        let (txn, cursor, shown, kind) = measured?;
        Some(Measured {
            txn,
            cursor,
            origin,
            shown,
            kind,
        })
    }

    /// H9: Exo deletes `length` bytes from `index` of its text (`truth`),
    /// which holds no composition, around its cursor, the guest's. `None`
    /// if no guest can be in step, or Exo's text measures no transaction.
    fn measure_delete(
        &mut self,
        truth: Option<&RegionTruth>,
        index: i32,
        length: u32,
        origin: Origin,
    ) -> Option<Measured> {
        let (truth, held) = truth.and_then(|truth| Some((truth, Self::held(truth)?)))?;
        let start = usize::try_from(index).ok();
        let lengths = start.and_then(|start| around(&truth.text, held.start, start as i64, length));
        let measured = lengths.filter(|&lengths| held.is_empty() && lengths != (0, 0));
        self.counts.deletes_misplaced += u64::from(measured.is_none());
        let (before, after) = measured?;
        Some(Measured {
            txn: Txn {
                delete: (before, after),
                ..Txn::default()
            },
            cursor: (0, 0),
            origin,
            shown: None,
            kind: "delete",
        })
    }

    /// The guest got `measured`, the transaction of the step's delete or
    /// region.
    fn accept(&mut self, measured: Measured) {
        let Measured {
            txn,
            cursor,
            origin,
            shown,
            kind,
        } = measured;
        self.end_lease();
        let over = std::mem::replace(&mut self.shown, shown).is_some();
        match kind {
            "delete" => self.counts.deletes += 1,
            _ if !txn.preedit.is_empty() => {
                self.counts.regions += 1;
                self.counts.regions_over_preedits += u64::from(over);
            }
            _ => {}
        }
        self.counts.late_shown += u64::from(origin.late);
        self.apply(&txn, cursor);
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
    ///
    /// With an Exo mirror the proxy may withhold a report while the guest
    /// shows a preedit, if Exo's tracker holds the
    /// report with that preedit as its composition, which the caller
    /// checks.
    fn check_reports(&mut self) {
        let reported = std::mem::take(&mut self.reported);
        let expected: Vec<_> = self.report.take().into_iter().collect();
        let mut wrong = Vec::new();
        let shown = self.shown.as_ref().map(|shown| shown.text.clone());
        let withheld = match (&expected[..], shown) {
            ([report], Some(shown)) if reported.is_empty() => Some((report.clone(), shown)),
            _ => None,
        };
        if !expected.is_empty() {
            self.report_withheld = withheld.as_ref().map(|(_, shown)| shown.clone());
        }
        if withheld.is_some() {
            self.withheld = withheld;
        } else if reported != expected {
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

    /// The report the proxy withheld in the step just ended, with the
    /// preedit the guest shows, which Exo's tracker must hold as a state
    /// with that composition at the report's cursor; the caller says
    /// whether it does.
    pub fn take_withheld(&mut self, held: impl FnOnce(&(String, u32, u32), &str) -> bool) {
        let Some((report, shown)) = self.withheld.take() else {
            return;
        };
        if !held(&report, &shown) && self.wrong_reports.len() < 5 {
            self.wrong_reports.push(format!(
                "after {}: the proxy withheld the surrounding text {report:?}, which Exo's \
                 tracker does not hold with the composition {shown:?}",
                self.input
            ));
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
    // The guest gets the step's delete or region as Exo's text measures
    // it, or not at all.
    if let Some(measuring) = spec.measuring.take() {
        spec.resolve(measuring, &shadow.txns);
    }
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
/// - the proxy's decisions against the specification's;
/// - the guest's preedit against what Exo composes;
/// - Exo's field on the focused surface against what the guest last
///   committed.
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
/// And Exo may compose a region the proxy dropped, which the guest never
/// shows, until Exo next composes, commits or confirms
/// ([`SpecText::region_dropped`]).
pub(super) fn exact(
    steps: &[String],
    spec: &SpecText,
    exo: &Exo,
    engine: &str,
    resets: u64,
) -> Vec<Failure> {
    let mut failures = Vec::new();
    let failure = |check, field, detail| Failure {
        tier: Tier::Exact,
        check,
        field,
        detail,
    };
    // `step` compared the fields after every step.
    if !steps.is_empty() {
        failures.push(failure("step", None, steps.join("\n")));
    }
    if !spec.wrong_decisions.is_empty() {
        failures.push(failure("decision", None, spec.wrong_decisions.join("\n")));
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
    let composing = exo.composition().filter(|_| !spec.region_dropped);
    if let (Some(field), Some(tracked)) = (spec.focus, composing) {
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
    let mut expected: Vec<exo::Field> = spec.committed.iter().map(V3Field::on_host).collect();
    let held = exo.focused_fields();
    // Exo still has the report before a withheld one, and its tracker holds
    // the withheld one with the preedit the guest showed as its composition
    // ([`SpecText::take_withheld`]).
    if let Some(shown) = &spec.report_withheld {
        for (expected, held) in expected.iter_mut().zip(&held) {
            if exo.holds_with_composition(&expected.surrounding_text, shown) {
                expected.surrounding_text = held.surrounding_text.clone();
            }
        }
    }
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

/// The no-loss check, tier 1, of a settled case: at each `Settle` of
/// `actions` and at the end, each of the client's fields shows every
/// character the user intends there, in order, preedit included at the
/// cursor. A field may show more, as a doubled commit does, which tier 2
/// judges; never less. `reference` is the intent before the actions,
/// `settles` the client's fields at each `Settle`, and `client` the
/// client at the end. Only a settled case: the reference does not describe
/// a free case's timing, where a key can reach a field raw, or another
/// field than the user meant.
pub(super) fn lost(
    mut reference: Reference,
    actions: &[UserAction],
    settles: &[Vec<TextModel>],
    client: &dyn Client,
) -> Vec<Failure> {
    let shown = |model: &TextModel| {
        let (before, after) = model.text.split_at(model.cursor);
        format!("{before}{}{after}", model.preedit)
    };
    let check = |at: &str, reference: &Reference, fields: &[TextModel]| -> Vec<Failure> {
        let pairs = reference.fields().iter().zip(fields).enumerate();
        pairs
            .filter(|(_, (intended, got))| !is_subsequence(&shown(intended), &shown(got)))
            .map(|(i, (intended, got))| Failure {
                tier: Tier::Exact,
                check: "no loss",
                field: Some(i),
                detail: format!(
                    "{at}: intended: {}\nclient:   {}",
                    show_model(intended),
                    show_model(got)
                ),
            })
            .collect()
    };
    let mut settles = settles.iter();
    for (i, &action) in actions.iter().enumerate() {
        if action == UserAction::Settle {
            let fields = settles.next().expect("the client's fields at each settle");
            let failures = check(&format!("at action {i}, a settle"), &reference, fields);
            if !failures.is_empty() {
                return failures;
            }
        }
        reference.apply(action);
    }
    let fields: Vec<TextModel> = client.fields().iter().map(|f| f.model.clone()).collect();
    check("at the end", &reference, &fields)
}

/// Whether `text` holds every character of `part`, in order.
fn is_subsequence(part: &str, text: &str) -> bool {
    let mut text = text.chars();
    part.chars().all(|c| text.any(|t| t == c))
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
        let typed = extended::Request::SetInputType {
            input_type: extended::INPUT_TYPE_PASSWORD,
            input_mode: extended::INPUT_MODE_DEFAULT,
            input_flags: extended::INPUT_FLAGS_HAS_BEEN_PASSWORD,
            learning_mode: extended::LEARNING_MODE_DISABLED,
            inline_composition_support: extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        };
        guest(&mut spec, &[commit()], &[typed.into()]);
        assert_eq!(spec.wrong_reports.len(), 4, "{:?}", spec.wrong_reports);
    }

    /// Exo's delete or region reaches the guest as Exo's text before it
    /// measures it, or not at all, a drop. A dropped region excuses the
    /// guest's preedit until Exo next composes.
    #[test]
    fn exos_delete_or_region_reaches_the_guest_as_exos_text_measures_it_or_not_at_all() {
        let delete = || -> AnyEvent {
            v1::Event::DeleteSurroundingText {
                index: 1,
                length: 2,
            }
            .into()
        };
        let region = || -> AnyEvent {
            extended::Event::SetPreeditRegion {
                index: -1,
                length: 1,
            }
            .into()
        };
        // Exo's "0ab|", which the guest has.
        let truth = RegionTruth {
            text: "0ab".to_owned(),
            cursor: 3,
            selection: exo::Selection { start: 3, end: 3 },
            ..RegionTruth::default()
        };
        let txn = |delete, preedit: &str| Txn {
            delete,
            preedit: preedit.to_owned(),
            ..Txn::default()
        };
        /// Exo's event, what the guest got, and the text it then has,
        /// `None` if the step fails.
        type Case = (AnyEvent, Vec<(Txn, (i32, i32))>, Option<&'static str>);
        let cases: [Case; 4] = [
            (delete(), vec![(txn((2, 0), ""), (0, 0))], Some("0")),
            (delete(), vec![], Some("0ab")),
            (delete(), vec![(txn((1, 0), ""), (0, 0))], None),
            (region(), vec![(txn((1, 0), "b"), (1, 1))], Some("0a")),
        ];
        for (event, got, text) in cases {
            let mut spec = attached(&["0ab"]);
            report(&mut spec, "0ab", 3);
            // The steps so far, which no shadow checked.
            spec.txns.clear();
            spec.see(Seen::Host {
                sender: SYNC,
                event: &event,
                origin: Origin::default(),
                region: Some(&truth),
            });
            spec.see(Seen::StepEnd);
            let mut shadow = Shadow::new(both(1), &texts(&["0ab"]));
            shadow.txns = got.clone();
            let failed = step(&mut spec, &mut shadow).is_some_and(|f| f.contains("transactions"));
            let case = format!("{event:?} {got:?}");
            assert_eq!(failed, text.is_none(), "{case}");
            if let Some(text) = text.filter(|_| !failed) {
                assert_eq!(spec.views[0].model.text, text, "{case}");
                let dropped_region =
                    got.is_empty() && matches!(event, AnyEvent::ZcrExtendedTextInputV1(_));
                assert_eq!(spec.region_dropped, dropped_region, "{case}");
                let in_step = u64::from(got.is_empty());
                assert_eq!(spec.counts.drops_truth_in_step, in_step, "{case}");
            }
        }
    }

    /// An empty region over a preedit the guest shows that the proxy does
    /// not measure must commit that preedit: Exo ends its composition as
    /// text all the same.
    #[test]
    fn an_unmeasured_empty_region_must_commit_the_shown_preedit() {
        let region = || -> AnyEvent {
            extended::Event::SetPreeditRegion {
                index: 0,
                length: 0,
            }
            .into()
        };
        let commit = Txn {
            commit: "가".to_owned(),
            ..Txn::default()
        };
        for (got, fails) in [(vec![(commit, (0, 0))], false), (vec![], true)] {
            let mut spec = attached(&["0ab"]);
            report(&mut spec, "0ab", 3);
            preedit(&mut spec, "가");
            // The steps so far, which no shadow checked.
            spec.txns.clear();
            spec.see(Seen::Host {
                sender: SYNC,
                event: &region(),
                origin: Origin::default(),
                region: None,
            });
            spec.see(Seen::StepEnd);
            let mut shadow = Shadow::new(both(1), &texts(&["0ab"]));
            shadow.txns = got;
            let failed = step(&mut spec, &mut shadow).is_some_and(|f| f.contains("transactions"));
            assert_eq!(failed, fails);
            assert_eq!(spec.views[0], view("0ab가", 6, ""));
        }
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

    /// The `done` of the proxy's sync, after Exo's confirmation, which
    /// decides a recovery that waits as an autorepeat's.
    fn sync_done() -> wl_callback::Event {
        wl_callback::Event::Done { callback_data: 0 }
    }

    /// An autorepeat's empty confirmation: the confirmation, then the
    /// `done` of the proxy's sync.
    fn repeat(spec: &mut SpecText) {
        host(spec, confirm(), &[]);
        host(spec, sync_done(), &[]);
    }

    #[test]
    fn an_empty_confirmation_recovers_the_latest_held_key_unless_delivered() {
        let mut spec = attached(&["0"]);
        spec.txns.clear();
        let bare = || vec![(Txn::default(), (0, 0))];
        // Nothing held: nothing goes out.
        repeat(&mut spec);
        assert_eq!(spec.txns, []);
        // A held Space the input method consumed: at the host's next event,
        // its press, then a done.
        host(&mut spec, peek_key(Key::Char(' '), DOWN), &[]);
        host(&mut spec, confirm(), &[]);
        assert_eq!(spec.txns, []);
        host(&mut spec, sync_done(), &[]);
        assert_eq!(
            (&spec.views[0], std::mem::take(&mut spec.txns)),
            (&view("0 ", 2, ""), bare())
        );
        // One that wl_keyboard delivered: only the done.
        host(&mut spec, peek_key(Key::Char(' '), UP), &[]);
        host(&mut spec, peek_key(Key::Char('a'), DOWN), &[]);
        let key = |key, state| wl_keyboard::Event::Key {
            serial: 1,
            time: 0,
            key: keymap::keycode(key).0,
            state,
        };
        host(&mut spec, key(Key::Char('a'), DOWN), &[]);
        repeat(&mut spec);
        assert_eq!(
            (&spec.views[0], std::mem::take(&mut spec.txns)),
            (&view("0 a", 3, ""), bare())
        );
        // A key held but not the latest peek: nothing.
        host(&mut spec, peek_key(Key::Char('s'), DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('s'), UP), &[]);
        repeat(&mut spec);
        assert_eq!(spec.txns, []);
        let c = spec.counts;
        let counts = (c.recovered_keys, c.recoveries_skipped, c.confirms_unheld);
        assert_eq!(counts, (1, 1, 2));
        // Exo's confirmation before the press of a key its input method
        // did not handle (E13), whose peek and key follow it (E8): the held
        // Space does not repeat.
        host(&mut spec, peek_key(Key::Char(' '), DOWN), &[]);
        let before_b = Origin {
            press: Some(Key::Char('b')),
            ..Origin::default()
        };
        let event = confirm().into();
        spec.see(Seen::Host {
            sender: SYNC,
            event: &event,
            origin: before_b,
            region: None,
        });
        spec.see(Seen::StepEnd);
        host(&mut spec, peek_key(Key::Char('b'), DOWN), &[]);
        host(&mut spec, key(Key::Char('b'), DOWN), &[]);
        host(&mut spec, sync_done(), &[]);
        assert_eq!(spec.views[0], view("0 ab", 4, ""));
        assert_eq!(spec.txns, []);
        let c = spec.counts;
        assert_eq!(
            (c.confirms_before_a_press, c.recoveries_before_a_press),
            (1, 0)
        );
        // A peek of a key the input method handles, then anything else:
        // the confirmation was the held Space's autorepeat.
        host(&mut spec, peek_key(Key::Char(' '), UP), &[]);
        host(&mut spec, peek_key(Key::Char(' '), DOWN), &[]);
        host(&mut spec, confirm(), &[]);
        host(&mut spec, peek_key(Key::Char('c'), DOWN), &[]);
        assert_eq!(spec.txns, []);
        host(&mut spec, sync_done(), &[]);
        assert_eq!(
            (&spec.views[0], std::mem::take(&mut spec.txns)),
            (&view("0 ab ", 5, ""), bare())
        );
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
        repeat(&mut spec);
        assert_eq!(spec.views[0], view("0a", 2, ""));
        assert_eq!((spec.counts.leases, spec.counts.leases_ended), (1, 0));
        // An edit ends it for the rest of the press.
        host(&mut spec, commit("x"), &[]);
        repeat(&mut spec);
        assert_eq!(spec.views[0], view("0ax", 3, ""));
        assert_eq!(spec.counts.leases_ended, 1);
        // So does another key's press, lease or not.
        host(&mut spec, backspace(UP), &[]);
        host(&mut spec, backspace(DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('a'), DOWN), &[]);
        host(&mut spec, peek_key(Key::Char('a'), UP), &[]);
        repeat(&mut spec);
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
            let failures = exact(&[], spec, &exo, engine, resets);
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
            let failures = exact(&[], &spec, exo, engine, 0);
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
    fn no_loss_needs_each_intended_character_in_order_at_each_settle() {
        let start = || Reference::new(vec![TextModel::new("ab"), TextModel::new("1")]);
        let fields = |texts: [&str; 2]| texts.map(TextModel::new).to_vec();
        let lost_at = |settle: [&str; 2], end: [&str; 2]| -> Vec<(Option<usize>, String)> {
            let settles = [fields(settle)];
            let failures = lost(start(), &[UserAction::Settle], &settles, &gtk(end, 0));
            let at = |f: Failure| (f.field, f.detail.split(':').next().unwrap().to_owned());
            failures.into_iter().map(at).collect()
        };
        // More is no loss; less, or out of order, is.
        assert_eq!(lost_at(["xaby", "1"], ["ab", "21"]), []);
        assert_eq!(
            lost_at(["a", "1"], ["ab", "1"]),
            [(Some(0), "at action 0, a settle".to_owned())]
        );
        assert_eq!(
            lost_at(["ab", "1"], ["ba", "1"]),
            [(Some(0), "at the end".to_owned())]
        );
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
}
