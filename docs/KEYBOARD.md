# Keyboard key states

How the keyboard concern tracks each physical key, so that a key reaches
the guest once, through one channel, and is released where it was
pressed. [DESIGN.md](DESIGN.md#keyboard-and-ime) places the concern in
the proxy; the rules it keeps are under
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime).

A key can reach the guest four ways: a `wl_keyboard.key`, a key the
`wl_keyboard.enter` lists, a text-input keysym, and a press and release
the proxy recovers for an empty IME confirmation (X2'). Exo announces each
key on `zcr_extended_keyboard_v1.peek_key` first, even one its input
method consumes, so the proxy sees the key's physical life on two
channels and its delivery on up to three.

## State

`KeyGenerationRegistry` (`sommelier/src/state/input.rs`) holds one
`KeyGeneration` per host keyboard and key, its current generation: the
key's life from one press to the next. A key with no entry is idle. A
generation is three independent parts, each an enum whose variants carry
only the data that exists in them:

- `phase: KeyPhase`, what the host has shown of the key:
  - `Unseen`: a keysym (or a test's claim) came before any physical
    event;
  - `Held { peek, lease, stale }`: `lease` is the IME's Backspace repeat
    lease (`RepeatLease::Free`, `Leased { text_input }` or `Cancelled`
    for the rest of the hold); `stale` marks a key held across the input
    method's own detach (H2);
  - `Released { peek, release_serial }`.
- `peek: PeekRecord`, inside `Held` and `Released`: `None`, `Announced {
  serial }` (a peek repeat of a key held since `enter`, or a generation
  whose keyboard has since held no key) or `Live(PeekKeyProvenance)`, the
  serial, time, sequence and eligibility of a peek press, which makes the
  generation a candidate for recovery and keysym routing.
- `guest: GuestPress`, who gave the guest the key: `None`, `Physical`,
  `Keysym { press_serial }`, `Shortcut` (Sommelier placed a window), or
  `Recovered { keysym_press_serial }`, a tombstone for a balanced pair
  that suppresses late duplicates.
- `host_accelerator_suppressed`: a `wl_keyboard` press that no channel
  gave the guest was a host accelerator. It is independent of the rest:
  its release and the generation's keysyms are dropped whatever the
  phase.

When a key is pressed again before the channel that gave the guest its
previous press released it, the retired generation's release waits
beside the new one (`retired_guest_releases`), at most one per channel.

Phase and guest delivery are independent because the channels race: a
keysym can come before the key's peek, and a `wl_keyboard` press after
its peek release. Within each part the old flags' illegal combinations
cannot be written:

| Flag before | Now |
| --- | --- |
| `physical_state` | the `KeyPhase` variant |
| `peek`, `peek_press_serial` | `PeekRecord`, inside `Held` and `Released`; a live peek's serial is its announced serial |
| `physical_release_serial` | `Released { release_serial }` only |
| `backspace_repeat_cancelled`, `ime_repeat_owner` | `RepeatLease`, inside `Held` only: never both, and a release ends it |
| `stale` | `Held { stale }` only: a release ends it |
| `guest_owner`, `guest_press_serial` | `GuestPress`: a serial only on a keysym press or its tombstone |

## Tests

- `key_generation_registry_matches_the_model_for_all_short_transition_sequences`
  checks every sequence of four registry operations, the detach and the
  retirement of a stale generation included, against a model of the flat
  fields the states replaced.
- `guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces`
  checks that the delivery reducer never opens two guest presses, nor
  forwards a repeat or release without one.
- The handler tests in `keyboard.rs`, `ime_keys.rs` and `text_input.rs`
  and the DST pin the wire behaviour.
