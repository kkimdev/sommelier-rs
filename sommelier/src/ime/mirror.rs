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

//! The Exo mirror: a replica of the `ui::SurroundingTextTracker` that Exo
//! keeps for one host `zwp_text_input_v1`, driven only by the wire. It is
//! pure: the core feeds it every request it sends and every event Exo
//! sends, and reads it to measure Exo's deletes and preedit regions
//! (`docs/IME_REDESIGN.md` 2.2, X1 and H9).
//!
//! Sources, all at Chromium `main` **`6975bb48a8cd`** (2026-11-19), which
//! line numbers below refer to: `ui/base/ime/surrounding_text_tracker.cc`
//! (STT; last changed by `a89e336612fb`, 2024-10-22),
//! `components/exo/text_input.cc` (TI),
//! `components/exo/wayland/zwp_text_input_manager.cc` (ZTIM) and
//! `ui/gfx/range/range.h`.
//!
//! The STT functions are ported line for line, with two differences that
//! hold for this client alone:
//!
//! - **Bytes, not UTF-16.** Every offset on the wire is a UTF-8 offset
//!   into the text Exo sent it for (ZTIM converts each one at a code
//!   point), and two valid UTF-16 strings are equal exactly when their
//!   UTF-8 forms are. A position off a char boundary fails the echo
//!   check instead.
//! - **No offset and no commands.** The proxy never sends
//!   `set_surrounding_text_offset_utf16` or `set_large_surrounding_text`,
//!   so a report's offset is 0 (ZTIM 506-545). The tracker's own
//!   `utf16_offset` then stays 0 too: it moves only in the "range outside
//!   the text" branches of `OnSetCompositionText` (STT 210-214) and
//!   `OnInsertText` (STT 301-306), and every range stays inside the text
//!   (TI 386-403, 483-508; here, the echo checks). With offset 0 every
//!   `Update` match is exact, the replay branch (STT 165-189) is
//!   unreachable, and the history needs states only.
//!
//! Exo maps each tracker call to one wire event ([`ExoMirror`]'s methods
//! name theirs). The proxy's own requests take effect where Exo processed
//! them, which the `wl_display.sync` sent right after each one reveals
//! ([`ExoMirror::sent`], [`ExoMirror::barrier_done`]).

use std::collections::VecDeque;
use std::ops::Range;

use super::BarrierToken;
use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1 as ext;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;

/// The most history states the mirror keeps. Evicting one makes it
/// [`Health::Uncertain`], since Exo still has it and could match it.
const HISTORY_CAP: usize = 64;

/// The longest text the mirror keeps, in bytes. A longer one makes it
/// [`Health::Uncertain`] and blind until a report resets it; no report can
/// be that long anyway, as it must fit one wire message.
const TEXT_CAP: usize = 4096;

/// A `gfx::Range`: directed, so `start` may lie after `end`, and empty
/// when they are equal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Span {
    pub(super) start: usize,
    pub(super) end: usize,
}

impl Span {
    pub(super) const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// `gfx::Range(pos)`: an empty range, a cursor.
    pub(super) const fn at(pos: usize) -> Self {
        Self::new(pos, pos)
    }

    fn forward(range: &Range<usize>) -> Self {
        Self::new(range.start, range.end)
    }

    pub(super) fn min(self) -> usize {
        self.start.min(self.end)
    }

    pub(super) fn max(self) -> usize {
        self.start.max(self.end)
    }

    pub(super) fn is_empty(self) -> bool {
        self.start == self.end
    }

    fn len(self) -> usize {
        self.max() - self.min()
    }

    /// `IsBoundedBy` (range.h 119-122).
    fn is_bounded_by(self, other: Self) -> bool {
        self.min() >= other.min() && self.max() <= other.max()
    }

    /// `Contains` (range.h 111-115): a non-empty range does not contain
    /// the empty range at its end.
    fn contains(self, other: Self) -> bool {
        other.is_bounded_by(self)
            && (other.max() != self.max() || other.is_empty() == self.is_empty())
    }

    /// `Intersects`, which is `Intersect(range).IsValid()` (range.h
    /// 106-108, 126-131).
    fn intersects(self, other: Self) -> bool {
        let min = self.min().max(other.min());
        let max = self.max().min(other.max());
        min < max || self.contains(other) || other.contains(self)
    }

    /// The smallest range that covers both, forward.
    fn union(self, other: Self) -> Self {
        Self::new(self.min().min(other.min()), self.max().max(other.max()))
    }
}

/// No composition. STT reads an empty composition's position nowhere, so
/// the mirror keeps every empty one as this.
const NO_COMPOSITION: Range<usize> = 0..0;

/// `range`, or [`NO_COMPOSITION`] if it is empty.
fn composition(range: Range<usize>) -> Range<usize> {
    if range.is_empty() {
        NO_COMPOSITION
    } else {
        range
    }
}

/// One `SurroundingTextTracker::State`, in UTF-8 bytes, with its offset
/// always 0 (module doc).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ExoState {
    pub(super) text: String,
    /// `selection`, directed (`OnSetEditableSelectionRange` keeps a range
    /// as given). Empty is a cursor.
    pub(super) selection: Span,
    /// `composition`, forward; [`NO_COMPOSITION`] if there is none.
    pub(super) composition: Range<usize>,
}

impl ExoState {
    /// `GetCompositionText` (STT 85-100): empty with no composition.
    pub(super) fn composition_text(&self) -> &str {
        &self.text[self.composition.clone()]
    }

    /// Whether `pos` is a position in the text, on a char boundary.
    fn holds(&self, pos: usize) -> bool {
        self.text.is_char_boundary(pos)
    }

    /// Whether both ends of `span` are positions in the text.
    fn holds_span(&self, span: Span) -> bool {
        self.holds(span.start) && self.holds(span.end)
    }

    /// What the STT header requires of a state, "Must be fit in
    /// |surrounding_text| range", on char boundaries; and an empty
    /// composition is [`NO_COMPOSITION`].
    #[cfg(test)]
    fn fits(&self) -> bool {
        let composition = &self.composition;
        self.holds_span(self.selection)
            && composition.start <= composition.end
            && self.holds_span(Span::forward(composition))
            && (!composition.is_empty() || *composition == NO_COMPOSITION)
    }
}

/// `InsertTextCursorBehavior`. Exo always passes
/// [`Self::MoveCursorAfterText`] (TI 274-280).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorBehavior {
    MoveCursorAfterText,
    #[cfg(test)]
    MoveCursorBeforeText,
}

/// `SurroundingTextTracker::UpdateResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateResult {
    /// A history state matched: the states before it are gone, and the
    /// prediction stays.
    Updated,
    /// None matched: the report replaced the prediction and the history.
    Reset,
}

/// A request the proxy sent Exo that changes its tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Sent {
    /// v1 `set_surrounding_text(text, cursor, anchor)`: `Update`.
    Report {
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// v1 `reset`: `CancelComposition` (TI 152-156).
    Cancel,
}

/// Whether the mirror is known to equal Exo's tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Health {
    Exact,
    /// Something the mirror cannot see may have happened: a history state
    /// was evicted, the text grew over [`TEXT_CAP`], a composition had a
    /// selection, a report may have been read apart from its barrier, or
    /// an event could be read two ways; and a host without the extension
    /// composes existing text with no event (`SetCompositionFromExistingText`,
    /// TI 483-508), so its mirror starts so. Clears only at a reset point
    /// (see [`Self::Healing`]).
    Uncertain,
    /// An event did not agree with the mirror's state (an echo check
    /// failed), and the mirror did not apply it. Clears only at a reset
    /// point.
    Diverged,
    /// A `reset` emptied Exo's history (`CancelComposition`, STT 124-129)
    /// while the mirror was unhealthy. The next report Exo processes then
    /// matches nothing and resets Exo to it, which makes the mirror
    /// [`Self::Exact`]. A tracker-changing event before that report could
    /// be matched instead, so it makes the mirror [`Self::Uncertain`]
    /// until the next `reset`.
    Healing,
}

/// What a request did once Exo processed it ([`ExoMirror::barrier_done`]),
/// which tells where the guest's report put the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// A report matched a history state: the guest is at the first equal
    /// state with no composition from there, and `in_step` if it has no
    /// selection and every transaction after it kept the guest in step.
    Matched { in_step: bool },
    /// A report matched none and reset Exo to it; `cursor` if its
    /// selection is empty.
    Reset { cursor: bool },
    /// A `reset` emptied the history.
    Cancel,
}

