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
//! (`docs/IME.md` 2.2, X1 and H9).
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
//!
//! The history keeps no text, only what a report and report suppression
//! compare: each state's [`Fingerprint`], whose texts are a length and a
//! 64-bit hash. Two texts of one length with one hash would match falsely;
//! that needs adversarial text, and the echo checks would likely catch
//! what follows from it.
//!
//! The mirror says how much it knows ([`Health`]): the prediction is
//! exact, exact but for its selection (vague), or unknown, and so is each
//! history state. An event the mirror cannot follow makes the prediction
//! unknown and pushes an unknown state; a report Exo processes heals it
//! whenever its outcome is certain.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hasher};
use std::ops::Range;

use super::BarrierToken;
use crate::protocols::text_input_unstable_v1::zwp_text_input_v1;

/// The most history states the mirror keeps. Exo keeps every one (STT has
/// no cap), so evicting the oldest leaves an unknown state in its place,
/// which Exo could match. Only a field with this many events of Exo's and
/// no report reaches it.
const HISTORY_CAP: usize = 4096;

/// The longest text the mirror predicts, in bytes. A longer one makes the
/// prediction unknown until a report resets it; the history holds no text,
/// so this cap bounds only the prediction.
const TEXT_CAP: usize = 256 * 1024;

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

/// A text's fingerprint: its length in bytes and its hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    len: usize,
    hash: u64,
}

impl Key {
    /// `DefaultHasher::new` uses fixed keys, so a text has one key.
    pub(super) fn of(text: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        hasher.write(text.as_bytes());
        Self {
            len: text.len(),
            hash: hasher.finish(),
        }
    }
}

/// What a report compares of a state's selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// The selection, directed.
    Exact(Span),
    /// The selection is vague ([`Health::Vague`]): these are the forward
    /// selections it may be, which are all a report's, forward, can equal.
    Vague(Vec<Span>),
}

/// What a report and report suppression compare of one tracker state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    text: Key,
    selection: Selection,
    /// The composition, [`NO_COMPOSITION`] if there is none.
    composition: Range<usize>,
    /// With a composition, the keys of the text without it and of its
    /// text.
    split: Option<(Key, Key)>,
}

impl Fingerprint {
    /// `state`'s fingerprint, vague as this one is.
    #[cfg(test)]
    pub(super) fn of_alike(&self, state: &ExoState) -> Self {
        let vague = match &self.selection {
            Selection::Vague(selections) => Some(&selections[..]),
            Selection::Exact(_) => None,
        };
        Self::of(state, vague)
    }

    /// `state`'s fingerprint, whose selection is one of `vague`, if given.
    fn of(state: &ExoState, vague: Option<&[Span]>) -> Self {
        let composition = state.composition.clone();
        let split = (!composition.is_empty()).then(|| {
            let mut rest = state.text[..composition.start].to_owned();
            rest.push_str(&state.text[composition.end..]);
            (Key::of(&rest), Key::of(state.composition_text()))
        });
        Self {
            text: Key::of(&state.text),
            selection: match vague {
                Some(selections) => Selection::Vague(selections.to_vec()),
                None => Selection::Exact(state.selection),
            },
            composition,
            split,
        }
    }
}

/// One state of the history: its fingerprint, with whether the transaction
/// that led to it kept a guest in step with Exo if it was
/// ([`ExoMirror::mark`]); or a state the mirror could not compute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Known { print: Fingerprint, in_step: bool },
    Unknown,
}

/// Where a report stands in the history, from the oldest state on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    /// The first state equal to it, with nothing before it that Exo could
    /// match instead.
    Match(usize),
    /// An unknown state, or a vague one with its text and a selection it
    /// may have, comes first: Exo may have matched there.
    Ambiguous,
    /// No state can equal it.
    Missing,
}

/// Why the mirror could not take an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Miss {
    /// It does not agree with the mirror's state: an echo check failed.
    Diverged,
    /// Its effect depends on what the mirror does not know.
    Unknowable,
}

/// `InsertTextCursorBehavior`. Exo always passes
/// [`Self::MoveCursorAfterText`] (TI 274-280).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorBehavior {
    MoveCursorAfterText,
    #[cfg(test)]
    MoveCursorBeforeText,
}

/// `SurroundingTextTracker::UpdateResult`, and the outcome the mirror
/// cannot decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateResult {
    /// A history state matched: the states before it are gone, and the
    /// prediction stays.
    Updated,
    /// None matched: the report replaced the prediction and the history.
    Reset,
    /// Exo may have done either ([`Lookup::Ambiguous`]).
    Ambiguous,
}

/// A request the proxy sent Exo that changes its tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Sent {
    /// v1 `set_surrounding_text(text, cursor, anchor)`: `Update`. `split`
    /// if it and its barrier may reach Exo in two reads, between which Exo
    /// may process an event.
    Report {
        text: String,
        cursor: u32,
        anchor: u32,
        split: bool,
    },
    /// v1 `reset`: `CancelComposition` (TI 152-156).
    Cancel,
}

