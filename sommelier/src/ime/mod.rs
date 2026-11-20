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

//! The IME core: the bridge from a guest's `zwp_text_input_v3` to Exo's
//! `zwp_text_input_v1` and `zcr_extended_text_input_v1`, as a sans-IO
//! state machine. [`ImeCore::step`] takes one [`Input`] and returns the
//! [`Effects`] the adapter queues; it reads the facts it does not own
//! through [`Env`]. The behaviour it implements is the table of section 2
//! of the design (`docs/IME.md`), whose row IDs the code and tests
//! cite.

use std::collections::BTreeMap;

use crate::wire::{IndexedString, Outgoing};

mod exo_view;
mod guest;
mod host;
mod mirror;
mod serial;
mod state;
mod step;
#[cfg(test)]
mod tests;

// What handler tests outside the core use: a pure helper they test, and
// the state they set up and read directly, as no peer can
// (`test_support::text_input`); and the drops the DST counts.
#[cfg(test)]
pub(crate) use host::map_v3_content_type;
#[cfg(test)]
pub(crate) use state::{Assembly, LinkState, TextInput, V3Committed, V3Pending};
#[cfg(test)]
pub(crate) use step::{take_drops_for_test, Drops};

/// A guest `zwp_text_input_v3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TiId(pub u32);

/// A guest `wl_seat` ID, which may no longer name a live seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SeatId(pub u32);

/// A guest `wl_surface`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SurfaceId(pub u32);

/// A host `wl_display.sync` the core waits for. The core numbers its own
/// barriers and never reuses a number; the adapter keeps the one map from
/// token to host callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BarrierToken(u64);

/// The host objects behind a guest text input, fixed at its creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTi {
    /// The host `zwp_text_input_v1`.
    pub v1: u32,
    /// The host `zcr_extended_text_input_v1`, at v11 or later: the adapter
    /// creates no text input without one.
    pub ext: HostExt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostExt {
    pub id: u32,
    /// The host `zcr_text_input_extension_v1` it is created from.
    pub manager: u32,
}

/// One message the core handles. Every trigger of an IME change is one of
/// these, and [`ImeCore::step`] matches them without a wildcard.
#[derive(Debug)]
pub enum Input {
    // Guest zwp_text_input_manager_v3 and zwp_text_input_v3 (G1 to G10).
    /// G1. The adapter has allocated `host`'s IDs: its v1 object from the
    /// host `zwp_text_input_manager_v1` `manager`, and its extension from
    /// the extension's manager.
    Create {
        ti: TiId,
        seat: SeatId,
        host: HostTi,
        manager: u32,
    },
    Enable(TiId),
    Disable(TiId),
    SurroundingText(TiId, IndexedString, i32, i32),
    ContentType(TiId, (u32, u32)),
    CursorRect(TiId, (i32, i32, i32, i32)),
    Commit(TiId),
    /// G10, and F6 with `host_deleted`: the host deleted the v1 object.
    Destroy {
        ti: TiId,
        host_deleted: bool,
    },

    // Host events (H, X and W rows). The adapter resolves a v1 sender to
    // its text input; an extension sender is resolved by the core.
    HostLeave(TiId),
    V1(TiId, HostIme),
    Ext {
        ext: u32,
        event: HostIme,
    },
    BarrierDone(BarrierToken),

    // Cross-concern messages (F1 to F4).
    /// F1: the seat's keyboard focus moved to `surface`, or to none. The
    /// keyboard concern sends one per seat whose focused surface changed.
    /// `keyboard` is the guest's copy of the keyboard events that moved it,
    /// which the core queues after the commit of a preedit the move ends
    /// and before the text inputs' `leave` and `enter`, so that the guest
    /// gets the commit while the field still has its focus.
    FocusChanged {
        seat: SeatId,
        surface: Option<SurfaceId>,
        keyboard: Vec<Outgoing>,
    },
    /// F2: the guest destroyed `surface`, before the `FocusChanged`s its
    /// destruction caused, which then find its fields reset.
    SurfaceDestroyed(SurfaceId),
    /// F3: the guest bound a `wl_seat` under this ID.
    SeatBound(SeatId),
    /// F4: a pointer press or a touch-down on `surface`. `resend` is the
    /// guest's copy of the event, which the core queues after the
    /// transaction if the press ends a composition.
    Press {
        surface: SurfaceId,
        resend: Outgoing,
    },
    /// The keyboard concern forwarded a key press or repeat to the guest on
    /// this seat, which may edit the guest's text where Exo does not see.
    GuestKey(SeatId),
}