/// Exo's tracker for one host text input, as the wire shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExoMirror {
    /// `predicted_state_` after every message Exo has processed so far.
    predicted: ExoState,
    /// `expected_updates_`, states only, oldest first, at most
    /// [`HISTORY_CAP`]; each with whether the transaction that led to it
    /// kept a guest in step with Exo if it was ([`Self::mark`]).
    expected: VecDeque<(ExoState, bool)>,
    /// The newest state has not been marked yet.
    unmarked: bool,
    /// Requests sent and not yet known to be processed, each with the
    /// barrier sent after it, oldest first.
    in_flight: VecDeque<(BarrierToken, Sent)>,
    /// The `preedit_cursor` since the last `preedit_string`.
    preedit_cursor: Option<i32>,
    /// The highlight `preedit_styling` spans since the last
    /// `preedit_string`.
    highlights: Vec<Span>,
    /// The `cursor_position(index, anchor)` since the last
    /// `commit_string`.
    cursor_position: Option<(i32, i32)>,
    /// The last report reset a non-empty composition (E4), and nothing
    /// has composed since.
    dropped: bool,
    health: Health,
    /// The text grew over [`TEXT_CAP`]: the mirror keeps no text and
    /// applies no event until a report resets it.
    blind: bool,
    /// The host extension's version, `None` without one.
    ext_version: Option<u32>,
}

impl ExoMirror {
    /// The tracker of a new host text input (`SurroundingTextTracker()`,
    /// STT 114-116).
    pub(super) fn new(ext_version: Option<u32>) -> Self {
        let mut mirror = Self {
            predicted: ExoState::default(),
            expected: VecDeque::new(),
            unmarked: false,
            in_flight: VecDeque::new(),
            preedit_cursor: None,
            highlights: Vec::new(),
            cursor_position: None,
            dropped: false,
            health: match ext_version {
                Some(_) => Health::Exact,
                None => Health::Uncertain,
            },
            blind: false,
            ext_version,
        };
        mirror.reset_internal("", Span::at(0));
        mirror
    }

    pub(super) fn predicted(&self) -> &ExoState {
        &self.predicted
    }

    pub(super) fn expected(&self) -> &VecDeque<(ExoState, bool)> {
        &self.expected
    }

    /// Records whether the transaction that led to the newest state kept a
    /// guest in step, if no state has been marked since it was pushed.
    pub(super) fn mark(&mut self, in_step: bool) {
        if std::mem::take(&mut self.unmarked) {
            if let Some((_, step)) = self.expected.back_mut() {
                *step = in_step;
            }
        }
    }

    /// Something the mirror cannot see may have happened.
    pub(super) fn uncertain(&mut self) {
        self.degrade(Health::Uncertain);
    }

    /// Whether a report the proxy sent and Exo has yet to process will
    /// match no state Exo has now, or follows a `reset` that will empty
    /// the history: the guest made an edit Exo has yet to see.
    pub(super) fn report_crosses(&self) -> bool {
        let mut from = 0;
        for (at, (_, sent)) in self.in_flight.iter().enumerate() {
            let Sent::Report {
                text,
                cursor,
                anchor,
            } = sent
            else {
                let mut rest = self.in_flight.iter().skip(at);
                return rest.any(|(_, sent)| matches!(sent, Sent::Report { .. }));
            };
            let Some(selection) = report_selection(text, *cursor, *anchor) else {
                continue;
            };
            let matches =
                |(state, _): &(ExoState, bool)| state.selection == selection && state.text == *text;
            match self.expected.iter().skip(from).position(matches) {
                Some(at) => from += at,
                None => return true,
            }
        }
        false
    }

    /// Report suppression (design exo-mirror 5): whether an exact mirror
    /// finds that the guest's report `(text, cursor, anchor)`, from a guest
    /// that shows the preedit `shown`, would only make Exo drop its
    /// composition. It names no state of the history Exo will hold once it
    /// has processed every request in flight, but one such state is the
    /// guest's text with the composition `shown` inline, at the cursor.
    /// Then `Some` of whether every transaction since that state kept a
    /// guest in step. A report that names a state is a no-op for Exo, and
    /// any other resets it to the guest's text, so both go out.
    pub(super) fn withholds(
        &self,
        text: &str,
        cursor: u32,
        anchor: u32,
        shown: &str,
    ) -> Option<bool> {
        let selection = report_selection(text, cursor, anchor)?;
        if self.health != Health::Exact || !selection.is_empty() {
            return None;
        }
        let from = self.projected_from()?;
        let mut states = self.expected.iter().skip(from);
        if states
            .clone()
            .any(|(state, _)| state.selection == selection && state.text == text)
        {
            return None;
        }
        let cursor = selection.start;
        states.find(|(state, _)| {
            let composition = &state.composition;
            !composition.is_empty()
                && composition.start == cursor
                && state.composition_text() == shown
                && text.get(..cursor) == Some(&state.text[..cursor])
                && text.get(cursor..) == Some(&state.text[composition.end..])
        })?;
        Some(states.all(|&(_, in_step)| in_step))
    }

    /// Where the history Exo will hold once it has processed every request
    /// in flight starts in `expected`, of which it is a suffix: every event
    /// so far came before them. `None` if one of them will reset it or
    /// empty it.
    fn projected_from(&self) -> Option<usize> {
        let mut from = 0;
        for (_, sent) in &self.in_flight {
            let Sent::Report {
                text,
                cursor,
                anchor,
            } = sent
            else {
                return None;
            };
            let Some(selection) = report_selection(text, *cursor, *anchor) else {
                continue;
            };
            let matches =
                |(state, _): &(ExoState, bool)| state.selection == selection && state.text == *text;
            from += self.expected.iter().skip(from).position(matches)?;
        }
        Some(from)
    }

    pub(super) fn health(&self) -> Health {
        self.health
    }

    pub(super) fn dropped(&self) -> bool {
        self.dropped
    }

    // The wire. Each event Exo sends is matched to the tracker call that
    // sent it, and applied if it agrees with the mirror (the echo check).

    /// H4 `preedit_styling`: a span of the next `preedit_string`. A
    /// highlight is IMA's composition selection (ZTIM 166-182; IMA
    /// `ExtractCompositionText`), whose end the wire does not carry.
    pub(super) fn preedit_styling(&mut self, index: u32, length: u32, style: u32) {
        if style == zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT && length > 0 {
            let start = index as usize;
            self.highlights
                .push(Span::new(start, start.saturating_add(length as usize)));
        }
    }

    /// H5 `preedit_cursor`: the first half of `SetCompositionText` (ZTIM
    /// 166-182).
    pub(super) fn preedit_cursor(&mut self, index: i32) {
        self.preedit_cursor = Some(index);
    }

    /// H7 `preedit_string(text, commit)`: `TI::SetCompositionText` →
    /// `OnSetCompositionText` (TI 235-238; ZTIM 166-182). The cursor is
    /// the composition's selection start; its end is not on the wire, so
    /// the mirror's is empty. `ClearCompositionText` sends an empty one
    /// (TI 265-272), which gives the state `OnClearCompositionText` does;
    /// and IMA's empty `UpdateCompositionText` sends one with no
    /// composition, which deletes the selection.
    ///
    /// Echo: `commit` is `text`, and the cursor is a char boundary of it.
    /// A highlight with one end at the cursor and the other elsewhere is a
    /// composition selection the mirror cannot know, which makes it
    /// [`Health::Uncertain`].
    pub(super) fn preedit_string(&mut self, text: &str, commit: &str) {
        let cursor = self.preedit_cursor.take();
        let highlights = std::mem::take(&mut self.highlights);
        let at = cursor.map_or(Ok(text.len()), usize::try_from);
        if highlights
            .iter()
            .any(|span| at.is_ok_and(|at| span.start == at || span.end == at))
        {
            self.uncertain();
        }
        self.observe(|mirror| {
            let cursor = match cursor {
                None => text.len(),
                Some(cursor) => match usize::try_from(cursor) {
                    Ok(cursor) if text.is_char_boundary(cursor) => cursor,
                    _ => return false,
                },
            };
            if commit != text {
                return false;
            }
            mirror.dropped = false;
            mirror.on_set_composition_text(text, Span::at(cursor));
            true
        });
    }

    /// H6 `cursor_position(index, anchor)`: `SetCursor` (ZTIM 192-199),
    /// which the next `commit_string` completes.
    pub(super) fn cursor_position(&mut self, index: i32, anchor: i32) {
        self.cursor_position = Some((index, anchor));
    }