/// How much the mirror knows of Exo's prediction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Health {
    /// It equals Exo's.
    Exact,
    /// It equals Exo's but for the selection, which IMA made from a
    /// highlight span whose other end the wire does not carry. With a
    /// composition, the selection lies inside it. An event that sets the
    /// selection absolutely, or replaces the whole composition, makes the
    /// mirror exact again; one whose effect depends on where the selection
    /// is makes it unknown.
    Vague,
    /// The mirror cannot tell Exo's text: an event did not agree with it
    /// (an echo check failed: it diverged), came while a report that may
    /// reach Exo apart from its barrier was on its way, or came while
    /// unknown; the text grew over [`TEXT_CAP`]; or a report's outcome is
    /// ambiguous. A report Exo must reset to makes it exact again, which
    /// after a press's `reset` (an empty history) is any report before the
    /// next event.
    Unknown,
}

/// What a request did once Exo processed it ([`ExoMirror::barrier_done`]),
/// which tells where the guest's report put the guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Applied {
    /// A report matched a history state: the guest is at the first equal
    /// state with no composition from there, and `in_step` if it has no
    /// selection and every transaction after it kept the guest in step.
    Matched { in_step: bool },
    /// A report matched none and reset Exo to it, or the mirror cannot
    /// tell whether it matched, and Exo's prediction is the report's
    /// either way or unknown; `cursor` if its selection is empty.
    Reset { cursor: bool },
    /// A `reset` emptied the history.
    Cancel,
}

/// Exo's tracker for one host text input, as the wire shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ExoMirror {
    /// `predicted_state_` after every message Exo has processed so far, as
    /// far as `health` says: the empty text while unknown.
    predicted: ExoState,
    health: Health,
    /// `expected_updates_`, as fingerprints, oldest first, at most
    /// [`HISTORY_CAP`].
    expected: VecDeque<Entry>,
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
    /// While the prediction is vague, the selections it may have
    /// ([`Selection::Vague`]).
    vague: Vec<Span>,
    /// The last report reset a non-empty composition (E4), and nothing
    /// has composed since.
    dropped: bool,
    /// An echo check failed since [`Self::take_diverged`] last looked.
    diverged: bool,
}

impl ExoMirror {
    /// The tracker of a new host text input (`SurroundingTextTracker()`,
    /// STT 114-116).
    pub(super) fn new() -> Self {
        let mut mirror = Self {
            predicted: ExoState::default(),
            health: Health::Exact,
            expected: VecDeque::new(),
            unmarked: false,
            in_flight: VecDeque::new(),
            preedit_cursor: None,
            highlights: Vec::new(),
            cursor_position: None,
            vague: Vec::new(),
            dropped: false,
            diverged: false,
        };
        mirror.reset_internal("", Span::at(0));
        mirror
    }

    /// Exo's prediction, if the mirror knows it exactly.
    pub(super) fn exact(&self) -> Option<&ExoState> {
        (self.health == Health::Exact).then_some(&self.predicted)
    }

    /// The prediction, which must be exact.
    #[cfg(test)]
    #[track_caller]
    pub(super) fn predicted(&self) -> &ExoState {
        self.exact().expect("an exact prediction")
    }

    #[cfg(test)]
    pub(super) fn expected(&self) -> &VecDeque<Entry> {
        &self.expected
    }

    /// Records whether the transaction that led to the newest state kept a
    /// guest in step, if no state has been marked since it was pushed.
    pub(super) fn mark(&mut self, in_step: bool) {
        if std::mem::take(&mut self.unmarked) {
            if let Some(Entry::Known { in_step: step, .. }) = self.expected.back_mut() {
                *step = in_step;
            }
        }
    }

    /// Whether an echo check failed since the last call.
    pub(super) fn take_diverged(&mut self) -> bool {
        std::mem::take(&mut self.diverged)
    }

    /// Where a report of `text` with `selection` stands in the history
    /// from state `from` on.
    fn lookup(&self, from: usize, text: &str, selection: Span) -> Lookup {
        let text = Key::of(text);
        for (at, entry) in self.expected.iter().enumerate().skip(from) {
            let Entry::Known { print, .. } = entry else {
                return Lookup::Ambiguous;
            };
            match &print.selection {
                _ if print.text != text => {}
                Selection::Vague(selections) if selections.contains(&selection) => {
                    return Lookup::Ambiguous
                }
                Selection::Exact(equal) if *equal == selection => return Lookup::Match(at),
                Selection::Vague(_) | Selection::Exact(_) => {}
            }
        }
        Lookup::Missing
    }

    /// Whether a report the proxy sent and Exo has yet to process will
    /// match no state Exo has now, or may not, or follows a `reset` that
    /// will empty the history: the guest made an edit Exo has yet to see.
    pub(super) fn report_crosses(&self) -> bool {
        let mut from = 0;
        for (at, (_, sent)) in self.in_flight.iter().enumerate() {
            let Sent::Report {
                text,
                cursor,
                anchor,
                ..
            } = sent
            else {
                let mut rest = self.in_flight.iter().skip(at);
                return rest.any(|(_, sent)| matches!(sent, Sent::Report { .. }));
            };
            let Some(selection) = report_selection(text, *cursor, *anchor) else {
                continue;
            };
            match self.lookup(from, text, selection) {
                Lookup::Match(at) => from = at,
                Lookup::Ambiguous | Lookup::Missing => return true,
            }
        }
        false
    }

