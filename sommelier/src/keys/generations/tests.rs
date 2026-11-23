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

use super::*;

impl KeyGenerationRegistry {
    /// Observe one physical state notification from either keyboard channel.
    ///
    /// Both channels describe the same hardware generation, so a release from
    /// either one closes physical state. Guest-delivery ownership remains until
    /// its corresponding channel consumes the release.
    fn observe_physical_state(&mut self, keyboard: HostId, key: u32, state: KeyState) {
        self.observe_physical_event(keyboard, key, state, None);
    }
}

#[test]
fn ime_repeat_lease_is_generation_and_keyboard_scoped() {
    let keyboard_a = HostId::for_test(10);
    let keyboard_b = HostId::for_test(11);
    let seat = [keyboard_a, keyboard_b];
    let key = EVDEV_KEY_BACKSPACE;
    let (ti, other) = (TiId(GuestId::for_test(40)), TiId(GuestId::for_test(41)));
    let arm = |ti| LeaseInput::Ime {
        ti,
        outcome: LeaseOutcome::ArmIfBackspaceHeld,
    };
    let end = |ti| LeaseInput::Ime {
        ti,
        outcome: LeaseOutcome::End,
    };
    let mut registry = KeyGenerationRegistry::default();
    registry.observe_physical_state(keyboard_a, key, KeyState::Pressed);
    registry.observe_physical_state(keyboard_b, key, KeyState::Pressed);

    // The seat's first keyboard that holds Backspace takes the lease.
    registry.lease(&seat, arm(ti));
    assert_eq!(registry.backspace_lease(keyboard_a), Some(ti));
    assert_eq!(registry.backspace_lease(keyboard_b), None);

    // A release ends it with the generation.
    registry.observe_physical_state(keyboard_a, key, KeyState::Released);
    assert_eq!(registry.backspace_lease(keyboard_a), None);
    registry.lease(&seat, arm(other));
    assert_eq!(registry.backspace_lease(keyboard_b), Some(other));

    // Only the text input that holds it ends it, for the rest of the
    // hold.
    registry.lease(&seat, end(ti));
    assert_eq!(registry.backspace_lease(keyboard_b), Some(other));
    registry.lease(&seat, end(other));
    assert_eq!(registry.backspace_lease(keyboard_b), None);
    assert!(registry.backspace_repeat_cancelled(keyboard_b));
    registry.lease(&seat, arm(other));
    assert_eq!(registry.backspace_lease(keyboard_b), None);
}

const KEYBOARD_A: HostId = HostId::for_test(10);
const KEYBOARD_B: HostId = HostId::for_test(11);

const PRESS: PhysicalKey = PhysicalKey::Press {
    chord: ChordClass::Guest,
    backspace_leased: false,
};
const ACCELERATOR_PRESS: PhysicalKey = PhysicalKey::Press {
    chord: ChordClass::HostAccelerator,
    backspace_leased: false,
};
const SHORTCUT_PRESS: PhysicalKey = PhysicalKey::Press {
    chord: ChordClass::Shortcut,
    backspace_leased: false,
};

/// Give `key` of keyboard A each event of `steps` in turn, and check
/// each verdict.
fn assert_verdicts(
    registry: &mut KeyGenerationRegistry,
    key: u32,
    steps: &[(PhysicalKey, KeyVerdict)],
) {
    for &(event, expected) in steps {
        assert_eq!(
            registry.transition_physical(KEYBOARD_A, key, event),
            expected,
            "{event:?} of key {key}"
        );
    }
}

/// A registry whose shortcut consumed the held `key`.
fn shortcut_consumed(key: u32) -> KeyGenerationRegistry {
    let mut registry = KeyGenerationRegistry::default();
    registry.observe_physical_state(KEYBOARD_A, key, KeyState::Pressed);
    assert_verdicts(
        &mut registry,
        key,
        &[(SHORTCUT_PRESS, KeyVerdict::Shortcut)],
    );
    registry
}