/// A host input-method event of a text input, from its v1 object or its
/// extension.
#[derive(Debug)]
pub enum HostIme {
    PreeditStyling {
        index: u32,
        length: u32,
        style: u32,
    },
    PreeditCursor(i32),
    CursorPosition {
        index: i32,
        anchor: i32,
    },
    Preedit {
        serial: u32,
        text: IndexedString,
        commit: String,
    },
    Commit {
        serial: u32,
        text: String,
    },
    Delete {
        index: i32,
        length: u32,
    },
    PreeditRegion {
        index: i32,
        length: u32,
    },
    Confirm {
        /// `confirm_preedit`'s selection behavior is `unchanged`.
        keep_selection: bool,
    },
}

/// What one step asks the adapter to send and do, each in order.
#[derive(Debug, Default)]
pub struct Effects {
    /// Guest events. Every IME transaction ends with its `done`. A press
    /// that ended a composition (F4) is re-sent here, after its
    /// transaction, and so are the keyboard events of a focus change (F1).
    pub to_guest: Vec<Outgoing>,
    /// Host requests and barriers.
    pub to_host: Vec<HostOut>,
    /// Requests to the keyboard concern, applied after `to_guest` and `to_host`.
    pub keys: Vec<KeyEffect>,
    /// Whether the press of an [`Input::Press`] is to be forwarded: it
    /// ended no composition. The core consumes every other input's
    /// message.
    pub forward: bool,
}

#[derive(Debug)]
pub enum HostOut {
    Request(Outgoing),
    /// Queue a host `wl_display.sync` here. Its `done` comes back as
    /// [`Input::BarrierDone`].
    Barrier(BarrierToken),
    /// `request`, then a [`Self::Barrier`], which reach the host in one
    /// submission, which Exo dispatches in one read, so that no event of
    /// Exo's falls between them.
    Glued {
        request: Outgoing,
        barrier: BarrierToken,
    },
    /// `request`, the destructor of host object `id`, which is then
    /// retired.
    Destroy {
        request: Outgoing,
        id: u32,
    },
}

/// What becomes of the Backspace repeat leased to a text input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseOutcome {
    End,
    /// Lease it to the text input if the seat holds Backspace; otherwise
    /// end it.
    ArmIfBackspaceHeld,
}

#[derive(Debug)]
pub enum KeyEffect {
    Lease {
        ti: TiId,
        seat: SeatId,
        outcome: LeaseOutcome,
    },
    /// X2': recover the seat's latest eligible held key, which the input
    /// method took, if the confirmation turns out to be its autorepeat and
    /// not Exo's confirmation before a new press (E13). The keyboard
    /// concern decides at the host's next event; if it recovers the key,
    /// it sends its press and release unless another channel already
    /// delivered it, then the transaction of [`ImeCore::recovery_done`],
    /// whether or not the pair went. The lease:
    /// - a held Backspace whose pair is sent arms it;
    /// - a held Backspace whose pair is skipped ends it;
    /// - another held key whose pair is sent ends it;
    /// - another held key whose pair is skipped leaves it alone;
    /// - with no held key, or a new press's confirmation, it ends, and
    ///   nothing is sent.
    RecoverHeldKey { ti: TiId, seat: SeatId },
}

/// Read-only facts the core needs and does not own. The adapter answers
/// them from the object table and the keyboard focus; tests use maps.
///
/// Precondition: every change of an answer that can deactivate a target,
/// a surface's end or a focus change, reaches the core as an [`Input`] in
/// the same message, so that the reconcile ending that step sees it. A
/// seat's end is the one exception: the core notices it at its next step
/// (R1), and an attached link with a dead seat is legal until then (I1b).
pub trait Env {
    /// The host ID of a live guest surface.
    fn host_surface(&self, surface: SurfaceId) -> Option<u32>;
    /// The host `wl_seat` this guest ID can carry, while it names a live
    /// `wl_seat`.
    fn host_seat(&self, seat: SeatId) -> Option<u32>;
    /// The surface the seat's keyboard focuses now.
    fn keyboard_focus(&self, seat: SeatId) -> Option<SurfaceId>;
}

/// The text inputs of one connection.
#[derive(Debug, Default)]
pub struct ImeCore {
    inputs: BTreeMap<TiId, state::TextInput>,
    next_barrier: u64,
}

impl ImeCore {
    /// G1: text input `ti` of `seat`, with the host objects `host`, focused
    /// on `surface`, if any.
    fn insert(&mut self, ti: TiId, seat: SeatId, host: HostTi, surface: Option<SurfaceId>) {
        // The guest cannot reuse a live text input's ID.
        let replaced = self
            .inputs
            .insert(ti, state::TextInput::new(seat, host, surface));
        debug_assert!(replaced.is_none(), "{ti:?} exists already");
    }

    /// The text input whose host extension is `ext`.
    fn by_ext(&self, ext: u32) -> Option<TiId> {
        let mut inputs = self.inputs.iter();
        inputs.find_map(|(&ti, t)| (t.host().ext.id == ext).then_some(ti))
    }
}