    /// Report suppression (G9c): whether an exact mirror
    /// finds that the guest's report `(text, cursor, anchor)`, from a guest
    /// that shows the preedit `shown`, would only make Exo drop its
    /// composition. It names no state of the history Exo will hold once it
    /// has processed every request in flight, which holds no unknown
    /// state, but one such state is the guest's text with the composition
    /// `shown` inline, at the cursor. Then `Some` of whether every
    /// transaction since that state kept a guest in step. A report that
    /// names a state is a no-op for Exo, and any other resets it to the
    /// guest's text, so both go out.
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
        if self.lookup(from, text, selection) != Lookup::Missing {
            return None;
        }
        let split = Some((Key::of(text), Key::of(shown)));
        let mut states = self.expected.iter().skip(from);
        states.find(|entry| {
            matches!(entry, Entry::Known { print, .. }
                if print.split == split && print.composition.start == selection.start)
        })?;
        Some(states.all(|entry| matches!(entry, Entry::Known { in_step: true, .. })))
    }

    /// Where the history Exo will hold once it has processed every request
    /// in flight starts in `expected`, of which it is a suffix: every event
    /// so far came before them. `None` if one of them will reset it, may,
    /// or will empty it.
    fn projected_from(&self) -> Option<usize> {
        let mut from = 0;
        for (_, sent) in &self.in_flight {
            let Sent::Report {
                text,
                cursor,
                anchor,
                ..
            } = sent
            else {
                return None;
            };
            let Some(selection) = report_selection(text, *cursor, *anchor) else {
                continue;
            };
            match self.lookup(from, text, selection) {
                Lookup::Match(at) => from = at,
                Lookup::Ambiguous | Lookup::Missing => return None,
            }
        }
        Some(from)
    }

    pub(super) fn health(&self) -> Health {
        self.health
    }

    #[cfg(test)]
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
    /// composition selection the mirror cannot know, so the selection is
    /// [`Health::Vague`]: empty at the cursor, or between it and such a
    /// highlight's other end. Without one, the cursor sets it.
    pub(super) fn preedit_string(&mut self, text: &str, commit: &str) {
        let cursor = self.preedit_cursor.take();
        let highlights = std::mem::take(&mut self.highlights);
        self.observe(|mirror| {
            let cursor = match cursor {
                None => text.len(),
                Some(cursor) => match usize::try_from(cursor) {
                    Ok(cursor) if text.is_char_boundary(cursor) => cursor,
                    _ => return Err(Miss::Diverged),
                },
            };
            if commit != text {
                return Err(Miss::Diverged);
            }
            // With no composition, it replaces the selection.
            if mirror.health == Health::Vague && mirror.predicted.composition.is_empty() {
                return Err(Miss::Unknowable);
            }
            // Where the composition will begin (`OnSetCompositionText`).
            let predicted = &mirror.predicted;
            let begin = match predicted.composition.is_empty() {
                true => predicted.selection.min(),
                false => predicted.composition.start,
            };
            let at = begin + cursor;
            let others = highlights.iter().filter_map(|span| match () {
                _ if span.start == cursor => Some(span.end),
                _ if span.end == cursor => Some(span.start),
                _ => None,
            });
            let others: Vec<Span> = others
                .map(|other| begin + other.min(text.len()))
                .map(|other| Span::new(at.min(other), at.max(other)))
                .collect();
            let vague = !others.is_empty();
            if vague {
                mirror.vague = [Span::at(at)].into_iter().chain(others).collect();
            }
            mirror.health = if vague { Health::Vague } else { Health::Exact };
            mirror.dropped = false;
            mirror.on_set_composition_text(text, Span::at(cursor));
            Ok(())
        });
    }

    /// H7 with a text that is not UTF-8: Exo changed its tracker in a way
    /// the mirror cannot read.
    pub(super) fn unreadable(&mut self) {
        self.observe(|_| Err(Miss::Unknowable));
    }

    /// H6 `cursor_position(index, anchor)`: `SetCursor` (ZTIM 192-199),
    /// which the next `commit_string` completes.
    pub(super) fn cursor_position(&mut self, index: i32, anchor: i32) {
        self.cursor_position = Some((index, anchor));
    }

    /// H8 `commit_string(text)`. Alone, it is `TI::InsertText` →
    /// `OnInsertText(text, kMoveCursorAfterText)` (TI 274-280), which over
    /// a composition holding the selection replaces the composition
    /// wherever the selection is in it. After a `cursor_position(range)`,
    /// it is `SetEditableSelectionRange(range)` (TI 386-403), which keeps
    /// the composition and sends its text. The mirror exists only from v11
    /// of the extension, whose confirmations are `confirm_preedit` (ZTIM
    /// 430-447), so the pair is never `ConfirmCompositionText(true)`'s
    /// fallback (TI 240-263), which IMA takes only below it.
    ///
    /// Echo, for a pair: the range is in the text, and `text` is the
    /// composition's.
    pub(super) fn commit_string(&mut self, text: &str) {
        let cursor_position = self.cursor_position.take();
        self.observe(|mirror| {
            let vague = mirror.health == Health::Vague;
            let composing = !mirror.predicted.composition.is_empty();
            let Some((index, anchor)) = cursor_position else {
                if vague && !composing {
                    return Err(Miss::Unknowable);
                }
                mirror.health = Health::Exact;
                mirror.on_insert_text(text, CursorBehavior::MoveCursorAfterText);
                return Ok(());
            };
            let (Ok(start), Ok(end)) = (usize::try_from(anchor), usize::try_from(index)) else {
                return Err(Miss::Diverged);
            };
            let range = Span::new(start, end);
            let predicted = &mirror.predicted;
            if !predicted.holds_span(range) || text != predicted.composition_text() {
                return Err(Miss::Diverged);
            }
            mirror.health = Health::Exact;
            mirror.on_set_editable_selection_range(range);
            Ok(())
        });
    }

    /// X2 `confirm_preedit`: `ConfirmComposition` →
    /// `OnConfirmCompositionText` (TI 240-263; ZTIM 430-447), keeping the
    /// selection for `unchanged`.
    pub(super) fn confirm_preedit(&mut self, keep_selection: bool) {
        self.observe(|mirror| {
            if !keep_selection && !mirror.predicted.composition.is_empty() {
                // The selection goes to the composition's end.
                mirror.health = Health::Exact;
            }
            mirror.on_confirm_composition_text(keep_selection);
            Ok(())
        });
    }

    /// H9 `delete_surrounding_text(index, length)`:
    /// `ExtendSelectionAndDelete` → `OnExtendSelectionAndDelete` (TI
    /// 434-449). ZTIM sends the absolute range TI clamped to the text
    /// (ZTIM 201-213), which gives back `before` and `after` exactly; so it
    /// erases that range wherever a vague selection is in it.
    ///
    /// Echo: the range is in the text and contains the selection, and
    /// there is no composition, as IMA never deletes while composing (IMA
    /// 1090-1102).
    pub(super) fn delete_surrounding_text(&mut self, index: i32, length: u32) {
        self.observe(|mirror| {
            let Ok(start) = usize::try_from(index) else {
                return Err(Miss::Diverged);
            };
            let end = start + length as usize;
            let vague = mirror.health == Health::Vague;
            let predicted = &mut mirror.predicted;
            let selection = predicted.selection;
            let around = start <= selection.min() && selection.max() <= end;
            if !predicted.composition.is_empty()
                || !predicted.holds_span(Span::new(start, end))
                || !(around || vague)
            {
                return Err(Miss::Diverged);
            }
            if vague {
                predicted.selection = Span::at(start);
                mirror.health = Health::Exact;
            }
            let selection = mirror.predicted.selection;
            mirror.on_extend_selection_and_delete(selection.min() - start, end - selection.max());
            Ok(())
        });
    }

    /// X1 `set_preedit_region(index, length)`:
    /// `SetCompositionFromExistingText` → `OnSetCompositionFromExistingText`
    /// (TI 483-508). ZTIM measures `index` from the selection's directed
    /// end (ZTIM 248-268), which a vague selection does not tell. An empty
    /// region ends the composition.
    ///
    /// Echo: the range is in the text (IMA's `text_range.Contains`).
    pub(super) fn set_preedit_region(&mut self, index: i32, length: u32) {
        self.observe(|mirror| {
            if mirror.health == Health::Vague {
                return Err(Miss::Unknowable);
            }
            let start = mirror.predicted.selection.end as i64 + i64::from(index);
            let Ok(start) = usize::try_from(start) else {
                return Err(Miss::Diverged);
            };
            let end = start + length as usize;
            if !mirror.predicted.holds_span(Span::new(start, end)) {
                return Err(Miss::Diverged);
            }
            mirror.dropped = false;
            mirror.on_set_composition_from_existing_text(start..end);
            Ok(())
        });
    }

    /// Applies one tracker-changing event: `apply` checks it against the
    /// mirror and changes the mirror only if it agrees and its effect is
    /// known. Otherwise Exo's state is unknown, and so is the state it
    /// pushed. A report that may reach Exo apart from its barrier and is
    /// still on its way may come before the event or after it.
    fn observe(&mut self, apply: impl FnOnce(&mut Self) -> Result<(), Miss>) {
        self.unmarked = false;
        let split = self
            .in_flight
            .iter()
            .any(|(_, sent)| matches!(sent, Sent::Report { split: true, .. }));
        let applied = match self.health {
            Health::Unknown => Err(Miss::Unknowable),
            _ if split => Err(Miss::Unknowable),
            _ => apply(self),
        };
        if let Err(miss) = applied {
            self.diverged |= miss == Miss::Diverged;
            self.unknown();
            self.push();
        }
    }

    /// The prediction is unknown.
    fn unknown(&mut self) {
        self.health = Health::Unknown;
        self.predicted = ExoState::default();
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
                ..
            } => self.report(&text, cursor, anchor),
            Sent::Cancel => {
                self.cancel_composition();
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
        let composing = self.health != Health::Unknown && !self.predicted.composition.is_empty();
        let cursor = selection.is_empty();
        match self.update(text, selection) {
            UpdateResult::Updated => {
                // A v3 guest reports no preedit, so it is at the first
                // equal state with no composition, not at one whose
                // composition it holds as text.
                let text = Key::of(text);
                let (mut found, mut in_step) = (false, cursor);
                for entry in &self.expected {
                    match entry {
                        Entry::Known { in_step: step, .. } if found => in_step &= *step,
                        Entry::Known { print, .. } => {
                            found = print.composition.is_empty()
                                && print.selection == Selection::Exact(selection)
                                && print.text == text;
                        }
                        Entry::Unknown => in_step = false,
                    }
                }
                Some(Applied::Matched {
                    in_step: found && in_step,
                })
            }
            UpdateResult::Reset => {
                self.dropped |= composing;
                Some(Applied::Reset { cursor })
            }
            UpdateResult::Ambiguous => Some(Applied::Reset { cursor }),
        }
    }

    // `SurroundingTextTracker`, line for line, in bytes with offset 0.

    /// `expected_updates_.emplace_back(predicted_state_, …)`, as a
    /// fingerprint, within the caps.
    fn push(&mut self) {
        if self.predicted.text.len() > TEXT_CAP {
            self.unknown();
        }
        let entry = match self.health {
            Health::Unknown => Entry::Unknown,
            health => Entry::Known {
                print: Fingerprint::of(
                    &self.predicted,
                    (health == Health::Vague).then_some(&self.vague[..]),
                ),
                in_step: false,
            },
        };
        self.expected.push_back(entry);
        self.unmarked = true;
        if self.expected.len() > HISTORY_CAP {
            self.expected.pop_front();
            self.expected[0] = Entry::Unknown;
        }
    }

    /// `CancelComposition` (STT 124-129).
    fn cancel_composition(&mut self) {
        self.predicted.composition = NO_COMPOSITION;
        self.expected.clear();
    }

    /// `Update` (STT 131-191). With offset 0, `compare_begin` is 0, so a
    /// state matches when its selection and its whole text are equal, and
    /// the match is exact. When an unknown state, or a vague one with the
    /// report's text and a selection it may have, comes before any equal
    /// state, the mirror cannot tell
    /// whether Exo matched there: the history is unknown, and so is the
    /// prediction, unless it is the report's with no composition, which
    /// Exo's is then either way.
    fn update(&mut self, text: &str, selection: Span) -> UpdateResult {
        match self.lookup(0, text, selection) {
            Lookup::Match(at) => {
                self.expected.drain(..at);
                UpdateResult::Updated
            }
            Lookup::Missing => {
                self.reset_internal(text, selection);
                UpdateResult::Reset
            }
            Lookup::Ambiguous => {
                let reported = self.exact().is_some_and(|predicted| {
                    predicted.composition.is_empty()
                        && predicted.selection == selection
                        && predicted.text == text
                });
                if !reported {
                    self.unknown();
                }
                self.expected = VecDeque::from([Entry::Unknown]);
                UpdateResult::Ambiguous
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
        self.predicted = ExoState {
            text: text.to_owned(),
            selection,
            composition: NO_COMPOSITION,
        };
        self.health = Health::Exact;
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
        let mut mirror = ExoMirror::new();
        let print = Fingerprint::of(&predicted, None);
        mirror.predicted = predicted;
        mirror.expected = VecDeque::from([Entry::Known {
            print,
            in_step: false,
        }]);
        mirror
    }

    /// A history entry is a state's if it is its exact fingerprint.
    impl PartialEq<ExoState> for Entry {
        fn eq(&self, state: &ExoState) -> bool {
            matches!(self, Entry::Known { print, .. } if *print == Fingerprint::of(state, None))
        }
    }

    /// The history, without the marks.
    fn history(mirror: &ExoMirror) -> Vec<Entry> {
        let unmarked = |entry: &Entry| match entry {
            Entry::Known { print, .. } => Entry::Known {
                print: print.clone(),
                in_step: false,
            },
            Entry::Unknown => Entry::Unknown,
        };
        mirror.expected().iter().map(unmarked).collect()
    }

    /// The prediction is the last history state, as every STT function
    /// but `CancelComposition` leaves it.
    #[track_caller]
    fn assert_pushed(mirror: &ExoMirror, predicted: &ExoState) {
        assert_eq!(mirror.predicted(), predicted);
        let back = mirror.expected().back();
        assert!(back.is_some_and(|entry| entry == predicted), "{back:?}");
        assert!(predicted.fits(), "{predicted:?}");
    }

    fn sent_report(text: &str, cursor: u32, anchor: u32) -> Sent {
        Sent::Report {
            text: text.to_owned(),
            cursor,
            anchor,
            split: false,
        }
    }

    /// Sends a report and its barrier, and lets Exo process both.
    fn report(mirror: &mut ExoMirror, text: &str, cursor: u32, anchor: u32) {
        let token = BarrierToken(1);
        mirror.sent(token, sent_report(text, cursor, anchor));
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
        let mirror = ExoMirror::new();
        let empty = state("", (0, 0), NO_COMPOSITION);
        assert_eq!(history(&mirror), std::slice::from_ref(&empty));
        assert_pushed(&mirror, &empty);
        assert_eq!(mirror.health(), Health::Exact);
        assert!(!mirror.dropped());
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
            assert_eq!(mirror.health(), Health::Exact, "{name}");
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
        assert!(history(&mirror).is_empty());
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
        mirror.sent(BarrierToken(7), sent_report("가", 0, 0));
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
        assert!(history(&mirror).is_empty());
        assert_eq!(mirror.predicted(), &state("가", (0, 0), NO_COMPOSITION));

        // Applied when sent, the report would have come before the
        // composition, which would then have stayed.
        let mut early = at(state("가", (3, 3), NO_COMPOSITION));
        report(&mut early, "가", 0, 0);
        early.preedit_string("나", "나");
        assert_eq!(early.predicted(), &state("나가", (3, 3), 0..3));
        assert!(!early.dropped());
    }

    /// `cursor_position(range)` + `commit_string(composition)` is
    /// `SetEditableSelectionRange` (TI 386-403), which keeps the
    /// composition, whatever the range: from v11, a confirm is
    /// `confirm_preedit`.
    #[test]
    fn a_cursor_position_pair_sets_the_selection_and_keeps_the_composition() {
        let composing = state(GANADA, (6, 6), 3..6);
        for ((index, anchor), expected) in [
            ((6, 6), composing.clone()),
            ((9, 0), state(GANADA, (0, 9), 3..6)),
        ] {
            let mut mirror = at(composing.clone());
            mirror.cursor_position(index, anchor);
            mirror.commit_string("나");
            assert_pushed(&mirror, &expected);
            // The pair is spent: the next commit is an insert.
            mirror.commit_string("라");
            assert!(mirror.predicted().text.contains('라'));
        }
        // With no composition, the pair commits "".
        let mut mirror = at(state(GANADA, (3, 3), NO_COMPOSITION));
        mirror.cursor_position(6, 6);
        mirror.commit_string("");
        assert_pushed(&mirror, &state(GANADA, (6, 6), NO_COMPOSITION));
    }

    /// Every echo check: an event that does not agree with the mirror
    /// diverges, which makes the prediction and the state the event pushed
    /// unknown.
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
            assert_eq!(mirror.health(), Health::Unknown, "{name}");
            assert!(mirror.take_diverged(), "{name}");
            let history = history(&mirror);
            assert!(history[0] == *start, "{name}");
            assert_eq!(history[1..], [Entry::Unknown], "{name}");
            // An unknown mirror pushes an unknown state for every event,
            // which is no divergence.
            mirror.confirm_preedit(true);
            assert!(!mirror.take_diverged(), "{name}");
            assert_eq!(mirror.expected().len(), 3, "{name}");
            assert_eq!(mirror.expected().back(), Some(&Entry::Unknown), "{name}");
        }
    }

    /// The history keeps fingerprints, so a field that gets many events
    /// of Exo's with no report, such as v11's empty confirmation for each
    /// key the input method does not handle, still holds the oldest state
    /// Exo does, which a report then matches. Past [`HISTORY_CAP`], the
    /// oldest goes, and an unknown state stands for it, which Exo could
    /// match: a report then leaves the history unknown.
    #[test]
    fn an_evicted_state_still_matches() {
        let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
        for _ in 0..100 {
            mirror.confirm_preedit(true);
        }
        mirror.commit_string("가");
        assert_eq!(mirror.expected().len(), 102);
        report(&mut mirror, "", 0, 0);
        assert_eq!(mirror.health(), Health::Exact);
        assert_eq!(mirror.expected().len(), 102, "the oldest state matched");

        for _ in 0..HISTORY_CAP {
            mirror.confirm_preedit(true);
        }
        assert_eq!(mirror.expected().len(), HISTORY_CAP);
        assert_eq!(mirror.expected().front(), Some(&Entry::Unknown));
        assert_eq!(mirror.health(), Health::Exact);
        // Exo's prediction is a report of it whether Exo matched a lost
        // state or reset; after another, the mirror cannot tell.
        report(&mut mirror, "가", 3, 3);
        assert_eq!(mirror.health(), Health::Exact);
        assert_eq!(history(&mirror), [Entry::Unknown]);
        report(&mut mirror, "나", 3, 3);
        assert_eq!(mirror.health(), Health::Unknown);
    }

    /// Text over [`TEXT_CAP`] makes the prediction unknown, and events are
    /// then neither checked nor applied, until a press's `reset` and a
    /// report.
    #[test]
    fn long_text_makes_it_unknown() {
        let full = "가".repeat(TEXT_CAP / 3);
        let mut mirror = at(state(&full, (full.len(), full.len()), NO_COMPOSITION));
        mirror.commit_string("a");
        assert_eq!(mirror.predicted().text.len(), TEXT_CAP);
        mirror.commit_string("가");
        assert_eq!(mirror.health(), Health::Unknown);
        assert_eq!(mirror.expected().back(), Some(&Entry::Unknown));
        mirror.delete_surrounding_text(30, 3);
        assert!(!mirror.take_diverged());
        report(&mut mirror, "가", 3, 3);
        assert_eq!(mirror.health(), Health::Unknown);
        cancel(&mut mirror);
        report(&mut mirror, "가", 3, 3);
        assert_pushed(&mirror, &state("가", (3, 3), NO_COMPOSITION));
    }

    /// A report heals the mirror whenever Exo must reset to it: no state
    /// Exo could match (an unknown one, or a vague one with the report's
    /// text) comes before, and none equals it. That needs no press. After
    /// a press's `reset` the history is empty, so any report before the
    /// next event heals.
    #[test]
    fn a_definite_reset_heals_without_a_press() {
        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        convert(&mut mirror, "漢字", 3);
        assert_eq!(mirror.health(), Health::Vague);
        report(&mut mirror, "가나다漢", 12, 12);
        assert_pushed(&mirror, &state("가나다漢", (12, 12), NO_COMPOSITION));
        assert_eq!(history(&mirror).len(), 1);

        let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
        mirror.delete_surrounding_text(0, 3);
        assert_eq!(mirror.health(), Health::Unknown);
        cancel(&mut mirror);
        report(&mut mirror, "라", 3, 3);
        assert_pushed(&mirror, &state("라", (3, 3), NO_COMPOSITION));
    }

    /// A report whose outcome the mirror cannot tell, as an unknown state
    /// or a vague one with its text comes first, leaves the history
    /// unknown, and the prediction too, unless it is the report's with no
    /// composition. Only a press's `reset` with no event before the next
    /// report heals that. A report that matches a state before the unknown
    /// one keeps the history from there.
    #[test]
    fn an_ambiguous_report_stays_unknown() {
        let start = state(GANADA, (9, 9), NO_COMPOSITION);
        let mut mirror = at(start.clone());
        mirror.delete_surrounding_text(0, 3);
        report(&mut mirror, GANADA, 9, 9);
        assert_eq!(mirror.health(), Health::Unknown);
        assert_eq!(mirror.expected().len(), 2);
        report(&mut mirror, "가", 3, 3);
        assert_eq!(history(&mirror), [Entry::Unknown]);
        assert_eq!(mirror.health(), Health::Unknown);
        // An event between the press's reset and the report.
        cancel(&mut mirror);
        mirror.commit_string("나");
        report(&mut mirror, "다", 3, 3);
        assert_eq!(mirror.health(), Health::Unknown);

        // A vague state with the report's text and a selection it may
        // have.
        let mut mirror = at(start);
        convert(&mut mirror, "漢字", 3);
        report(&mut mirror, "가나다漢字", 12, 12);
        assert_eq!(history(&mirror), [Entry::Unknown]);
        assert_eq!(mirror.health(), Health::Unknown);
    }

    /// A report's selection is forward. A vague one starts at the preedit
    /// cursor, and is empty there or runs to the highlight's other end, so
    /// only a report with one of those selections may match a vague state.
    /// After a conversion that a confirmation keeping the selection ends,
    /// as Japanese input methods send, the guest's report of the committed
    /// text with its cursor after it resets Exo, which heals the mirror.
    #[test]
    fn a_report_unlike_a_vague_selection_heals_it() {
        let confirmed = || {
            let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
            convert(&mut mirror, "漢字", 3);
            mirror.confirm_preedit(true);
            mirror
        };
        let mut mirror = confirmed();
        report(&mut mirror, "가나다漢字", 15, 15);
        assert_pushed(&mirror, &state("가나다漢字", (15, 15), NO_COMPOSITION));
        assert_eq!(history(&mirror).len(), 1);
        // The cursor at the highlight's end, and the highlight selected.
        for (cursor, anchor) in [(12, 12), (12, 9)] {
            let mut mirror = confirmed();
            report(&mut mirror, "가나다漢字", cursor, anchor);
            assert_eq!(mirror.health(), Health::Unknown, "{cursor}, {anchor}");
            assert_eq!(history(&mirror), [Entry::Unknown]);
        }
    }

    /// A conversion's preedit `text`, its first `selected` bytes
    /// highlighted, with the cursor at their end.
    fn convert(mirror: &mut ExoMirror, text: &str, selected: u32) {
        mirror.preedit_styling(0, selected, zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT);
        mirror.preedit_cursor(selected as i32);
        mirror.preedit_string(text, text);
    }

    /// IMA makes a composition selection from a highlight span with the
    /// cursor at one end (`ExtractCompositionText`), whose other end the
    /// wire does not carry: the selection is vague, inside the
    /// composition, and the rest of the state is known. An event that sets
    /// the selection absolutely or replaces the whole composition makes
    /// the mirror exact again; one that depends on where the selection is
    /// makes it unknown.
    #[test]
    fn a_highlight_is_vague_until_a_commit() {
        let highlight = zwp_text_input_v1::PREEDIT_STYLE_HIGHLIGHT;
        // The selection is empty at the cursor, or runs to the other end.
        for (case, style, (index, length), other) in [
            (
                "a highlight ending at the cursor",
                highlight,
                (0, 3),
                Some(0),
            ),
            ("one starting at it", highlight, (3, 3), Some(6)),
            ("one apart from it", highlight, (6, 3), None),
            (
                "an underline",
                zwp_text_input_v1::PREEDIT_STYLE_UNDERLINE,
                (0, 3),
                None,
            ),
        ] {
            let mut mirror = at(state("", (0, 0), NO_COMPOSITION));
            mirror.preedit_styling(index, length, style);
            mirror.preedit_cursor(3);
            mirror.preedit_string(GANADA, GANADA);
            let health = [Health::Exact, Health::Vague][usize::from(other.is_some())];
            assert_eq!(mirror.health(), health, "{case}");
            let vague =
                other.map(|other: usize| [Span::at(3), Span::new(other.min(3), other.max(3))]);
            let print =
                Fingerprint::of(&state(GANADA, (3, 3), 0..9), vague.as_ref().map(|v| &v[..]));
            let pushed = Entry::Known {
                print,
                in_step: false,
            };
            assert_eq!(history(&mirror).last(), Some(&pushed), "{case}");
            // The spans are spent.
            mirror.preedit_cursor(3);
            mirror.preedit_string("가", "가");
            assert_eq!(mirror.health(), Health::Exact, "{case}");
        }

        let vague = || {
            let mut mirror = at(state(GANADA, (9, 9), NO_COMPOSITION));
            convert(&mut mirror, "漢字", 3);
            mirror
        };
        let converted = state("가나다漢字", (15, 15), NO_COMPOSITION);
        // A commit replaces the composition, which holds the selection.
        let mut mirror = vague();
        mirror.commit_string("漢字");
        assert_pushed(&mirror, &converted);
        let mut mirror = vague();
        mirror.confirm_preedit(false);
        assert_pushed(&mirror, &converted);
        let mut mirror = vague();
        mirror.cursor_position(9, 9);
        mirror.commit_string("漢字");
        assert_pushed(&mirror, &state("가나다漢字", (9, 9), 9..15));
        // A confirm that keeps the selection keeps it vague, and a delete
        // over it sets it.
        let mut mirror = vague();
        mirror.confirm_preedit(true);
        assert_eq!(mirror.health(), Health::Vague);
        mirror.delete_surrounding_text(9, 6);
        assert_pushed(&mirror, &state(GANADA, (9, 9), NO_COMPOSITION));
        // A region is measured from the selection, and a commit with no
        // composition replaces it.
        let mut mirror = vague();
        mirror.set_preedit_region(-3, 3);
        assert_eq!(mirror.health(), Health::Unknown);
        let mut mirror = vague();
        mirror.confirm_preedit(true);
        mirror.commit_string("라");
        assert_eq!(mirror.health(), Health::Unknown);
        assert!(!mirror.take_diverged());
    }

    /// A report that may reach Exo apart from its barrier leaves the
    /// mirror exact if no event of Exo's comes while it is on its way.
    #[test]
    fn a_split_report_without_an_event_stays_exact() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.sent(BarrierToken(1), split_report("가", 0));
        mirror.barrier_done(BarrierToken(1));
        assert_pushed(&mirror, &state("가", (0, 0), NO_COMPOSITION));
        mirror.commit_string("나");
        assert_pushed(&mirror, &state("나가", (3, 3), NO_COMPOSITION));
    }

    /// An event of Exo's that comes while a report that may reach Exo
    /// apart from its barrier is on its way may have come before the
    /// report or after it: its state is unknown, and so is the mirror,
    /// until a press's `reset` and a report.
    #[test]
    fn a_split_report_with_an_event_is_unknown() {
        let mut mirror = at(state("가", (3, 3), NO_COMPOSITION));
        mirror.sent(BarrierToken(1), split_report("가", 0));
        mirror.commit_string("나");
        assert_eq!(mirror.health(), Health::Unknown);
        assert!(!mirror.take_diverged());
        mirror.barrier_done(BarrierToken(1));
        assert_eq!(mirror.health(), Health::Unknown);
        cancel(&mut mirror);
        report(&mut mirror, "가나", 6, 6);
        assert_pushed(&mirror, &state("가나", (6, 6), NO_COMPOSITION));
    }

    fn split_report(text: &str, cursor: u32) -> Sent {
        Sent::Report {
            text: text.to_owned(),
            cursor,
            anchor: cursor,
            split: true,
        }
    }

    /// A report on its way to Exo crosses the events since it was sent if
    /// it will name no state then, or follows a `reset`.
    #[test]
    fn a_report_crosses_when_it_will_name_no_state() {
        let mut mirror = at(state("ab", (2, 2), NO_COMPOSITION));
        mirror.commit_string("c");
        let sent = |text: &str, cursor| sent_report(text, cursor, cursor);
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
            mirror.sent(token, sent_report("가", 3, 3));
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
        let mut mirror = ExoMirror::new();
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
