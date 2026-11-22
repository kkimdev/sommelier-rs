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

//! What the keyboard core knows of each physical key: its generations
//! (`docs/KEYBOARD.md`), their delivery to the guest, the IME's Backspace
//! repeat lease and the peek watermarks. Only [`super::KeyboardCore`]
//! moves them.

use std::collections::BTreeMap;

use crate::ime::{LeaseOutcome, TiId};
use crate::protocols::wayland::wl_keyboard::KeyState;
use crate::state::{serial_is_after, GuestId, HostId};

/// Linux evdev keycode reported by wl_keyboard and peek_key for Backspace.
pub(crate) const EVDEV_KEY_BACKSPACE: u32 = 14;

/// Provenance for one physical key generation observed through ChromeOS
/// `peek_key`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PeekKeyProvenance {
    pub serial: u32,
    pub time: u32,
    pub sequence: u64,
    /// The generation is guest input, which may recover an IME-consumed
    /// repeat. Host accelerators and shortcut chords permanently clear
    /// this bit for the lifetime of the generation.
    pub eligible: bool,
}

/// Causal watermark for the newest physical generation in one seat/focus
/// domain.
///
/// Recording the owning keyboard lets keyboard teardown retire an otherwise
/// stale watermark atomically with its key generations. Without the owner, a
/// newer generation from a destroyed keyboard can permanently make an older
/// still-held key on another keyboard ineligible for IME repeat recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PeekWatermark {
    keyboard: HostId,
    sequence: u64,
}

/// Who gave the guest a key generation's key. A key has at most one: two
/// would deliver it twice, and none could leave it stuck.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub(crate) enum GuestKeyOwner {
    /// A `wl_keyboard.key` press, or one the enter listed, awaits its
    /// release.
    Physical,
    /// Sommelier consumed the key as a shortcut's chord. The matching
    /// release is acknowledged as handled and never reaches the guest
    /// client.
    Shortcut,
    /// The guest got a balanced pair from an IME recovery. This
    /// tombstone suppresses delayed duplicates on the other channels.
    ImeRecovery,
}

/// What a key's chord makes of its press, under its keyboard's state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChordClass {
    /// Ordinary guest input.
    Guest,
    /// Listed in `SOMMELIER_ACCELERATORS`: the host runs the accelerator.
    HostAccelerator,
    /// Bound to a shortcut: Sommelier owns the chord.
    Shortcut,
}

/// A `wl_keyboard.key` of its key's current generation
/// ([`KeyGenerationRegistry::transition_physical`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PhysicalKey {
    Press {
        chord: ChordClass,
        /// The keyboard's Backspace is leased to the IME's repeat, read
        /// before the press moved its generation.
        backspace_leased: bool,
    },
    Release,
}

/// What becomes of a `wl_keyboard.key`: how it is acknowledged through
/// `ack_key`, and whether the guest gets it. A forwarded key is always
/// acknowledged as handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyVerdict {
    /// Handled, and forwarded to the guest.
    Forward,
    /// Handled, and dropped: the guest has the key already, or a shortcut
    /// consumed it.
    Consume,
    /// Not handled, and dropped: the host keeps the key, a host
    /// accelerator's or one the input method took, or its release, or a
    /// release of a key the guest never got.
    Decline,
    /// Handled, and dropped: the press claimed its generation for its
    /// shortcut, which the caller runs.
    Shortcut,
}

impl KeyVerdict {
    /// The `ack_key` it makes.
    pub fn handled(self) -> bool {
        !matches!(self, Self::Decline)
    }

    pub fn forward(self) -> bool {
        matches!(self, Self::Forward)
    }
}

/// What moves the IME's Backspace repeat lease
/// ([`KeyGenerationRegistry::lease`]). While a text input holds a held
/// Backspace's lease, the key's physical repeats belong to the input
/// method's synthetic presses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LeaseInput {
    /// The IME core's outcome for text input `ti` (`ime::KeyEffect::Lease`),
    /// or a confirmation that recovered no key (X2').
    Ime { ti: TiId, outcome: LeaseOutcome },
    /// A press of another key on `keyboard`: its held Backspace is not
    /// leased again in this hold, and the seat's leases end.
    OtherPress { keyboard: HostId },
    /// A release of Backspace on a keyboard of the seat.
    BackspaceRelease,
    /// The seat's keyboard focus moved.
    FocusLost,
    /// X2': the recovery for `ti` of a key `keyboard` holds, Backspace or
    /// not, whose pair went if `sent`. A held Backspace whose pair went is
    /// leased; one whose pair was skipped, and another key whose pair went,
    /// end the lease; another key whose pair was skipped leaves it.
    Recovered {
        ti: TiId,
        keyboard: HostId,
        backspace: bool,
        sent: bool,
    },
}

/// What the guest got of one key generation: who delivered it, and
/// whether a host accelerator took it. [`Self::physical`] and
/// [`Self::recover`] are its only transitions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Delivery {
    owner: Option<GuestKeyOwner>,
    /// A `wl_keyboard` press of a generation no channel gave the guest was
    /// a host accelerator: its release is dropped too, and no recovery may
    /// deliver it. Only the generation's release, an `enter` or the
    /// keyboard's end takes it. A shortcut's release leaves it.
    accelerator_suppressed: bool,
}

