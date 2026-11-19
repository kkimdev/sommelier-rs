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

//! What the core knows of Exo's text for one text input ([`ExoView`]):
//! the [`ExoMirror`] of its host text input, fed every request the core
//! sends and every event Exo sends, and whether the guest is in step with
//! it, which is when a [`Frame`] measures Exo's deletes (H9) and preedit
//! regions (X1) in Exo's own text (`docs/IME.md` 2.2). The core
//! drops every delete and region it cannot measure so ([`Unmeasured`]).

use std::collections::VecDeque;
use std::ops::Range;

use super::guest::{resolve_preedit_cursor, GuestTxn, Preedit};
use super::mirror::{Applied, ExoMirror, ExoState, Health, Sent};
use super::state::{Assembly, DeleteDrop, RegionDrop};
use super::{BarrierToken, HostIme, TiId};
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as ext;
use crate::wire::IndexedString;

/// The guest's edits so far: the tracker-changing transactions it got,
/// and the edits Exo does not see that the proxy forwarded or caused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Edits {
    from_exo: u64,
    unseen: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExoView {
    /// The mirror, from v11 of the host's text-input extension, the first
    /// whose confirmations are `confirm_preedit` and not a
    /// `cursor_position` and `commit_string` pair the mirror could misread,
    /// and whose regions it can see; `None` below, where Exo's deletes and
    /// regions are dropped.
    mirror: Option<ExoMirror>,
    /// The guest's text is Exo's predicted text without the composition
    /// Exo holds, which the guest shows as its preedit, with its cursor
    /// where Exo predicts it, but for `unseen`.
    /// Only a report sets it: one Exo has processed, or one the core
    /// withheld. Never without the mirror.
    in_step: bool,
    /// The proxy forwarded the guest a key or a press since the guest last
    /// stated its text, which may have edited it where Exo does not see.
    unseen: bool,
    edits: Edits,
    /// `edits` once the proxy had handled the guest's latest commit.
    at_commit: Edits,
    /// Each report's barrier, with the edits the report may not hold: Exo's
    /// since the guest's commit before it, which the guest may not have got
    /// when it made the report, and the others since the report, `None`
    /// for the empty stand-in of a guest that stated no text, which tells
    /// nothing of the guest's; and whether a later report the core withheld
    /// has already said where the guest is, so that this one can only take
    /// it out of step, by resetting Exo.
    reports: VecDeque<(BarrierToken, Option<Edits>, bool)>,
    /// The mirror has diverged from Exo once, which warned.
    warned: bool,
}

/// Why the core cannot measure an event of Exo's in the mirror, and so
/// drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unmeasured {
    /// The host's text-input extension is older than v11, or missing, so
    /// the text input has no mirror.
    NoMirror,
    /// The mirror does not know Exo's state exactly.
    Unsure,
    /// The proxy forwarded the guest a key or a press it has not yet
    /// stated the effect of.
    Unseen,
    /// A report on its way to Exo will name no state Exo has, or may not.
    Crossing,
    /// The guest is not in step with Exo's state before the event.
    OutOfStep,
}

/// What reached the guest of one host event.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Edit {
    /// The bytes it deleted before the cursor, and whether it deleted any.
    pub(super) deleted_before: u32,
    pub(super) deleted: bool,
    /// The length of the text it committed.
    pub(super) committed: usize,
    /// The guest showed a preedit before it.
    pub(super) shown_before: bool,
}

/// Exo's state before a host event, with the guest in step with it, in
/// which the event is measured.
#[derive(Clone, Debug)]
pub(super) struct Frame {
    state: ExoState,
    /// The composition Exo holds, which the guest shows as its preedit; or
    /// else empty, at Exo's cursor, which is the guest's.
    preedit: Range<usize>,
}

impl ExoView {
    /// The view of a text input whose host extension is `ext_version`, if
    /// any.
    pub(super) fn new(ext_version: Option<u32>) -> Self {
        let mirrored = ext_version.is_some_and(|v| v >= ext::EVT_CONFIRM_PREEDIT_SINCE);
        Self {
            mirror: mirrored.then(ExoMirror::new),
            in_step: false,
            unseen: false,
            edits: Edits::default(),
            at_commit: Edits::default(),
            reports: VecDeque::new(),
            warned: false,
        }
    }

    /// Whether a frame measures the next event.
    #[cfg(test)]
    pub(super) fn in_step(&self) -> bool {
        self.in_step && !self.unseen
    }

