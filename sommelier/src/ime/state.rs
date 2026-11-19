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

//! The state of one text input (design 3.1). Its fields are private to
//! this file: every change is a named method, and a host composition event
//! or a press can change what the guest is shown only through
//! [`Current`], which only [`TextInput::current`] makes.

use std::ops::Range;

use super::guest::{delete_lengths, lengths_around_cursor, resolve_preedit_cursor};
use super::guest::{GuestTxn, Preedit, TooLong};
use super::host;
use super::serial::{CommitCount, Serial};
use super::{BarrierToken, Env, HostTi, KeyEffect, LeaseOutcome, SeatId, SurfaceId, TiId};
use crate::state::{HostContentType, V3Committed, V3Pending};
use crate::wire::Outgoing;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextInput {
    seat: SeatId,
    host: HostTi,
    commits: CommitCount,
    /// The requests since the guest's latest commit.
    pending: V3Pending,
    field: Field,
    link: LinkState,
    /// The reset fence of a press (F4) until its `done` (W2). It lives
    /// beside the link and survives every change of it.
    fence: Option<BarrierToken>,
    composition: Composition,
    warned: Warned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Field {
    Unfocused,
    Focused(FocusedField),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FocusedField {
    surface: SurfaceId,
    /// The committed state. Its `enabled` lives here, so ending or
    /// refocusing the field clears it by construction (B7).
    committed: V3Committed,
    text: ExoText,
}

/// Whether Exo's prediction of the text may hold input-method edits the
/// guest has not yet reported in a surrounding text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExoText {
    Synced,
    Ahead,
}

/// What the host text input has applied, by Exo's rules (INVARIANTS "The
/// host gets each content type once it can apply it, and only then").
/// Only the transitions of [`TextInput`] change it, and only
/// [`TextInput::leave_attached`] leaves `Attached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// No input method is attached, and none is about to be.
    Detached,
    /// `activate` was sent. A later v1 `leave` is Exo's own detach (H2).
    Attached {
        /// The content type the input method applied, `None` until sent.
        content_type: Option<HostContentType>,
    },
    /// `deactivate` was sent, and `barrier` drains Exo's events from
    /// before it.
    Draining { barrier: BarrierToken },
}

/// What the guest shows, and the v1 pieces of the next host event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Composition {
    shown: Option<Preedit>,
    /// The bytes before and after the committed cursor that preedit
    /// regions took since the guest last reported its surrounding text
    /// (X1). The committed text still holds them, but the guest no longer
    /// does. `Some` only while the text is `Ahead`: a report clears it
    /// ([`TextInput::apply_commit`]), and so does every end of the
    /// composition, which gives the bytes back as text, a reset commit
    /// (H7) included. Any other host preedit keeps it.
    taken: Option<(u32, u32)>,
    /// Where Exo's cursor is in `shown`, while Exo holds it as its
    /// composition, inline in its text at the guest's cursor (X1). `None`
    /// with nothing shown, and for a region's preedit once the guest
    /// reported text without the region's bytes, which Exo cannot match,
    /// so it drops its composition (E4).
    held: Option<u32>,
    assembly: Assembly,
}

/// The v1 `preedit_cursor` and `preedit_styling` selection that the next
/// preedit or preedit region applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Assembly {
    pub(crate) cursor: Option<i32>,
    pub(crate) selection: Option<(u32, u32)>,
}

/// What the proxy warned of, so that it warns once per text input. The
/// invariants ignore it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Warned {
    seat_missing: bool,
    delete_range: bool,
    /// The deferral last logged.
    deferral: Option<Deferral>,
}

/// Why an activation the target needs waits (R2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deferral {
    /// The previous activation still drains.
    Draining,
    /// No live host seat can carry it.
    NoSeat,
}

impl TextInput {
    pub(crate) fn new(seat: SeatId, host: HostTi, surface: Option<SurfaceId>) -> Self {
        Self {
            seat,
            host,
            commits: CommitCount::default(),
            pending: V3Pending::default(),
            field: Field::focused_on(surface),
            link: LinkState::Detached,
            fence: None,
            composition: Composition::default(),
            warned: Warned::default(),
        }
    }

