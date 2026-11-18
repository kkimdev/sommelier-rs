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

use super::guest::{delete_lengths, lengths_around_cursor, resolve_preedit_cursor};
use super::guest::{GuestTxn, Preedit, TooLong};
use super::host::{self, HostSteps};
use super::serial::{CommitCount, Serial};
use super::{BarrierToken, Env, HostTi, KeyEffect, LeaseOutcome, SeatId, SurfaceId, TiId};
use crate::state::{HostContentType, V3Committed, V3Pending};
use crate::wire::Outgoing;

#[derive(Debug)]
pub(super) struct TextInput {
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

#[derive(Debug)]
enum Field {
    Unfocused,
    Focused(FocusedField),
}

#[derive(Debug)]
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
pub(super) enum LinkState {
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
#[derive(Debug, Default)]
struct Composition {
    shown: Option<Preedit>,
    assembly: Assembly,
}

/// The v1 `preedit_cursor` and `preedit_styling` selection that the next
/// preedit or preedit region applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Assembly {
    pub(super) cursor: Option<i32>,
    pub(super) selection: Option<(u32, u32)>,
}

/// What the proxy warned of, so that it warns once per text input. The
/// invariants ignore it.
#[derive(Debug, Default)]
struct Warned {
    seat_missing: bool,
    delete_range: bool,
}

impl TextInput {
    pub(super) fn new(seat: SeatId, host: HostTi, surface: Option<SurfaceId>) -> Self {
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

    pub(super) fn seat(&self) -> SeatId {
        self.seat
    }

    pub(super) fn host(&self) -> HostTi {
        self.host
    }

    pub(super) fn serial(&self) -> Serial {
        self.commits.serial()
    }

    pub(super) fn link(&self) -> LinkState {
        self.link
    }

    pub(super) fn is_attached(&self) -> bool {
        matches!(self.link, LinkState::Attached { .. })
    }

    /// The focused field's surface and committed state.
    pub(super) fn field(&self) -> Option<(SurfaceId, &V3Committed)> {
        match &self.field {
            Field::Focused(f) => Some((f.surface, &f.committed)),
            Field::Unfocused => None,
        }
    }

    pub(super) fn focused_surface(&self) -> Option<SurfaceId> {
        self.field().map(|(surface, _)| surface)
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.field().is_some_and(|(_, committed)| committed.enabled)
    }

    /// The pending state of G3 to G8, which are ignored while unfocused.
    pub(super) fn pending_mut(&mut self) -> Option<&mut V3Pending> {
        matches!(self.field, Field::Focused(_)).then_some(&mut self.pending)
    }

    /// G9: count a guest commit. The Commit arm of `step` is the only
    /// caller (test `count_bumps_only_in_the_commit_arm`).
    pub(super) fn count_commit(&mut self) {
        self.commits.bump();
    }

    /// The pending state a commit consumes, or `None` while unfocused
    /// (G9a), which leaves it alone.
    pub(super) fn take_pending(&mut self) -> Option<V3Pending> {
        let pending = self.pending_mut()?;
        Some(std::mem::take(pending))
    }

    /// G9c steps 2 to 4: apply a commit's pending state, but an enable
    /// that `enable_conflict` rejects. Returns whether the commit started a
    /// new field. A new field is never attached after the step: a disable
    /// makes the reconcile leave `Attached`, which ends the composition
    /// with no guest output, since the guest reset itself; an enable or a
    /// rejected one starts from a link that was not attached (I1a).
    pub(super) fn apply_commit(&mut self, pending: V3Pending, enable_conflict: bool) -> bool {
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
        }
        if let Some(content_type) = content_type {
            f.committed.content_type = content_type;
        }
        new_field
    }