impl Delivery {
    /// A `wl_keyboard.key` of the generation.
    ///
    /// A generation a shortcut owns stays the shortcut's: its duplicate
    /// press and its release are consumed. A shortcut claims only a
    /// generation no channel owns; another channel's keeps it, and its
    /// press is that channel's duplicate. An accelerator's press, or any
    /// press of a suppressed generation, is declined and suppresses a
    /// generation the guest does not hold. A press while the IME holds
    /// the Backspace lease, of a generation the guest does not hold,
    /// belongs to the input method's repeat and becomes its tombstone, as
    /// does any press of a recovered generation.
    fn physical(self, key: PhysicalKey) -> (Self, KeyVerdict) {
        use GuestKeyOwner::{ImeRecovery, Physical, Shortcut};
        let Self {
            owner,
            accelerator_suppressed,
        } = self;
        match (owner, key) {
            (Some(Shortcut), PhysicalKey::Press { .. }) => (self, KeyVerdict::Consume),
            (Some(Shortcut), PhysicalKey::Release) => {
                let owner = None;
                (Self { owner, ..self }, KeyVerdict::Consume)
            }
            (
                None,
                PhysicalKey::Press {
                    chord: ChordClass::Shortcut,
                    ..
                },
            ) => {
                let owner = Some(Shortcut);
                (Self { owner, ..self }, KeyVerdict::Shortcut)
            }
            (
                _,
                PhysicalKey::Press {
                    chord,
                    backspace_leased,
                },
            ) => {
                let forwarded = owner == Some(Physical);
                let for_ime = owner == Some(ImeRecovery) || (backspace_leased && !forwarded);
                let owner = match (owner, for_ime) {
                    (None, true) => Some(ImeRecovery),
                    (owner, _) => owner,
                };
                if accelerator_suppressed || chord == ChordClass::HostAccelerator {
                    let accelerator_suppressed = !forwarded || accelerator_suppressed;
                    let next = Self {
                        owner,
                        accelerator_suppressed,
                    };
                    return (next, KeyVerdict::Decline);
                }
                let next = Self { owner, ..self };
                match (for_ime, forwarded) {
                    (true, _) => (next, KeyVerdict::Decline),
                    // A duplicate press.
                    (false, true) => (next, KeyVerdict::Consume),
                    (false, false) => {
                        let owner = Some(Physical);
                        (Self { owner, ..next }, KeyVerdict::Forward)
                    }
                }
            }
            (_, PhysicalKey::Release) => {
                let verdict = match (owner, accelerator_suppressed) {
                    (Some(Physical), false) => KeyVerdict::Forward,
                    (Some(Physical), true) => KeyVerdict::Consume,
                    _ => KeyVerdict::Decline,
                };
                // A recovery's tombstone outlives the release.
                let owner = owner.filter(|&owner| owner == ImeRecovery);
                let next = Self {
                    owner,
                    accelerator_suppressed: false,
                };
                (next, verdict)
            }
        }
    }

    /// An IME recovery of the generation: whether the guest gets its
    /// balanced pair. A generation the guest holds, or a shortcut
    /// consumed, is not recovered: the guest would get the key twice, or
    /// the chord's key after Sommelier acted on it.
    fn recover(self) -> (Self, bool) {
        match self.owner {
            Some(GuestKeyOwner::Physical | GuestKeyOwner::Shortcut) => (self, false),
            None | Some(GuestKeyOwner::ImeRecovery) => {
                let owner = Some(GuestKeyOwner::ImeRecovery);
                (Self { owner, ..self }, true)
            }
        }
    }
}

/// One `zcr_extended_keyboard_v1.peek_key` of a key
/// ([`KeyGenerationRegistry::peek_key`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PeekInput {
    /// `guest_input`: under its keyboard's current state the key forms
    /// neither a host accelerator nor a shortcut chord.
    Press {
        serial: u32,
        time: u32,
        guest_input: bool,
    },
    Release {
        serial: u32,
        time: u32,
    },
}

/// What a `peek_key` did to its key's generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct PeekOutcome {
    /// The press retired a generation held across the input method's own
    /// detach, whose press the guest still holds: the guest is owed its
    /// release, before anything of the new press.
    pub owes_release: bool,
    /// The press numbered a new peek generation with this sequence, which
    /// its seat records as its latest.
    pub numbered: Option<u64>,
    /// The peek changed nothing: a release older than the key's latest
    /// peek.
    pub ignored: bool,
}

/// How a `wl_keyboard.key` relates to its key's generation
/// ([`KeyGenerationRegistry::admit_key`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum KeyAdmission {
    /// The event belongs to another generation, which settles it:
    /// [`KeyVerdict::Forward`] closes a retired generation's guest press,
    /// and [`KeyVerdict::Decline`] drops an event the current generation
    /// does not expect.
    Settled(KeyVerdict),
    /// The event belongs to the key's current generation: its delivery is
    /// decided by [`KeyGenerationRegistry::transition_physical`].
    Current,
}

/// The kind of a generation's [`KeyPhase`], without its data.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PhysicalKeyState {
    #[default]
    Unseen,
    Held,
    Released,
}

