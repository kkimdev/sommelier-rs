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

//! A model of the `zwp_text_input_v3` client of GTK3 and GTK4: facts C6 to
//! C9.
//!
//! Sources:
//!
//! - GTK3, branch `gtk-3-24`: the input module `modules/input/imwayland.c`
//!   and the entry `gtk/gtkentry.c`;
//! - GTK4, branch `main`: `gtk/gtkimcontextwayland.c` and the text widget
//!   `gtk/gtktext.c`.
//!
//! A display has one text input (`GtkIMContextWaylandGlobal`). Each field
//! has an input method context, and the context of the field with the
//! keyboard focus is the current one (`global->current`), which every
//! event goes to and which alone sends requests. The two versions differ
//! in three ways:
//!
//! - GTK3 sends requests only between its own `enable` and `disable`
//!   (`context->enabled`); GTK4 whenever its context is current.
//! - On a press, GTK4 always resets; GTK3 only when its entry needs it
//!   (C8).
//! - On a single-click release, GTK4 always enables; GTK3 only if the
//!   caret did not jump (C7).

use std::mem;

use super::{
    caret_rect, content_type, cursor_rectangle, field_on, utf8, Client, DropReason, Dropped, Field,
    Part, PressKind, Protocol,
};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{self, req, Event};
use crate::sim::clock::Micros;
use crate::sim::editor::{Target, Txn};
use crate::sim::hangul::Key;
use crate::wire::Outgoing;

/// The events of the transaction in progress, as the current context
/// holds them (`pending_preedit`, `pending_commit`,
/// `pending_surrounding_delete`). A preedit or commit of null is `None`.
#[derive(Clone, Debug, Default)]
struct Events {
    preedit: Option<String>,
    commit: Option<String>,
    delete: (u32, u32),
}

/// The v3 client of GTK3 or GTK4.
#[derive(Debug)]
pub(super) struct Gtk {
    /// GTK4 rather than GTK3.
    v4: bool,
    protocol: Protocol,
    fields: Vec<Field>,
    /// The field with the keyboard focus, whose context is current.
    current: Option<usize>,
    /// GTK3: whether the current context enabled the text input
    /// (`context->enabled`).
    enabled: bool,
    /// The commits it made (`global->serial`).
    commits: u32,
    /// GTK3: whether the entry resets its context before its cursor next
    /// moves (`GtkEntryPrivate::need_im_reset`).
    need_im_reset: bool,
    /// The transaction in progress.
    events: Events,
    /// The preedit the current context shows (`current_preedit`).
    preedit: Option<String>,
    /// The field of the last press and whether it made the caret jump.
    press: Option<(usize, bool)>,
    drops: Vec<Dropped>,
}

impl Gtk {
    /// An application with `fields`, on GTK4 if `v4` and otherwise on GTK3,
    /// its text input `text_input`.
    pub fn new(v4: bool, text_input: u32, fields: Vec<Field>) -> Self {
        Self {
            v4,
            protocol: Protocol::new(text_input),
            fields,
            current: None,
            enabled: false,
            commits: 0,
            need_im_reset: false,
            events: Events::default(),
            preedit: None,
            press: None,
            drops: Vec::new(),
        }
    }

    fn text_input(&self) -> u32 {
        self.protocol.text_input
    }

    /// Whether the current context may send requests.
    fn may_send(&self) -> bool {
        self.current.is_some() && (self.v4 || self.enabled)
    }

    /// `commit`, counted (`commit_state`).
    fn commit(&mut self) -> Outgoing {
        self.commits = self.commits.wrapping_add(1);
        req::commit(self.text_input())
    }

    /// The current field's state and a `commit`, with `cause`
    /// (`notify_im_change`). GTK3 sends `anchor = cursor`, and GTK4 the
    /// selection bound, which is the cursor here (C9).
    fn notify(&mut self, cause: u32) -> Vec<Outgoing> {
        let Some(field) = self.current.filter(|_| self.may_send()) else {
            return Vec::new();
        };
        let model = &self.fields[field].model;
        let ti = self.text_input();
        let mut out = vec![
            self.protocol.surrounding_text(&model.text, model.cursor),
            req::set_text_change_cause(ti, cause),
            content_type(ti),
            cursor_rectangle(ti, caret_rect(model)),
        ];
        out.push(self.commit());
        out
    }

