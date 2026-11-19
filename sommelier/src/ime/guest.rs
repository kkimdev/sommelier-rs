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

//! What the guest is shown: the preedit, and the one encoder of a v3
//! transaction (B5 in the design), with the byte-range helpers its
//! transactions are measured with.

use super::serial::Serial;
use super::TiId;
use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
use crate::wire::Outgoing;

/// A preedit as the guest is shown it. Its text is never empty: an empty
/// preedit is no preedit, which [`Self::new`] makes impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Preedit {
    text: String,
    /// The `cursor_begin` and `cursor_end` sent with it.
    cursor: (i32, i32),
}

impl Preedit {
    /// The preedit `text` with `cursor`, or `None` if `text` is empty.
    pub(super) fn new(text: String, cursor: (i32, i32)) -> Option<Self> {
        (!text.is_empty()).then_some(Self { text, cursor })
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }
}

/// What the guest applies at one `done`, in the protocol's order: the
/// delete, the commit, then the preedit. `preedit: None` clears a preedit
/// the guest shows; what tells "clear" from "omit" is whether it showed
/// one (`shown_before` of [`Self::encode`]).
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GuestTxn {
    /// The bytes to delete before and after the cursor.
    pub(super) delete: Option<(u32, u32)>,
    pub(super) commit: Option<String>,
    pub(super) preedit: Option<Preedit>,
}

/// A string of a transaction does not fit one message.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TooLong;

impl GuestTxn {
    /// The only builder of `zwp_text_input_v3` `preedit_string`,
    /// `delete_surrounding_text`, `commit_string` and `done`, in mutter's
    /// and KWin's order:
    ///
    /// 1. an empty `preedit_string`, only to clear the preedit the guest
    ///    shows (`shown_before`) before an edit, or when no new one
    ///    follows;
    /// 2. `delete_surrounding_text`, only if it deletes something;
    /// 3. `commit_string`, if it commits something;
    /// 4. the new `preedit_string`;
    /// 5. `done(serial)`.
    ///
    /// Clients that follow the protocol apply a transaction at its `done`
    /// in that order whatever the wire order, but some apply events as
    /// they arrive: winit drops a preedit that precedes a commit, and SDL3
    /// ignores a delete while a preedit shows. An empty preedit over none
    /// makes GTK4 delete the selection, as do a zero-length delete in
    /// Chromium and an empty commit in GTK, so none of them is ever sent.
    /// A new preedit equal to the shown one is still sent: a `done`
    /// without one clears it.
    pub(super) fn encode(
        &self,
        ti: TiId,
        shown_before: bool,
        serial: Serial,
    ) -> Result<Vec<Outgoing>, TooLong> {
        use zwp_text_input_v3::evt;
        let ti = ti.0;
        let delete = self.delete.filter(|&lengths| lengths != (0, 0));
        let commit = self.commit.as_deref().filter(|text| !text.is_empty());
        let edits = delete.is_some() || commit.is_some();
        let mut events = Vec::new();
        if shown_before && (edits || self.preedit.is_none()) {
            events.push(evt::preedit_string(ti, Some(""), 0, 0).map_err(|_| TooLong)?);
        }
        if let Some((before, after)) = delete {
            events.push(evt::delete_surrounding_text(ti, before, after));
        }
        if let Some(text) = commit {
            events.push(evt::commit_string(ti, Some(text)).map_err(|_| TooLong)?);
        }
        if let Some(Preedit { text, cursor }) = &self.preedit {
            let preedit = evt::preedit_string(ti, Some(text), cursor.0, cursor.1);
            events.push(preedit.map_err(|_| TooLong)?);
        }
        events.push(evt::done(ti, serial.get()));
        Ok(events)
    }
}

/// The cursor of the preedit `text` from the host's `preedit_styling`
/// selection or `preedit_cursor`, which are byte offsets into it. An
/// invalid one is ignored, with a warning, and the cursor goes at the end.
/// An empty preedit's cursor is `(0, 0)`.
pub(super) fn resolve_preedit_cursor(
    text: &str,
    selection: Option<(u32, u32)>,
    cursor: Option<i32>,
) -> (i32, i32) {
    if text.is_empty() {
        return (0, 0);
    }
    if let Some((index, length)) = selection {
        let end = index.saturating_add(length);
        if end <= text.len() as u32
            && text.is_char_boundary(index as usize)
            && text.is_char_boundary(end as usize)
        {
            return (index as i32, end as i32);
        }
        log::warn!(
            "Ignoring invalid preedit selection: index={}, length={}, text_len={}",
            index,
            length,
            text.len()
        );
    } else if let Some(cursor) = cursor {
        if cursor < 0 {
            return (-1, -1);
        }
        if (cursor as usize) <= text.len() && text.is_char_boundary(cursor as usize) {
            return (cursor, cursor);
        }
        log::warn!(
            "Ignoring invalid preedit cursor: cursor={}, text_len={}",
            cursor,
            text.len()
        );
    }

    let end = text.len() as i32;
    (end, end)
}

/// Split the `length` bytes of `text` from byte `start` into the
/// text-input-v3 lengths before and after `cursor`, the only form in which
/// v3 can delete them.
///
/// # Returns
///
/// `None` if the range does not contain the cursor or does not lie on
/// character boundaries of `text`.
pub(super) fn lengths_around_cursor(
    text: &str,
    cursor: i32,
    start: i64,
    length: u32,
) -> Option<(u32, u32)> {
    let start = usize::try_from(start).ok()?;
    let end = start.checked_add(usize::try_from(length).ok()?)?;
    let cursor = usize::try_from(cursor).ok()?;
    if !(start <= cursor && cursor <= end) || text.get(start..end).is_none() {
        return None;
    }
    // Both lengths are at most `length`.
    Some(((cursor - start) as u32, (end - cursor) as u32))
}

/// The text-input-v3 lengths of a v1 `delete_surrounding_text(index,
/// length)` in `text` around `cursor`. A negative `index` can only mean the
/// v1 XML's reading, relative to the cursor; Exo sends it non-negative,
/// counted from the start of the text (INVARIANTS "Exo's deletes are
/// absolute and stand alone").
///
/// # Returns
///
/// `None` as [`lengths_around_cursor`] does.
pub(crate) fn delete_lengths(
    text: &str,
    cursor: i32,
    index: i32,
    length: u32,
) -> Option<(u32, u32)> {
    let start = match index {
        ..0 => i64::from(cursor) + i64::from(index),
        _ => i64::from(index),
    };
    lengths_around_cursor(text, cursor, start, length)
}
