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

use super::exo_view::{Edit, ExoView, Frame, Unmeasured};
use super::guest::{resolve_preedit_cursor, GuestTxn, Preedit, TooLong};
#[cfg(test)]
use super::host;
use super::host::HostContentType;
use super::serial::{CommitCount, Serial};
#[cfg(test)]
use super::Env;
use super::{BarrierToken, HostTi, KeyEffect, LeaseOutcome, SeatId, SurfaceId, TiId};
use crate::wire::Outgoing;

/// A text-input-v3 content type: `(hint, purpose)`.
pub(super) type ContentType = (u32, u32);
/// Text-input-v3 surrounding text: `(text, cursor, anchor)`.
pub(super) type SurroundingText = (String, i32, i32);
/// A text-input-v3 cursor rectangle: `(x, y, width, height)`.
pub(super) type CursorRect = (i32, i32, i32, i32);

/// The text-input-v3 requests a guest made since its most recent commit,
/// each `None` until made.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct V3Pending {
    pub(crate) enabled: Option<bool>,
    pub(crate) surrounding_text: Option<Option<SurroundingText>>,
    pub(crate) content_type: Option<ContentType>,
    pub(crate) cursor_rect: Option<CursorRect>,
}

impl V3Pending {
    /// The pending state after `enable` or `disable`, which reset every
    /// other piece of state. The host's v1 has no unset cursor rectangle,
    /// so an empty one stands for it.
    pub(crate) fn reset(enabled: bool) -> Self {
        Self {
            enabled: Some(enabled),
            surrounding_text: Some(None),
            content_type: Some(ContentType::default()),
            cursor_rect: Some(CursorRect::default()),
        }
    }
}

/// The text-input-v3 state a guest has committed, but the cursor
/// rectangle, which only the host keeps.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct V3Committed {
    pub(crate) enabled: bool,
    pub(crate) surrounding_text: Option<SurroundingText>,
    pub(crate) content_type: ContentType,
}

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
    /// Exo's text, which the host text input keeps for its whole life, so
    /// it lives beside the link too.
    exo: ExoView,
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

/// What the guest shows, and the v1 pieces of the next host event. What
/// Exo holds is the Exo mirror's ([`ExoView`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Composition {
    shown: Option<Preedit>,
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
    /// The deferral last logged.
    deferral: Option<Deferral>,
}

