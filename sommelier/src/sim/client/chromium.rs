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
//! Sources, at Chromium `main@ffb4f13519`:
//!
//! - `ZwpTextInputV3Impl`, in
//!   `ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`: the commit
//!   count, the reports and the transaction at `done`;
//! - `WaylandInputMethodContext`, in
//!   `ui/ozone/platform/wayland/host/wayland_input_method_context.cc`:
//!   enabling, and where a delete counts from;
//! - `InputMethodAuraLinux`, in `ui/base/ime/linux/input_method_auralinux.cc`:
//!   the delete it drops while composing and the 100 ms guard.
//!
//! Chromium has one text input per Wayland connection
//! (`WaylandConnection::EnsureTextInputV3`), which its windows share, so
//! the model has one text input and a field on each surface. The field of
//! the entered surface is the text input's client.

use std::mem;

use super::{
    caret_rect, check_delete, content_type, cursor_rectangle, field_on, surrounding_text, utf8,
    Client, DropReason, Dropped, Field, Protocol,
};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{req, Event};
use crate::sim::clock::Micros;
use crate::sim::editor::{Target, TextModel, Txn};
use crate::sim::hangul::Key;
use crate::wire::Outgoing;

/// How long a reset that ends a composition makes Chromium ignore the
/// input method's commits and preedits (`kIgnoreCommitsDuration`).
const GUARD: Micros = Micros::from_millis(100);

/// What the text input sent of a field's state, or holds to send
/// (`ZwpTextInputV3Impl::ImeData`). The content type never changes after
/// the enable that sends it, so it is not tracked.
#[derive(Clone, Debug, Default)]
struct ImeData {
    /// The text and the cursor.
    surrounding: Option<(String, usize)>,
    /// The caret.
    rect: Option<[i32; 4]>,
}

/// A preedit as Chromium compares two: its text and cursor
/// (`ZwpTextInputV3Impl::PreeditData`).
type Preedit = (String, i32, i32);

/// The events of the transaction in progress
/// (`ZwpTextInputV3Impl::InputEvents`). No `preedit_string` means an
/// empty preedit.
#[derive(Clone, Debug, Default)]
struct Events {
    preedit: Preedit,
    commit: String,
    delete: Option<(u32, u32)>,
}

/// Chromium's v3 client.
#[derive(Debug)]
pub(super) struct Chromium {
    protocol: Protocol,
    fields: Vec<Field>,
    /// The field with the keyboard focus.
    keyboard: Option<usize>,
    /// The commits it made (`commit_count`).
    commits: u32,
    /// The serial of the last `done` (`last_done_serial`).
    last_done: u32,
    /// What it last sent of the state.
    committed: ImeData,
    /// What it holds to send once a `done` names its last commit.
    pending: ImeData,
    /// The transaction in progress.
    events: Events,
    /// The preedit the last transaction applied, as the text input saw it.
    applied_preedit: Preedit,
    /// Until when it ignores the input method's commits and preedits.
    guard_until: Micros,
    drops: Vec<Dropped>,
}

impl Chromium {
    /// An application with `fields`, its text input `text_input`.
    pub fn new(text_input: u32, fields: Vec<Field>) -> Self {
        Self {
            protocol: Protocol::new(text_input),
            fields,
            keyboard: None,
            commits: 0,
            last_done: 0,
            committed: ImeData::default(),
            pending: ImeData::default(),
            events: Events::default(),
            applied_preedit: Preedit::default(),
            guard_until: Micros::default(),
            drops: Vec::new(),
        }
    }

    fn text_input(&self) -> u32 {
        self.protocol.text_input
    }

    /// `commit`, counted (`ZwpTextInputV3Impl::Commit`).
    fn commit(&mut self) -> Outgoing {
        self.commits = self.commits.wrapping_add(1);
        req::commit(self.text_input())
    }