    #[cfg(test)]
    pub(super) fn mirror(&self) -> Option<&ExoMirror> {
        self.mirror.as_ref()
    }

    /// The mirror diverged, which only the log shows.
    #[cfg(test)]
    pub(super) fn warned(&self) -> bool {
        self.warned
    }

    /// The frame of the next host event, before the mirror takes it: Exo's
    /// state, if the mirror knows it exactly and a guest that shows `shown`
    /// can be in step with it.
    pub(super) fn frame(&self, shown: Option<&Preedit>) -> Result<Frame, Unmeasured> {
        let mirror = self.mirror.as_ref().ok_or(Unmeasured::NoMirror)?;
        let state = mirror.exact().ok_or(Unmeasured::Unsure)?;
        let preedit = preedit_in(state, shown).ok_or(Unmeasured::OutOfStep)?;
        Ok(Frame {
            state: state.clone(),
            preedit,
        })
    }

    /// The frame `frame` made before the event just observed, if it
    /// measures the event: the text input has a mirror, which was exact
    /// before the event and after it, the guest has stated every key and
    /// press, no report on its way to Exo crossed the event, and the guest
    /// was in step with the frame.
    pub(super) fn measuring(&self, frame: Result<Frame, Unmeasured>) -> Result<Frame, Unmeasured> {
        let mirror = self.mirror.as_ref().ok_or(Unmeasured::NoMirror)?;
        // An event can make a mirror that was unsure exact again.
        if mirror.health() != Health::Exact || frame.as_ref().err() == Some(&Unmeasured::Unsure) {
            return Err(Unmeasured::Unsure);
        }
        if self.unseen {
            return Err(Unmeasured::Unseen);
        }
        if mirror.report_crosses() {
            return Err(Unmeasured::Crossing);
        }
        frame
            .ok()
            .filter(|_| self.in_step)
            .ok_or(Unmeasured::OutOfStep)
    }

    /// The mirror takes host event `event`, before the gate: stale events
    /// and those behind a fence changed Exo's tracker too.
    pub(super) fn observe(&mut self, ti: TiId, event: &HostIme) {
        let Some(mirror) = &mut self.mirror else {
            return;
        };
        let health = mirror.health();
        match event {
            &HostIme::PreeditStyling {
                index,
                length,
                style,
            } => mirror.preedit_styling(index, length, style),
            &HostIme::PreeditCursor(index) => mirror.preedit_cursor(index),
            &HostIme::CursorPosition { index, anchor } => mirror.cursor_position(index, anchor),
            HostIme::Preedit { text, commit, .. } => match text {
                IndexedString::Utf8(text) => mirror.preedit_string(text, commit),
                IndexedString::NotUtf8(_) => mirror.unreadable(),
            },
            HostIme::Commit { text, .. } => mirror.commit_string(text),
            &HostIme::Delete { index, length } => mirror.delete_surrounding_text(index, length),
            &HostIme::PreeditRegion { index, length } => mirror.set_preedit_region(index, length),
            &HostIme::Confirm { keep_selection } => mirror.confirm_preedit(keep_selection),
        }
        if mirror.take_diverged() {
            // A misreading of Chromium's: a bug to report, and the same one
            // each time.
            let level = match std::mem::replace(&mut self.warned, true) {
                false => log::Level::Warn,
                true => log::Level::Debug,
            };
            log::log!(
                level,
                "The Exo mirror of text input {} diverged: an event of Exo's did not agree with it",
                ti.0
            );
        }
        Self::health_changed(mirror, ti, health);
    }

    /// After a tracker-changing host event, with `frame` the frame made
    /// before it, `edit` what reached the guest, if anything did, and
    /// `shown` the preedit the guest shows now: whether the event's
    /// transaction keeps a guest in step with Exo if it was, which the new
    /// state records, and so whether the guest still is.
    pub(super) fn after(
        &mut self,
        frame: Option<&Frame>,
        edit: Option<Edit>,
        shown: Option<&Preedit>,
    ) {
        let cursor = frame.zip(edit).and_then(|(frame, edit)| {
            // Chromium applies a delete before it clears the preedit, and
            // drops it (C3).
            if edit.deleted && edit.shown_before {
                return None;
            }
            let cursor = frame
                .preedit
                .start
                .checked_sub(edit.deleted_before as usize)?;
            Some(cursor + edit.committed)
        });
        let Some(mirror) = &mut self.mirror else {
            return;
        };
        let preedit = mirror.exact().and_then(|state| preedit_in(state, shown));
        let keeps = cursor.is_some_and(|cursor| preedit.map(|range| range.start) == Some(cursor));
        self.in_step &= keeps;
        if let Some(edit) = edit {
            self.edits.from_exo += u64::from(edit.deleted || edit.committed > 0);
        }
        mirror.mark(keeps);
    }

