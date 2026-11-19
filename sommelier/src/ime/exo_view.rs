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
//! regions (X1) in Exo's own text (`docs/IME_REDESIGN.md` 2.2).

use std::collections::VecDeque;
use std::ops::Range;

use super::guest::{resolve_preedit_cursor, GuestTxn, Preedit};
use super::mirror::{Applied, ExoMirror, ExoState, Health, Sent};
use super::state::{Assembly, DeleteDrop, RegionDrop};
use super::{BarrierToken, HostIme, ImeMode, TiId};
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
    mode: ImeMode,
    mirror: ExoMirror,
    /// The guest's text is Exo's predicted text without the composition
    /// Exo holds, which the guest shows as its preedit, with its cursor
    /// where Exo predicts it (design exo-mirror 3.1), but for `unseen`.
    /// Only a report sets it: one Exo has processed, or one the core
    /// withheld.
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
    /// it out of step.
    reports: VecDeque<(BarrierToken, Option<Edits>, bool)>,
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
/// which the event is measured (design exo-mirror 3.3).
#[derive(Clone, Debug)]
pub(super) struct Frame {
    state: ExoState,
    /// The composition Exo holds, which the guest shows as its preedit; or
    /// else empty, at Exo's cursor, which is the guest's.
    held: Range<usize>,
}

impl ExoView {
    pub(super) fn new(mode: ImeMode, ext_version: Option<u32>) -> Self {
        Self {
            mode,
            mirror: ExoMirror::new(ext_version),
            in_step: false,
            unseen: false,
            edits: Edits::default(),
            at_commit: Edits::default(),
            reports: VecDeque::new(),
        }
    }

    pub(super) fn mode(&self) -> ImeMode {
        self.mode
    }

    /// Whether a frame measures the next event (design exo-mirror 3.1).
    #[cfg(test)]
    pub(super) fn in_step(&self) -> bool {
        self.in_step && !self.unseen
    }

    #[cfg(test)]
    pub(super) fn mirror(&self) -> &ExoMirror {
        &self.mirror
    }

    /// The frame of the next host event, before the mirror takes it: Exo's
    /// state, if a guest that shows `shown` can be in step with it.
    pub(super) fn frame(&self, shown: Option<&Preedit>) -> Option<Frame> {
        let state = self.mirror.predicted();
        let held = held(state, shown)?;
        Some(Frame {
            state: state.clone(),
            held,
        })
    }

    /// Whether the frame taken before the event just observed measures
    /// it: the guest was in step with it, the mirror stayed exact, and no
    /// report on its way to Exo crossed the event (design exo-mirror 3.3,
    /// "Crossings").
    pub(super) fn measures(&self) -> bool {
        self.in_step
            && !self.unseen
            && self.mirror.health() == Health::Exact
            && !self.mirror.report_crosses()
    }

