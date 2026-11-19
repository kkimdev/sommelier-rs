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

//! A model of Chromium's `zwp_text_input_v3` client: facts C1 to C5.
//!
//! Sources, at Chromium `main@ffb4f13519`, under `ui/` unless they start
//! with `content/`:
//!
//! - `ZwpTextInputV3Impl` (`ozone/platform/wayland/host/zwp_text_input_v3.cc`):
//!   the commit count, the state it holds and sends, and the transaction
//!   at `done`;
//! - `WaylandInputMethodContext`
//!   (`ozone/platform/wayland/host/wayland_input_method_context.cc`):
//!   activation and the text it predicts (`SurroundingTextTracker`, in
//!   `base/ime/surrounding_text_tracker.cc`);
//! - `InputMethodAuraLinux` (`base/ime/linux/input_method_auralinux.cc`):
//!   a window's input method, which drops what its window cannot take,
//!   resets, and keeps the 100 ms guard;
//! - `RenderWidgetHostViewAura` and `RenderWidgetHostViewEventHandler`
//!   (`content/browser/renderer_host/`): blur, clicks and taps.
//!
//! Chromium has one text input per connection
//! (`WaylandConnection::EnsureTextInputV3`), shared by its windows, and an
//! input method per window. Here each field is a window. A window is
//! focused while it has the keyboard focus, which makes it active
//! (`WaylandToplevelWindow::UpdateActivationState`) and gives its input
//! method a text input client. The text input is enabled for a window
//! while it is focused and entered (`MaybeUpdateActivated`).
//!
//! The text the context predicts starts from what the window last
//! reported; the tracker's replay of the edits a report has not caught up
//! with is not modelled, as a report here always catches up at once.

use std::mem;

use super::{
    caret_rect, chars_around, confirm, content_type, cursor_rectangle, delete_chars, field_on,
    utf8, Client, DropReason, Dropped, Field, Part, PressKind, Protocol,
};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{req, Event};
use crate::sim::clock::Micros;
use crate::sim::editor::{Target, TextModel};
use crate::sim::hangul::Key;
use crate::wire::Outgoing;

/// How long a reset that ends a composition makes a window's input method
/// ignore the engine's commits and preedits (`kIgnoreCommitsDuration`).
const GUARD: Micros = Micros::from_millis(100);

/// A text and cursor, as `set_surrounding_text` sends them.
type Surrounding = (String, usize);

/// Some of a field's state (`ZwpTextInputV3Impl::ImeData`). The content
/// type is sent only by `enable`, so it is not tracked.
#[derive(Clone, Debug, Default, PartialEq)]
struct ImeData {
    surrounding: Option<Surrounding>,
    rect: Option<[i32; 4]>,
}

/// A preedit as Chromium compares two: text and cursor (`PreeditData`).
type Preedit = (String, i32, i32);

/// The events of the transaction in progress (`InputEvents`). No
/// `preedit_string` means an empty preedit.
#[derive(Clone, Debug, Default)]
struct Events {
    preedit: Preedit,
    commit: String,
    delete: Option<(u32, u32)>,
}

impl Events {
    /// The parts that would change something.
    fn parts(self) -> Vec<Part> {
        let mut parts = Vec::new();
        if let Some((before, after)) = self.delete {
            parts.push(Part::Delete { before, after });
        }
        if !self.commit.is_empty() {
            parts.push(Part::Commit(self.commit));
        }
        if self.preedit != Preedit::default() {
            parts.push(Part::Preedit(self.preedit.0));
        }
        parts
    }
}

/// What Chromium keeps for a window besides its field.
#[derive(Clone, Debug, Default)]
struct Window {
    /// It has the keyboard focus, so its input method has a client.
    focused: bool,
    /// The text input is enabled for it (`activated_`).
    activated: bool,
    /// The text its input method context predicts.
    predicted: TextModel,
    /// What its input method last reported of the field
    /// (`surrounding_text_`, `selection_range_`); `None` after a focus
    /// change.
    reported: Option<TextModel>,
    /// Until when its input method ignores commits and preedits.
    guard_until: Micros,
}

