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

use std::collections::BTreeMap;

use super::{serial_is_after, HostId};

/// A text-input-v3 content type: `(hint, purpose)`.
pub type ContentType = (u32, u32);
/// Text-input-v3 surrounding text: `(text, cursor, anchor)`.
pub type SurroundingText = (String, i32, i32);
/// A text-input-v3 cursor rectangle: `(x, y, width, height)`.
pub type CursorRect = (i32, i32, i32, i32);

/// The text-input-v3 requests a guest made since its most recent commit,
/// each `None` until made.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct V3Pending {
    pub enabled: Option<bool>,
    pub surrounding_text: Option<Option<SurroundingText>>,
    pub content_type: Option<ContentType>,
    pub cursor_rect: Option<CursorRect>,
}

impl V3Pending {
    /// The pending state after `enable` or `disable`, which reset every
    /// other piece of state. The host's v1 has no unset cursor rectangle,
    /// so an empty one stands for it.
    pub fn reset(enabled: bool) -> Self {
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
pub struct V3Committed {
    pub enabled: bool,
    pub surrounding_text: Option<SurroundingText>,
    pub content_type: ContentType,
}

/// A content type as the host applies it: the v3 content type, and the
/// surrounding-text support told with it, or `None` if the host's
/// extension cannot be told one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostContentType {
    pub content_type: ContentType,
    pub surrounding_text_support: Option<bool>,
}

/// Provenance for one physical key generation observed through ChromeOS
/// `peek_key`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeekKeyProvenance {
    pub serial: u32,
    pub time: u32,
    pub sequence: u64,
    /// The generation may recover an IME-consumed repeat. Host accelerators
    /// and placement chords permanently clear this bit for the lifetime of
    /// the generation.
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

/// Exclusive guest-side ownership of one evdev key generation.
///
/// A key can have at most one owner: either a real `wl_keyboard` press is
/// awaiting its release, a text-input keysym press is awaiting its release, or
/// IME recovery already emitted a balanced pair and later host events must be
/// suppressed. Encoding these states as one enum prevents the parallel-set
/// inconsistencies that previously caused duplicate and stuck keys.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub enum GuestKeyOwner {
    Physical,
    TextInputKeysym,
    /// Sommelier consumed an application window-layout shortcut. The
    /// matching release is acknowledged as handled and never reaches the
    /// guest client.
    CompositorShortcut,
    /// A balanced synthetic pair was delivered. This completed-generation
    /// tombstone suppresses delayed duplicate channels and permits repeat
    /// recovery without leaving an open guest press.
    ImeRecovery,
}

impl GuestKeyOwner {
    /// The arbitration contract for compositor shortcuts; this is its only
    /// definition.
    ///
    /// A compositor shortcut may own a key generation only while no channel
    /// owns it (the first press, which also places the window) or while it
    /// already owns it (a duplicate press, or a repeat of the consumed
    /// chord). A generation that another channel delivered or recovered
    /// belongs to that channel, and the chord is not acted on.
    pub(crate) const fn admits_compositor_shortcut(owner: Option<Self>) -> bool {
        matches!(owner, None | Some(Self::CompositorShortcut))
    }
}

/// Normalized guest-delivery input from every host key channel.
///
/// Handlers decode protocol-specific fields and provide policy facts, while
/// the key-generation registry alone decides ownership, forwarding, and
/// wl_keyboard ACK semantics. The sum type prevents impossible combinations
/// such as attaching a host ACK to a text-input keysym.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyEvent {
    PhysicalPress {
        repeated: bool,
        host_accelerator: bool,
        ime_repeat_active: bool,
    },
    PhysicalRelease,
    CompositorShortcutPress,
    CompositorShortcutRelease,
    TextInputPress {
        serial: u32,
    },
    TextInputRepeat,
    TextInputRelease {
        serial: u32,
    },
    RecoverIme,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyDelivery {
    Drop,
    Forward,
    EmitBalancedPair,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestKeyDecision {
    pub delivery: GuestKeyDelivery,
    /// Present only for real wl_keyboard events.
    pub ack_handled: Option<bool>,
    /// The event closes the current generation's repeat session. Retired
    /// delayed releases deliberately leave a newer session untouched.
    pub ends_repeat: bool,
}

/// A guest press from a retired physical generation whose release channel has
/// not arrived yet.
///
/// Physical releases can be matched exactly to the preceding peek release.
/// Text-input releases only carry their own serial, so they are matched by
/// wrap-aware ordering against the press serial.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RetiredGuestRelease {
    press_serial: Option<u32>,
    release_serial: Option<u32>,
    next_press_serial: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PhysicalKeyState {
    #[default]
    Unseen,
    Held,
    Released,
}

/// All mutable state for one keyboard/key generation.
///
/// A generation is retained after physical release only while a delayed guest
/// delivery channel still needs its ownership or suppression tombstone. This
/// keeps physical state, ChromeOS peek provenance, IME repeat cancellation,
/// and guest delivery decisions under one authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KeyGeneration {
    id: u64,
    physical_state: PhysicalKeyState,
    peek: Option<PeekKeyProvenance>,
    /// Serial of the generation's latest peek press or repeat: its first
    /// peek press sets it, and each repeat or legacy duplicate press
    /// (`refresh_peek`) replaces it. `peek_key` promises that the
    /// `wl_keyboard.key` after it has the same serial, so this names the
    /// event the latest peek announced. Retained after physical release
    /// while another tombstone keeps the generation alive so the
    /// corresponding delayed wl_keyboard press can still be recognized.
    peek_press_serial: Option<u32>,
    /// Serial of the first physical release observed from either the extended
    /// peek channel or the regular wl_keyboard channel.
    physical_release_serial: Option<u32>,
    backspace_repeat_cancelled: bool,
    /// The host IME consumed this held key and subsequent physical repeat
    /// events belong to the same synthetic guest generation.
    ime_repeat_owner: Option<u32>,
    guest_owner: Option<GuestKeyOwner>,
    guest_press_serial: Option<u32>,
    host_accelerator_suppressed: bool,
}

impl KeyGeneration {
    fn new(id: u64) -> Self {
        Self {
            id,
            physical_state: PhysicalKeyState::Unseen,
            peek: None,
            peek_press_serial: None,
            physical_release_serial: None,
            backspace_repeat_cancelled: false,
            ime_repeat_owner: None,
            guest_owner: None,
            guest_press_serial: None,
            host_accelerator_suppressed: false,
        }
    }

    fn is_unreferenced(self) -> bool {
        self.physical_state != PhysicalKeyState::Held
            && self.peek.is_none()
            && !self.backspace_repeat_cancelled
            && self.ime_repeat_owner.is_none()
            && self.guest_owner.is_none()
            && !self.host_accelerator_suppressed
    }
}

/// Canonical state machine for every physical keyboard/key generation.
#[derive(Default)]
pub struct KeyGenerationRegistry {
    next_generation: u64,
    entries: BTreeMap<HostId, BTreeMap<u32, KeyGeneration>>,
    /// Releases that retired generations of each keyboard's key still owe
    /// the guest, at most one per channel that pressed them (`Physical` or
    /// `TextInputKeysym`); the current generation's lives in `entries`. A
    /// newer retired release replaces the older one. Exo sends each
    /// `wl_keyboard.key` right after its `peek_key`, so a replaced
    /// `wl_keyboard` release is not coming; it sends no keysym releases, so
    /// a keysym entry only waits, and the bound stops such entries piling
    /// up. See INVARIANTS "A key waits for at most one late release per
    /// channel".
    retired_guest_releases: BTreeMap<(HostId, u32, GuestKeyOwner), RetiredGuestRelease>,
    latest_peek_sequences: BTreeMap<(u32, Option<u32>), PeekWatermark>,
}

