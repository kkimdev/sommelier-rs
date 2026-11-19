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

//! A model of the `zwp_text_input_v3` client of GTK3 and GTK4, for a
//! single-line entry: facts C6 to C9.
//!
//! Sources: GTK3 (`gtk-3-24`) `modules/input/imwayland.c` and the entry,
//! `gtk/gtkentry.c`; GTK4 (`main`) `gtk/gtkimcontextwayland.c` and the
//! entry's text widget, `gtk/gtktext.c`. A multi-line `GtkTextView`
//! resets and reports differently and is not modelled.
//!
//! A display has one text input (`GtkIMContextWaylandGlobal`). Each field
//! has an entry and its input method context; the context of the field
//! with the keyboard focus is current (`global->current`), receives every
//! event, and alone sends requests. The versions differ:
//!
//! - GTK3 sends requests only between its own `enable` and `disable`
//!   (`context->enabled`); GTK4 whenever its context is current.
//! - GTK3 states a surrounding text that changed twice in one report.
//! - GTK4 reports the caret it last laid out, and lays out after each
//!   event the model handles: a caret that moved goes out with a `commit`
//!   of its own (`after_layout_cb`).
//! - On a press GTK4 always resets, and GTK3 only when its entry needs it
//!   (C8); a single-click release enables on GTK4 always, and on GTK3
//!   only if the caret did not jump (C7).
//!
//! GTK's gestures take a touch as a press, so the press kind is ignored.

use std::mem;

use super::{
    caret_rect, chars_around, content_type, cursor_rectangle, delete_chars, utf8, Client,
    DropReason, Dropped, Field, Part, PressKind, Protocol,
};
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::{self, req, Event};
use crate::sim::clock::Micros;
use crate::sim::editor::{Target, Txn};
use crate::sim::hangul::Key;
use crate::wire::Outgoing;

/// A transaction in progress (`pending_preedit`, `pending_commit`,
/// `pending_surrounding_delete`). A null preedit or commit is `None`; the
/// delete is in characters.
#[derive(Clone, Debug, Default)]
struct Events {
    preedit: Option<String>,
    commit: Option<String>,
    delete: (usize, usize),
}

/// What GTK keeps for a field: its context and its entry.
#[derive(Clone, Debug)]
struct Context {
    /// The transaction in progress.
    events: Events,
    /// The preedit it shows (`current_preedit`).
    preedit: Option<String>,
    /// GTK3: it enabled the text input (`context->enabled`).
    enabled: bool,
    /// The text and cursor it last retrieved from the entry
    /// (`context->surrounding`).
    surrounding: Option<(String, usize)>,
    /// GTK4: the caret it last laid out (`surface_cursor_rect`).
    laid_out: [i32; 4],
    /// The entry resets the context before its cursor next moves
    /// (`need_im_reset`).
    need_im_reset: bool,
}

/// The v3 client of GTK3 or GTK4.
#[derive(Debug)]
pub(in crate::sim) struct Gtk {
    /// GTK4 rather than GTK3.
    v4: bool,
    protocol: Protocol,
    fields: Vec<Field>,
    contexts: Vec<Context>,
    /// The field with the keyboard focus, whose context is current.
    current: Option<usize>,
    /// The commits it made (`global->serial`).
    commits: u32,
    /// The field of the last press and whether it made the caret jump.
    press: Option<(usize, bool)>,
    drops: Vec<Dropped>,
}

impl Gtk {
    /// An application with `fields`, on GTK4 if `v4` and otherwise on GTK3,
    /// its text input `text_input`. Each field was laid out once.
    pub fn new(v4: bool, text_input: u32, fields: Vec<Field>) -> Self {
        let contexts = fields
            .iter()
            .map(|field| Context {
                events: Events::default(),
                preedit: None,
                enabled: false,
                surrounding: None,
                laid_out: caret_rect(&field.model),
                need_im_reset: false,
            })
            .collect();
        Self {
            v4,
            protocol: Protocol::new(text_input),
            fields,
            contexts,
            current: None,
            commits: 0,
            press: None,
            drops: Vec::new(),
        }
    }