    /// C7: `enable`, the state and a `commit` (`enable`, or GTK4's
    /// `activate_osk_with_event`).
    fn enable(&mut self) -> Vec<Outgoing> {
        self.enabled = true;
        let mut out = vec![req::enable(self.text_input())];
        out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_OTHER));
        out
    }

    /// `disable` and a `commit`; the context then clears its preedit
    /// locally (`disable`).
    fn disable(&mut self) -> Vec<Outgoing> {
        let mut out = vec![req::disable(self.text_input())];
        if self.may_send() {
            out.push(self.commit());
        }
        self.enabled = false;
        if let Some(field) = self.current {
            self.preedit = None;
            self.fields[field].model.preedit.clear();
        }
        out
    }

    /// C6: at `done`, whatever its serial, the current context applies the
    /// delete, the commit, then the preedit, which an absent
    /// `preedit_string` clears (`text_input_done`). It answers each
    /// applied delete (`text_input_delete_surrounding_text_apply`) and
    /// each applied commit (the class handler of `commit`,
    /// `gtk_im_context_wayland_commit`) with its state and a `commit`. A
    /// transaction that only changes the preedit is answered only when its
    /// serial names the last commit.
    ///
    /// C9: a delete counts from the cursor, with the preedit still shown.
    fn done(&mut self, serial: u32) -> Vec<Outgoing> {
        let Some(field) = self.current else {
            return Vec::new();
        };
        let events = mem::take(&mut self.events);
        let update_im = events.commit.is_some() || events.preedit != self.preedit;
        let mut out = Vec::new();
        let (before, after) = events.delete;
        if before > 0 || after > 0 {
            let model = &mut self.fields[field].model;
            model.apply_txn(&Txn {
                delete: (before as usize, after as usize),
                preedit: model.preedit.clone(),
                ..Txn::default()
            });
            out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD));
        }
        if let Some(commit) = events.commit {
            let model = &mut self.fields[field].model;
            model.apply_txn(&Txn {
                commit,
                preedit: model.preedit.clone(),
                ..Txn::default()
            });
            // The entry takes the commit (`gtk_entry_enter_text`).
            self.need_im_reset = false;
            out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD));
        }
        self.preedit = events.preedit;
        self.fields[field].model.preedit = self.preedit.clone().unwrap_or_default();
        if update_im && serial == self.commits {
            out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD));
        }
        out
    }

    /// Holds an event part for `done` with `hold`, or drops it if no
    /// context is current (`text_input_preedit` and the others).
    fn hold(&mut self, now: Micros, part: Part, hold: impl FnOnce(&mut Events)) -> Vec<Outgoing> {
        if self.current.is_some() {
            hold(&mut self.events);
        } else {
            let reason = DropReason::NoFocusedField;
            self.drops.push(Dropped {
                at: now,
                part,
                reason,
            });
        }
        Vec::new()
    }
}

impl Client for Gtk {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn focused_field(&self) -> Option<usize> {
        self.current
    }

    fn drops(&self) -> &[Dropped] {
        &self.drops
    }

    /// C7: the context that loses the keyboard focus disables, if the text
    /// input is entered, and the one that gains it enables
    /// (`gtk_im_context_wayland_focus_out`, `focus_in`). GTK3's entry needs
    /// a reset after either (`gtk_entry_focus_in`, `gtk_entry_focus_out`).
    fn keyboard_focus(&mut self, _now: Micros, surface: Option<u32>) -> Vec<Outgoing> {
        let focus = surface.map(|surface| field_on(&self.fields, surface));
        if focus == self.current {
            return Vec::new();
        }
        let entered = self.protocol.entered.is_some();
        let mut out = Vec::new();
        if self.current.is_some() && entered {
            out.extend(self.disable());
        }
        self.current = focus;
        self.need_im_reset = true;
        if self.current.is_some() && entered {
            out.extend(self.enable());
        }
        out
    }