impl KeyGenerationRegistry {
    fn allocate_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.next_generation
    }

    fn ensure_generation(&mut self, keyboard: HostId, key: u32) -> &mut KeyGeneration {
        let needs_entry = !self
            .entries
            .get(&keyboard)
            .is_some_and(|keys| keys.contains_key(&key));
        if needs_entry {
            let id = self.allocate_generation();
            self.entries
                .entry(keyboard)
                .or_default()
                .insert(key, KeyGeneration::new(id));
        }
        self.entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .expect("key generation was inserted")
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
                    .map(|&(keyboard, _, _)| keyboard),
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
    ) -> impl Iterator<Item = (u32, Option<u32>, HostId)> + '_ {
        self.latest_peek_sequences
            .iter()
            .map(|(&(seat, surface), watermark)| (seat, surface, watermark.keyboard))
    }

    pub(crate) fn clear_keyboard(&mut self, keyboard: HostId) {
        self.entries.remove(&keyboard);
        self.retired_guest_releases
            .retain(|(pending_keyboard, _, _), _| *pending_keyboard != keyboard);
        self.latest_peek_sequences
            .retain(|_, watermark| watermark.keyboard != keyboard);
    }

    pub(crate) fn record_latest_peek(
        &mut self,
        guest_seat: u32,
        focused_surface: Option<u32>,
        keyboard: HostId,
        sequence: u64,
    ) {
        self.latest_peek_sequences.insert(
            (guest_seat, focused_surface),
            PeekWatermark { keyboard, sequence },
        );
    }

    pub(crate) fn latest_peek_sequence(
        &self,
        guest_seat: u32,
        focused_surface: Option<u32>,
    ) -> Option<u64> {
        self.latest_peek_sequences
            .get(&(guest_seat, focused_surface))
            .map(|watermark| watermark.sequence)
    }

    pub(crate) fn clear_peek_watermarks_for_seat(&mut self, guest_seat: u32) {
        self.latest_peek_sequences
            .retain(|(seat, _), _| *seat != guest_seat);
    }

    pub(crate) fn clear_peek_watermarks_for_surface(&mut self, guest_surface: u32) {
        self.latest_peek_sequences
            .retain(|(_, surface), _| *surface != Some(guest_surface));
    }

    pub(crate) fn physically_held(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Held)
    }

    pub(crate) fn physical_released(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Released)
    }

    /// The serial of the latest peek press or repeat of `key`'s generation
    /// (see `KeyGeneration::peek_press_serial`).
    pub(crate) fn peek_press_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.peek_press_serial)
    }

    pub(crate) fn physical_release_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.physical_release_serial)
    }

    pub(crate) fn take_pending_physical_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        self.take_retired_guest_release(keyboard, key, GuestKeyOwner::Physical, |release| {
            release.release_serial == Some(serial)
        })
    }

    pub(crate) fn take_pending_text_input_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        let current_press_serial = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| {
                generation
                    .guest_press_serial
                    .or(generation.peek_press_serial)
            });
        self.take_retired_guest_release(keyboard, key, GuestKeyOwner::TextInputKeysym, |release| {
            release
                .press_serial
                .is_some_and(|press_serial| serial_is_after(serial, press_serial))
                && release
                    .next_press_serial
                    .or(current_press_serial)
                    .is_none_or(|current_serial| !serial_is_after(serial, current_serial))
        })
    }

    fn take_retired_guest_release(
        &mut self,
        keyboard: HostId,
        key: u32,
        owner: GuestKeyOwner,
        matches: impl FnOnce(&RetiredGuestRelease) -> bool,
    ) -> bool {
        let map_key = (keyboard, key, owner);
        let taken = self
            .retired_guest_releases
            .get(&map_key)
            .is_some_and(matches);
        if taken {
            self.retired_guest_releases.remove(&map_key);
        }
        taken
    }

    pub(crate) fn any_physically_held(&self, keyboard: HostId) -> bool {
        self.entries.get(&keyboard).is_some_and(|keys| {
            keys.values()
                .any(|generation| generation.physical_state == PhysicalKeyState::Held)
        })
    }

    /// Observe one physical state notification from either keyboard channel.
    ///
    /// Both channels describe the same hardware generation, so a release from
    /// either one closes physical state. Guest-delivery ownership remains until
    /// its corresponding channel consumes the release.
    #[cfg(test)]
    pub(crate) fn observe_physical_state(&mut self, keyboard: HostId, key: u32, state: u32) {
        self.observe_physical_event(keyboard, key, state, None);
    }

    pub(crate) fn observe_physical_event(
        &mut self,
        keyboard: HostId,
        key: u32,
        state: u32,
        serial: Option<u32>,
    ) {
        match state {
            1 => self.observe_physical_press(keyboard, key, None),
            2 => {
                // A repeat is evidence about an existing physical generation,
                // never permission to invent one after a missing press.
                if let Some(generation) = self
                    .entries
                    .get_mut(&keyboard)
                    .and_then(|keys| keys.get_mut(&key))
                    .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    generation.physical_state = PhysicalKeyState::Held;
                }
            }
            0 => {
                if let Some(generation) = self
                    .entries
                    .get_mut(&keyboard)
                    .and_then(|keys| keys.get_mut(&key))
                {
                    generation.physical_state = PhysicalKeyState::Released;
                    if let Some(serial) = serial {
                        generation.physical_release_serial = Some(serial);
                    }
                    generation.backspace_repeat_cancelled = false;
                    generation.ime_repeat_owner = None;
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
                            generation.peek = None;
                        }
                    }
                    for key in keys {
                        self.prune_key(keyboard, key);
                    }
                }
                self.prune_key(keyboard, key);
            }
            _ => {}
        }
    }

    fn observe_physical_press(
        &mut self,
        keyboard: HostId,
        key: u32,
        next_press_serial: Option<u32>,
    ) {
        self.retire_released_generation(keyboard, key, next_press_serial);
        self.ensure_generation(keyboard, key).physical_state = PhysicalKeyState::Held;
    }

    fn retire_released_generation(
        &mut self,
        keyboard: HostId,
        key: u32,
        next_press_serial: Option<u32>,
    ) -> bool {
        let Some(generation) = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .copied()
            .filter(|generation| generation.physical_state == PhysicalKeyState::Released)
        else {
            return false;
        };
        let retired_release = match generation.guest_owner {
            Some(owner @ GuestKeyOwner::Physical) => {
                generation.physical_release_serial.map(|release_serial| {
                    (
                        owner,
                        RetiredGuestRelease {
                            press_serial: generation.guest_press_serial,
                            release_serial: Some(release_serial),
                            next_press_serial,
                        },
                    )
                })
            }
            Some(owner @ GuestKeyOwner::TextInputKeysym) => {
                generation.guest_press_serial.map(|press_serial| {
                    (
                        owner,
                        RetiredGuestRelease {
                            press_serial: Some(press_serial),
                            release_serial: None,
                            next_press_serial,
                        },
                    )
                })
            }
            Some(GuestKeyOwner::ImeRecovery | GuestKeyOwner::CompositorShortcut) | None => None,
        };
        if let Some((owner, retired_release)) = retired_release {
            // Replaces the channel's older retired release, if any; see
            // `retired_guest_releases`.
            if let Some(replaced) = self
                .retired_guest_releases
                .insert((keyboard, key, owner), retired_release)
            {
                log::debug!(
                    "key {key} of keyboard {}: a newer {owner:?} retired release \
                     replaces {replaced:?}, whose release has not come",
                    keyboard.0
                );
            }
        }
        if let Some(keys) = self.entries.get_mut(&keyboard) {
            keys.remove(&key);
        }
        true
    }

    pub(crate) fn install_enter_snapshot<I>(&mut self, keyboard: HostId, keys: I)
    where
        I: IntoIterator<Item = u32>,
    {
        self.clear_keyboard(keyboard);
        for key in keys {
            let generation = self.ensure_generation(keyboard, key);
            generation.physical_state = PhysicalKeyState::Held;
            generation.guest_owner = Some(GuestKeyOwner::Physical);
        }
    }

    pub(crate) fn peek(&self, keyboard: HostId, key: u32) -> Option<PeekKeyProvenance> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.peek)
    }

    pub(crate) fn peek_keys(
        &self,
        keyboard: HostId,
    ) -> impl Iterator<Item = (u32, PeekKeyProvenance)> + '_ {
        self.entries
            .get(&keyboard)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter_map(|(&key, generation)| generation.peek.map(|peek| (key, peek)))
    }

    pub(crate) fn observe_peek_press(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
        time: u32,
        eligible: bool,
    ) -> u64 {
        if !self.physically_held(keyboard, key) {
            // Unseen means a synthetic text-input channel arrived first and
            // this physical observation belongs to that same generation.
            // Released is the only unambiguous boundary for a new generation.
            self.observe_physical_press(keyboard, key, Some(serial));
        }
        let generation = self.ensure_generation(keyboard, key);
        let sequence = generation.id;
        generation.peek_press_serial = Some(serial);
        generation.peek = Some(PeekKeyProvenance {
            serial,
            time,
            sequence,
            eligible,
        });
        sequence
    }

    pub(crate) fn refresh_peek(&mut self, keyboard: HostId, key: u32, serial: u32, time: u32) {
        if let Some(generation) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
        {
            generation.peek_press_serial = Some(serial);
            if let Some(peek) = generation.peek.as_mut() {
                peek.serial = serial;
                peek.time = time;
            }
        }
    }

    pub(crate) fn observe_peek_release(&mut self, keyboard: HostId, key: u32, serial: u32) {
        self.observe_physical_event(keyboard, key, 0, Some(serial));
    }

    pub(crate) fn invalidate_peek(&mut self, keyboard: HostId, key: u32) {
        if let Some(peek) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .and_then(|generation| generation.peek.as_mut())
        {
            peek.eligible = false;
        }
    }

    pub(crate) fn cancel_backspace_repeat(&mut self, keyboard: HostId, backspace: u32) {
        if self.physically_held(keyboard, backspace) {
            let generation = self.ensure_generation(keyboard, backspace);
            generation.backspace_repeat_cancelled = true;
            generation.ime_repeat_owner = None;
        }
    }

    pub(crate) fn backspace_repeat_cancelled(&self, keyboard: HostId, backspace: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&backspace))
            .is_some_and(|generation| generation.backspace_repeat_cancelled)
    }

    pub(crate) fn arm_ime_repeat(
        &mut self,
        keyboard: HostId,
        key: u32,
        guest_text_input: u32,
    ) -> bool {
        let Some(generation) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .filter(|generation| {
                generation.physical_state == PhysicalKeyState::Held
                    && !generation.backspace_repeat_cancelled
                    && generation
                        .ime_repeat_owner
                        .is_none_or(|owner| owner == guest_text_input)
            })
        else {
            return false;
        };
        generation.ime_repeat_owner = Some(guest_text_input);
        true
    }

    pub(crate) fn ime_repeat_active(&self, keyboard: HostId, key: u32) -> bool {
        self.ime_repeat_owner(keyboard, key).is_some()
    }

    pub(crate) fn ime_repeat_owner(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.ime_repeat_owner)
    }

    pub(crate) fn cancel_backspace_repeat_for_owner(
        &mut self,
        keyboard: HostId,
        backspace: u32,
        guest_text_input: u32,
    ) -> bool {
        if self.ime_repeat_owner(keyboard, backspace) != Some(guest_text_input) {
            return false;
        }
        self.cancel_backspace_repeat(keyboard, backspace);
        true
    }

    pub(crate) fn guest_owner(&self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.guest_owner)
    }

    pub(crate) fn guest_press_serial(&self, keyboard: HostId, key: u32) -> Option<u32> {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .and_then(|generation| generation.guest_press_serial)
    }

    pub(crate) fn claim_guest_owner(
        &mut self,
        keyboard: HostId,
        key: u32,
        owner: GuestKeyOwner,
    ) -> bool {
        let generation = self.ensure_generation(keyboard, key);
        if generation.guest_owner.is_some() {
            return false;
        }
        generation.guest_owner = Some(owner);
        true
    }

    pub(crate) fn claim_text_input_owner(
        &mut self,
        keyboard: HostId,
        key: u32,
        serial: u32,
    ) -> bool {
        let (starts_new, released) = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .map_or((false, false), |generation| {
                let newer_than_guest_press = generation
                    .guest_press_serial
                    .is_none_or(|press_serial| serial_is_after(serial, press_serial));
                let released = generation.physical_state == PhysicalKeyState::Released;
                let after_release_boundary = generation
                    .physical_release_serial
                    .is_some_and(|release_serial| serial_is_after(serial, release_serial));
                let owner_can_start_next = match generation.guest_owner {
                    Some(GuestKeyOwner::TextInputKeysym) => released,
                    Some(GuestKeyOwner::ImeRecovery | GuestKeyOwner::CompositorShortcut) => {
                        generation.physical_state == PhysicalKeyState::Unseen || released
                    }
                    Some(GuestKeyOwner::Physical) | None => false,
                };
                (
                    owner_can_start_next
                        && newer_than_guest_press
                        && (!released || after_release_boundary),
                    released,
                )
            });
        if starts_new {
            if released {
                let retired = self.retire_released_generation(keyboard, key, Some(serial));
                debug_assert!(retired, "released generation was checked above");
            } else if let Some(keys) = self.entries.get_mut(&keyboard) {
                keys.remove(&key);
            }
        }
        let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym);
        if claimed {
            self.ensure_generation(keyboard, key).guest_press_serial = Some(serial);
        }
        claimed
    }

    pub(crate) fn take_guest_owner(&mut self, keyboard: HostId, key: u32) -> Option<GuestKeyOwner> {
        let owner = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .and_then(|generation| {
                let owner = generation.guest_owner.take();
                if owner.is_some() {
                    generation.guest_press_serial = None;
                }
                owner
            });
        self.prune_key(keyboard, key);
        owner
    }

    #[cfg(test)]
    pub(crate) fn take_guest_owner_if(
        &mut self,
        keyboard: HostId,
        key: u32,
        expected: GuestKeyOwner,
    ) -> bool {
        let matches = self
            .entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.guest_owner == Some(expected));
        if !matches {
            return false;
        }
        self.take_guest_owner(keyboard, key) == Some(expected)
    }

    pub(crate) fn complete_guest_owner_if(
        &mut self,
        keyboard: HostId,
        key: u32,
        expected: GuestKeyOwner,
    ) -> bool {
        let Some(generation) = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .filter(|generation| generation.guest_owner == Some(expected))
        else {
            return false;
        };
        generation.guest_owner = Some(GuestKeyOwner::ImeRecovery);
        true
    }

    pub(crate) fn suppress_host_accelerator(&mut self, keyboard: HostId, key: u32) {
        self.ensure_generation(keyboard, key)
            .host_accelerator_suppressed = true;
    }

    pub(crate) fn take_host_accelerator_suppression(&mut self, keyboard: HostId, key: u32) -> bool {
        let suppressed = self
            .entries
            .get_mut(&keyboard)
            .and_then(|keys| keys.get_mut(&key))
            .is_some_and(|generation| std::mem::take(&mut generation.host_accelerator_suppressed));
        self.prune_key(keyboard, key);
        suppressed
    }

    pub(crate) fn host_accelerator_suppressed(&self, keyboard: HostId, key: u32) -> bool {
        self.entries
            .get(&keyboard)
            .and_then(|keys| keys.get(&key))
            .is_some_and(|generation| generation.host_accelerator_suppressed)
    }

    /// Route a `wl_keyboard` event to the channel that owns its generation.
    ///
    /// This applies [`GuestKeyOwner::admits_compositor_shortcut`] inside the
    /// reducer, so the reducer stays consistent even if a caller routes an
    /// event to the wrong channel:
    ///
    /// * A physical press or release of a generation a compositor shortcut
    ///   owns is the shortcut's duplicate press or its release.
    /// * A shortcut press of a generation the contract does not admit is
    ///   rejected and becomes the owner's duplicate physical press. A shortcut
    ///   release of a generation the shortcut does not own is a physical
    ///   release. Both rejections are logged, because the keyboard handler
    ///   never routes them.
    fn arbitrate(event: GuestKeyEvent, owner: Option<GuestKeyOwner>) -> GuestKeyEvent {
        let shortcut_owned = owner == Some(GuestKeyOwner::CompositorShortcut);
        match event {
            GuestKeyEvent::PhysicalPress { .. } if shortcut_owned => {
                GuestKeyEvent::CompositorShortcutPress
            }
            GuestKeyEvent::PhysicalRelease if shortcut_owned => {
                GuestKeyEvent::CompositorShortcutRelease
            }
            GuestKeyEvent::CompositorShortcutPress
                if !GuestKeyOwner::admits_compositor_shortcut(owner) =>
            {
                log::warn!("rejecting a compositor shortcut press of a key owned by {owner:?}");
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: false,
                    ime_repeat_active: false,
                }
            }
            GuestKeyEvent::CompositorShortcutRelease if !shortcut_owned => {
                log::warn!("rejecting a compositor shortcut release of a key owned by {owner:?}");
                GuestKeyEvent::PhysicalRelease
            }
            event => event,
        }
    }

    /// Apply the only guest-delivery ownership transition for one key event.
    ///
    /// Physical state and peek provenance are observed separately because they
    /// can arrive even when no guest event is emitted. This reducer owns the
    /// mutually exclusive delivery channels and returns all information the
    /// protocol handlers need to encode their result.
    pub(crate) fn transition_guest_key(
        &mut self,
        keyboard: HostId,
        key: u32,
        event: GuestKeyEvent,
    ) -> GuestKeyDecision {
        match Self::arbitrate(event, self.guest_owner(keyboard, key)) {
            GuestKeyEvent::PhysicalPress {
                repeated,
                host_accelerator,
                ime_repeat_active,
            } => {
                let owner = self.guest_owner(keyboard, key);
                let forwarded = matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                );
                let ime_recovered = owner == Some(GuestKeyOwner::ImeRecovery);
                let suppress_for_ime = ime_recovered || (ime_repeat_active && !forwarded);
                if suppress_for_ime && owner.is_none() {
                    let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::ImeRecovery);
                    debug_assert!(claimed, "guest owner was checked above");
                }

                let accelerator_was_suppressed = self.host_accelerator_suppressed(keyboard, key);
                if accelerator_was_suppressed || host_accelerator {
                    if !forwarded {
                        self.suppress_host_accelerator(keyboard, key);
                    }
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(false),
                        ends_repeat: false,
                    };
                }
                if suppress_for_ime {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(false),
                        ends_repeat: false,
                    };
                }
                if forwarded && !repeated {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: Some(true),
                        ends_repeat: false,
                    };
                }
                if repeated {
                    return GuestKeyDecision {
                        delivery: if forwarded {
                            GuestKeyDelivery::Forward
                        } else {
                            GuestKeyDelivery::Drop
                        },
                        ack_handled: Some(forwarded),
                        ends_repeat: false,
                    };
                }

                let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::Physical);
                debug_assert!(claimed, "new physical delivery had no guest owner");
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::Forward,
                    ack_handled: Some(true),
                    ends_repeat: false,
                }
            }
            GuestKeyEvent::PhysicalRelease => {
                let accelerator_suppressed = self.take_host_accelerator_suppression(keyboard, key);
                let owner = self.guest_owner(keyboard, key);
                let ime_recovered = owner == Some(GuestKeyOwner::ImeRecovery);
                let forwarded = matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                );
                if !ime_recovered {
                    self.take_guest_owner(keyboard, key);
                }
                GuestKeyDecision {
                    delivery: if forwarded && !accelerator_suppressed && !ime_recovered {
                        GuestKeyDelivery::Forward
                    } else {
                        GuestKeyDelivery::Drop
                    },
                    ack_handled: Some(forwarded),
                    ends_repeat: true,
                }
            }
            GuestKeyEvent::CompositorShortcutPress => {
                let owner = self.guest_owner(keyboard, key);
                debug_assert!(
                    GuestKeyOwner::admits_compositor_shortcut(owner),
                    "arbitrate routes only admitted shortcut presses here"
                );
                if owner.is_none() {
                    let claimed =
                        self.claim_guest_owner(keyboard, key, GuestKeyOwner::CompositorShortcut);
                    debug_assert!(claimed, "shortcut press had no guest owner");
                }
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::Drop,
                    ack_handled: Some(true),
                    ends_repeat: false,
                }
            }
            GuestKeyEvent::CompositorShortcutRelease => {
                let taken = self.take_guest_owner(keyboard, key);
                debug_assert_eq!(taken, Some(GuestKeyOwner::CompositorShortcut));
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::Drop,
                    ack_handled: Some(true),
                    ends_repeat: true,
                }
            }
            GuestKeyEvent::TextInputPress { serial } => {
                let accelerator_generation = self.host_accelerator_suppressed(keyboard, key)
                    || (self.physically_held(keyboard, key)
                        && self
                            .peek(keyboard, key)
                            .is_some_and(|press| !press.eligible))
                    || self.guest_owner(keyboard, key) == Some(GuestKeyOwner::CompositorShortcut);
                if accelerator_generation
                    || self.guest_owner(keyboard, key) == Some(GuestKeyOwner::Physical)
                    || !self.claim_text_input_owner(keyboard, key, serial)
                {
                    GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: None,
                        ends_repeat: false,
                    }
                } else {
                    GuestKeyDecision {
                        delivery: GuestKeyDelivery::Forward,
                        ack_handled: None,
                        ends_repeat: false,
                    }
                }
            }
            GuestKeyEvent::TextInputRepeat => GuestKeyDecision {
                delivery: if matches!(
                    self.guest_owner(keyboard, key),
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                ) {
                    GuestKeyDelivery::Forward
                } else {
                    GuestKeyDelivery::Drop
                },
                ack_handled: None,
                ends_repeat: false,
            },
            GuestKeyEvent::TextInputRelease { serial } => {
                let retired = self.take_pending_text_input_release(keyboard, key, serial);
                let current = if retired {
                    true
                } else {
                    let serial_is_current = self
                        .guest_press_serial(keyboard, key)
                        .is_none_or(|press_serial| serial_is_after(serial, press_serial));
                    serial_is_current
                        && self.complete_guest_owner_if(
                            keyboard,
                            key,
                            GuestKeyOwner::TextInputKeysym,
                        )
                };
                if !current {
                    log::debug!(
                        "dropping keysym release {serial} of key {key} of keyboard {}: \
                         it matches no pending keysym press",
                        keyboard.0
                    );
                }
                GuestKeyDecision {
                    delivery: if current {
                        GuestKeyDelivery::Forward
                    } else {
                        GuestKeyDelivery::Drop
                    },
                    ack_handled: None,
                    ends_repeat: current && !retired,
                }
            }
            GuestKeyEvent::RecoverIme => {
                let owner = self.guest_owner(keyboard, key);
                // A compositor shortcut consumed this physical generation;
                // recovering it would deliver the chord's key to the guest
                // after Sommelier already acted on it.
                if matches!(
                    owner,
                    Some(
                        GuestKeyOwner::Physical
                            | GuestKeyOwner::TextInputKeysym
                            | GuestKeyOwner::CompositorShortcut
                    )
                ) {
                    return GuestKeyDecision {
                        delivery: GuestKeyDelivery::Drop,
                        ack_handled: None,
                        ends_repeat: false,
                    };
                }
                if owner.is_none() {
                    let claimed = self.claim_guest_owner(keyboard, key, GuestKeyOwner::ImeRecovery);
                    debug_assert!(claimed, "guest owner was checked above");
                }
                GuestKeyDecision {
                    delivery: GuestKeyDelivery::EmitBalancedPair,
                    ack_handled: None,
                    ends_repeat: false,
                }
            }
        }
    }
}

