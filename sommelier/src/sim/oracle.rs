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
//! proxy and what it sent the host, in order ([`Seen`]), and applies the
//! text semantics of the design's section 2 (`docs/IME_REDESIGN.md`) to
//! an ideal client's [`TextModel`] per field. It takes the proxy's own
//! activation decisions (`activate`, `deactivate`, the `reset` and its
//! fence) from what the proxy sent, as given, and so the keys it passed,
//! which are the keyboard concern's. [`Shadow`] is the same ideal
//! client fed what the proxy actually sent the guest. They must agree
//! ([`exact`]), whatever the timing, since neither lags. Exo's composition
//! must equal the guest's preedit, and Exo's field must hold what the
//! guest last committed ([`V3Field`]).
//!
//! **Tier 2, user intent, settled cases only.** The client model's fields
//! must equal those of [`Reference`], which applied the user's actions
//! through an ideal input method ([`intended`]). Both sides are compared
//! after NFC normalisation, so that it does not matter which form of a
//! syllable a client keeps (C14).
//!
//! A tier-2 failure the proxy is known to cause is a [`KnownIssue`]
//! ([`classify`]), counted rather than failed.
//!
//! The engine makes no deletes or preedit regions before D6, so `SpecText`
//! does not track whether the guest's text is ahead of what it reported
//! (H9, X1), and panics on either event. For the same reason no case yet
//! needs Chromium's prediction to replay Exo's history (the design's
//! 4.5): the text a client predicts matters only to measure a delete.

use super::client::Client;
use super::editor::{Reference, TextModel, Txn};
use super::exo::{self, Exo};
use super::keymap;
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
    /// An event from host object `sender` reached the proxy; `late` if it
    /// carries an engine answer made before a reset that came after its key
    /// (E5's outcome (c), P3).
    Host {
        sender: u32,
        event: &'a AnyEvent,
        late: bool,
    },
    /// The proxy sent the host a request.
    ToHost(&'a AnyRequest),
    /// The proxy sent the guest an event.
    ToGuest(&'a AnyEvent),
}

/// Which oracle a check belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tier {
    /// Tier 1: the specification's text, exactly.
    Exact,
    /// Tier 2: the user's intent.
    Intent,
}

/// A failed check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub tier: Tier,
    /// What was checked: `"text"`, `"composition"`, `"field state"` or
    /// `"focus"`.
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
            surrounding_text: self.surrounding_text.clone().unwrap_or_default(),
            cursor_rectangle: self.cursor_rectangle.unwrap_or_default(),
        }
    }
}

/// The v3 state a guest has sent since its last commit (G3 to G8).
#[derive(Clone, Debug, Default)]
struct Pending {
    enabled: Option<bool>,
    surrounding_text: Option<Option<(String, u32, u32)>>,
    content_type: Option<(u32, u32)>,
    cursor_rectangle: Option<Option<(i32, i32, i32, i32)>>,
}

/// The reset fence (F4, W2): open from the proxy's `reset` until the
/// `done` of the sync it sends right after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fence {
    Closed,
    /// The `reset` went out; its sync is next.
    Reset,
    /// The sync with this callback went out.
    Open(u32),
}

/// What the tier-1 oracle counts in a case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    /// Presses that ended a composition (F4).
    pub press_commits: u64,
    /// Host composition events the fence dropped.
    pub fence_drops: u64,
    /// Late engine answers the guest got (P3).
    pub late_applied: u64,
    /// Keys that reached the guest while no text input was attached (E7).
    pub raw_keys: u64,
}

/// What the specification says an ideal client shows, from what reached
/// the proxy and the proxy's own activation decisions. The case has one
/// guest text input on one seat, which follows the keyboard focus.
pub(super) struct SpecText {
    /// Each field's host surface.
    surfaces: Vec<u32>,
    fields: Vec<TextModel>,
    /// The field the keyboard focuses, which the text input follows (F1).
    focus: Option<usize>,
    /// Shift is held.
    shift: bool,
    /// The surface the pointer is on, and where.
    pointer: Option<(u32, i32)>,
    /// The committed state, if the field is enabled.
    committed: Option<V3Field>,
    pending: Pending,
    /// The proxy activated the host text input, and neither it nor Exo
    /// detached it since.
    attached: bool,
    fence: Fence,
    /// The preedit the guest shows, as the proxy tracks it.
    shown: Option<String>,
    /// The host event being read carries a late answer.
    late: bool,
    /// The shown preedit came from a late answer, which the engine made
    /// before a reset that dropped it since.
    shown_late: bool,
    pub counts: Counts,
}