    /// G9b and G9c: the answer to a commit, if the field is enabled: a
    /// transaction that repeats the preedit the guest shows. A preedit
    /// that does not fit is forgotten, and a bare `done` goes out.
    pub(super) fn answer(&mut self, ti: TiId) -> Vec<Outgoing> {
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
            bare_done(ti, serial)
        })
    }

    /// F1, F2 and G10: the field ends, and a new one starts on `surface`,
    /// if any, disabled and with nothing pending. Returns the old field's
    /// surface. The new field is disabled, so the step's reconcile leaves
    /// `Attached`, which ends the composition.
    pub(super) fn refocus(&mut self, surface: Option<SurfaceId>) -> Option<SurfaceId> {
        let old = self.focused_surface();
        self.pending = V3Pending::default();
        self.field = Field::focused_on(surface);
        old
    }

    /// H4 and H5 record into the assembly only while attached.
    pub(super) fn assembly_mut(&mut self) -> Option<&mut Assembly> {
        self.is_attached().then_some(&mut self.composition.assembly)
    }

    /// B3: every host composition event takes the assembly first, whatever
    /// becomes of the event.
    pub(super) fn take_assembly(&mut self) -> Assembly {
        std::mem::take(&mut self.composition.assembly)
    }

    /// The gate of every host composition event and press: the link is
    /// attached, no fence is pending, and the field is focused. The only
    /// constructor of [`Current`].
    pub(super) fn current(&mut self, ti: TiId) -> Option<Current<'_>> {
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

    pub(super) fn activate_sent(&mut self) {
        debug_assert_eq!(self.link, LinkState::Detached);
        self.link = LinkState::Attached { content_type: None };
    }

    pub(super) fn content_type_sent(&mut self, applied: HostContentType) {
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

    pub(super) fn deactivate_sent(&mut self, barrier: BarrierToken) {
        let left = self.leave_attached(LinkState::Draining { barrier });
        debug_assert!(left.is_some(), "only an attached link is deactivated");
    }

    /// R1: no live seat can carry the `deactivate` an attached link needs.
    pub(super) fn abandoned(&mut self) {
        let left = self.leave_attached(LinkState::Detached);
        debug_assert!(left.is_some(), "only an attached link is abandoned");
    }

    /// H2: Exo detached its input method itself. Returns `None` if the
    /// link was not attached (H2'), and otherwise whether the guest showed
    /// a preedit, which it must now be told to clear.
    pub(super) fn exo_detached(&mut self) -> Option<bool> {
        let left = self.leave_attached(LinkState::Detached)?;
        Some(left.shown.is_some())
    }

    /// F6: the host deleted the v1 object, so nothing more goes to it.
    pub(super) fn host_deleted(&mut self) {
        self.leave_attached(LinkState::Detached);
    }

    /// W1 and W2: the barrier `token` returned. It ends the drain or the
    /// fence it is, in any link state.
    pub(super) fn barrier_done(&mut self, token: BarrierToken) {
        if self.link == (LinkState::Draining { barrier: token }) {
            self.link = LinkState::Detached;
        }
        if self.fence == Some(token) {
            self.fence = None;
        }
    }

    /// Whether R2's missing seat is warned of now: once per text input.
    pub(super) fn first_seat_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned.seat_missing, true)
    }

    /// Whether H9's delete around no cursor is warned of now: once per
    /// text input.
    pub(super) fn first_delete_range_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned.delete_range, true)
    }

    /// I1a, I1b and I2 of design 3.1.
    pub(super) fn assert_invariants(&self, ti: TiId, env: &impl Env) {
        let attached = self.is_attached();
        assert!(!attached || self.is_enabled(), "I1a: {ti:?} {self:?}");
        let steps = host::reconcile(&host::target(self, env), self.link);
        assert_eq!(steps, HostSteps::default(), "I1b: {ti:?} {self:?}");
        let Composition { shown, assembly } = &self.composition;
        if shown.is_some() || *assembly != Assembly::default() {
            assert!(attached, "I2: {ti:?} composes while not attached");
        }
        if shown.is_some() {
            assert!(
                self.fence.is_none(),
                "I2: {ti:?} shows a preedit behind a fence"
            );
        }
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

/// The transaction of a recovered key (X2'): a lone `done`.
fn bare_done(ti: TiId, serial: Serial) -> Vec<Outgoing> {
    GuestTxn::default()
        .encode(ti, false, serial)
        .expect("a bare done has no string")
}

/// Proof that a text input's composition is current (the gate of
/// [`TextInput::current`]). Its fields are private to this file, so no
/// other module can make one. Each method plans the transaction of one
/// row.
pub(super) struct Current<'a> {
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
pub(super) enum DeleteDrop {
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
pub(super) enum RegionDrop {
    NoSurroundingText,
    NotAroundCursor,
}

/// What a confirmation (X2, X2') does.
pub(super) enum Confirm<'a> {
    Commit(Planned<'a>),
    RecoverHeldKey(KeyEffect),
}

impl<'a> Current<'a> {
    pub(super) fn shown(&self) -> Option<&Preedit> {
        self.composition.shown.as_ref()
    }

    fn plan(self, txn: GuestTxn, ahead: bool, lease: LeaseOutcome) -> Planned<'a> {
        Planned {
            cur: self,
            txn,
            ahead,
            lease,
            fence: None,
        }
    }

    /// H7. A cleared preedit with a commit text is Exo's reset commit.
    pub(super) fn preedit(self, text: String, commit: String, assembly: Assembly) -> Planned<'a> {
        let shown = self.shown().is_some();
        if text.is_empty() && shown && !commit.is_empty() {
            let txn = GuestTxn {
                commit: Some(commit),
                ..GuestTxn::default()
            };
            return self.plan(txn, true, LeaseOutcome::End);
        }
        let lease = match text.is_empty() && shown {
            true => LeaseOutcome::ArmIfBackspaceHeld,
            false => LeaseOutcome::End,
        };
        let cursor = resolve_preedit_cursor(&text, assembly.selection, assembly.cursor);
        let preedit = Preedit::new(text, cursor);
        let txn = GuestTxn {
            preedit,
            ..GuestTxn::default()
        };
        self.plan(txn, false, lease)
    }

    /// H8.
    pub(super) fn host_commit(self, text: String) -> Planned<'a> {
        let ahead = self.shown().is_some() || !text.is_empty();
        let txn = GuestTxn {
            commit: Some(text),
            ..GuestTxn::default()
        };
        self.plan(txn, ahead, LeaseOutcome::End)
    }

    /// H9: a delete measured against the surrounding text the guest last
    /// committed (INVARIANTS "Exo's deletes are absolute and stand alone").
    pub(super) fn delete(self, index: i32, length: u32) -> Result<Planned<'a>, DeleteDrop> {
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
        Ok(self.plan(txn, true, LeaseOutcome::End))
    }

    /// X1: Exo's cursor-relative region of the committed surrounding text
    /// becomes the preedit. It is not gated by the text being ahead, on
    /// purpose (INVARIANTS "Exo's deletes are absolute and stand alone").
    pub(super) fn region(
        self,
        index: i32,
        length: u32,
        assembly: Assembly,
    ) -> Result<Planned<'a>, RegionDrop> {
        let Some((text, cursor, _)) = &self.committed.surrounding_text else {
            return Err(RegionDrop::NoSurroundingText);
        };
        let start = i64::from(*cursor) + i64::from(index);
        let (before, after) = lengths_around_cursor(text, *cursor, start, length)
            .ok_or(RegionDrop::NotAroundCursor)?;
        // The cursor is a character boundary of `text`, at most its length.
        let cursor = *cursor as usize;
        let region = &text[cursor - before as usize..cursor + after as usize];
        let cursor = assembly.cursor.or(Some(before as i32));
        let cursor = resolve_preedit_cursor(region, assembly.selection, cursor);
        // An empty region deletes nothing and leaves no preedit, so the
        // guest has no edit to report.
        let ahead = !region.is_empty();
        let txn = GuestTxn {
            delete: Some((before, after)),
            preedit: Preedit::new(region.to_string(), cursor),
            ..GuestTxn::default()
        };
        Ok(self.plan(txn, ahead, LeaseOutcome::End))
    }

    /// X2 commits the shown preedit; X2', with none shown, recovers the
    /// key the input method took.
    pub(super) fn confirm(self) -> Confirm<'a> {
        match self.commit_shown() {
            Ok(planned) => Confirm::Commit(planned),
            Err(cur) => Confirm::RecoverHeldKey(KeyEffect::RecoverHeldKey {
                ti: cur.ti,
                seat: cur.seat,
                done: bare_done(cur.ti, cur.serial),
            }),
        }
    }

    /// F4: a press commits the shown preedit, takes the assembly, and
    /// fences the host's `reset` with `fence`. `None` if nothing is shown.
    pub(super) fn press_commit(self, fence: BarrierToken) -> Option<Planned<'a>> {
        let mut planned = self.commit_shown().ok()?;
        planned.cur.composition.assembly = Assembly::default();
        planned.fence = Some(fence);
        Some(planned)
    }

    fn commit_shown(self) -> Result<Planned<'a>, Self> {
        let Some(text) = self.shown().map(|shown| shown.text().to_string()) else {
            return Err(self);
        };
        let txn = GuestTxn {
            commit: Some(text),
            ..GuestTxn::default()
        };
        Ok(self.plan(txn, true, LeaseOutcome::End))
    }
}

/// A planned transaction and the change it makes. [`Self::apply`] encodes
/// it and makes the change; dropping it changes nothing.
#[must_use = "apply it, or drop it to change nothing"]
pub(super) struct Planned<'a> {
    cur: Current<'a>,
    txn: GuestTxn,
    /// The guest's text gains an edit it has yet to report.
    ahead: bool,
    lease: LeaseOutcome,
    /// The press's reset fence (F4).
    fence: Option<BarrierToken>,
}

impl Planned<'_> {
    /// Encode the transaction and, if it fits, make its change: the guest
    /// then shows the transaction's preedit. Returns its events and the
    /// lease effect; if it does not fit, nothing changes.
    pub(super) fn apply(self) -> Result<(Vec<Outgoing>, KeyEffect), TooLong> {
        let Planned {
            cur,
            txn,
            ahead,
            lease,
            fence,
        } = self;
        let events = txn.encode(cur.ti, cur.composition.shown.is_some(), cur.serial)?;
        cur.composition.shown = txn.preedit;
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