/// What the generation's latest `peek_key` press announced.
///
/// `peek_key` promises that the `wl_keyboard.key` after it has its serial,
/// so the serial names the event the latest peek announced. Only a peek
/// press numbers a generation for recovery ([`PeekKeyProvenance`]); the provenance lasts while its keyboard holds a
/// key, and the serial as long as the generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PeekRecord {
    /// No peek press: the key is held since `wl_keyboard.enter`, or only
    /// `wl_keyboard` showed it.
    #[default]
    None,
    /// A peek announced `serial`, but the generation has no provenance:
    /// its keyboard has since held no key.
    Announced { serial: u32 },
    /// A peek press numbered the generation; `provenance.serial` is the
    /// latest peek press's.
    Live(PeekKeyProvenance),
}

impl PeekRecord {
    fn serial(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Announced { serial } => Some(serial),
            Self::Live(provenance) => Some(provenance.serial),
        }
    }

    fn provenance(self) -> Option<PeekKeyProvenance> {
        match self {
            Self::Live(provenance) => Some(provenance),
            Self::None | Self::Announced { .. } => None,
        }
    }

    /// The keyboard holds no key any more: the provenance ends, the
    /// serial stays.
    fn without_provenance(self) -> Self {
        match self {
            Self::Live(provenance) => Self::Announced {
                serial: provenance.serial,
            },
            other => other,
        }
    }
}

/// The IME's Backspace repeat lease of a held key: while a text input holds it, the key's physical
/// repeats belong to the input method's synthetic presses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RepeatLease {
    #[default]
    Free,
    /// Text input `text_input` holds the lease.
    Leased { text_input: TiId },
    /// The lease ended for the rest of the hold: no text input may take it
    /// again until the key is released.
    Cancelled,
}

/// What the host keyboard has shown of one key's generation. The data
/// each state carries exists only in it: a lease and an input method's
/// detach only while the key is held, a release serial only once it is
/// released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyPhase {
    /// No physical event yet: a claim, or a suppression, came first.
    Unseen,
    Held {
        peek: PeekRecord,
        lease: RepeatLease,
        /// The input method detached on its own while the key was held
        /// (H2), so the key's release may have died with it: the
        /// generation recovers no IME-consumed repeat, and the key's next
        /// `peek_key` press retires it for a new one. It concerns every
        /// held key, one held since `wl_keyboard.enter` without a peek
        /// included, and only that recovery and that press; the key is
        /// still guest input.
        stale: bool,
    },
    Released {
        peek: PeekRecord,
        /// Serial of the release, from the peek channel or `wl_keyboard`.
        release_serial: Option<u32>,
    },
}

impl KeyPhase {
    const HELD: Self = Self::Held {
        peek: PeekRecord::None,
        lease: RepeatLease::Free,
        stale: false,
    };

    fn kind(self) -> PhysicalKeyState {
        match self {
            Self::Unseen => PhysicalKeyState::Unseen,
            Self::Held { .. } => PhysicalKeyState::Held,
            Self::Released { .. } => PhysicalKeyState::Released,
        }
    }

    fn peek(self) -> PeekRecord {
        match self {
            Self::Unseen => PeekRecord::None,
            Self::Held { peek, .. } | Self::Released { peek, .. } => peek,
        }
    }

    fn peek_mut(&mut self) -> Option<&mut PeekRecord> {
        match self {
            Self::Unseen => None,
            Self::Held { peek, .. } | Self::Released { peek, .. } => Some(peek),
        }
    }

    fn lease(self) -> RepeatLease {
        match self {
            Self::Held { lease, .. } => lease,
            Self::Unseen | Self::Released { .. } => RepeatLease::Free,
        }
    }
}

/// All state for one keyboard/key generation: what the host showed of the
/// key ([`KeyPhase`]), and what the guest got of it ([`Delivery`]).
/// docs/KEYBOARD.md describes the machine.
///
/// A generation is retained after physical release only while a delayed guest
/// delivery channel still needs its ownership or suppression tombstone, or
/// another held key needs its peek provenance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyGeneration {
    id: u64,
    phase: KeyPhase,
    delivery: Delivery,
}

impl KeyGeneration {
    fn new(id: u64) -> Self {
        Self {
            id,
            phase: KeyPhase::Unseen,
            delivery: Delivery::default(),
        }
    }

    fn is_unreferenced(self) -> bool {
        let phase_unreferenced = match self.phase {
            KeyPhase::Unseen => true,
            KeyPhase::Held { .. } => false,
            KeyPhase::Released { peek, .. } => peek.provenance().is_none(),
        };
        phase_unreferenced && self.delivery == Delivery::default()
    }
}

/// The generation as the flat fields it had before its states were
/// explicit, which the model test checks it against.
#[cfg(test)]
impl KeyGeneration {
    fn physical_state(self) -> PhysicalKeyState {
        self.phase.kind()
    }

    fn peek(self) -> Option<PeekKeyProvenance> {
        self.phase.peek().provenance()
    }

    fn peek_press_serial(self) -> Option<u32> {
        self.phase.peek().serial()
    }

    fn physical_release_serial(self) -> Option<u32> {
        match self.phase {
            KeyPhase::Released { release_serial, .. } => release_serial,
            KeyPhase::Unseen | KeyPhase::Held { .. } => None,
        }
    }