    /// C2: `enable` is not gated. It forgets what was sent and held, then
    /// sends the content type, the field's text and caret, and a `commit`
    /// (`ZwpTextInputV3Impl::Enable`). Chromium sends the text and caret
    /// it last cached; the model takes the field's, as its renderer
    /// reports them when the field gains focus.
    fn enable(&mut self, field: usize) -> Vec<Outgoing> {
        self.pending = ImeData::default();
        self.events = Events::default();
        self.applied_preedit = Preedit::default();
        let model = &self.fields[field].model;
        let surrounding = (model.text.clone(), model.cursor);
        let rect = caret_rect(model);
        let ti = self.text_input();
        let mut out = vec![
            req::enable(ti),
            content_type(ti),
            surrounding_text(ti, model),
            cursor_rectangle(ti, rect),
        ];
        self.committed = ImeData {
            surrounding: Some(surrounding),
            rect: Some(rect),
        };
        out.push(self.commit());
        out
    }

    /// C2: `disable` is not gated either; it drops what was held
    /// (`ZwpTextInputV3Impl::Disable`).
    fn disable(&mut self) -> Vec<Outgoing> {
        self.pending = ImeData::default();
        self.events = Events::default();
        vec![req::disable(self.text_input()), self.commit()]
    }

    /// The renderer of `field` reports its text and caret, if the text
    /// input is enabled for it: each is held unless it is what was last
    /// sent (`SetSurroundingText`, `SetCursorRect`). As in Chromium, a
    /// report equal to what was sent leaves an older held one in place.
    fn report(&mut self, field: usize) {
        if self.protocol.entered != Some(field) {
            return;
        }
        let model = &self.fields[field].model;
        let surrounding = Some((model.text.clone(), model.cursor));
        if self.committed.surrounding != surrounding {
            self.pending.surrounding = surrounding;
        }
        let rect = Some(caret_rect(model));
        if self.committed.rect != rect {
            self.pending.rect = rect;
        }
    }

    /// C1: sends what is held, then one `commit`, only once a `done` has
    /// named the last commit (`SendPendingImeData`,
    /// `DoneSerialEqualsCommitCount`).
    fn flush(&mut self) -> Vec<Outgoing> {
        if self.last_done != self.commits {
            return Vec::new();
        }
        let ti = self.text_input();
        let mut out = Vec::new();
        if let Some(rect) = self.pending.rect.take() {
            out.push(cursor_rectangle(ti, rect));
            self.committed.rect = Some(rect);
        }
        if let Some((text, cursor)) = self.pending.surrounding.take() {
            let model = TextModel {
                text,
                cursor,
                preedit: String::new(),
            };
            out.push(surrounding_text(ti, &model));
            self.committed.surrounding = Some((model.text, model.cursor));
        }
        if !out.is_empty() {
            out.push(self.commit());
        }
        out
    }