    pub(crate) fn seat(&self) -> SeatId {
        self.seat
    }

    pub(crate) fn host(&self) -> HostTi {
        self.host
    }

    pub(crate) fn serial(&self) -> Serial {
        self.commits.serial()
    }

    pub(crate) fn link(&self) -> LinkState {
        self.link
    }

    pub(crate) fn is_attached(&self) -> bool {
        matches!(self.link, LinkState::Attached { .. })
    }

    /// The focused field's surface and committed state.
    pub(crate) fn field(&self) -> Option<(SurfaceId, &V3Committed)> {
        match &self.field {
            Field::Focused(f) => Some((f.surface, &f.committed)),
            Field::Unfocused => None,
        }
    }

    pub(crate) fn focused_surface(&self) -> Option<SurfaceId> {
        self.field().map(|(surface, _)| surface)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.field().is_some_and(|(_, committed)| committed.enabled)
    }

    /// The pending state of G3 to G8, which are ignored while unfocused.
    pub(crate) fn pending_mut(&mut self) -> Option<&mut V3Pending> {
        matches!(self.field, Field::Focused(_)).then_some(&mut self.pending)
    }

    /// G9: count a guest commit. The Commit arm of `step` is the only
    /// caller (test `count_bumps_only_in_the_commit_arm`).
    pub(crate) fn count_commit(&mut self) {
        self.commits.bump();
    }

    /// The pending state a commit consumes, or `None` while unfocused
    /// (G9a), which leaves it alone.
    pub(crate) fn take_pending(&mut self) -> Option<V3Pending> {
        let pending = self.pending_mut()?;
        Some(std::mem::take(pending))
    }

    /// G9c steps 2 to 4: apply a commit's pending state, but an enable
    /// that `enable_conflict` rejects. Returns whether the commit started a
    /// new field. A new field is never attached after the step: a disable
    /// makes the reconcile leave `Attached`, which ends the composition
    /// with no guest output, since the guest reset itself; an enable or a
    /// rejected one starts from a link that was not attached (I1a).
    pub(crate) fn apply_commit(&mut self, pending: V3Pending, enable_conflict: bool) -> bool {
        let Field::Focused(f) = &mut self.field else {
            debug_assert!(false, "a commit applies only to a focused field");
            return false;
        };
        let V3Pending {
            enabled,
            surrounding_text,
            content_type,
            cursor_rect: _,
        } = pending;
        let mut new_field = false;
        if let Some(enabled) = enabled {
            new_field = enable_conflict || enabled != f.committed.enabled;
            if !enable_conflict {
                f.committed.enabled = enabled;
            }
        }
        if let Some(surrounding_text) = surrounding_text {
            f.committed.surrounding_text = surrounding_text;
            f.text = ExoText::Synced;
            // A text without the bytes regions took names no state Exo
            // predicted, so Exo drops its composition (E4).
            if self.composition.taken.take().is_some() {
                self.composition.held = None;
            }
        }
        if let Some(content_type) = content_type {
            f.committed.content_type = content_type;
        }
        new_field
    }