    /// The mirror takes host event `event`, before the gate: stale events
    /// and those behind a fence changed Exo's tracker too.
    pub(super) fn observe(&mut self, ti: TiId, event: &HostIme) {
        let health = self.mirror.health();
        let mirror = &mut self.mirror;
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
                IndexedString::NotUtf8(_) => mirror.uncertain(),
            },
            HostIme::Commit { text, .. } => mirror.commit_string(text),
            &HostIme::Delete { index, length } => mirror.delete_surrounding_text(index, length),
            &HostIme::PreeditRegion { index, length } => mirror.set_preedit_region(index, length),
            &HostIme::Confirm { keep_selection } => mirror.confirm_preedit(keep_selection),
        }
        self.health_changed(ti, health);
    }

    /// After a tracker-changing host event, with `frame` the frame taken
    /// before it, `edit` what reached the guest, if anything did, and
    /// `shown` the preedit the guest shows now: whether the event's
    /// transaction keeps a guest in step with Exo if it was, which the new
    /// state records, and so whether the guest still is (design exo-mirror
    /// 3.2).
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
            let cursor = frame.held.start.checked_sub(edit.deleted_before as usize)?;
            Some(cursor + edit.committed)
        });
        let state = self.mirror.predicted();
        let keeps = self.mirror.health() == Health::Exact
            && cursor
                .is_some_and(|cursor| held(state, shown).map(|held| held.start) == Some(cursor));
        self.in_step &= keeps;
        if let Some(edit) = edit {
            self.edits.from_exo += u64::from(edit.deleted || edit.committed > 0);
        }
        self.mirror.mark(keeps);
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
    /// Exo in separate reads, Exo may process an event between them.
    pub(super) fn report_sent(
        &mut self,
        token: BarrierToken,
        (text, cursor, anchor): (String, u32, u32),
        one_read: bool,
        stated: bool,
    ) {
        if !one_read {
            self.mirror.uncertain();
        }
        self.mirror.sent(
            token,
            Sent::Report {
                text,
                cursor,
                anchor,
            },
        );
        let since = Edits {
            from_exo: self.at_commit.from_exo,
            unseen: self.edits.unseen,
        };
        self.reports
            .push_back((token, stated.then_some(since), false));
    }

    /// Report suppression (design exo-mirror 5): in Mirror mode, whether
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
        let withheld = (self.mode == ImeMode::Mirror)
            .then_some(shown)
            .flatten()
            .and_then(|shown| self.mirror.withholds(text, *cursor, *anchor, shown.text()));
        let Some(in_step) = withheld else {
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
        self.mirror.sent(token, Sent::Cancel);
    }

    /// The barrier `token` is done. A report Exo processed puts the guest in
    /// step if it matched a state the guest is in step with, or reset Exo
    /// to the guest's text with no edit of Exo's since the guest's commit
    /// before it; either with no other edit since the report (design
    /// exo-mirror 3.2).
    pub(super) fn barrier_done(&mut self, ti: TiId, token: BarrierToken) {
        let health = self.mirror.health();
        let applied = self.mirror.barrier_done(token);
        let since = match self.reports.front() {
            Some(&(front, since, superseded)) if front == token => {
                self.reports.pop_front();
                Some((since, superseded))
            }
            _ => None,
        };
        let exact = self.mirror.health() == Health::Exact;
        let Some((since, superseded)) = since else {
            return self.health_changed(ti, health);
        };
        let Some(since) = since else {
            // A stand-in: Exo holds empty text, the guest perhaps not.
            self.in_step = false;
            self.mirror.mark(false);
            return self.health_changed(ti, health);
        };
        let was = self.in_step || !superseded;
        match applied {
            Some(Applied::Matched { in_step }) => self.in_step = was && exact && in_step,
            Some(Applied::Reset { cursor }) => {
                self.in_step = was && exact && cursor && self.edits.from_exo == since.from_exo;
                self.mirror.mark(self.in_step);
            }
            Some(Applied::Cancel) | None => {}
        }
        // The report states every key and press before it.
        self.unseen &= self.edits.unseen != since.unseen;
        self.health_changed(ti, health);
    }

    fn health_changed(&self, ti: TiId, before: Health) {
        let health = self.mirror.health();
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
fn held(state: &ExoState, shown: Option<&Preedit>) -> Option<Range<usize>> {
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
    /// Returns the transaction and where Exo's cursor is in its preedit.
    pub(super) fn region(
        &self,
        index: i32,
        length: u32,
        assembly: Assembly,
    ) -> Result<(GuestTxn, Option<u32>), RegionDrop> {
        let state = &self.state;
        let exo_cursor = state.selection.end;
        let start = usize::try_from(exo_cursor as i64 + i64::from(index))
            .map_err(|_| RegionDrop::NotAroundCursor)?;
        let end = start + length as usize;
        let text = state
            .text
            .get(start..end)
            .ok_or(RegionDrop::NotAroundCursor)?;
        let held = &self.held;
        if text.is_empty() {
            let txn = match held.is_empty() {
                true => GuestTxn {
                    delete: Some((0, 0)),
                    ..GuestTxn::default()
                },
                false => GuestTxn {
                    commit: Some(state.text[held.clone()].to_owned()),
                    ..GuestTxn::default()
                },
            };
            return Ok((txn, None));
        }
        if held.start < start || end < held.end {
            return Err(RegionDrop::Uncovered);
        }
        // Both are at most the region's length.
        let delete = ((held.start - start) as u32, (end - held.end) as u32);
        let before = exo_cursor.clamp(start, end) - start;
        let cursor = assembly.cursor.or(Some(before as i32));
        let cursor = resolve_preedit_cursor(text, assembly.selection, cursor);
        let preedit = Preedit::new(text.to_owned(), cursor);
        let txn = GuestTxn {
            delete: Some(delete),
            preedit,
            ..GuestTxn::default()
        };
        Ok((txn, Some(before as u32)))
    }

    /// H9: Exo deletes `length` bytes from `index` of its text, which holds
    /// no composition (IMA 1090-1102), around its cursor, the guest's.
    /// Returns the lengths before and after the cursor.
    pub(super) fn delete(&self, index: i32, length: u32) -> Result<(u32, u32), DeleteDrop> {
        let cursor = self.held.start;
        let start = usize::try_from(index).map_err(|_| DeleteDrop::NotAroundCursor)?;
        let end = start + length as usize;
        let around = self.held.is_empty() && start <= cursor && cursor <= end;
        if !around || self.state.text.get(start..end).is_none() {
            return Err(DeleteDrop::NotAroundCursor);
        }
        match ((cursor - start) as u32, (end - cursor) as u32) {
            (0, 0) => Err(DeleteDrop::Empty),
            lengths => Ok(lengths),
        }
    }
}