    /// C3: at `done`, whatever its serial, applies the delete, the commit,
    /// then the preedit, which an absent `preedit_string` clears; then
    /// reports the text the edits left and sends what is held
    /// (`ZwpTextInputV3Impl::OnDone`, `OnInputEventsApplied`).
    ///
    /// - C3, C5: a delete counts from the composition, or else the
    ///   selection, here the cursor
    ///   (`OnDeleteSurroundingTextAroundCursor`); while a composition is
    ///   shown, the input method drops it
    ///   (`InputMethodAuraLinux::OnDeleteSurroundingText`).
    /// - A commit replaces the composition.
    /// - The preedit is applied only if it differs from the one applied
    ///   last.
    /// - C4: within the guard, the commit and the preedit are dropped
    ///   (`InputMethodAuraLinux::IgnoringNonKeyInput`), but the text input
    ///   still takes the preedit as applied.
    fn done(&mut self, now: Micros, field: usize, serial: u32) -> Vec<Outgoing> {
        self.last_done = serial;
        let events = mem::take(&mut self.events);
        let guarded = now < self.guard_until;
        let model = &self.fields[field].model;
        let mut txn = Txn {
            preedit: model.preedit.clone(),
            ..Txn::default()
        };
        let mut drops = Vec::new();
        let mut applied = false;
        if let Some((before, after)) = events.delete {
            check_delete(model, before, after);
            applied = true;
            if model.preedit.is_empty() {
                txn.delete = (before as usize, after as usize);
            } else {
                drops.push(DropReason::DeleteWhileComposing { before, after });
            }
        }
        if !events.commit.is_empty() {
            applied = true;
            if guarded {
                drops.push(DropReason::CommitInGuard(events.commit));
            } else {
                txn.commit = events.commit;
                txn.preedit.clear();
            }
        }
        if events.preedit != self.applied_preedit {
            applied = true;
            if guarded {
                drops.push(DropReason::PreeditInGuard(events.preedit.0.clone()));
            } else {
                txn.preedit.clone_from(&events.preedit.0);
            }
            self.applied_preedit = events.preedit;
        }
        self.fields[field].model.apply_txn(&txn);
        self.drops
            .extend(drops.into_iter().map(|reason| Dropped { at: now, reason }));
        if applied {
            self.report(field);
        }
        self.flush()
    }

    /// Ends the composition `field` shows, keeping its text.
    fn confirm(&mut self, field: usize) {
        let model = &mut self.fields[field].model;
        let commit = mem::take(&mut model.preedit);
        model.apply_txn(&Txn {
            commit,
            ..Txn::default()
        });
    }
}

impl Client for Chromium {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn drops(&self) -> &[Dropped] {
        &self.drops
    }

    /// Keys go to the field with the keyboard focus. A field that loses
    /// it keeps its composition as text, as Blink finishes composing when
    /// its page loses focus (`WebViewImpl::SetPageFocus`, in
    /// `third_party/blink/renderer/core/exported/web_view_impl.cc`). The
    /// model takes the keyboard focus for the window's activation.
    fn keyboard_focus(&mut self, _now: Micros, surface: Option<u32>) -> Vec<Outgoing> {
        let focus = surface.map(|surface| field_on(&self.fields, surface));
        let left = self.keyboard.filter(|&field| Some(field) != focus);
        self.keyboard = focus;
        let Some(field) = left else {
            return Vec::new();
        };
        if self.fields[field].model.preedit.is_empty() {
            return Vec::new();
        }
        self.confirm(field);
        self.report(field);
        self.flush()
    }