    /// H8 `commit_string(text)`. Alone, it is `TI::InsertText` →
    /// `OnInsertText(text, kMoveCursorAfterText)` (TI 274-280). After a
    /// `cursor_position(range)`, it is one of two pairs, both of which
    /// commit the composition text (TI 240-263, 386-403):
    ///
    /// - `ConfirmCompositionText(true)`'s fallback, which IMA takes only
    ///   below the extension's `confirm_preedit` (ZTIM 430-447), and which
    ///   sends the predicted selection;
    /// - `SetEditableSelectionRange(range)`, which keeps the composition.
    ///
    /// The pair is read as a confirm only when it can be one; one that can
    /// be either while composing makes the mirror [`Health::Uncertain`].
    ///
    /// Echo, for a pair: the range is in the text, and `text` is the
    /// composition's.
    pub(super) fn commit_string(&mut self, text: &str) {
        let cursor_position = self.cursor_position.take();
        self.observe(|mirror| {
            let Some((index, anchor)) = cursor_position else {
                mirror.on_insert_text(text, CursorBehavior::MoveCursorAfterText);
                return true;
            };
            let (Ok(start), Ok(end)) = (usize::try_from(anchor), usize::try_from(index)) else {
                return false;
            };
            let range = Span::new(start, end);
            let predicted = &mirror.predicted;
            if !predicted.holds_span(range) || text != predicted.composition_text() {
                return false;
            }
            let confirms_by_event = mirror
                .ext_version
                .is_some_and(|version| version >= ext::EVT_CONFIRM_PREEDIT_SINCE);
            if !confirms_by_event && range == predicted.selection {
                if !predicted.composition.is_empty() {
                    mirror.uncertain();
                }
                mirror.on_confirm_composition_text(true);
            } else {
                mirror.on_set_editable_selection_range(range);
            }
            true
        });
    }

    /// X2 `confirm_preedit`: `ConfirmComposition` →
    /// `OnConfirmCompositionText` (TI 240-263; ZTIM 430-447), keeping the
    /// selection for `unchanged`.
    pub(super) fn confirm_preedit(&mut self, keep_selection: bool) {
        self.observe(|mirror| {
            mirror.on_confirm_composition_text(keep_selection);
            true
        });
    }

    /// H9 `delete_surrounding_text(index, length)`:
    /// `ExtendSelectionAndDelete` → `OnExtendSelectionAndDelete` (TI
    /// 434-449). ZTIM sends the absolute range TI clamped to the text
    /// (ZTIM 201-213), which gives back `before` and `after` exactly.
    ///
    /// Echo: the range is in the text and contains the selection, and
    /// there is no composition, as IMA never deletes while composing (IMA
    /// 1090-1102).
    pub(super) fn delete_surrounding_text(&mut self, index: i32, length: u32) {
        self.observe(|mirror| {
            let Ok(start) = usize::try_from(index) else {
                return false;
            };
            let end = start + length as usize;
            let predicted = &mirror.predicted;
            let selection = predicted.selection;
            if !predicted.composition.is_empty()
                || !predicted.holds_span(Span::new(start, end))
                || start > selection.min()
                || end < selection.max()
            {
                return false;
            }
            mirror.on_extend_selection_and_delete(selection.min() - start, end - selection.max());
            true
        });
    }

    /// X1 `set_preedit_region(index, length)`:
    /// `SetCompositionFromExistingText` → `OnSetCompositionFromExistingText`
    /// (TI 483-508). ZTIM measures `index` from the selection's directed
    /// end (ZTIM 248-268). An empty region ends the composition.
    ///
    /// Echo: the range is in the text (IMA's `text_range.Contains`).
    pub(super) fn set_preedit_region(&mut self, index: i32, length: u32) {
        self.observe(|mirror| {
            let start = mirror.predicted.selection.end as i64 + i64::from(index);
            let Ok(start) = usize::try_from(start) else {
                return false;
            };
            let end = start + length as usize;
            if !mirror.predicted.holds_span(Span::new(start, end)) {
                return false;
            }
            mirror.dropped = false;
            mirror.on_set_composition_from_existing_text(start..end);
            true
        });
    }

    /// Applies one tracker-changing event: `apply` checks it against the
    /// mirror and changes the mirror only if it agrees.
    fn observe(&mut self, apply: impl FnOnce(&mut Self) -> bool) {
        self.unmarked = false;
        if self.health == Health::Healing {
            self.health = Health::Uncertain;
        }
        if !self.blind && !apply(self) {
            self.degrade(Health::Diverged);
        }
    }

    // The proxy's requests.

    /// The proxy sent `sent`, then the barrier `token`.
    pub(super) fn sent(&mut self, token: BarrierToken, sent: Sent) {
        self.in_flight.push_back((token, sent));
    }

    /// The barrier `token` is done, so Exo has processed the request
    /// before it, after every event that came before this. Returns what
    /// the request did; barriers the mirror did not queue are not its, and
    /// a report Exo ignores did nothing.
    pub(super) fn barrier_done(&mut self, token: BarrierToken) -> Option<Applied> {
        self.unmarked = false;
        if self
            .in_flight
            .front()
            .is_none_or(|&(front, _)| front != token)
        {
            return None;
        }
        let (_, sent) = self.in_flight.pop_front()?;
        match sent {
            Sent::Report {
                text,
                cursor,
                anchor,
            } => self.report(&text, cursor, anchor),
            Sent::Cancel => {
                self.cancel_composition();
                // Without the extension, Exo may compose existing text
                // unseen at any time, so nothing heals the mirror.
                if self.health != Health::Exact && self.ext_version.is_some() {
                    self.health = Health::Healing;
                }
                Some(Applied::Cancel)
            }
        }
    }

    /// `set_surrounding_text`, as Exo processes it: `SetSurroundingTextImpl`
    /// (ZTIM 506-545) takes `minmax(cursor, anchor)` as the selection, and
    /// ignores the whole request if an offset does not convert; then
    /// `TI::SetSurroundingText` calls `Update` (TI 158-175).
    fn report(&mut self, text: &str, cursor: u32, anchor: u32) -> Option<Applied> {
        let selection = report_selection(text, cursor, anchor)?;
        let composing = !self.predicted.composition.is_empty();
        match self.update(text, selection) {
            UpdateResult::Updated => {
                // A v3 guest reports no preedit, so it is at the first
                // equal state with no composition, not at one whose
                // composition it holds as text.
                let mut states = self.expected.iter().skip_while(|(state, _)| {
                    !state.composition.is_empty()
                        || state.selection != selection
                        || state.text != text
                });
                let at = states.next();
                let in_step =
                    at.is_some() && selection.is_empty() && states.all(|&(_, in_step)| in_step);
                Some(Applied::Matched { in_step })
            }
            UpdateResult::Reset => {
                self.dropped |= composing;
                if self.health == Health::Healing {
                    self.health = Health::Exact;
                }
                Some(Applied::Reset {
                    cursor: selection.is_empty(),
                })
            }
        }
    }

    /// Marks the mirror unhealthy; a divergence outranks an uncertainty.
    fn degrade(&mut self, to: Health) {
        if self.health != Health::Diverged {
            self.health = to;
        }
    }

    // `SurroundingTextTracker`, line for line, in bytes with offset 0.

    /// `expected_updates_.emplace_back(predicted_state_, …)`, within the
    /// caps.
    fn push(&mut self) {
        if self.predicted.text.len() > TEXT_CAP {
            self.degrade(Health::Uncertain);
            self.blind = true;
            self.predicted = ExoState::default();
            self.expected.clear();
            return;
        }
        self.expected.push_back((self.predicted.clone(), false));
        self.unmarked = true;
        if self.expected.len() > HISTORY_CAP {
            self.expected.pop_front();
            self.degrade(Health::Uncertain);
        }
    }

    /// `CancelComposition` (STT 124-129).
    fn cancel_composition(&mut self) {
        self.predicted.composition = NO_COMPOSITION;
        self.expected.clear();
    }

    /// `Update` (STT 131-191). With offset 0, `compare_begin` is 0, so a
    /// state matches when its selection and its whole text are equal, and
    /// the match is exact.
    fn update(&mut self, text: &str, selection: Span) -> UpdateResult {
        let matches =
            |(state, _): &(ExoState, bool)| state.selection == selection && state.text == text;
        match self.expected.iter().position(matches) {
            Some(at) => {
                self.expected.drain(..at);
                UpdateResult::Updated
            }
            None => {
                self.reset_internal(text, selection);
                UpdateResult::Reset
            }
        }
    }

    /// `OnSetEditableSelectionRange` (STT 193-200).
    fn on_set_editable_selection_range(&mut self, range: Span) {
        self.predicted.selection = range;
        self.push();
    }