/// Chromium's v3 client.
#[derive(Debug)]
pub(in crate::sim) struct Chromium {
    protocol: Protocol,
    fields: Vec<Field>,
    windows: Vec<Window>,
    /// The field with the keyboard focus.
    keyboard: Option<usize>,
    /// The window the text input last enabled for (`client_`).
    client: Option<usize>,
    /// The commits it made (`commit_count`).
    commits: u32,
    /// The serial of the last `done` (`last_done_serial`).
    last_done: u32,
    /// What it sent since it last reset (`committed_ime_data_`).
    committed: ImeData,
    /// What it holds until a `done` names its last commit.
    pending: ImeData,
    /// What it was last given, which its next `enable` sends
    /// (`latest_surrounding_text_`, `latest_cursor_rect_`).
    latest: ImeData,
    /// The transaction in progress.
    events: Events,
    /// The preedit the last transaction applied.
    applied_preedit: Preedit,
    /// A touch waiting to lift: its field and target.
    touch: Option<(usize, Target)>,
    drops: Vec<Dropped>,
}

impl Chromium {
    /// An application with `fields`, its text input `text_input`.
    pub fn new(text_input: u32, fields: Vec<Field>) -> Self {
        let windows = fields
            .iter()
            .map(|field| Window {
                predicted: field.model.clone(),
                ..Window::default()
            })
            .collect();
        Self {
            protocol: Protocol::new(text_input),
            fields,
            windows,
            keyboard: None,
            client: None,
            commits: 0,
            last_done: 0,
            committed: ImeData::default(),
            pending: ImeData::default(),
            latest: ImeData::default(),
            events: Events::default(),
            applied_preedit: Preedit::default(),
            touch: None,
            drops: Vec::new(),
        }
    }

    fn text_input(&self) -> u32 {
        self.protocol.text_input
    }

    fn drop(&mut self, at: Micros, part: Part, reason: DropReason) {
        self.drops.push(Dropped { at, part, reason });
    }

    /// Drops the transaction in progress for `reason`.
    fn discard(&mut self, now: Micros, reason: DropReason) {
        for part in mem::take(&mut self.events).parts() {
            self.drop(now, part, reason);
        }
    }

    /// `commit`, counted (`ZwpTextInputV3Impl::Commit`).
    fn commit(&mut self) -> Outgoing {
        self.commits = self.commits.wrapping_add(1);
        req::commit(self.text_input())
    }

    /// The text input forgets what it sent, held and applied
    /// (`ZwpTextInputV3Impl::Reset`).
    fn reset_text_input(&mut self, now: Micros) {
        self.committed = ImeData::default();
        self.pending = ImeData::default();
        self.applied_preedit = Preedit::default();
        self.discard(now, DropReason::DiscardedByReset);
    }

    /// C2: `enable` is not gated. It forgets what was sent, held and
    /// applied, then sends the content type, the text and caret it was
    /// last given, if any, and a `commit` (`ZwpTextInputV3Impl::Enable`).
    fn enable(&mut self, now: Micros, field: usize) -> Vec<Outgoing> {
        self.client = Some(field);
        self.reset_text_input(now);
        let ti = self.text_input();
        let mut out = vec![req::enable(ti), content_type(ti)];
        if let Some((text, cursor)) = self.latest.surrounding.clone() {
            out.push(self.protocol.surrounding_text(&text, cursor));
        }
        if let Some(rect) = self.latest.rect {
            out.push(cursor_rectangle(ti, rect));
        }
        self.committed = self.latest.clone();
        out.push(self.commit());
        out
    }

    /// C2: `disable` is not gated either; it drops what was held
    /// (`ZwpTextInputV3Impl::Disable`).
    fn disable(&mut self, now: Micros) -> Vec<Outgoing> {
        self.pending = ImeData::default();
        self.discard(now, DropReason::DiscardedByDisable);
        vec![req::disable(self.text_input()), self.commit()]
    }

