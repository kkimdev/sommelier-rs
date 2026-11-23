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

//! The two oracles that judge a case.
//!
//! **Tier 1, every case.** [`Judge`] holds what the guest got, as
//! [`Shadow`], an ideal client fed it, shows it, to what the simulation
//! did, which [`Origin`] labels each of Exo's events with. It does not
//! model the proxy's decisions; the IME and keyboard cores' row tests pin
//! those. After every step it checks:
//!
//! - **Exo's edits.** A preedit, commit or confirmation that Exo sent in
//!   the state the proxy last asked it for reaches the guest, as the
//!   design's section 2 translates it, and one that crossed a request
//!   changing that state (an `activate`, `deactivate` or `reset`) does
//!   not: its text is for a composition the guest already ended
//!   ([`Judge::current`]). Exo's deletes (H9) and preedit regions (X1)
//!   reach the guest as Exo's text before them measures them
//!   ([`RegionTruth`]), or not at all.
//! - **The composition.** Nothing else changes what the guest shows,
//!   except that the guest commits the preedit it shows where the
//!   composition ends: at a press, a focus change or Exo's own detach. No
//!   preedit is dropped by a press or a `leave`, and the proxy resets Exo
//!   exactly when a press ends the composition the guest shows, so that
//!   both sides end it.
//! - **Keys.** Each press the guest gets is Exo's `wl_keyboard.key`
//!   press of that key, or a held key's autorepeat that Exo's input method
//!   did not handle (E13), at most once each; every such press and
//!   autorepeat reaches the guest, unless the guest holds the key, which
//!   repeats it itself; and the guest never gets a second press of a key it
//!   holds.
//! - **Reports.** The proxy reports what the guest committed, or withholds
//!   a report that Exo's tracker holds with the guest's preedit.
//!
//! At quiescence ([`exact`]), the guest's preedit must be what Exo
//! composes, and Exo must hold the field the guest last committed
//! ([`V3Field`]). In a settled case, no field may lose a character the
//! user means ([`lost`]).
//!
//! **Tier 2, user intent, settled cases only.** The client model's fields
//! and focus must equal those of [`Reference`], which applied the user's
//! actions through an ideal input method ([`intended`]).

use std::collections::BTreeSet;

use super::client::Client;
use super::clock::Micros;
use super::editor::{Reference, Target, TextModel, Txn, UserAction};
use super::exo::{self, Exo, RegionTruth};
use super::hangul::Key;
use super::keymap;
use super::surfaces::Surfaces;
use crate::protocols::keyboard_extension_unstable_v1::zcr_extended_keyboard_v1 as peek;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as extended;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1 as v1;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3 as v3;
use crate::protocols::wayland::{wl_keyboard, wl_pointer, wl_touch};
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
        /// What Exo did of it, if it is a delete or a `set_preedit_region`.
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

/// What the simulation knows of a host event: what made Exo send it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Origin {
    /// An answer the engine made before a reset that reached it after the
    /// key (E5's outcome (c), P3).
    pub late: bool,
    /// How many resets Exo had handed the engine when it sent the event.
    pub resets: u64,
    /// The key whose press, not an autorepeat, the event answers.
    pub press: Option<Key>,
    /// The held key whose autorepeat the event answers (D7).
    pub repeat: Option<Key>,
    /// The event answers a surrounding text (E17).
    pub report: bool,
    /// How many of the proxy's requests that change what Exo's text input
    /// serves ([`changes_service`]) Exo had handled when it sent the event.
    pub requests: u64,
    /// Exo's keyboard held a key whose release died with the input method,
    /// so that a press of it sends only the input method's confirmation
    /// (E13), which nothing tells from an autorepeat's.
    pub stale_held: bool,
}

/// Whether `request` to the host changes what Exo's text input serves: an
/// `activate`, a `deactivate` or a `reset`. Exo's events that cross one
/// are for a state the proxy has left.
pub(super) fn changes_service(request: &AnyRequest) -> bool {
    matches!(
        request,
        AnyRequest::ZwpTextInputV1(
            v1::Request::Activate { .. } | v1::Request::Deactivate { .. } | v1::Request::Reset {}
        )
    )
}

/// Which oracle a check belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tier {
    /// Tier 1: what the simulation did, exactly.
    Exact,
    /// Tier 2: the user's intent.
    Intent,
}

/// What a check holds a case to. The shrinker keeps a candidate only if it
/// fails the same check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Check {
    /// The proxy keeps the session.
    Session,
    /// The proxy's Exo mirror agrees with the Exo model.
    Mirror,
    /// The case does not panic.
    Panic,
    /// A case run twice leaves the same trace.
    Determinism,
    /// Tier 1 after every step: what the guest shows against Exo's edits.
    Step,
    /// Tier 1: the guest's key presses against Exo's keys and autorepeats.
    Keys,
    /// Tier 1: the reports the proxy sends the host.
    Report,
    /// Tier 1: the guest's preedit against Exo's composition.
    Composition,
    /// Tier 1: Exo's fields against what the guest committed.
    FieldState,
    /// Tier 1, settled cases: no field loses a character the user means.
    NoLoss,
    /// Tier 1, settled cases: the proxy measures every delete and region
    /// of Exo's.
    Drops,
    /// Tier 1, settled cases: no edit of Exo's meets a press's fence.
    Fence,
    /// Tier 2: a field's text, cursor and preedit.
    Text,
    /// Tier 2: the focused field.
    Focus,
}

/// A failed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub tier: Tier,
    pub check: Check,
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

/// A preedit a guest shows, and its cursor; empty for none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Preedit {
    pub text: String,
    pub cursor: (i32, i32),
}

impl Preedit {
    /// The transaction that commits it.
    fn commit(&self) -> (Txn, (i32, i32)) {
        let txn = Txn {
            commit: self.text.clone(),
            ..Txn::default()
        };
        (txn, (0, 0))
    }

    /// Whether `txn`, with its preedit at `cursor`, leaves a guest that
    /// shows this preedit as it is: a `done` that restates it.
    fn kept_by(&self, txn: &Txn, cursor: (i32, i32)) -> bool {
        let same_cursor = txn.preedit.is_empty() || cursor == self.cursor;
        txn.delete == (0, 0) && txn.commit.is_empty() && txn.preedit == self.text && same_cursor
    }
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

    /// The preedit it shows.
    fn preedit(&self) -> Preedit {
        Preedit {
            text: self.model.preedit.clone(),
            cursor: self.preedit_cursor,
        }
    }
}

/// What a case counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// Presses that ended a composition the guest showed (F4).
    pub press_commits: u64,
    /// Exo's edits that crossed the proxy's `reset` and so reached no
    /// guest (the fence).
    pub fence_drops: u64,
    /// Exo's edits of late engine answers that the guest got (P3).
    pub late_shown: u64,
    /// Engine answers made before a reset that reached the engine after
    /// their key (E5's outcome (c)).
    pub late: u64,
    /// Engine answers whose text a reset dropped (E5's outcome (b)).
    pub dropped: u64,
    /// Exo's own detaches (H2): a `leave` that crossed no request of the
    /// proxy's.
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
    /// Exo's regions that reached the proxy after it had sent a report Exo
    /// made them before.
    pub regions_crossing_reports: u64,
    /// Guest reports the proxy withheld while the guest showed a preedit
    /// (G9c).
    pub reports_withheld: u64,
    /// Reconversions Exo refused, as their range lay past its text.
    pub regions_refused: u64,
    /// Keys Exo sent to a surface the guest destroyed, which reach no
    /// window.
    pub keys_to_no_window: u64,
    /// The states the Exo model reached; the world reads them at the end
    /// of the case.
    pub exo: exo::Reached,
    /// Focus moves that the text input heard of before the keyboard, bound
    /// after its activate, so that Exo detached while attached (H2).
    pub detaches_before_keyboard: u64,
    /// Autorepeats of a held key that Exo handed the engine (D7).
    pub repeats: u64,
    /// Autorepeats the input method passed, whose empty confirmation
    /// (E13) reached the proxy: those the guest got as the held key's
    /// press (X2'), and those of a key the guest held itself.
    pub recovered_keys: u64,
    pub recoveries_skipped: u64,
    /// Exo's empty confirmations, while a key was held, before the press
    /// of another key the input method did not handle (E13), and as it
    /// detached (E6): neither is an autorepeat.
    pub confirms_before_a_press: u64,
    pub confirms_before_a_detach: u64,
    /// Autorepeats the input method passed of a held key that ended no
    /// composition the guest showed, which the guest does not get.
    pub repeats_of_no_ended_composition: u64,
    /// An English engine's empty confirmations of a surrounding text
    /// (E17), none of them an autorepeat.
    pub confirms_of_reports: u64,
    /// Field surfaces the guest destroyed, and those the guest's keyboard
    /// focused (F2).
    pub surface_destroys: u64,
    pub focused_surface_destroys: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, other: Self) {
        self.press_commits += other.press_commits;
        self.fence_drops += other.fence_drops;
        self.late_shown += other.late_shown;
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
        self.regions_crossing_reports += other.regions_crossing_reports;
        self.reports_withheld += other.reports_withheld;
        self.regions_refused += other.regions_refused;
        self.keys_to_no_window += other.keys_to_no_window;
        self.exo += other.exo;
        self.detaches_before_keyboard += other.detaches_before_keyboard;
        self.repeats += other.repeats;
        self.recovered_keys += other.recovered_keys;
        self.recoveries_skipped += other.recoveries_skipped;
        self.confirms_before_a_press += other.confirms_before_a_press;
        self.confirms_before_a_detach += other.confirms_before_a_detach;
        self.repeats_of_no_ended_composition += other.repeats_of_no_ended_composition;
        self.confirms_of_reports += other.confirms_of_reports;
        self.surface_destroys += other.surface_destroys;
        self.focused_surface_destroys += other.focused_surface_destroys;
    }
}