    /// `OnSetCompositionText` (STT 202-229), with the composition's text
    /// and its selection within that text.
    fn on_set_composition_text(&mut self, text: &str, selection: Span) {
        let predicted = &mut self.predicted;
        // If it has a composition text already, replace it. Otherwise,
        // replace (or insert) selected text.
        let old_range = if predicted.composition.is_empty() {
            predicted.selection
        } else {
            Span::forward(&predicted.composition)
        };
        let composition_begin = old_range.min();
        // The branch for a range outside the text is unreachable: every
        // range is in the text (module doc).
        replace(
            &mut predicted.text,
            composition_begin,
            old_range.len(),
            text,
        );
        predicted.selection = Span::new(
            composition_begin + selection.start,
            composition_begin + selection.end,
        );
        predicted.composition = composition(composition_begin..composition_begin + text.len());
        self.push();
    }

    /// `OnSetCompositionFromExistingText` (STT 231-239).
    fn on_set_composition_from_existing_text(&mut self, range: Range<usize>) {
        self.predicted.composition = composition(range);
        self.push();
    }

    /// `OnConfirmCompositionText` (STT 241-253).
    fn on_confirm_composition_text(&mut self, keep_selection: bool) {
        let predicted = &mut self.predicted;
        if !predicted.composition.is_empty() {
            if !keep_selection {
                predicted.selection = Span::at(predicted.composition.end);
            }
            predicted.composition = NO_COMPOSITION;
        }
        self.push();
    }

    /// `OnClearCompositionText` (STT 255-270). Exo sends it as an empty
    /// `preedit_string`, which [`Self::on_set_composition_text`] reads to
    /// the same state, so only tests call it.
    #[cfg(test)]
    fn on_clear_composition_text(&mut self) {
        let predicted = &mut self.predicted;
        if !predicted.composition.is_empty() {
            let composition = predicted.composition.clone();
            erase(&mut predicted.text, composition.start, composition.len());
            // Set selection to the position where composition existed.
            predicted.selection = Span::at(composition.start);
            predicted.composition = NO_COMPOSITION;
        }
        self.push();
    }

    /// `OnInsertText` (STT 272-326).
    fn on_insert_text(&mut self, text: &str, cursor_behavior: CursorBehavior) {
        let predicted = &mut self.predicted;
        let mut rewritten_range = predicted.selection;
        if !predicted.composition.is_empty() {
            // Cancel the current composition.
            let composition = Span::forward(&predicted.composition);
            if composition.intersects(rewritten_range) {
                // Selection and composition overlap: the whole union is
                // replaced by `text`.
                rewritten_range = composition.union(rewritten_range);
            } else {
                // Otherwise, remove the composition, and shift the range if
                // the composition comes before it.
                erase(&mut predicted.text, composition.min(), composition.len());
                if rewritten_range.min() > composition.min() {
                    rewritten_range = Span::new(
                        rewritten_range.start - composition.len(),
                        rewritten_range.end - composition.len(),
                    );
                }
            }
        }
        // The branch for a range outside the text is unreachable: every
        // range is in the text (module doc).
        replace(
            &mut predicted.text,
            rewritten_range.min(),
            rewritten_range.len(),
            text,
        );
        predicted.selection = match cursor_behavior {
            CursorBehavior::MoveCursorAfterText => Span::at(rewritten_range.min() + text.len()),
            #[cfg(test)]
            CursorBehavior::MoveCursorBeforeText => Span::at(rewritten_range.min()),
        };
        predicted.composition = NO_COMPOSITION;
        self.push();
    }

    /// `OnExtendSelectionAndDelete` (STT 328-372), with `before` and
    /// `after` in bytes.
    fn on_extend_selection_and_delete(&mut self, before: usize, after: usize) {
        let predicted = &mut self.predicted;
        let selection = predicted.selection;
        if before != 0 || after != 0 || !selection.is_empty() || !predicted.composition.is_empty() {
            let mut delete_range = Span::new(
                selection.min() - before.min(selection.min()),
                selection.max() + after,
            );
            if !predicted.composition.is_empty() {
                // Cancel the current composition.
                let composition = Span::forward(&predicted.composition);
                if composition.intersects(delete_range) {
                    // Widen the range to the whole composition.
                    delete_range = composition.union(delete_range);
                } else {
                    // Otherwise, remove the composition, and shift the
                    // range if the composition comes before it.
                    erase(&mut predicted.text, composition.min(), composition.len());
                    if delete_range.min() > composition.min() {
                        delete_range = Span::new(
                            delete_range.start - composition.len(),
                            delete_range.end - composition.len(),
                        );
                    }
                }
            }
            erase(&mut predicted.text, delete_range.min(), delete_range.len());
            predicted.selection = Span::at(delete_range.min());
            predicted.composition = NO_COMPOSITION;
        }
        self.push();
    }

    /// `ResetInternal` (STT 374-381).
    fn reset_internal(&mut self, text: &str, selection: Span) {
        self.blind = false;
        self.predicted = ExoState {
            text: text.to_owned(),
            selection,
            composition: NO_COMPOSITION,
        };
        self.expected.clear();
        self.push();
    }
}

/// A report's selection, `minmax(cursor, anchor)`, or `None` if an offset
/// does not convert, for which Exo ignores the report (ZTIM 506-545).
fn report_selection(text: &str, cursor: u32, anchor: u32) -> Option<Span> {
    let (start, end) = (cursor.min(anchor) as usize, cursor.max(anchor) as usize);
    (text.is_char_boundary(start) && text.is_char_boundary(end)).then(|| Span::new(start, end))
}

/// `ReplaceString16WithOffset` (STT 20-49) with offset 0: replaces
/// `n` bytes at `pos`, clamped to the text, with `replacement`. The
/// branches for a positive offset cannot run.
fn replace(text: &mut String, pos: usize, n: usize, replacement: &str) {
    if text.len() < pos {
        // `replacement` starts after the text.
        return;
    }
    let end = pos + n.min(text.len() - pos);
    text.replace_range(pos..end, replacement);
}