    /// G9b and G9c: the answer to a commit, if the field is enabled: a
    /// transaction that repeats the preedit the guest shows. A preedit
    /// that does not fit is forgotten, and a bare `done` goes out.
    pub(crate) fn answer(&mut self, ti: TiId) -> Vec<Outgoing> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let serial = self.serial();
        let txn = GuestTxn {
            preedit: self.composition.shown.clone(),
            ..GuestTxn::default()
        };
        txn.encode(ti, false, serial).unwrap_or_else(|TooLong| {
            log::warn!(
                "Forgetting the preedit of text input {}: it no longer fits",
                ti.0
            );
            self.composition.shown = None;
            self.composition.taken = None;
            self.composition.held = None;
            vec![bare_done(ti, serial)]
        })
    }

    /// Show `preedit` in place of the shown one, as no input can: every
    /// preedit a plan shows fit a message once, so only this reaches the
    /// answer that forgets one (G9c).
    #[cfg(test)]
    pub(crate) fn replace_shown(&mut self, preedit: Preedit) {
        assert!(self.composition.shown.is_some(), "a preedit shows");
        self.composition.shown = Some(preedit);
    }

    /// F1, F2 and G10: the field ends, and a new one starts on `surface`,
    /// if any, disabled and with nothing pending. Returns the old field's
    /// surface. The new field is disabled, so the step's reconcile leaves
    /// `Attached`, which ends the composition.
    pub(crate) fn refocus(&mut self, surface: Option<SurfaceId>) -> Option<SurfaceId> {
        let old = self.focused_surface();
        self.pending = V3Pending::default();
        self.field = Field::focused_on(surface);
        old
    }

    /// H4 and H5 record into the assembly only while attached.
    pub(crate) fn assembly_mut(&mut self) -> Option<&mut Assembly> {
        self.is_attached().then_some(&mut self.composition.assembly)
    }

    /// B3: take the assembly, which a host composition event or a press
    /// used up (design H9).
    pub(crate) fn take_assembly(&mut self) -> Assembly {
        std::mem::take(&mut self.composition.assembly)
    }

    /// The gate of every host composition event and press: the link is
    /// attached, no fence is pending, and the field is focused. The only
    /// constructor of [`Current`].
    pub(crate) fn current(&mut self, ti: TiId) -> Option<Current<'_>> {
        if !self.is_attached() || self.fence.is_some() {
            return None;
        }
        let Field::Focused(f) = &mut self.field else {
            return None;
        };
        Some(Current {
            ti,
            seat: self.seat,
            serial: self.commits.serial(),
            committed: &f.committed,
            text: &mut f.text,
            composition: &mut self.composition,
            fence: &mut self.fence,
        })
    }

    pub(crate) fn activate_sent(&mut self) {
        debug_assert_eq!(self.link, LinkState::Detached);
        self.link = LinkState::Attached { content_type: None };
    }

    pub(crate) fn content_type_sent(&mut self, applied: HostContentType) {
        debug_assert!(self.is_attached());
        self.link = LinkState::Attached {
            content_type: Some(applied),
        };
    }

    /// The one way out of `Attached`, into `next`. It ends the composition
    /// with the link, since one exists only while attached (I2), and
    /// returns it; if the link was not attached, it changes nothing and
    /// returns `None`.
    fn leave_attached(&mut self, next: LinkState) -> Option<Composition> {
        debug_assert!(!matches!(next, LinkState::Attached { .. }));
        if !self.is_attached() {
            return None;
        }
        self.link = next;
        Some(std::mem::take(&mut self.composition))
    }

    pub(crate) fn deactivate_sent(&mut self, barrier: BarrierToken) {
        let left = self.leave_attached(LinkState::Draining { barrier });
        debug_assert!(left.is_some(), "only an attached link is deactivated");
    }

    /// R1: no live seat can carry the `deactivate` an attached link needs.
    pub(crate) fn abandoned(&mut self) {
        let left = self.leave_attached(LinkState::Detached);
        debug_assert!(left.is_some(), "only an attached link is abandoned");
    }

    /// H2: Exo detached its input method itself. Returns `None` if the
    /// link was not attached (H2'), and otherwise whether the guest showed
    /// a preedit, which it must now be told to clear.
    pub(crate) fn exo_detached(&mut self) -> Option<bool> {
        let left = self.leave_attached(LinkState::Detached)?;
        Some(left.shown.is_some())
    }

    /// F6: the host deleted the v1 object, so nothing more goes to it.
    pub(crate) fn host_deleted(&mut self) {
        self.leave_attached(LinkState::Detached);
    }

    /// W1 and W2: the barrier `token` returned. It ends the drain or the
    /// fence it is, in any link state.
    pub(crate) fn barrier_done(&mut self, token: BarrierToken) {
        if self.link == (LinkState::Draining { barrier: token }) {
            self.link = LinkState::Detached;
        }
        if self.fence == Some(token) {
            self.fence = None;
        }
    }

    /// Note the activation's current deferral, and whether it started or
    /// changed, which is when it is logged.
    pub(crate) fn deferral_changed(&mut self, deferral: Option<Deferral>) -> bool {
        let last = std::mem::replace(&mut self.warned.deferral, deferral);
        deferral.is_some() && deferral != last
    }

    /// Whether R2's missing seat is warned of now: once per text input.
    pub(crate) fn first_seat_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned.seat_missing, true)
    }

    /// Whether H9's delete around no cursor is warned of now: once per
    /// text input.
    pub(crate) fn first_delete_range_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned.delete_range, true)
    }

    /// The barriers this text input waits for: its drain and its fence.
    pub(crate) fn barriers(&self) -> impl Iterator<Item = BarrierToken> {
        let drain = match self.link {
            LinkState::Draining { barrier } => Some(barrier),
            LinkState::Detached | LinkState::Attached { .. } => None,
        };
        drain.into_iter().chain(self.fence)
    }

    /// I1a, I1b and I2 of design 3.1, and that an unfocused text input has
    /// nothing pending.
    pub(crate) fn assert_invariants(&self, ti: TiId, env: &impl Env) {
        let attached = self.is_attached();
        assert!(!attached || self.is_enabled(), "I1a: {ti:?} {self:?}");
        // I1b: the link matches the target, but while it drains, while an
        // activation waits for a live seat (R2), or while it is attached
        // with a dead seat.
        let target = host::target(self, env);
        match (target.surface, self.link) {
            (_, LinkState::Draining { .. }) | (None, LinkState::Detached) => {}
            (Some(_), LinkState::Detached) => {
                assert_eq!(target.seat, None, "I1b: {ti:?} can activate: {self:?}")
            }
            (Some(_), LinkState::Attached { content_type }) => assert_eq!(
                content_type,
                Some(target.content_type),
                "I1b: {ti:?} lacks its content type"
            ),
            (None, LinkState::Attached { .. }) => {
                panic!("I1b: {ti:?} is attached with no target: {self:?}")
            }
        }
        let Composition {
            shown,
            taken,
            held,
            assembly,
        } = &self.composition;
        if shown.is_some() || taken.is_some() || *assembly != Assembly::default() {
            assert!(attached, "I2: {ti:?} composes while not attached");
        }
        if taken.is_some() {
            let ahead = matches!(&self.field, Field::Focused(f) if f.text == ExoText::Ahead);
            assert!(
                ahead,
                "I2: {ti:?} keeps taken bytes past a report: {self:?}"
            );
        }
        if let Some(at) = held {
            let at = *at as usize;
            let holds = shown
                .as_ref()
                .is_some_and(|p| p.text().is_char_boundary(at));
            assert!(holds, "I2: {ti:?} holds no preedit at {at}: {self:?}");
        }
        if shown.is_some() {
            assert!(
                self.fence.is_none(),
                "I2: {ti:?} shows a preedit behind a fence"
            );
        }
        if matches!(self.field, Field::Unfocused) {
            assert_eq!(self.pending, V3Pending::default(), "{ti:?} is unfocused");
        }
    }
}