/// What handler tests outside the core set up and read directly, as no
/// peer can (`test_support::text_input` and the tests of
/// `handler/text_input.rs`).
#[cfg(test)]
impl ImeCore {
    /// Text input `ti` as [`Input::Create`] makes it, without its host
    /// requests. Its mirror holds Exo's empty text, and the guest is not in
    /// step with it, so Exo's deletes and regions are dropped until a
    /// report.
    pub(crate) fn insert_for_test(
        &mut self,
        ti: TiId,
        seat: SeatId,
        host: HostTi,
        surface: Option<SurfaceId>,
    ) {
        self.insert(ti, seat, host, surface);
    }

    pub(crate) fn get(&self, ti: TiId) -> Option<&TextInput> {
        self.inputs.get(&ti)
    }

    pub(crate) fn get_mut(&mut self, ti: TiId) -> Option<&mut TextInput> {
        self.inputs.get_mut(&ti)
    }

    /// Every text input, in `TiId` order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (TiId, &TextInput)> {
        self.inputs.iter().map(|(&ti, t)| (ti, t))
    }

    /// Where the Exo mirror of host text input `v1`, if it says it is
    /// exact, differs from Exo's tracker, which predicts `predicted` and
    /// holds `history`, oldest first: what the mirror holds. A known
    /// history entry fits a state whose fingerprint it is, with no
    /// selection if it is vague; an unknown one fits any state, and a
    /// leading one, which stands for the states the mirror lost (evicted,
    /// or matched by an ambiguous report), fits one or more.
    pub(crate) fn mirror_differs_for_test(
        &self,
        v1: u32,
        predicted: &StateForTest,
        history: &[StateForTest],
    ) -> Option<String> {
        let t = self.inputs.values().find(|t| t.host().v1 == v1)?;
        let mirror = t.exo().mirror();
        let exact = mirror.exact()?;
        let state = |(text, (start, end), composition): &StateForTest| mirror::ExoState {
            text: text.clone(),
            selection: mirror::Span::new(*start, *end),
            composition: composition.clone(),
        };
        let fits = |entry: &mirror::Entry, exo: &StateForTest| match entry {
            mirror::Entry::Known { print, .. } => *print == print.of_alike(&state(exo)),
            mirror::Entry::Unknown => true,
        };
        let entries = mirror.expected();
        let lost = matches!(entries.front(), Some(mirror::Entry::Unknown));
        let known = entries.len() - usize::from(lost);
        let fit = match (lost, history.len().checked_sub(known)) {
            (true, Some(skipped)) => skipped >= 1,
            (false, Some(skipped)) => skipped == 0,
            (_, None) => false,
        } && entries
            .iter()
            .skip(usize::from(lost))
            .zip(&history[history.len().saturating_sub(known)..])
            .all(|(entry, exo)| fits(entry, exo));
        (*exact != state(predicted) || !fit).then(|| format!("{exact:?}, {entries:?}"))
    }
}

/// A tracker state for tests: its text, its directed selection, and its
/// composition, empty for none.
#[cfg(test)]
pub(crate) type StateForTest = (String, (usize, usize), std::ops::Range<usize>);

impl ImeCore {
    /// The seat of text input `ti` while Exo's input method is attached
    /// to it, whatever its fence.
    pub fn attached_seat(&self, ti: TiId) -> Option<SeatId> {
        self.inputs
            .get(&ti)
            .filter(|t| t.is_attached())
            .map(|t| t.seat())
    }

    /// X2': the transaction that closes text input `ti`'s empty
    /// confirmation once the keyboard concern has recovered the held key
    /// it stood for ([`KeyEffect::RecoverHeldKey`]): G9c's answer, what the
    /// guest shows under the current serial. `None` if the text input no
    /// longer passes the gate, in which case nothing is recovered.
    pub fn recovery_done(&mut self, ti: TiId) -> Option<Vec<Outgoing>> {
        let input = self.inputs.get_mut(&ti)?;
        input.current(ti)?;
        // The key may edit the guest's text.
        input.exo_mut().guest_edited();
        Some(input.answer(ti))
    }

    /// Check the invariants of design 3.1 (I1a to I3) that hold between
    /// steps, and that every pending barrier is one the core numbered,
    /// once. I5 is the adapter's. Test-only, so that a panicking check
    /// cannot be wired into production.
    #[cfg(test)]
    pub fn assert_invariants(&self, env: &impl Env) {
        let mut enabled = Vec::new();
        let mut barriers = Vec::new();
        for (&ti, t) in &self.inputs {
            t.assert_invariants(ti, env);
            if t.is_enabled() {
                enabled.push(ti);
            }
            barriers.extend(t.barriers());
        }
        assert!(enabled.len() <= 1, "I3: {enabled:?} are all enabled");
        let count = barriers.len();
        barriers.sort();
        barriers.dedup();
        assert_eq!(barriers.len(), count, "a barrier is pending twice");
        assert!(
            barriers.iter().all(|b| b.0 < self.next_barrier),
            "{barriers:?} holds a barrier the core never numbered"
        );
    }
}