impl SpecText {
    /// Fields holding `texts`, on host surfaces `surfaces`.
    pub fn new(surfaces: &[u32], texts: &[String]) -> Self {
        Self {
            surfaces: surfaces.to_vec(),
            fields: texts.iter().map(|text| TextModel::new(text)).collect(),
            focus: None,
            shift: false,
            pointer: None,
            committed: None,
            pending: Pending::default(),
            attached: false,
            fence: Fence::Closed,
            shown: None,
            late: false,
            shown_late: false,
            counts: Counts::default(),
        }
    }

    /// Follows one message at the proxy.
    pub fn see(&mut self, seen: Seen<'_>) {
        match seen {
            Seen::Guest(request) => self.guest(request),
            Seen::Host {
                sender,
                event,
                late,
            } => {
                self.late = late;
                self.host(sender, event);
            }
            Seen::ToHost(request) => self.decision(request),
            Seen::ToGuest(event) => self.key(event),
        }
    }

    /// The gate: host composition events reach the guest only while the
    /// text input is attached and no fence is open.
    fn gate(&self) -> bool {
        self.attached && self.fence == Fence::Closed
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
        let was_enabled = self.committed.is_some();
        let enabled = enabled.unwrap_or(was_enabled);
        if enabled != was_enabled {
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
        let preedit = self.shown.clone().unwrap_or_default();
        self.apply(Txn {
            preedit,
            ..Txn::default()
        });
    }

    /// What reached the proxy from the host.
    fn host(&mut self, sender: u32, event: &AnyEvent) {
        match event {
            AnyEvent::WlKeyboard(event) => self.keyboard(event),
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
                let (surface, x) = self.pointer.expect("a press on a surface");
                self.press(surface, x);
            }
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, x, .. }) => self.press(*surface, *x),
            AnyEvent::WlCallback(wl_callback::Event::Done { .. }) => {
                if self.fence == Fence::Open(sender) {
                    self.fence = Fence::Closed;
                }
            }
            AnyEvent::ZwpTextInputV1(event) => self.text_input(event),
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::ConfirmPreedit { .. }) => {
                // X2: confirm the shown preedit; X2' recovers a held key, of
                // which a case has none.
                let commit = self.shown.clone();
                self.composition_event(|_| {
                    commit.map(|commit| Txn {
                        commit,
                        ..Txn::default()
                    })
                });
            }
            AnyEvent::ZcrExtendedTextInputV1(extended::Event::SetPreeditRegion { .. }) => {
                panic!("preedit regions come with D6")
            }
            _ => {}
        }
    }

    /// The keyboard: its focus, which resets the text input (F1), and
    /// Shift.
    fn keyboard(&mut self, event: &wl_keyboard::Event) {
        match *event {
            wl_keyboard::Event::Enter { surface, .. } => self.move_focus(Some(self.field(surface))),
            wl_keyboard::Event::Leave { .. } => self.move_focus(None),
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                self.shift = mods_depressed & keymap::SHIFT != 0;
            }
            _ => {}
        }
    }

    /// A key the proxy passed the guest, which an ideal client types. Which
    /// keys reach the guest is the keyboard concern's, not the IME's: it
    /// passes a press of a held key, for one, only once.
    fn key(&mut self, event: &AnyEvent) {
        let AnyEvent::WlKeyboard(wl_keyboard::Event::Key { key, state, .. }) = *event else {
            return;
        };
        let typed = keymap::key(key, self.shift);
        if let (true, Some(field), Some(key)) =
            (state == wl_keyboard::KEY_STATE_PRESSED, self.focus, typed)
        {
            self.counts.raw_keys += u64::from(!self.attached);
            self.fields[field].insert_key(key);
        }
    }

    /// F1: a focus change resets the text input. The field it leaves ends
    /// its composition, which the guest drops at the text input's `leave`.
    fn move_focus(&mut self, focus: Option<usize>) {
        if focus == self.focus {
            return;
        }
        if let Some(left) = self.focus {
            self.fields[left].preedit.clear();
        }
        self.focus = focus;
        self.committed = None;
        self.pending = Pending::default();
        self.shown = None;
    }

    /// F4: a press on the surface of the enabled, focused field while the
    /// gate is open and the guest shows a preedit commits it; the press
    /// then moves the cursor of the field it lands in.
    fn press(&mut self, surface: u32, x: i32) {
        let field = self.field(surface);
        let enabled = self.committed.is_some() && self.focus == Some(field);
        if enabled && self.gate() {
            if let Some(commit) = self.shown.take() {
                self.counts.press_commits += 1;
                self.apply(Txn {
                    commit,
                    ..Txn::default()
                });
            }
        }
        let at = keymap::target(x).expect("a press at a target");
        self.fields[field].click(at);
    }

    /// H2, H7 and H8.
    fn text_input(&mut self, event: &v1::Event) {
        match event {
            v1::Event::Leave {} if self.attached => {
                // H2: Exo detached on its own.
                if self.shown.take().is_some() {
                    self.apply(Txn::default());
                }
                self.attached = false;
            }
            v1::Event::PreeditString { text, commit, .. } => {
                let text = match text {
                    IndexedString::Utf8(text) => Some(text.clone()),
                    IndexedString::NotUtf8(_) => None,
                };
                let shown = self.shown.is_some();
                self.composition_event(|spec| {
                    let text = text?;
                    if text.is_empty() {
                        spec.shown = None;
                        let commit = if shown { commit.clone() } else { String::new() };
                        return Some(Txn {
                            commit,
                            ..Txn::default()
                        });
                    }
                    spec.shown = Some(text.clone());
                    Some(Txn {
                        preedit: text,
                        ..Txn::default()
                    })
                });
            }
            v1::Event::CommitString { text, .. } => {
                let commit = text.clone();
                self.composition_event(|_| {
                    Some(Txn {
                        commit,
                        ..Txn::default()
                    })
                });
            }
            v1::Event::DeleteSurroundingText { .. } => panic!("deletes come with D6"),
            _ => {}
        }
    }

    /// A host composition event: through the gate, `txn` decides what the guest gets (none if it drops the
    /// event), which ends the shown preedit unless `txn` set it. Without
    /// the gate it is dropped.
    fn composition_event(&mut self, txn: impl FnOnce(&mut Self) -> Option<Txn>) {
        if !self.gate() {
            self.counts.fence_drops += u64::from(self.attached);
            return;
        }
        if let Some(txn) = txn(self) {
            if txn.preedit.is_empty() {
                self.shown = None;
            }
            self.counts.late_applied += u64::from(self.late);
            self.shown_late = self.late && self.shown.is_some();
            self.apply(txn);
        }
    }

    /// The proxy's own activation decisions, and its fence.
    fn decision(&mut self, request: &AnyRequest) {
        match request {
            AnyRequest::ZwpTextInputV1(v1::Request::Activate { .. }) => self.attached = true,
            AnyRequest::ZwpTextInputV1(v1::Request::Deactivate { .. }) => {
                self.attached = false;
                self.shown = None;
            }
            AnyRequest::ZwpTextInputV1(v1::Request::Reset {}) => self.fence = Fence::Reset,
            &AnyRequest::WlDisplay(wl_display::Request::Sync { callback })
                if self.fence == Fence::Reset =>
            {
                self.fence = Fence::Open(callback);
            }
            _ => {}
        }
    }

    /// The guest applies `txn` to the focused field.
    fn apply(&mut self, txn: Txn) {
        let field = self.focus.expect("a transaction for the focused field");
        self.fields[field].apply_txn(&txn);
    }

    fn field(&self, surface: u32) -> usize {
        let field = self.surfaces.iter().position(|&s| s == surface);
        field.unwrap_or_else(|| panic!("host surface {surface} has no field"))
    }
}

