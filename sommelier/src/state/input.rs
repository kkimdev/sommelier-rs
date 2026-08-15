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

use std::collections::HashMap;

use super::{serial_is_after, HostId};

const MAX_PENDING_IME_DELETES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextInputState {
    pub host_v1_id: u32,
    pub host_ext_id: Option<u32>,
    pub guest_seat: u32,
    pub active_surface: Option<u32>,
    /// State requested by the guest since its most recent v3 commit.
    pub pending_enabled: bool,
    /// State applied by the most recent v3 commit.
    pub committed_enabled: bool,
    /// Whether an enable/disable request is waiting for the next commit.
    pub enabled_dirty: bool,
    /// Double-buffered v3 surrounding-text state.
    pub pending_surrounding_text: Option<(String, i32, i32)>,
    pub committed_surrounding_text: Option<(String, i32, i32)>,
    pub surrounding_text_dirty: bool,
    pub content_hint: u32,
    pub content_purpose: u32,
    /// Content type last committed to the host. `None` forces the next v3
    /// transaction to replay the pending type after an enable/disable reset.
    pub committed_content_type: Option<(u32, u32)>,
    pub content_type_dirty: bool,
    pub cursor_rect: Option<(i32, i32, i32, i32)>,
    pub cursor_rect_dirty: bool,
    pub text_change_cause: u32,
    pub current_preedit: String,
    /// Number of v3 commit requests received from this guest object.
    ///
    /// The v3 protocol requires every `done` event to use this counter as its
    /// serial. The same value is sent to the v1 host in `commit_state`, but
    /// Exo assigns its own serials to v1 events, so event translation must use
    /// this local counter rather than trusting the host event serial.
    pub guest_commit_serial: u32,
    /// Cursor/selection metadata accumulated before the next v1 preedit event.
    pub pending_preedit_cursor: Option<i32>,
    pub pending_preedit_selection: Option<(u32, u32)>,
    /// Edits accumulated by v1 and applied atomically by the following
    /// `commit_string` event.
    pub pending_deletes: Vec<(u32, u32)>,
    pub pending_cursor_position: Option<(i32, i32)>,
    pub host_activated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuestCommitPlan {
    pub host_v1_id: u32,
    pub host_ext_id: Option<u32>,
    pub guest_seat: u32,
    pub serial: u32,
    pub enabled: bool,
    pub enable_conflict: bool,
    pub resets_composition: bool,
    pub surrounding_text: Option<Option<(String, i32, i32)>>,
    pub has_surrounding_text: bool,
    pub content_type: Option<(u32, u32)>,
    pub cursor_rect: Option<(i32, i32, i32, i32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostPreeditPlan {
    pub guest_seat: u32,
    pub done_serial: u32,
    pub had_preedit: bool,
    pub selection: Option<(u32, u32)>,
    pub cursor: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostCommitPlan {
    pub guest_seat: u32,
    pub done_serial: u32,
    pub had_preedit: bool,
    pub deletes: Vec<(u32, u32)>,
    pub cursor_position: Option<(i32, i32)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreeditRegionPlan {
    pub guest_seat: u32,
    pub surrounding_text: String,
    pub surrounding_cursor: i32,
    pub done_serial: u32,
    pub selection: Option<(u32, u32)>,
    pub cursor: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfirmPreeditPlan {
    pub guest_seat: u32,
    pub done_serial: u32,
    pub preedit_text: String,
}

impl TextInputState {
    pub fn new(
        host_v1_id: u32,
        host_ext_id: Option<u32>,
        guest_seat: u32,
        active_surface: Option<u32>,
    ) -> Self {
        Self {
            host_v1_id,
            host_ext_id,
            guest_seat,
            active_surface,
            pending_enabled: false,
            committed_enabled: false,
            enabled_dirty: false,
            pending_surrounding_text: None,
            committed_surrounding_text: None,
            surrounding_text_dirty: false,
            content_hint: 0,
            content_purpose: 0,
            committed_content_type: None,
            content_type_dirty: false,
            cursor_rect: None,
            cursor_rect_dirty: false,
            text_change_cause: 0,
            current_preedit: String::new(),
            guest_commit_serial: 0,
            pending_preedit_cursor: None,
            pending_preedit_selection: None,
            pending_deletes: Vec::new(),
            pending_cursor_position: None,
            host_activated: false,
        }
    }

    /// Apply a keyboard-focus generation boundary.
    ///
    /// text-input-v3 requires the client to resend enable and editor state
    /// after every enter. The guest commit serial and host activation marker
    /// deliberately survive so the bridge can deactivate the previous host
    /// generation with the correct transaction ordering.
    pub fn apply_focus(&mut self, active_surface: Option<u32>) -> Option<u32> {
        let previous_surface = self.active_surface;
        self.pending_enabled = false;
        self.committed_enabled = false;
        self.enabled_dirty = false;
        self.pending_surrounding_text = None;
        self.committed_surrounding_text = None;
        self.surrounding_text_dirty = false;
        self.content_hint = 0;
        self.content_purpose = 0;
        self.committed_content_type = None;
        self.content_type_dirty = false;
        self.cursor_rect = None;
        self.cursor_rect_dirty = false;
        self.text_change_cause = 0;
        self.clear_host_composition();
        self.active_surface = active_surface;
        previous_surface
    }

    /// Move the object to its final disabled target before host reconciliation.
    pub fn begin_destroy(&mut self) {
        self.committed_enabled = false;
        self.active_surface = None;
    }

    /// Start a new text-input-v3 enable or disable transaction.
    ///
    /// Both requests reset the pending editor state. Committed state remains
    /// untouched until [`Self::finish_guest_commit`] publishes a fully encoded
    /// host transaction.
    pub fn begin_enabled_transaction(&mut self, enabled: bool) {
        self.pending_enabled = enabled;
        self.enabled_dirty = true;
        self.pending_surrounding_text = None;
        self.surrounding_text_dirty = true;
        self.content_hint = 0;
        self.content_purpose = 0;
        self.content_type_dirty = true;
        self.cursor_rect = None;
        self.cursor_rect_dirty = true;
        self.text_change_cause = 0;
    }

    pub fn set_surrounding_text(&mut self, text: String, cursor: i32, anchor: i32) {
        self.pending_surrounding_text = Some((text, cursor, anchor));
        self.surrounding_text_dirty = true;
    }

    pub fn set_text_change_cause(&mut self, cause: u32) {
        self.text_change_cause = cause;
    }

    pub fn set_content_type(&mut self, hint: u32, purpose: u32) {
        self.content_hint = hint;
        self.content_purpose = purpose;
        self.content_type_dirty =
            self.enabled_dirty || self.committed_content_type != Some((hint, purpose));
    }

    pub fn set_cursor_rect(&mut self, rect: (i32, i32, i32, i32)) {
        self.cursor_rect = Some(rect);
        self.cursor_rect_dirty = true;
    }

    /// Snapshot the next guest commit without mutating committed state.
    ///
    /// The handler owns this plan while encoding every host message. Dropping
    /// the plan on an encoding failure leaves the state byte-for-byte
    /// unchanged; only [`Self::finish_guest_commit`] advances the transaction.
    pub(crate) fn prepare_guest_commit(&self, enable_conflict: bool) -> GuestCommitPlan {
        let resets_composition = self.enabled_dirty;
        let enabled = if resets_composition && !enable_conflict {
            self.pending_enabled
        } else {
            self.committed_enabled
        };
        let effective_surrounding_text = if self.surrounding_text_dirty {
            &self.pending_surrounding_text
        } else {
            &self.committed_surrounding_text
        };
        GuestCommitPlan {
            host_v1_id: self.host_v1_id,
            host_ext_id: self.host_ext_id,
            guest_seat: self.guest_seat,
            serial: self.guest_commit_serial.wrapping_add(1),
            enabled,
            enable_conflict,
            resets_composition,
            surrounding_text: self
                .surrounding_text_dirty
                .then(|| self.pending_surrounding_text.clone()),
            has_surrounding_text: effective_surrounding_text.is_some(),
            content_type: self
                .content_type_dirty
                .then_some((self.content_hint, self.content_purpose)),
            cursor_rect: self
                .cursor_rect_dirty
                .then_some(self.cursor_rect.unwrap_or((0, 0, 0, 0))),
        }
    }

    /// Publish a previously encoded guest commit plan.
    pub(crate) fn finish_guest_commit(&mut self, plan: GuestCommitPlan) {
        debug_assert_eq!(plan.host_v1_id, self.host_v1_id);
        debug_assert_eq!(plan.host_ext_id, self.host_ext_id);
        debug_assert_eq!(plan.guest_seat, self.guest_seat);
        debug_assert_eq!(
            plan.serial,
            self.guest_commit_serial.wrapping_add(1),
            "guest commit plans must be finished exactly once and in order"
        );

        self.guest_commit_serial = plan.serial;
        if plan.resets_composition {
            if plan.enable_conflict {
                self.pending_enabled = self.committed_enabled;
            } else {
                self.pending_enabled = plan.enabled;
                self.committed_enabled = plan.enabled;
            }
            self.enabled_dirty = false;
            self.clear_host_composition();
        }
        if let Some(surrounding_text) = plan.surrounding_text {
            self.surrounding_text_dirty = false;
            self.committed_surrounding_text = surrounding_text;
        }
        if let Some(content_type) = plan.content_type {
            self.content_type_dirty = false;
            self.committed_content_type = Some(content_type);
        }
        if plan.cursor_rect.is_some() {
            self.cursor_rect_dirty = false;
        }
        self.text_change_cause = 0;
    }

    pub fn record_preedit_selection(&mut self, index: u32, length: u32) {
        self.pending_preedit_selection = Some((index, length));
    }

    pub fn record_preedit_cursor(&mut self, index: i32) {
        self.pending_preedit_cursor = Some(index);
    }

    pub fn record_cursor_position(&mut self, index: i32, anchor: i32) {
        self.pending_cursor_position = Some((index, anchor));
    }

    pub fn record_delete(&mut self, before: u32, after: u32) -> bool {
        if self.pending_deletes.len() >= MAX_PENDING_IME_DELETES {
            return false;
        }
        self.pending_deletes.push((before, after));
        true
    }

    pub(crate) fn prepare_host_preedit(&self) -> Option<HostPreeditPlan> {
        self.host_activated.then_some(HostPreeditPlan {
            guest_seat: self.guest_seat,
            done_serial: self.guest_commit_serial,
            had_preedit: !self.current_preedit.is_empty(),
            selection: self.pending_preedit_selection,
            cursor: self.pending_preedit_cursor,
        })
    }

    pub(crate) fn finish_host_preedit(
        &mut self,
        plan: HostPreeditPlan,
        text: String,
        reset_commit_is_empty: bool,
        backspace_held: bool,
    ) -> bool {
        debug_assert_eq!(plan.done_serial, self.guest_commit_serial);
        debug_assert_eq!(plan.selection, self.pending_preedit_selection);
        debug_assert_eq!(plan.cursor, self.pending_preedit_cursor);
        debug_assert_eq!(plan.had_preedit, !self.current_preedit.is_empty());

        self.pending_preedit_selection = None;
        self.pending_preedit_cursor = None;
        let arm_backspace_repeat =
            text.is_empty() && plan.had_preedit && reset_commit_is_empty && backspace_held;
        self.current_preedit = text;
        arm_backspace_repeat
    }

    pub(crate) fn prepare_host_commit(&self) -> Option<HostCommitPlan> {
        self.host_activated.then(|| HostCommitPlan {
            guest_seat: self.guest_seat,
            done_serial: self.guest_commit_serial,
            had_preedit: !self.current_preedit.is_empty(),
            deletes: self.pending_deletes.clone(),
            cursor_position: self.pending_cursor_position,
        })
    }

    pub(crate) fn finish_host_commit(&mut self, plan: HostCommitPlan) {
        debug_assert_eq!(plan.done_serial, self.guest_commit_serial);
        debug_assert_eq!(plan.deletes, self.pending_deletes);
        debug_assert_eq!(plan.cursor_position, self.pending_cursor_position);
        debug_assert_eq!(plan.had_preedit, !self.current_preedit.is_empty());
        self.clear_host_composition();
    }

    pub(crate) fn prepare_preedit_region(&self) -> Option<PreeditRegionPlan> {
        if !self.host_activated {
            return None;
        }
        let (surrounding_text, surrounding_cursor, _) = self.committed_surrounding_text.as_ref()?;
        Some(PreeditRegionPlan {
            guest_seat: self.guest_seat,
            surrounding_text: surrounding_text.clone(),
            surrounding_cursor: *surrounding_cursor,
            done_serial: self.guest_commit_serial,
            selection: self.pending_preedit_selection,
            cursor: self.pending_preedit_cursor,
        })
    }

    pub(crate) fn finish_preedit_region(&mut self, plan: PreeditRegionPlan, preedit_text: String) {
        debug_assert_eq!(plan.done_serial, self.guest_commit_serial);
        debug_assert_eq!(plan.selection, self.pending_preedit_selection);
        debug_assert_eq!(plan.cursor, self.pending_preedit_cursor);
        self.pending_preedit_selection = None;
        self.pending_preedit_cursor = None;
        self.current_preedit = preedit_text;
    }

    pub(crate) fn prepare_confirm_preedit(&self) -> Option<ConfirmPreeditPlan> {
        self.host_activated.then(|| ConfirmPreeditPlan {
            guest_seat: self.guest_seat,
            done_serial: self.guest_commit_serial,
            preedit_text: self.current_preedit.clone(),
        })
    }

    /// Close a host composition transaction after its complete guest message
    /// sequence has been encoded.
    pub(crate) fn finish_confirm_preedit(&mut self, plan: ConfirmPreeditPlan) {
        debug_assert_eq!(plan.done_serial, self.guest_commit_serial);
        debug_assert_eq!(plan.preedit_text, self.current_preedit);
        self.current_preedit.clear();
        self.pending_preedit_cursor = None;
        self.pending_preedit_selection = None;
    }

    fn clear_host_composition(&mut self) {
        self.current_preedit.clear();
        self.pending_preedit_cursor = None;
        self.pending_preedit_selection = None;
        self.pending_deletes.clear();
        self.pending_cursor_position = None;
    }
}

/// Provenance for one physical key generation observed through ChromeOS
/// `peek_key`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeekKeyProvenance {
    pub serial: u32,
    pub time: u32,
    pub sequence: u64,
    /// The generation may recover an IME-consumed repeat. Host accelerators
    /// permanently clear this bit for the lifetime of the generation.
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestKeyOwner {
    Physical,
    TextInputKeysym,
    /// A balanced synthetic pair was delivered. This completed-generation
    /// tombstone suppresses delayed duplicate channels and permits repeat
    /// recovery without leaving an open guest press.
    ImeRecovery,
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
    owner: GuestKeyOwner,
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
    /// Serial of the generation's initial peek press. Retained after physical
    /// release while another tombstone keeps the generation alive so the
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
    entries: HashMap<HostId, HashMap<u32, KeyGeneration>>,
    retired_guest_releases: HashMap<(HostId, u32), Vec<RetiredGuestRelease>>,
    latest_peek_sequences: HashMap<(u32, Option<u32>), PeekWatermark>,
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

    pub(crate) fn clear_keyboard(&mut self, keyboard: HostId) {
        self.entries.remove(&keyboard);
        self.retired_guest_releases
            .retain(|(pending_keyboard, _), _| *pending_keyboard != keyboard);
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
        self.take_retired_guest_release(keyboard, key, |release| {
            release.owner == GuestKeyOwner::Physical && release.release_serial == Some(serial)
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
        self.take_retired_guest_release(keyboard, key, |release| {
            release.owner == GuestKeyOwner::TextInputKeysym
                && release
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
        predicate: impl Fn(&RetiredGuestRelease) -> bool,
    ) -> bool {
        let map_key = (keyboard, key);
        let Some(releases) = self.retired_guest_releases.get_mut(&map_key) else {
            return false;
        };
        let Some(index) = releases.iter().position(predicate) else {
            return false;
        };
        releases.remove(index);
        if releases.is_empty() {
            self.retired_guest_releases.remove(&map_key);
        }
        true
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
            Some(GuestKeyOwner::Physical) => {
                generation
                    .physical_release_serial
                    .map(|release_serial| RetiredGuestRelease {
                        owner: GuestKeyOwner::Physical,
                        press_serial: generation.guest_press_serial,
                        release_serial: Some(release_serial),
                        next_press_serial,
                    })
            }
            Some(GuestKeyOwner::TextInputKeysym) => {
                generation
                    .guest_press_serial
                    .map(|press_serial| RetiredGuestRelease {
                        owner: GuestKeyOwner::TextInputKeysym,
                        press_serial: Some(press_serial),
                        release_serial: None,
                        next_press_serial,
                    })
            }
            Some(GuestKeyOwner::ImeRecovery) | None => None,
        };
        if let Some(retired_release) = retired_release {
            self.retired_guest_releases
                .entry((keyboard, key))
                .or_default()
                .push(retired_release);
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
                    Some(GuestKeyOwner::ImeRecovery) => {
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

    pub(crate) fn clear_accelerator_suppressions(&mut self, keyboard: HostId) {
        let keys = self
            .entries
            .get(&keyboard)
            .map(|keys| keys.keys().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        if let Some(generations) = self.entries.get_mut(&keyboard) {
            for generation in generations.values_mut() {
                generation.host_accelerator_suppressed = false;
            }
        }
        for key in keys {
            self.prune_key(keyboard, key);
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
        match event {
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
            GuestKeyEvent::TextInputPress { serial } => {
                let accelerator_generation = self.host_accelerator_suppressed(keyboard, key)
                    || (self.physically_held(keyboard, key)
                        && self
                            .peek(keyboard, key)
                            .is_some_and(|press| !press.eligible));
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
                if matches!(
                    owner,
                    Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
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
    keyboards: HashMap<HostId, KeyboardFocus>,
    /// Focus generations superseded by a different keyboard resource.
    ///
    /// Their seat focus is no longer authoritative, but the guest resource
    /// received an enter and still needs exactly one matching leave. A newer
    /// enter on the same keyboard supersedes its older generation without a
    /// tombstone because forwarding that delayed leave would clear the newer
    /// resource focus.
    retired_guest_enters: HashMap<(HostId, u32), KeyboardFocus>,
}

impl KeyboardFocusRegistry {
    pub fn surface_for_seat(&self, guest_seat: u32) -> Option<u32> {
        self.keyboards
            .values()
            .find(|focus| focus.guest_seat == guest_seat)
            .map(|focus| focus.guest_surface)
    }

    pub fn focus_for_keyboard(&self, host_keyboard: HostId) -> Option<KeyboardFocus> {
        self.keyboards.get(&host_keyboard).copied()
    }

    pub fn keyboard_owns_surface(&self, host_keyboard: HostId, guest_surface: u32) -> bool {
        self.focus_for_keyboard(host_keyboard)
            .is_some_and(|focus| focus.guest_surface == guest_surface)
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
/// Internal `wl_display.sync` barriers that separate host text-input
/// activation generations.
///
/// Host text-input-v1 objects have no destructor and are reused across guest
/// focus changes. A deactivate followed by this barrier proves that every
/// event from the previous activation has been dispatched while
/// `host_activated` is false, before the object may be activated again.
#[derive(Default)]
pub struct TextInputActivationBarrierRegistry {
    by_callback: HashMap<HostId, (u32, u32)>,
    by_text_input_generation: HashMap<(u32, u32), HostId>,
}

impl TextInputActivationBarrierRegistry {
    pub(crate) fn is_pending(&self, guest_text_input: u32, host_v1_id: u32) -> bool {
        self.by_text_input_generation
            .contains_key(&(guest_text_input, host_v1_id))
    }

    pub(crate) fn install(
        &mut self,
        callback: HostId,
        guest_text_input: u32,
        host_v1_id: u32,
    ) -> bool {
        if self.by_callback.contains_key(&callback)
            || self
                .by_text_input_generation
                .contains_key(&(guest_text_input, host_v1_id))
        {
            return false;
        }
        self.by_callback
            .insert(callback, (guest_text_input, host_v1_id));
        self.by_text_input_generation
            .insert((guest_text_input, host_v1_id), callback);
        true
    }

    pub(crate) fn complete(&mut self, callback: HostId) -> Option<(u32, u32)> {
        let generation = self.by_callback.remove(&callback)?;
        if self.by_text_input_generation.get(&generation) == Some(&callback) {
            self.by_text_input_generation.remove(&generation);
        }
        Some(generation)
    }

    #[cfg(test)]
    pub(crate) fn callback_for(&self, guest_text_input: u32, host_v1_id: u32) -> Option<HostId> {
        self.by_text_input_generation
            .get(&(guest_text_input, host_v1_id))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;

    #[test]
    fn text_input_focus_boundary_resets_editor_state_only() {
        let mut state = TextInputState::new(10, Some(11), 12, Some(13));
        state.pending_enabled = true;
        state.committed_enabled = true;
        state.enabled_dirty = true;
        state.pending_surrounding_text = Some(("pending".to_string(), 7, 7));
        state.committed_surrounding_text = Some(("committed".to_string(), 9, 9));
        state.surrounding_text_dirty = true;
        state.content_hint = 5;
        state.content_purpose = 6;
        state.committed_content_type = Some((5, 6));
        state.content_type_dirty = true;
        state.cursor_rect = Some((1, 2, 3, 4));
        state.cursor_rect_dirty = true;
        state.text_change_cause = 1;
        state.current_preedit = "조합".to_string();
        state.guest_commit_serial = 17;
        state.pending_preedit_cursor = Some(3);
        state.pending_preedit_selection = Some((0, 3));
        state.pending_deletes.push((3, 0));
        state.pending_cursor_position = Some((1, 1));
        state.host_activated = true;

        assert_eq!(state.apply_focus(Some(14)), Some(13));

        let mut expected = TextInputState::new(10, Some(11), 12, Some(14));
        expected.guest_commit_serial = 17;
        expected.host_activated = true;
        assert_eq!(state, expected);
    }

    #[test]
    fn preparing_guest_commit_is_non_mutating_until_finish() {
        let mut state = TextInputState::new(10, Some(11), 12, Some(13));
        state.committed_content_type = Some((1, 2));
        state.begin_enabled_transaction(true);
        state.set_surrounding_text("가나다".to_string(), 9, 9);
        state.set_content_type(3, 4);
        state.set_cursor_rect((1, 2, 3, 4));
        state.set_text_change_cause(1);
        let before = state.clone();

        let plan = state.prepare_guest_commit(false);

        assert_eq!(state, before);
        assert_eq!(plan.serial, 1);
        assert!(plan.enabled);
        assert_eq!(
            plan.surrounding_text,
            Some(Some(("가나다".to_string(), 9, 9)))
        );
        assert_eq!(plan.content_type, Some((3, 4)));
        assert_eq!(plan.cursor_rect, Some((1, 2, 3, 4)));
    }

    #[test]
    fn finishing_guest_commit_applies_exact_owned_snapshot() {
        let mut state = TextInputState::new(10, Some(11), 12, Some(13));
        state.current_preedit = "이전".to_string();
        state.pending_preedit_cursor = Some(3);
        state.pending_deletes.push((3, 0));
        state.begin_enabled_transaction(true);
        state.set_surrounding_text("가나다".to_string(), 9, 9);
        state.set_content_type(3, 4);
        state.set_cursor_rect((1, 2, 3, 4));
        state.set_text_change_cause(1);

        let plan = state.prepare_guest_commit(false);
        state.finish_guest_commit(plan);

        assert_eq!(state.guest_commit_serial, 1);
        assert!(state.pending_enabled);
        assert!(state.committed_enabled);
        assert!(!state.enabled_dirty);
        assert_eq!(
            state.committed_surrounding_text,
            Some(("가나다".to_string(), 9, 9))
        );
        assert!(!state.surrounding_text_dirty);
        assert_eq!(state.committed_content_type, Some((3, 4)));
        assert!(!state.content_type_dirty);
        assert!(!state.cursor_rect_dirty);
        assert_eq!(state.text_change_cause, 0);
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_preedit_cursor.is_none());
        assert!(state.pending_deletes.is_empty());
    }

    #[test]
    fn enable_replays_same_content_type_but_ordinary_repeat_is_clean() {
        let mut state = TextInputState::new(10, Some(11), 12, Some(13));
        state.committed_content_type = Some((3, 4));

        state.set_content_type(3, 4);
        assert_eq!(state.prepare_guest_commit(false).content_type, None);

        state.begin_enabled_transaction(true);
        state.set_content_type(3, 4);
        assert_eq!(state.prepare_guest_commit(false).content_type, Some((3, 4)));
    }

    #[test]
    fn rejected_enable_advances_serial_and_closes_composition() {
        let mut state = TextInputState::new(10, Some(11), 12, Some(13));
        state.current_preedit = "가".to_string();
        state.pending_deletes.push((3, 0));
        state.begin_enabled_transaction(true);

        let plan = state.prepare_guest_commit(true);
        assert!(!plan.enabled);
        state.finish_guest_commit(plan);

        assert_eq!(state.guest_commit_serial, 1);
        assert!(!state.pending_enabled);
        assert!(!state.committed_enabled);
        assert!(!state.enabled_dirty);
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_deletes.is_empty());
    }

    #[test]
    fn guest_commit_serial_wraps_to_zero() {
        let mut state = TextInputState::new(10, None, 12, Some(13));
        state.guest_commit_serial = u32::MAX;

        let plan = state.prepare_guest_commit(false);
        assert_eq!(plan.serial, 0);
        state.finish_guest_commit(plan);
        assert_eq!(state.guest_commit_serial, 0);
    }

    #[test]
    fn host_preedit_metadata_is_consumed_only_after_finish() {
        let mut state = TextInputState::new(10, None, 12, Some(13));
        assert!(state.prepare_host_preedit().is_none());
        state.host_activated = true;
        state.guest_commit_serial = 7;
        state.current_preedit = "가".to_string();
        state.record_preedit_selection(0, 3);
        state.record_preedit_cursor(3);
        let before = state.clone();

        let plan = state.prepare_host_preedit().expect("active host input");
        assert_eq!(state, before);
        assert_eq!(plan.done_serial, 7);
        assert!(plan.had_preedit);
        assert_eq!(plan.selection, Some((0, 3)));
        assert_eq!(plan.cursor, Some(3));

        state.finish_host_preedit(plan, "각".to_string(), true, true);
        assert_eq!(state.current_preedit, "각");
        assert!(state.pending_preedit_selection.is_none());
        assert!(state.pending_preedit_cursor.is_none());
    }

    #[test]
    fn host_commit_edits_are_atomic_and_consumed_exactly_once() {
        let mut state = TextInputState::new(10, None, 12, Some(13));
        state.host_activated = true;
        state.guest_commit_serial = 7;
        state.current_preedit = "가".to_string();
        state.record_preedit_selection(0, 3);
        state.record_preedit_cursor(3);
        assert!(state.record_delete(3, 0));
        state.record_cursor_position(1, 1);
        let before = state.clone();

        let plan = state.prepare_host_commit().expect("active host input");
        assert_eq!(
            state, before,
            "preparing an event must not consume metadata"
        );
        assert_eq!(plan.deletes, vec![(3, 0)]);
        assert_eq!(plan.cursor_position, Some((1, 1)));

        state.finish_host_commit(plan);
        assert!(state.current_preedit.is_empty());
        assert!(state.pending_preedit_selection.is_none());
        assert!(state.pending_preedit_cursor.is_none());
        assert!(state.pending_deletes.is_empty());
        assert!(state.pending_cursor_position.is_none());

        let next = state.prepare_host_commit().expect("still active");
        assert!(!next.had_preedit);
        assert!(next.deletes.is_empty());
        assert!(next.cursor_position.is_none());
    }

    #[test]
    fn confirming_preedit_closes_only_preedit_metadata() {
        let mut state = TextInputState::new(10, None, 12, Some(13));
        state.host_activated = true;
        state.current_preedit = "가".to_string();
        state.record_preedit_selection(0, 3);
        state.record_preedit_cursor(3);
        assert!(state.record_delete(3, 0));
        state.record_cursor_position(1, 1);

        let plan = state.prepare_confirm_preedit().expect("active host input");
        state.finish_confirm_preedit(plan);

        let next_preedit = state.prepare_host_preedit().expect("still active");
        assert!(!next_preedit.had_preedit);
        assert!(next_preedit.selection.is_none());
        assert!(next_preedit.cursor.is_none());
        let next_commit = state.prepare_host_commit().expect("still active");
        assert_eq!(next_commit.deletes, vec![(3, 0)]);
        assert_eq!(next_commit.cursor_position, Some((1, 1)));
    }

    #[test]
    fn pending_host_deletes_are_bounded_and_next_commit_still_completes() {
        let mut state = TextInputState::new(10, None, 12, Some(13));
        state.host_activated = true;
        for index in 0..MAX_PENDING_IME_DELETES {
            assert!(state.record_delete(index as u32, 0));
        }
        assert!(!state.record_delete(u32::MAX, u32::MAX));

        let plan = state.prepare_host_commit().expect("active host input");
        assert_eq!(plan.deletes.len(), MAX_PENDING_IME_DELETES);
        state.finish_host_commit(plan);
        assert!(state.pending_deletes.is_empty());
    }

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

    #[test]
    fn text_input_lifecycle_short_traces_preserve_commit_atomicity() {
        #[derive(Clone, Copy)]
        enum Step {
            Focus,
            Leave,
            Enable,
            Disable,
            Surrounding,
            ContentType,
            CursorRect,
            Commit,
            FailedCommit,
            Destroy,
        }

        const STEPS: [Step; 10] = [
            Step::Focus,
            Step::Leave,
            Step::Enable,
            Step::Disable,
            Step::Surrounding,
            Step::ContentType,
            Step::CursorRect,
            Step::Commit,
            Step::FailedCommit,
            Step::Destroy,
        ];

        fn walk(state: TextInputState, depth: usize) {
            if depth == 0 {
                return;
            }
            for step in STEPS {
                let mut next = state.clone();
                match step {
                    Step::Focus | Step::Leave => {
                        let serial = next.guest_commit_serial;
                        let host_activated = next.host_activated;
                        let surface = matches!(step, Step::Focus).then_some(13);
                        next.apply_focus(surface);
                        let mut expected = TextInputState::new(10, Some(11), 12, surface);
                        expected.guest_commit_serial = serial;
                        expected.host_activated = host_activated;
                        assert_eq!(next, expected);
                    }
                    Step::Enable | Step::Disable if next.active_surface.is_some() => {
                        next.begin_enabled_transaction(matches!(step, Step::Enable));
                        assert!(next.enabled_dirty);
                        assert!(next.surrounding_text_dirty);
                        assert!(next.content_type_dirty);
                        assert!(next.cursor_rect_dirty);
                    }
                    Step::Surrounding if next.active_surface.is_some() => {
                        next.set_surrounding_text("가".to_string(), 3, 3);
                        assert!(next.surrounding_text_dirty);
                    }
                    Step::ContentType if next.active_surface.is_some() => {
                        next.set_content_type(3, 4);
                    }
                    Step::CursorRect if next.active_surface.is_some() => {
                        next.set_cursor_rect((1, 2, 3, 4));
                        assert!(next.cursor_rect_dirty);
                    }
                    Step::Commit if next.active_surface.is_some() => {
                        let previous_serial = next.guest_commit_serial;
                        let plan = next.prepare_guest_commit(false);
                        let resets_composition = plan.resets_composition;
                        let commits_surrounding = plan.surrounding_text.is_some();
                        let commits_content_type = plan.content_type.is_some();
                        let commits_cursor_rect = plan.cursor_rect.is_some();
                        next.finish_guest_commit(plan);
                        assert_eq!(next.guest_commit_serial, previous_serial.wrapping_add(1));
                        if resets_composition {
                            assert!(!next.enabled_dirty);
                            assert!(next.current_preedit.is_empty());
                            assert!(next.pending_deletes.is_empty());
                        }
                        if commits_surrounding {
                            assert!(!next.surrounding_text_dirty);
                        }
                        if commits_content_type {
                            assert!(!next.content_type_dirty);
                        }
                        if commits_cursor_rect {
                            assert!(!next.cursor_rect_dirty);
                        }
                    }
                    Step::FailedCommit if next.active_surface.is_some() => {
                        let before = next.clone();
                        let _unencodable_plan = next.prepare_guest_commit(false);
                        assert_eq!(
                            next, before,
                            "dropping an unencoded plan must not advance any state"
                        );
                    }
                    Step::Destroy => {
                        let serial = next.guest_commit_serial;
                        let host_activated = next.host_activated;
                        next.begin_destroy();
                        assert_eq!(next.guest_commit_serial, serial);
                        assert_eq!(next.host_activated, host_activated);
                        assert!(!next.committed_enabled);
                        assert!(next.active_surface.is_none());
                    }
                    _ => {}
                }
                walk(next, depth - 1);
            }
        }

        let mut initial = TextInputState::new(10, Some(11), 12, Some(13));
        initial.host_activated = true;
        walk(initial, 4);
    }

    #[test]
    fn keyboard_focus_registry_balances_replacement_and_delayed_leave() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };
        let mut registry = KeyboardFocusRegistry::default();

        assert_eq!(
            registry.enter(keyboard_a, focus_a),
            KeyboardFocusUpdate {
                accepted: true,
                seat_changes: vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: None,
                    current_surface: Some(20),
                }],
                retired_keyboards: Vec::new(),
            }
        );
        assert_eq!(
            registry.enter(keyboard_b, focus_b),
            KeyboardFocusUpdate {
                accepted: true,
                seat_changes: vec![SeatFocusChange {
                    guest_seat: 1,
                    previous_surface: Some(20),
                    current_surface: Some(21),
                }],
                retired_keyboards: vec![keyboard_a],
            }
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
        assert_eq!(
            registry.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate {
                accepted: true,
                ..KeyboardFocusUpdate::default()
            },
            "a delayed leave must balance the retired guest enter once"
        );
        assert_eq!(
            registry.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a duplicate delayed leave must be rejected"
        );
        assert_eq!(registry.surface_for_seat(1), Some(21));
    }

    #[test]
    fn keyboard_focus_registry_rejects_old_leave_after_same_keyboard_reenter() {
        let keyboard = HostId(10);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };
        let mut registry = KeyboardFocusRegistry::default();

        registry.enter(keyboard, focus_a);
        registry.enter(keyboard, focus_b);
        assert_eq!(
            registry.leave(keyboard, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "an old leave on the same resource must not clear its newer enter"
        );
        assert_eq!(registry.focus_for_keyboard(keyboard), Some(focus_b));
    }

    #[test]
    fn keyboard_focus_registry_release_and_destroy_discard_retired_enters() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus_a = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let focus_b = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 21,
            host_surface: 201,
        };

        let mut released = KeyboardFocusRegistry::default();
        released.enter(keyboard_a, focus_a);
        released.enter(keyboard_b, focus_b);
        released.release(keyboard_a);
        assert_eq!(
            released.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a released keyboard resource cannot receive a delayed leave"
        );

        let mut destroyed = KeyboardFocusRegistry::default();
        destroyed.enter(keyboard_a, focus_a);
        destroyed.enter(keyboard_b, focus_b);
        destroyed.destroy_surface(focus_a.guest_surface);
        assert_eq!(
            destroyed.leave(keyboard_a, focus_a.host_surface),
            KeyboardFocusUpdate::default(),
            "a destroyed guest surface cannot receive a delayed leave"
        );
    }

    #[test]
    fn keyboard_focus_registry_keeps_shared_surface_until_last_owner() {
        let keyboard_a = HostId(10);
        let keyboard_b = HostId(11);
        let focus = KeyboardFocus {
            guest_seat: 1,
            guest_surface: 20,
            host_surface: 200,
        };
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(keyboard_a, focus);
        registry.enter(keyboard_b, focus);

        let first_leave = registry.leave(keyboard_a, focus.host_surface);
        assert!(first_leave.accepted);
        assert!(first_leave.seat_changes.is_empty());
        assert_eq!(registry.surface_for_seat(1), Some(20));

        let last_leave = registry.leave(keyboard_b, focus.host_surface);
        assert_eq!(
            last_leave.seat_changes,
            vec![SeatFocusChange {
                guest_seat: 1,
                previous_surface: Some(20),
                current_surface: None,
            }]
        );
        assert_eq!(registry.surface_for_seat(1), None);
    }

    #[test]
    fn keyboard_focus_registry_destroys_surface_across_seats_without_fallback() {
        let mut registry = KeyboardFocusRegistry::default();
        registry.enter(
            HostId(10),
            KeyboardFocus {
                guest_seat: 1,
                guest_surface: 20,
                host_surface: 200,
            },
        );
        registry.enter(
            HostId(11),
            KeyboardFocus {
                guest_seat: 2,
                guest_surface: 20,
                host_surface: 200,
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

        let update = registry.destroy_surface(20);
        assert_eq!(update.retired_keyboards, vec![HostId(10), HostId(11)]);
        assert_eq!(
            update.seat_changes,
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
            ]
        );
        assert_eq!(registry.surface_for_seat(1), None);
        assert_eq!(registry.surface_for_seat(2), None);
        assert_eq!(registry.surface_for_seat(3), Some(30));
    }

    #[test]
    fn keyboard_focus_registry_preserves_invariants_for_all_short_sequences() {
        #[derive(Clone, Copy)]
        enum Operation {
            EnterA20,
            EnterA21,
            EnterB20,
            EnterB21,
            LeaveA20,
            LeaveA21,
            LeaveB20,
            LeaveB21,
            ReleaseA,
            ReleaseB,
            Destroy20,
            Destroy21,
        }

        #[derive(Clone, Copy)]
        enum GuestDelivery {
            Enter(HostId, KeyboardFocus),
            Leave(HostId, u32),
            Release(HostId),
            Destroy(u32),
        }

        let operations = [
            Operation::EnterA20,
            Operation::EnterA21,
            Operation::EnterB20,
            Operation::EnterB21,
            Operation::LeaveA20,
            Operation::LeaveA21,
            Operation::LeaveB20,
            Operation::LeaveB21,
            Operation::ReleaseA,
            Operation::ReleaseB,
            Operation::Destroy20,
            Operation::Destroy21,
        ];
        let sequence_len = 5;
        let sequence_count = operations.len().pow(sequence_len);

        for mut encoded in 0..sequence_count {
            let mut registry = KeyboardFocusRegistry::default();
            // Model the focus currently visible to each guest wl_keyboard
            // resource. A newer enter on the same resource supersedes its
            // previous surface; active and retired registry generations must
            // account for this model exactly.
            let mut guest_focus = HashMap::new();
            for _ in 0..sequence_len {
                let (update, delivery) = match operations[encoded % operations.len()] {
                    Operation::EnterA20 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 20,
                            host_surface: 200,
                        };
                        (
                            registry.enter(HostId(10), focus),
                            GuestDelivery::Enter(HostId(10), focus),
                        )
                    }
                    Operation::EnterA21 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 21,
                            host_surface: 201,
                        };
                        (
                            registry.enter(HostId(10), focus),
                            GuestDelivery::Enter(HostId(10), focus),
                        )
                    }
                    Operation::EnterB20 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 20,
                            host_surface: 200,
                        };
                        (
                            registry.enter(HostId(11), focus),
                            GuestDelivery::Enter(HostId(11), focus),
                        )
                    }
                    Operation::EnterB21 => {
                        let focus = KeyboardFocus {
                            guest_seat: 1,
                            guest_surface: 21,
                            host_surface: 201,
                        };
                        (
                            registry.enter(HostId(11), focus),
                            GuestDelivery::Enter(HostId(11), focus),
                        )
                    }
                    Operation::LeaveA20 => (
                        registry.leave(HostId(10), 200),
                        GuestDelivery::Leave(HostId(10), 200),
                    ),
                    Operation::LeaveA21 => (
                        registry.leave(HostId(10), 201),
                        GuestDelivery::Leave(HostId(10), 201),
                    ),
                    Operation::LeaveB20 => (
                        registry.leave(HostId(11), 200),
                        GuestDelivery::Leave(HostId(11), 200),
                    ),
                    Operation::LeaveB21 => (
                        registry.leave(HostId(11), 201),
                        GuestDelivery::Leave(HostId(11), 201),
                    ),
                    Operation::ReleaseA => (
                        registry.release(HostId(10)),
                        GuestDelivery::Release(HostId(10)),
                    ),
                    Operation::ReleaseB => (
                        registry.release(HostId(11)),
                        GuestDelivery::Release(HostId(11)),
                    ),
                    Operation::Destroy20 => {
                        (registry.destroy_surface(20), GuestDelivery::Destroy(20))
                    }
                    Operation::Destroy21 => {
                        (registry.destroy_surface(21), GuestDelivery::Destroy(21))
                    }
                };
                encoded /= operations.len();

                match delivery {
                    GuestDelivery::Enter(keyboard, focus) => {
                        if update.accepted {
                            guest_focus.insert(keyboard, focus);
                        }
                    }
                    GuestDelivery::Leave(keyboard, host_surface) => {
                        if update.accepted {
                            assert_eq!(
                                guest_focus.get(&keyboard).map(|focus| focus.host_surface),
                                Some(host_surface),
                                "only a guest-visible enter can accept a leave"
                            );
                            guest_focus.remove(&keyboard);
                        }
                    }
                    GuestDelivery::Release(keyboard) => {
                        guest_focus.remove(&keyboard);
                    }
                    GuestDelivery::Destroy(surface) => {
                        guest_focus.retain(|_, focus| focus.guest_surface != surface);
                    }
                }

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
    fn guest_key_owner_is_exclusive_and_source_checked() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        let keyboard = HostId(10);
        let key = 57;

        assert!(ctx.claim_guest_key(keyboard, key, GuestKeyOwner::Physical));
        for competing_owner in [GuestKeyOwner::TextInputKeysym, GuestKeyOwner::ImeRecovery] {
            assert!(
                !ctx.claim_guest_key(keyboard, key, competing_owner),
                "a second source must not overwrite the live owner"
            );
        }
        assert_eq!(
            ctx.guest_key_owner(keyboard, key),
            Some(GuestKeyOwner::Physical)
        );
        assert!(!ctx.take_guest_key_if(keyboard, key, GuestKeyOwner::TextInputKeysym));
        assert_eq!(
            ctx.guest_key_owner(keyboard, key),
            Some(GuestKeyOwner::Physical),
            "a mismatched release must not steal another source's key"
        );
        assert!(ctx.take_guest_key_if(keyboard, key, GuestKeyOwner::Physical));
        assert!(ctx.guest_key_owner(keyboard, key).is_none());
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
            Operation::Take(GuestKeyOwner::Physical),
            Operation::Take(GuestKeyOwner::TextInputKeysym),
            Operation::Take(GuestKeyOwner::ImeRecovery),
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
    fn guest_key_reducer_couples_delivery_ack_and_generation_ownership() {
        let keyboard = HostId(10);
        let physical_key = 30;
        let accelerator_key = 31;
        let keysym_key = 32;
        let mut registry = KeyGenerationRegistry::default();

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: false,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::TextInputPress { serial: 1 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: None,
                ends_repeat: false,
            },
            "a second protocol channel cannot duplicate the physical press"
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: true,
                    host_accelerator: false,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(keyboard, physical_key, GuestKeyEvent::PhysicalRelease,),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: Some(true),
                ends_repeat: true,
            }
        );

        assert_eq!(
            registry.transition_guest_key(keyboard, physical_key, GuestKeyEvent::RecoverIme,),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::EmitBalancedPair,
                ack_handled: None,
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                physical_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: false,
                    ime_repeat_active: true,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            },
            "delayed physical delivery cannot duplicate a recovered pair"
        );

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                accelerator_key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: true,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                accelerator_key,
                GuestKeyEvent::PhysicalRelease,
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: true,
            },
            "an accelerator release cannot escape without a guest press"
        );

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                keysym_key,
                GuestKeyEvent::TextInputPress { serial: 10 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: None,
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(keyboard, keysym_key, GuestKeyEvent::TextInputRepeat,)
                .delivery,
            GuestKeyDelivery::Forward
        );
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                keysym_key,
                GuestKeyEvent::TextInputRelease { serial: 11 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Forward,
                ack_handled: None,
                ends_repeat: true,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(
                    keyboard,
                    keysym_key,
                    GuestKeyEvent::TextInputRelease { serial: 12 },
                )
                .delivery,
            GuestKeyDelivery::Drop,
            "each synthetic press has exactly one releasable owner"
        );
    }

    #[test]
    fn guest_key_reducer_prevents_keysym_from_bypassing_accelerator_generation() {
        let keyboard = HostId(10);
        let key = 57;
        let mut registry = KeyGenerationRegistry::default();

        registry.observe_peek_press(keyboard, key, 10, 100, false);
        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                key,
                GuestKeyEvent::TextInputPress { serial: 10 },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: None,
                ends_repeat: false,
            },
            "an ineligible held peek generation is already known to be a host accelerator"
        );
        assert_eq!(registry.guest_owner(keyboard, key), None);

        assert_eq!(
            registry.transition_guest_key(
                keyboard,
                key,
                GuestKeyEvent::PhysicalPress {
                    repeated: false,
                    host_accelerator: true,
                    ime_repeat_active: false,
                },
            ),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: false,
            }
        );
        assert_eq!(
            registry
                .transition_guest_key(keyboard, key, GuestKeyEvent::TextInputPress { serial: 11 },)
                .delivery,
            GuestKeyDelivery::Drop,
            "the text-input channel must also honor explicit accelerator suppression"
        );
        registry.observe_peek_release(keyboard, key, 12);
        assert_eq!(
            registry.transition_guest_key(keyboard, key, GuestKeyEvent::PhysicalRelease),
            GuestKeyDecision {
                delivery: GuestKeyDelivery::Drop,
                ack_handled: Some(false),
                ends_repeat: true,
            }
        );
        assert_eq!(registry.guest_owner(keyboard, key), None);
        assert_eq!(
            registry
                .transition_guest_key(keyboard, key, GuestKeyEvent::TextInputPress { serial: 13 },)
                .delivery,
            GuestKeyDelivery::Forward,
            "accelerator suppression must not leak into the next released generation"
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
        }

        let operations = [
            Operation::PhysicalPress,
            Operation::PhysicalRepeat,
            Operation::PhysicalRelease,
            Operation::TextPress,
            Operation::TextRepeat,
            Operation::TextRelease,
            Operation::Recover,
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
                    Operation::PhysicalPress => GuestKeyEvent::PhysicalPress {
                        repeated: false,
                        host_accelerator: false,
                        ime_repeat_active: false,
                    },
                    Operation::PhysicalRepeat => GuestKeyEvent::PhysicalPress {
                        repeated: true,
                        host_accelerator: false,
                        ime_repeat_active: false,
                    },
                    Operation::PhysicalRelease => GuestKeyEvent::PhysicalRelease,
                    Operation::TextPress => GuestKeyEvent::TextInputPress { serial },
                    Operation::TextRepeat => GuestKeyEvent::TextInputRepeat,
                    Operation::TextRelease => GuestKeyEvent::TextInputRelease { serial },
                    Operation::Recover => GuestKeyEvent::RecoverIme,
                };
                let owner_before = registry.guest_owner(keyboard, key);
                let decision = registry.transition_guest_key(keyboard, key, event);

                assert_eq!(
                    decision.ack_handled,
                    match operation {
                        Operation::PhysicalPress => {
                            Some(owner_before != Some(GuestKeyOwner::ImeRecovery))
                        }
                        Operation::PhysicalRepeat | Operation::PhysicalRelease => Some(matches!(
                            owner_before,
                            Some(GuestKeyOwner::Physical | GuestKeyOwner::TextInputKeysym)
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
                        Operation::PhysicalRelease => true,
                        Operation::TextRelease => {
                            owner_before == Some(GuestKeyOwner::TextInputKeysym)
                        }
                        Operation::PhysicalPress
                        | Operation::PhysicalRepeat
                        | Operation::TextPress
                        | Operation::TextRepeat
                        | Operation::Recover => false,
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
                        Operation::PhysicalRelease | Operation::TextRelease,
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
            keys: HashMap<u32, ModelGeneration>,
            retired_guest_releases: HashMap<u32, Vec<RetiredGuestRelease>>,
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
                    Some(GuestKeyOwner::Physical) => {
                        generation.physical_release_serial.map(|release_serial| {
                            RetiredGuestRelease {
                                owner: GuestKeyOwner::Physical,
                                press_serial: generation.guest_press_serial,
                                release_serial: Some(release_serial),
                                next_press_serial,
                            }
                        })
                    }
                    Some(GuestKeyOwner::TextInputKeysym) => {
                        generation
                            .guest_press_serial
                            .map(|press_serial| RetiredGuestRelease {
                                owner: GuestKeyOwner::TextInputKeysym,
                                press_serial: Some(press_serial),
                                release_serial: None,
                                next_press_serial,
                            })
                    }
                    Some(GuestKeyOwner::ImeRecovery) | None => None,
                };
                if let Some(retired_release) = retired_release {
                    self.retired_guest_releases
                        .entry(key)
                        .or_default()
                        .push(retired_release);
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
                                    Some(GuestKeyOwner::ImeRecovery) => {
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
                        let releases = model.retired_guest_releases.entry(key).or_default();
                        let pending = releases.iter().position(|release| {
                            release.owner == GuestKeyOwner::TextInputKeysym
                                && release.press_serial.is_some_and(|press_serial| {
                                    serial_is_after(serial, press_serial)
                                })
                                && release
                                    .next_press_serial
                                    .or(current_press_serial)
                                    .is_none_or(|current_serial| {
                                        !serial_is_after(serial, current_serial)
                                    })
                        });
                        let expected = pending.is_some();
                        if let Some(index) = pending {
                            releases.remove(index);
                        }
                        if releases.is_empty() {
                            model.retired_guest_releases.remove(&key);
                        }
                        assert_eq!(actual, expected);
                    }
                    Operation::TakePendingPhysicalRelease(key) => {
                        let release_serial = serial.wrapping_sub(2);
                        let actual =
                            registry.take_pending_physical_release(keyboard, key, release_serial);
                        let releases = model.retired_guest_releases.entry(key).or_default();
                        let pending = releases.iter().position(|release| {
                            release.owner == GuestKeyOwner::Physical
                                && release.release_serial == Some(release_serial)
                        });
                        let expected = pending.is_some();
                        if let Some(index) = pending {
                            releases.remove(index);
                        }
                        if releases.is_empty() {
                            model.retired_guest_releases.remove(&key);
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
                    actual.map(HashMap::len).unwrap_or_default(),
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
                        .filter(|((pending_keyboard, _), _)| *pending_keyboard == keyboard)
                        .map(|((_, key), releases)| (*key, releases.clone()))
                        .collect::<HashMap<_, _>>(),
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

    #[test]
    fn delayed_keyboard_press_preserves_keysym_release_owner() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        assert!(registry.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym));
        registry.observe_physical_state(keyboard, key, 1);

        assert!(registry.physically_held(keyboard, key));
        assert_eq!(
            registry.guest_owner(keyboard, key),
            Some(GuestKeyOwner::TextInputKeysym),
            "the duplicate keyboard channel must not orphan the synthetic press"
        );
    }

    #[test]
    fn delayed_peek_press_preserves_keysym_release_owner() {
        let keyboard = HostId(10);
        let key = 30;
        let mut registry = KeyGenerationRegistry::default();

        assert!(registry.claim_guest_owner(keyboard, key, GuestKeyOwner::TextInputKeysym));
        registry.observe_peek_press(keyboard, key, 1, 10, true);

        assert!(registry.physically_held(keyboard, key));
        assert_eq!(
            registry.guest_owner(keyboard, key),
            Some(GuestKeyOwner::TextInputKeysym),
            "a delayed peek must not orphan the synthetic press"
        );
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
            registry.take_pending_text_input_release(keyboard, text_key, 11),
            "an older delayed release must remain available after a newer interval closes"
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