    /// Enables or disables the text input for each window whose focus or
    /// text-input focus changed (`MaybeUpdateActivated`).
    fn update_activation(&mut self, now: Micros) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for field in 0..self.windows.len() {
            let window = &mut self.windows[field];
            let activated = window.focused && self.protocol.entered == Some(field);
            if window.activated == activated {
                continue;
            }
            window.activated = activated;
            out.extend(if activated {
                self.enable(now, field)
            } else {
                self.disable(now)
            });
        }
        out
    }

    /// C1: sends what is held, then one `commit`, only once a `done` has
    /// named the last commit (`SendPendingImeData`,
    /// `DoneSerialEqualsCommitCount`).
    fn flush(&mut self) -> Vec<Outgoing> {
        if self.last_done != self.commits {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(rect) = self.pending.rect.take() {
            out.push(cursor_rectangle(self.text_input(), rect));
            self.committed.rect = Some(rect);
        }
        if let Some((text, cursor)) = self.pending.surrounding.take() {
            out.push(self.protocol.surrounding_text(&text, cursor));
            self.committed.surrounding = Some((text, cursor));
        }
        if !out.is_empty() {
            out.push(self.commit());
        }
        out
    }

    /// The caret: held unless it is what was sent, then sent if it may be
    /// (`SetCursorRect`).
    fn set_rect(&mut self, rect: [i32; 4]) -> Vec<Outgoing> {
        self.latest.rect = Some(rect);
        if self.committed.rect == Some(rect) {
            return Vec::new();
        }
        self.pending.rect = Some(rect);
        self.flush()
    }

    /// The text: held unless it is what was sent, then sent if it may be
    /// (`SetSurroundingText`). As in Chromium, a text equal to what was
    /// sent leaves an older held one in place.
    fn set_surrounding(&mut self, model: &TextModel) -> Vec<Outgoing> {
        let surrounding = Some((model.text.clone(), model.cursor));
        self.latest.surrounding.clone_from(&surrounding);
        if self.committed.surrounding == surrounding {
            return Vec::new();
        }
        self.pending.surrounding = surrounding;
        self.flush()
    }

    /// A focused window reports its caret, then its text if that changed,
    /// which also resets the prediction to it
    /// (`InputMethodAuraLinux::OnCaretBoundsChanged`,
    /// `SurroundingTextTracker::Update`). Each sends at once if it may.
    fn report(&mut self, field: usize) -> Vec<Outgoing> {
        if !self.windows[field].focused {
            return Vec::new();
        }
        let model = self.fields[field].model.clone();
        let mut out = self.set_rect(caret_rect(&model));
        let window = &mut self.windows[field];
        if window.reported.as_ref() != Some(&model) {
            window.reported = Some(model.clone());
            window.predicted = model.clone();
            out.extend(self.set_surrounding(&model));
        }
        out
    }

    /// The input method of `field`'s window resets
    /// (`InputMethodAuraLinux::ResetContext`): if it was `composing`, it
    /// ignores commits and preedits for 100 ms (C4); its prediction keeps
    /// the composition as text (`SurroundingTextTracker::CancelComposition`);
    /// and the shared text input forgets what it sent, held and applied.
    fn reset_context(&mut self, now: Micros, field: usize, composing: bool) {
        let window = &mut self.windows[field];
        if composing {
            window.guard_until = now + GUARD;
        }
        confirm(&mut window.predicted);
        self.reset_text_input(now);
    }

    /// `field`'s window loses the keyboard focus. Its input method loses
    /// its client, which resets it; it forgets what it reported; and the
    /// text input forgets what it was given and disables
    /// (`RenderWidgetHostViewAura::DetachFromInputMethod`,
    /// `InputMethodAuraLinux::OnWillChangeFocusedClient`,
    /// `UpdateContextFocusState`, `WaylandInputMethodContext::UpdateFocus`).
    /// Blink then finishes the composition, keeping its text
    /// (`WebViewImpl::SetPageFocus`), which the window does not report.
    fn blur(&mut self, now: Micros, field: usize) -> Vec<Outgoing> {
        let composing = !self.fields[field].model.preedit.is_empty();
        self.reset_context(now, field, composing);
        let window = &mut self.windows[field];
        window.focused = false;
        window.reported = None;
        self.latest = ImeData::default();
        let out = self.update_activation(now);
        confirm(&mut self.fields[field].model);
        out
    }

    /// `field`'s window gains the keyboard focus: the text input forgets
    /// what it was given and may enable, then the window reports
    /// (`InputMethodAuraLinux::OnDidChangeFocusedClient`).
    fn focus(&mut self, now: Micros, field: usize) -> Vec<Outgoing> {
        let window = &mut self.windows[field];
        window.focused = true;
        window.reported = None;
        self.latest = ImeData::default();
        let mut out = self.update_activation(now);
        out.extend(self.report(field));
        out
    }

    /// C4: a mouse press or a tap confirms the composition locally and,
    /// in a focused window, resets, whether or not it composed
    /// (`RenderWidgetHostViewEventHandler::FinishImeCompositionSession`,
    /// `InputMethodAuraLinux::CancelComposition`). The cursor then moves
    /// and the window reports. It never enables (C2).
    fn click(&mut self, now: Micros, field: usize, at: Target) -> Vec<Outgoing> {
        let model = &mut self.fields[field].model;
        let composing = !model.preedit.is_empty();
        confirm(model);
        if self.windows[field].focused {
            self.reset_context(now, field, composing);
        }
        self.fields[field].model.click(at);
        self.report(field)
    }

    /// C3: at `done`, whatever its serial, the text input applies the
    /// delete, the commit, then the preedit, which an absent
    /// `preedit_string` clears, if it changed (`ZwpTextInputV3Impl::OnDone`).
    /// The window it last enabled for predicts each, and its input method
    /// applies what it takes. The text input then reports the predicted
    /// text and sends what it holds (`OnInputEventsApplied`,
    /// `SendPredictedSurroundingText`), and the window reports.
    fn done(&mut self, now: Micros, serial: u32) -> Vec<Outgoing> {
        self.last_done = serial;
        let Some(field) = self.client else {
            return Vec::new();
        };
        let events = mem::take(&mut self.events);
        let before = self.fields[field].model.clone();
        let mut applied = false;
        if let Some((back, ahead)) = events.delete {
            applied = true;
            self.delete(now, field, back, ahead);
        }
        if !events.commit.is_empty() {
            applied = true;
            insert(&mut self.windows[field].predicted, &events.commit);
            match self.refusal(now, field) {
                Some(reason) => self.drop(now, Part::Commit(events.commit), reason),
                None => insert(&mut self.fields[field].model, &events.commit),
            }
        }
        if events.preedit != self.applied_preedit {
            applied = true;
            let text = events.preedit.0.clone();
            self.windows[field].predicted.preedit.clone_from(&text);
            match self.refusal(now, field) {
                Some(reason) => self.drop(now, Part::Preedit(text), reason),
                None => self.fields[field].model.preedit = text,
            }
            self.applied_preedit = events.preedit;
        }
        let mut out = Vec::new();
        if applied && (self.committed.surrounding.is_some() || self.pending.surrounding.is_some()) {
            let predicted = self.windows[field].predicted.clone();
            out.extend(self.set_surrounding(&predicted));
        }
        out.extend(self.flush());
        if self.fields[field].model != before {
            out.extend(self.report(field));
        }
        out
    }

    /// Why `field`'s input method refuses a commit or preedit now, if it
    /// does (`InputMethodAuraLinux::OnCommit`, `OnPreeditUpdate`): C4's
    /// guard, or no client.
    fn refusal(&self, now: Micros, field: usize) -> Option<DropReason> {
        let window = &self.windows[field];
        if now < window.guard_until {
            Some(DropReason::InGuard)
        } else if !window.focused {
            Some(DropReason::NoTextInputClient)
        } else {
            None
        }
    }

    /// C5: a delete counts from the composition, or else the selection,
    /// here the cursor, in the predicted text, clamped to it
    /// (`OnDeleteSurroundingTextAroundCursor`); a range that splits a
    /// character is dropped (`DeleteSurroundingTextInRange`). The
    /// prediction deletes it with any composition. C3: the input method
    /// then drops it while the field composes
    /// (`InputMethodAuraLinux::OnDeleteSurroundingText`), and otherwise
    /// deletes as many characters around the field's cursor.
    fn delete(&mut self, now: Micros, field: usize, before: u32, after: u32) {
        let part = Part::Delete { before, after };
        let window = &mut self.windows[field];
        let predicted = &mut window.predicted;
        let start = predicted.cursor.saturating_sub(before as usize);
        let end = predicted.cursor.saturating_add(after as usize);
        let end = end.min(predicted.text.len());
        if !predicted.text.is_char_boundary(start) || !predicted.text.is_char_boundary(end) {
            return self.drop(now, part, DropReason::DeleteSplitsCharacter);
        }
        let (back, ahead) = chars_around(&predicted.text, predicted.cursor, before, after);
        predicted.preedit.clear();
        delete_chars(predicted, back, ahead);
        let focused = window.focused;
        let model = &mut self.fields[field].model;
        let reason = if !focused {
            DropReason::NoTextInputClient
        } else if !model.preedit.is_empty() {
            DropReason::DeleteWhileComposing
        } else {
            return delete_chars(model, back, ahead);
        };
        self.drop(now, part, reason);
    }
}