    /// C2: `enter` enables the field and `leave` disables it
    /// (`WaylandInputMethodContext::MaybeUpdateActivated`). Other events
    /// wait for `done`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing> {
        self.protocol
            .check(&self.fields, sender, &event, self.commits);
        match event {
            Event::Enter { .. } => {
                let field = self.protocol.entered.expect("checked");
                self.enable(field)
            }
            Event::Leave { .. } => self.disable(),
            Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => {
                self.events.preedit = (utf8(text), cursor_begin, cursor_end);
                Vec::new()
            }
            Event::CommitString { text } => {
                self.events.commit = text.unwrap_or_default();
                Vec::new()
            }
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                self.events.delete = Some((before_length, after_length));
                Vec::new()
            }
            Event::Done { serial } => {
                let field = self.protocol.entered.expect("checked");
                self.done(now, field, serial)
            }
        }
    }

    /// The field edits as [`TextModel::insert_key`] does, then reports.
    /// C4: a key ends the guard, as any key the input method handles does
    /// (`InputMethodAuraLinux::DispatchKeyEvent`).
    fn key(&mut self, _now: Micros, key: Key) -> Vec<Outgoing> {
        let field = self
            .keyboard
            .expect("protocol violation: a key without the keyboard focus");
        self.guard_until = Micros::default();
        self.fields[field].model.insert_key(key);
        self.report(field);
        self.flush()
    }

    /// C2: a press never enables. C4: on a press in a field that shows a
    /// composition, Chromium confirms it locally and sends nothing for it
    /// (`RenderWidgetHostViewEventHandler::FinishImeCompositionSession`),
    /// then resets: it ignores the input method's commits and preedits for
    /// 100 ms (`InputMethodAuraLinux::ResetContext`) and its text input
    /// forgets what it sent, held and applied (`ZwpTextInputV3Impl::Reset`).
    /// The cursor then moves, and the field reports.
    fn press(&mut self, now: Micros, surface: u32, at: Target) -> Vec<Outgoing> {
        let field = field_on(&self.fields, surface);
        if !self.fields[field].model.preedit.is_empty() {
            self.confirm(field);
            self.guard_until = now + GUARD;
            self.committed = ImeData::default();
            self.pending = ImeData::default();
            self.events = Events::default();
            self.applied_preedit = Preedit::default();
        }
        self.fields[field].model.click(at);
        self.report(field);
        self.flush()
    }

    fn release(&mut self, _now: Micros, _dragged: bool) -> Vec<Outgoing> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::script::*;
    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;

    /// Chromium with fields holding `texts`, the keyboard focus and the
    /// text input on field 0, and the enable answered.
    fn entered(texts: &[&str]) -> Chromium {
        let fields = texts
            .iter()
            .enumerate()
            .map(|(index, text)| field(index, text))
            .collect();
        let mut chromium = Chromium::new(TEXT_INPUT, fields);
        chromium.keyboard_focus(Micros(0), Some(SURFACES[0]));
        let enter = Event::Enter {
            surface: SURFACES[0],
        };
        chromium.event(Micros(0), TEXT_INPUT, enter);
        assert_eq!(send(&mut chromium, Event::Done { serial: 1 }), []);
        chromium
    }

    /// The requests Chromium sends for `event`, at time 0.
    fn send(chromium: &mut Chromium, event: Event) -> Vec<Request> {
        send_at(chromium, Micros(0), event)
    }

    /// The requests Chromium sends for `event` at `now`.
    fn send_at(chromium: &mut Chromium, now: Micros, event: Event) -> Vec<Request> {
        requests(chromium.event(now, TEXT_INPUT, event))
    }

    /// The requests Chromium sends for each of `events` at `now`.
    fn send_all(
        chromium: &mut Chromium,
        now: Micros,
        events: impl IntoIterator<Item = Event>,
    ) -> Vec<Request> {
        let mut out = Vec::new();
        for event in events {
            out.extend(send_at(chromium, now, event));
        }
        out
    }

    /// Shows `text` as the preedit of the entered field, at time 0, in a
    /// transaction ending with `done(serial)`, then answers the commit
    /// that reports it as the proxy does, repeating the preedit.
    fn compose(chromium: &mut Chromium, text: &str, serial: u32) {
        send_all(chromium, Micros(0), [preedit(text), Event::Done { serial }]);
        let answer = Event::Done { serial: serial + 1 };
        let out = send_all(chromium, Micros(0), [preedit(text), answer]);
        assert_eq!(out, [], "the answer changes nothing");
    }

    #[test]
    fn c1_reports_wait_for_a_done_naming_the_last_commit() {
        let mut chromium = Chromium::new(TEXT_INPUT, vec![field(0, "")]);
        chromium.keyboard_focus(Micros(0), Some(SURFACES[0]));
        let enter = Event::Enter {
            surface: SURFACES[0],
        };
        assert_eq!(
            send(&mut chromium, enter),
            [
                Request::Enable {},
                plain_text(),
                surrounding("", 0),
                caret(0),
                Request::Commit {},
            ]
        );
        // One commit is not yet answered: the report is held.
        assert_eq!(requests(chromium.key(Micros(0), Key::Char('a'))), []);
        assert_eq!(
            send(&mut chromium, Event::Done { serial: 1 }),
            [caret(1), surrounding("a", 1), Request::Commit {}]
        );
        // The second commit is not answered: the next two keys' reports
        // are held, and only the last of each goes.
        chromium.key(Micros(0), Key::Char('b'));
        chromium.key(Micros(0), Key::Char('c'));
        assert_eq!(send(&mut chromium, Event::Done { serial: 1 }), []);
        assert_eq!(
            send(&mut chromium, Event::Done { serial: 2 }),
            [caret(3), surrounding("abc", 3), Request::Commit {}]
        );
        // Nothing changed: nothing to send.
        assert_eq!(send(&mut chromium, Event::Done { serial: 3 }), []);
    }

    #[test]
    fn c2_enables_on_enter_and_disables_on_leave_but_never_on_a_click() {
        let mut chromium = entered(&["ab", "x"]);
        let click = chromium.press(Micros(0), SURFACES[0], Target::Start);
        assert_eq!(
            requests(click),
            [caret(0), surrounding("ab", 0), Request::Commit {}]
        );
        assert_eq!(requests(chromium.release(Micros(0), false)), []);
        let leave = Event::Leave {
            surface: SURFACES[0],
        };
        assert_eq!(
            send(&mut chromium, leave),
            [Request::Disable {}, Request::Commit {}]
        );
        chromium.keyboard_focus(Micros(0), Some(SURFACES[1]));
        // The enable is not gated: three commits are unanswered.
        let enter = Event::Enter {
            surface: SURFACES[1],
        };
        assert_eq!(
            send(&mut chromium, enter),
            [
                Request::Enable {},
                plain_text(),
                surrounding("x", 1),
                caret(1),
                Request::Commit {},
            ]
        );
    }

    #[test]
    fn c3_a_done_applies_the_delete_then_the_commit_then_the_preedit() {
        let mut chromium = entered(&["0ab1"]);
        let out = send_all(
            &mut chromium,
            Micros(0),
            [
                preedit("ㄴ"),
                commit("가"),
                delete(1, 0),
                Event::Done { serial: 1 },
            ],
        );
        assert_eq!(chromium.fields()[0].model, model("0ab가", 6, "ㄴ"));
        // It then reports the text the edits left.
        assert_eq!(out, [caret(5), surrounding("0ab가", 6), Request::Commit {}]);
        assert_eq!(chromium.drops(), []);
    }

    #[test]
    fn c3_a_bare_done_clears_the_preedit() {
        let mut chromium = entered(&["0"]);
        compose(&mut chromium, "가", 1);
        assert_eq!(chromium.fields()[0].model, model("0", 1, "가"));
        send(&mut chromium, Event::Done { serial: 2 });
        assert_eq!(chromium.fields()[0].model, model("0", 1, ""));
    }

    #[test]
    fn c3_a_repeated_preedit_changes_nothing() {
        let mut chromium = entered(&["0"]);
        compose(&mut chromium, "가", 1);
        let out = send_all(
            &mut chromium,
            Micros(0),
            [preedit("가"), Event::Done { serial: 2 }],
        );
        assert_eq!(out, []);
        assert_eq!(chromium.fields()[0].model, model("0", 1, "가"));
    }

    #[test]
    fn c4_a_click_while_composing_confirms_it_and_ignores_the_input_method_for_100_ms() {
        let mut chromium = entered(&["0ab"]);
        compose(&mut chromium, "가", 1);
        let pressed = Micros::from_millis(10);
        // The reset forgets what was sent, so the report goes in full.
        let click = chromium.press(pressed, SURFACES[0], Target::Start);
        assert_eq!(
            requests(click),
            [caret(0), surrounding("0ab가", 0), Request::Commit {}]
        );
        assert_eq!(chromium.fields()[0].model, model("0ab가", 0, ""));
        // Just before the guard ends, 100 ms after the press, Exo's commit
        // and next preedit are dropped.
        let late = Micros(109_999);
        send_all(
            &mut chromium,
            late,
            [commit("가"), preedit("ㄴ"), Event::Done { serial: 3 }],
        );
        assert_eq!(chromium.fields()[0].model, model("0ab가", 0, ""));
        assert_eq!(
            chromium.drops(),
            [
                Dropped {
                    at: late,
                    reason: DropReason::CommitInGuard("가".to_owned()),
                },
                Dropped {
                    at: late,
                    reason: DropReason::PreeditInGuard("ㄴ".to_owned()),
                },
            ]
        );
        // When it ends, they apply again.
        send_all(
            &mut chromium,
            Micros::from_millis(110),
            [commit("다"), preedit("ㄹ"), Event::Done { serial: 3 }],
        );
        assert_eq!(chromium.fields()[0].model, model("다0ab가", 3, "ㄹ"));
        assert_eq!(chromium.drops().len(), 2);
    }

    #[test]
    fn c4_only_a_click_on_a_composition_arms_the_guard_and_a_key_ends_it() {
        let mut chromium = entered(&["0"]);
        chromium.press(Micros(0), SURFACES[0], Target::End);
        send_all(
            &mut chromium,
            Micros(1),
            [commit("가"), Event::Done { serial: 1 }],
        );
        compose(&mut chromium, "나", 2);
        chromium.press(Micros(10), SURFACES[0], Target::End);
        chromium.key(Micros(20), Key::Char(' '));
        send_all(
            &mut chromium,
            Micros(30),
            [commit("다"), Event::Done { serial: 4 }],
        );
        assert_eq!(chromium.fields()[0].model, model("0가나 다", 11, ""));
        assert_eq!(chromium.drops(), []);
    }

    #[test]
    fn c5_a_delete_counts_from_the_composition_or_else_the_cursor() {
        let mut chromium = entered(&["0ab1cd"]);
        chromium.press(Micros(0), SURFACES[0], Target::BeforeMarker(1));
        send_all(
            &mut chromium,
            Micros(0),
            [delete(1, 1), Event::Done { serial: 2 }],
        );
        assert_eq!(chromium.fields()[0].model, model("0acd", 2, ""));
        // From the composition, it would delete around the preedit, but
        // the input method drops it while composing.
        compose(&mut chromium, "가", 3);
        send_all(
            &mut chromium,
            Micros(5),
            [preedit("가"), delete(1, 1), Event::Done { serial: 4 }],
        );
        assert_eq!(chromium.fields()[0].model, model("0acd", 2, "가"));
        assert_eq!(
            chromium.drops(),
            [Dropped {
                at: Micros(5),
                reason: DropReason::DeleteWhileComposing {
                    before: 1,
                    after: 1,
                },
            }]
        );
    }

    #[test]
    fn a_field_that_loses_the_keyboard_focus_keeps_its_composition_as_text() {
        let mut chromium = entered(&["0", "1"]);
        compose(&mut chromium, "가", 1);
        let out = chromium.keyboard_focus(Micros(0), Some(SURFACES[1]));
        assert_eq!(requests(out), [surrounding("0가", 4), Request::Commit {}]);
        assert_eq!(chromium.fields()[0].model, model("0가", 4, ""));
    }

    #[test]
    #[should_panic(expected = "protocol violation: done(2) after 1 commits")]
    fn a_done_past_the_commits_is_a_protocol_violation() {
        let mut chromium = entered(&[""]);
        send(&mut chromium, Event::Done { serial: 2 });
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn a_delete_past_the_text_is_a_protocol_violation() {
        let mut chromium = entered(&["가"]);
        send_all(
            &mut chromium,
            Micros(0),
            [delete(1, 0), Event::Done { serial: 1 }],
        );
    }

    #[test]
    #[should_panic(expected = "protocol violation: Done { serial: 1 } outside enter and leave")]
    fn an_event_before_enter_is_a_protocol_violation() {
        let mut chromium = Chromium::new(TEXT_INPUT, vec![field(0, "")]);
        send(&mut chromium, Event::Done { serial: 1 });
    }
}