/// Why an activation the target needs waits (R2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Deferral {
    /// The previous activation still drains.
    Draining,
    /// No live host seat can carry it.
    NoSeat,
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
            exo: ExoView::new(),
            composition: Composition::default(),
            warned: Warned::default(),
        }
    }

    pub(super) fn seat(&self) -> SeatId {
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
    pub(super) fn count_commit(&mut self) {
        self.commits.bump();
    }

    pub(super) fn exo(&self) -> &ExoView {
        &self.exo
    }

    pub(super) fn exo_mut(&mut self) -> &mut ExoView {
        &mut self.exo
    }

    /// The preedit the guest shows.
    pub(super) fn shown(&self) -> Option<&Preedit> {
        self.composition.shown.as_ref()
    }

    /// Whether the core withholds the guest's report `report`
    /// (`ExoView::withholds`).
    pub(super) fn withholds_report(&mut self, report: &(String, u32, u32)) -> bool {
        self.exo.withholds(report, self.composition.shown.as_ref())
    }

    /// After a tracker-changing host event, with `frame` the one made
    /// before it and `edit` what reached the guest (`ExoView::after`).
    pub(super) fn exo_after(&mut self, frame: Option<&Frame>, edit: Option<Edit>) {
        self.exo.after(frame, edit, self.composition.shown.as_ref());
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
        if new_field {
            self.exo.step_lost();
        }
        if let Some(surrounding_text) = surrounding_text {
            f.committed.surrounding_text = surrounding_text;
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
            let done = GuestTxn::default().encode(ti, false, serial);
            done.expect("a bare done has no string")
        })
    }

    /// Show `preedit` in place of the shown one, as no input can: every
    /// preedit a plan shows fit a message once, so only this reaches the
    /// answer that forgets one (G9c).
    #[cfg(test)]
    pub(super) fn replace_shown(&mut self, preedit: Preedit) {
        assert!(self.composition.shown.is_some(), "a preedit shows");
        self.composition.shown = Some(preedit);
    }

    /// F1, F2 and G10: the field ends, and a new one starts on `surface`,
    /// if any, disabled and with nothing pending. Returns the old field's
    /// surface. The new field is disabled, so the step's reconcile leaves
    /// `Attached`, which ends the composition.
    pub(super) fn refocus(&mut self, surface: Option<SurfaceId>) -> Option<SurfaceId> {
        let old = self.focused_surface();
        self.exo.step_lost();
        self.pending = V3Pending::default();
        self.field = Field::focused_on(surface);
        old
    }

    /// H4 and H5 record into the assembly only while attached.
    pub(super) fn assembly_mut(&mut self) -> Option<&mut Assembly> {
        self.is_attached().then_some(&mut self.composition.assembly)
    }

    /// B3: take the assembly, which a host composition event or a press
    /// used up (design H9).
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
        if matches!(self.field, Field::Unfocused) {
            return None;
        }
        Some(Current {
            ti,
            seat: self.seat,
            serial: self.commits.serial(),
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
    /// link was not attached (H2'), and otherwise whether the guest still
    /// showed a preedit, which it must now be told to clear.
    pub(super) fn exo_detached(&mut self) -> Option<bool> {
        let left = self.leave_attached(LinkState::Detached)?;
        Some(left.shown.is_some())
    }

    /// F6: the host deleted the v1 object, so nothing more goes to it.
    pub(super) fn host_deleted(&mut self) {
        self.leave_attached(LinkState::Detached);
    }

    /// W1 and W2: the barrier `token` returned. It ends the drain or the
    /// fence it is, in any link state. A fence or a report's barrier also
    /// tells the mirror that Exo processed the request before it.
    pub(super) fn barrier_done(&mut self, ti: TiId, token: BarrierToken) {
        self.exo.barrier_done(ti, token);
        if self.link == (LinkState::Draining { barrier: token }) {
            self.link = LinkState::Detached;
        }
        if self.fence == Some(token) {
            self.fence = None;
        }
    }

    /// Note the activation's current deferral, and whether it started or
    /// changed, which is when it is logged.
    pub(super) fn deferral_changed(&mut self, deferral: Option<Deferral>) -> bool {
        let last = std::mem::replace(&mut self.warned.deferral, deferral);
        deferral.is_some() && deferral != last
    }

    /// Whether R2's missing seat is warned of now: once per text input.
    pub(super) fn first_seat_warning(&mut self) -> bool {
        !std::mem::replace(&mut self.warned.seat_missing, true)
    }

    /// The barriers this text input waits for: its drain and its fence.
    #[cfg(test)]
    pub(crate) fn barriers(&self) -> impl Iterator<Item = BarrierToken> {
        let drain = match self.link {
            LinkState::Draining { barrier } => Some(barrier),
            LinkState::Detached | LinkState::Attached { .. } => None,
        };
        drain.into_iter().chain(self.fence)
    }

    /// I1a, I1b and I2 of design 3.1, and that an unfocused text input has
    /// nothing pending. Test-only; see `ImeCore::assert_invariants`.
    #[cfg(test)]
    pub(super) fn assert_invariants(&self, ti: TiId, env: &impl Env) {
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
        if matches!(self.field, Field::Unfocused) {
            assert_eq!(self.pending, V3Pending::default(), "{ti:?} is unfocused");
        }
    }
}

/// What handler tests set up and read directly (`test_support::text_input`
/// and the tests of `handler/text_input.rs`), as no peer can. Each keeps
/// the state as it finds it otherwise; the fixtures check that what they
/// build is legal.
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

    /// Attach the host text input with the content type the field calls
    /// for, as the reconcile of the guest commit that set its state would.
    pub(crate) fn attach_for_test(&mut self, env: &impl Env) {
        let content_type = Some(host::target(self, env).content_type);
        self.link = LinkState::Attached { content_type };
    }

    /// Show `text` with the cursor at its end, or show nothing for empty
    /// `text`.
    pub(crate) fn show_for_test(&mut self, text: &str) {
        let end = text.len() as i32;
        self.composition.shown = Preedit::new(text.to_string(), (end, end));
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

    /// Take `other`'s view of Exo's text.
    pub(crate) fn copy_exo_for_test(&mut self, other: &Self) {
        self.exo = other.exo.clone();
    }

    /// Whether it warned of a missing seat.
    pub(crate) fn warned_for_test(&self) -> bool {
        self.warned.seat_missing
    }
}

impl Field {
    fn focused_on(surface: Option<SurfaceId>) -> Self {
        match surface {
            Some(surface) => Self::Focused(FocusedField {
                surface,
                committed: V3Committed::default(),
            }),
            None => Self::Unfocused,
        }
    }
}

/// Proof that a text input's composition is current (the gate of
/// [`TextInput::current`]). Its fields are private to this file, so no
/// other module can make one. Each method plans the transaction of one
/// row.
pub(super) struct Current<'a> {
    ti: TiId,
    seat: SeatId,
    serial: Serial,
    composition: &'a mut Composition,
    fence: &'a mut Option<BarrierToken>,
}