    /// C7: `enter` enables the current context and `leave` disables it
    /// (`text_input_enter`, `text_input_leave`). Other events wait for
    /// `done`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing> {
        self.protocol
            .check(&self.fields, sender, &event, self.commits);
        match event {
            Event::Enter { .. } if self.current.is_some() => self.enable(),
            Event::Leave { .. } if self.current.is_some() => self.disable(),
            Event::Enter { .. } | Event::Leave { .. } => Vec::new(),
            Event::PreeditString { text, .. } => {
                let text = utf8(text);
                let part = Part::Preedit(text.clone().unwrap_or_default());
                self.hold(now, part, |events| events.preedit = text)
            }
            Event::CommitString { text } => {
                let part = Part::Commit(text.clone().unwrap_or_default());
                self.hold(now, part, |events| events.commit = text)
            }
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                let part = Part::Delete {
                    before: before_length,
                    after: after_length,
                };
                self.hold(now, part, |events| {
                    events.delete = (before_length, after_length)
                })
            }
            Event::Done { serial } => self.done(serial),
        }
    }

    /// The field edits as [`TextModel::insert_key`] does. A printable key
    /// goes through the context, which commits it to the entry and so
    /// reports it as C6 does (GTK3's `GtkIMContextSimple`, GTK4's
    /// `gtk_im_context_wayland_filter_keypress`); GTK3's entry then needs a
    /// reset (`gtk_entry_key_press`).
    ///
    /// [`TextModel::insert_key`]: crate::sim::editor::TextModel::insert_key
    fn key(&mut self, _now: Micros, key: Key) -> Vec<Outgoing> {
        let field = self
            .current
            .expect("protocol violation: a key without the keyboard focus");
        self.fields[field].model.insert_key(key);
        if !matches!(key, Key::Char(_)) {
            return Vec::new();
        }
        let out = self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD);
        self.need_im_reset = true;
        out
    }

    /// C8: before the cursor moves, GTK4 always resets
    /// (`gtk_text_click_gesture_pressed`), and GTK3 only if the entry
    /// needs it and the cursor moves (`gtk_entry_real_set_position`). A
    /// reset is the state and a `commit` with cause `other`, from the
    /// current context only (`gtk_im_context_wayland_reset`); the preedit
    /// stays.
    fn press(&mut self, _now: Micros, surface: u32, at: Target, _kind: PressKind) -> Vec<Outgoing> {
        let field = field_on(&self.fields, surface);
        let model = &self.fields[field].model;
        let mut clicked = model.clone();
        clicked.click(at);
        let moved = clicked.cursor != model.cursor;
        // GTK3 forgets a press once the caret moves by its height
        // (`gtk_im_context_wayland_set_cursor_location`).
        let [x, _, _, height] = caret_rect(model);
        let jumped = (caret_rect(&clicked)[0] - x).abs() >= height;
        let mut out = Vec::new();
        if self.v4 || (self.need_im_reset && moved) {
            self.need_im_reset = false;
            if self.current == Some(field) {
                out = self.notify(zwp_text_input_v3::CHANGE_CAUSE_OTHER);
            }
        }
        self.fields[field].model.click(at);
        self.press = Some((field, jumped));
        out
    }

    /// C7: a single-click release in the current field that did not drag
    /// enables: on GTK4 always (`gtk_text_click_gesture_released`); on
    /// GTK3 if the text input is entered and the caret did not jump
    /// (`released_cb`).
    fn release(&mut self, _now: Micros, dragged: bool) -> Vec<Outgoing> {
        let Some((field, jumped)) = self.press.take() else {
            return Vec::new();
        };
        let entered = self.protocol.entered.is_some();
        if dragged || self.current != Some(field) || !(self.v4 || (entered && !jumped)) {
            return Vec::new();
        }
        self.enable()
    }
}

#[cfg(test)]
mod tests {
    use super::super::script::*;
    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;

    /// GTK4 if `v4`, else GTK3, with fields holding `texts`, the keyboard
    /// focus on field 0 and the text input entered there.
    fn entered(v4: bool, texts: &[&str]) -> Gtk {
        let mut gtk = Gtk::new(v4, TEXT_INPUT, fields(texts));
        assert_eq!(
            requests(gtk.keyboard_focus(Micros(0), Some(SURFACES[0]))),
            []
        );
        let enter = Event::Enter {
            surface: SURFACES[0],
        };
        assert_eq!(gtk.event(Micros(0), TEXT_INPUT, enter).len(), 6);
        gtk
    }

    /// The requests GTK sends for each of `events`, at time 0.
    fn send(gtk: &mut Gtk, events: impl IntoIterator<Item = Event>) -> Vec<Request> {
        let mut out = Vec::new();
        for event in events {
            out.extend(requests(gtk.event(Micros(0), TEXT_INPUT, event)));
        }
        out
    }

    /// The requests that state `text` with the cursor at byte `cursor`
    /// and the caret after `cells` characters, with `cause`, then commit.
    fn state(text: &str, cursor: i32, cells: i32, cause: u32) -> Vec<Request> {
        vec![
            surrounding(text, cursor),
            Request::SetTextChangeCause { cause },
            plain_text(),
            caret(cells),
            Request::Commit {},
        ]
    }