/// What the handler tests of the storage switch's window set up and read
/// directly (`test_support::text_input` and the tests of
/// `handler/text_input.rs`), as no peer can. Each keeps the state as it
/// finds it otherwise, legal or not, as those tests' fixtures did.
#[cfg(test)]
impl TextInput {
    /// Start the commit count at `commits`, as if the guest had made them.
    pub(crate) fn set_commits_for_test(&mut self, commits: u32) {
        self.commits = CommitCount::starting_at(commits);
    }

    /// The committed state of the focused field.
    pub(crate) fn committed_mut_for_test(&mut self) -> Option<&mut V3Committed> {
        match &mut self.field {
            Field::Focused(f) => Some(&mut f.committed),
            Field::Unfocused => None,
        }
    }

    pub(crate) fn set_link_for_test(&mut self, link: LinkState) {
        self.link = link;
    }

    /// Show `text` with the cursor at its end, which Exo holds there, or
    /// show nothing for empty `text`.
    pub(crate) fn show_for_test(&mut self, text: &str) {
        let end = text.len() as i32;
        self.composition.shown = Preedit::new(text.to_string(), (end, end));
        self.composition.held = self.composition.shown.as_ref().map(|_| end as u32);
    }

    /// The text of the preedit the guest shows.
    pub(crate) fn shown_for_test(&self) -> Option<&str> {
        self.composition.shown.as_ref().map(Preedit::text)
    }

