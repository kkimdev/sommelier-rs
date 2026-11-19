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

//! How a text input in Legacy mode measures Exo's deletes (H9) and
//! preedit regions (X1): against the surrounding text the guest last
//! committed, with what the core knows of the edits since
//! ([`ExoText`], `Composition::taken` and `Composition::held`). Every
//! text input measured this way before the Exo mirror, and one whose host
//! extension is older than v11 still does (`docs/IME_REDESIGN.md` 2.2).

use std::ops::Range;

use super::super::guest::{delete_lengths, lengths_around_cursor, resolve_preedit_cursor};
use super::super::guest::{GuestTxn, Preedit};
use super::super::LeaseOutcome;
use super::{Assembly, Current, DeleteDrop, Planned, RegionDrop};

/// Whether Exo's prediction of the text may hold input-method edits the
/// guest has not yet reported in a surrounding text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExoText {
    Synced,
    Ahead,
}

/// Exo's prediction of a field's text, in which it measures a region
/// (X1).
struct Predicted {
    text: String,
    /// Exo's cursor.
    cursor: usize,
    /// Where the preedit Exo holds lies in `text`; empty, at the guest's
    /// cursor, if none.
    held: Range<usize>,
}

impl<'a> Current<'a> {
    /// H9: a delete measured against the surrounding text the guest last
    /// committed (INVARIANTS "Exo's deletes are absolute and stand alone").
    pub(in crate::ime) fn delete(self, index: i32, length: u32) -> Result<Planned<'a>, DeleteDrop> {
        let Some((text, cursor, anchor)) = &self.committed.surrounding_text else {
            return Err(DeleteDrop::NoSurroundingText);
        };
        if cursor != anchor {
            return Err(DeleteDrop::Selection);
        }
        if index >= 0 && *self.text == ExoText::Ahead {
            return Err(DeleteDrop::TextAhead);
        }
        let lengths =
            delete_lengths(text, *cursor, index, length).ok_or(DeleteDrop::NotAroundCursor)?;
        if lengths == (0, 0) {
            return Err(DeleteDrop::Empty);
        }
        let txn = GuestTxn {
            delete: Some(lengths),
            ..GuestTxn::default()
        };
        Ok(self.plan(txn, true, None, LeaseOutcome::End))
    }

    /// X1: Exo's cursor-relative region of its text becomes the preedit.
    /// It is not gated by the text being ahead, on purpose (INVARIANTS
    /// "Exo's deletes are absolute and stand alone"). The region must
    /// cover the preedit Exo holds, which the guest does not have as text,
    /// and deletes only the bytes beyond it; an empty one at Exo's cursor
    /// after that preedit commits it.
    pub(in crate::ime) fn region(
        self,
        index: i32,
        length: u32,
        assembly: Assembly,
    ) -> Result<Planned<'a>, RegionDrop> {
        let Some((text, cursor, _)) = &self.committed.surrounding_text else {
            return Err(RegionDrop::NoSurroundingText);
        };
        let exo = self
            .predicted(text, *cursor)
            .ok_or(RegionDrop::NotAroundCursor)?;
        let exo_cursor = i32::try_from(exo.cursor).map_err(|_| RegionDrop::NotAroundCursor)?;
        let start = i64::from(exo_cursor) + i64::from(index);
        let (before, after) = lengths_around_cursor(&exo.text, exo_cursor, start, length)
            .ok_or(RegionDrop::NotAroundCursor)?;
        let region = exo.cursor - before as usize..exo.cursor + after as usize;
        let held = &exo.held;
        // An empty region ends Exo's composition and leaves the preedit it
        // held as text, as a confirmation does, so the guest commits it.
        // Only with Exo's cursor after that preedit, where the guest's is
        // after a commit.
        if region.is_empty() && !held.is_empty() && held.end == exo.cursor {
            return Ok(self.commit_shown());
        }
        // The guest does not have the preedit Exo holds as text, so it is
        // not deleted. A region that does not cover it would leave its
        // rest lost or shown twice.
        let (Some(delete_before), Some(delete_after)) = (
            held.start.checked_sub(region.start),
            region.end.checked_sub(held.end),
        ) else {
            return Err(RegionDrop::Uncovered);
        };
        // Both are at most the region's length.
        let deleted = (delete_before as u32, delete_after as u32);
        let preedit_text = exo.text[region].to_string();
        let cursor = assembly.cursor.or(Some(before as i32));
        let cursor = resolve_preedit_cursor(&preedit_text, assembly.selection, cursor);
        // An empty region deletes nothing and leaves no preedit, so the
        // guest has no edit to report. The guest no longer has the bytes
        // a region deletes, which the committed text still holds.
        let ahead = !preedit_text.is_empty();
        let (taken_before, taken_after) = self.composition.taken.unwrap_or_default();
        let taken = (taken_before + deleted.0, taken_after + deleted.1);
        let taken = (taken != (0, 0)).then_some(taken);
        let preedit = Preedit::new(preedit_text, cursor);
        let held = preedit.as_ref().map(|_| before);
        let txn = GuestTxn {
            delete: Some(deleted),
            preedit,
            ..GuestTxn::default()
        };
        let mut planned = self.plan(txn, ahead, taken, LeaseOutcome::End);
        planned.held = held;
        Ok(planned)
    }

    /// Exo's prediction of the text, given the committed surrounding
    /// `text` and `cursor`, as far as the core knows it: the guest's text,
    /// which is the committed one without the bytes regions took, with the
    /// preedit Exo holds at the guest's cursor. `None` if the cursor, or
    /// the bytes taken around it, do not split `text` on character
    /// boundaries.
    fn predicted(&self, text: &str, cursor: i32) -> Option<Predicted> {
        let (before, after) = self.composition.taken.unwrap_or_default();
        let cursor = usize::try_from(cursor).ok()?;
        let start = cursor.checked_sub(before as usize)?;
        let end = cursor.checked_add(after as usize)?;
        let (head, tail) = (text.get(..start)?, text.get(end..)?);
        let (held, at) = match (&self.composition.shown, self.composition.held) {
            (Some(shown), Some(at)) => (shown.text(), at as usize),
            _ => ("", 0),
        };
        Some(Predicted {
            text: [head, held, tail].concat(),
            cursor: start + at,
            held: start..start + held.len(),
        })
    }
}