/// A shortcut claims only a generation no channel owns: a press of its
/// chord on a generation another channel owns is that channel's
/// duplicate press, and its release that channel's release.
#[test]
fn a_shortcut_press_of_another_channels_key_is_that_channels() {
    let key = 16;
    for owner in [GuestKeyOwner::Physical, GuestKeyOwner::ImeRecovery] {
        // `reference` gets a guest press instead of the chord.
        let owned = || {
            let mut registry = KeyGenerationRegistry::default();
            registry.observe_physical_state(KEYBOARD_A, key, KeyState::Pressed);
            assert!(registry.claim_guest_owner(KEYBOARD_A, key, owner));
            registry
        };
        let (mut registry, mut reference) = (owned(), owned());
        let verdict = registry.transition_physical(KEYBOARD_A, key, SHORTCUT_PRESS);
        assert_eq!(
            verdict,
            reference.transition_physical(KEYBOARD_A, key, PRESS),
            "{owner:?}"
        );
        let duplicate = match owner {
            GuestKeyOwner::ImeRecovery => KeyVerdict::Decline,
            _ => KeyVerdict::Consume,
        };
        assert_eq!(verdict, duplicate, "{owner:?}");
        assert_eq!(registry.guest_owner(KEYBOARD_A, key), Some(owner));
    }
}

#[test]
fn physical_events_of_a_consumed_shortcut_stay_with_the_shortcut() {
    let key = 16;
    let mut registry = shortcut_consumed(key);
    assert_verdicts(&mut registry, key, &[(PRESS, KeyVerdict::Consume)]);
    assert_eq!(
        registry.guest_owner(KEYBOARD_A, key),
        Some(GuestKeyOwner::Shortcut)
    );
    assert_verdicts(
        &mut registry,
        key,
        &[(PhysicalKey::Release, KeyVerdict::Consume)],
    );
    assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);
}

#[test]
fn ime_recovery_does_not_replay_a_compositor_shortcut_key() {
    let key = 16;
    let mut registry = shortcut_consumed(key);

    // A consumed shortcut key must not reach the guest as a recovered
    // pair, and recovery must leave its ownership for the release.
    assert!(!registry.recover(KEYBOARD_A, key));
    assert_eq!(
        registry.guest_owner(KEYBOARD_A, key),
        Some(GuestKeyOwner::Shortcut)
    );
    assert_verdicts(
        &mut registry,
        key,
        &[(PhysicalKey::Release, KeyVerdict::Consume)],
    );
    assert_eq!(registry.guest_owner(KEYBOARD_A, key), None);
}

#[test]
fn guest_key_reducer_couples_delivery_ack_and_generation_ownership() {
    let mut registry = KeyGenerationRegistry::default();

    let physical_key = 30;
    assert_verdicts(
        &mut registry,
        physical_key,
        &[
            (PRESS, KeyVerdict::Forward),
            (PhysicalKey::Release, KeyVerdict::Forward),
        ],
    );
    assert!(registry.recover(KEYBOARD_A, physical_key));
    // A delayed physical press cannot duplicate the recovered pair.
    let leased = PhysicalKey::Press {
        chord: ChordClass::Guest,
        backspace_leased: true,
    };
    assert_verdicts(
        &mut registry,
        physical_key,
        &[(leased, KeyVerdict::Decline)],
    );

    // An accelerator release cannot escape without a guest press.
    let accelerator_key = 31;
    assert_verdicts(
        &mut registry,
        accelerator_key,
        &[
            (ACCELERATOR_PRESS, KeyVerdict::Decline),
            (PhysicalKey::Release, KeyVerdict::Decline),
        ],
    );
}