    pub(crate) fn pending_for_test(&self) -> &V3Pending {
        &self.pending
    }

    pub(crate) fn assembly_for_test(&self) -> Assembly {
        self.composition.assembly
    }

    /// Record `assembly`, whatever the link.
    pub(crate) fn set_assembly_for_test(&mut self, assembly: Assembly) {
        self.composition.assembly = assembly;
    }

    pub(crate) fn warned_for_test(&self) -> (bool, bool) {
        (self.warned.seat_missing, self.warned.delete_range)
    }
}

impl Field {
    fn focused_on(surface: Option<SurfaceId>) -> Self {
        match surface {
            Some(surface) => Self::Focused(FocusedField {
                surface,
                committed: V3Committed::default(),
                text: ExoText::Synced,
            }),
            None => Self::Unfocused,
        }
    }
}

/// A lone `done`: the transaction of a recovered key (X2').
fn bare_done(ti: TiId, serial: Serial) -> Outgoing {
    let mut txn = GuestTxn::default()
        .encode(ti, false, serial)
        .expect("a bare done has no string");
    debug_assert_eq!(txn.len(), 1);
    txn.remove(0)
}

/// Proof that a text input's composition is current (the gate of
/// [`TextInput::current`]). Its fields are private to this file, so no
/// other module can make one. Each method plans the transaction of one
/// row.
pub(crate) struct Current<'a> {
    ti: TiId,
    seat: SeatId,
    serial: Serial,
    committed: &'a V3Committed,
    text: &'a mut ExoText,
    composition: &'a mut Composition,
    fence: &'a mut Option<BarrierToken>,
}

/// Why a host delete (H9) is dropped.
#[derive(Debug)]
pub(crate) enum DeleteDrop {
    NoSurroundingText,
    Selection,
    /// A non-negative index is measured against Exo's prediction of the
    /// text, which holds an edit the guest has not reported.
    TextAhead,
    NotAroundCursor,
    Empty,
}

/// Why a preedit region (X1') is dropped.
#[derive(Debug)]
pub(crate) enum RegionDrop {
    NoSurroundingText,
    NotAroundCursor,
    /// The region does not cover the preedit Exo holds, whose rest Exo
    /// would leave as text.
    Uncovered,
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

/// What a confirmation (X2, X2') does.
pub(crate) enum Confirm<'a> {
    Commit(Planned<'a>),
    RecoverHeldKey(KeyEffect),
}

impl<'a> Current<'a> {
    pub(crate) fn shown(&self) -> Option<&Preedit> {
        self.composition.shown.as_ref()
    }

    /// Plan `txn`, after which regions have taken `taken`.
    fn plan(
        self,
        txn: GuestTxn,
        ahead: bool,
        taken: Option<(u32, u32)>,
        lease: LeaseOutcome,
    ) -> Planned<'a> {
        Planned {
            cur: self,
            txn,
            ahead,
            taken,
            held: None,
            lease,
            fence: None,
        }
    }