    /// The guest may have made an edit Exo does not see: the proxy
    /// forwarded it a key or a press. A report the guest made before it
    /// may not hold it; one it makes after it states it.
    pub(super) fn guest_edited(&mut self) {
        self.edits.unseen += 1;
        self.unseen = true;
    }

    /// A guest commit stated no surrounding text: the guest still has the
    /// text it last reported, so a key or a press since changed nothing
    /// (C1: Chromium reports its text only when it changed).
    pub(super) fn unchanged(&mut self) {
        self.unseen = false;
    }

    /// The guest's field changed, or its composition ended without an
    /// event of Exo's. Its next report, which states the field, may put it
    /// in step again.
    pub(super) fn step_lost(&mut self) {
        self.in_step = false;
    }

    /// The proxy has handled a guest commit.
    pub(super) fn commit_handled(&mut self) {
        self.at_commit = self.edits;
    }

    /// The core forwarded the guest's surrounding text `text` with
    /// `cursor` and `anchor`, then barrier `token`. If the two may reach
    /// Exo in separate reads, Exo may process an event between them, which
    /// the mirror then cannot place.
    pub(super) fn report_sent(
        &mut self,
        token: BarrierToken,
        (text, cursor, anchor): (String, u32, u32),
        one_read: bool,
        stated: bool,
    ) {
        let Some(mirror) = &mut self.mirror else {
            return;
        };
        mirror.sent(
            token,
            Sent::Report {
                text,
                cursor,
                anchor,
                split: !one_read,
            },
        );
        let since = Edits {
            from_exo: self.at_commit.from_exo,
            unseen: self.edits.unseen,
        };
        self.reports
            .push_back((token, stated.then_some(since), false));
    }

    /// Report suppression (G9c): with the mirror, whether
    /// the core withholds the guest's report `report`, from a guest that
    /// shows `shown`, which Exo could not match only for the composition
    /// the guest shows outside its text, so that it would only drop that
    /// composition (E4). A withheld report puts the guest in step as a
    /// matched one does, at the state with that composition, and states
    /// every key and press before it.
    pub(super) fn withholds(
        &mut self,
        (text, cursor, anchor): &(String, u32, u32),
        shown: Option<&Preedit>,
    ) -> bool {
        let (Some(mirror), Some(shown)) = (&self.mirror, shown) else {
            return false;
        };
        let Some(in_step) = mirror.withholds(text, *cursor, *anchor, shown.text()) else {
            return false;
        };
        self.in_step = in_step;
        self.unseen = false;
        for (_, _, superseded) in &mut self.reports {
            *superseded = true;
        }
        true
    }

    /// The core sent a `reset`, then its fence `token`.
    pub(super) fn cancel_sent(&mut self, token: BarrierToken) {
        if let Some(mirror) = &mut self.mirror {
            mirror.sent(token, Sent::Cancel);
        }
    }

    /// The barrier `token` is done. A report Exo processed puts the guest in
    /// step if it matched a state the guest is in step with, or reset Exo to
    /// the guest's text with no edit of Exo's since the guest's commit
    /// before it; either with no other edit since the report. Without the
    /// mirror nothing waits for a barrier.
    pub(super) fn barrier_done(&mut self, ti: TiId, token: BarrierToken) {
        let Some(mirror) = &mut self.mirror else {
            return;
        };
        let health = mirror.health();
        let applied = mirror.barrier_done(token);
        let since = match self.reports.front() {
            Some(&(front, since, superseded)) if front == token => {
                self.reports.pop_front();
                Some((since, superseded))
            }
            _ => None,
        };
        let exact = mirror.health() == Health::Exact;
        let Some((since, superseded)) = since else {
            return Self::health_changed(mirror, ti, health);
        };
        let Some(since) = since else {
            // A stand-in: Exo holds empty text, the guest perhaps not.
            self.in_step = false;
            mirror.mark(false);
            return Self::health_changed(mirror, ti, health);
        };
        match applied {
            // A later report the core withheld placed the guest, and a
            // report Exo matches changes nothing of Exo's.
            Some(Applied::Matched { .. }) if superseded => {}
            Some(Applied::Matched { in_step }) => self.in_step = exact && in_step,
            Some(Applied::Reset { cursor }) => {
                // Exo dropped the composition a later withheld report took
                // it to hold.
                self.in_step =
                    !superseded && exact && cursor && self.edits.from_exo == since.from_exo;
                mirror.mark(self.in_step);
            }
            Some(Applied::Cancel) | None => {}
        }
        // The report states every key and press before it.
        self.unseen &= self.edits.unseen != since.unseen;
        Self::health_changed(mirror, ti, health);
    }