    fn text_input(&self) -> u32 {
        self.protocol.text_input
    }

    /// The current field, if its context may send requests.
    fn sender(&self) -> Option<usize> {
        self.current
            .filter(|&field| self.v4 || self.contexts[field].enabled)
    }

    /// `commit`, counted (`commit_state`).
    fn commit(&mut self) -> Outgoing {
        self.commits = self.commits.wrapping_add(1);
        req::commit(self.text_input())
    }

    /// The current field's state and a `commit`, with `cause`
    /// (`notify_im_change`). The context first retrieves the entry's text,
    /// which GTK3 states at once if it changed (`set_surrounding`). GTK3
    /// sends `anchor = cursor`, and GTK4 the selection bound, which is the
    /// cursor here (C9).
    fn notify(&mut self, cause: u32) -> Vec<Outgoing> {
        let Some(field) = self.sender() else {
            return Vec::new();
        };
        let model = &self.fields[field].model;
        let context = &mut self.contexts[field];
        let surrounding = (model.text.clone(), model.cursor);
        let changed = context.surrounding.as_ref() != Some(&surrounding);
        context.surrounding = Some(surrounding);
        let rect = if self.v4 {
            context.laid_out
        } else {
            caret_rect(model)
        };
        let ti = self.text_input();
        let mut out = Vec::new();
        let times = if !self.v4 && changed { 2 } else { 1 };
        for _ in 0..times {
            out.push(self.protocol.surrounding_text(&model.text, model.cursor));
            out.push(req::set_text_change_cause(ti, cause));
        }
        out.push(content_type(ti));
        out.push(cursor_rectangle(ti, rect));
        out.push(self.commit());
        out
    }