/// One `wl_keyboard` focus generation.
///
/// Keep both object namespaces: the guest surface drives text-input events,
/// while the host surface identifies delayed `wl_keyboard.leave` events even
/// after the guest mapping has been retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyboardFocus {
    pub guest_seat: u32,
    pub guest_surface: u32,
    pub host_surface: u32,
}

/// The guest surface each host `wl_pointer` has entered. A
/// `wl_pointer.button` names no surface, and a press on the surface of a
/// composition ends it (`handler::text_input::end_composition_on_press`).
#[derive(Debug, Default)]
pub struct PointerFocus(BTreeMap<HostId, u32>);

impl PointerFocus {
    /// `pointer` entered guest surface `surface`.
    pub fn enter(&mut self, pointer: HostId, surface: u32) {
        self.0.insert(pointer, surface);
    }

    /// `pointer` left guest surface `surface`. A `leave` of a surface it
    /// is not on, which a host can deliver late, changes nothing, as for
    /// a keyboard ([`KeyboardFocusRegistry::leave`]).
    pub fn leave(&mut self, pointer: HostId, surface: u32) {
        if self.surface(pointer) == Some(surface) {
            self.0.remove(&pointer);
        }
    }

    /// `pointer` ended.
    pub fn forget_pointer(&mut self, pointer: HostId) {
        self.0.remove(&pointer);
    }

    /// The surface `pointer` is on, if any.
    pub fn surface(&self, pointer: HostId) -> Option<u32> {
        self.0.get(&pointer).copied()
    }

    /// Guest surface `surface` ended: no pointer is on it.
    pub fn forget_surface(&mut self, surface: u32) {
        self.0.retain(|_, &mut entered| entered != surface);
    }

    #[cfg(test)]
    pub(super) fn focuses_for_test(&self) -> impl Iterator<Item = (HostId, u32)> + '_ {
        self.0.iter().map(|(&pointer, &surface)| (pointer, surface))
    }
}

/// One authoritative seat focus transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeatFocusChange {
    pub guest_seat: u32,
    pub previous_surface: Option<u32>,
    pub current_surface: Option<u32>,
}

/// Result of mutating keyboard focus ownership.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct KeyboardFocusUpdate {
    /// Whether the event changed focus or consumed one deliverable retired leave.
    pub accepted: bool,
    /// Seat transitions that must be projected to text-input objects.
    pub seat_changes: Vec<SeatFocusChange>,
    /// Keyboard sessions whose focus-scoped key state is no longer valid.
    pub retired_keyboards: Vec<HostId>,
}