/// Replaces `model`'s composition with `text`, the cursor after it, as an
/// input method's commit does.
fn insert(model: &mut TextModel, text: &str) {
    model.preedit = text.to_owned();
    confirm(model);
}

impl Client for Chromium {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn focused_field(&self) -> Option<usize> {
        self.keyboard
    }

    fn drops(&self) -> &[Dropped] {
        &self.drops
    }

    fn keyboard_focus(&mut self, now: Micros, surface: Option<u32>) -> Vec<Outgoing> {
        self.protocol.closed &= surface.is_none();
        let focus = surface.map(|surface| field_on(&self.fields, surface));
        if focus == self.keyboard {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(field) = self.keyboard {
            out.extend(self.blur(now, field));
        }
        self.keyboard = focus;
        if let Some(field) = focus {
            out.extend(self.focus(now, field));
        }
        out
    }

    /// C2: `enter` and `leave` change which window may be enabled
    /// (`OnTextInputFocusChanged`). Other events wait for `done`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing> {
        self.protocol
            .check(&self.fields, sender, &event, self.commits);
        match event {
            Event::Enter { .. } | Event::Leave { .. } => return self.update_activation(now),
            Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                let text = utf8(text).unwrap_or_default();
                self.events.preedit = (text, cursor_begin, cursor_end);
            }
            Event::CommitString { text } => self.events.commit = text.unwrap_or_default(),
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => self.events.delete = Some((before_length, after_length)),
            Event::Done { serial } => return self.done(now, serial),
        }
        Vec::new()
    }

    /// The field edits as [`TextModel::insert_key`] does, then reports.
    /// C4: a key ends its window's guard
    /// (`InputMethodAuraLinux::DispatchKeyEvent`). A key whose propagation
    /// an accelerator stops resets the input method; no key the simulation
    /// types is one.
    fn key(&mut self, _now: Micros, key: Key) -> Vec<Outgoing> {
        let Some(field) = self.keyboard else {
            assert!(
                self.protocol.closed,
                "protocol violation: a key without the keyboard focus"
            );
            return Vec::new();
        };
        self.windows[field].guard_until = Micros::default();
        self.fields[field].model.insert_key(key);
        self.report(field)
    }

    /// A mouse press clicks at once; a touch clicks when it lifts as a tap
    /// (`kGestureTap`), as the press path skips touch (`EF_FROM_TOUCH`).
    fn press(&mut self, now: Micros, surface: u32, at: Target, kind: PressKind) -> Vec<Outgoing> {
        let field = field_on(&self.fields, surface);
        match kind {
            PressKind::Button => self.click(now, field, at),
            PressKind::Touch => {
                self.touch = Some((field, at));
                Vec::new()
            }
        }
    }

    /// A lift that did not drag is a tap.
    fn release(&mut self, now: Micros, dragged: bool) -> Vec<Outgoing> {
        match self.touch.take() {
            Some((field, at)) if !dragged => self.click(now, field, at),
            _ => Vec::new(),
        }
    }

    /// The window blurs if it was focused, and opens again with a fresh
    /// input method context that predicts the field's text.
    fn reopen(&mut self, now: Micros, field: usize, surface: u32) -> Vec<Outgoing> {
        let focused = self.keyboard == Some(field);
        let out = match focused {
            true => self.keyboard_focus(now, None),
            false => Vec::new(),
        };
        self.protocol.closed |= focused;
        if self.touch.is_some_and(|(touched, _)| touched == field) {
            self.touch = None;
        }
        self.fields[field].reopen(surface);
        self.windows[field] = Window {
            predicted: self.fields[field].model.clone(),
            ..Window::default()
        };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::script::*;
    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;

    /// The requests `chromium` sends for each of `events` at `now`, then for
    /// a `done` that names its last commit.
    fn answered(
        chromium: &mut Chromium,
        now: Micros,
        events: impl IntoIterator<Item = Event>,
    ) -> Vec<Request> {
        let mut out = send(chromium, now, events);
        let serial = chromium.commits;
        out.extend(send(chromium, now, [done(serial)]));
        out
    }

    /// Chromium with fields holding `texts`, field 0 focused and entered,
    /// and everything it sent answered.
    fn entered(texts: &[&str]) -> Chromium {
        let mut chromium = Chromium::new(TEXT_INPUT, fields(texts));
        let t0 = Micros(0);
        chromium.keyboard_focus(t0, Some(SURFACES[0]));
        send(&mut chromium, t0, [enter(0)]);
        assert_eq!(answered(&mut chromium, t0, []), []);
        chromium
    }

    /// Answers what the client holds, shows `text` as the preedit of the
    /// entered field, at time 0, then answers what it reports as the proxy
    /// does, repeating the preedit.
    fn compose(chromium: &mut Chromium, text: &str) {
        let t0 = Micros(0);
        answered(chromium, t0, []);
        answered(chromium, t0, [preedit(text)]);
        let answer = answered(chromium, t0, [preedit(text)]);
        assert_eq!(answer, [], "the answer changes nothing");
    }

    #[test]
    fn c1_reports_wait_for_a_done_naming_the_last_commit() {
        let mut chromium = Chromium::new(TEXT_INPUT, fields(&[""]));
        let t0 = Micros(0);
        // Focused before it is entered, the window reports; its caret goes
        // at once and closes the gate, so its text waits.
        let focus = chromium.keyboard_focus(t0, Some(SURFACES[0]));
        assert_eq!(requests(focus), [caret(0), Request::Commit {}]);
        // The enable is not gated, and carries what it was given.
        let enable = [
            Request::Enable {},
            plain_text(),
            surrounding("", 0),
            caret(0),
            Request::Commit {},
        ];
        assert_eq!(send(&mut chromium, t0, [enter(0)]), enable);
        assert_eq!(requests(chromium.key(t0, Key::Char('a'))), []);
        let out = send(&mut chromium, t0, [done(2)]);
        assert_eq!(out, [caret(1), surrounding("a", 1), Request::Commit {}]);
        assert_eq!(requests(chromium.key(t0, Key::Char('b'))), []);
        assert_eq!(send(&mut chromium, t0, [done(2)]), []);
        let out = send(&mut chromium, t0, [done(3)]);
        assert_eq!(out, [caret(2), surrounding("ab", 2), Request::Commit {}]);
        // With the gate open, the caret goes at once and closes it, so the
        // text waits.
        assert_eq!(send(&mut chromium, t0, [done(4)]), []);
        let out = requests(chromium.key(t0, Key::Char('c')));
        assert_eq!(out, [caret(3), Request::Commit {}]);
        let out = send(&mut chromium, t0, [done(5)]);
        assert_eq!(out, [surrounding("abc", 3), Request::Commit {}]);
        assert_eq!(send(&mut chromium, t0, [done(6)]), []);
    }

    #[test]
    fn c2_enables_only_while_focused_and_entered_and_never_on_a_click() {
        let mut chromium = entered(&["ab", "x"]);
        let t0 = Micros(0);
        chromium.press(t0, SURFACES[0], Target::Start, PressKind::Button);
        assert_eq!(requests(chromium.release(t0, false)), []);
        let disable = [Request::Disable {}, Request::Commit {}];
        assert_eq!(send(&mut chromium, t0, [leave(0)]), disable);
        // Entered but not focused: no enable.
        assert_eq!(requests(chromium.keyboard_focus(t0, None)), []);
        assert_eq!(send(&mut chromium, t0, [enter(1)]), []);
        // Focused too: the enable carries nothing, as the focus change made
        // the text input forget what it was given.
        let focus = chromium.keyboard_focus(t0, Some(SURFACES[1]));
        let enable = [Request::Enable {}, plain_text(), Request::Commit {}];
        assert_eq!(requests(focus), enable);
        assert_eq!(chromium.focused_field(), Some(1));
    }

    #[test]
    fn c3_a_done_applies_the_delete_then_the_commit_then_the_preedit() {
        let mut chromium = entered(&["0ab1"]);
        let events = [preedit("ㄴ"), commit("가"), delete(1, 0)];
        let out = answered(&mut chromium, Micros(0), events);
        assert_eq!(chromium.fields()[0].model, model("0ab가", 6, "ㄴ"));
        assert_eq!(out, [surrounding("0ab가", 6), Request::Commit {}]);
        assert_eq!(chromium.drops(), []);
    }

    #[test]
    fn c3_a_bare_done_clears_the_preedit() {
        let mut chromium = entered(&["0"]);
        compose(&mut chromium, "가");
        assert_eq!(chromium.fields()[0].model, model("0", 1, "가"));
        answered(&mut chromium, Micros(0), []);
        assert_eq!(chromium.fields()[0].model, model("0", 1, ""));
    }

    #[test]
    fn c4_every_click_resets_the_text_input() {
        let mut chromium = entered(&["0ab"]);
        // The caret did not move, but the text input forgot sending it.
        let click = chromium.press(Micros(0), SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(click), [caret(3), Request::Commit {}]);
        assert_eq!(chromium.windows[0].guard_until, Micros(0));
    }

    #[test]
    fn c4_a_click_while_composing_confirms_it_and_ignores_the_engine_for_100_ms() {
        let mut chromium = entered(&["0ab"]);
        compose(&mut chromium, "가");
        let pressed = Micros::from_millis(10);
        let click = chromium.press(pressed, SURFACES[0], Target::Start, PressKind::Button);
        assert_eq!(requests(click), [caret(0), Request::Commit {}]);
        assert_eq!(chromium.fields()[0].model, model("0ab가", 0, ""));
        // Just before the guard ends, 100 ms after the press, the engine's
        // commit and next preedit are dropped.
        let late = Micros(109_999);
        let serial = chromium.commits;
        send(
            &mut chromium,
            late,
            [commit("가"), preedit("ㄴ"), done(serial)],
        );
        assert_eq!(chromium.fields()[0].model, model("0ab가", 0, ""));
        let drops = [
            dropped(late, Part::Commit("가".to_owned()), DropReason::InGuard),
            dropped(late, Part::Preedit("ㄴ".to_owned()), DropReason::InGuard),
        ];
        assert_eq!(chromium.drops(), drops);
        // When it ends, they apply again.
        let ended = Micros::from_millis(110);
        let serial = chromium.commits;
        send(
            &mut chromium,
            ended,
            [commit("다"), preedit("ㄹ"), done(serial)],
        );
        assert_eq!(chromium.fields()[0].model, model("다0ab가", 3, "ㄹ"));
    }

    #[test]
    fn c4_the_guard_is_the_composing_windows_and_a_key_ends_it() {
        let mut chromium = entered(&["0", "1"]);
        compose(&mut chromium, "가");
        // A blur while composing arms the guard of field 0's window only.
        chromium.keyboard_focus(Micros(0), Some(SURFACES[1]));
        chromium.press(Micros(1), SURFACES[1], Target::End, PressKind::Button);
        assert_eq!(chromium.windows[0].guard_until, Micros::from_millis(100));
        assert_eq!(chromium.windows[1].guard_until, Micros(0));
        chromium.windows[1].guard_until = Micros::from_millis(50);
        chromium.key(Micros(2), Key::Char('a'));
        assert_eq!(chromium.windows[1].guard_until, Micros(0));
    }

    #[test]
    fn c4_a_tap_confirms_when_the_finger_lifts() {
        let mut chromium = entered(&["0"]);
        compose(&mut chromium, "가");
        let t0 = Micros(0);
        chromium.press(t0, SURFACES[0], Target::Start, PressKind::Touch);
        assert_eq!(chromium.fields()[0].model, model("0", 1, "가"));
        chromium.release(t0, false);
        assert_eq!(chromium.fields()[0].model, model("0가", 0, ""));
        // A drag is no tap.
        chromium.press(t0, SURFACES[0], Target::End, PressKind::Touch);
        chromium.release(t0, true);
        assert_eq!(chromium.fields()[0].model, model("0가", 0, ""));
    }

    #[test]
    fn c5_a_delete_counts_from_the_cursor_and_is_dropped_while_composing() {
        let mut chromium = entered(&["0ab1cd"]);
        let t0 = Micros(0);
        chromium.press(t0, SURFACES[0], Target::BeforeMarker(1), PressKind::Button);
        answered(&mut chromium, t0, []);
        answered(&mut chromium, t0, [delete(1, 1)]);
        assert_eq!(chromium.fields()[0].model, model("0acd", 2, ""));
        compose(&mut chromium, "가");
        let at = Micros(5);
        answered(&mut chromium, at, [preedit("가"), delete(1, 1)]);
        assert_eq!(chromium.fields()[0].model, model("0acd", 2, "가"));
        let part = Part::Delete {
            before: 1,
            after: 1,
        };
        let drops = [dropped(at, part, DropReason::DeleteWhileComposing)];
        assert_eq!(chromium.drops(), drops);
    }

    #[test]
    fn a_blur_resets_and_disables_without_a_report_and_drops_what_follows() {
        let mut chromium = entered(&["0", "1"]);
        compose(&mut chromium, "가");
        let t0 = Micros(0);
        send(&mut chromium, t0, [commit("x")]);
        let blur = chromium.keyboard_focus(t0, Some(SURFACES[1]));
        assert_eq!(requests(blur), [Request::Disable {}, Request::Commit {}]);
        assert_eq!(chromium.fields()[0].model, model("0가", 4, ""));
        // Field 0 is still entered, but its window has no client.
        let at = Micros::from_millis(200);
        answered(&mut chromium, at, [commit("나")]);
        assert_eq!(chromium.fields()[0].model, model("0가", 4, ""));
        let drops = [
            dropped(
                t0,
                Part::Commit("x".to_owned()),
                DropReason::DiscardedByReset,
            ),
            dropped(
                at,
                Part::Commit("나".to_owned()),
                DropReason::NoTextInputClient,
            ),
        ];
        assert_eq!(chromium.drops(), drops);
    }

    #[test]
    fn after_a_done_it_reports_the_text_it_predicts() {
        let mut chromium = entered(&["0"]);
        compose(&mut chromium, "가");
        chromium.press(Micros(0), SURFACES[0], Target::End, PressKind::Button);
        // The guard drops the commit, but the prediction has it.
        let out = answered(&mut chromium, Micros(1), [commit("나")]);
        assert_eq!(out, [surrounding("0가나", 7), Request::Commit {}]);
        assert_eq!(chromium.fields()[0].model, model("0가", 4, ""));
    }

    #[test]
    #[should_panic(expected = "protocol violation: done(5) after 2 commits")]
    fn a_done_past_the_commits_is_a_protocol_violation() {
        let mut chromium = Chromium::new(TEXT_INPUT, fields(&[""]));
        chromium.keyboard_focus(Micros(0), Some(SURFACES[0]));
        send(&mut chromium, Micros(0), [enter(0), done(5)]);
    }

    #[test]
    #[should_panic(expected = "fits no text sent")]
    fn a_delete_past_every_text_sent_is_a_protocol_violation() {
        let mut chromium = entered(&["가"]);
        send(&mut chromium, Micros(0), [delete(1, 0)]);
    }

    #[test]
    #[should_panic(expected = "protocol violation: Done { serial: 0 } outside enter and leave")]
    fn an_event_before_enter_is_a_protocol_violation() {
        let mut chromium = Chromium::new(TEXT_INPUT, fields(&[""]));
        send(&mut chromium, Micros(0), [done(0)]);
    }
}