/// What the guest got in a step, in order, as [`Judge`] checks it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Got {
    /// A transaction, at its `done`, with its preedit at `cursor`; the
    /// field showed `before`.
    Txn {
        txn: Txn,
        cursor: (i32, i32),
        before: Preedit,
    },
    /// A press of `key`, which the guest held already if `held`.
    Key { key: u32, held: bool },
    /// A button press or a touch-down in `field`, which composed `shows`
    /// ([`Shadow::composing`]).
    Pressed { field: usize, shows: Preedit },
    /// The text input's `leave` of `field`, which composed `before`.
    Leave { field: usize, before: Preedit },
}

/// An ideal client fed what the proxy sent the guest: it applies each
/// transaction at `done` to the entered field, drops the preedit at
/// `leave`, types keys and takes presses as [`TextModel`] does, and keeps
/// what it committed of its field. It logs what it got in each step
/// ([`Got`]) for the judge.
pub(super) struct Shadow {
    /// Which field each guest surface is.
    surfaces: Surfaces,
    views: Vec<View>,
    /// The field the text input entered, and its surface.
    entered: Option<(usize, u32)>,
    /// The field the keyboard focuses.
    keyboard: Option<usize>,
    shift: bool,
    /// The keys the guest holds.
    held: BTreeSet<u32>,
    /// The transaction in progress, with its preedit cursor.
    txn: (Txn, (i32, i32)),
    /// What the guest committed of the entered field, if it is enabled,
    /// and what it sent since.
    committed: Option<V3Field>,
    pending: Pending,
    /// The surrounding text the step's commit stated, and whether the
    /// commit enabled or disabled the field.
    stated: Option<(String, u32, u32)>,
    toggled: bool,
    /// What the guest got in the step.
    got: Vec<Got>,
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
            held: BTreeSet::new(),
            txn: Default::default(),
            committed: None,
            pending: Pending::default(),
            stated: None,
            toggled: false,
            got: Vec::new(),
        }
    }

    /// The preedit the entered field shows; none if no field is entered.
    fn shown(&self) -> Preedit {
        self.entered
            .map(|(field, _)| self.views[field].preedit())
            .unwrap_or_default()
    }

    /// The preedit of `field` that is the compositor's to end: the one it
    /// shows if it is the entered field and enabled. A disabled field's
    /// preedit is the client's own.
    fn composing(&self, field: usize) -> Preedit {
        let entered = self.entered.is_some_and(|(entered, _)| entered == field);
        match entered && self.committed.is_some() {
            true => self.views[field].preedit(),
            false => Preedit::default(),
        }
    }

    /// Follows one message at the proxy: what the guest sent and got.
    pub fn see(&mut self, seen: Seen<'_>) {
        match seen {
            Seen::Guest(request) => self.request(request),
            Seen::ToGuest(AnyEvent::ZwpTextInputV3(event)) => self.text_input(event),
            Seen::ToGuest(AnyEvent::WlKeyboard(event)) => match *event {
                wl_keyboard::Event::Enter { surface, .. } => {
                    self.keyboard = Some(self.field(surface));
                    self.held.clear();
                }
                wl_keyboard::Event::Leave { .. } => {
                    self.keyboard = None;
                    self.held.clear();
                }
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    self.shift = mods_depressed & keymap::SHIFT != 0;
                }
                wl_keyboard::Event::Key { key, state, .. }
                    if state == wl_keyboard::KEY_STATE_PRESSED =>
                {
                    let held = !self.held.insert(key);
                    self.got.push(Got::Key { key, held });
                    let typed = keymap::key(key, self.shift);
                    if let (Some(field), Some(key)) = (self.keyboard, typed) {
                        self.views[field].model.insert_key(key);
                    }
                }
                wl_keyboard::Event::Key { key, .. } => {
                    self.held.remove(&key);
                }
                _ => {}
            },
            Seen::GuestPress { surface, at } => {
                let field = self.field(surface);
                let shows = self.composing(field);
                self.got.push(Got::Pressed { field, shows });
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

    /// G3 to G9: the guest's requests, which count only while its text
    /// input is entered.
    fn request(&mut self, request: &v3::Request) {
        use v3::Request;
        if self.entered.is_none() {
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
    /// the field is enabled starts a new field.
    fn commit(&mut self) {
        let Pending {
            enabled,
            surrounding_text,
            content_type,
            cursor_rectangle,
        } = std::mem::take(&mut self.pending);
        // A stated surrounding text is reported, enabled or not.
        if let Some(text) = &surrounding_text {
            self.stated = Some(text.clone().unwrap_or_default());
        }
        let was_enabled = self.committed.is_some();
        let enabled = enabled.unwrap_or(was_enabled);
        if enabled != was_enabled {
            self.toggled = true;
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
    }

    fn text_input(&mut self, event: &v3::Event) {
        let (txn, cursor) = &mut self.txn;
        match event {
            &v3::Event::Enter { surface } => {
                self.entered = Some((self.field(surface), surface));
                self.committed = None;
                self.pending = Pending::default();
            }
            v3::Event::Leave { .. } => {
                if let Some((field, _)) = self.entered {
                    let before = self.composing(field);
                    self.entered = None;
                    self.got.push(Got::Leave { field, before });
                    self.views[field].model.preedit.clear();
                    self.views[field].preedit_cursor = (0, 0);
                }
                self.txn = Default::default();
                self.committed = None;
                self.pending = Pending::default();
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
                let before = view.preedit();
                // The view keeps the cursor as sent, an empty preedit's
                // included.
                view.model.apply_txn(&clamped(&view.model, &txn));
                view.preedit_cursor = cursor;
                self.got.push(Got::Txn {
                    txn,
                    cursor,
                    before,
                });
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

/// What reached the proxy in a step, as far as what the guest shows and
/// types goes.
#[derive(Debug, Default)]
struct Input {
    /// What it was, for a report.
    text: String,
    /// Exo's edit, and how it must reach the guest.
    edit: Option<Edit>,
    /// The composition may end: a press, a focus change or Exo's own
    /// detach, which the guest commits the preedit it shows at.
    ends: bool,
    /// Exo detached on its own (H2), after which the guest shows no
    /// preedit: Exo's detach confirmed its composition (E6).
    detached: bool,
    /// The guest destroyed a field's surface, whose preedit is lost with
    /// its window (F2).
    destroyed: bool,
    /// A guest commit enabled or disabled the field, which starts a new
    /// one, with no preedit.
    toggled: bool,
    /// Exo's `wl_keyboard.key` press of this key, which the guest must
    /// get.
    key: Option<u32>,
    /// Exo's confirmation, and if it is of an autorepeat that Exo's input
    /// method passed, the press the guest may get for it ([`Repeat`]).
    confirm: bool,
    repeat: Option<Repeat>,
}

/// An autorepeat of a held key that Exo's input method passed (E13), whose
/// press the guest gets in its stead (X2').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Repeat {
    key: u32,
    /// The guest must get it. It need not while Exo's keyboard holds a key
    /// whose release died with the input method ([`Origin::stale_held`]):
    /// a press of that key sends only a confirmation like this one, which
    /// the proxy cannot tell from it; nor once the proxy changed what Exo's
    /// text input serves, as the guest's field ended or lost the focus.
    owed: bool,
}

/// Exo's edit (H7 to H9, X1, X2), and how it must change what the guest
/// shows.
#[derive(Debug)]
struct Edit {
    origin: Origin,
    /// It crossed none of the proxy's requests that change what Exo's
    /// text input serves ([`Judge::current`]).
    current: bool,
    /// It crossed the proxy's `reset`.
    fenced: bool,
    expect: Expect,
}

/// How an edit must change what the guest shows.
#[derive(Debug)]
enum Expect {
    /// By this transaction and preedit cursor, or not at all if `None`.
    Exactly(Option<(Txn, (i32, i32))>),
    /// By the transaction Exo's text measures, if the proxy can measure it
    /// too, or else not at all (H9, X1).
    Measured(Measuring),
}

/// The transaction of a delete or a region measured in Exo's text (H9,
/// X1), which the guest gets only if the proxy sends it.
#[derive(Debug)]
struct Measuring {
    /// What Exo's text measures, if the guest can be in step with Exo;
    /// `None` for none.
    measured: Option<(Txn, (i32, i32))>,
    region: bool,
    /// What the guest gets if the proxy cannot measure it: an empty
    /// region over a preedit the guest shows commits it, as Exo ends its
    /// composition as text all the same.
    fallback: Option<(Txn, (i32, i32))>,
    /// The guest held Exo's text, without its composition, which it
    /// showed, cursor included.
    truth_in_step: bool,
    /// The guest showed a preedit.
    over: bool,
}

/// Tier 1: what the guest gets, held to what the simulation did, after
/// every step ([`Judge::see`]) and at quiescence ([`exact`]).
pub(super) struct Judge {
    /// What the guest got and shows.
    guest: Shadow,
    /// How many of the proxy's requests that change what Exo's text input
    /// serves it has sent, and how many it had sent before its last
    /// `reset`.
    requests: u64,
    last_reset: Option<u64>,
    /// The keys the host holds, as its key events said.
    host_held: BTreeSet<u32>,
    /// The key the host pressed last, and the held keys during whose hold
    /// an edit of Exo's ended a composition the guest showed: only their
    /// autorepeats may be recovered (X2').
    last_pressed: Option<u32>,
    composed: BTreeSet<u32>,
    /// Whether the guest showed a preedit at the end of the last step.
    showed: bool,
    /// The `preedit_cursor` since Exo's last edit (B3).
    pieces: Option<i32>,
    /// What reached the proxy in this step.
    input: Input,
    /// The step's `reset`s, and the proxy's reports of surrounding text
    /// and content-type requests that are not the committed field's.
    reset: bool,
    reported: Vec<(String, u32, u32)>,
    typed: Vec<String>,
    /// The press of a held key the guest may get, or is owed, for an
    /// autorepeat ([`Input::repeat`]).
    owed: Option<Repeat>,
    /// The guest got none of a region Exo composed, so it shows another
    /// preedit than Exo composes until Exo's next preedit, commit or
    /// confirmation.
    region_dropped: bool,
    /// The origin of Exo's edit that set the preedit the guest shows.
    shown_origin: Option<Origin>,
    /// The preedit the guest showed when the proxy withheld the field's
    /// latest report, if it did, so that Exo holds that report with the
    /// preedit as its composition.
    report_withheld: Option<String>,
    /// The first failures of each check after a step.
    failures: Vec<(Check, String)>,
    pub counts: Counts,
}

impl Judge {
    /// Fields holding `texts`, on the surfaces `surfaces` names.
    pub fn new(surfaces: Surfaces, texts: &[String]) -> Self {
        Self {
            guest: Shadow::new(surfaces, texts),
            requests: 0,
            last_reset: None,
            host_held: BTreeSet::new(),
            last_pressed: None,
            composed: BTreeSet::new(),
            showed: false,
            pieces: None,
            input: Input::default(),
            reset: false,
            reported: Vec::new(),
            typed: Vec::new(),
            owed: None,
            region_dropped: false,
            shown_origin: None,
            report_withheld: None,
            failures: Vec::new(),
            counts: Counts::default(),
        }
    }

    /// Follows one message at the proxy, at `now`; at the end of a step,
    /// checks what the guest got in it, and a report withheld in it against
    /// `exo`'s tracker.
    pub fn see(&mut self, seen: Seen<'_>, now: Micros, exo: &Exo) {
        match seen {
            Seen::Guest(request) => {
                self.input.text = format!("{request:?}");
                self.guest.see(seen);
                self.input.toggled |= std::mem::take(&mut self.guest.toggled);
            }
            Seen::Host {
                sender,
                event,
                origin,
                region,
            } => {
                self.input.text = format!("{sender} {event:?}");
                self.host(event, origin, region);
            }
            Seen::ToHost(request) => self.request(request),
            Seen::SurfaceDestroyed { field } => {
                self.input.text = format!("the destroy of field {field}'s surface");
                self.input.destroyed = true;
                self.counts.surface_destroys += 1;
                let focused = self.guest.keyboard == Some(field);
                self.counts.focused_surface_destroys += u64::from(focused);
                self.guest.see(seen);
            }
            Seen::StepEnd => {
                self.guest.see(seen);
                self.step_end(now, exo);
            }
            Seen::ToGuest(_) | Seen::GuestPress { .. } => self.guest.see(seen),
        }
    }

    /// Whether Exo sent an event of `origin` in the state the proxy last
    /// asked of its text input: it had handled every request the proxy
    /// sent that changes it ([`changes_service`]).
    fn current(&self, origin: Origin) -> bool {
        origin.requests == self.requests
    }

    /// What reached the proxy from the host.
    fn host(&mut self, event: &AnyEvent, origin: Origin, truth: Option<&RegionTruth>) {
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        match event {
            AnyEvent::WlKeyboard(wl_keyboard::Event::Enter { .. }) => {
                self.host_held.clear();
                self.composed.clear();
                self.input.ends = true;
            }
            AnyEvent::WlKeyboard(wl_keyboard::Event::Leave { .. }) => self.input.ends = true,
            &AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. })
            | &AnyEvent::ZcrExtendedKeyboardV1(peek::Event::PeekKey { key, state, .. }) => {
                if state == pressed {
                    // A key's `wl_keyboard.key` follows its peek, both of
                    // one press.
                    if self.host_held.insert(key) {
                        self.composed.remove(&key);
                    }
                    self.last_pressed = Some(key);
                } else {
                    self.host_held.remove(&key);
                    self.composed.remove(&key);
                }
                if state == pressed && matches!(event, AnyEvent::WlKeyboard(_)) {
                    self.input.key = Some(key);
                }
            }
            AnyEvent::WlPointer(wl_pointer::Event::Button { state, .. })
                if *state == wl_pointer::BUTTON_STATE_PRESSED =>
            {
                self.input.ends = true;
            }
            AnyEvent::WlTouch(wl_touch::Event::Down { .. }) => self.input.ends = true,
            AnyEvent::ZwpTextInputV1(event) => self.text_input(event, origin, truth),
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::ConfirmPreedit { .. }) => {
                self.confirm(origin);
            }
            &AnyEvent::ZcrExtendedTextInputV1(extended::Event::SetPreeditRegion {
                index,
                length,
            }) => {
                let current = self.current(origin);
                // Exo composes a region the guest does not get.
                self.region_dropped |= !current;
                let measuring = current.then(|| {
                    let measured = self.measure_region(truth, index, length);
                    self.measuring(truth, measured, true, length)
                });
                self.edit(
                    origin,
                    measuring.map_or(Expect::Exactly(None), Expect::Measured),
                );
            }
            _ => {}
        }
    }

    /// H2 and H7 to H9.
    fn text_input(&mut self, event: &v1::Event, origin: Origin, truth: Option<&RegionTruth>) {
        match event {
            // H2: a `leave` that crossed no request of the proxy's is Exo's
            // own detach, as when its input method restarts.
            v1::Event::Leave {} if self.current(origin) => {
                self.counts.host_detaches += 1;
                self.input.ends = true;
                self.input.detached = true;
            }
            // B3: it waits for its edit.
            &v1::Event::PreeditCursor { index } => self.pieces = Some(index),
            v1::Event::PreeditString { text, .. } => {
                self.region_dropped = false;
                let text = match text {
                    IndexedString::Utf8(text) => text.clone(),
                    IndexedString::NotUtf8(_) => String::new(),
                };
                let preedit = match text.is_empty() {
                    true => (Txn::default(), (0, 0)),
                    false => {
                        let cursor = preedit_cursor(&text, self.pieces);
                        let txn = Txn {
                            preedit: text,
                            ..Txn::default()
                        };
                        (txn, cursor)
                    }
                };
                self.edit(origin, Expect::Exactly(Some(preedit)));
            }
            // H8: an empty commit is no edit.
            v1::Event::CommitString { text, .. } => {
                self.region_dropped = false;
                let commit = Txn {
                    commit: text.clone(),
                    ..Txn::default()
                };
                let commit = (!text.is_empty()).then_some((commit, (0, 0)));
                self.edit(origin, Expect::Exactly(commit));
            }
            &v1::Event::DeleteSurroundingText { index, length } => {
                let current = self.current(origin);
                let measuring = current.then(|| {
                    let measured = self.measure_delete(truth, index, length);
                    self.measuring(truth, measured, false, length)
                });
                self.edit(
                    origin,
                    measuring.map_or(Expect::Exactly(None), Expect::Measured),
                );
            }
            _ => {}
        }
    }

    /// X2: Exo's confirmation commits the preedit the guest shows. With
    /// none, it is empty: if it is an autorepeat's that the input method
    /// passed (E13), the guest is owed the held key's press, unless it
    /// holds the key itself (X2').
    fn confirm(&mut self, origin: Origin) {
        self.region_dropped = false;
        self.input.confirm = true;
        let shown = self.guest.shown();
        let current = self.current(origin);
        if shown.text.is_empty() && current {
            let held_other = |key: Key| {
                let code = keymap::keycode(key).0;
                self.host_held.iter().any(|&held| held != code)
            };
            match (origin.press, origin.repeat) {
                _ if origin.report => self.counts.confirms_of_reports += 1,
                (Some(press), _) if held_other(press) => self.counts.confirms_before_a_press += 1,
                (None, None) if !self.host_held.is_empty() => {
                    self.counts.confirms_before_a_detach += 1;
                }
                // An autorepeat answered once the key's release, or a
                // keyboard `enter`, reached the proxy is of no held key.
                (None, Some(key)) => {
                    let key = keymap::keycode(key).0;
                    match (
                        self.host_held.contains(&key),
                        self.guest.held.contains(&key),
                    ) {
                        (true, true) => self.counts.recoveries_skipped += 1,
                        // Only a key that ended a composition the guest
                        // showed may be recovered: an autorepeat of another,
                        // as of a key that ended a composition only Exo
                        // held, which a late answer across a focus change
                        // leaves (E5 (c)), is a repeat lost, not text.
                        (true, false) if !self.composed.contains(&key) => {
                            self.counts.repeats_of_no_ended_composition += 1;
                        }
                        (true, false) => {
                            let owed = !origin.stale_held;
                            self.input.repeat = Some(Repeat { key, owed });
                        }
                        (false, _) => {}
                    }
                }
                _ => {}
            }
        }
        let commit = (!shown.text.is_empty()).then(|| shown.commit());
        self.edit(origin, Expect::Exactly(commit));
    }

    /// Exo's edit of `origin` reached the proxy, which must change what the
    /// guest shows as `expect` says if it is current, and not at all
    /// otherwise. It takes the pieces (B3).
    fn edit(&mut self, origin: Origin, expect: Expect) {
        self.pieces = None;
        let shown = self.guest.shown();
        let expect = match expect {
            Expect::Exactly(Some((txn, cursor))) if shown.kept_by(&txn, cursor) => {
                Expect::Exactly(None)
            }
            expect => expect,
        };
        let fenced = self.last_reset >= Some(origin.requests);
        self.input.edit = Some(Edit {
            origin,
            current: self.current(origin),
            fenced,
            expect,
        });
    }

    /// A request the proxy sent the host.
    fn request(&mut self, request: &AnyRequest) {
        if changes_service(request) {
            // The field the press was for no longer takes input as it did.
            if let Some(owed) = &mut self.owed {
                owed.owed = false;
            }
            if matches!(request, AnyRequest::ZwpTextInputV1(v1::Request::Reset {})) {
                self.reset = true;
                self.last_reset = Some(self.requests);
            }
            self.requests += 1;
        }
        if let AnyRequest::ZwpTextInputV1(v1::Request::SetSurroundingText {
            text: IndexedString::Utf8(text),
            cursor,
            anchor,
        }) = request
        {
            self.reported.push((text.clone(), *cursor, *anchor));
        }
        // The step's commit came before the proxy sent anything.
        let committed = self.guest.committed.as_ref();
        let matches = committed.and_then(|field| {
            let surrounding = field.surrounding_text.is_some();
            content_type_matches(request, field.content_type, surrounding)
        });
        if is_content_type(request) && matches != Some(true) {
            self.typed
                .push(format!("{request:?} for the committed {committed:?}"));
        }
    }

    /// The end of a step: what the guest got in it against what reached
    /// the proxy.
    fn step_end(&mut self, now: Micros, exo: &Exo) {
        // An edit of Exo's that ended the composition the guest showed ends
        // it for the key pressed last, if the host still holds it.
        let shows = !self.guest.shown().text.is_empty();
        if self.input.edit.is_some() && self.showed && !shows {
            let held = self.last_pressed.filter(|key| self.host_held.contains(key));
            self.composed.extend(held);
        }
        self.showed = shows;
        let mut input = std::mem::take(&mut self.input);
        input.text = format!("{:.3} ms {}", now.0 as f64 / 1_000.0, input.text);
        let got = std::mem::take(&mut self.guest.got);
        let mut wrong = Vec::new();
        self.keys(&input, &got, &mut wrong);
        if !wrong.is_empty() {
            self.fail(Check::Keys, &input, wrong.join("; "));
        }
        let mut wrong = Vec::new();
        self.text(&input, &got, &mut wrong);
        if !wrong.is_empty() {
            self.fail(Check::Step, &input, wrong.join("; "));
        }
        self.check_reports(&input, exo);
        self.reset = false;
    }

    /// Records the failure of `check` after `input`, the first few of each.
    fn fail(&mut self, check: Check, input: &Input, wrong: String) {
        let failed = self.failures.iter().filter(|(c, _)| *c == check).count();
        if failed < 3 {
            let detail = format!("after {}: {wrong}", input.text);
            self.failures.push((check, detail));
        }
    }

    /// Each press the guest got is Exo's press of the key or the one it may
    /// get for an autorepeat, and is of a key it does not hold; Exo's
    /// press reaches it; and an owed press reaches it before the next
    /// confirmation.
    fn keys(&mut self, input: &Input, got: &[Got], wrong: &mut Vec<String>) {
        let mut sent = input.key;
        let presses = got.iter().filter_map(|got| match *got {
            Got::Key { key, held } => Some((key, held)),
            _ => None,
        });
        for (key, held) in presses {
            if held {
                wrong.push(format!(
                    "a second press of key {key}, which the guest holds"
                ));
            }
            if sent == Some(key) {
                sent = None;
            } else if self.owed.is_some_and(|owed| owed.key == key) {
                self.owed = None;
                self.counts.recovered_keys += 1;
            } else {
                wrong.push(format!(
                    "a press of key {key}, which Exo neither sent nor repeated"
                ));
            }
        }
        if let Some(key) = sent {
            wrong.push(format!("Exo's press of key {key} did not reach the guest"));
        }
        // The proxy decides an autorepeat's press before it handles the
        // next confirmation.
        if input.confirm {
            if let Some(Repeat { key, owed: true }) =
                std::mem::replace(&mut self.owed, input.repeat)
            {
                wrong.push(format!(
                    "an autorepeat of key {key} never reached the guest"
                ));
            }
        }
    }

    /// What the guest shows changes only by the step's edit, as it must
    /// if current, and by a commit of its preedit where the composition
    /// ends; a press or a `leave` drops no preedit; and a `reset` goes to
    /// Exo exactly with the commit at a press.
    fn text(&mut self, input: &Input, got: &[Got], wrong: &mut Vec<String>) {
        let txns = got.iter().filter_map(|got| match got {
            Got::Txn {
                txn,
                cursor,
                before,
            } => Some((txn, *cursor, before)),
            _ => None,
        });
        let changes: Vec<_> = txns
            .clone()
            .filter(|(txn, cursor, before)| !before.kept_by(txn, *cursor))
            .collect();
        let as_got = |changes: &[(&Txn, (i32, i32), &Preedit)]| -> Vec<(Txn, (i32, i32))> {
            changes.iter().map(|(t, c, _)| ((*t).clone(), *c)).collect()
        };
        let commits_shown = |(txn, cursor, before): &(&Txn, (i32, i32), &Preedit)| {
            !before.text.is_empty() && ((*txn).clone(), *cursor) == before.commit()
        };
        let mut pressed = false;
        let mut committed_at_press = false;
        for got in got {
            match got {
                Got::Txn {
                    txn,
                    cursor,
                    before,
                } => committed_at_press |= commits_shown(&(txn, *cursor, before)),
                Got::Pressed { field, shows } => {
                    pressed = true;
                    if !shows.text.is_empty() {
                        wrong.push(format!(
                            "a press landed in field {field}'s preedit {shows:?}, which the guest \
                             did not commit first"
                        ));
                    }
                }
                Got::Leave { field, before } if !before.text.is_empty() && !input.destroyed => {
                    wrong.push(format!(
                        "the text input left field {field} with its preedit {before:?}"
                    ));
                }
                _ => {}
            }
        }
        let committed_at_press = pressed && committed_at_press;
        match &input.edit {
            Some(edit) if !edit.current => {
                if !changes.is_empty() {
                    wrong.push(format!(
                        "the guest got {:?} of an edit Exo made before it handled the proxy's \
                         last request",
                        as_got(&changes)
                    ));
                }
                self.counts.fence_drops += u64::from(edit.fenced);
            }
            Some(Edit {
                expect: Expect::Exactly(expected),
                origin,
                ..
            }) => {
                let expected: Vec<_> = expected.iter().cloned().collect();
                if as_got(&changes) != expected {
                    wrong.push(format!(
                        "the guest got {:?} of Exo's edit, which asks {expected:?}",
                        as_got(&changes)
                    ));
                } else if !expected.is_empty() {
                    self.counts.late_shown += u64::from(origin.late);
                    self.shown_origin = Some(*origin);
                }
            }
            Some(Edit {
                expect: Expect::Measured(measuring),
                origin,
                ..
            }) => {
                let all: Vec<_> = txns.map(|(t, c, _)| (t.clone(), c)).collect();
                self.resolve(measuring, *origin, &all, &as_got(&changes), wrong);
            }
            None if input.ends => {
                let commit = changes.first().filter(|change| commits_shown(change));
                if changes.len() > usize::from(commit.is_some()) {
                    wrong.push(format!(
                        "the guest got {:?} where only its preedit may be committed",
                        as_got(&changes)
                    ));
                }
            }
            None if input.toggled => {
                let cleared = |(txn, _, _): &(&Txn, _, _)| **txn == Txn::default();
                if !changes.iter().all(cleared) {
                    wrong.push(format!(
                        "the guest got {:?} for a commit that starts a new field",
                        as_got(&changes)
                    ));
                }
            }
            None if !changes.is_empty() => {
                wrong.push(format!("the guest got {:?} of no edit", as_got(&changes)));
            }
            None => {}
        }
        if input.detached && !self.guest.shown().text.is_empty() {
            wrong.push(format!(
                "the guest still shows {:?} after Exo's detach confirmed it",
                self.guest.shown()
            ));
        }
        if self.reset != committed_at_press {
            wrong.push(format!(
                "the proxy reset Exo: {}, the guest committed its preedit at a press: \
                 {committed_at_press}",
                self.reset
            ));
        }
        self.counts.press_commits += u64::from(committed_at_press);
        if self.guest.shown().text.is_empty() {
            self.shown_origin = None;
        }
    }

    /// The step of Exo's delete or region (`measuring`), of `origin`, in
    /// which the guest got the transactions `all`, of which `changes`
    /// changed what it showed: either the one Exo's text measures, or a
    /// drop: none, or the commit of the shown preedit for an empty region.
    fn resolve(
        &mut self,
        measuring: &Measuring,
        origin: Origin,
        all: &[(Txn, (i32, i32))],
        changes: &[(Txn, (i32, i32))],
        wrong: &mut Vec<String>,
    ) {
        let in_step = measuring.measured.is_some() && measuring.truth_in_step;
        match &measuring.measured {
            Some(measured) if all.contains(measured) && changes.len() <= 1 => {
                let (txn, _) = measured;
                if measuring.region {
                    self.region_dropped = false;
                }
                match measuring.region {
                    false => self.counts.deletes += 1,
                    true if !txn.preedit.is_empty() => {
                        self.counts.regions += 1;
                        self.counts.regions_over_preedits += u64::from(measuring.over);
                    }
                    true => {}
                }
                self.counts.late_shown += u64::from(origin.late);
                self.shown_origin = Some(origin);
                return;
            }
            _ => {}
        }
        match &measuring.fallback {
            Some(fallback) if changes == [fallback.clone()] => {
                self.counts.drops_truth_in_step += u64::from(in_step);
            }
            None if changes.is_empty() => {
                self.counts.drops_truth_in_step += u64::from(in_step);
                self.region_dropped |= measuring.region;
            }
            _ => wrong.push(format!(
                "the guest got {changes:?} of Exo's delete or region, which Exo's text \
                 measures as {:?}, else {:?}",
                measuring.measured, measuring.fallback
            )),
        }
    }

    /// H9 and X1: the step's delete or region, which Exo's text before it
    /// (`truth`) measures to `measured`, the transaction the guest gets if
    /// the proxy sends it, which its Exo mirror measures.
    fn measuring(
        &self,
        truth: Option<&RegionTruth>,
        measured: Option<(Txn, (i32, i32))>,
        region: bool,
        length: u32,
    ) -> Measuring {
        let shown = self.guest.shown();
        let fallback = (region && length == 0 && !shown.text.is_empty()).then(|| shown.commit());
        Measuring {
            measured,
            region,
            fallback,
            truth_in_step: truth.is_some_and(|truth| self.truth_in_step(truth)),
            over: !shown.text.is_empty(),
        }
    }

    /// Whether the guest, as it shows the entered field, holds Exo's text
    /// before an event (`truth`) without its composition, which it shows
    /// as its preedit, with its cursor where Exo's is: in step, as a
    /// perfect mirror would find it.
    fn truth_in_step(&self, truth: &RegionTruth) -> bool {
        let (Some((field, _)), Some(held)) = (self.guest.entered, held(truth)) else {
            return false;
        };
        let model = &self.guest.views[field].model;
        let text = [&truth.text[..held.start], &truth.text[held.end..]].concat();
        model.text == text && model.cursor == held.start && model.preedit == truth.text[held]
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
    ) -> Option<(Txn, (i32, i32))> {
        let (truth, held) = truth.and_then(|truth| Some((truth, held(truth)?)))?;
        let start = usize::try_from(truth.cursor as i64 + i64::from(index)).ok()?;
        let end = start + length as usize;
        let text = truth.text.get(start..end)?;
        if text.is_empty() {
            let commit = truth.text[held].to_owned();
            let txn = Txn {
                commit,
                ..Txn::default()
            };
            return Some((txn, (0, 0)));
        }
        if held.start < start || end < held.end {
            return None;
        }
        let before = truth.cursor.clamp(start, end) - start;
        let cursor = preedit_cursor(text, Some(self.pieces.unwrap_or(before as i32)));
        let txn = Txn {
            delete: (held.start - start, end - held.end),
            preedit: text.to_owned(),
            ..Txn::default()
        };
        Some((txn, cursor))
    }

    /// H9: Exo deletes `length` bytes from `index` of its text (`truth`),
    /// which holds no composition, around its cursor, the guest's. `None`
    /// if no guest can be in step, or Exo's text measures no transaction.
    fn measure_delete(
        &mut self,
        truth: Option<&RegionTruth>,
        index: i32,
        length: u32,
    ) -> Option<(Txn, (i32, i32))> {
        let (truth, held) = truth.and_then(|truth| Some((truth, held(truth)?)))?;
        let start = usize::try_from(index).ok();
        let lengths = start.and_then(|start| around(&truth.text, held.start, start as i64, length));
        let measured = lengths.filter(|&lengths| held.is_empty() && lengths != (0, 0));
        self.counts.deletes_misplaced += u64::from(measured.is_none());
        let (before, after) = measured?;
        let txn = Txn {
            delete: (before, after),
            ..Txn::default()
        };
        Some((txn, (0, 0)))
    }

    /// The reports the proxy sent the host in the step must be what the
    /// guest committed in it: the surrounding text a commit stated, once,
    /// and every content type and surrounding-text support as the committed
    /// field's.
    ///
    /// With an Exo mirror the proxy may withhold a report while the guest
    /// shows a preedit, if `exo`'s tracker holds the report with that
    /// preedit as its composition at the report's cursor.
    fn check_reports(&mut self, input: &Input, exo: &Exo) {
        let reported = std::mem::take(&mut self.reported);
        let expected: Vec<_> = self.guest.stated.take().into_iter().collect();
        let mut wrong = std::mem::take(&mut self.typed);
        let shown = self.guest.shown().text;
        let withheld = match &expected[..] {
            [report] if reported.is_empty() && !shown.is_empty() => Some(report),
            _ => None,
        };
        if let Some(report) = withheld {
            self.counts.reports_withheld += 1;
            if !exo.holds_with_composition(report, &shown) {
                wrong.push(format!(
                    "withheld the surrounding text {report:?}, which Exo's tracker does not \
                     hold with the composition {shown:?}"
                ));
            }
        } else if reported != expected {
            wrong.push(format!(
                "reported the surrounding text {reported:?}, the guest committed {expected:?}"
            ));
        }
        if !expected.is_empty() {
            self.report_withheld = withheld.map(|_| shown);
        }
        if !wrong.is_empty() {
            self.fail(
                Check::Report,
                input,
                format!("the proxy {}", wrong.join("; ")),
            );
        }
    }
}

/// Where a guest in step with Exo's text before the event, `truth`, has
/// its preedit: the composition, which it shows, with the rest of Exo's
/// text and its cursor at the composition's start; or else, with no
/// selection, empty at Exo's cursor, the guest's. `None` with a
/// selection, in which no guest is in step.
///
/// Whether the guest is in step is the proxy's to tell, and the guest's
/// text is no guide: a client model's can differ, as GTK's Backspace
/// leaves a syllable's first jamo (C14). The judge holds the proxy only
/// to a measurement in Exo's text, if it makes one, or else to a drop.
fn held(truth: &RegionTruth) -> Option<std::ops::Range<usize>> {
    match &truth.composition {
        Some(composition) => Some(composition.clone()),
        None => {
            let selection = truth.selection;
            (selection.start == selection.end).then_some(selection.end..selection.end)
        }
    }
}

/// Tier 1 at quiescence:
///
/// - the steps that failed ([`Judge::see`]), the first few of each check;
/// - a press of a held key the guest is still owed, and a key the guest
///   still holds that Exo's keyboard does not;
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
/// And Exo may compose a region the guest never got, until Exo next
/// composes, commits or confirms ([`Judge::region_dropped`]).
pub(super) fn exact(judge: &Judge, exo: &Exo, engine: &str, resets: u64) -> Vec<Failure> {
    let guest = &judge.guest;
    let mut failures = Vec::new();
    let failure = |check, field, detail| Failure {
        tier: Tier::Exact,
        check,
        field,
        detail,
    };
    for check in [Check::Step, Check::Keys, Check::Report] {
        let steps = judge.failures.iter().filter(|(c, _)| *c == check);
        let steps: Vec<&str> = steps.map(|(_, detail)| detail.as_str()).collect();
        if !steps.is_empty() {
            failures.push(failure(check, None, steps.join("\n")));
        }
    }
    let mut keys = Vec::new();
    if let Some(Repeat { key, owed: true }) = judge.owed {
        keys.push(format!(
            "an autorepeat of key {key} never reached the guest"
        ));
    }
    let held = exo.held_keys();
    let stuck: Vec<u32> = guest
        .held
        .iter()
        .copied()
        .filter(|key| !held.contains(key))
        .collect();
    if !stuck.is_empty() {
        keys.push(format!(
            "the guest holds keys {stuck:?}, which Exo's keyboard released"
        ));
    }
    if !keys.is_empty() {
        failures.push(failure(Check::Keys, None, keys.join("\n")));
    }
    let composing = exo.composition().filter(|_| !judge.region_dropped);
    if let (Some(field), Some(tracked)) = (guest.keyboard, composing) {
        let preedit = &guest.views[field].model.preedit;
        let dropped = judge
            .shown_origin
            .is_some_and(|origin| (origin.late || origin.resets < resets) && engine.is_empty());
        // Without the extension, Exo composes a reconverted region the
        // guest never sees: while it stands uncommitted, the guest still
        // shows the composition that stood before it.
        let tracked = exo.composition_shown_instead().unwrap_or(tracked);
        if *preedit != tracked && preedit != engine && !(dropped && tracked.is_empty()) {
            let detail = format!(
                "the guest shows {preedit:?}; Exo tracks {tracked:?}, its engine composes {engine:?}"
            );
            failures.push(failure(Check::Composition, Some(field), detail));
        }
    }
    let mut expected: Vec<exo::Field> = guest.committed.iter().map(V3Field::on_host).collect();
    let held = exo.focused_fields();
    // Exo still has the report before a withheld one, and its tracker holds
    // the withheld one with the preedit the guest showed as its composition
    // ([`Judge::check_reports`]).
    if let Some(shown) = &judge.report_withheld {
        for (expected, held) in expected.iter_mut().zip(&held) {
            if exo.holds_with_composition(&expected.surrounding_text, shown) {
                expected.surrounding_text = held.surrounding_text.clone();
            }
        }
    }
    if held != expected {
        let detail = format!("committed: {expected:?}\nExo holds: {held:?}");
        let field = guest.entered.map(|(field, _)| field);
        failures.push(failure(Check::FieldState, field, detail));
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
            failures.push(failure(Check::Text, Some(i), detail));
        }
    }
    if client.focused_field() != Some(reference.focus()) {
        let detail = format!(
            "intended field {}, client {:?}",
            reference.focus(),
            client.focused_field()
        );
        failures.push(failure(Check::Focus, None, detail));
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
                check: Check::NoLoss,
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

/// A model as `"text with | at the cursor" preedit "…"`.
fn show_model(model: &TextModel) -> String {
    let (before, after) = model.text.split_at(model.cursor);
    format!(
        "{:?} preedit {:?}",
        format!("{before}|{after}"),
        model.preedit
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1 as manager;
    use crate::protocols::wayland::{wl_callback, wl_display};
    use crate::sim::client::{gtk::Gtk, Field};

    /// The host surfaces of field 0 and field 1, and their guest ones.
    const HOST: [u32; 2] = [20, 21];
    const GUEST: [u32; 2] = [30, 31];
    const BACKSPACE: u32 = 14;

    fn texts(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| text.to_string()).collect()
    }

    /// What the proxy sends in a step.
    enum Out {
        Host(AnyRequest),
        Guest(AnyEvent),
        /// A press the guest gets in `field`.
        Press(usize),
    }

    /// The end of a step: the proxy sends `out`.
    fn sends(judge: &mut Judge, out: &[Out]) {
        let exo = Exo::default();
        for out in out {
            let seen = match out {
                Out::Host(request) => Seen::ToHost(request),
                Out::Guest(event) => Seen::ToGuest(event),
                &Out::Press(field) => Seen::GuestPress {
                    surface: GUEST[field],
                    at: Target::Start,
                },
            };
            judge.see(seen, Micros(0), &exo);
        }
        judge.see(Seen::StepEnd, Micros(0), &exo);
    }

    /// A step: Exo's `event`, of `origin`, reaches the proxy, which sends
    /// `out`.
    fn host(judge: &mut Judge, event: impl Into<AnyEvent>, origin: Origin, out: &[Out]) {
        let event = event.into();
        let seen = Seen::Host {
            sender: 1,
            event: &event,
            origin,
            region: None,
        };
        judge.see(seen, Micros(0), &Exo::default());
        sends(judge, out);
    }

    /// A step: the guest's `request` reaches the proxy, which sends `out`.
    fn guest(judge: &mut Judge, request: v3::Request, out: &[Out]) {
        judge.see(Seen::Guest(&request), Micros(0), &Exo::default());
        sends(judge, out);
    }

    /// An event of Exo's that crossed no request of the proxy's.
    fn current(judge: &Judge) -> Origin {
        Origin {
            requests: judge.requests,
            ..Origin::default()
        }
    }

    /// The checks that failed so far, one entry a failed step.
    fn failed(judge: &Judge) -> Vec<Check> {
        judge.failures.iter().map(|(check, _)| *check).collect()
    }

    fn activate() -> AnyRequest {
        v1::Request::Activate {
            seat: 1,
            surface: HOST[0],
        }
        .into()
    }

    fn reported(text: &str, cursor: u32) -> AnyRequest {
        v1::Request::SetSurroundingText {
            text: IndexedString::Utf8(text.to_owned()),
            cursor,
            anchor: cursor,
        }
        .into()
    }

    /// The transaction `txn`, with its preedit at `cursor`, to the guest.
    fn to_guest(txn: &Txn, cursor: (i32, i32)) -> Vec<Out> {
        let mut out: Vec<AnyEvent> = Vec::new();
        if txn.delete != (0, 0) {
            let (before, after) = txn.delete;
            out.push(
                v3::Event::DeleteSurroundingText {
                    before_length: before as u32,
                    after_length: after as u32,
                }
                .into(),
            );
        }
        if !txn.commit.is_empty() {
            let text = Some(txn.commit.clone());
            out.push(v3::Event::CommitString { text }.into());
        }
        if !txn.preedit.is_empty() {
            let text = Some(IndexedString::Utf8(txn.preedit.clone()));
            let (cursor_begin, cursor_end) = cursor;
            let preedit = v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            };
            out.push(preedit.into());
        }
        out.push(v3::Event::Done { serial: 0 }.into());
        out.into_iter().map(Out::Guest).collect()
    }

    /// The guest is shown `text` as its preedit, at its end.
    fn shows(text: &str) -> Vec<Out> {
        let end = text.len() as i32;
        let txn = Txn {
            preedit: text.to_owned(),
            ..Txn::default()
        };
        to_guest(&txn, (end, end))
    }

    /// The guest commits `text`.
    fn commits(text: &str) -> Vec<Out> {
        let txn = Txn {
            commit: text.to_owned(),
            ..Txn::default()
        };
        to_guest(&txn, (0, 0))
    }

    fn preedit(text: &str) -> v1::Event {
        v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8(text.to_owned()),
            commit: text.to_owned(),
        }
    }

    fn keyboard_enter(surface: u32) -> wl_keyboard::Event {
        wl_keyboard::Event::Enter {
            serial: 1,
            surface,
            keys: Vec::new(),
        }
    }

    /// Fields holding `fields`, the keyboard and the text input on field 0,
    /// which the guest enabled; Exo attached it.
    fn entered(fields: &[&str]) -> Judge {
        let surfaces = Surfaces::new(&GUEST[..fields.len()], &HOST[..fields.len()]);
        let mut judge = Judge::new(surfaces, &texts(fields));
        let entered = [
            Out::Guest(keyboard_enter(GUEST[0]).into()),
            Out::Guest(v3::Event::Enter { surface: GUEST[0] }.into()),
        ];
        host(
            &mut judge,
            keyboard_enter(HOST[0]),
            Origin::default(),
            &entered,
        );
        guest(&mut judge, v3::Request::Enable {}, &[]);
        let enabled = [
            Out::Host(activate()),
            Out::Host(reported("", 0)),
            Out::Guest(v3::Event::Done { serial: 1 }.into()),
        ];
        guest(&mut judge, v3::Request::Commit {}, &enabled);
        judge
    }

    #[test]
    fn exos_edit_reaches_the_guest_unless_it_crossed_a_request_of_the_proxys() {
        let mut judge = entered(&["0"]);
        let now = current(&judge);
        host(&mut judge, v1::Event::PreeditCursor { index: 0 }, now, &[]);
        host(
            &mut judge,
            preedit("가"),
            now,
            &to_guest(
                &Txn {
                    preedit: "가".to_owned(),
                    ..Txn::default()
                },
                (0, 0),
            ),
        );
        // Dropped, or another edit than Exo's.
        host(&mut judge, preedit("각"), now, &[]);
        host(&mut judge, preedit("각"), now, &shows("간"));
        assert_eq!(failed(&judge), [Check::Step, Check::Step]);
        assert_eq!(judge.guest.shown().text, "간");
        // Exo's commit, made before it handled the proxy's `deactivate`,
        // must not reach the guest: it ends a composition the proxy has
        // left.
        let done = || wl_callback::Event::Done { callback_data: 0 };
        let deactivate = v1::Request::Deactivate { seat: 1 }.into();
        host(&mut judge, done(), now, &[Out::Host(deactivate)]);
        host(
            &mut judge,
            v1::Event::CommitString {
                serial: 0,
                text: "각".into(),
            },
            now,
            &[],
        );
        assert_eq!(failed(&judge).len(), 2);
        host(
            &mut judge,
            v1::Event::CommitString {
                serial: 0,
                text: "각".into(),
            },
            now,
            &commits("각"),
        );
        assert_eq!(failed(&judge).len(), 3);
        // No step but Exo's edit or one that ends the composition changes
        // what the guest shows.
        let mut judge = entered(&["0"]);
        let now = current(&judge);
        host(&mut judge, done(), now, &commits("a"));
        assert_eq!(failed(&judge), [Check::Step]);
    }

    #[test]
    fn a_press_or_a_leave_commits_the_preedit_and_a_press_resets_exo_with_it() {
        let reset = || Out::Host(v1::Request::Reset {}.into());
        let sync = || Out::Host(wl_display::Request::Sync { callback: 9 }.into());
        let down = || wl_touch::Event::Down {
            serial: 0,
            time: 0,
            surface: HOST[0],
            id: 0,
            x: 0,
            y: 0,
        };
        let then = |mut first: Vec<Out>, rest: Vec<Out>| {
            first.extend(rest);
            first
        };
        // What the proxy sends at a press in field 0 while the guest shows
        // a preedit there, and the checks that fail.
        let presses: [(Vec<Out>, Vec<Check>); 4] = [
            (
                then(commits("가"), vec![Out::Press(0), reset(), sync()]),
                vec![],
            ),
            (vec![Out::Press(0)], vec![Check::Step]),
            (then(commits("가"), vec![Out::Press(0)]), vec![Check::Step]),
            (vec![Out::Press(0), reset(), sync()], vec![Check::Step]),
        ];
        for (out, checks) in presses {
            let mut judge = entered(&["0"]);
            let now = current(&judge);
            host(&mut judge, preedit("가"), now, &shows("가"));
            host(&mut judge, down(), Origin::default(), &out);
            assert_eq!(failed(&judge), checks);
        }
        // A `leave` that drops the preedit.
        let mut judge = entered(&["0", "1"]);
        let now = current(&judge);
        host(&mut judge, preedit("가"), now, &shows("가"));
        let leave = wl_keyboard::Event::Leave {
            serial: 2,
            surface: HOST[0],
        };
        let left = Out::Guest(v3::Event::Leave { surface: GUEST[0] }.into());
        host(&mut judge, leave, Origin::default(), &[left]);
        assert_eq!(failed(&judge), [Check::Step]);
        // A reset with no press.
        let mut judge = entered(&["0"]);
        let now = current(&judge);
        host(&mut judge, preedit("가"), now, &shows("가"));
        let done = wl_callback::Event::Done { callback_data: 0 };
        host(&mut judge, done, Origin::default(), &[reset(), sync()]);
        assert_eq!(failed(&judge), [Check::Step]);
    }

    #[test]
    fn each_press_the_guest_gets_is_exos_or_an_autorepeat_the_input_method_passed() {
        let key = |key, state| wl_keyboard::Event::Key {
            serial: 1,
            time: 0,
            key,
            state,
        };
        let pressed = wl_keyboard::KEY_STATE_PRESSED;
        let press = |code| Out::Guest(key(code, pressed).into());
        let release = |code| Out::Guest(key(code, wl_keyboard::KEY_STATE_RELEASED).into());
        let mut judge = entered(&["0"]);
        let any = Origin::default();
        // Exo's press reaches the guest, not another, and not twice.
        host(&mut judge, key(30, pressed), any, &[press(30)]);
        assert_eq!(failed(&judge), []);
        host(&mut judge, key(31, pressed), any, &[]);
        host(&mut judge, key(32, pressed), any, &[press(32), press(33)]);
        host(&mut judge, key(32, pressed), any, &[press(32)]);
        assert_eq!(failed(&judge), [Check::Keys; 3]);
        // Backspace, which the input method took, is held, and it passes
        // an autorepeat: the guest gets its press before the next
        // confirmation, once.
        let mut judge = entered(&["0"]);
        let peek = || peek::Event::PeekKey {
            serial: 1,
            time: 0,
            key: BACKSPACE,
            state: pressed,
        };
        host(&mut judge, peek(), any, &[]);
        // It emptied a composition the guest showed, as `step_end` sees.
        judge.composed.insert(BACKSPACE);
        let confirm = || extended::Event::ConfirmPreedit {
            selection_behavior: 1,
        };
        let repeat = Origin {
            repeat: Some(Key::Backspace),
            ..current(&judge)
        };
        let done = || wl_callback::Event::Done { callback_data: 0 };
        host(&mut judge, confirm(), repeat, &[]);
        host(
            &mut judge,
            done(),
            any,
            &[press(BACKSPACE), release(BACKSPACE)],
        );
        host(
            &mut judge,
            done(),
            any,
            &[press(BACKSPACE), release(BACKSPACE)],
        );
        assert_eq!(failed(&judge), [Check::Keys]);
        // One it never gets fails at the next confirmation, or at the end.
        host(&mut judge, confirm(), repeat, &[]);
        host(&mut judge, confirm(), repeat, &[]);
        assert_eq!(failed(&judge), [Check::Keys; 2]);
        let keys = exact(&judge, &Exo::default(), "", 0);
        assert!(keys.iter().any(|failure| failure.check == Check::Keys));
        // It need not get it while Exo's keyboard holds a key whose release
        // died with the input method, which a confirmation like it may
        // announce.
        let mut judge = entered(&["0"]);
        host(&mut judge, peek(), any, &[]);
        judge.composed.insert(BACKSPACE);
        let stale = Origin {
            stale_held: true,
            ..repeat
        };
        host(&mut judge, confirm(), stale, &[]);
        host(
            &mut judge,
            confirm(),
            stale,
            &[press(BACKSPACE), release(BACKSPACE)],
        );
        host(&mut judge, confirm(), stale, &[]);
        assert_eq!(failed(&judge), []);
        assert_eq!(judge.counts.recovered_keys, 1);
        // An autorepeat of a key that ended no composition the guest showed
        // is lost, not owed, and the guest may not get it.
        let mut judge = entered(&["0"]);
        host(&mut judge, peek(), any, &[]);
        host(&mut judge, confirm(), repeat, &[]);
        assert_eq!(failed(&judge), []);
        assert_eq!(judge.counts.repeats_of_no_ended_composition, 1);
        host(
            &mut judge,
            done(),
            any,
            &[press(BACKSPACE), release(BACKSPACE)],
        );
        assert_eq!(failed(&judge), [Check::Keys]);
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
    }

    /// A step: Exo's delete or region `event`, which Exo's text `truth`
    /// measures, reaches the proxy, which sends `out`.
    fn measured(judge: &mut Judge, event: &AnyEvent, truth: Option<&RegionTruth>, out: &[Out]) {
        let seen = Seen::Host {
            sender: 1,
            event,
            origin: current(judge),
            region: truth,
        };
        judge.see(seen, Micros(0), &Exo::default());
        sends(judge, out);
    }

    /// The guest states `text` with the cursor at `cursor`, which the proxy
    /// reports.
    fn report(judge: &mut Judge, text: &str, cursor: u32) {
        let surrounding = v3::Request::SetSurroundingText {
            text: IndexedString::Utf8(text.to_owned()),
            cursor: cursor as i32,
            anchor: cursor as i32,
        };
        guest(judge, surrounding, &[]);
        let done = Out::Guest(v3::Event::Done { serial: 2 }.into());
        guest(
            judge,
            v3::Request::Commit {},
            &[Out::Host(reported(text, cursor)), done],
        );
    }

    /// Exo's delete or region reaches the guest as Exo's text before it
    /// measures it, or not at all, a drop. A dropped region excuses the
    /// guest's preedit until Exo next composes.
    #[test]
    fn exos_delete_or_region_reaches_the_guest_as_exos_text_measures_it_or_not_at_all() {
        let delete: AnyEvent = v1::Event::DeleteSurroundingText {
            index: 1,
            length: 2,
        }
        .into();
        let region: AnyEvent = extended::Event::SetPreeditRegion {
            index: -1,
            length: 1,
        }
        .into();
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
        type Case<'a> = (&'a AnyEvent, Vec<Out>, Option<&'static str>);
        let cases: [Case; 4] = [
            (&delete, to_guest(&txn((2, 0), ""), (0, 0)), Some("0")),
            (&delete, vec![], Some("0ab")),
            (&delete, to_guest(&txn((1, 0), ""), (0, 0)), None),
            (&region, to_guest(&txn((1, 0), "b"), (1, 1)), Some("0a")),
        ];
        for (event, out, text) in cases {
            let mut judge = entered(&["0ab"]);
            report(&mut judge, "0ab", 3);
            let case = format!("{event:?} {:?}", judge.guest.views[0]);
            measured(&mut judge, event, Some(&truth), &out);
            assert_eq!(judge.failures.is_empty(), text.is_some(), "{case}");
            if let Some(text) = text {
                assert_eq!(judge.guest.views[0].model.text, text, "{case}");
                let dropped = out.is_empty();
                let region = matches!(event, AnyEvent::ZcrExtendedTextInputV1(_));
                assert_eq!(judge.region_dropped, dropped && region, "{case}");
                assert_eq!(
                    judge.counts.drops_truth_in_step,
                    u64::from(dropped),
                    "{case}"
                );
            }
        }
    }

    /// An empty region over a preedit the guest shows that the proxy does
    /// not measure must commit that preedit: Exo ends its composition as
    /// text all the same.
    #[test]
    fn an_unmeasured_empty_region_must_commit_the_shown_preedit() {
        let region: AnyEvent = extended::Event::SetPreeditRegion {
            index: 0,
            length: 0,
        }
        .into();
        for (out, fails) in [(commits("가"), false), (vec![], true)] {
            let mut judge = entered(&["0ab"]);
            report(&mut judge, "0ab", 3);
            let now = current(&judge);
            host(&mut judge, preedit("가"), now, &shows("가"));
            measured(&mut judge, &region, None, &out);
            assert_eq!(judge.failures.is_empty(), !fails);
        }
    }

    #[test]
    fn every_report_to_the_host_must_be_what_the_guest_committed_in_the_step() {
        let mut judge = entered(&["0ab"]);
        let surrounding = |cursor| v3::Request::SetSurroundingText {
            text: IndexedString::Utf8("0ab".to_owned()),
            cursor,
            anchor: cursor,
        };
        let commit = || v3::Request::Commit {};
        // A report with another cursor, a missing report, and one no commit
        // stated.
        guest(&mut judge, surrounding(3), &[]);
        guest(&mut judge, commit(), &[Out::Host(reported("0ab", 0))]);
        guest(&mut judge, surrounding(2), &[]);
        guest(&mut judge, commit(), &[]);
        assert_eq!(failed(&judge), [Check::Report; 2]);
        let done = wl_callback::Event::Done { callback_data: 0 };
        host(
            &mut judge,
            done,
            Origin::default(),
            &[Out::Host(reported("0ab", 2))],
        );
        // A content type that is not the committed one.
        let typed = extended::Request::SetInputType {
            input_type: extended::INPUT_TYPE_PASSWORD,
            input_mode: extended::INPUT_MODE_DEFAULT,
            input_flags: extended::INPUT_FLAGS_HAS_BEEN_PASSWORD,
            learning_mode: extended::LEARNING_MODE_DISABLED,
            inline_composition_support: extended::INLINE_COMPOSITION_SUPPORT_SUPPORTED,
        };
        guest(&mut judge, commit(), &[Out::Host(typed.into())]);
        // The first three steps of a check are kept.
        assert_eq!(failed(&judge), [Check::Report; 3]);
    }

    #[test]
    #[should_panic(expected = "the proxy named guest surface 30, which the guest destroyed")]
    fn the_shadow_fails_on_an_event_naming_a_destroyed_surface() {
        let surfaces = Surfaces::new(&GUEST, &HOST);
        let mut shadow = Shadow::new(surfaces.clone(), &texts(&["0", "1"]));
        surfaces.destroy(GUEST[0]);
        shadow.see(Seen::SurfaceDestroyed { field: 0 });
        shadow.see(Seen::StepEnd);
        shadow.see(Seen::ToGuest(&keyboard_enter(GUEST[0]).into()));
    }

    #[test]
    #[should_panic(expected = "the text input stays entered on guest surface 30")]
    fn the_shadow_fails_on_a_text_input_entered_on_a_destroyed_surface() {
        let surfaces = Surfaces::new(&GUEST, &HOST);
        let mut shadow = Shadow::new(surfaces.clone(), &texts(&["0", "1"]));
        let enter = v3::Event::Enter { surface: GUEST[0] }.into();
        shadow.see(Seen::ToGuest(&enter));
        surfaces.destroy(GUEST[0]);
        shadow.see(Seen::SurfaceDestroyed { field: 0 });
        shadow.see(Seen::StepEnd);
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
    fn the_shadow_applies_what_the_guest_got_at_done() {
        let surfaces = Surfaces::new(&GUEST, &HOST);
        let mut shadow = Shadow::new(surfaces, &texts(&["0", "1"]));
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
        // A delete that does not fit is clamped to whole characters.
        let mut shadow = Shadow::new(Surfaces::new(&GUEST, &HOST), &texts(&["a가", "1"]));
        shadow.see(Seen::ToGuest(
            &v3::Event::Enter { surface: GUEST[0] }.into(),
        ));
        for (before_length, after_length, text) in [(1, 2, "a가"), (9, 0, "")] {
            let delete = v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            };
            shadow.see(Seen::ToGuest(&delete.into()));
            shadow.see(Seen::ToGuest(&v3::Event::Done { serial: 0 }.into()));
            assert_eq!(shadow.views[0].model.text, text);
        }
    }

    /// An Exo whose text input 10 is attached on field 0.
    fn attached_exo() -> Exo {
        let mut exo = Exo::default();
        exo.set_text_input_focus(Some(HOST[0]));
        exo.request(3, &manager::Request::CreateTextInput { id: 10 }.into());
        exo.request(10, &activate());
        exo
    }

    #[test]
    fn exos_composition_must_be_the_engines_or_the_one_it_tracks() {
        // Exo tracks no composition.
        let exo = attached_exo();
        let checks = |judge: &Judge, engine: &str, resets| -> Vec<Check> {
            let failures = exact(judge, &exo, engine, resets);
            let checks = failures.iter().map(|failure| failure.check);
            checks
                .filter(|&check| check == Check::Composition)
                .collect()
        };
        let mut judge = entered(&["0"]);
        // The guest shows nothing, which Exo tracks.
        assert_eq!(checks(&judge, "가", 0), []);
        // The guest shows what the engine composes.
        let now = current(&judge);
        host(&mut judge, preedit("가"), now, &shows("가"));
        assert_eq!(checks(&judge, "가", 0), []);
        // Nothing composes it.
        assert_eq!(checks(&judge, "", 0), [Check::Composition]);
        // Unless the engine reset since it made it, without Exo's client
        // hearing of it.
        assert_eq!(checks(&judge, "", 1), []);
        assert_eq!(checks(&judge, "각", 1), [Check::Composition]);
        // Or the guest got it late.
        let late = Origin {
            late: true,
            ..current(&judge)
        };
        host(&mut judge, preedit("각"), late, &shows("각"));
        assert_eq!(checks(&judge, "", 0), []);
        assert_eq!(judge.counts.late_shown, 1);
    }

    #[test]
    fn a_hidden_region_excuses_a_composition_only_while_it_stands() {
        use crate::sim::engine::{EngineOut, Request, TextAction};
        // Exo, with no extension, composes a region of "0a" that the guest
        // never sees.
        let mut exo = attached_exo();
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
        let judge = entered(&["0a"]);
        let checks = |exo: &Exo, engine: &str| -> Vec<Check> {
            let failures = exact(&judge, exo, engine, 0);
            let checks = failures.iter().map(|failure| failure.check);
            checks
                .filter(|&check| check == Check::Composition)
                .collect()
        };
        assert_eq!(checks(&exo, ""), []);
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
        assert_eq!(checks(&exo, "ㄱ"), [Check::Composition]);
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
        assert_eq!(checks, [(Check::Text, Some(1)), (Check::Focus, None)]);
    }
}
