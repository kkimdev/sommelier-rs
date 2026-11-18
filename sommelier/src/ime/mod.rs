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
//! of the design (`docs/IME_REDESIGN.md`), whose row IDs the code and tests
//! cite.

use std::collections::BTreeMap;

use crate::wire::{IndexedString, Outgoing};

mod guest;
mod host;
mod serial;
mod state;
mod step;
#[cfg(test)]
mod tests;

// Shared with the old handlers until the staged switch deletes them.
pub(crate) use guest::{delete_lengths, lengths_around_cursor, resolve_preedit_cursor};
#[cfg(test)]
pub(crate) use host::map_v3_content_type;
pub(crate) use host::{content_type_requests, takes_surrounding_text_support};
use state::TextInput;

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
#[derive(Clone, Copy, Debug)]
pub struct HostTi {
    /// The host `zwp_text_input_v1`.
    pub v1: u32,
    /// The host `zcr_extended_text_input_v1`, if the host has the extension.
    pub ext: Option<HostExt>,
}

#[derive(Clone, Copy, Debug)]
pub struct HostExt {
    pub id: u32,
    /// Its version; an unknown one is the largest, which allows every
    /// request.
    pub version: u32,
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
    FocusChanged {
        seat: SeatId,
        surface: Option<SurfaceId>,
    },
    /// F2: the guest destroyed `surface`, after the `FocusChanged`s its
    /// destruction caused.
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
    Confirm,
}

/// What one step asks the adapter to send and do, each in order.
#[derive(Debug, Default)]
pub struct Effects {
    /// Guest events. Every IME transaction ends with its `done`. A press
    /// that ended a composition (F4) is re-sent here, after its
    /// transaction.
    pub to_guest: Vec<Outgoing>,
    /// Host requests and barriers.
    pub to_host: Vec<HostOut>,
    /// Requests to the keyboard concern, applied after `to_guest`.
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
    /// Retire the host object the previous request destroyed.
    Retire(u32),
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
    /// method took. If one is held, send its press and release unless
    /// another channel already delivered it, then `done`, whether or not
    /// the pair went. The lease:
    /// - a held Backspace whose pair is sent arms it;
    /// - a held Backspace whose pair is skipped ends it;
    /// - another held key whose pair is sent ends it;
    /// - another held key whose pair is skipped leaves it alone;
    /// - with no held key, it ends, and nothing is sent.
    RecoverHeldKey {
        ti: TiId,
        seat: SeatId,
        /// The bare transaction, a lone `done`.
        done: Outgoing,
    },
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
    inputs: BTreeMap<TiId, TextInput>,
    /// Each extension object's text input. Only `Create` and removal
    /// change it.
    by_ext: BTreeMap<u32, TiId>,
    next_barrier: u64,
}

impl ImeCore {
    /// H10: the seat of the text input whose host keysyms reach the
    /// guest: an attached one, whatever its fence.
    pub fn keysym_target(&self, ti: TiId) -> Option<SeatId> {
        self.inputs
            .get(&ti)
            .filter(|t| t.is_attached())
            .map(|t| t.seat())
    }

    /// Check the invariants of design 3.1 (I1a to I4) that hold between
    /// steps, and that every pending barrier is one the core numbered,
    /// once. I5 is the adapter's.
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
        let extensions: BTreeMap<u32, TiId> = self
            .inputs
            .iter()
            .filter_map(|(&ti, t)| Some((t.host().ext?.id, ti)))
            .collect();
        assert_eq!(self.by_ext, extensions, "I4");
    }
}