    /// [`state`] for an edit of the input method.
    fn edited(text: &str, cursor: i32, cells: i32) -> Vec<Request> {
        state(
            text,
            cursor,
            cells,
            zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD,
        )
    }

    /// `enable`, then [`state`] with cause `other`.
    fn enabled(text: &str, cursor: i32, cells: i32) -> Vec<Request> {
        let state = state(text, cursor, cells, zwp_text_input_v3::CHANGE_CAUSE_OTHER);
        [vec![Request::Enable {}], state]
            .into_iter()
            .flatten()
            .collect()
    }

    #[test]
    fn c6_each_applied_commit_or_delete_is_answered_at_once() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            // A commit is answered whatever the serial.
            let out = send(&mut gtk, [commit("가"), Event::Done { serial: 0 }]);
            assert_eq!(out, edited("0가", 4, 2), "v4: {v4}");
            // A preedit alone is answered only when the serial names the
            // last commit.
            assert_eq!(
                send(&mut gtk, [preedit("ㄴ"), Event::Done { serial: 1 }]),
                []
            );
            let out = send(&mut gtk, [preedit("나"), Event::Done { serial: 2 }]);
            assert_eq!(out, edited("0가", 4, 3), "v4: {v4}");
            // The delete, then the commit, each answered; the preedit
            // goes last.
            let out = send(
                &mut gtk,
                [delete(3, 0), commit("다"), Event::Done { serial: 3 }],
            );
            let answers: Vec<_> = [edited("0", 1, 2), edited("0다", 4, 3)]
                .into_iter()
                .flatten()
                .collect();
            assert_eq!(out, answers, "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("0다", 4, ""), "v4: {v4}");
            // Nothing changed: no answer.
            assert_eq!(send(&mut gtk, [Event::Done { serial: 5 }]), []);
            assert_eq!(gtk.drops(), []);
        }
    }

    #[test]
    fn c6_a_bare_done_clears_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            send(&mut gtk, [preedit("가"), Event::Done { serial: 0 }]);
            assert_eq!(gtk.fields()[0].model, model("0", 1, "가"), "v4: {v4}");
            send(&mut gtk, [Event::Done { serial: 0 }]);
            assert_eq!(gtk.fields()[0].model, model("0", 1, ""), "v4: {v4}");
        }
    }

    #[test]
    fn c7_gtk3_enables_on_enter_focus_and_a_release_where_the_caret_stays() {
        let mut gtk = Gtk::new(false, TEXT_INPUT, fields(&["ab1", ""]));
        let enter = Event::Enter {
            surface: SURFACES[0],
        };
        // No context is current yet.
        assert_eq!(send(&mut gtk, [enter]), []);
        let focus = gtk.keyboard_focus(Micros(0), Some(SURFACES[0]));
        assert_eq!(requests(focus), enabled("ab1", 3, 3));
        // A click where the caret is enables again.
        assert_eq!(
            requests(gtk.press(Micros(0), SURFACES[0], Target::End, PressKind::Button)),
            []
        );
        let release = gtk.release(Micros(0), false);
        assert_eq!(requests(release), enabled("ab1", 3, 3));
        // So does one a character away, which moves the caret less than
        // its height. The focus left the entry needing a reset, which the
        // press sends before the cursor moves.
        let press = gtk.press(
            Micros(0),
            SURFACES[0],
            Target::BeforeMarker(1),
            PressKind::Button,
        );
        let reset = state("ab1", 3, 3, zwp_text_input_v3::CHANGE_CAUSE_OTHER);
        assert_eq!(requests(press), reset);
        let release = gtk.release(Micros(0), false);
        assert_eq!(requests(release), enabled("ab1", 2, 2));
        // A drag does not enable.
        assert_eq!(
            requests(gtk.press(Micros(0), SURFACES[0], Target::End, PressKind::Button)),
            []
        );
        assert_eq!(requests(gtk.release(Micros(0), true)), []);
        // Nor does a click that makes the caret jump.
        assert_eq!(
            requests(gtk.press(Micros(0), SURFACES[0], Target::Start, PressKind::Button)),
            []
        );
        assert_eq!(requests(gtk.release(Micros(0), false)), []);
        // The keyboard focus disables one field and enables the other.
        let focus = gtk.keyboard_focus(Micros(0), Some(SURFACES[1]));
        let switch = [
            vec![Request::Disable {}, Request::Commit {}],
            enabled("", 0, 0),
        ];
        let switch: Vec<_> = switch.into_iter().flatten().collect();
        assert_eq!(requests(focus), switch);
    }

    #[test]
    fn c7_gtk4_enables_on_any_single_click_release() {
        let mut gtk = entered(true, &["ab"]);
        let press = gtk.press(Micros(0), SURFACES[0], Target::Start, PressKind::Button);
        let reset = state("ab", 2, 2, zwp_text_input_v3::CHANGE_CAUSE_OTHER);
        assert_eq!(requests(press), reset);
        assert_eq!(requests(gtk.release(Micros(0), false)), enabled("ab", 0, 0));
        gtk.press(Micros(0), SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(gtk.release(Micros(0), true)), []);
    }

    #[test]
    fn c8_a_press_resets_on_gtk4_always_and_on_gtk3_when_the_entry_needs_it() {
        let other = zwp_text_input_v3::CHANGE_CAUSE_OTHER;
        // GTK4 resets even where the caret is, after an input method's
        // commit, and keeps the preedit.
        let mut gtk = entered(true, &["0"]);
        send(&mut gtk, [commit("가"), Event::Done { serial: 1 }]);
        send(&mut gtk, [preedit("ㄴ"), Event::Done { serial: 2 }]);
        let press = gtk.press(Micros(0), SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(press), state("0가", 4, 3, other));
        assert_eq!(gtk.fields()[0].model, model("0가", 4, "ㄴ"));

        let mut gtk = entered(false, &["0"]);
        // The focus left the entry needing a reset; the input method's
        // commit then clears that.
        send(&mut gtk, [commit("가"), Event::Done { serial: 1 }]);
        assert_eq!(
            requests(gtk.press(Micros(0), SURFACES[0], Target::Start, PressKind::Button)),
            []
        );
        // A passed key sets it again, but a press that does not move the
        // cursor does not reset.
        let key = gtk.key(Micros(0), Key::Char('a'));
        assert_eq!(requests(key), edited("a0가", 1, 1));
        assert_eq!(
            requests(gtk.press(
                Micros(0),
                SURFACES[0],
                Target::BeforeMarker(0),
                PressKind::Button
            )),
            []
        );
        let press = gtk.press(Micros(0), SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(press), state("a0가", 1, 1, other));
        assert_eq!(
            requests(gtk.press(Micros(0), SURFACES[0], Target::Start, PressKind::Button)),
            []
        );
    }

    #[test]
    fn c9_a_delete_counts_from_the_cursor_and_ignores_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0ab1cd"]);
            gtk.press(
                Micros(0),
                SURFACES[0],
                Target::BeforeMarker(1),
                PressKind::Button,
            );
            // The answer reports the moved cursor, which the delete counts
            // from.
            let serial = gtk.commits;
            send(&mut gtk, [preedit("가"), Event::Done { serial }]);
            let out = send(
                &mut gtk,
                [preedit("가"), delete(1, 1), Event::Done { serial: 0 }],
            );
            assert_eq!(out, edited("0acd", 2, 3), "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("0acd", 2, "가"), "v4: {v4}");
            assert_eq!(gtk.drops(), []);
        }
    }

    #[test]
    fn leave_disables_and_clears_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            send(&mut gtk, [preedit("가"), Event::Done { serial: 1 }]);
            let leave = Event::Leave {
                surface: SURFACES[0],
            };
            assert_eq!(
                send(&mut gtk, [leave]),
                [Request::Disable {}, Request::Commit {}]
            );
            assert_eq!(gtk.fields()[0].model, model("0", 1, ""), "v4: {v4}");
        }
    }

    #[test]
    fn events_without_a_focused_field_are_dropped() {
        let mut gtk = entered(true, &["0"]);
        let blur = gtk.keyboard_focus(Micros(0), None);
        assert_eq!(requests(blur), [Request::Disable {}, Request::Commit {}]);
        let at = Micros(7);
        gtk.event(at, TEXT_INPUT, commit("가"));
        gtk.event(at, TEXT_INPUT, Event::Done { serial: 2 });
        assert_eq!(gtk.fields()[0].model, model("0", 1, ""));
        let dropped = Dropped {
            at,
            part: Part::Commit("가".to_owned()),
            reason: DropReason::NoFocusedField,
        };
        assert_eq!(gtk.drops(), [dropped]);
    }
}