    /// C7: `enable`, the state and a `commit` (`enable`, or GTK4's
    /// `activate_osk_with_event`), for the current context.
    fn enable(&mut self) -> Vec<Outgoing> {
        let Some(field) = self.current else {
            return Vec::new();
        };
        self.contexts[field].enabled = true;
        let mut out = vec![req::enable(self.text_input())];
        out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_OTHER));
        out
    }

    /// `disable` and a `commit`; the current context then clears its
    /// preedit (`disable`).
    fn disable(&mut self) -> Vec<Outgoing> {
        let mut out = vec![req::disable(self.text_input())];
        let Some(field) = self.current else {
            return out;
        };
        if self.sender().is_some() {
            out.push(self.commit());
        }
        let context = &mut self.contexts[field];
        context.enabled = false;
        context.preedit = None;
        self.fields[field].model.preedit.clear();
        out
    }

    /// The entry of `field` resets its context, which reports with cause
    /// `other` if it is current (`gtk_im_context_wayland_reset`). The
    /// preedit stays.
    fn reset(&mut self, field: usize) -> Vec<Outgoing> {
        self.contexts[field].need_im_reset = false;
        if self.current != Some(field) {
            return Vec::new();
        }
        self.notify(zwp_text_input_v3::CHANGE_CAUSE_OTHER)
    }

    /// C6: at `done`, whatever its serial, the current context applies the
    /// delete, the commit, then the preedit, which an absent
    /// `preedit_string` clears (`text_input_done`). It reports after each
    /// applied delete (`text_input_delete_surrounding_text_apply`) and
    /// each applied commit (`gtk_im_context_wayland_commit`, the class
    /// handler of `commit`); a transaction that changes only the preedit
    /// is reported only when its serial names the last commit.
    ///
    /// C9: the delete counts characters from the entry's cursor, with the
    /// preedit still shown, clamped to its text
    /// (`gtk_entry_delete_surrounding_cb`, `gtk_editable_delete_text`).
    fn done(&mut self, serial: u32) -> Vec<Outgoing> {
        let Some(field) = self.current else {
            return Vec::new();
        };
        let context = &mut self.contexts[field];
        let events = mem::take(&mut context.events);
        let update_im = events.commit.is_some() || events.preedit != context.preedit;
        let mut out = Vec::new();
        let (before, after) = events.delete;
        if before + after > 0 {
            delete_chars(&mut self.fields[field].model, before, after);
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
            self.contexts[field].need_im_reset = false;
            out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD));
        }
        let context = &mut self.contexts[field];
        context.preedit = events.preedit;
        self.fields[field].model.preedit = context.preedit.clone().unwrap_or_default();
        if update_im && serial == self.commits {
            out.extend(self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD));
        }
        out
    }

    /// Holds a part of a transaction in the current context, or drops it
    /// if none is current (`text_input_preedit` and the others).
    fn hold(&mut self, now: Micros, part: Part) {
        let Some(field) = self.current else {
            let reason = DropReason::NoFocusedField;
            self.drops.push(Dropped {
                at: now,
                part,
                reason,
            });
            return;
        };
        let context = &mut self.contexts[field];
        match part {
            Part::Preedit(text) => context.events.preedit = Some(text),
            Part::Commit(text) => context.events.commit = Some(text),
            // Counted in characters of the text it last retrieved
            // (`text_input_delete_surrounding_text`).
            Part::Delete { before, after } => {
                let (text, cursor) = context.surrounding.clone().unwrap_or_default();
                context.events.delete = chars_around(&text, cursor, before, after);
            }
        }
    }

    /// A null string replaces what the current context held, with nothing
    /// to drop.
    fn hold_null(&mut self, clear: impl FnOnce(&mut Events)) {
        if let Some(field) = self.current {
            clear(&mut self.contexts[field].events);
        }
    }

    /// GTK4 lays out each field: a caret that moved is laid out, and the
    /// current context sends it with a `commit` (`after_layout_cb`).
    fn layout(&mut self, mut out: Vec<Outgoing>) -> Vec<Outgoing> {
        if !self.v4 {
            return out;
        }
        for field in 0..self.fields.len() {
            let rect = caret_rect(&self.fields[field].model);
            if self.contexts[field].laid_out == rect {
                continue;
            }
            self.contexts[field].laid_out = rect;
            if self.current == Some(field) {
                out.push(cursor_rectangle(self.text_input(), rect));
                out.push(self.commit());
            }
        }
        out
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
    /// (`gtk_im_context_wayland_focus_out`, `focus_in`). The entry needs a
    /// reset after either (`gtk_entry_focus_in`, `gtk_entry_focus_out`).
    fn keyboard_focus(&mut self, _now: Micros, surface: Option<u32>) -> Vec<Outgoing> {
        let focus = match surface {
            // The keys that follow an enter that crossed the client's
            // destroy of its surface go nowhere.
            Some(surface) => match self.protocol.field_on(&self.fields, surface) {
                None => {
                    self.protocol.closed = true;
                    return Vec::new();
                }
                field => field,
            },
            None => None,
        };
        self.protocol.closed &= surface.is_none();
        if focus == self.current {
            return Vec::new();
        }
        let entered = self.protocol.entered.is_some();
        let mut out = Vec::new();
        if let Some(field) = self.current {
            self.contexts[field].need_im_reset = true;
            if entered {
                out.extend(self.disable());
            }
        }
        self.current = focus;
        if let Some(field) = focus {
            self.contexts[field].need_im_reset = true;
            if entered {
                out.extend(self.enable());
            }
        }
        self.layout(out)
    }

    /// C7: `enter` enables the current context and `leave` disables it
    /// (`text_input_enter`, `text_input_leave`). Other events wait for
    /// `done`.
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing> {
        self.protocol
            .check(&self.fields, sender, &event, self.commits);
        let out = match event {
            Event::Enter { .. } if self.current.is_some() => self.enable(),
            Event::Leave { .. } if self.current.is_some() => self.disable(),
            Event::Done { serial } => self.done(serial),
            Event::PreeditString { text, .. } => {
                match utf8(text) {
                    Some(text) => self.hold(now, Part::Preedit(text)),
                    None => self.hold_null(|events| events.preedit = None),
                }
                Vec::new()
            }
            Event::CommitString { text } => {
                match text {
                    Some(text) => self.hold(now, Part::Commit(text)),
                    None => self.hold_null(|events| events.commit = None),
                }
                Vec::new()
            }
            Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => {
                let part = Part::Delete {
                    before: before_length,
                    after: after_length,
                };
                self.hold(now, part);
                Vec::new()
            }
            Event::Enter { .. } | Event::Leave { .. } => Vec::new(),
        };
        self.layout(out)
    }

    /// The entry edits as [`TextModel::insert_key`] does, except that
    /// Enter activates it rather than inserting a line break, and
    /// Backspace may leave part of a Hangul syllable
    /// ([`TextModel::backspace_to_nfd_prefix`], C14).
    ///
    /// - A printable key goes through the context, which commits it to the
    ///   entry and so reports as C6 does (GTK3's `GtkIMContextSimple`,
    ///   GTK4's `gtk_im_context_wayland_filter_keypress`); GTK3's entry
    ///   then needs a reset (`gtk_entry_key_press`).
    /// - Left and Right reset (`gtk_entry_move_cursor`), and so does a
    ///   Backspace that deletes (`gtk_entry_backspace`).
    /// - Enter resets if the entry needs it.
    ///
    /// [`TextModel::insert_key`]: crate::sim::editor::TextModel::insert_key
    /// [`TextModel::backspace_to_nfd_prefix`]:
    ///     crate::sim::editor::TextModel::backspace_to_nfd_prefix
    fn key(&mut self, _now: Micros, key: Key) -> Vec<Outgoing> {
        let Some(field) = self.current else {
            assert!(
                self.protocol.closed,
                "protocol violation: a key without the keyboard focus"
            );
            return Vec::new();
        };
        let model = &mut self.fields[field].model;
        let out = match key {
            Key::Char(_) => {
                model.insert_key(key);
                self.contexts[field].need_im_reset = false;
                let out = self.notify(zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD);
                self.contexts[field].need_im_reset = !self.v4;
                out
            }
            Key::Backspace if model.cursor == 0 => Vec::new(),
            Key::Backspace => {
                model.backspace_to_nfd_prefix();
                self.reset(field)
            }
            Key::Left | Key::Right => {
                model.insert_key(key);
                self.reset(field)
            }
            Key::Enter if self.contexts[field].need_im_reset => self.reset(field),
            Key::Enter => Vec::new(),
        };
        self.layout(out)
    }

    fn deleted(&mut self, id: u32) {
        self.protocol.deleted(id);
    }

    /// C8: before the cursor moves, GTK4 always resets
    /// (`gtk_text_click_gesture_pressed`), and GTK3 only if the entry needs
    /// it and the cursor moves (`gtk_entry_real_set_position`).
    fn press(&mut self, _now: Micros, surface: u32, at: Target, _kind: PressKind) -> Vec<Outgoing> {
        let Some(field) = self.protocol.field_on(&self.fields, surface) else {
            self.press = None;
            return Vec::new();
        };
        let model = &self.fields[field].model;
        let mut clicked = model.clone();
        clicked.click(at);
        let moved = clicked.cursor != model.cursor;
        // GTK3 forgets a press once the caret moves by its height
        // (`gtk_im_context_wayland_set_cursor_location`).
        let [x, _, _, height] = caret_rect(model);
        let jumped = (caret_rect(&clicked)[0] - x).abs() >= height;
        let out = if self.v4 || (self.contexts[field].need_im_reset && moved) {
            self.reset(field)
        } else {
            Vec::new()
        };
        self.fields[field].model.click(at);
        self.press = Some((field, jumped));
        self.layout(out)
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
        let out = self.enable();
        self.layout(out)
    }

    /// The field's context loses the focus if it had it, and the window
    /// opens again with a fresh context, laid out once.
    fn reopen(&mut self, now: Micros, field: usize, surface: u32) -> Vec<Outgoing> {
        let focused = self.current == Some(field);
        let out = match focused {
            true => self.keyboard_focus(now, None),
            false => Vec::new(),
        };
        self.protocol.closed |= focused;
        if self.press.is_some_and(|(pressed, _)| pressed == field) {
            self.press = None;
        }
        self.protocol.reopen(&mut self.fields[field], surface);
        self.contexts[field] = Context {
            events: Events::default(),
            preedit: None,
            enabled: false,
            surrounding: None,
            laid_out: caret_rect(&self.fields[field].model),
            need_im_reset: false,
        };
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::script::*;
    use super::*;
    use crate::protocols::text_input_unstable_v3::zwp_text_input_v3::Request;

    /// The requests `gtk` sends for each of `events` at `now`, then for
    /// a `done` that names its last commit.
    fn answered(
        gtk: &mut Gtk,
        now: Micros,
        events: impl IntoIterator<Item = Event>,
    ) -> Vec<Request> {
        let mut out = send(gtk, now, events);
        let serial = gtk.commits;
        out.extend(send(gtk, now, [done(serial)]));
        out
    }

    const OTHER: u32 = zwp_text_input_v3::CHANGE_CAUSE_OTHER;
    const INPUT_METHOD: u32 = zwp_text_input_v3::CHANGE_CAUSE_INPUT_METHOD;

    /// What GTK sends to report `text`, the cursor at byte `cursor`, for
    /// `cause`, with the caret after `cells` characters. GTK3 states a
    /// text that `changed` twice. GTK4 sends the caret it laid out, after
    /// `was` characters, and then lays out the new one.
    fn report(
        v4: bool,
        cause: u32,
        (text, cursor): (&str, i32),
        (was, cells): (i32, i32),
        changed: bool,
    ) -> Vec<Request> {
        let mut out = Vec::new();
        for _ in 0..if !v4 && changed { 2 } else { 1 } {
            out.push(surrounding(text, cursor));
            out.push(Request::SetTextChangeCause { cause });
        }
        out.push(plain_text());
        out.push(caret(if v4 { was } else { cells }));
        out.push(Request::Commit {});
        if v4 && was != cells {
            out.extend([caret(cells), Request::Commit {}]);
        }
        out
    }

    /// `enable`, then the [`report`] of a text that changed.
    fn enabled(v4: bool, text: (&str, i32), cells: (i32, i32)) -> Vec<Request> {
        let mut out = vec![Request::Enable {}];
        out.extend(report(v4, OTHER, text, cells, true));
        out
    }

    /// GTK4 if `v4`, else GTK3, with fields holding `texts`, the keyboard
    /// focus on field 0 and the text input entered there.
    fn entered(v4: bool, texts: &[&str]) -> Gtk {
        let mut gtk = Gtk::new(v4, TEXT_INPUT, fields(texts));
        let focus = gtk.keyboard_focus(Micros(0), Some(SURFACES[0]));
        assert_eq!(requests(focus), []);
        let cells = i32::try_from(texts[0].chars().count()).unwrap();
        let cursor = i32::try_from(texts[0].len()).unwrap();
        let enable = send(&mut gtk, Micros(0), [enter(0)]);
        assert_eq!(enable, enabled(v4, (texts[0], cursor), (cells, cells)));
        gtk
    }

    #[test]
    fn c6_each_applied_commit_or_delete_is_reported_at_once() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            let t0 = Micros(0);
            // A commit is reported whatever the serial.
            let out = send(&mut gtk, t0, [commit("가"), done(0)]);
            assert_eq!(
                out,
                report(v4, INPUT_METHOD, ("0가", 4), (1, 2), true),
                "v4: {v4}"
            );
            // A preedit alone, only when the serial names the last commit;
            // GTK4 still lays out the caret it moved.
            let out = send(&mut gtk, t0, [preedit("ㄴ"), done(0)]);
            let laid_out = if v4 {
                vec![caret(3), Request::Commit {}]
            } else {
                vec![]
            };
            assert_eq!(out, laid_out, "v4: {v4}");
            let out = answered(&mut gtk, t0, [preedit("나")]);
            assert_eq!(
                out,
                report(v4, INPUT_METHOD, ("0가", 4), (3, 3), false),
                "v4: {v4}"
            );
            // The delete, then the commit, each reported; the preedit goes
            // last.
            let out = answered(&mut gtk, t0, [delete(3, 0), commit("다")]);
            // GTK3 shows the preedit until the end; GTK4 lays out once.
            let (deleted, committed) = if v4 {
                ((3, 3), (3, 2))
            } else {
                ((2, 2), (3, 3))
            };
            let mut reports = report(v4, INPUT_METHOD, ("0", 1), deleted, true);
            reports.extend(report(v4, INPUT_METHOD, ("0다", 4), committed, true));
            assert_eq!(out, reports, "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("0다", 4, ""), "v4: {v4}");
            assert_eq!(answered(&mut gtk, t0, []), []);
            assert_eq!(gtk.drops(), []);
        }
    }

    #[test]
    fn c6_a_bare_done_clears_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            send(&mut gtk, Micros(0), [preedit("가"), done(0)]);
            assert_eq!(gtk.fields()[0].model, model("0", 1, "가"), "v4: {v4}");
            send(&mut gtk, Micros(0), [done(0)]);
            assert_eq!(gtk.fields()[0].model, model("0", 1, ""), "v4: {v4}");
        }
    }

    #[test]
    fn c7_gtk3_enables_on_enter_focus_and_a_release_where_the_caret_stays() {
        let mut gtk = Gtk::new(false, TEXT_INPUT, fields(&["ab1", ""]));
        let t0 = Micros(0);
        // No context is current yet.
        assert_eq!(send(&mut gtk, t0, [enter(0)]), []);
        let focus = gtk.keyboard_focus(t0, Some(SURFACES[0]));
        assert_eq!(requests(focus), enabled(false, ("ab1", 3), (3, 3)));
        // A click where the caret is enables again.
        let press = gtk.press(t0, SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(press), []);
        let mut again = vec![Request::Enable {}];
        again.extend(report(false, OTHER, ("ab1", 3), (3, 3), false));
        assert_eq!(requests(gtk.release(t0, false)), again);
        // So does one a character away, which moves the caret less than
        // its height. The focus left the entry needing a reset, which the
        // press sends before the cursor moves.
        let press = gtk.press(t0, SURFACES[0], Target::BeforeMarker(1), PressKind::Button);
        assert_eq!(
            requests(press),
            report(false, OTHER, ("ab1", 3), (3, 3), false)
        );
        let release = gtk.release(t0, false);
        assert_eq!(requests(release), enabled(false, ("ab1", 2), (2, 2)));
        // A drag does not enable, nor does a click that makes the caret
        // jump.
        gtk.press(t0, SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(gtk.release(t0, true)), []);
        gtk.press(t0, SURFACES[0], Target::Start, PressKind::Button);
        assert_eq!(requests(gtk.release(t0, false)), []);
        // The keyboard focus disables one field and enables the other.
        let focus = gtk.keyboard_focus(t0, Some(SURFACES[1]));
        let mut switch = vec![Request::Disable {}, Request::Commit {}];
        switch.extend(enabled(false, ("", 0), (0, 0)));
        assert_eq!(requests(focus), switch);
    }

    #[test]
    fn c7_gtk4_enables_on_any_single_click_release() {
        let mut gtk = entered(true, &["ab"]);
        let t0 = Micros(0);
        let press = gtk.press(t0, SURFACES[0], Target::Start, PressKind::Touch);
        assert_eq!(
            requests(press),
            report(true, OTHER, ("ab", 2), (2, 0), false)
        );
        let release = gtk.release(t0, false);
        assert_eq!(requests(release), enabled(true, ("ab", 0), (0, 0)));
        gtk.press(t0, SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(requests(gtk.release(t0, true)), []);
    }

    #[test]
    fn c8_a_press_resets_on_gtk4_always_and_on_gtk3_when_the_entry_needs_it() {
        let t0 = Micros(0);
        // GTK4 resets even where the caret is, after an input method's
        // commit, and keeps the preedit.
        let mut gtk = entered(true, &["0"]);
        send(&mut gtk, t0, [commit("가"), preedit("ㄴ"), done(0)]);
        let press = gtk.press(t0, SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(
            requests(press),
            report(true, OTHER, ("0가", 4), (3, 3), false)
        );
        assert_eq!(gtk.fields()[0].model, model("0가", 4, "ㄴ"));

        // The focus left GTK3's entry needing a reset; the input method's
        // commit cleared that.
        let mut gtk = entered(false, &["0"]);
        send(&mut gtk, t0, [commit("가"), done(0)]);
        let press = gtk.press(t0, SURFACES[0], Target::Start, PressKind::Button);
        assert_eq!(requests(press), []);
        // A passed key sets it again, but a press must move the cursor.
        let key = gtk.key(t0, Key::Char('a'));
        assert_eq!(
            requests(key),
            report(false, INPUT_METHOD, ("a0가", 1), (1, 1), true)
        );
        let press = gtk.press(t0, SURFACES[0], Target::BeforeMarker(0), PressKind::Button);
        assert_eq!(requests(press), []);
        let press = gtk.press(t0, SURFACES[0], Target::End, PressKind::Button);
        assert_eq!(
            requests(press),
            report(false, OTHER, ("a0가", 1), (1, 1), false)
        );
        let press = gtk.press(t0, SURFACES[0], Target::Start, PressKind::Button);
        assert_eq!(requests(press), []);
    }

    #[test]
    fn c8_keys_that_move_the_cursor_or_delete_reset() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["ab"]);
            let t0 = Micros(0);
            let left = gtk.key(t0, Key::Left);
            assert_eq!(
                requests(left),
                report(v4, OTHER, ("ab", 1), (2, 1), true),
                "v4: {v4}"
            );
            let back = gtk.key(t0, Key::Backspace);
            assert_eq!(
                requests(back),
                report(v4, OTHER, ("b", 0), (1, 0), true),
                "v4: {v4}"
            );
            assert_eq!(requests(gtk.key(t0, Key::Backspace)), [], "v4: {v4}");
            // A printable key leaves GTK3's entry needing a reset, which
            // Enter sends; GTK4's took the key as a commit. Neither
            // inserts a line break.
            gtk.key(t0, Key::Char('c'));
            let enter = requests(gtk.key(t0, Key::Enter));
            let reset = if v4 {
                vec![]
            } else {
                report(v4, OTHER, ("cb", 1), (1, 1), false)
            };
            assert_eq!(enter, reset, "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("cb", 1, ""), "v4: {v4}");
        }
    }

    #[test]
    fn c9_a_delete_counts_from_the_cursor_and_ignores_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0ab1cd"]);
            let t0 = Micros(0);
            gtk.press(t0, SURFACES[0], Target::BeforeMarker(1), PressKind::Button);
            gtk.release(t0, false);
            answered(&mut gtk, t0, [preedit("가")]);
            let events = [preedit("가"), delete(1, 1), done(0)];
            let out = send(&mut gtk, t0, events);
            let expected = report(v4, INPUT_METHOD, ("0acd", 2), (4, 3), true);
            assert_eq!(out, expected, "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("0acd", 2, "가"), "v4: {v4}");
        }
    }

    #[test]
    fn leave_disables_and_clears_the_preedit() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0"]);
            answered(&mut gtk, Micros(0), [preedit("가")]);
            let mut disable = vec![Request::Disable {}, Request::Commit {}];
            if v4 {
                disable.extend([caret(1), Request::Commit {}]);
            }
            assert_eq!(send(&mut gtk, Micros(0), [leave(0)]), disable, "v4: {v4}");
            assert_eq!(gtk.fields()[0].model, model("0", 1, ""), "v4: {v4}");
        }
    }

    #[test]
    fn events_without_a_focused_field_are_dropped() {
        let mut gtk = entered(true, &["0"]);
        let blur = gtk.keyboard_focus(Micros(0), None);
        assert_eq!(requests(blur), [Request::Disable {}, Request::Commit {}]);
        assert_eq!(gtk.focused_field(), None);
        let at = Micros(7);
        answered(&mut gtk, at, [commit("가")]);
        assert_eq!(gtk.fields()[0].model, model("0", 1, ""));
        let expected = dropped(
            at,
            Part::Commit("가".to_owned()),
            DropReason::NoFocusedField,
        );
        assert_eq!(gtk.drops(), [expected]);
    }

    #[test]
    fn a_window_that_opens_again_blurs_and_moves_to_its_new_surface() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["0", "1"]);
            answered(&mut gtk, Micros(0), [preedit("가")]);
            // F2: the focused window closes, blurring as at any blur, and
            // opens again on surface 22.
            let reopen = gtk.reopen(Micros(0), 0, 22);
            assert_eq!(
                requests(reopen),
                [Request::Disable {}, Request::Commit {}],
                "v4: {v4}"
            );
            let field = &gtk.fields()[0];
            let expected = (22, &model("0", 1, ""));
            assert_eq!((field.surface, &field.model), expected, "v4: {v4}");
            // A key, a focus or a press the proxy sent before it got the
            // destroy goes nowhere; it leaves the surface while it is
            // valid.
            assert_eq!(requests(gtk.key(Micros(0), Key::Char('a'))), []);
            gtk.keyboard_focus(Micros(0), Some(SURFACES[0]));
            assert_eq!(gtk.focused_field(), None, "v4: {v4}");
            let press = gtk.press(Micros(0), SURFACES[0], Target::Start, PressKind::Button);
            assert_eq!(requests(press), []);
            assert_eq!(requests(gtk.release(Micros(0), false)), []);
            assert_eq!(gtk.fields()[0].model, model("0", 1, ""), "v4: {v4}");
            send(&mut gtk, Micros(0), [leave(0)]);
            gtk.deleted(SURFACES[0]);
            // The new window takes the focus.
            gtk.keyboard_focus(Micros(0), Some(22));
            assert_eq!(gtk.focused_field(), Some(0), "v4: {v4}");
            // An unfocused window opens again with no request.
            assert_eq!(requests(gtk.reopen(Micros(0), 1, 23)), []);
            assert_eq!(gtk.focused_field(), Some(0), "v4: {v4}");
            // The keys after a keyboard enter that crossed the destroy go
            // nowhere.
            gtk.keyboard_focus(Micros(0), None);
            gtk.keyboard_focus(Micros(0), Some(SURFACES[1]));
            assert_eq!(requests(gtk.key(Micros(0), Key::Char('a'))), []);
        }
    }

    #[test]
    fn backspace_leaves_the_start_of_a_hangul_syllable() {
        for v4 in [false, true] {
            let mut gtk = entered(v4, &["a가각"]);
            let t0 = Micros(0);
            // 각 leaves the conjoining 가, which looks the same as 가.
            gtk.key(t0, Key::Backspace);
            let field = &gtk.fields()[0].model;
            assert_eq!(field, &model("a가\u{1100}\u{1161}", 10, ""), "v4: {v4}");
            // That cluster, and then 가, each leave the conjoining ᄀ, which
            // a third Backspace deletes; Latin goes whole.
            gtk.key(t0, Key::Backspace);
            assert_eq!(
                gtk.fields()[0].model,
                model("a가\u{1100}", 7, ""),
                "v4: {v4}"
            );
            gtk.key(t0, Key::Backspace);
            assert_eq!(gtk.fields()[0].model, model("a가", 4, ""), "v4: {v4}");
            gtk.key(t0, Key::Backspace);
            assert_eq!(gtk.fields()[0].model, model("a\u{1100}", 4, ""), "v4: {v4}");
            gtk.key(t0, Key::Backspace);
            gtk.key(t0, Key::Backspace);
            assert_eq!(gtk.fields()[0].model, model("", 0, ""), "v4: {v4}");
        }
    }
}