#[test]
fn guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces() {
    #[derive(Clone, Copy, Debug)]
    enum Operation {
        Physical(PhysicalKey),
        Recover,
    }

    let operations = [
        Operation::Physical(PRESS),
        Operation::Physical(ACCELERATOR_PRESS),
        Operation::Physical(SHORTCUT_PRESS),
        Operation::Physical(PhysicalKey::Release),
        Operation::Recover,
    ];
    let sequence_len = 6;
    let sequence_count = operations.len().pow(sequence_len);
    let keyboard = HostId::for_test(10);
    let key = 30;

    for mut encoded in 0..sequence_count {
        let mut registry = KeyGenerationRegistry::default();
        let mut guest_press_open = false;

        for _ in 0..sequence_len {
            let operation = operations[encoded % operations.len()];
            encoded /= operations.len();
            let owner_before = registry.guest_owner(keyboard, key);
            let suppressed_before = registry.host_accelerator_suppressed(keyboard, key);
            let event = match operation {
                Operation::Physical(event) => event,
                Operation::Recover => {
                    let recovered = registry.recover(keyboard, key);
                    assert!(
                        !recovered || !guest_press_open,
                        "recovery cannot overlap an owned guest press"
                    );
                    assert_eq!(
                        registry.guest_owner(keyboard, key) == Some(GuestKeyOwner::Physical),
                        guest_press_open
                    );
                    continue;
                }
            };
            let verdict = registry.transition_physical(keyboard, key, event);
            let shortcut = owner_before == Some(GuestKeyOwner::Shortcut);
            let handled = match event {
                PhysicalKey::Press { chord, .. } => {
                    shortcut
                        || (owner_before.is_none() && chord == ChordClass::Shortcut)
                        || (owner_before != Some(GuestKeyOwner::ImeRecovery)
                            && !suppressed_before
                            && chord != ChordClass::HostAccelerator)
                }
                PhysicalKey::Release => shortcut || owner_before == Some(GuestKeyOwner::Physical),
            };
            assert_eq!(
                verdict.handled(),
                handled,
                "ACK policy must remain coupled to the generation owner: {event:?} \
                 of {owner_before:?}"
            );
            match (event, verdict) {
                (PhysicalKey::Press { .. }, KeyVerdict::Forward) => {
                    assert!(!guest_press_open, "a second press cannot be forwarded");
                    guest_press_open = true;
                }
                (PhysicalKey::Release, KeyVerdict::Forward) => {
                    assert!(
                        guest_press_open,
                        "release forwarding requires an open guest press"
                    );
                    guest_press_open = false;
                }
                // Only a forwarded release closes the guest's press.
                (PhysicalKey::Release, _) => {}
                (
                    PhysicalKey::Press {
                        chord: ChordClass::Shortcut,
                        ..
                    },
                    KeyVerdict::Shortcut,
                ) => assert_eq!(owner_before, None, "a shortcut claims an unowned key"),
                (_, KeyVerdict::Shortcut) => panic!("only a shortcut press claims"),
                (PhysicalKey::Press { .. }, _) => {}
            }
            assert_eq!(
                registry.guest_owner(keyboard, key) == Some(GuestKeyOwner::Physical),
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
        host_accelerator_suppressed: bool,
        stale: bool,
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
        /// Each key's retired release serial.
        retired_guest_releases: BTreeMap<u32, u32>,
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

        fn press(&mut self, key: u32) {
            self.retire_released(key);
            self.ensure(key).physical_state = PhysicalKeyState::Held;
        }

        fn retire_released(&mut self, key: u32) {
            let Some(generation) = self
                .keys
                .get(&key)
                .copied()
                .filter(|generation| generation.physical_state == PhysicalKeyState::Released)
            else {
                return;
            };
            if let (Some(GuestKeyOwner::Physical), Some(release_serial)) =
                (generation.guest_owner, generation.physical_release_serial)
            {
                self.retired_guest_releases.insert(key, release_serial);
            }
            self.keys.remove(&key);
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
                self.press(key);
            }
            self.ensure(key);
            self.next_generation = self.next_generation.wrapping_add(1).max(1);
            let sequence = self.next_generation;
            let generation = self.ensure(key);
            generation.id = sequence;
            generation.peek_press_serial = Some(serial);
            generation.peek = Some(PeekKeyProvenance {
                serial,
                time,
                sequence,
                eligible,
                composed: false,
            });
        }

        fn peek_release(&mut self, key: u32, serial: u32) {
            self.release(key, Some(serial));
        }

        /// H2: every held key is stale.
        fn mark_stale(&mut self) {
            for generation in self.keys.values_mut() {
                if generation.physical_state == PhysicalKeyState::Held {
                    generation.stale = true;
                }
            }
        }

        /// A peek press of a stale held key ends its generation as a
        /// physical release would; whether the guest is owed that
        /// release.
        fn retire_stale(&mut self, key: u32) -> bool {
            let Some(generation) = self.keys.get(&key).copied().filter(|generation| {
                generation.physical_state == PhysicalKeyState::Held && generation.stale
            }) else {
                return false;
            };
            self.keys.remove(&key);
            generation.guest_owner == Some(GuestKeyOwner::Physical)
                && !generation.host_accelerator_suppressed
        }

        /// The IME ends `owner`'s lease of `key` for the rest of its hold.
        fn end_lease(&mut self, key: u32, owner: u32) {
            if let Some(generation) = self
                .keys
                .get_mut(&key)
                .filter(|generation| generation.ime_repeat_owner == Some(owner))
            {
                generation.backspace_repeat_cancelled = true;
                generation.ime_repeat_owner = None;
            }
        }

        fn held(&self, key: u32) -> Option<ModelGeneration> {
            self.keys
                .get(&key)
                .copied()
                .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
        }

        /// A duplicate peek press of a held key.
        fn refresh_peek(&mut self, key: u32, serial: u32, time: u32) {
            if let Some(generation) = self
                .keys
                .get_mut(&key)
                .filter(|generation| generation.physical_state == PhysicalKeyState::Held)
            {
                generation.peek_press_serial = Some(serial);
                if let Some(provenance) = generation.peek.as_mut() {
                    provenance.serial = serial;
                    provenance.time = time;
                }
            }
        }

        fn peek_key(&mut self, key: u32, input: PeekInput) -> PeekOutcome {
            match input {
                PeekInput::Press {
                    serial,
                    time,
                    guest_input,
                } => {
                    let owes_release = self.retire_stale(key);
                    let held = self.held(key);
                    if held.is_some_and(|generation| generation.peek.is_some()) {
                        self.refresh_peek(key, serial, time);
                        return PeekOutcome {
                            owes_release,
                            ..PeekOutcome::default()
                        };
                    }
                    let consumed_by_shortcut = held.is_some_and(|generation| {
                        generation.guest_owner == Some(GuestKeyOwner::Shortcut)
                    });
                    let suppressed = self
                        .keys
                        .get(&key)
                        .is_some_and(|generation| generation.host_accelerator_suppressed);
                    let eligible = guest_input && !consumed_by_shortcut && !suppressed;
                    self.peek_press(key, serial, time, eligible);
                    PeekOutcome {
                        owes_release,
                        numbered: Some(self.next_generation),
                        ignored: false,
                    }
                }
                PeekInput::Release { serial, time } => {
                    let peek = self.keys.get(&key).and_then(|generation| generation.peek);
                    if peek.is_some_and(|press| {
                        !serial_is_after(serial, press.serial)
                            && (serial != press.serial || serial_is_after(press.time, time))
                    }) {
                        return PeekOutcome {
                            ignored: true,
                            ..PeekOutcome::default()
                        };
                    }
                    self.peek_release(key, serial);
                    PeekOutcome::default()
                }
            }
        }

        fn admit_key(&mut self, key: u32, state: KeyState, serial: u32) -> KeyAdmission {
            let generation = self.keys.get(&key).copied();
            let peek_press_serial = generation.and_then(|generation| generation.peek_press_serial);
            let dropped = KeyAdmission::Settled(KeyVerdict::Decline);
            if state == KeyState::Released {
                if self.retired_guest_releases.get(&key) == Some(&serial) {
                    self.retired_guest_releases.remove(&key);
                    return KeyAdmission::Settled(KeyVerdict::Forward);
                }
                if peek_press_serial.is_some()
                    && generation.and_then(|generation| generation.physical_release_serial)
                        != Some(serial)
                {
                    return dropped;
                }
                self.release(key, Some(serial));
            } else {
                if self.held(key).is_some()
                    && peek_press_serial.is_some_and(|peek_serial| peek_serial != serial)
                {
                    return dropped;
                }
                let delayed_released_peek_press = generation.is_some_and(|generation| {
                    generation.physical_state == PhysicalKeyState::Released
                }) && peek_press_serial == Some(serial);
                if !delayed_released_peek_press {
                    self.press(key);
                }
            }
            KeyAdmission::Current
        }

        /// A `wl_keyboard.enter` lists `keys` held, in its order.
        fn enter(&mut self, keys: &[u32]) {
            self.keys.clear();
            self.retired_guest_releases.clear();
            for &key in keys {
                let generation = self.ensure(key);
                generation.physical_state = PhysicalKeyState::Held;
                generation.guest_owner = Some(GuestKeyOwner::Physical);
            }
        }
    }

    #[derive(Clone, Copy)]
    enum Operation {
        Press(u32),
        Release(u32),
        /// `peek_key` press, with whether it is guest input.
        PeekPress(u32, bool),
        /// `peek_key` release, under an older serial if true.
        PeekRelease(u32, bool),
        /// `wl_keyboard` press, under the key's peek serial if true.
        AdmitPress(u32, bool),
        /// `wl_keyboard` release, under a release serial the registry
        /// knows (a retired generation's or the key's own) if true.
        AdmitRelease(u32, bool),
        /// `wl_keyboard.enter` with both keys held.
        EnterSnapshot,
        InvalidatePeek(u32),
        /// A press of another key: Backspace's lease is cancelled.
        CancelRepeat,
        /// The IME arms the lease for a text input, if Backspace is held.
        ArmImeRepeat(u32),
        /// The IME ends a text input's lease.
        CancelRepeatOwner(u32),
        SuppressAccelerator(u32),
        TakeAcceleratorSuppression(u32),
        ClaimOwner(u32, GuestKeyOwner),
        TakePendingPhysicalRelease(u32),
        TakeOwner(u32),
        MarkStale,
        RetireStale(u32),
        Clear,
    }

    // Backspace, which the lease concerns.
    const KEY_A: u32 = EVDEV_KEY_BACKSPACE;
    const KEY_B: u32 = 57;
    let keyboard = HostId::for_test(10);
    let operations = [
        Operation::Press(KEY_A),
        Operation::Press(KEY_B),
        Operation::Release(KEY_A),
        Operation::Release(KEY_B),
        Operation::PeekPress(KEY_A, true),
        Operation::PeekPress(KEY_A, false),
        Operation::PeekPress(KEY_B, true),
        Operation::PeekRelease(KEY_A, false),
        Operation::PeekRelease(KEY_A, true),
        Operation::AdmitPress(KEY_A, false),
        Operation::AdmitPress(KEY_A, true),
        Operation::AdmitRelease(KEY_A, false),
        Operation::AdmitRelease(KEY_A, true),
        Operation::EnterSnapshot,
        Operation::InvalidatePeek(KEY_B),
        Operation::CancelRepeat,
        Operation::ArmImeRepeat(77),
        Operation::ArmImeRepeat(78),
        Operation::CancelRepeatOwner(77),
        Operation::CancelRepeatOwner(78),
        Operation::SuppressAccelerator(KEY_B),
        Operation::TakeAcceleratorSuppression(KEY_B),
        Operation::ClaimOwner(KEY_A, GuestKeyOwner::Physical),
        Operation::ClaimOwner(KEY_A, GuestKeyOwner::ImeRecovery),
        Operation::ClaimOwner(KEY_A, GuestKeyOwner::Shortcut),
        Operation::TakePendingPhysicalRelease(KEY_A),
        Operation::TakeOwner(KEY_A),
        Operation::MarkStale,
        Operation::RetireStale(KEY_A),
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
                    registry.observe_physical_state(keyboard, key, KeyState::Pressed);
                    model.press(key);
                }
                Operation::Release(key) => {
                    registry.observe_physical_state(keyboard, key, KeyState::Released);
                    model.release(key, None);
                }
                Operation::PeekPress(key, guest_input) => {
                    let input = PeekInput::Press {
                        serial,
                        time,
                        guest_input,
                    };
                    let actual = registry.peek_key(keyboard, key, input);
                    assert_eq!(actual, model.peek_key(key, input));
                }
                Operation::PeekRelease(key, older) => {
                    let serial = if older {
                        serial.wrapping_sub(2)
                    } else {
                        serial
                    };
                    let input = PeekInput::Release { serial, time };
                    let actual = registry.peek_key(keyboard, key, input);
                    assert_eq!(actual, model.peek_key(key, input));
                }
                Operation::AdmitPress(key, under_peek) => {
                    let serial = model
                        .keys
                        .get(&key)
                        .and_then(|generation| generation.peek_press_serial)
                        .filter(|_| under_peek)
                        .unwrap_or(serial);
                    let actual = registry.admit_key(keyboard, key, KeyState::Pressed, serial);
                    assert_eq!(actual, model.admit_key(key, KeyState::Pressed, serial));
                }
                Operation::AdmitRelease(key, known) => {
                    let generation = model.keys.get(&key);
                    let serial = model
                        .retired_guest_releases
                        .get(&key)
                        .copied()
                        .or(generation.and_then(|generation| generation.physical_release_serial))
                        .filter(|_| known)
                        .unwrap_or(serial);
                    let actual = registry.admit_key(keyboard, key, KeyState::Released, serial);
                    assert_eq!(actual, model.admit_key(key, KeyState::Released, serial));
                }
                Operation::EnterSnapshot => {
                    registry.install_enter_snapshot(keyboard, [KEY_B, KEY_A]);
                    model.enter(&[KEY_B, KEY_A]);
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
                Operation::CancelRepeat => {
                    registry.lease(&[keyboard], LeaseInput::OtherPress { keyboard });
                    if model.held(KEY_A).is_some() {
                        let generation = model.ensure(KEY_A);
                        generation.backspace_repeat_cancelled = true;
                        generation.ime_repeat_owner = None;
                    }
                }
                Operation::ArmImeRepeat(owner) => {
                    let outcome = LeaseOutcome::ArmIfBackspaceHeld;
                    let ti = TiId(GuestId::for_test(owner));
                    registry.lease(&[keyboard], LeaseInput::Ime { ti, outcome });
                    match model.held(KEY_A) {
                        Some(held) if !held.backspace_repeat_cancelled => {
                            let generation = model.ensure(KEY_A);
                            if generation.ime_repeat_owner.is_none() {
                                generation.ime_repeat_owner = Some(owner);
                            }
                        }
                        _ => model.end_lease(KEY_A, owner),
                    }
                }
                Operation::CancelRepeatOwner(owner) => {
                    let outcome = LeaseOutcome::End;
                    let ti = TiId(GuestId::for_test(owner));
                    registry.lease(&[keyboard], LeaseInput::Ime { ti, outcome });
                    model.end_lease(KEY_A, owner);
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
                Operation::TakePendingPhysicalRelease(key) => {
                    let release_serial = serial.wrapping_sub(2);
                    let actual =
                        registry.take_pending_physical_release(keyboard, key, release_serial);
                    let expected = model.retired_guest_releases.get(&key) == Some(&release_serial);
                    if expected {
                        model.retired_guest_releases.remove(&key);
                    }
                    assert_eq!(actual, expected);
                }
                Operation::TakeOwner(key) => {
                    registry.take_guest_owner(keyboard, key);
                    if let Some(generation) = model.keys.get_mut(&key) {
                        generation.guest_owner = None;
                    }
                    model.prune();
                }
                Operation::MarkStale => {
                    registry.mark_held_keys_stale(keyboard);
                    model.mark_stale();
                }
                Operation::RetireStale(key) => {
                    let actual = registry.retire_stale_generation(keyboard, key);
                    assert_eq!(actual, model.retire_stale(key));
                }
                Operation::Clear => {
                    registry.clear_keyboard(keyboard);
                    model.keys.clear();
                    model.retired_guest_releases.clear();
                }
            }
            for key in [KEY_A, KEY_B] {
                let held = model
                    .keys
                    .get(&key)
                    .filter(|generation| generation.physical_state == PhysicalKeyState::Held);
                assert_eq!(
                    registry.peek_press_is_new(keyboard, key),
                    held.is_none_or(|generation| generation.stale)
                );
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
                assert_eq!(actual.physical_state(), expected.physical_state);
                assert_eq!(actual.peek(), expected.peek);
                assert_eq!(actual.peek_press_serial(), expected.peek_press_serial);
                assert_eq!(
                    actual.physical_release_serial(),
                    expected.physical_release_serial
                );
                assert_eq!(
                    actual.backspace_repeat_cancelled(),
                    expected.backspace_repeat_cancelled
                );
                assert_eq!(
                    actual.ime_repeat_owner(),
                    expected.ime_repeat_owner.map(GuestId::for_test)
                );
                assert_eq!(actual.guest_owner(), expected.guest_owner);
                // A detach marks only a held key; a release ends it.
                assert_eq!(
                    actual.stale(),
                    expected.stale && expected.physical_state == PhysicalKeyState::Held
                );
                assert_eq!(
                    actual.host_accelerator_suppressed(),
                    expected.host_accelerator_suppressed
                );
            }
            assert_eq!(
                registry
                    .retired_guest_releases
                    .iter()
                    .filter(|((pending_keyboard, _), _)| *pending_keyboard == keyboard)
                    .map(|(&(_, key), &release)| (key, release))
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
    let keyboard = HostId::for_test(10);
    let key_a = 30;
    let key_b = 57;
    let mut registry = KeyGenerationRegistry::default();

    let first_a = registry.observe_peek_press(keyboard, key_a, 1, 10, false);
    let first_b = registry.observe_peek_press(keyboard, key_b, 2, 20, true);
    registry.observe_physical_state(keyboard, key_a, KeyState::Released);
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

/// A late `wl_keyboard` release of a retired generation still closes
/// the guest's press of it once the key is pressed again, also across
/// the serial's wrap, and leaves the new generation held.
#[test]
fn a_retired_release_closes_its_press_across_the_serial_wrap() {
    let keyboard = HostId::for_test(10);
    let key = 57;
    let mut registry = KeyGenerationRegistry::default();

    registry.observe_peek_press(keyboard, key, u32::MAX - 1, 400, true);
    assert!(registry.claim_guest_owner(keyboard, key, GuestKeyOwner::Physical));
    registry.observe_peek_release(keyboard, key, u32::MAX);
    registry.observe_peek_press(keyboard, key, 0, 500, true);
    assert!(
        registry.take_pending_physical_release(keyboard, key, u32::MAX),
        "a physical release must survive the next generation across serial wrap"
    );
    assert!(
        !registry.take_pending_physical_release(keyboard, key, u32::MAX),
        "each retired press must be released exactly once"
    );
    assert!(registry.physically_held(keyboard, key));
}

/// A host that never sends the `wl_keyboard` release of a key the
/// guest got leaves one retired release of that key pending, however
/// often the key is pressed again: the newer retired release replaces
/// the older one.
#[test]
fn a_key_keeps_one_pending_release() {
    let key = 30;
    let generations = 100;
    let mut registry = KeyGenerationRegistry::default();
    // Generation g is pressed with serial 10g on the peek channel and
    // on `wl_keyboard`, and released with 10g + 1 on the peek channel
    // only.
    for generation in 1..=generations {
        let serial = 10 * generation;
        registry.observe_peek_press(KEYBOARD_A, key, serial, serial, true);
        let verdict = registry.transition_physical(KEYBOARD_A, key, PRESS);
        assert_eq!(verdict, KeyVerdict::Forward);
        registry.observe_peek_release(KEYBOARD_A, key, serial + 1);
    }
    let next = 10 * (generations + 1);
    registry.observe_peek_press(KEYBOARD_A, key, next, next, true);

    for generation in 1..generations {
        assert!(
            !registry.take_pending_physical_release(KEYBOARD_A, key, 10 * generation + 1),
            "generation {generation}'s release must not be taken"
        );
    }
    assert!(
        registry.take_pending_physical_release(KEYBOARD_A, key, 10 * generations + 1),
        "the last generation must still take its release"
    );
}

#[test]
fn clearing_guest_keys_is_scoped_to_one_keyboard() {
    let mut registry = KeyGenerationRegistry::default();
    assert!(registry.claim_guest_owner(HostId::for_test(10), 57, GuestKeyOwner::ImeRecovery));
    assert!(registry.claim_guest_owner(HostId::for_test(11), 57, GuestKeyOwner::Physical));
    for keyboard in [HostId::for_test(10), HostId::for_test(11)] {
        registry.observe_physical_state(keyboard, 30, KeyState::Pressed);
        assert!(registry.claim_guest_owner(keyboard, 30, GuestKeyOwner::Physical));
        registry.observe_physical_event(keyboard, 30, KeyState::Released, Some(2));
        registry.observe_physical_state(keyboard, 30, KeyState::Pressed);
    }

    registry.clear_keyboard(HostId::for_test(10));

    assert!(registry.guest_owner(HostId::for_test(10), 57).is_none());
    assert_eq!(
        registry.guest_owner(HostId::for_test(11), 57),
        Some(GuestKeyOwner::Physical)
    );
    assert!(
        !registry.take_pending_physical_release(HostId::for_test(10), 30, 2),
        "clearing a keyboard must discard its retired releases"
    );
    assert!(
        registry.take_pending_physical_release(HostId::for_test(11), 30, 2),
        "clearing one keyboard must preserve another keyboard's retired releases"
    );
}

/// A focus change ends the seat's leases, and leaves another keyboard's
/// held Backspace, which no text input leases, free.
#[test]
fn ending_one_keyboard_repeat_does_not_cancel_another_held_keyboard() {
    let seat = [KEYBOARD_A, KEYBOARD_B];
    let mut registry = KeyGenerationRegistry::default();
    for keyboard in seat {
        registry.observe_physical_state(keyboard, EVDEV_KEY_BACKSPACE, KeyState::Pressed);
    }
    let outcome = LeaseOutcome::ArmIfBackspaceHeld;
    registry.lease(
        &seat,
        LeaseInput::Ime {
            ti: TiId(GuestId::for_test(1)),
            outcome,
        },
    );
    assert_eq!(
        registry.backspace_lease(KEYBOARD_A),
        Some(TiId(GuestId::for_test(1)))
    );

    registry.lease(&seat, LeaseInput::FocusLost);

    assert!(registry.backspace_repeat_cancelled(KEYBOARD_A));
    assert!(!registry.backspace_repeat_cancelled(KEYBOARD_B));
    assert!(registry.physically_held(KEYBOARD_B, EVDEV_KEY_BACKSPACE));
}