/// An ideal client fed what the proxy sent the guest: it applies each
/// transaction at `done` to the entered field, drops the preedit at
/// `leave`, and takes keys and presses as [`TextModel`] does.
pub(super) struct Shadow {
    /// Each field's guest surface.
    surfaces: Vec<u32>,
    fields: Vec<TextModel>,
    /// The field the text input entered.
    entered: Option<usize>,
    /// The field the keyboard focuses.
    keyboard: Option<usize>,
    shift: bool,
    pointer: Option<(u32, i32)>,
    /// The transaction in progress.
    txn: Txn,
}

impl Shadow {
    /// Fields holding `texts`, on guest surfaces `surfaces`.
    pub fn new(surfaces: &[u32], texts: &[String]) -> Self {
        Self {
            surfaces: surfaces.to_vec(),
            fields: texts.iter().map(|text| TextModel::new(text)).collect(),
            entered: None,
            keyboard: None,
            shift: false,
            pointer: None,
            txn: Txn::default(),
        }
    }

    /// Follows one message at the proxy: what it sent the guest.
    pub fn see(&mut self, seen: Seen<'_>) {
        let Seen::ToGuest(event) = seen else {
            return;
        };
        match event {
            AnyEvent::ZwpTextInputV3(event) => self.text_input(event),
            AnyEvent::WlKeyboard(event) => match *event {
                wl_keyboard::Event::Enter { surface, .. } => {
                    self.keyboard = Some(self.field(surface))
                }
                wl_keyboard::Event::Leave { .. } => self.keyboard = None,
                wl_keyboard::Event::Modifiers { mods_depressed, .. } => {
                    self.shift = mods_depressed & keymap::SHIFT != 0;
                }
                wl_keyboard::Event::Key { key, state, .. }
                    if state == wl_keyboard::KEY_STATE_PRESSED =>
                {
                    if let (Some(field), Some(key)) = (self.keyboard, keymap::key(key, self.shift))
                    {
                        self.fields[field].insert_key(key);
                    }
                }
                _ => {}
            },
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
                let (surface, x) = self.pointer.expect("a press on a surface");
                self.press(surface, x);
            }
            AnyEvent::WlTouch(wl_touch::Event::Down { surface, x, .. }) => self.press(*surface, *x),
            _ => {}
        }
    }

    fn text_input(&mut self, event: &v3::Event) {
        match event {
            &v3::Event::Enter { surface } => self.entered = Some(self.field(surface)),
            v3::Event::Leave { .. } => {
                if let Some(field) = self.entered.take() {
                    self.fields[field].preedit.clear();
                }
                self.txn = Txn::default();
            }
            v3::Event::PreeditString { text, .. } => {
                self.txn.preedit = match text {
                    Some(IndexedString::Utf8(text)) => text.clone(),
                    _ => String::new(),
                };
            }
            v3::Event::CommitString { text } => self.txn.commit = text.clone().unwrap_or_default(),
            &v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => self.txn.delete = (before_length as usize, after_length as usize),
            v3::Event::Done { .. } => {
                let txn = std::mem::take(&mut self.txn);
                let field = &mut self.fields[self.entered.expect("a done while entered")];
                let (before, after) = txn.delete;
                assert!(
                    field.delete_range(before, after).is_some(),
                    "the proxy sent a delete of ({before}, {after}) that does not fit {field:?}"
                );
                field.apply_txn(&txn);
            }
        }
    }

    fn press(&mut self, surface: u32, x: i32) {
        let field = self.field(surface);
        let at = keymap::target(x).expect("a press at a target");
        self.fields[field].click(at);
    }

    fn field(&self, surface: u32) -> usize {
        let field = self.surfaces.iter().position(|&s| s == surface);
        field.unwrap_or_else(|| panic!("guest surface {surface} has no field"))
    }
}