    fn backspace_repeat_cancelled(self) -> bool {
        self.phase.lease() == RepeatLease::Cancelled
    }

    fn ime_repeat_owner(self) -> Option<GuestId> {
        match self.phase.lease() {
            RepeatLease::Leased { text_input } => Some(text_input.0),
            RepeatLease::Free | RepeatLease::Cancelled => None,
        }
    }

    fn guest_owner(self) -> Option<GuestKeyOwner> {
        self.delivery.owner
    }

    fn host_accelerator_suppressed(self) -> bool {
        self.delivery.accelerator_suppressed
    }

    fn stale(self) -> bool {
        matches!(self.phase, KeyPhase::Held { stale: true, .. })
    }
}

/// The next number of the counter `next`, which is never 0.
fn allocate(next: &mut u64) -> u64 {
    *next = next.wrapping_add(1).max(1);
    *next
}

/// Canonical state machine for every physical keyboard/key generation.
#[derive(Debug, Default)]
pub(super) struct KeyGenerationRegistry {
    next_generation: u64,
    entries: BTreeMap<HostId, BTreeMap<u32, KeyGeneration>>,
    /// The serial of the `wl_keyboard` release that a retired generation
    /// of each keyboard's key still owes the guest, at most one per key;
    /// the current generation's lives in `entries`. A newer retired release
    /// replaces the older one: Exo sends each `wl_keyboard.key` right after
    /// its `peek_key`, so a replaced release is not coming. See INVARIANTS
    /// "A key waits for at most one late release".
    retired_guest_releases: BTreeMap<(HostId, u32), u32>,
    latest_peek_sequences: BTreeMap<(GuestId, Option<GuestId>), PeekWatermark>,
}

impl KeyGenerationRegistry {
    /// `key`'s generation on `keyboard`, a new one, `Unseen`, if it has
    /// none.
    fn ensure_generation(&mut self, keyboard: HostId, key: u32) -> &mut KeyGeneration {
        let next = &mut self.next_generation;
        self.entries
            .entry(keyboard)
            .or_default()
            .entry(key)
            .or_insert_with(|| KeyGeneration::new(allocate(next)))
    }