/// Canonical keyboard-to-seat/surface focus registry.
///
/// Every keyboard on one seat must own the same surface. Entering a different
/// surface retires older keyboard generations for that seat instead of
/// retaining a fallback focus that a delayed leave could revive.
#[derive(Default)]
pub struct KeyboardFocusRegistry {
    keyboards: BTreeMap<HostId, KeyboardFocus>,
    /// Focus generations superseded by a different keyboard resource.
    ///
    /// Their seat focus is no longer authoritative, but the guest resource
    /// received an enter and still needs exactly one matching leave. A newer
    /// enter on the same keyboard supersedes its older generation without a
    /// tombstone because forwarding that delayed leave would clear the newer
    /// resource focus.
    retired_guest_enters: BTreeMap<(HostId, u32), KeyboardFocus>,
}

impl KeyboardFocusRegistry {
    /// The surface the keyboards of `guest_seat` focus. [`Self::enter`]
    /// retires every other keyboard of the seat on another surface, so
    /// they all focus the same one.
    pub fn surface_for_seat(&self, guest_seat: u32) -> Option<u32> {
        let surfaces = || {
            self.keyboards
                .values()
                .filter(move |focus| focus.guest_seat == guest_seat)
                .map(|focus| focus.guest_surface)
        };
        let surface = surfaces().next();
        debug_assert!(
            surfaces().all(|other| Some(other) == surface),
            "the keyboards of seat {guest_seat} focus different surfaces"
        );
        surface
    }

    pub fn focus_for_keyboard(&self, host_keyboard: HostId) -> Option<KeyboardFocus> {
        self.keyboards.get(&host_keyboard).copied()
    }

    pub fn keyboard_owns_surface(&self, host_keyboard: HostId, guest_surface: u32) -> bool {
        self.focus_for_keyboard(host_keyboard)
            .is_some_and(|focus| focus.guest_surface == guest_surface)
    }