/// Tier 1 at quiescence: `shadow` against `spec` per field; the guest's
/// preedit against what Exo composes; and Exo's field on the focused
/// surface against what the guest last committed, for a host whose
/// text-input extension is `extension`.
///
/// Exo composes in two places that can disagree for a while, so the
/// preedit must match one of them: the engine's composition (`engine`),
/// and the one Exo tracks in the field's text. A key the engine answered
/// after a reset sends a composition the engine itself has since dropped
/// (E5), and a report Exo cannot match to its prediction drops the one Exo
/// tracks (E4). A preedit from such a late answer (P3) may match neither,
/// and is not checked. A proxy that did not reset Exo where it ended the
/// guest's composition, or that showed the guest a composition Exo never
/// sent, fails.
pub(super) fn exact(
    spec: &SpecText,
    shadow: &Shadow,
    exo: &Exo,
    engine: &str,
    extension: Option<u32>,
) -> Vec<Failure> {
    let mut failures = Vec::new();
    let failure = |check, field, detail| Failure {
        tier: Tier::Exact,
        check,
        field,
        detail,
    };
    for (i, (spec, shadow)) in spec.fields.iter().zip(&shadow.fields).enumerate() {
        if spec != shadow {
            let detail = format!("spec:   {}\nshadow: {}", show(spec), show(shadow));
            failures.push(failure("text", Some(i), detail));
        }
    }
    let late = spec.shown_late && spec.shown.is_some();
    let composition = exo.composition().filter(|_| !late);
    if let (Some(field), Some(tracked)) = (spec.focus, composition) {
        let preedit = &spec.fields[field].preedit;
        if *preedit != tracked && preedit != engine {
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

/// Tier 2: each of `client`'s fields against `reference`'s, after NFC
/// normalisation, and the focus.
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
        if nfc_model(intended) != nfc_model(got) {
            let detail = format!(
                "intended: {}\nclient:   {}\nclient drops: {:?}",
                show(intended),
                show(got),
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
    /// P1: a focus change while the client shows a preedit loses the
    /// syllable, or keeps it, depending on the client.
    FocusWhileComposing,
}

impl KnownIssue {
    /// Why the proxy fails the user here.
    pub fn reason(self) -> &'static str {
        match self {
            Self::FocusWhileComposing => {
                "the keyboard's leave reaches the proxy before Exo's confirmation (E12), so the \
                 field reset clears the composition without telling the guest, and the drain \
                 drops the confirmation"
            }
        }
    }

    /// Where its fix is tracked.
    pub fn tracking(self) -> &'static str {
        match self {
            Self::FocusWhileComposing => "docs/IME_REDESIGN.md 1.3 P1; fixed by C5",
        }
    }
}

/// The known issue that explains a case's `failures`, if one does: tier 1
/// passed, and tier 2 failed only on the text of fields that a focus move,
/// click or tap left while their client showed a preedit (`left`).
pub(super) fn classify(failures: &[Failure], left: &[bool]) -> Option<KnownIssue> {
    let known = |failure: &Failure| {
        failure.tier == Tier::Intent
            && failure.check == "text"
            && failure.field.is_some_and(|field| left[field])
    };
    (!failures.is_empty() && failures.iter().all(known)).then_some(KnownIssue::FocusWhileComposing)
}

/// A field as `"text with | at the cursor" preedit "…"`.
fn show(model: &TextModel) -> String {
    let (before, after) = model.text.split_at(model.cursor);
    format!(
        "{:?} preedit {:?}",
        format!("{before}|{after}"),
        model.preedit
    )
}

/// `model` with its text and preedit in NFC, and its cursor after the
/// normalised text before it.
fn nfc_model(model: &TextModel) -> TextModel {
    let text = nfc(&model.text);
    let cursor = nfc(&model.text[..model.cursor]).len();
    TextModel {
        text,
        cursor,
        preedit: nfc(&model.preedit),
    }
}

/// `text` in NFC, as far as the simulation types it: conjoining Hangul
/// jamo compose into syllables; nothing else it types decomposes.
fn nfc(text: &str) -> String {
    let mut out: Vec<char> = Vec::new();
    for c in text.chars() {
        match out.last().and_then(|&last| compose(last, c)) {
            Some(syllable) => *out.last_mut().expect("a last character") = syllable,
            None => out.push(c),
        }
    }
    out.into_iter().collect()
}

/// The syllable that `first` then `second` compose into: a leading
/// consonant and a vowel, or a syllable without a trailing consonant and
/// one (Unicode's canonical composition of Hangul).
fn compose(first: char, second: char) -> Option<char> {
    let (first, second) = (u32::from(first), u32::from(second));
    let syllable = if (0x1100..0x1113).contains(&first) && (0x1161..0x1176).contains(&second) {
        0xAC00 + ((first - 0x1100) * 21 + (second - 0x1161)) * 28
    } else if (0xAC00..0xD7A4).contains(&first)
        && (first - 0xAC00) % 28 == 0
        && (0x11A8..0x11C3).contains(&second)
    {
        first + second - 0x11A7
    } else {
        return None;
    };
    char::from_u32(syllable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_callback;
    use crate::sim::client::{gtk::Gtk, Field};
    use crate::sim::clock::Micros;
    use crate::sim::editor::{Target, UserAction};
    use crate::sim::hangul::Key;

    /// The host surfaces of field 0 and field 1, and their guest ones.
    const HOST: [u32; 2] = [20, 21];
    const GUEST: [u32; 2] = [30, 31];
    const CALLBACK: u32 = 40;

    /// The spec of fields holding `texts`, the keyboard on field 0, with
    /// the guest's field enabled and the text input attached.
    fn attached(texts: &[&str]) -> SpecText {
        let texts: Vec<String> = texts.iter().map(|text| text.to_string()).collect();
        let mut spec = SpecText::new(&HOST, &texts);
        host(
            &mut spec,
            wl_keyboard::Event::Enter {
                serial: 1,
                surface: HOST[0],
                keys: Vec::new(),
            },
        );
        guest(&mut spec, &[v3::Request::Enable {}, v3::Request::Commit {}]);
        decide(
            &mut spec,
            v1::Request::Activate {
                seat: 1,
                surface: HOST[0],
            },
        );
        spec
    }

    fn host(spec: &mut SpecText, event: impl Into<AnyEvent>) {
        let event = event.into();
        spec.see(Seen::Host {
            sender: CALLBACK,
            event: &event,
            late: false,
        });
    }

    fn guest(spec: &mut SpecText, requests: &[v3::Request]) {
        requests
            .iter()
            .for_each(|request| spec.see(Seen::Guest(request)));
    }

    fn decide(spec: &mut SpecText, request: impl Into<AnyRequest>) {
        spec.see(Seen::ToHost(&request.into()));
    }

    fn preedit(text: &str) -> v1::Event {
        v1::Event::PreeditString {
            serial: 0,
            text: IndexedString::Utf8(text.to_owned()),
            commit: text.to_owned(),
        }
    }

    fn commit(text: &str) -> v1::Event {
        v1::Event::CommitString {
            serial: 0,
            text: text.to_owned(),
        }
    }

    fn press(spec: &mut SpecText, field: usize, at: Target) {
        let surface = HOST[field];
        let x = keymap::position(at);
        host(
            spec,
            wl_touch::Event::Down {
                serial: 0,
                time: 0,
                surface,
                id: 0,
                x,
                y: 0,
            },
        );
    }

    fn model(text: &str, cursor: usize, preedit: &str) -> TextModel {
        TextModel {
            text: text.to_owned(),
            cursor,
            preedit: preedit.to_owned(),
        }
    }

    #[test]
    fn composition_events_reach_the_guest_only_through_the_gate() {
        let texts = ["0".to_owned()];
        let mut spec = SpecText::new(&HOST[..1], &texts);
        host(
            &mut spec,
            wl_keyboard::Event::Enter {
                serial: 1,
                surface: HOST[0],
                keys: Vec::new(),
            },
        );
        // Not attached: dropped, but not by a fence.
        host(&mut spec, preedit("가"));
        assert_eq!(spec.fields, [model("0", 1, "")]);
        // H7, then H8 ends the preedit.
        let mut spec = attached(&["0"]);
        host(&mut spec, preedit("가"));
        assert_eq!(spec.fields[0], model("0", 1, "가"));
        host(&mut spec, commit("각"));
        assert_eq!(spec.fields[0], model("0각", 4, ""));
        // X2 commits the shown preedit; X2' with none changes nothing.
        host(&mut spec, preedit("ㄴ"));
        let confirm = || extended::Event::ConfirmPreedit {
            selection_behavior: 1,
        };
        host(&mut spec, confirm());
        host(&mut spec, confirm());
        assert_eq!(spec.fields[0], model("0각ㄴ", 7, ""));
        assert_eq!(spec.counts.fence_drops, 0);
    }

    #[test]
    fn a_press_commits_the_preedit_and_fences_until_its_sync_returns() {
        let mut spec = attached(&["0a", "1"]);
        host(&mut spec, preedit("가"));
        // F4 commits, then the press moves the cursor.
        press(&mut spec, 0, Target::Start);
        assert_eq!(spec.fields[0], model("0a가", 0, ""));
        assert_eq!(spec.counts.press_commits, 1);
        decide(&mut spec, v1::Request::Reset {});
        decide(&mut spec, wl_display::Request::Sync { callback: CALLBACK });
        // W2: the fence drops what Exo sent before the reset.
        host(&mut spec, preedit("각"));
        assert_eq!(spec.fields[0], model("0a가", 0, ""));
        assert_eq!(spec.counts.fence_drops, 1);
        host(&mut spec, wl_callback::Event::Done { callback_data: 0 });
        host(&mut spec, preedit("ㄴ"));
        assert_eq!(spec.fields[0], model("0a가", 0, "ㄴ"));
        // A press on the other field commits nothing, and only moves its
        // cursor.
        press(&mut spec, 1, Target::Start);
        assert_eq!(spec.fields[0].preedit, "ㄴ");
        assert_eq!(spec.fields[1], model("1", 0, ""));
    }

    #[test]
    fn exo_or_the_proxy_detaching_or_a_focus_change_drops_the_preedit() {
        // H2: Exo's own detach clears the preedit, committing nothing.
        let mut spec = attached(&["0", "1"]);
        host(&mut spec, preedit("가"));
        host(&mut spec, v1::Event::Leave {});
        assert_eq!(spec.fields[0], model("0", 1, ""));
        // F1: the keyboard leaves; the field resets, and the guest drops the
        // preedit at the text input's leave.
        let mut spec = attached(&["0", "1"]);
        host(&mut spec, preedit("가"));
        host(
            &mut spec,
            wl_keyboard::Event::Leave {
                serial: 2,
                surface: HOST[0],
            },
        );
        assert_eq!(spec.fields[0], model("0", 1, ""));
        assert_eq!(spec.committed, None);
        // The proxy's deactivate closes the gate.
        let mut spec = attached(&["0"]);
        decide(&mut spec, v1::Request::Deactivate { seat: 1 });
        host(&mut spec, preedit("가"));
        assert_eq!(spec.fields[0], model("0", 1, ""));
    }

    #[test]
    fn a_guest_commit_is_answered_with_the_shown_preedit_and_records_the_field() {
        let mut spec = attached(&["0"]);
        host(&mut spec, preedit("가"));
        let surrounding = v3::Request::SetSurroundingText {
            text: IndexedString::Utf8("0".to_owned()),
            cursor: 1,
            anchor: 1,
        };
        guest(&mut spec, &[surrounding, v3::Request::Commit {}]);
        assert_eq!(spec.fields[0], model("0", 1, "가"));
        let field = V3Field {
            surrounding_text: Some(("0".to_owned(), 1, 1)),
            ..V3Field::default()
        };
        assert_eq!(spec.committed, Some(field));
        // A disable is a new field, which forgets the shown preedit; the
        // guest keeps showing it until the answer to the next enable, a
        // `done` with no preedit, clears it.
        guest(
            &mut spec,
            &[v3::Request::Disable {}, v3::Request::Commit {}],
        );
        assert_eq!((&spec.committed, &spec.shown), (&None, &None));
        assert_eq!(spec.fields[0], model("0", 1, "가"));
        guest(&mut spec, &[v3::Request::Enable {}, v3::Request::Commit {}]);
        assert_eq!(spec.fields[0], model("0", 1, ""));
    }

    #[test]
    fn the_guest_types_the_keys_the_proxy_passes() {
        let mut spec = attached(&["0"]);
        let key = |state| wl_keyboard::Event::Key {
            serial: 0,
            time: 0,
            key: keymap::keycode(Key::Char('a')).0,
            state,
        };
        // Only a press types, and only one the proxy passed.
        host(&mut spec, key(wl_keyboard::KEY_STATE_PRESSED));
        let pressed = AnyEvent::from(key(wl_keyboard::KEY_STATE_PRESSED));
        let released = AnyEvent::from(key(wl_keyboard::KEY_STATE_RELEASED));
        spec.see(Seen::ToGuest(&pressed));
        spec.see(Seen::ToGuest(&released));
        assert_eq!(spec.fields[0], model("0a", 2, ""));
        assert_eq!(spec.counts.raw_keys, 0);
    }

    #[test]
    fn the_shadow_applies_what_the_guest_got() {
        let texts = ["0".to_owned(), "1".to_owned()];
        let mut shadow = Shadow::new(&GUEST, &texts);
        let events: Vec<AnyEvent> = vec![
            v3::Event::Enter { surface: GUEST[0] }.into(),
            v3::Event::PreeditString {
                text: Some(IndexedString::Utf8("가".to_owned())),
                cursor_begin: 3,
                cursor_end: 3,
            }
            .into(),
            v3::Event::CommitString {
                text: Some("a".to_owned()),
            }
            .into(),
        ];
        for event in &events {
            shadow.see(Seen::ToGuest(event));
        }
        // Nothing applies before `done`.
        assert_eq!(shadow.fields[0], model("0", 1, ""));
        shadow.see(Seen::ToGuest(&v3::Event::Done { serial: 0 }.into()));
        assert_eq!(shadow.fields[0], model("0a", 2, "가"));
        // `leave` drops the preedit.
        let leave = v3::Event::Leave { surface: GUEST[0] }.into();
        shadow.see(Seen::ToGuest(&leave));
        assert_eq!(shadow.fields[0], model("0a", 2, ""));
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn the_shadow_refuses_a_delete_that_does_not_fit() {
        let mut shadow = Shadow::new(&GUEST[..1], &["가".to_owned()]);
        let events: Vec<AnyEvent> = vec![
            v3::Event::Enter { surface: GUEST[0] }.into(),
            v3::Event::DeleteSurroundingText {
                before_length: 1,
                after_length: 0,
            }
            .into(),
            v3::Event::Done { serial: 0 }.into(),
        ];
        events
            .iter()
            .for_each(|event| shadow.see(Seen::ToGuest(event)));
    }

    #[test]
    fn exos_composition_must_be_the_engines_or_the_one_it_tracks() {
        use crate::protocols::text_input_unstable_v1::zwp_text_input_manager_v1 as manager;
        // Exo attached a text input on field 0, and tracks no composition.
        let mut exo = Exo::default();
        exo.set_text_input_focus(Some(HOST[0]));
        let create = manager::Request::CreateTextInput { id: 10 };
        exo.request(3, &create.into());
        let activate = v1::Request::Activate {
            seat: 1,
            surface: HOST[0],
        };
        exo.request(10, &activate.into());
        let checks = |spec: &SpecText, engine: &str| -> Vec<&'static str> {
            let shadow = Shadow {
                fields: spec.fields.clone(),
                ..Shadow::new(&GUEST[..1], &[])
            };
            let failures = exact(spec, &shadow, &exo, engine, None);
            failures.iter().map(|failure| failure.check).collect()
        };
        let mut spec = attached(&["0"]);
        // The guest shows nothing, which Exo tracks.
        assert_eq!(checks(&spec, "가"), ["field state"]);
        // The guest shows what the engine composes.
        host(&mut spec, preedit("가"));
        assert_eq!(checks(&spec, "가"), ["field state"]);
        // Neither composes it.
        assert_eq!(checks(&spec, "각"), ["composition", "field state"]);
        // A late answer's preedit is not checked.
        spec.see(Seen::Host {
            sender: 10,
            event: &preedit("각").into(),
            late: true,
        });
        assert_eq!(checks(&spec, ""), ["field state"]);
    }

    #[test]
    fn nfc_composes_conjoining_hangul_only() {
        assert_eq!(nfc("\u{1100}\u{1161}"), "가");
        assert_eq!(nfc("\u{1100}\u{1161}\u{11A8}"), "각");
        assert_eq!(nfc("가\u{11A8}"), "각");
        assert_eq!(nfc("각\u{11A8}"), "각\u{11A8}");
        assert_eq!(nfc("a\u{1100}b"), "a\u{1100}b");
        let model = nfc_model(&model("\u{1100}\u{1161}b", 6, "\u{1100}"));
        assert_eq!(
            model,
            TextModel {
                text: "가b".to_owned(),
                cursor: 3,
                preedit: "\u{1100}".to_owned(),
            }
        );
    }

    #[test]
    fn the_intent_is_compared_after_nfc_with_the_focus() {
        let fields = |texts: [&str; 2]| -> Vec<Field> {
            texts
                .iter()
                .zip(GUEST)
                .map(|(text, surface)| Field {
                    surface,
                    model: TextModel::new(text),
                })
                .collect()
        };
        let mut gtk = Gtk::new(true, 10, fields(["\u{1100}\u{1161}", "1"]));
        gtk.keyboard_focus(Micros(0), Some(GUEST[1]));
        let mut reference = Reference::new(vec![TextModel::new("가"), TextModel::new("1")]);
        reference.apply(UserAction::Focus(1));
        assert_eq!(intended(&reference, &gtk), []);
        reference.apply(UserAction::Key(Key::Char('a')));
        reference.apply(UserAction::Focus(0));
        let failures = intended(&reference, &gtk);
        let checks: Vec<_> = failures.iter().map(|f| (f.check, f.field)).collect();
        assert_eq!(checks, [("text", Some(1)), ("focus", None)]);
    }

    #[test]
    fn only_lost_text_in_a_field_left_while_composing_is_known() {
        let failure = |tier, check, field| Failure {
            tier,
            check,
            field,
            detail: String::new(),
        };
        let lost = failure(Tier::Intent, "text", Some(0));
        let known = Some(KnownIssue::FocusWhileComposing);
        assert_eq!(classify(std::slice::from_ref(&lost), &[true, false]), known);
        assert_eq!(classify(std::slice::from_ref(&lost), &[false, true]), None);
        assert_eq!(classify(&[], &[true, true]), None);
        let exact = failure(Tier::Exact, "text", Some(0));
        assert_eq!(classify(&[lost.clone(), exact], &[true, true]), None);
        let focus = failure(Tier::Intent, "focus", None);
        assert_eq!(classify(&[lost, focus], &[true, true]), None);
        let issue = KnownIssue::FocusWhileComposing;
        assert!(!issue.reason().is_empty() && issue.tracking().contains("C5"));
    }
}