    fn health_changed(mirror: &ExoMirror, ti: TiId, before: Health) {
        let health = mirror.health();
        if health != before {
            log::debug!(
                "The Exo mirror of text input {} is {health:?}, was {before:?}",
                ti.0
            );
        }
    }
}

/// Where the guest's preedit lies in `state`: the composition, if the guest
/// shows its text; or else empty at Exo's cursor, if it is one. `None` if
/// the guest cannot be in step with `state`.
fn preedit_in(state: &ExoState, shown: Option<&Preedit>) -> Option<Range<usize>> {
    if state.composition.is_empty() {
        let cursor = state.selection;
        return cursor.is_empty().then_some(cursor.end..cursor.end);
    }
    (shown.map(Preedit::text) == Some(state.composition_text())).then(|| state.composition.clone())
}

impl Frame {
    /// X1: the bytes from `index` past Exo's cursor, `length` long, become
    /// the preedit. Exo measures from its selection's end (ZTIM 248-268). A
    /// region must cover the preedit Exo holds, or Exo's cursor: it deletes
    /// the rest, which the guest has, around the guest's cursor. An empty
    /// region leaves Exo's composition as text, which the guest commits.
    pub(super) fn region(
        &self,
        index: i32,
        length: u32,
        assembly: Assembly,
    ) -> Result<GuestTxn, RegionDrop> {
        let state = &self.state;
        let exo_cursor = state.selection.end;
        let start = usize::try_from(exo_cursor as i64 + i64::from(index))
            .map_err(|_| RegionDrop::NotAroundCursor)?;
        let end = start + length as usize;
        let text = state
            .text
            .get(start..end)
            .ok_or(RegionDrop::NotAroundCursor)?;
        let shown = &self.preedit;
        if text.is_empty() {
            let txn = match shown.is_empty() {
                true => GuestTxn {
                    delete: Some((0, 0)),
                    ..GuestTxn::default()
                },
                false => GuestTxn {
                    commit: Some(state.text[shown.clone()].to_owned()),
                    ..GuestTxn::default()
                },
            };
            return Ok(txn);
        }
        if shown.start < start || end < shown.end {
            return Err(RegionDrop::Uncovered);
        }
        // Both are at most the region's length.
        let delete = ((shown.start - start) as u32, (end - shown.end) as u32);
        let before = exo_cursor.clamp(start, end) - start;
        let cursor = assembly.cursor.or(Some(before as i32));
        let cursor = resolve_preedit_cursor(text, assembly.selection, cursor);
        let preedit = Preedit::new(text.to_owned(), cursor);
        let txn = GuestTxn {
            delete: Some(delete),
            preedit,
            ..GuestTxn::default()
        };
        Ok(txn)
    }

    /// H9: Exo deletes `length` bytes from `index` of its text, which holds
    /// no composition (IMA 1090-1102), around its cursor, the guest's.
    /// Returns the lengths before and after the cursor.
    pub(super) fn delete(&self, index: i32, length: u32) -> Result<(u32, u32), DeleteDrop> {
        let cursor = self.preedit.start;
        let start = usize::try_from(index).map_err(|_| DeleteDrop::NotAroundCursor)?;
        let end = start + length as usize;
        let around = self.preedit.is_empty() && start <= cursor && cursor <= end;
        if !around || self.state.text.get(start..end).is_none() {
            return Err(DeleteDrop::NotAroundCursor);
        }
        match ((cursor - start) as u32, (end - cursor) as u32) {
            (0, 0) => Err(DeleteDrop::Empty),
            lengths => Ok(lengths),
        }
    }
}