    /// Every focus, current or retired, and its host keyboard.
    #[cfg(test)]
    pub(super) fn focuses_for_test(&self) -> impl Iterator<Item = (HostId, KeyboardFocus)> + '_ {
        self.keyboards
            .iter()
            .map(|(&keyboard, &focus)| (keyboard, focus))
            .chain(
                self.retired_guest_enters
                    .iter()
                    .map(|(&(keyboard, _), &focus)| (keyboard, focus)),
            )
    }

    #[cfg(test)]
    pub fn set_for_test(
        &mut self,
        host_keyboard: HostId,
        guest_seat: u32,
        guest_surface: u32,
        host_surface: u32,
    ) {
        let _ = self.enter(
            host_keyboard,
            KeyboardFocus {
                guest_seat,
                guest_surface,
                host_surface,
            },
        );
    }

    pub fn enter(&mut self, host_keyboard: HostId, focus: KeyboardFocus) -> KeyboardFocusUpdate {
        if self.focus_for_keyboard(host_keyboard) == Some(focus) {
            return KeyboardFocusUpdate::default();
        }

        let mut affected_seats = vec![focus.guest_seat];
        if let Some(previous_focus) = self.focus_for_keyboard(host_keyboard) {
            affected_seats.push(previous_focus.guest_seat);
        }
        affected_seats.sort_unstable();
        affected_seats.dedup();
        let previous_surfaces = affected_seats
            .iter()
            .map(|&guest_seat| (guest_seat, self.surface_for_seat(guest_seat)))
            .collect::<Vec<_>>();

        // A newer enter on the same wl_keyboard resource supersedes every
        // older resource generation. Delayed leaves for those generations
        // must not become visible after the newer enter.
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, current)| {
                (keyboard == host_keyboard
                    || (current.guest_seat == focus.guest_seat
                        && current.guest_surface != focus.guest_surface))
                    .then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable_by_key(|keyboard| keyboard.0);
        for keyboard in &retired_keyboards {
            if let Some(retired_focus) = self.keyboards.remove(keyboard) {
                if *keyboard != host_keyboard {
                    self.retired_guest_enters
                        .insert((*keyboard, retired_focus.host_surface), retired_focus);
                }
            }
        }
        self.keyboards.insert(host_keyboard, focus);

        let seat_changes = previous_surfaces
            .into_iter()
            .filter_map(|(guest_seat, previous_surface)| {
                let current_surface = self.surface_for_seat(guest_seat);
                (previous_surface != current_surface).then_some(SeatFocusChange {
                    guest_seat,
                    previous_surface,
                    current_surface,
                })
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards,
        }
    }

    pub fn leave(&mut self, host_keyboard: HostId, host_surface: u32) -> KeyboardFocusUpdate {
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return if self
                .retired_guest_enters
                .remove(&(host_keyboard, host_surface))
                .is_some()
            {
                KeyboardFocusUpdate {
                    accepted: true,
                    ..KeyboardFocusUpdate::default()
                }
            } else {
                KeyboardFocusUpdate::default()
            };
        };
        if focus.host_surface != host_surface {
            return KeyboardFocusUpdate::default();
        }
        self.remove_keyboard(host_keyboard, focus)
    }

    pub fn release(&mut self, host_keyboard: HostId) -> KeyboardFocusUpdate {
        self.retired_guest_enters
            .retain(|(keyboard, _), _| *keyboard != host_keyboard);
        let Some(focus) = self.focus_for_keyboard(host_keyboard) else {
            return KeyboardFocusUpdate::default();
        };
        self.remove_keyboard(host_keyboard, focus)
    }

    pub fn destroy_surface(&mut self, guest_surface: u32) -> KeyboardFocusUpdate {
        let mut affected_seats = self
            .keyboards
            .values()
            .filter_map(|focus| (focus.guest_surface == guest_surface).then_some(focus.guest_seat))
            .collect::<Vec<_>>();
        affected_seats.sort_unstable();
        affected_seats.dedup();

        let mut retired_keyboards = self
            .keyboards
            .iter()
            .filter_map(|(&keyboard, focus)| {
                (focus.guest_surface == guest_surface).then_some(keyboard)
            })
            .collect::<Vec<_>>();
        retired_keyboards.sort_unstable_by_key(|keyboard| keyboard.0);
        for keyboard in &retired_keyboards {
            self.keyboards.remove(keyboard);
        }
        self.retired_guest_enters
            .retain(|_, focus| focus.guest_surface != guest_surface);

        let seat_changes = affected_seats
            .into_iter()
            .map(|guest_seat| SeatFocusChange {
                guest_seat,
                previous_surface: Some(guest_surface),
                current_surface: self.surface_for_seat(guest_seat),
            })
            .collect();
        KeyboardFocusUpdate {
            accepted: !retired_keyboards.is_empty(),
            seat_changes,
            retired_keyboards,
        }
    }

    fn remove_keyboard(
        &mut self,
        host_keyboard: HostId,
        focus: KeyboardFocus,
    ) -> KeyboardFocusUpdate {
        let previous_surface = self.surface_for_seat(focus.guest_seat);
        self.keyboards.remove(&host_keyboard);
        let current_surface = self.surface_for_seat(focus.guest_seat);
        let seat_changes = (previous_surface != current_surface)
            .then_some(SeatFocusChange {
                guest_seat: focus.guest_seat,
                previous_surface,
                current_surface,
            })
            .into_iter()
            .collect();
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards: vec![host_keyboard],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;

    #[test]
    fn ime_repeat_lease_is_generation_and_keyboard_scoped() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let key = 14;
        let mut registry = KeyGenerationRegistry::default();
        registry.observe_physical_state(keyboard_a, key, 1);
        registry.observe_physical_state(keyboard_b, key, 1);

        assert!(registry.arm_ime_repeat(keyboard_a, key, 40));
        assert!(registry.ime_repeat_active(keyboard_a, key));
        assert_eq!(registry.ime_repeat_owner(keyboard_a, key), Some(40));
        assert!(!registry.ime_repeat_active(keyboard_b, key));

        registry.observe_physical_state(keyboard_a, key, 0);
        assert!(!registry.ime_repeat_active(keyboard_a, key));
        assert!(registry.arm_ime_repeat(keyboard_b, key, 41));
        registry.clear_keyboard(keyboard_a);
        assert!(registry.ime_repeat_active(keyboard_b, key));
        assert_eq!(registry.ime_repeat_owner(keyboard_b, key), Some(41));

        assert!(!registry.cancel_backspace_repeat_for_owner(keyboard_b, key, 40));
        assert!(registry.ime_repeat_active(keyboard_b, key));
        assert!(registry.cancel_backspace_repeat_for_owner(keyboard_b, key, 41));
        assert!(!registry.ime_repeat_active(keyboard_b, key));
        assert!(!registry.arm_ime_repeat(keyboard_b, key, 41));
    }

    const KEYBOARD_A: HostId = HostId(10);
    const KEYBOARD_B: HostId = HostId(11);

    /// Seat 1's focus on guest surface 20, host surface 200.
    const ON_20: KeyboardFocus = KeyboardFocus {
        guest_seat: 1,
        guest_surface: 20,
        host_surface: 200,
    };
    /// Seat 1's focus on guest surface 21, host surface 201.
    const ON_21: KeyboardFocus = KeyboardFocus {
        guest_seat: 1,
        guest_surface: 21,
        host_surface: 201,
    };

    fn accepted(
        seat_changes: Vec<SeatFocusChange>,
        retired_keyboards: Vec<HostId>,
    ) -> KeyboardFocusUpdate {
        KeyboardFocusUpdate {
            accepted: true,
            seat_changes,
            retired_keyboards,
        }
    }

    #[test]
    fn keyboard_focus_registry_balances_replacement_and_delayed_leave() {
        let mut registry = KeyboardFocusRegistry::default();

        assert_eq!(
            registry.enter(KEYBOARD_A, ON_20),
            accepted(
                vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: None,
                    current_surface: Some(20),
                }],
                Vec::new()
            )
        );
        assert_eq!(
            registry.enter(KEYBOARD_B, ON_21),
            accepted(
                vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: Some(21),
                }],
                vec![KEYBOARD_A]
            )
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
        assert_eq!(
            registry.leave(KEYBOARD_A, ON_20.host_surface),
            accepted(Vec::new(), Vec::new()),
            "a delayed leave must balance the retired guest enter once"
        );
        assert_eq!(
            registry.leave(KEYBOARD_A, ON_20.host_surface),
            KeyboardFocusUpdate::default(),
            "a duplicate delayed leave must be rejected"
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
    }

    /// A delayed leave answers an enter the guest saw. Once that enter is
    /// gone, superseded by a newer enter on the same keyboard, released or
    /// destroyed, the leave changes nothing.
    #[test]
    fn keyboard_focus_registry_rejects_a_leave_for_a_superseded_or_ended_enter() {
        struct Case {
            what_ended_the_enter: &'static str,
            end_the_enter: fn(&mut KeyboardFocusRegistry),
            expected_focus_of_a: Option<KeyboardFocus>,
        }
        let cases = [
            Case {
                what_ended_the_enter: "a newer enter on the same keyboard",
                end_the_enter: |registry| {
                    registry.enter(KEYBOARD_A, ON_21);
                },
                expected_focus_of_a: Some(ON_21),
            },
            Case {
                what_ended_the_enter: "a release, after another keyboard took the seat",
                end_the_enter: |registry| {
                    registry.enter(KEYBOARD_B, ON_21);
                    registry.release(KEYBOARD_A);
                },
                expected_focus_of_a: None,
            },
            Case {
                what_ended_the_enter: "a destroy, after another keyboard took the seat",
                end_the_enter: |registry| {
                    registry.enter(KEYBOARD_B, ON_21);
                    registry.destroy_surface(ON_20.guest_surface);
                },
                expected_focus_of_a: None,
            },
        ];
        for case in cases {
            let mut registry = KeyboardFocusRegistry::default();
            registry.enter(KEYBOARD_A, ON_20);
            (case.end_the_enter)(&mut registry);

            let what = case.what_ended_the_enter;
            assert_eq!(
                registry.leave(KEYBOARD_A, ON_20.host_surface),
                KeyboardFocusUpdate::default(),
                "{what}"
            );
            assert_eq!(
                registry.focus_for_keyboard(KEYBOARD_A),
                case.expected_focus_of_a,
                "{what}"
            );
            assert_eq!(registry.surface_for_seat(1), Some(21), "{what}");
        }
    }

    #[test]
    fn keyboard_focus_registry_keeps_shared_surface_until_last_owner() {
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(KEYBOARD_A, ON_20);
        registry.enter(KEYBOARD_B, ON_20);

        assert_eq!(
            registry.leave(KEYBOARD_A, ON_20.host_surface),
            accepted(Vec::new(), vec![KEYBOARD_A])
        );
        assert_eq!(registry.surface_for_seat(1), Some(20));
        assert_eq!(
            registry.leave(KEYBOARD_B, ON_20.host_surface),
            accepted(
                vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: None,
                }],
                vec![KEYBOARD_B]
            )
        );
        assert_eq!(registry.surface_for_seat(1), None);
    }

    #[test]
    fn keyboard_focus_registry_destroys_surface_across_seats_without_fallback() {
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(KEYBOARD_A, ON_20);
        registry.enter(
            KEYBOARD_B,
            KeyboardFocus {
                guest_seat: 2,
                ..ON_20
            },
        );
        registry.enter(
            HostId(12),
            KeyboardFocus {
                guest_seat: 3,
                guest_surface: 30,
                host_surface: 300,
            },
        );

        assert_eq!(
            registry.destroy_surface(20),
            accepted(
                vec![
                    SeatFocusChange {
                        guest_seat: 1,
                        previous_surface: Some(20),
                        current_surface: None,
                    },
                    SeatFocusChange {
                        guest_seat: 2,
                        previous_surface: Some(20),
                        current_surface: None,
                    },
                ],
                vec![KEYBOARD_A, KEYBOARD_B]
            )
        );
        assert_eq!(registry.surface_for_seat(1), None);
        assert_eq!(registry.surface_for_seat(2), None);
        assert_eq!(registry.surface_for_seat(3), Some(30));
    }

    #[test]
    fn keyboard_focus_registry_preserves_invariants_for_all_short_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            Enter(HostId, KeyboardFocus),
            /// The keyboard leaves the focus's host surface.
            Leave(HostId, KeyboardFocus),
            Release(HostId),
            /// The focus's guest surface is destroyed.
            Destroy(KeyboardFocus),
        }

        let mut operations = Vec::new();
        for keyboard in [KEYBOARD_A, KEYBOARD_B] {
            for focus in [ON_20, ON_21] {
                operations.push(Operation::Enter(keyboard, focus));
                operations.push(Operation::Leave(keyboard, focus));
            }
            operations.push(Operation::Release(keyboard));
        }
        operations.extend([Operation::Destroy(ON_20), Operation::Destroy(ON_21)]);
        let sequence_len = 5;
        let sequence_count = operations.len().pow(sequence_len);

        for mut encoded in 0..sequence_count {
            let mut registry = KeyboardFocusRegistry::default();
            // Model the focus currently visible to each guest wl_keyboard
            // resource. A newer enter on the same resource supersedes its
            // previous surface; active and retired registry generations must
            // account for this model exactly.
            let mut guest_focus = BTreeMap::new();
            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                let update = match operation {
                    Operation::Enter(keyboard, focus) => {
                        let update = registry.enter(keyboard, focus);
                        if update.accepted {
                            guest_focus.insert(keyboard, focus);
                        }
                        update
                    }
                    Operation::Leave(keyboard, focus) => {
                        let update = registry.leave(keyboard, focus.host_surface);
                        if update.accepted {
                            assert_eq!(
                                guest_focus.get(&keyboard).map(|focus| focus.host_surface),
                                Some(focus.host_surface),
                                "only a guest-visible enter can accept a leave"
                            );
                            guest_focus.remove(&keyboard);
                        }
                        update
                    }
                    Operation::Release(keyboard) => {
                        guest_focus.remove(&keyboard);
                        registry.release(keyboard)
                    }
                    Operation::Destroy(focus) => {
                        guest_focus.retain(|_, guest| guest.guest_surface != focus.guest_surface);
                        registry.destroy_surface(focus.guest_surface)
                    }
                };

                for change in &update.seat_changes {
                    assert_ne!(change.previous_surface, change.current_surface);
                    assert_eq!(
                        registry.surface_for_seat(change.guest_seat),
                        change.current_surface
                    );
                }
                for left in registry.keyboards.values() {
                    for right in registry.keyboards.values() {
                        if left.guest_seat == right.guest_seat {
                            assert_eq!(
                                left.guest_surface, right.guest_surface,
                                "one seat must never retain competing surface generations"
                            );
                        }
                    }
                }
                for (&(keyboard, host_surface), retired) in &registry.retired_guest_enters {
                    assert_eq!(retired.host_surface, host_surface);
                    assert!(
                        !registry.keyboards.contains_key(&keyboard),
                        "one keyboard cannot have active and retired guest enters"
                    );
                    assert_eq!(
                        registry
                            .retired_guest_enters
                            .keys()
                            .filter(|(candidate, _)| *candidate == keyboard)
                            .count(),
                        1,
                        "one keyboard can have at most one deliverable retired leave"
                    );
                }
                assert_eq!(
                    guest_focus.len(),
                    registry.keyboards.len() + registry.retired_guest_enters.len(),
                    "every guest-visible focus must be active or await one retired leave"
                );
                for (&keyboard, &focus) in &guest_focus {
                    assert!(
                        registry.focus_for_keyboard(keyboard) == Some(focus)
                            || registry
                                .retired_guest_enters
                                .get(&(keyboard, focus.host_surface))
                                == Some(&focus),
                        "the registry must account for the guest resource's current focus"
                    );
                }
            }
        }
    }

    #[test]
    fn guest_key_owner_matches_the_model_for_all_short_transition_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            Claim(GuestKeyOwner),
            Take(GuestKeyOwner),
        }

        let operations = [
            Operation::Claim(GuestKeyOwner::Physical),
            Operation::Claim(GuestKeyOwner::TextInputKeysym),
            Operation::Claim(GuestKeyOwner::ImeRecovery),
            Operation::Claim(GuestKeyOwner::CompositorShortcut),
            Operation::Take(GuestKeyOwner::Physical),
            Operation::Take(GuestKeyOwner::TextInputKeysym),
            Operation::Take(GuestKeyOwner::ImeRecovery),
            Operation::Take(GuestKeyOwner::CompositorShortcut),
        ];
        let sequence_len = 5;
        let sequence_count = operations.len().pow(sequence_len);
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let keyboard = HostId(10);
        let key = 57;

        for mut encoded in 0..sequence_count {
            ctx.key_generations.clear_keyboard(keyboard);
            let mut model = None;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                match operation {
                    Operation::Claim(owner) => {
                        let expected = model.is_none();
                        assert_eq!(ctx.claim_guest_key(keyboard, key, owner), expected);
                        if expected {
                            model = Some(owner);
                        }
                    }
                    Operation::Take(owner) => {
                        let expected = model == Some(owner);
                        assert_eq!(ctx.take_guest_key_if(keyboard, key, owner), expected);
                        if expected {
                            model = None;
                        }
                    }
                }
                assert_eq!(ctx.guest_key_owner(keyboard, key), model);
            }
        }
    }

    #[test]
    fn compositor_shortcut_arbitration_admits_only_unowned_or_shortcut_keys() {
        for (owner, admitted) in [
            (None, true),
            (Some(GuestKeyOwner::CompositorShortcut), true),
            (Some(GuestKeyOwner::Physical), false),
            (Some(GuestKeyOwner::TextInputKeysym), false),
            (Some(GuestKeyOwner::ImeRecovery), false),
        ] {
            assert_eq!(
                GuestKeyOwner::admits_compositor_shortcut(owner),
                admitted,
                "{owner:?}"
            );
        }
    }

    const PRESS: GuestKeyEvent = GuestKeyEvent::PhysicalPress {
        repeated: false,
        host_accelerator: false,
        ime_repeat_active: false,
    };
    const REPEAT: GuestKeyEvent = GuestKeyEvent::PhysicalPress {
        repeated: true,
        host_accelerator: false,
        ime_repeat_active: false,
    };
    const ACCELERATOR_PRESS: GuestKeyEvent = GuestKeyEvent::PhysicalPress {
        repeated: false,
        host_accelerator: true,
        ime_repeat_active: false,
    };

    fn forwarded(ack_handled: Option<bool>, ends_repeat: bool) -> GuestKeyDecision {
        GuestKeyDecision {
            delivery: GuestKeyDelivery::Forward,
            ack_handled,
            ends_repeat,
        }
    }

    fn dropped(ack_handled: Option<bool>, ends_repeat: bool) -> GuestKeyDecision {
        GuestKeyDecision {
            delivery: GuestKeyDelivery::Drop,
            ack_handled,
            ends_repeat,
        }
    }

    /// Give `key` of keyboard A `event`, and check the reducer's decision.
    fn assert_decision(
        registry: &mut KeyGenerationRegistry,
        key: u32,
        event: GuestKeyEvent,
        expected: GuestKeyDecision,
    ) {
        assert_eq!(
            registry.transition_guest_key(KEYBOARD_A, key, event),
            expected,
            "{event:?} of key {key}"
        );
    }

    /// [`assert_decision`] for each event of `steps` in turn.
    fn assert_decisions(
        registry: &mut KeyGenerationRegistry,
        key: u32,
        steps: &[(GuestKeyEvent, GuestKeyDecision)],
    ) {
        for &(event, expected) in steps {
            assert_decision(registry, key, event, expected);
        }
    }

    /// A registry whose compositor shortcut consumed the held `key`.
    fn shortcut_consumed(key: u32) -> KeyGenerationRegistry {
        let mut registry = KeyGenerationRegistry::default();
        registry.observe_physical_state(KEYBOARD_A, key, 1);
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::CompositorShortcutPress,
            dropped(Some(true), false),
        );
        registry
    }

    /// The reducer used to ack a shortcut press or release as handled
    /// whatever owned the key. An event that breaks the arbitration contract
    /// is now rejected: it is decided as the owner's physical event.
    #[test]
    fn compositor_shortcut_press_is_rejected_for_another_owner() {
        let key = 16;
        for owner in [
            GuestKeyOwner::Physical,
            GuestKeyOwner::TextInputKeysym,
            GuestKeyOwner::ImeRecovery,
        ] {
            // `reference` gets the physical events the shortcut stands for.
            let owned = || {
                let mut registry = KeyGenerationRegistry::default();
                registry.observe_physical_state(KEYBOARD_A, key, 1);
                assert!(registry.claim_guest_owner(KEYBOARD_A, key, owner));
                registry
            };
            let (mut registry, mut reference) = (owned(), owned());

            let decision = registry.transition_guest_key(
                KEYBOARD_A,
                key,
                GuestKeyEvent::CompositorShortcutPress,
            );
            assert_eq!(
                decision,
                reference.transition_guest_key(KEYBOARD_A, key, PRESS),
                "{owner:?}"
            );
            assert_eq!(
                decision,
                dropped(Some(owner != GuestKeyOwner::ImeRecovery), false),
                "{owner:?}"
            );
            assert_eq!(registry.guest_owner(KEYBOARD_A, key), Some(owner));

            // A shortcut release of that generation is a physical release.
            assert_eq!(
                registry.transition_guest_key(
                    KEYBOARD_A,
                    key,
                    GuestKeyEvent::CompositorShortcutRelease
                ),
                reference.transition_guest_key(KEYBOARD_A, key, GuestKeyEvent::PhysicalRelease),
                "{owner:?}"
            );
            assert_eq!(
                registry.guest_owner(KEYBOARD_A, key),
                reference.guest_owner(KEYBOARD_A, key)
            );
        }
    }

    #[test]
    fn physical_events_of_a_consumed_shortcut_stay_with_the_shortcut() {
        let key = 16;
        let mut registry = shortcut_consumed(key);
        for event in [PRESS, REPEAT] {
            assert_decision(&mut registry, key, event, dropped(Some(true), false));
            assert_eq!(
                registry.guest_owner(KEYBOARD_A, key),
                Some(GuestKeyOwner::CompositorShortcut)
            );
        }
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::PhysicalRelease,
            dropped(Some(true), true),
        );
        assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);
    }

    #[test]
    fn ime_recovery_does_not_replay_a_compositor_shortcut_key() {
        let key = 16;
        let mut registry = shortcut_consumed(key);

        // A consumed shortcut key must not reach the guest as a recovered
        // pair, and recovery must leave its ownership for the release.
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::RecoverIme,
            dropped(None, false),
        );
        assert_eq!(
            registry.guest_owner(KEYBOARD_A, key),
            Some(GuestKeyOwner::CompositorShortcut)
        );
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::CompositorShortcutRelease,
            dropped(Some(true), true),
        );
        assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);
    }

    #[test]
    fn guest_key_reducer_couples_delivery_ack_and_generation_ownership() {
        let mut registry = KeyGenerationRegistry::default();

        let physical_key = 30;
        assert_decisions(
            &mut registry,
            physical_key,
            &[
                (PRESS, forwarded(Some(true), false)),
                // A second protocol channel cannot duplicate the press.
                (
                    GuestKeyEvent::TextInputPress { serial: 1 },
                    dropped(None, false),
                ),
                (REPEAT, forwarded(Some(true), false)),
                (GuestKeyEvent::PhysicalRelease, forwarded(Some(true), true)),
                (
                    GuestKeyEvent::RecoverIme,
                    GuestKeyDecision {
                        delivery: GuestKeyDelivery::EmitBalancedPair,
                        ack_handled: None,
                        ends_repeat: false,
                    },
                ),
                // A delayed physical press cannot duplicate the recovered
                // pair.
                (
                    GuestKeyEvent::PhysicalPress {
                        repeated: false,
                        host_accelerator: false,
                        ime_repeat_active: true,
                    },
                    dropped(Some(false), false),
                ),
            ],
        );

        // An accelerator release cannot escape without a guest press.
        let accelerator_key = 31;
        assert_decisions(
            &mut registry,
            accelerator_key,
            &[
                (ACCELERATOR_PRESS, dropped(Some(false), false)),
                (GuestKeyEvent::PhysicalRelease, dropped(Some(false), true)),
            ],
        );

        // Each synthetic press has exactly one releasable owner.
        let keysym_key = 32;
        assert_decisions(
            &mut registry,
            keysym_key,
            &[
                (
                    GuestKeyEvent::TextInputPress { serial: 10 },
                    forwarded(None, false),
                ),
                (GuestKeyEvent::TextInputRepeat, forwarded(None, false)),
                (
                    GuestKeyEvent::TextInputRelease { serial: 11 },
                    forwarded(None, true),
                ),
                (
                    GuestKeyEvent::TextInputRelease { serial: 12 },
                    dropped(None, false),
                ),
            ],
        );
    }

    #[test]
    fn guest_key_reducer_prevents_keysym_from_bypassing_accelerator_generation() {
        let key = 57;
        let mut registry = KeyGenerationRegistry::default();

        // An ineligible held peek generation is already known to be a host
        // accelerator.
        registry.observe_peek_press(KEYBOARD_A, key, 10, 100, false);
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::TextInputPress { serial: 10 },
            dropped(None, false),
        );
        assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);

        // The text-input channel must also honor explicit accelerator
        // suppression.
        assert_decisions(
            &mut registry,
            key,
            &[
                (ACCELERATOR_PRESS, dropped(Some(false), false)),
                (
                    GuestKeyEvent::TextInputPress { serial: 11 },
                    dropped(None, false),
                ),
            ],
        );
        registry.observe_peek_release(KEYBOARD_A, key, 12);
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::PhysicalRelease,
            dropped(Some(false), true),
        );
        assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);

        // Accelerator suppression must not leak into the next released
        // generation.
        assert_decision(
            &mut registry,
            key,
            GuestKeyEvent::TextInputPress { serial: 13 },
            forwarded(None, false),
        );
    }

    #[test]
    fn guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces() {
        #[derive(Clone, Copy)]
        enum Operation {
            PhysicalPress,
            PhysicalRepeat,
            PhysicalRelease,
            TextPress,
            TextRepeat,
            TextRelease,
            Recover,
            ShortcutPress,
            ShortcutRelease,
        }

        let operations = [
            Operation::PhysicalPress,
            Operation::PhysicalRepeat,
            Operation::PhysicalRelease,
            Operation::TextPress,
            Operation::TextRepeat,
            Operation::TextRelease,
            Operation::Recover,
            Operation::ShortcutPress,
            Operation::ShortcutRelease,
        ];
        let sequence_len = 6;
        let sequence_count = operations.len().pow(sequence_len);
        let keyboard = HostId(10);
        let key = 30;

        for mut encoded in 0..sequence_count {
            let mut registry = KeyGenerationRegistry::default();
            let mut guest_press_open = false;
            let mut serial = 0_u32;

            for _ in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                serial = serial.wrapping_add(1);
                let event = match operation {
                    Operation::PhysicalPress => PRESS,
                    Operation::PhysicalRepeat => REPEAT,
                    Operation::PhysicalRelease => GuestKeyEvent::PhysicalRelease,
                    Operation::TextPress => GuestKeyEvent::TextInputPress { serial },
                    Operation::TextRepeat => GuestKeyEvent::TextInputRepeat,
                    Operation::TextRelease => GuestKeyEvent::TextInputRelease { serial },
                    Operation::Recover => GuestKeyEvent::RecoverIme,
                    Operation::ShortcutPress => GuestKeyEvent::CompositorShortcutPress,
                    Operation::ShortcutRelease => GuestKeyEvent::CompositorShortcutRelease,
                };
                let owner_before = registry.guest_owner(keyboard, key);
                let decision = registry.transition_guest_key(keyboard, key, event);

                assert_eq!(
                    decision.ack_handled,
                    match operation {
                        Operation::PhysicalPress => {
                            Some(owner_before != Some(GuestKeyOwner::ImeRecovery))
                        }
                        // An admitted shortcut press is acked as handled; a
                        // rejected one is the owner's duplicate press.
                        Operation::ShortcutPress => {
                            Some(owner_before != Some(GuestKeyOwner::ImeRecovery))
                        }
                        Operation::PhysicalRepeat
                        | Operation::PhysicalRelease
                        | Operation::ShortcutRelease => Some(matches!(
                            owner_before,
                            Some(
                                GuestKeyOwner::Physical
                                    | GuestKeyOwner::TextInputKeysym
                                    | GuestKeyOwner::CompositorShortcut
                            )
                        )),
                        Operation::TextPress
                        | Operation::TextRepeat
                        | Operation::TextRelease
                        | Operation::Recover => None,
                    },
                    "ACK policy must remain coupled to the generation owner"
                );
                assert_eq!(
                    decision.ends_repeat,
                    match operation {
                        Operation::PhysicalRelease | Operation::ShortcutRelease => true,
                        Operation::TextRelease => {
                            owner_before == Some(GuestKeyOwner::TextInputKeysym)
                        }
                        Operation::PhysicalPress
                        | Operation::PhysicalRepeat
                        | Operation::TextPress
                        | Operation::TextRepeat
                        | Operation::Recover
                        | Operation::ShortcutPress => false,
                    },
                    "only the release that closes the current generation may end repeat"
                );
                match (operation, decision.delivery) {
                    (
                        Operation::PhysicalPress | Operation::TextPress,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(!guest_press_open, "a second press cannot be forwarded");
                        guest_press_open = true;
                    }
                    (
                        Operation::PhysicalRepeat | Operation::TextRepeat,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(
                            guest_press_open,
                            "repeat forwarding requires an open guest press"
                        );
                    }
                    (
                        Operation::PhysicalRelease
                        | Operation::TextRelease
                        | Operation::ShortcutRelease,
                        GuestKeyDelivery::Forward,
                    ) => {
                        assert!(
                            guest_press_open,
                            "release forwarding requires an open guest press"
                        );
                        guest_press_open = false;
                    }
                    (_, GuestKeyDelivery::EmitBalancedPair) => {
                        assert!(
                            matches!(operation, Operation::Recover),
                            "only IME recovery may emit a balanced pair"
                        );
                        assert!(
                            !guest_press_open,
                            "recovery cannot overlap an owned guest press"
                        );
                    }
                    (_, GuestKeyDelivery::Drop) => {}
                    _ => panic!("delivery kind does not match its normalized input event"),
                }
                match operation {
                    Operation::ShortcutPress => assert_eq!(
                        registry.guest_owner(keyboard, key),
                        if GuestKeyOwner::admits_compositor_shortcut(owner_before) {
                            Some(GuestKeyOwner::CompositorShortcut)
                        } else {
                            owner_before
                        },
                        "a shortcut claims only an admitted generation"
                    ),
                    Operation::ShortcutRelease
                        if owner_before == Some(GuestKeyOwner::CompositorShortcut) =>
                    {
                        assert_eq!(registry.guest_owner(keyboard, key), None);
                    }
                    _ => {}
                }

                assert_eq!(
                    matches!(
                        registry.guest_owner(keyboard, key),
                        Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
                    ),
                    guest_press_open,
                    "registry ownership must exactly match the observer's open pair"
                );
            }
        }
    }

    #[test]
    fn key_generation_registry_matches_the_model_for_all_short_transition_sequences() {
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        struct ModelGeneration {
            id: u64,
            physical_state: PhysicalKeyState,
            peek: Option<PeekKeyProvenance>,
            peek_press_serial: Option<u32>,
            physical_release_serial: Option<u32>,
            backspace_repeat_cancelled: bool,
            ime_repeat_owner: Option<u32>,
            guest_owner: Option<GuestKeyOwner>,
            guest_press_serial: Option<u32>,
            host_accelerator_suppressed: bool,
        }

        impl ModelGeneration {
            fn is_unreferenced(self) -> bool {
                self.physical_state != PhysicalKeyState::Held
                    && self.peek.is_none()
                    && !self.backspace_repeat_cancelled
                    && self.ime_repeat_owner.is_none()
                    && self.guest_owner.is_none()
                    && !self.host_accelerator_suppressed
            }
        }

        #[derive(Default)]
        struct Model {
            next_generation: u64,
            keys: BTreeMap<u32, ModelGeneration>,
            retired_guest_releases: BTreeMap<(u32, GuestKeyOwner), RetiredGuestRelease>,
        }

        impl Model {
            fn ensure(&mut self, key: u32) -> &mut ModelGeneration {
                self.keys.entry(key).or_insert_with(|| {
                    self.next_generation = self.next_generation.wrapping_add(1).max(1);
                    ModelGeneration {
                        id: self.next_generation,
                        ..ModelGeneration::default()
                    }
                })
            }

            fn prune(&mut self) {
                self.keys
                    .retain(|_, generation| !generation.is_unreferenced());
            }

            fn press(&mut self, key: u32, next_press_serial: Option<u32>) {
                self.retire_released(key, next_press_serial);
                self.ensure(key).physical_state = PhysicalKeyState::Held;
            }

            fn retire_released(&mut self, key: u32, next_press_serial: Option<u32>) -> bool {
                let Some(generation) =
                    self.keys.get(&key).copied().filter(|generation| {
                        generation.physical_state == PhysicalKeyState::Released
                    })
                else {
                    return false;
                };
                let retired_release = match generation.guest_owner {
                    Some(owner @ GuestKeyOwner::Physical) => {
                        generation.physical_release_serial.map(|release_serial| {
                            (
                                owner,
                                RetiredGuestRelease {
                                    press_serial: generation.guest_press_serial,
                                    release_serial: Some(release_serial),
                                    next_press_serial,
                                },
                            )
                        })
                    }
                    Some(owner @ GuestKeyOwner::TextInputKeysym) => {
                        generation.guest_press_serial.map(|press_serial| {
                            (
                                owner,
                                RetiredGuestRelease {
                                    press_serial: Some(press_serial),
                                    release_serial: None,
                                    next_press_serial,
                                },
                            )
                        })
                    }
                    Some(GuestKeyOwner::ImeRecovery | GuestKeyOwner::CompositorShortcut) | None => {
                        None
                    }
                };
                if let Some((owner, retired_release)) = retired_release {
                    self.retired_guest_releases
                        .insert((key, owner), retired_release);
                }
                self.keys.remove(&key);
                true
            }

            fn repeat(&mut self, key: u32) {
                if let Some(generation) = self
                    .keys
                    .get_mut(&key)
                    .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    generation.physical_state = PhysicalKeyState::Held;
                }
            }

            fn release(&mut self, key: u32, serial: Option<u32>) {
                if let Some(generation) = self.keys.get_mut(&key) {
                    generation.physical_state = PhysicalKeyState::Released;
                    if let Some(serial) = serial {
                        generation.physical_release_serial = Some(serial);
                    }
                    generation.backspace_repeat_cancelled = false;
                    generation.ime_repeat_owner = None;
                }
                if !self
                    .keys
                    .values()
                    .any(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    for generation in self.keys.values_mut() {
                        generation.peek = None;
                    }
                }
                self.prune();
            }

            fn peek_press(&mut self, key: u32, serial: u32, time: u32, eligible: bool) {
                if !self
                    .keys
                    .get(&key)
                    .is_some_and(|generation| generation.physical_state == PhysicalKeyState::Held)
                {
                    self.press(key, Some(serial));
                }
                let generation = self.ensure(key);
                generation.peek_press_serial = Some(serial);
                generation.peek = Some(PeekKeyProvenance {
                    serial,
                    time,
                    sequence: generation.id,
                    eligible,
                });
            }

            fn peek_release(&mut self, key: u32, serial: u32) {
                self.release(key, Some(serial));
            }
        }

        #[derive(Clone, Copy)]
        enum Operation {
            Press(u32),
            Repeat(u32),
            Release(u32),
            PeekPress(u32, bool),
            PeekRelease(u32),
            RefreshPeek(u32),
            InvalidatePeek(u32),
            CancelRepeat(u32),
            ArmImeRepeat(u32, u32),
            CancelRepeatOwner(u32, u32),
            SuppressAccelerator(u32),
            TakeAcceleratorSuppression(u32),
            ClaimOwner(u32, GuestKeyOwner),
            ClaimTextInput(u32),
            CompleteTextInput(u32),
            TakePendingPhysicalRelease(u32),
            TakePendingTextInputRelease(u32),
            TakeOwner(u32),
            Clear,
        }

        const KEY_A: u32 = 30;
        const KEY_B: u32 = 57;
        let keyboard = HostId(10);
        let operations = [
            Operation::Press(KEY_A),
            Operation::Press(KEY_B),
            Operation::Repeat(KEY_A),
            Operation::Release(KEY_A),
            Operation::Release(KEY_B),
            Operation::PeekPress(KEY_A, true),
            Operation::PeekPress(KEY_B, false),
            Operation::PeekRelease(KEY_A),
            Operation::RefreshPeek(KEY_A),
            Operation::InvalidatePeek(KEY_B),
            Operation::CancelRepeat(KEY_A),
            Operation::ArmImeRepeat(KEY_A, 77),
            Operation::ArmImeRepeat(KEY_A, 78),
            Operation::CancelRepeatOwner(KEY_A, 77),
            Operation::CancelRepeatOwner(KEY_A, 78),
            Operation::SuppressAccelerator(KEY_B),
            Operation::TakeAcceleratorSuppression(KEY_B),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::Physical),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::TextInputKeysym),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::ImeRecovery),
            Operation::ClaimOwner(KEY_A, GuestKeyOwner::CompositorShortcut),
            Operation::ClaimTextInput(KEY_A),
            Operation::CompleteTextInput(KEY_A),
            Operation::TakePendingPhysicalRelease(KEY_A),
            Operation::TakePendingTextInputRelease(KEY_A),
            Operation::TakeOwner(KEY_A),
            Operation::Clear,
        ];
        let sequence_len = 4;
        let sequence_count = operations.len().pow(sequence_len);

        for mut encoded in 0..sequence_count {
            let mut registry = KeyGenerationRegistry::default();
            let mut model = Model::default();

            for step in 0..sequence_len {
                let operation = operations[encoded % operations.len()];
                encoded /= operations.len();
                let serial = step + 1;
                let time = step + 101;

                match operation {
                    Operation::Press(key) => {
                        registry.observe_physical_state(keyboard, key, 1);
                        model.press(key, None);
                    }
                    Operation::Repeat(key) => {
                        registry.observe_physical_state(keyboard, key, 2);
                        model.repeat(key);
                    }
                    Operation::Release(key) => {
                        registry.observe_physical_state(keyboard, key, 0);
                        model.release(key, None);
                    }
                    Operation::PeekPress(key, eligible) => {
                        registry.observe_peek_press(keyboard, key, serial, time, eligible);
                        model.peek_press(key, serial, time, eligible);
                    }
                    Operation::PeekRelease(key) => {
                        registry.observe_peek_release(keyboard, key, serial);
                        model.peek_release(key, serial);
                    }
                    Operation::RefreshPeek(key) => {
                        registry.refresh_peek(keyboard, key, serial, time);
                        if let Some(peek) = model.keys.get_mut(&key) {
                            peek.peek_press_serial = Some(serial);
                            if let Some(provenance) = peek.peek.as_mut() {
                                provenance.serial = serial;
                                provenance.time = time;
                            }
                        }
                    }
                    Operation::InvalidatePeek(key) => {
                        registry.invalidate_peek(keyboard, key);
                        if let Some(peek) = model
                            .keys
                            .get_mut(&key)
                            .and_then(|generation| generation.peek.as_mut())
                        {
                            peek.eligible = false;
                        }
                    }
                    Operation::CancelRepeat(key) => {
                        registry.cancel_backspace_repeat(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key).filter(|generation| {
                            generation.physical_state == PhysicalKeyState::Held
                        }) {
                            generation.backspace_repeat_cancelled = true;
                            generation.ime_repeat_owner = None;
                        }
                    }
                    Operation::ArmImeRepeat(key, owner) => {
                        let actual = registry.arm_ime_repeat(keyboard, key, owner);
                        let expected = model.keys.get_mut(&key).is_some_and(|generation| {
                            if generation.physical_state == PhysicalKeyState::Held
                                && !generation.backspace_repeat_cancelled
                                && generation
                                    .ime_repeat_owner
                                    .is_none_or(|current| current == owner)
                            {
                                generation.ime_repeat_owner = Some(owner);
                                true
                            } else {
                                false
                            }
                        });
                        assert_eq!(actual, expected);
                    }
                    Operation::CancelRepeatOwner(key, owner) => {
                        let actual =
                            registry.cancel_backspace_repeat_for_owner(keyboard, key, owner);
                        let expected = model.keys.get_mut(&key).is_some_and(|generation| {
                            if generation.ime_repeat_owner == Some(owner) {
                                generation.backspace_repeat_cancelled = true;
                                generation.ime_repeat_owner = None;
                                true
                            } else {
                                false
                            }
                        });
                        assert_eq!(actual, expected);
                    }
                    Operation::SuppressAccelerator(key) => {
                        registry.suppress_host_accelerator(keyboard, key);
                        model.ensure(key).host_accelerator_suppressed = true;
                    }
                    Operation::TakeAcceleratorSuppression(key) => {
                        registry.take_host_accelerator_suppression(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key) {
                            generation.host_accelerator_suppressed = false;
                        }
                        model.prune();
                    }
                    Operation::ClaimOwner(key, owner) => {
                        let actual = registry.claim_guest_owner(keyboard, key, owner);
                        let generation = model.ensure(key);
                        let expected = generation.guest_owner.is_none();
                        if expected {
                            generation.guest_owner = Some(owner);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::ClaimTextInput(key) => {
                        let actual = registry.claim_text_input_owner(keyboard, key, serial);
                        let (starts_new, released) =
                            model.keys.get(&key).map_or((false, false), |generation| {
                                let newer_than_guest_press =
                                    generation.guest_press_serial.is_none_or(|press_serial| {
                                        serial_is_after(serial, press_serial)
                                    });
                                let released =
                                    generation.physical_state == PhysicalKeyState::Released;
                                let after_release_boundary =
                                    generation.physical_release_serial.is_some_and(
                                        |release_serial| serial_is_after(serial, release_serial),
                                    );
                                let owner_can_start_next = match generation.guest_owner {
                                    Some(GuestKeyOwner::TextInputKeysym) => released,
                                    Some(
                                        GuestKeyOwner::ImeRecovery
                                        | GuestKeyOwner::CompositorShortcut,
                                    ) => {
                                        generation.physical_state == PhysicalKeyState::Unseen
                                            || released
                                    }
                                    Some(GuestKeyOwner::Physical) | None => false,
                                };
                                (
                                    owner_can_start_next
                                        && newer_than_guest_press
                                        && (!released || after_release_boundary),
                                    released,
                                )
                            });
                        if starts_new {
                            if released {
                                assert!(model.retire_released(key, Some(serial)));
                            } else {
                                model.keys.remove(&key);
                            }
                        }
                        let generation = model.ensure(key);
                        let expected = generation.guest_owner.is_none();
                        if expected {
                            generation.guest_owner = Some(GuestKeyOwner::TextInputKeysym);
                            generation.guest_press_serial = Some(serial);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakePendingTextInputRelease(key) => {
                        let actual =
                            registry.take_pending_text_input_release(keyboard, key, serial);
                        let current_press_serial = model.keys.get(&key).and_then(|generation| {
                            generation
                                .guest_press_serial
                                .or(generation.peek_press_serial)
                        });
                        let pending = (key, GuestKeyOwner::TextInputKeysym);
                        let expected =
                            model
                                .retired_guest_releases
                                .get(&pending)
                                .is_some_and(|release| {
                                    release.press_serial.is_some_and(|press_serial| {
                                        serial_is_after(serial, press_serial)
                                    }) && release
                                        .next_press_serial
                                        .or(current_press_serial)
                                        .is_none_or(|current_serial| {
                                            !serial_is_after(serial, current_serial)
                                        })
                                });
                        if expected {
                            model.retired_guest_releases.remove(&pending);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakePendingPhysicalRelease(key) => {
                        let release_serial = serial.wrapping_sub(2);
                        let actual =
                            registry.take_pending_physical_release(keyboard, key, release_serial);
                        let pending = (key, GuestKeyOwner::Physical);
                        let expected = model
                            .retired_guest_releases
                            .get(&pending)
                            .is_some_and(|release| release.release_serial == Some(release_serial));
                        if expected {
                            model.retired_guest_releases.remove(&pending);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::CompleteTextInput(key) => {
                        let actual = registry.complete_guest_owner_if(
                            keyboard,
                            key,
                            GuestKeyOwner::TextInputKeysym,
                        );
                        let expected = model.keys.get(&key).is_some_and(|generation| {
                            generation.guest_owner == Some(GuestKeyOwner::TextInputKeysym)
                        });
                        if expected {
                            model.ensure(key).guest_owner = Some(GuestKeyOwner::ImeRecovery);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakeOwner(key) => {
                        registry.take_guest_owner(keyboard, key);
                        if let Some(generation) = model.keys.get_mut(&key) {
                            generation.guest_owner = None;
                            generation.guest_press_serial = None;
                        }
                        model.prune();
                    }
                    Operation::Clear => {
                        registry.clear_keyboard(keyboard);
                        model.keys.clear();
                        model.retired_guest_releases.clear();
                    }
                }

                assert_eq!(registry.next_generation, model.next_generation);
                let actual = registry.entries.get(&keyboard);
                assert_eq!(
                    actual.map(BTreeMap::len).unwrap_or_default(),
                    model.keys.len()
                );
                for (&key, expected) in &model.keys {
                    let actual = &actual.expect("modeled keyboard entry")[&key];
                    assert_eq!(actual.id, expected.id);
                    assert_eq!(actual.physical_state, expected.physical_state);
                    assert_eq!(actual.peek, expected.peek);
                    assert_eq!(actual.peek_press_serial, expected.peek_press_serial);
                    assert_eq!(
                        actual.physical_release_serial,
                        expected.physical_release_serial
                    );
                    assert_eq!(
                        actual.backspace_repeat_cancelled,
                        expected.backspace_repeat_cancelled
                    );
                    assert_eq!(actual.ime_repeat_owner, expected.ime_repeat_owner);
                    assert_eq!(actual.guest_owner, expected.guest_owner);
                    assert_eq!(actual.guest_press_serial, expected.guest_press_serial);
                    assert_eq!(
                        actual.host_accelerator_suppressed,
                        expected.host_accelerator_suppressed
                    );
                }
                assert_eq!(
                    registry
                        .retired_guest_releases
                        .iter()
                        .filter(|((pending_keyboard, _, _), _)| *pending_keyboard == keyboard)
                        .map(|(&(_, key, owner), &release)| ((key, owner), release))
                        .collect::<BTreeMap<_, _>>(),
                    model.retired_guest_releases
                );
                assert!(
                    registry
                        .entries
                        .values()
                        .all(|keys| !keys.is_empty()
                            && keys.values().all(|key| !key.is_unreferenced())),
                    "the registry must not retain empty generation tombstones"
                );
            }
        }
    }

    #[test]
    fn repressed_key_replaces_its_released_peek_generation() {
        let keyboard = HostId(10);
        let key_a = 30;
        let key_b = 57;
        let mut registry = KeyGenerationRegistry::default();

        let first_a = registry.observe_peek_press(keyboard, key_a, 1, 10, false);
        let first_b = registry.observe_peek_press(keyboard, key_b, 2, 20, true);
        registry.observe_physical_state(keyboard, key_a, 0);
        assert_eq!(
            registry.peek(keyboard, key_a).map(|peek| peek.sequence),
            Some(first_a),
            "another held key retains the released generation as a causal tombstone"
        );

        let second_a = registry.observe_peek_press(keyboard, key_a, 3, 30, true);
        let peek = registry.peek(keyboard, key_a).unwrap();
        assert!(second_a > first_b);
        assert_ne!(second_a, first_a);
        assert_eq!(peek.sequence, second_a);
        assert!(peek.eligible);
    }

    /// A physical press that reaches the proxy after the IME's keysym press
    /// of the same key joins that press's generation.
    #[test]
    fn delayed_physical_press_preserves_keysym_release_owner() {
        type ObservePress = fn(&mut KeyGenerationRegistry, u32);
        let channels: [(&str, ObservePress); 2] = [
            ("wl_keyboard", |registry, key| {
                registry.observe_physical_state(KEYBOARD_A, key, 1)
            }),
            ("peek", |registry, key| {
                registry.observe_peek_press(KEYBOARD_A, key, 1, 10, true);
            }),
        ];
        let key = 30;
        for (channel, observe_press) in channels {
            let mut registry = KeyGenerationRegistry::default();
            assert!(registry.claim_guest_owner(KEYBOARD_A, key, GuestKeyOwner::TextInputKeysym));
            observe_press(&mut registry, key);

            assert!(registry.physically_held(KEYBOARD_A, key), "{channel}");
            assert_eq!(
                registry.guest_owner(KEYBOARD_A, key),
                Some(GuestKeyOwner::TextInputKeysym),
                "a delayed {channel} press must not orphan the synthetic press"
            );
        }
    }

    #[test]
    fn retired_releases_match_generation_intervals_and_serial_wrap() {
        let keyboard = HostId(10);
        let text_key = 30;
        let physical_key = 57;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, text_key, 10, 100, true);
        assert!(registry.claim_text_input_owner(keyboard, text_key, 10));
        registry.observe_peek_release(keyboard, text_key, 11);
        registry.observe_peek_press(keyboard, text_key, 20, 200, true);
        assert!(registry.claim_text_input_owner(keyboard, text_key, 20));
        registry.observe_peek_release(keyboard, text_key, 21);
        registry.observe_peek_press(keyboard, text_key, 30, 300, true);

        assert!(
            registry.take_pending_text_input_release(keyboard, text_key, 21),
            "a release must match the retired interval immediately before it"
        );
        assert!(
            !registry.take_pending_text_input_release(keyboard, text_key, 11),
            "a newer retired release of the channel replaces the older one"
        );
        assert!(
            !registry.take_pending_text_input_release(keyboard, text_key, 21),
            "each retired press must be released exactly once"
        );

        registry.observe_peek_press(keyboard, physical_key, u32::MAX - 1, 400, true);
        assert!(registry.claim_guest_owner(keyboard, physical_key, GuestKeyOwner::Physical));
        registry.observe_peek_release(keyboard, physical_key, u32::MAX);
        registry.observe_peek_press(keyboard, physical_key, 0, 500, true);
        assert!(
            registry.take_pending_physical_release(keyboard, physical_key, u32::MAX),
            "a physical release must survive the next generation across serial wrap"
        );
        assert!(registry.physically_held(keyboard, physical_key));
    }

    /// A host that never sends a channel's release of a key the guest got
    /// leaves one retired release of that key pending, however often the
    /// key is pressed again: the channel's newer retired release replaces
    /// the older one.
    #[test]
    fn a_channel_keeps_one_pending_release_per_key() {
        type Press = fn(&mut KeyGenerationRegistry, u32, u32);
        type TakeRelease = fn(&mut KeyGenerationRegistry, u32, u32) -> bool;
        let channels: [(&str, Press, TakeRelease); 2] = [
            (
                "wl_keyboard",
                |registry, key, _| {
                    let decision = registry.transition_guest_key(KEYBOARD_A, key, PRESS);
                    assert_eq!(decision, forwarded(Some(true), false));
                },
                |registry, key, serial| {
                    registry.take_pending_physical_release(KEYBOARD_A, key, serial)
                },
            ),
            (
                "keysym",
                |registry, key, serial| {
                    assert!(registry.claim_text_input_owner(KEYBOARD_A, key, serial));
                },
                |registry, key, serial| {
                    registry.take_pending_text_input_release(KEYBOARD_A, key, serial)
                },
            ),
        ];
        let key = 30;
        let generations = 100;
        for (channel, press, take_release) in channels {
            let mut registry = KeyGenerationRegistry::default();
            // Generation g is pressed with serial 10g on the peek channel
            // and on `channel`, and released with 10g + 1 on the peek
            // channel only.
            for generation in 1..=generations {
                let serial = 10 * generation;
                registry.observe_peek_press(KEYBOARD_A, key, serial, serial, true);
                press(&mut registry, key, serial);
                registry.observe_peek_release(KEYBOARD_A, key, serial + 1);
            }
            let next = 10 * (generations + 1);
            registry.observe_peek_press(KEYBOARD_A, key, next, next, true);

            for generation in 1..generations {
                assert!(
                    !take_release(&mut registry, key, 10 * generation + 1),
                    "{channel}: generation {generation}'s release must not be taken"
                );
            }
            assert!(
                take_release(&mut registry, key, 10 * generations + 1),
                "{channel}: the last generation must still take its release"
            );
        }
    }

    #[test]
    fn released_generation_rejects_delayed_press_before_release_boundary() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, key, u32::MAX - 3, 100, true);
        assert!(registry.claim_text_input_owner(keyboard, key, u32::MAX - 2));
        registry.observe_peek_release(keyboard, key, 0);

        assert!(
            !registry.claim_text_input_owner(keyboard, key, u32::MAX - 1),
            "a delayed press from before the physical release must remain in the old generation"
        );
        assert_eq!(
            registry.guest_press_serial(keyboard, key),
            Some(u32::MAX - 2)
        );
        assert!(
            registry.claim_text_input_owner(keyboard, key, 1),
            "a press after the wrapped release boundary must open the next generation"
        );
        assert_eq!(registry.guest_press_serial(keyboard, key), Some(1));

        let raw_key = 57;
        assert!(registry.claim_text_input_owner(keyboard, raw_key, 10));
        registry.observe_physical_event(keyboard, raw_key, 0, Some(20));
        assert!(
            !registry.claim_text_input_owner(keyboard, raw_key, 15),
            "the regular wl_keyboard release must also bound delayed text-input presses"
        );
        assert!(registry.claim_text_input_owner(keyboard, raw_key, 21));
    }

    #[test]
    fn clearing_guest_keys_is_scoped_to_one_keyboard() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        assert!(ctx.claim_guest_key(HostId(10), 57, GuestKeyOwner::ImeRecovery));
        assert!(ctx.claim_guest_key(HostId(11), 57, GuestKeyOwner::TextInputKeysym));
        for keyboard in [HostId(10), HostId(11)] {
            assert!(ctx.key_generations.claim_text_input_owner(keyboard, 30, 1));
            ctx.key_generations.observe_physical_state(keyboard, 30, 0);
            ctx.key_generations.observe_physical_state(keyboard, 30, 1);
        }

        ctx.key_generations.clear_keyboard(HostId(10));

        assert!(ctx.guest_key_owner(HostId(10), 57).is_none());
        assert_eq!(
            ctx.guest_key_owner(HostId(11), 57),
            Some(GuestKeyOwner::TextInputKeysym)
        );
        assert!(
            !ctx.key_generations
                .take_pending_text_input_release(HostId(10), 30, 2),
            "clearing a keyboard must discard its retired releases"
        );
        assert!(
            ctx.key_generations
                .take_pending_text_input_release(HostId(11), 30, 2),
            "clearing one keyboard must preserve another keyboard's retired releases"
        );
    }
}