    /// H7. A cleared preedit with a commit text is Exo's reset commit,
    /// which gives the bytes regions took back as text, as every edit
    /// that ends the composition does. A preedit keeps them.
    pub(crate) fn preedit(self, text: String, commit: String, assembly: Assembly) -> Planned<'a> {
        let shown = self.shown().is_some();
        if text.is_empty() && shown && !commit.is_empty() {
            let txn = GuestTxn {
                commit: Some(commit),
                ..GuestTxn::default()
            };
            return self.plan(txn, true, None, LeaseOutcome::End);
        }
        let taken = self.composition.taken;
        let lease = match text.is_empty() && shown {
            true => LeaseOutcome::ArmIfBackspaceHeld,
            false => LeaseOutcome::End,
        };
        // Exo's cursor is where it sent the preedit's, or else, as no
        // cursor of its own converted, at the end.
        let held = assembly
            .cursor
            .and_then(|at| u32::try_from(at).ok())
            .filter(|&at| text.is_char_boundary(at as usize))
            .unwrap_or(text.len() as u32);
        let cursor = resolve_preedit_cursor(&text, assembly.selection, assembly.cursor);
        let preedit = Preedit::new(text, cursor);
        let held = preedit.as_ref().map(|_| held);
        let txn = GuestTxn {
            preedit,
            ..GuestTxn::default()
        };
        let mut planned = self.plan(txn, false, taken, lease);
        planned.held = held;
        planned
    }

    /// H8. An empty commit is no edit, whether or not a preedit shows, so
    /// it leaves Exo's prediction of the text where it was.
    pub(crate) fn host_commit(self, text: String) -> Planned<'a> {
        let ahead = !text.is_empty();
        let txn = GuestTxn {
            commit: Some(text),
            ..GuestTxn::default()
        };
        self.plan(txn, ahead, None, LeaseOutcome::End)
    }

    /// H9: a delete measured against the surrounding text the guest last
    /// committed (INVARIANTS "Exo's deletes are absolute and stand alone").
    pub(crate) fn delete(self, index: i32, length: u32) -> Result<Planned<'a>, DeleteDrop> {
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
    pub(crate) fn region(
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

    /// X2 commits the shown preedit; X2', with none shown, recovers the
    /// key the input method took.
    pub(crate) fn confirm(self) -> Confirm<'a> {
        if self.shown().is_none() {
            return Confirm::RecoverHeldKey(KeyEffect::RecoverHeldKey {
                ti: self.ti,
                seat: self.seat,
                done: bare_done(self.ti, self.serial),
            });
        }
        Confirm::Commit(self.commit_shown())
    }

    /// F4: a press commits the shown preedit, and fences the host's
    /// `reset` with `fence`.
    pub(crate) fn press_commit(self, fence: BarrierToken) -> Planned<'a> {
        debug_assert!(self.shown().is_some(), "a press commits a shown preedit");
        let mut planned = self.commit_shown();
        planned.fence = Some(fence);
        planned
    }

    /// The commit of the shown preedit (X1, X2, F4).
    fn commit_shown(self) -> Planned<'a> {
        let text = self.shown().map(|shown| shown.text().to_string());
        let txn = GuestTxn {
            commit: text,
            ..GuestTxn::default()
        };
        self.plan(txn, true, None, LeaseOutcome::End)
    }
}

/// A planned transaction and the change it makes. [`Self::apply`] encodes
/// it and makes the change; dropping it changes nothing.
#[must_use = "apply it, or drop it to change nothing"]
pub(crate) struct Planned<'a> {
    cur: Current<'a>,
    txn: GuestTxn,
    /// The guest's text gains an edit it has yet to report.
    ahead: bool,
    /// The bytes regions have taken after the transaction.
    taken: Option<(u32, u32)>,
    /// Exo's cursor in the transaction's preedit, if Exo holds it.
    held: Option<u32>,
    lease: LeaseOutcome,
    /// The press's reset fence (F4).
    fence: Option<BarrierToken>,
}

impl Planned<'_> {
    /// Encode the transaction and, if it fits, make its change: the guest
    /// then shows the transaction's preedit. Returns its events and the
    /// lease effect; if it does not fit, nothing changes.
    pub(crate) fn apply(self) -> Result<(Vec<Outgoing>, KeyEffect), TooLong> {
        let Planned {
            cur,
            txn,
            ahead,
            taken,
            held,
            lease,
            fence,
        } = self;
        let events = txn.encode(cur.ti, cur.composition.shown.is_some(), cur.serial)?;
        cur.composition.shown = txn.preedit;
        cur.composition.taken = taken;
        cur.composition.held = held;
        if ahead {
            *cur.text = ExoText::Ahead;
        }
        if fence.is_some() {
            // The gate admitted the press, so no fence was pending.
            *cur.fence = fence;
        }
        let lease = KeyEffect::Lease {
            ti: cur.ti,
            seat: cur.seat,
            outcome: lease,
        };
        Ok((events, lease))
    }
}
