# Keyboard key states

How the keyboard concern tracks each physical key, so that a key reaches
the guest once, through one channel, and is released where it was
pressed. [DESIGN.md](DESIGN.md#keyboard-and-ime) places the concern in
the proxy; the rules it keeps are under
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime).

A key can reach the guest three ways: a `wl_keyboard.key`, a key the
`wl_keyboard.enter` lists, and a press and release the proxy recovers
for an empty IME confirmation (X2' of [IME.md](IME.md#22-host-events)).
Text-input keysyms and key repeats reach the guest as nothing
(INVARIANTS "Host keysyms and repeats are dropped"). Exo announces each
key on `zcr_extended_keyboard_v1.peek_key` first, even one its input
method consumes, so the proxy sees the key's physical life on two
channels and its delivery on up to two.

## State

`KeyGenerationRegistry` (`sommelier/src/state/input.rs`) holds one
`KeyGeneration` per host keyboard and key, its current generation: the
key's life from one press to the next. A key with no entry is idle. A
generation is three independent parts, each an enum whose variants carry
only the data that exists in them:

- `phase: KeyPhase`, what the host has shown of the key:
  - `Unseen`: a claim or a suppression came before any physical event,
    which only the registry's tests make;
  - `Held { peek, lease, stale }`: `lease` is the IME's Backspace repeat
    lease (`RepeatLease::Free`, `Leased { text_input }` or `Cancelled`
    for the rest of the hold); `stale` marks a key held across the input
    method's own detach (H2);
  - `Released { peek, release_serial }`.
- `peek: PeekRecord`, inside `Held` and `Released`: `None`, `Announced {
  serial }` (a generation whose keyboard has since held no key) or
  `Live(PeekKeyProvenance)`, the serial, time, sequence and eligibility of
  a peek press, which makes the generation a candidate for recovery.
- `guest: GuestPress`, who gave the guest the key: `None`, `Physical`,
  `Shortcut` (Sommelier placed a window), or `Recovered`, a tombstone for
  a recovered pair that suppresses late duplicates.
- `host_accelerator_suppressed`: a `wl_keyboard` press that no channel
  gave the guest was a host accelerator. It is independent of the rest:
  its release is dropped whatever the phase.

When a key is pressed again before `wl_keyboard` released the guest's
previous press of it, the retired generation's release waits beside the
new one (`retired_guest_releases`), at most one per key.

Phase and guest delivery are independent because the channels race: a
`wl_keyboard` press can come after its peek release. The registry's model test (see [Tests](#tests)) keeps
each generation as independent flags; within each part, their illegal
combinations cannot be written:

| Model flag | State |
| --- | --- |
| `physical_state` | the `KeyPhase` variant |
| `peek`, `peek_press_serial` | `PeekRecord`, inside `Held` and `Released`; a live peek's serial is its announced serial |
| `physical_release_serial` | `Released { release_serial }` only |
| `backspace_repeat_cancelled`, `ime_repeat_owner` | `RepeatLease`, inside `Held` only: never both, and a release ends it |
| `stale` | `Held { stale }` only: a release ends it |
| `guest_owner` | `GuestPress` |

## Transitions

Each input has one transition in the registry. The handler decodes the
event, supplies the facts the registry cannot know (the chord under the
keyboard's XKB state, focus), and applies the effects on the guest:

| Input | Transition | Handler |
| --- | --- | --- |
| `peek_key` press, release | `peek_key(PeekInput)` → `PeekOutcome` | `on_peek_key` |
| `wl_keyboard.key` | `admit_key` (physical), then `transition_guest_key` (delivery) | `on_key` |
| recovered pair | `transition_guest_key(RecoverIme)` | `ime_keys::recover_pending` |
| `wl_keyboard.enter` | `install_enter_snapshot`: every listed key `Held`, `Physical` | `on_enter` |
| leave, keyboard end | `clear_keyboard` of each keyboard that lost its focus or ended | `on_leave`, `forget_keyboard` |
| H2 detach | `mark_held_keys_stale` | `ime_keys::forget_held_keys_for_seat` |
| Backspace lease | `arm_ime_repeat`, `cancel_backspace_repeat(_for_owner)` | `ime_keys`, `text_input::apply_lease` |

`wl_keyboard.key` has two steps because placing a window, which a
placement chord does before the key's delivery is decided, needs the
whole `Context`. `admit_key` settles the events that belong to another
generation (a late release of a retired generation, a release under
another serial than the key's peek release, or a press of a held key
under another serial than its peek announced) and moves the phase;
`transition_guest_key` decides the delivery and the `ack_key` from the
guest owner. `peek_key` retires a stale held generation on a press, owing
the guest its release, numbers a new peek generation, refreshes the peek
of a held one, or closes it on a release, unless the release is older
than the peek.

The phase moves:

- idle or `Unseen` → `Held`: a press on either channel, or the enter.
- `Held` → `Released`: a release on either channel; it ends the lease
  and the detach marker.
- `Released` → a new `Held` generation: a press. If the guest still
  holds the old press, its release waits as a retired release.
- `Held { stale: true }` → a new `Held` generation: a peek press. If the
  guest still holds the old press, it gets its release first.
- any → idle: once nothing refers to the generation (not held, no live
  peek, no guest owner, no suppression), and at an enter, a leave that
  ends the keyboard's focus, or the keyboard's end.

## The recovery decision

An empty confirmation at extension v11 is either a held key's autorepeat
or the confirmation Exo sends before a press its input method does not
handle (INVARIANTS: "An empty confirmation recovers a held key only once
the host shows it repeated"). `ime_keys::PendingRecovery` holds the
recovery, and `PendingRecovery::decide` is its transition for each host
event, read as a `HostSignal`. No recovery starts while a keyboard of
the seat holds a key `stale`, whose next press Exo may announce with the
confirmation alone (`held_repeat_key_for_seat`).

| Waiting for | Event | Step |
| --- | --- | --- |
| `NextEvent` | `wl_keyboard.modifiers` | wait |
| `NextEvent` | peek press, on a known host keyboard, of a key that keyboard does not hold, or holds stale | wait for that key's `Press` |
| `Press` | that key's `wl_keyboard.key` press with the peek's serial | drop |
| any | the text input's `leave` (E6) | drop |
| any | anything else | recover, then handle the event |

## Tests

- `key_generation_registry_matches_the_model_for_all_short_transition_sequences`
  checks every sequence of four registry operations, `peek_key`,
  `admit_key`, the enter's snapshot, the detach and the retirement of a
  stale generation included, against a model of the flat fields the
  states replaced.
- `guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces`
  checks that the delivery reducer never opens two guest presses, nor
  forwards a release without one.
- `a_waiting_recovery_is_decided_by_the_hosts_next_event` pins the
  recovery's transition table.
- The handler tests in `keyboard.rs`, `ime_keys.rs` and `text_input.rs`
  and the DST pin the wire behaviour.