/// Why a host delete (H9) is dropped.
#[derive(Debug)]
pub(super) enum DeleteDrop {
    /// The Exo mirror cannot measure it.
    Unmeasured(Unmeasured),
    NotAroundCursor,
    Empty,
}

/// Why a preedit region (X1') is dropped.
#[derive(Debug)]
pub(super) enum RegionDrop {
    /// The Exo mirror cannot measure it.
    Unmeasured(Unmeasured),
    NotAroundCursor,
    /// The region does not cover the preedit Exo holds, whose rest Exo
    /// would leave as text.
    Uncovered,
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

    /// Plan `txn`, with what becomes of the Backspace lease, `lease`. H9
    /// and X1 plan the transaction a `exo_view::Frame` measured in Exo's
    /// text.
    pub(super) fn plan(self, txn: GuestTxn, lease: LeaseOutcome) -> Planned<'a> {
        Planned {
            cur: self,
            txn,
            lease,
            fence: None,
        }
    }

    /// H7. Exo's commit text is always the preedit's, so it is not read.
    pub(super) fn preedit(self, text: String, assembly: Assembly) -> Planned<'a> {
        let lease = match text.is_empty() && self.shown().is_some() {
            true => LeaseOutcome::ArmIfBackspaceHeld,
            false => LeaseOutcome::End,
        };
        let cursor = resolve_preedit_cursor(&text, assembly.selection, assembly.cursor);
        let txn = GuestTxn {
            preedit: Preedit::new(text, cursor),
            ..GuestTxn::default()
        };
        self.plan(txn, lease)
    }

    /// H8.
    pub(super) fn host_commit(self, text: String) -> Planned<'a> {
        let txn = GuestTxn {
            commit: Some(text),
            ..GuestTxn::default()
        };
        self.plan(txn, LeaseOutcome::End)
    }

    /// X2 commits the shown preedit; X2', with none shown, recovers the
    /// key the input method took.
    pub(super) fn confirm(self) -> Confirm<'a> {
        if self.shown().is_none() {
            return Confirm::RecoverHeldKey(KeyEffect::RecoverHeldKey {
                ti: self.ti,
                seat: self.seat,
            });
        }
        Confirm::Commit(self.commit_shown())
    }

    /// The assembly, which a preedit and a region apply (B3).
    pub(super) fn take_assembly(&mut self) -> Assembly {
        std::mem::take(&mut self.composition.assembly)
    }

    /// F4: a press commits the shown preedit, and fences the host's
    /// `reset` with `fence`.
    pub(super) fn press_commit(self, fence: BarrierToken) -> Planned<'a> {
        debug_assert!(self.shown().is_some(), "a press commits a shown preedit");
        let mut planned = self.commit_shown();
        planned.fence = Some(fence);
        planned
    }

    /// The commit of the shown preedit (X1, X2, F1, F4, H2).
    pub(super) fn commit_shown(self) -> Planned<'a> {
        let text = self.shown().map(|shown| shown.text().to_string());
        let txn = GuestTxn {
            commit: text,
            ..GuestTxn::default()
        };
        self.plan(txn, LeaseOutcome::End)
    }
}

/// A planned transaction and the change it makes. [`Self::apply`] encodes
/// it and makes the change; dropping it changes nothing.
#[must_use = "apply it, or drop it to change nothing"]
pub(super) struct Planned<'a> {
    cur: Current<'a>,
    txn: GuestTxn,
    lease: LeaseOutcome,
    /// The press's reset fence (F4).
    fence: Option<BarrierToken>,
}

impl Planned<'_> {
    /// What reaches the guest of the transaction, if it applies.
    pub(super) fn edit(&self) -> Edit {
        let txn = &self.txn;
        let (deleted_before, deleted_after) = txn.delete.unwrap_or_default();
        Edit {
            deleted_before,
            deleted: (deleted_before, deleted_after) != (0, 0),
            committed: txn.commit.as_ref().map_or(0, String::len),
            shown_before: self.cur.composition.shown.is_some(),
        }
    }

    /// Encode the transaction and, if it fits, make its change: the guest
    /// then shows the transaction's preedit. Returns its events and the
    /// lease effect; if it does not fit, nothing changes.
    pub(super) fn apply(self) -> Result<(Vec<Outgoing>, KeyEffect), TooLong> {
        let Planned {
            cur,
            txn,
            lease,
            fence,
        } = self;
        let events = txn.encode(cur.ti, cur.composition.shown.is_some(), cur.serial)?;
        cur.composition.shown = txn.preedit;
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