/// `EraseString16WithOffset` (STT 52-77) with offset 0: erases `n` bytes
/// at `pos`, clamped to the text.
fn erase(text: &mut String, pos: usize, n: usize) {
    if text.len() <= pos {
        // The range is after the text.
        return;
    }
    let end = pos + n.min(text.len() - pos);
    text.replace_range(pos..end, "");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Something that happens to a mirror.
    type Event = fn(&mut ExoMirror);

    /// Three syllables of three bytes each: 가 0..3, 나 3..6, 다 6..9.
    const GANADA: &str = "가나다";

    fn state(text: &str, selection: (usize, usize), composition: Range<usize>) -> ExoState {
        ExoState {
            text: text.to_owned(),
            selection: Span::new(selection.0, selection.1),
            composition,
        }
    }

    /// A mirror at extension v11 whose prediction and only history state
    /// is `predicted`.
    fn at(predicted: ExoState) -> ExoMirror {
        let mut mirror = ExoMirror::new(Some(11));
        mirror.predicted = predicted.clone();
        mirror.expected = VecDeque::from([(predicted, false)]);
        mirror
    }

    fn history(mirror: &ExoMirror) -> Vec<ExoState> {
        mirror
            .expected()
            .iter()
            .map(|(state, _)| state.clone())
            .collect()
    }

    /// The prediction is the last history state, as every STT function
    /// but `CancelComposition` leaves it.
    #[track_caller]
    fn assert_pushed(mirror: &ExoMirror, predicted: &ExoState) {
        assert_eq!(mirror.predicted(), predicted);
        assert_eq!(
            mirror.expected().back().map(|(state, _)| state),
            Some(predicted)
        );
        assert!(predicted.fits(), "{predicted:?}");
    }

    /// Sends a report and its barrier, and lets Exo process both.
    fn report(mirror: &mut ExoMirror, text: &str, cursor: u32, anchor: u32) {
        let token = BarrierToken(1);
        mirror.sent(
            token,
            Sent::Report {
                text: text.to_owned(),
                cursor,
                anchor,
            },
        );
        mirror.barrier_done(token);
    }

    fn cancel(mirror: &mut ExoMirror) {
        let token = BarrierToken(2);
        mirror.sent(token, Sent::Cancel);
        mirror.barrier_done(token);
    }

    /// The constructor's state, from `ResetInternal(u"", 0, Range(0))`
    /// (STT 114-116).
    #[test]
    fn a_new_mirror_holds_the_empty_text() {
        let mirror = ExoMirror::new(Some(11));
        let empty = state("", (0, 0), NO_COMPOSITION);
        assert_eq!(history(&mirror), std::slice::from_ref(&empty));
        assert_pushed(&mirror, &empty);
        assert_eq!(mirror.health(), Health::Exact);
        assert!(!mirror.dropped());
        // Without the extension, Exo composes existing text unseen.
        assert_eq!(ExoMirror::new(None).health(), Health::Uncertain);
    }

    /// `Update` (STT 131-191), exact match: the first state from the
    /// oldest with an equal text and selection is matched, the states
    /// before it go, and the prediction stays.
    #[test]
    fn update_matches_an_exact_state_and_erases_older_ones() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.on_set_composition_text("ㄴ", Span::at(3));
        mirror.on_set_composition_text("나", Span::at(3));
        mirror.on_confirm_composition_text(false);
        let states = history(&mirror);
        assert_eq!(states.len(), 4);
        let predicted = mirror.predicted().clone();
        assert_eq!(predicted, state("가나", (6, 6), NO_COMPOSITION));

        // `가ㄴ` with its cursor at 6 is the second state; the composition
        // is not compared ("intentionally ignored composition text").
        assert_eq!(mirror.update("가ㄴ", Span::at(6)), UpdateResult::Updated);
        assert_eq!(history(&mirror), states[1..]);
        assert_eq!(mirror.predicted(), &predicted);
        // An older state is gone, so it no longer matches.
        let mut reset = mirror.clone();
        assert_eq!(reset.update("가", Span::at(3)), UpdateResult::Reset);

        // The oldest of two equal states matches, which erases nothing.
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.on_insert_text("나", CursorBehavior::MoveCursorAfterText);
        mirror.on_extend_selection_and_delete(3, 0);
        let states = history(&mirror);
        assert_eq!(states[0], states[2]);
        assert_eq!(mirror.update("가", Span::at(3)), UpdateResult::Updated);
        assert_eq!(history(&mirror), states);
    }

    /// `Update` compares selections with the directed
    /// `gfx::Range::operator==` (range.h 99), while a report's selection
    /// is `minmax(cursor, anchor)` (ZTIM 506-545): a reversed selection
    /// never matches a report.
    #[test]
    fn update_compares_a_directed_selection() {
        let mut mirror = at(state(GANADA, (0, 0), NO_COMPOSITION));
        mirror.on_set_editable_selection_range(Span::new(6, 3));
        let states = history(&mirror);
        assert_eq!(
            mirror.clone().update(GANADA, Span::new(6, 3)),
            UpdateResult::Updated
        );
        assert_eq!(
            mirror.clone().update(GANADA, Span::new(3, 6)),
            UpdateResult::Reset
        );

        report(&mut mirror, GANADA, 6, 3);
        assert_eq!(history(&mirror), [state(GANADA, (3, 6), NO_COMPOSITION)]);
        assert_ne!(history(&mirror), states[1..]);

        // A forward selection matches the report either way round.
        let mut mirror = at(state(GANADA, (0, 0), NO_COMPOSITION));
        mirror.on_set_editable_selection_range(Span::new(3, 6));
        let states = history(&mirror);
        report(&mut mirror, GANADA, 6, 3);
        assert_eq!(history(&mirror), states[1..]);
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// `Update` with no match: `ResetInternal` (STT 374-381) replaces the
    /// prediction, composition included, and the history is the report
    /// alone. A report that resets a composition sets `dropped` (E4),
    /// which the next composition clears.
    #[test]
    fn update_with_no_match_resets_and_drops_the_composition() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.on_set_composition_text("나", Span::at(3));
        assert_eq!(mirror.update("가", Span::at(0)), UpdateResult::Reset);
        let reported = state("가", (0, 0), NO_COMPOSITION);
        assert_eq!(history(&mirror), std::slice::from_ref(&reported));
        assert_pushed(&mirror, &reported);

        // On the wire.
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.preedit_cursor(3);
        mirror.preedit_string("나", "나");
        // `가` with the cursor at 3 is still in the history; at 0 it is not.
        report(&mut mirror, "가", 3, 3);
        assert!(!mirror.dropped(), "a matched report keeps `나`");
        report(&mut mirror, "가", 0, 0);
        assert!(mirror.dropped(), "the report dropped `나`");
        assert_eq!(history(&mirror), [state("가", (0, 0), NO_COMPOSITION)]);
        mirror.set_preedit_region(0, 3);
        assert!(!mirror.dropped(), "a region composes");
        assert_eq!(mirror.predicted().composition, 0..3);
        report(&mut mirror, "가나", 6, 6);
        assert!(mirror.dropped());
        mirror.preedit_string("다", "다");
        assert!(!mirror.dropped(), "a preedit composes");

        // A reset with nothing composed drops nothing.
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        report(&mut mirror, "나", 3, 3);
        assert!(!mirror.dropped());
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// M3's induction: from states that fit, every STT function leaves a
    /// state that fits the text, on char boundaries, so no range ever
    /// leaves the text and the offset stays 0. The echo checks keep the
    /// wire's ranges in the text.
    #[test]
    fn offsets_stay_zero() {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        let steps: [(&str, Event); 14] = [
            ("compose at the end", |m| {
                m.on_set_composition_text("ㄹ", Span::at(3))
            }),
            ("insert over it", |m| {
                m.on_insert_text("라", CursorBehavior::MoveCursorAfterText)
            }),
            ("select all, reversed", |m| {
                m.on_set_editable_selection_range(Span::new(12, 0))
            }),
            ("compose over it", |m| {
                m.on_set_composition_text("마", Span::new(3, 0))
            }),
            ("confirm after", |m| m.on_confirm_composition_text(false)),
            ("delete past both ends", |m| {
                m.on_extend_selection_and_delete(30, 30)
            }),
            ("insert into nothing", |m| {
                m.on_insert_text(GANADA, CursorBehavior::MoveCursorBeforeText)
            }),
            ("compose the end", |m| {
                m.on_set_composition_from_existing_text(6..9)
            }),
            ("delete after a disjoint composition", |m| {
                m.on_extend_selection_and_delete(0, 3)
            }),
            ("compose the start", |m| {
                m.on_set_composition_from_existing_text(0..3)
            }),
            ("cursor at the end", |m| {
                m.on_set_editable_selection_range(Span::at(3))
            }),
            ("insert after a disjoint composition", |m| {
                m.on_insert_text("바", CursorBehavior::MoveCursorAfterText)
            }),
            ("compose and clear", |m| {
                m.on_set_composition_text("사", Span::at(3));
                m.on_clear_composition_text();
            }),
            ("confirm with nothing composed", |m| {
                m.on_confirm_composition_text(true)
            }),
        ];
        for (name, step) in steps {
            step(&mut mirror);
            let predicted = mirror.predicted().clone();
            assert_pushed(&mirror, &predicted);
            assert!(mirror.expected().iter().all(|(s, _)| s.fits()), "{name}");
        }
        assert_eq!(mirror.predicted(), &state("바", (3, 3), NO_COMPOSITION));
    }

    /// `OnSetCompositionText` (STT 202-229): the composition replaces the
    /// old composition, or the selection if there is none, from its min;
    /// the selection is the composition's own, directed, at its start.
    #[test]
    fn set_composition_replaces_the_composition_or_the_selection() {
        // At a cursor, it inserts.
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.on_set_composition_text("ㅎ", Span::at(3));
        assert_pushed(&mirror, &state("가ㅎ나다", (6, 6), 3..6));
        // Over a composition, it replaces it wherever the selection is.
        mirror.on_set_editable_selection_range(Span::at(0));
        mirror.on_set_composition_text("한", Span::new(3, 0));
        assert_pushed(&mirror, &state("가한나다", (6, 3), 3..6));
        assert_eq!(mirror.predicted().composition_text(), "한");
        // Over a reversed selection, it replaces the selection from its
        // min.
        let mut mirror = at(state(GANADA, (9, 3), NO_COMPOSITION));
        mirror.on_set_composition_text("글", Span::at(0));
        assert_pushed(&mirror, &state("가글", (3, 3), 3..6));
        // On the wire, the cursor is `preedit_cursor`, or else the end.
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.preedit_cursor(0);
        mirror.preedit_string("한", "한");
        assert_pushed(&mirror, &state("가한나다", (3, 3), 3..6));
        mirror.preedit_string("한글", "한글");
        assert_pushed(&mirror, &state("가한글나다", (9, 9), 3..9));
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// An empty `SetCompositionText` with no composition, which IMA's
    /// empty `UpdateCompositionText` sends, replaces the selection with
    /// nothing; with a composition it ends as `OnClearCompositionText`
    /// (STT 255-270) does, which Exo sends the same way (TI 265-272).
    #[test]
    fn an_empty_preedit_with_no_composition_deletes_the_selection() {
        let mut mirror = at(state(GANADA, (6, 3), NO_COMPOSITION));
        mirror.preedit_string("", "");
        assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));

        let composing = state(GANADA, (6, 6), 3..6);
        let mut cleared = at(composing.clone());
        cleared.on_clear_composition_text();
        assert_pushed(&cleared, &state("가다", (3, 3), NO_COMPOSITION));
        let mut mirror = at(composing);
        mirror.preedit_string("", "");
        assert_pushed(&mirror, cleared.predicted());
    }

    /// `OnClearCompositionText` (STT 255-270): the composition goes, with
    /// its text, and the cursor goes to where it was; with none, the state
    /// is pushed unchanged.
    #[test]
    fn clear_composition_erases_it_and_puts_the_cursor_at_its_start() {
        let mut mirror = at(state(GANADA, (0, 9), 3..6));
        mirror.on_clear_composition_text();
        assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));
        mirror.on_clear_composition_text();
        assert_pushed(&mirror, &state("가다", (3, 3), NO_COMPOSITION));
        assert_eq!(mirror.expected().len(), 3);
    }

    /// `OnSetCompositionFromExistingText` (STT 231-239): only the
    /// composition changes, to the range as given.
    #[test]
    fn compose_existing_text_sets_only_the_composition() {
        let mut mirror = at(state(GANADA, (9, 0), NO_COMPOSITION));
        mirror.on_set_composition_from_existing_text(3..6);
        assert_pushed(&mirror, &state(GANADA, (9, 0), 3..6));
        mirror.on_set_composition_from_existing_text(0..9);
        assert_pushed(&mirror, &state(GANADA, (9, 0), 0..9));
    }

    /// X1: ZTIM sends the region's start relative to the selection's
    /// directed end, `cursor.end()` (ZTIM 248-268).
    #[test]
    fn region_is_measured_from_the_selection_end() {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        mirror.set_preedit_region(-6, 3);
        assert_pushed(&mirror, &state(GANADA, (9, 9), 3..6));

        let mut forward = at(state(GANADA, (0, 6), NO_COMPOSITION));
        forward.set_preedit_region(0, 3);
        assert_eq!(forward.predicted().composition_text(), "다");
        let mut reversed = at(state(GANADA, (6, 0), NO_COMPOSITION));
        reversed.set_preedit_region(0, 3);
        assert_eq!(reversed.predicted().composition_text(), "가");
        assert_eq!(reversed.health(), Health::Exact);
    }

    /// X1 with an empty region: the composition ends, its text stays, and
    /// the state is pushed.
    #[test]
    fn empty_region_ends_the_composition() {
        let mut mirror = at(state(GANADA, (6, 6), 3..6));
        mirror.set_preedit_region(0, 0);
        assert_pushed(&mirror, &state(GANADA, (6, 6), NO_COMPOSITION));
        assert_eq!(mirror.expected().len(), 2);
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// `OnConfirmCompositionText` (STT 241-253): the composition stays as
    /// text; the selection stays, or moves after it. With no composition,
    /// the state is still pushed. X2's `unchanged` keeps the selection.
    #[test]
    fn confirm_keep_or_after() {
        let composing = state(GANADA, (3, 3), 3..6);
        let mut kept = at(composing.clone());
        kept.confirm_preedit(true);
        assert_pushed(&kept, &state(GANADA, (3, 3), NO_COMPOSITION));
        let mut after = at(composing);
        after.confirm_preedit(false);
        assert_pushed(&after, &state(GANADA, (6, 6), NO_COMPOSITION));
        after.confirm_preedit(false);
        assert_pushed(&after, &state(GANADA, (6, 6), NO_COMPOSITION));
        assert_eq!(after.expected().len(), 3);
    }

    /// `OnInsertText` (STT 272-326) with no composition: `text` replaces
    /// the selection, and the cursor goes after it, or before it.
    #[test]
    fn insert_replaces_the_selection() {
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.commit_string("한");
        assert_pushed(&mirror, &state("가한나다", (6, 6), NO_COMPOSITION));
        let mut mirror = at(state(GANADA, (9, 3), NO_COMPOSITION));
        mirror.on_insert_text("글", CursorBehavior::MoveCursorBeforeText);
        assert_pushed(&mirror, &state("가글", (3, 3), NO_COMPOSITION));
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        mirror.on_insert_text("", CursorBehavior::MoveCursorAfterText);
        assert_pushed(&mirror, &state(GANADA, (9, 9), NO_COMPOSITION));
    }

    /// `OnInsertText` over a composition the selection intersects (by
    /// `gfx::Range::Intersects`, range.h 106-131): `text` replaces their
    /// union. An empty selection inside the composition intersects it,
    /// as does one at its start; one at its end does not.
    #[test]
    fn insert_over_an_intersecting_composition_replaces_the_union() {
        for (selection, expected) in [
            ((3, 3), state("가한다", (6, 6), NO_COMPOSITION)),
            ((6, 0), state("한다", (3, 3), NO_COMPOSITION)),
            ((9, 3), state("가한", (6, 6), NO_COMPOSITION)),
        ] {
            let mut mirror = at(state(GANADA, selection, 3..6));
            mirror.commit_string("한");
            assert_pushed(&mirror, &expected);
        }
    }

    /// `OnInsertText` over a disjoint composition: the composition goes
    /// first, and the selection shifts back if it came after it. Then the
    /// selection is replaced.
    #[test]
    fn insert_over_a_disjoint_composition_erases_it() {
        // The composition before the selection, the cursor at its end.
        let mut mirror = at(state(GANADA, (6, 6), 3..6));
        mirror.commit_string("한");
        assert_pushed(&mirror, &state("가한다", (6, 6), NO_COMPOSITION));
        // Before a reversed selection, which keeps its length.
        let mut mirror = at(state(GANADA, (9, 6), 0..3));
        mirror.commit_string("한");
        assert_pushed(&mirror, &state("나한", (6, 6), NO_COMPOSITION));
        // After the selection: nothing shifts.
        let mut mirror = at(state(GANADA, (0, 0), 6..9));
        mirror.commit_string("한");
        assert_pushed(&mirror, &state("한가나", (3, 3), NO_COMPOSITION));
    }

    /// `OnSetEditableSelectionRange` (STT 193-200): the range, as given,
    /// becomes the selection, and the composition stays.
    #[test]
    fn set_editable_selection_range_keeps_direction_and_composition() {
        let mut mirror = at(state(GANADA, (6, 6), 3..6));
        mirror.on_set_editable_selection_range(Span::new(9, 0));
        assert_pushed(&mirror, &state(GANADA, (9, 0), 3..6));
        mirror.on_set_editable_selection_range(Span::at(3));
        assert_pushed(&mirror, &state(GANADA, (3, 3), 3..6));
    }

    /// `OnExtendSelectionAndDelete` (STT 328-372) with no composition:
    /// `before` and `after` extend the selection, clamped to the text
    /// (`std::min` and `EraseString16WithOffset`); the range goes and the
    /// cursor goes to its start. Nothing to delete still pushes.
    #[test]
    fn extend_selection_and_delete_erases_around_the_selection() {
        for (selection, before, after, expected) in [
            ((3, 3), 3, 0, state("나다", (0, 0), NO_COMPOSITION)),
            ((3, 3), 0, 3, state("가다", (3, 3), NO_COMPOSITION)),
            ((6, 3), 3, 3, state("", (0, 0), NO_COMPOSITION)),
            ((3, 6), 0, 0, state("가다", (3, 3), NO_COMPOSITION)),
            ((3, 3), 30, 30, state("", (0, 0), NO_COMPOSITION)),
            ((6, 6), 0, 0, state(GANADA, (6, 6), NO_COMPOSITION)),
        ] {
            let mut mirror = at(state(GANADA, selection, NO_COMPOSITION));
            mirror.on_extend_selection_and_delete(before, after);
            assert_pushed(&mirror, &expected);
            assert_eq!(mirror.expected().len(), 2);
        }
        // On the wire, the absolute range gives `before` and `after`.
        let mut mirror = at(state(GANADA, (6, 3), NO_COMPOSITION));
        mirror.delete_surrounding_text(0, 9);
        assert_pushed(&mirror, &state("", (0, 0), NO_COMPOSITION));
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.delete_surrounding_text(3, 0);
        assert_pushed(&mirror, &state(GANADA, (3, 3), NO_COMPOSITION));
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// `OnExtendSelectionAndDelete` with a composition, which IMA never
    /// causes (the wire's echo check diverges instead): an intersecting
    /// composition widens the range, and a disjoint one goes first, which
    /// shifts the range if it came before it.
    #[test]
    fn extend_selection_and_delete_over_a_composition() {
        for (composition, selection, before, after, expected) in [
            // Intersecting: the union goes.
            (3..6, (6, 6), 3, 0, state("가다", (3, 3), NO_COMPOSITION)),
            (3..6, (9, 9), 6, 0, state("가", (3, 3), NO_COMPOSITION)),
            (3..9, (6, 6), 6, 0, state("", (0, 0), NO_COMPOSITION)),
            // An empty range inside the composition.
            (0..6, (3, 3), 0, 0, state("다", (0, 0), NO_COMPOSITION)),
            // Disjoint, before the range: both go, and the range shifts.
            // A range that starts at the composition's end is disjoint.
            (0..3, (6, 6), 3, 0, state("다", (0, 0), NO_COMPOSITION)),
            (3..6, (6, 6), 0, 3, state("가", (3, 3), NO_COMPOSITION)),
            (0..3, (9, 9), 3, 0, state("나", (3, 3), NO_COMPOSITION)),
            (0..3, (6, 6), 0, 0, state("나다", (3, 3), NO_COMPOSITION)),
            // Disjoint, after the range: both go, nothing shifts.
            (6..9, (3, 3), 3, 0, state("나", (0, 0), NO_COMPOSITION)),
        ] {
            let mut mirror = at(state(GANADA, selection, composition.clone()));
            mirror.on_extend_selection_and_delete(before, after);
            assert_pushed(&mirror, &expected);
        }
    }

    /// `CancelComposition` (STT 124-129): the composition ends, its text
    /// stays, and the history empties without the prediction, so the next
    /// report resets even when it names the prediction.
    #[test]
    fn cancel_empties_the_history() {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        mirror.preedit_string("라", "라");
        cancel(&mut mirror);
        assert_eq!(
            mirror.predicted(),
            &state("가나다라", (12, 12), NO_COMPOSITION)
        );
        assert_eq!(history(&mirror), []);
        assert_eq!(mirror.update("가나다라", Span::at(12)), UpdateResult::Reset);
        // An event after the cancel leaves a history of its own state.
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        cancel(&mut mirror);
        mirror.commit_string("라");
        assert_eq!(
            history(&mirror),
            [state("가나다라", (12, 12), NO_COMPOSITION)]
        );
        assert_eq!(mirror.health(), Health::Exact);
    }

    /// `SetSurroundingTextImpl` (ZTIM 506-545): the selection is
    /// `minmax(cursor, anchor)`, and a request whose offsets do not
    /// convert to UTF-16 (past the end, or inside a character) is ignored
    /// whole.
    #[test]
    fn a_report_selection_is_minmax_and_unconvertible_offsets_are_ignored() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        report(&mut mirror, GANADA, 9, 3);
        assert_eq!(history(&mirror), [state(GANADA, (3, 9), NO_COMPOSITION)]);
        for (cursor, anchor) in [(1, 1), (3, 4), (10, 0)] {
            let mut ignored = mirror.clone();
            ignored.preedit_string("라", "라");
            let before = (ignored.predicted().clone(), history(&ignored));
            report(&mut ignored, GANADA, cursor, anchor);
            assert_eq!((ignored.predicted().clone(), history(&ignored)), before);
            assert!(!ignored.dropped());
        }
    }

    /// A request takes effect when its barrier is done, after the events
    /// that came before that `done`, not when it is sent; a barrier the
    /// mirror did not queue changes nothing.
    #[test]
    fn a_request_applies_at_its_barrier_not_when_sent() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.sent(
            BarrierToken(7),
            Sent::Report {
                text: "가".to_owned(),
                cursor: 0,
                anchor: 0,
            },
        );
        mirror.sent(BarrierToken(8), Sent::Cancel);
        // Exo composed before it processed the report.
        mirror.preedit_string("나", "나");
        let composing = state("가나", (6, 6), 3..6);
        assert_pushed(&mirror, &composing);
        mirror.barrier_done(BarrierToken(8));
        mirror.barrier_done(BarrierToken(9));
        assert_eq!(history(&mirror).len(), 2, "nothing applied out of order");
        // The report then matches nothing and drops the composition.
        mirror.barrier_done(BarrierToken(7));
        assert_eq!(history(&mirror), [state("가", (0, 0), NO_COMPOSITION)]);
        assert!(mirror.dropped());
        mirror.barrier_done(BarrierToken(8));
        assert_eq!(history(&mirror), []);
        assert_eq!(mirror.predicted(), &state("가", (0, 0), NO_COMPOSITION));

        // Applied when sent, the report would have come before the
        // composition, which would then have stayed.
        let mut early = at(state("가", (3, 3), NO_COMPOSITION));
        report(&mut early, "가", 0, 0);
        early.preedit_string("나", "나");
        assert_eq!(early.predicted(), &state("나가", (3, 3), 0..3));
        assert!(!early.dropped());
    }

    /// Below the extension's `confirm_preedit` (v11), a confirm is
    /// `cursor_position(selection)` + `commit_string(composition)` (TI
    /// 240-263); so is `SetEditableSelectionRange` (TI 386-403), which
    /// keeps the composition. The pair is a confirm only below v11 and
    /// with the predicted selection, where it could be either, which makes
    /// the mirror `Uncertain`.
    #[test]
    fn a_cursor_position_pair_is_a_confirm_only_below_v11_with_the_predicted_selection() {
        let composing = state(GANADA, (6, 6), 3..6);
        let confirmed = state(GANADA, (6, 6), NO_COMPOSITION);
        for (version, (index, anchor), expected, health) in [
            (None, (6, 6), confirmed.clone(), Health::Uncertain),
            (Some(10), (6, 6), confirmed, Health::Uncertain),
            (Some(11), (6, 6), composing.clone(), Health::Exact),
            (Some(10), (9, 0), state(GANADA, (0, 9), 3..6), Health::Exact),
            (None, (3, 3), state(GANADA, (3, 3), 3..6), Health::Exact),
        ] {
            let mut mirror = at(composing.clone());
            mirror.ext_version = version;
            mirror.cursor_position(index, anchor);
            mirror.commit_string("나");
            assert_pushed(&mirror, &expected);
            assert_eq!(mirror.health(), health);
            // The pair is spent: the next commit is an insert.
            mirror.commit_string("라");
            assert!(mirror.predicted().text.contains('라'));
        }
        // With no composition, both pairs commit "".
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.cursor_position(6, 6);
        mirror.commit_string("");
        assert_pushed(&mirror, &state(GANADA, (6, 6), NO_COMPOSITION));
    }

    /// Every echo check: an event that does not agree with the mirror
    /// changes nothing and makes the mirror `Diverged`.
    #[test]
    fn each_echo_check_diverges() {
        let composing = state(GANADA, (6, 6), 3..6);
        let idle = state(GANADA, (6, 3), NO_COMPOSITION);
        let cases: [(&str, &ExoState, Event); 15] = [
            ("a preedit whose commit differs", &idle, |m| {
                m.preedit_string("라", "마")
            }),
            ("a preedit cursor inside a character", &idle, |m| {
                m.preedit_cursor(1);
                m.preedit_string("라", "라");
            }),
            ("a preedit cursor past its end", &idle, |m| {
                m.preedit_cursor(4);
                m.preedit_string("라", "라");
            }),
            ("a negative preedit cursor", &idle, |m| {
                m.preedit_cursor(-1);
                m.preedit_string("라", "라");
            }),
            ("a pair that commits other text", &composing, |m| {
                m.cursor_position(6, 6);
                m.commit_string("다");
            }),
            ("a pair that commits text with no composition", &idle, |m| {
                m.cursor_position(6, 6);
                m.commit_string("다");
            }),
            ("a pair out of the text", &composing, |m| {
                m.cursor_position(10, 6);
                m.commit_string("나");
            }),
            ("a pair inside a character", &composing, |m| {
                m.cursor_position(4, 4);
                m.commit_string("나");
            }),
            ("a delete while composing", &composing, |m| {
                m.delete_surrounding_text(3, 3)
            }),
            ("a delete inside a character", &idle, |m| {
                m.delete_surrounding_text(2, 4)
            }),
            ("a delete past the text", &idle, |m| {
                m.delete_surrounding_text(3, 9)
            }),
            ("a delete beside the selection", &idle, |m| {
                m.delete_surrounding_text(0, 3)
            }),
            ("a negative delete", &idle, |m| {
                m.delete_surrounding_text(-3, 9)
            }),
            ("a region inside a character", &idle, |m| {
                m.set_preedit_region(-1, 1)
            }),
            ("a region out of the text", &idle, |m| {
                m.set_preedit_region(-6, 3)
            }),
        ];
        for (name, start, event) in cases {
            let mut mirror = at(start.clone());
            event(&mut mirror);
            assert_eq!(mirror.health(), Health::Diverged, "{name}");
            assert_eq!(history(&mirror), std::slice::from_ref(start), "{name}");
            assert_pushed(&mirror, start);
            // A diverged mirror still applies the events that agree, and
            // a divergence outranks an uncertainty.
            mirror.confirm_preedit(true);
            mirror.degrade(Health::Uncertain);
            assert_eq!(mirror.health(), Health::Diverged, "{name}");
            assert_eq!(mirror.expected().len(), 2, "{name}");
        }
    }

    /// The caps: evicting a history state, which Exo still has, makes the
    /// mirror `Uncertain`; so does text over the cap, which the mirror
    /// then stops keeping until a report resets it.
    #[test]
    fn eviction_and_long_text_make_it_uncertain() {
        let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
        for _ in 1..HISTORY_CAP {
            mirror.confirm_preedit(true);
        }
        assert_eq!(mirror.expected().len(), HISTORY_CAP);
        assert_eq!(mirror.health(), Health::Exact);
        mirror.commit_string("가");
        assert_eq!(mirror.expected().len(), HISTORY_CAP);
        assert_eq!(
            history(&mirror).last(),
            Some(&state("가", (3, 3), NO_COMPOSITION))
        );
        assert_eq!(mirror.health(), Health::Uncertain);

        let full = "가".repeat(TEXT_CAP / 3 - 1);
        let mut mirror = at(state(&full, (full.len(), full.len()), NO_COMPOSITION));
        mirror.commit_string("가");
        assert_eq!(mirror.health(), Health::Exact);
        mirror.commit_string("가");
        assert_eq!(mirror.health(), Health::Uncertain);
        assert_eq!(history(&mirror), []);
        // Blind: events are not checked or applied.
        mirror.delete_surrounding_text(30, 3);
        mirror.commit_string("가");
        assert_eq!(mirror.health(), Health::Uncertain);
        assert_eq!(history(&mirror), []);
        // A report gives it text again, still uncertain.
        report(&mut mirror, "가", 3, 3);
        assert_eq!(history(&mirror), [state("가", (3, 3), NO_COMPOSITION)]);
        mirror.commit_string("나");
        assert_eq!(mirror.predicted(), &state("가나", (6, 6), NO_COMPOSITION));
        assert_eq!(mirror.health(), Health::Uncertain);
    }

    /// An unhealthy mirror heals only at a reset point: a cancel, which
    /// empties Exo's history, and then a report, which must reset Exo,
    /// with no tracker-changing event between them. A matched report heals
    /// nothing, and the cancel of a healthy mirror changes nothing.
    #[test]
    fn healing_needs_a_cancel_then_a_clean_report() {
        let start = state(GANADA, (9, 9), NO_COMPOSITION);
        let mut mirror = at(start.clone());
        cancel(&mut mirror);
        assert_eq!(mirror.health(), Health::Exact);

        let mut mirror = at(start.clone());
        mirror.delete_surrounding_text(0, 3);
        assert_eq!(mirror.health(), Health::Diverged);
        report(&mut mirror, GANADA, 9, 9);
        assert_eq!(mirror.health(), Health::Diverged, "a matched report");
        report(&mut mirror, "가", 3, 3);
        assert_eq!(
            mirror.health(),
            Health::Diverged,
            "a reset without a cancel"
        );

        // An event between the cancel and the report.
        cancel(&mut mirror);
        assert_eq!(mirror.health(), Health::Healing);
        mirror.commit_string("나");
        assert_eq!(mirror.health(), Health::Uncertain);
        report(&mut mirror, "다", 3, 3);
        assert_eq!(mirror.health(), Health::Uncertain);

        // A cancel, then a report Exo must reset to.
        cancel(&mut mirror);
        assert_eq!(mirror.health(), Health::Healing);
        report(&mut mirror, "라", 3, 3);
        assert_eq!(mirror.health(), Health::Exact);
        assert_eq!(history(&mirror), [state("라", (3, 3), NO_COMPOSITION)]);

        // Without the extension Exo composes existing text unseen, so
        // nothing heals the mirror.
        let mut mirror = ExoMirror::new(None);
        cancel(&mut mirror);
        report(&mut mirror, "라", 3, 3);
        assert_eq!(mirror.health(), Health::Uncertain);
    }

    /// IMA makes a composition selection from a highlight span with the
    /// cursor at one end (`ExtractCompositionText`), whose other end the
    /// wire does not carry: the mirror cannot know the selection.
    #[test]
    fn a_composition_selection_makes_it_uncertain() {
        let highlight = zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT;
        for (case, style, (index, length), health) in [
            (
                "a highlight ending at the cursor",
                highlight,
                (0, 3),
                Health::Uncertain,
            ),
            ("one starting at it", highlight, (3, 3), Health::Uncertain),
            ("one apart from it", highlight, (6, 3), Health::Exact),
            (
                "an underline",
                zwp_text_input_v1::PREEDIT_STYLE_UNDERLINE,
                (0, 3),
                Health::Exact,
            ),
        ] {
            let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
            mirror.preedit_styling(index, length, style);
            mirror.preedit_cursor(3);
            mirror.preedit_string(GANADA, GANADA);
            assert_eq!(mirror.health(), health, "{case}");
            // The spans are spent.
            mirror.preedit_cursor(3);
            mirror.preedit_string("가", "가");
            assert_eq!(mirror.health(), health, "{case}");
        }
    }

    /// A report on its way to Exo crosses the events since it was sent if
    /// it will name no state then, or follows a `reset`.
    #[test]
    fn a_report_crosses_when_it_will_name_no_state() {
        let mut mirror = at(state("ab", (2, 2), NO_COMPOSITION));
        mirror.commit_string("c");
        let sent = |text: &str, cursor| Sent::Report {
            text: text.to_owned(),
            cursor,
            anchor: cursor,
        };
        mirror.sent(BarrierToken(1), sent("ab", 2));
        mirror.sent(BarrierToken(2), sent("abc", 3));
        assert!(!mirror.report_crosses());
        mirror.sent(BarrierToken(3), sent("abX", 3));
        assert!(mirror.report_crosses());

        let mut mirror = at(state("ab", (2, 2), NO_COMPOSITION));
        mirror.sent(BarrierToken(1), Sent::Cancel);
        assert!(!mirror.report_crosses());
        mirror.sent(BarrierToken(2), sent("ab", 2));
        assert!(mirror.report_crosses());
    }

    /// A matched report puts a guest that shows no preedit at the first
    /// equal state without a composition, and in step if every transaction
    /// since kept it so.
    #[test]
    fn a_matched_report_finds_the_guest_without_a_composition() {
        let applied = |second: bool| {
            let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
            mirror.preedit_string("가", "가");
            mirror.mark(false);
            mirror.commit_string("가");
            mirror.mark(true);
            mirror.preedit_string("나", "나");
            mirror.mark(second);
            let token = BarrierToken(1);
            let report = Sent::Report {
                text: "가".to_owned(),
                cursor: 3,
                anchor: 3,
            };
            mirror.sent(token, report);
            mirror.barrier_done(token)
        };
        // The report names the state with the composition "가" first, whose
        // transaction did not keep the guest in step; but the guest, which
        // reports "가" as text, committed it.
        assert_eq!(applied(true), Some(Applied::Matched { in_step: true }));
        assert_eq!(applied(false), Some(Applied::Matched { in_step: false }));
    }

    /// Korean typing, end to end on the wire: each preedit replaces the
    /// last, the commit replaces the composition, and the guest's report
    /// after it names the latest state.
    #[test]
    fn korean_typing_through_the_wire() {
        let mut mirror = ExoMirror::new(Some(11));
        for syllable in ["ㅎ", "하", "한"] {
            mirror.preedit_cursor(3);
            mirror.preedit_string(syllable, syllable);
        }
        assert_pushed(&mirror, &state("한", (3, 3), 0..3));
        mirror.commit_string("한");
        mirror.preedit_cursor(3);
        mirror.preedit_string("ㄱ", "ㄱ");
        assert_pushed(&mirror, &state("한ㄱ", (6, 6), 3..6));
        // The guest reports the commit, not the preedit it shows. That
        // matches the oldest state equal to it, the preedit `한`, as
        // `Update` ignores compositions.
        report(&mut mirror, "한", 3, 3);
        assert_eq!(history(&mirror)[0], state("한", (3, 3), 0..3));
        assert_eq!(history(&mirror).len(), 3);
        assert_eq!(mirror.predicted(), &state("한ㄱ", (6, 6), 3..6));
        assert!(!mirror.dropped());
        mirror.preedit_string("글", "글");
        mirror.confirm_preedit(false);
        assert_pushed(&mirror, &state("한글", (6, 6), NO_COMPOSITION));
        report(&mut mirror, "한글", 6, 6);
        assert_eq!(
            history(&mirror),
            [
                state("한글", (6, 6), 3..6),
                state("한글", (6, 6), NO_COMPOSITION)
            ]
        );
        mirror.delete_surrounding_text(3, 3);
        assert_pushed(&mirror, &state("한", (3, 3), NO_COMPOSITION));
        assert_eq!(mirror.health(), Health::Exact);
    }
}