    fn prune_key(&mut self, keyboard: HostId, key: u32) {
        let remove = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.is_unreferenced());
        if remove {
            if let Some(keys) = self.entries.get_mut(&keyboard) {
                keys.remove(&key);
            }
        }
        if self
            .entries
            .get(&keyboard)
            .is_some_and(|keys| keys.is_empty())
        {
            self.entries.remove(&keyboard);
        }
    }

    /// Every host keyboard with a key generation, a retired guest
    /// release or a peek watermark.
    #[cfg(test)]
    pub(super) fn keyboards_for_test(&self) -> impl Iterator<Item = HostId> + '_ {
        self.entries
            .keys()
            .copied()
            .chain(
                self.retired_guest_releases
                    .keys()
                    .map(|&(keyboard, _)| keyboard),
            )
            .chain(
                self.latest_peek_sequences
                    .values()
                    .map(|watermark| watermark.keyboard),
            )
    }

    /// Every peek watermark's guest seat, guest wl_surface and host
    /// keyboard.
    #[cfg(test)]
    pub(super) fn peek_watermarks_for_test(
        &self,
    ) -> impl Iterator<Item = (GuestId, Option<GuestId>, HostId)> + '_ {
        self.latest_peek_sequences
            .iter()
            .map(|(&(seat, surface), watermark)| (seat, surface, watermark.keyboard))
    }

    pub(super) fn clear_keyboard(&mut self, keyboard: HostId) {
        self.entries.remove(&keyboard);
        self.retired_guest_releases
            .retain(|(pending_keyboard, _), _| *pending_keyboard != keyboard);
        self.latest_peek_sequences
            .retain(|_, watermark| watermark.keyboard != keyboard);
    }

    pub(super) fn record_latest_peek(
        &mut self,
        guest_seat: GuestId,
        focused_surface: Option<GuestId>,
        keyboard: HostId,
        sequence: u64,
    ) {
        self.latest_peek_sequences.insert(
            (guest_seat, focused_surface),
            PeekWatermark { keyboard, sequence },
        );
    }

    pub(super) fn latest_peek_sequence(
        &self,
        guest_seat: GuestId,
        focused_surface: Option<GuestId>,
    ) -> Option<u64> {
        self.latest_peek_sequences
            .get(&(guest_seat, focused_surface))
            .map(|watermark| watermark.sequence)
    }

    pub(super) fn clear_peek_watermarks_for_seat(&mut self, guest_seat: GuestId) {
        self.latest_peek_sequences
            .retain(|(seat, _), _| *seat != guest_seat);
    }

    pub(super) fn physically_held(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.phase.kind() == PhysicalKeyState::Held)
    }

    pub(super) fn physical_released(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.phase.kind() == PhysicalKeyState::Released)
    }

    /// The serial of the latest peek press of `key`'s generation (see
    /// [`PeekRecord`]).
    pub(super) fn peek_press_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.phase.peek().serial())
    }

    pub(super) fn physical_release_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| match generation.phase {
                KeyPhase::Released { release_serial, .. } => release_serial,
                KeyPhase::Unseen | KeyPhase::Held { .. } => None,
            })
    }

    /// Take the release a retired generation of `key` owes the guest, if
    /// it is the one under `serial`.
    pub(super) fn take_pending_physical_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        let taken = self.retired_guest_releases.get(&(keyboard, key)) == Some(&serial);
        if taken {
            self.retired_guest_releases.remove(&(keyboard, key));
        }
        taken
    }

    pub(super) fn any_physically_held(&self, keyboard: HostId) -> bool {
        self.entries.get(&keyboard).is_some_and(|keys| {
            keys.values()
                .any(|generation| generation.phase.kind() == PhysicalKeyState::Held)
        })
    }

    pub(super) fn observe_physical_event(
        &mut self,
        keyboard: HostId,
        key: u32,
        state: KeyState,
        serial: Option<u32>,
    ) {
        match state {
            KeyState::Pressed => self.observe_physical_press(keyboard, key),
            KeyState::Released => {
                if let Some(generation) = self
                    .entries
                    .get_mut(&keyboard)
                    .and_then(|keys| keys.get_mut(&key))
                {
                    // The release ends the hold's lease and detach marker.
                    let (peek, previous_release) = match generation.phase {
                        KeyPhase::Unseen => (PeekRecord::None, None),
                        KeyPhase::Held { peek, .. } => (peek, None),
                        KeyPhase::Released {
                            peek,
                            release_serial,
                        } => (peek, release_serial),
                    };
                    generation.phase = KeyPhase::Released {
                        peek,
                        release_serial: serial.or(previous_release),
                    };
                }
                // Match the previous physical/peek lifecycle: once no key on
                // this keyboard remains held, old peek provenance is no longer
                // a candidate. Delivery tombstones may still survive.
                if !self.any_physically_held(keyboard) {
                    let keys = self
                        .entries
                        .get(&keyboard)
                        .map(|keys| keys.keys().copied().collect::<Vec<_>>())
                        .unwrap_or_default();
                    if let Some(generations) = self.entries.get_mut(&keyboard) {
                        for generation in generations.values_mut() {
                            if let Some(peek) = generation.phase.peek_mut() {
                                *peek = peek.without_provenance();
                            }
                        }
                    }
                    for key in keys {
                        self.prune_key(keyboard, key);
                    }
                }
                self.prune_key(keyboard, key);
            }
            KeyState::Repeated => {}
        }
    }

    fn observe_physical_press(&mut self, keyboard: HostId, key: u32) {
        self.retire_released_generation(keyboard, key);
        let generation = self.ensure_generation(keyboard, key);
        if generation.phase == KeyPhase::Unseen {
            generation.phase = KeyPhase::HELD;
        }
    }

    /// Retire `key`'s released generation, if it has one, for a new
    /// press; the guest's press of it waits for its release beside the new
    /// generation.
    fn retire_released_generation(&mut self, keyboard: HostId, key: u32) {
        let Some(generation) = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .copied()
        else {
            return;
        };
        let KeyPhase::Released { release_serial, .. } = generation.phase else {
            return;
        };
        if let (Some(GuestKeyOwner::Physical), Some(release_serial)) =
            (generation.delivery.owner, release_serial)
        {
            // Replaces the key's older retired release, if any; see
            // `retired_guest_releases`.
            if let Some(replaced) = self
                .retired_guest_releases
                .insert((keyboard, key), release_serial)
            {
                log::debug!(
                    "key {key} of keyboard {keyboard}: a newer retired release replaces the \
                     one under serial {replaced}, which has not come"
                );
            }
        }
        if let Some(keys) = self.entries.get_mut(&keyboard) {
            keys.remove(&key);
        }
    }

    pub(super) fn install_enter_snapshot<I>(&mut self, keyboard: HostId, keys: I)
    where
        I: IntoIterator<Item = u32>,
    {
        self.clear_keyboard(keyboard);
        for key in keys {
            let generation = self.ensure_generation(keyboard, key);
            generation.phase = KeyPhase::HELD;
            generation.delivery.owner = Some(GuestKeyOwner::Physical);
        }
    }

    pub(super) fn peek(&self, keyboard: HostId, key: u32) -> Option<PeekKeyProvenance> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.phase.peek().provenance())
    }

    pub(super) fn peek_keys(
        &self,
        keyboard: HostId,
    ) -> impl Iterator<Item = (u32, PeekKeyProvenance)> + '_ {
        self.entries
            .get(&keyboard)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter_map(|(&key, generation)| {
                generation.phase.peek().provenance().map(|peek| (key, peek))
            })
    }

    fn observe_peek_press(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
        time: u32,
        eligible: bool,
    ) -> u64 {
        if !self.physically_held(keyboard, key) {
            // A released generation is retired for a new one.
            self.retire_released_generation(keyboard, key);
        }
        let Self {
            entries,
            next_generation,
            ..
        } = self;
        let keys = entries.entry(keyboard).or_default();
        let generation = keys
            .entry(key)
            .or_insert_with(|| KeyGeneration::new(allocate(next_generation)));
        // The peek numbers the generation as it arrives, so that the latest
        // peek has the highest sequence, a key held since `enter` included,
        // which the enter numbered in the order it listed the keys.
        let sequence = allocate(next_generation);
        generation.id = sequence;
        // The press holds the key; a held key keeps its lease and detach
        // marker.
        let (lease, stale) = match generation.phase {
            KeyPhase::Held { lease, stale, .. } => (lease, stale),
            KeyPhase::Unseen | KeyPhase::Released { .. } => (RepeatLease::Free, false),
        };
        let peek = PeekRecord::Live(PeekKeyProvenance {
            serial,
            time,
            sequence,
            eligible,
        });
        generation.phase = KeyPhase::Held { peek, lease, stale };
        sequence
    }

    /// A legacy duplicate peek press of a held, peeked key announces its
    /// next `wl_keyboard.key`.
    fn refresh_peek(&mut self, keyboard: HostId, key: u32, serial: u32, time: u32) {
        if let Some(KeyPhase::Held {
            peek: PeekRecord::Live(provenance),
            ..
        }) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .map(|generation| &mut generation.phase)
        {
            provenance.serial = serial;
            provenance.time = time;
        }
    }

    fn observe_peek_release(&mut self, keyboard: HostId, key: u32, serial: u32) {
        self.observe_physical_event(keyboard, key, KeyState::Released, Some(serial));
    }

    pub(super) fn invalidate_peek(&mut self, keyboard: HostId, key: u32) {
        if let Some(PeekRecord::Live(peek)) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .and_then(|generation| generation.phase.peek_mut())
        {
            peek.eligible = false;
        }
    }

    /// No key `keyboard` holds now may recover an IME-consumed repeat, and
    /// its next `peek_key` press is a new press (`KeyPhase::Held::stale`).
    pub(super) fn mark_held_keys_stale(&mut self, keyboard: HostId) {
        let generations = self.entries.get_mut(&keyboard).into_iter().flatten();
        for (_, generation) in generations {
            if let KeyPhase::Held { stale, .. } = &mut generation.phase {
                *stale = true;
            }
        }
    }

    /// Whether a `peek_key` press of `key` is a new press: the key is not
    /// held, or is held only across the input method's own detach (H2),
    /// whose generation that press retires
    /// ([`Self::retire_stale_generation`]).
    pub(super) fn peek_press_is_new(&self, keyboard: HostId, key: u32) -> bool {
        !self.physically_held(keyboard, key) || self.held_key_is_stale(keyboard, key)
    }

    /// Whether `keyboard` holds any key across the input method's own
    /// detach (H2) (`KeyPhase::Held::stale`).
    pub(super) fn holds_a_stale_key(&self, keyboard: HostId) -> bool {
        let mut generations = self.entries.get(&keyboard).into_iter().flatten();
        generations
            .any(|(_, generation)| matches!(generation.phase, KeyPhase::Held { stale: true, .. }))
    }

    /// Whether `key` is held across the input method's own detach (H2)
    /// (`KeyPhase::Held::stale`).
    pub(super) fn held_key_is_stale(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| {
                matches!(generation.phase, KeyPhase::Held { stale: true, .. })
            })
    }

    /// Retire `key`'s held generation if its peek is stale, for the key's
    /// new `peek_key` press: the old press's release died with the input
    /// method (H2), so nothing of its generation, the guest's ownership,
    /// a recovery or an accelerator's suppression, may carry over to the
    /// new press. The old press ends as a physical release would end it.
    ///
    /// Returns whether the guest still holds the old press, which
    /// `wl_keyboard` or the enter gave it, and is owed its release.
    pub(super) fn retire_stale_generation(&mut self, keyboard: HostId, key: u32) -> bool {
        if !self.held_key_is_stale(keyboard, key) {
            return false;
        }
        let release = self.transition_physical(keyboard, key, PhysicalKey::Release);
        if let Some(keys) = self.entries.get_mut(&keyboard) {
            keys.remove(&key);
        }
        self.prune_key(keyboard, key);
        release.forward()
    }

    /// The transition for one `peek_key` of `key` on `keyboard`; its
    /// effects on the guest and the seat are the caller's
    /// ([`PeekOutcome`]).
    ///
    /// A press of a key held across a detach retires that generation
    /// first ([`Self::retire_stale_generation`]). A press of a key not
    /// held, or held without a peek press, numbers the generation; it is
    /// eligible for recovery only if it is guest input that no host
    /// accelerator or shortcut has taken. A press of a key
    /// already peeked (a legacy duplicate) refreshes the peek's serial and
    /// time. A release closes the generation unless it is older
    /// than the key's latest peek: ChromeOS may reuse one serial across
    /// several key transitions, so within one serial the time orders them,
    /// but a newer time alone does not, as an old serial can arrive late
    /// with an unrelated time.
    pub(super) fn peek_key(&mut self, keyboard: HostId, key: u32, input: PeekInput) -> PeekOutcome {
        match input {
            PeekInput::Press {
                serial,
                time,
                guest_input,
            } => {
                let owes_release = self.retire_stale_generation(keyboard, key);
                let held = self.physically_held(keyboard, key);
                if held && self.peek(keyboard, key).is_some() {
                    self.refresh_peek(keyboard, key, serial, time);
                    return PeekOutcome {
                        owes_release,
                        ..PeekOutcome::default()
                    };
                }
                // peek_key may trail the wl_keyboard press it describes,
                // which may have found a host accelerator or a compositor
                // shortcut, even if a reload has since removed its
                // binding. Only a still-held generation is the one this
                // peek describes; a released one is retired by the press.
                let consumed_by_shortcut =
                    held && self.guest_owner(keyboard, key) == Some(GuestKeyOwner::Shortcut);
                let eligible = guest_input
                    && !consumed_by_shortcut
                    && !self.host_accelerator_suppressed(keyboard, key);
                let sequence = self.observe_peek_press(keyboard, key, serial, time, eligible);
                PeekOutcome {
                    owes_release,
                    numbered: Some(sequence),
                    ignored: false,
                }
            }
            PeekInput::Release { serial, time } => {
                if let Some(press) = self.peek(keyboard, key) {
                    let ordered = serial_is_after(serial, press.serial)
                        || (serial == press.serial && !serial_is_after(press.time, time));
                    if !ordered {
                        log::debug!(
                            "peek_key: dropping stale release serial={serial} time={time} \
                             before current serial={} time={}",
                            press.serial,
                            press.time,
                        );
                        return PeekOutcome {
                            ignored: true,
                            ..PeekOutcome::default()
                        };
                    }
                }
                self.observe_peek_release(keyboard, key, serial);
                PeekOutcome::default()
            }
        }
    }

    /// The physical transition for one `wl_keyboard.key` of `key` on
    /// `keyboard` with `serial`, which says whether the event still needs
    /// its delivery decided ([`KeyAdmission`]).
    ///
    /// `peek_key` promises that the `wl_keyboard.key` after it has its
    /// serial, and the extension sends each peek release before the
    /// matching `wl_keyboard` release. So a release under a retired
    /// generation's release serial closes only that generation's guest
    /// press, and leaves the current generation alone; a release of a
    /// peeked generation under another serial, and a press of a held peeked
    /// generation under another serial, belong to another generation and
    /// are dropped. Otherwise a press after a release starts a new
    /// generation, except the delayed press a released peek announced, and
    /// a release releases the key.
    pub(super) fn admit_key(
        &mut self,
        keyboard: HostId,
        key: u32,
        state: KeyState,
        serial: u32,
    ) -> KeyAdmission {
        let peek_press_serial = self.peek_press_serial(keyboard, key);
        match state {
            KeyState::Released => {
                if self.take_pending_physical_release(keyboard, key, serial) {
                    return KeyAdmission::Settled(KeyVerdict::Forward);
                }
                if peek_press_serial.is_some()
                    && self.physical_release_serial(keyboard, key) != Some(serial)
                {
                    return KeyAdmission::Settled(KeyVerdict::Decline);
                }
                self.observe_physical_event(keyboard, key, state, Some(serial));
            }
            KeyState::Pressed => {
                if self.physically_held(keyboard, key)
                    && peek_press_serial.is_some_and(|peek_serial| peek_serial != serial)
                {
                    return KeyAdmission::Settled(KeyVerdict::Decline);
                }
                let delayed_released_peek_press =
                    self.physical_released(keyboard, key) && peek_press_serial == Some(serial);
                if !delayed_released_peek_press {
                    self.observe_physical_event(keyboard, key, state, Some(serial));
                }
            }
            // The core drops a repeat.
            KeyState::Repeated => {}
        }
        KeyAdmission::Current
    }

    /// The one transition of the IME's Backspace repeat lease, for an input
    /// on the seat whose host keyboards are `seat`, in the order of their
    /// guest IDs ([`LeaseInput`]).
    pub(super) fn lease(&mut self, seat: &[HostId], input: LeaseInput) {
        match input {
            LeaseInput::Ime {
                ti,
                outcome: LeaseOutcome::End,
            } => self.end_leases(seat, Some(ti)),
            LeaseInput::Ime {
                ti,
                outcome: LeaseOutcome::ArmIfBackspaceHeld,
            } => {
                let held = seat.iter().copied().find(|&keyboard| {
                    self.physically_held(keyboard, EVDEV_KEY_BACKSPACE)
                        && !self.backspace_repeat_cancelled(keyboard)
                });
                match held {
                    Some(keyboard) => self.arm(keyboard, ti),
                    None => self.end_leases(seat, Some(ti)),
                }
            }
            LeaseInput::OtherPress { keyboard } => {
                self.set_lease(keyboard, |_| RepeatLease::Cancelled);
                self.end_leases(seat, None);
            }
            LeaseInput::BackspaceRelease | LeaseInput::FocusLost => self.end_leases(seat, None),
            LeaseInput::Recovered {
                ti,
                keyboard,
                backspace,
                sent,
            } => match (backspace, sent) {
                (true, true) => self.arm(keyboard, ti),
                (true, false) | (false, true) => self.end_leases(seat, Some(ti)),
                (false, false) => {}
            },
        }
    }

    /// Lease `keyboard`'s held Backspace to `ti`, unless it is cancelled or
    /// leased to another text input.
    fn arm(&mut self, keyboard: HostId, ti: TiId) {
        self.set_lease(keyboard, |lease| match lease {
            RepeatLease::Free => RepeatLease::Leased { text_input: ti },
            other => other,
        });
    }

    /// End the leases of `seat`'s keyboards, those of `ti` only if given,
    /// for the rest of their holds: ending the IME's repeat is not
    /// releasing the key, whose next empty confirmation in the same hold
    /// must not repeat it again.
    fn end_leases(&mut self, seat: &[HostId], ti: Option<TiId>) {
        for &keyboard in seat {
            self.set_lease(keyboard, |lease| match lease {
                RepeatLease::Leased { text_input } if ti.is_none_or(|ti| ti == text_input) => {
                    RepeatLease::Cancelled
                }
                other => other,
            });
        }
    }

    /// Move the lease of `keyboard`'s Backspace by `transition`, if it is
    /// held.
    fn set_lease(&mut self, keyboard: HostId, transition: impl FnOnce(RepeatLease) -> RepeatLease) {
        if let Some(KeyPhase::Held { lease, .. }) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&EVDEV_KEY_BACKSPACE))
            .map(|generation| &mut generation.phase)
        {
            *lease = transition(*lease);
        }
    }

    /// Whether `keyboard`'s held Backspace may not be leased again in this
    /// hold.
    pub(super) fn backspace_repeat_cancelled(&self, keyboard: HostId) -> bool {
        self.generation(keyboard, EVDEV_KEY_BACKSPACE)
            .is_some_and(|generation| generation.phase.lease() == RepeatLease::Cancelled)
    }

    /// The text input `keyboard`'s Backspace is leased to, if any.
    pub(super) fn backspace_lease(&self, keyboard: HostId) -> Option<TiId> {
        let generation = self.generation(keyboard, EVDEV_KEY_BACKSPACE)?;
        match generation.phase.lease() {
            RepeatLease::Leased { text_input } => Some(text_input),
            RepeatLease::Free | RepeatLease::Cancelled => None,
        }
    }

    pub(super) fn guest_owner(&self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        self.generation(keyboard, key)
            .and_then(|generation| generation.delivery.owner)
    }

    pub(super) fn host_accelerator_suppressed(&self, keyboard: HostId, key: u32) -> bool {
        self.generation(keyboard, key)
            .is_some_and(|generation| generation.delivery.accelerator_suppressed)
    }

    fn generation(&self, keyboard: HostId, key: u32) -> Option<&KeyGeneration> {
        self.entries.get(&keyboard).and_then(|keys| keys.get(&key))
    }

    /// Move `key`'s delivery by `transition`, and prune the generation if
    /// nothing refers to it any more.
    fn deliver<T>(
        &mut self,
        keyboard: HostId,
        key: u32,
        transition: impl FnOnce(Delivery) -> (Delivery, T),
    ) -> T {
        let before = self
            .generation(keyboard, key)
            .map_or_else(Delivery::default, |generation| generation.delivery);
        let (delivery, output) = transition(before);
        if delivery != before {
            self.ensure_generation(keyboard, key).delivery = delivery;
            self.prune_key(keyboard, key);
        }
        output
    }

    /// The delivery transition of a `wl_keyboard.key` that
    /// [`Self::admit_key`] found current ([`Delivery::physical`]). A press
    /// of a shortcut's generation, or of a host accelerator's, is not
    /// guest input, so its peek no longer makes it a recovery candidate,
    /// if `peek_key` raced ahead of it.
    pub(super) fn transition_physical(
        &mut self,
        keyboard: HostId,
        key: u32,
        event: PhysicalKey,
    ) -> KeyVerdict {
        let verdict = self.deliver(keyboard, key, |delivery| delivery.physical(event));
        if let PhysicalKey::Press { chord, .. } = event {
            let shortcut = self.guest_owner(keyboard, key) == Some(GuestKeyOwner::Shortcut);
            if shortcut || chord == ChordClass::HostAccelerator {
                self.invalidate_peek(keyboard, key);
            }
        }
        verdict
    }

    /// An IME recovery of `key` ([`Delivery::recover`]): whether the guest
    /// gets its balanced pair.
    pub(super) fn recover(&mut self, keyboard: HostId, key: u32) -> bool {
        self.deliver(keyboard, key, Delivery::recover)
    }
}

/// The registry's states as the model test sets them, through no
/// transition.
#[cfg(test)]
impl KeyGenerationRegistry {
    pub(super) fn claim_guest_owner(
        &mut self,
        keyboard: HostId,
        key: u32,
        owner: GuestKeyOwner,
    ) -> bool {
        let generation = self.ensure_generation(keyboard, key);
        if generation.delivery.owner.is_some() {
            return false;
        }
        generation.delivery.owner = Some(owner);
        true
    }

    fn take_guest_owner(&mut self, keyboard: HostId, key: u32) {
        self.deliver(keyboard, key, |delivery| {
            (
                Delivery {
                    owner: None,
                    ..delivery
                },
                (),
            )
        });
    }

    pub(super) fn suppress_host_accelerator(&mut self, keyboard: HostId, key: u32) {
        self.ensure_generation(keyboard, key)
            .delivery
            .accelerator_suppressed = true;
    }

    fn take_host_accelerator_suppression(&mut self, keyboard: HostId, key: u32) {
        self.deliver(keyboard, key, |delivery| {
            let accelerator_suppressed = false;
            let next = Delivery {
                accelerator_suppressed,
                ..delivery
            };
            (next, ())
        });
    }
}

#[cfg(test)]
mod tests;
