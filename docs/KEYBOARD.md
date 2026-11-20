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

## The keyboard core

The keyboard concern is a sans-IO state machine, the IME core's twin:
`KeyboardCore` (`sommelier/src/keys/`, `Context::keys`). Every message
that can change its state reaches it as one `KeyInput` through
`KeyboardCore::step`, which returns the `KeyEffects` the adapter
(`handler/keyboard.rs`) applies, in this order: a recovery the host
event decided, a consumed shortcut's binding for its subscriber, the
`ack_key`, the sync of a recovery that waits, the destroy of an ended
keyboard's extended keyboard, whether the guest gets the event, guest
events, and inputs of the IME core. It reads what it does
not own through `keys::Env`: the shortcut bindings and the IME core's
gate. It owns:

- the keyboard focus (`keys/focus.rs`): which surface each keyboard
  focuses, and which superseded enters still owe the guest a leave. A
  focus input (`Enter`, `Leave`, `SurfaceDestroyed`, `Ended`) moves it
  and, in the same step, ends what the move ends of the keys, so the
  focus and the keys cannot disagree; the IME core and placement read it
  (`KeyboardCore::focus`);
- one `Keyboard` record per host keyboard (`keys/keyboards.rs`): its
  guest object and seat, its keymap, its modifiers and its extended
  keyboard;
- each key's generations, their delivery, the Backspace lease and the
  peek watermarks (`keys/generations.rs`, below);
- the recovery of an empty confirmation's held key (`keys/recovery.rs`,
  [below](#the-recovery-decision));
- the host accelerators (`SOMMELIER_ACCELERATORS`, [below](#host-accelerators)),
  whose chords the host keeps. A chord is the host's, a shortcut's, or the guest's
  (`KeyboardCore::classify`, which both key channels ask). A shortcut is
  generic: the core asks `Env` for a chord's binding
  (`accelerator::BindingId`), consumes its press and release, and hands
  the binding to its subscriber, window placement, as
  `KeyEffects::shortcut`.

The core and the IME core talk only through typed messages: the IME
core's requests (`ime::KeyEffect`) reach it as `KeyInput::Ime`, and it
hands the IME core `ime::Input`s: `GuestKey`, `FocusChanged` and the
recovered key (`HeldKeyRecovered`). Only the core changes its state; the
registry's transitions below are private to it, and tests outside it
drive it through its step or a whole session.

## State

`KeyGenerationRegistry` (`sommelier/src/keys/generations.rs`) holds one
`KeyGeneration` per host keyboard and key, its current generation: the
key's life from one press to the next. A key with no entry is idle. A
generation has two independent parts: its phase, with the peek record
inside it, and its delivery. The phase and the peek record are enums
whose variants carry only the data that exists in them:

- `phase: KeyPhase`, what the host has shown of the key:
  - `Unseen`: a claim or a suppression came before any physical event,
    which only tests make;
  - `Held { peek, lease, stale }`: `lease` is the IME's Backspace repeat
    lease (`RepeatLease::Free`, `Leased { text_input }` or `Cancelled`
    for the rest of the hold); `stale` marks a key held across the input
    method's own detach (H2);
  - `Released { peek, release_serial }`.

  The `peek: PeekRecord` of `Held` and `Released` is `None`, `Announced {
  serial }` (a generation whose keyboard has since held no key) or
  `Live(PeekKeyProvenance)`, the serial, time, sequence and eligibility of
  a peek press, which makes the generation a candidate for recovery.
- `delivery: Delivery`, what the guest got of the key:
  - `owner`, who gave the guest the key, if anyone (`GuestKeyOwner`):
    `Physical`, `Shortcut` (Sommelier consumed it for a shortcut), or
    `ImeRecovery`, a tombstone for a recovered pair that suppresses late
    duplicates;
  - `accelerator_suppressed`: a `wl_keyboard` press that no channel gave
    the guest was a host accelerator. Its release is dropped whatever the
    phase.

A retired generation's release that `wl_keyboard` still owes the guest
waits beside the new one (`retired_guest_releases`; INVARIANTS "A key
waits for at most one late release").

Phase and guest delivery are independent because the channels race: a
`wl_keyboard` press can come after its peek release. The registry's model
test (see [Tests](#tests)) keeps each generation as independent flags;
within each part, their illegal combinations cannot be written:

| Model flag | State |
| --- | --- |
| `physical_state` | the `KeyPhase` variant |
| `peek`, `peek_press_serial` | `PeekRecord`, inside `Held` and `Released`; a live peek's serial is its announced serial |
| `physical_release_serial` | `Released { release_serial }` only |
| `backspace_repeat_cancelled`, `ime_repeat_owner` | `RepeatLease`, inside `Held` only: never both, and a release ends it |
| `stale` | `Held { stale }` only: a release ends it |
| `guest_owner`, `host_accelerator_suppressed` | `Delivery` |

## Transitions

Each input has one transition in the registry, which the core's step
calls with the facts the registry cannot know (the chord under the
keyboard's XKB state, focus):

| Input | Transition | `KeyInput` |
| --- | --- | --- |
| `peek_key` press, release | `peek_key(PeekInput)` → `PeekOutcome` | `Peek` |
| `wl_keyboard.key` | `admit_key` (physical), then `transition_physical` (delivery) | `Key` |
| recovered pair | `recover` | the host event that decides the recovery |
| `wl_keyboard.enter` | `install_enter_snapshot`: every listed key `Held`, `Physical` | `Enter` |
| leave, surface or keyboard end | `clear_keyboard` of each keyboard that lost its focus or ended | `Leave`, `SurfaceDestroyed`, `Ended` |
| H2 detach | `mark_held_keys_stale` | `Ime(KeyEffect::InputMethodDetached)` |
| Backspace lease | `lease(LeaseInput)` | `Key`, `Peek`, `Ime(KeyEffect::Lease)`, a recovery and a focus input |

`wl_keyboard.key` has two steps, because only a press of the current
generation needs its chord classified. `admit_key` settles the events
that belong to another generation (a late release of a retired
generation, a release under another serial than the key's peek release,
or a press of a held key under another serial than its peek announced)
and moves the phase. `transition_physical` takes the press's chord class
(`ChordClass`) and returns its `KeyVerdict`: forwarded, consumed or
declined, which also fixes its `ack_key`, or `Shortcut`, a press that
claimed its generation for the shortcut whose binding the handler hands
its subscriber, window placement.
Its pure core, `Delivery::physical`, is one match of the delivery and
the event; `Delivery::recover` is the recovery's. `peek_key` retires a stale held generation on a press, owing
the guest its release, numbers a new peek generation, refreshes the peek
of a held one, or closes it on a release, unless the release is older
than the peek.

The lease has one transition, `lease`, for the seat's keyboards and a
`LeaseInput`: the IME core's outcome for a text input (arm the seat's
first held Backspace whose lease is not cancelled, or end that text
input's lease), another key's press (which cancels its keyboard's held
Backspace for the rest of the hold), a Backspace release or a focus
change (which end the seat's leases), and a recovery (which arms the
recovered Backspace whose pair went, and ends the lease after another
recovered pair, or a Backspace whose pair was skipped). A lease names
its text input by `TiId`.

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

`keys::recovery::PendingRecovery` holds the recovery that INVARIANTS "An
empty confirmation recovers a held key only once the host shows it
repeated" defers, and `PendingRecovery::decide` is its transition for
each host event, read as a `HostSignal`. The core reads the signal off the
input itself, decoded once, by the dispatcher: `Key`, `Modifiers`,
`Peek` and `TextInputLeave` are their events, and the adapter hands the
core `HostEvent` before any other host event while a recovery waits
(`handler::keyboard::before_host_event`, which reads the `AnyEvent` the
proxy decodes each host event into, once). A `wl_keyboard.key` or `modifiers` the proxy drops unhandled,
of a keyboard the guest has released, reaches the core undecoded
(`UnhandledKey`, `UnhandledModifiers`): Exo sends a key right after its
peek, so such a key of the keyboard a waited-for peek came from is that
press (test:
`a_press_dropped_with_its_released_keyboard_is_still_the_confirmations`).
Any other event the proxy drops is any other event. What the recovery
sends precedes the
event: the adapter sends a `wl_keyboard.key` or `modifiers` it decided
after it, in place of the forward (test:
`a_recovery_reaches_the_guest_before_the_event_that_decides_it`).
`KeyboardCore::held_repeat_key` keeps the rule's exception for a key
held `stale` across a detach.

| Waiting for | Event | Step |
| --- | --- | --- |
| `NextEvent` | `wl_keyboard.modifiers` | wait |
| `NextEvent` | peek press, on a known host keyboard, of a key that keyboard does not hold, or holds stale | wait for that key's `Press` |
| `Press` | that key's `wl_keyboard.key` press with the peek's serial | drop |
| `Press` | a `wl_keyboard.key` of that keyboard the proxy drops undecoded, as the guest released the keyboard | drop |
| any | the text input's `leave` (E6) | drop |
| any | anything else | recover, then handle the event |

## Host accelerators

Exo runs a Chrome accelerator (Ctrl+T opens a tab, Ctrl+W closes one)
on a key it also sends the client, unless the keyboard has a
`zcr_extended_keyboard_v1`: then it holds each key's accelerator in
`pending_key_acks_` until the client's `ack_key`, or for 1000 ms, and
runs it only for `not_handled` (`Keyboard::AckKeyboardKey` in
`components/exo/keyboard.cc`; `SetNeedKeyboardKeyAcks` in
`components/exo/wayland/zcr_keyboard_extension.cc`). The proxy creates an
extended keyboard for every host keyboard and acknowledges each key: a
chord in `SOMMELIER_ACCELERATORS` (the chords the host keeps; ChromeOS's
`cros-sommelier-override.conf` sets `Super_L`, `<Alt>bracketleft`,
`<Alt>bracketright` and `<Alt>tab`) as `not_handled`, so Exo runs it and
the guest does not get it, and every other key as `handled`, so the guest
app gets Ctrl+T. `zwp_keyboard_shortcuts_inhibit_manager_v1` only sets a
flag on the surface that Exo's accelerator path never reads
(`components/exo/wayland/zwp_keyboard_shortcuts_inhibit_manager.cc`), so
the proxy does not use it.

## Tests

- Every sequence of four registry operations, `peek_key`, `admit_key`,
  the enter's snapshot, the detach and the retirement of a stale
  generation included, leaves the registry as a model of the flat fields
  the states replaced. Test:
  `key_generation_registry_matches_the_model_for_all_short_transition_sequences`.
- The delivery reducer never opens two guest presses, nor forwards a
  release without one. Test:
  `guest_key_reducer_preserves_pairing_and_ack_invariants_for_short_traces`.
- The recovery's transition table. Test:
  `a_waiting_recovery_is_decided_by_the_hosts_next_event`.
- The core reads each host event before it handles it, and a recovery
  the event decides comes with the event's own delivery. Test:
  `a_waiting_recovery_reads_each_host_event_before_it_is_handled`.
- A Backspace release on any keyboard of the seat, on either channel,
  and a move of the seat's focus end its lease, and the move its peek
  watermarks; a recovery sends nothing once its text input fails the
  gate. Tests:
  `a_backspace_release_or_a_focus_change_ends_the_seats_lease`,
  `a_focus_change_ends_the_seats_peek_watermarks`,
  `a_seats_peek_watermarks_end_with_its_focus`,
  `a_recovery_sends_nothing_once_its_text_input_fails_the_gate`,
  `an_unhandled_key_of_the_announcing_keyboard_is_its_press`.
- A shortcut's chord is consumed for its subscriber, before its key is
  acknowledged, an accelerator's is the host's, and a key the guest
  already received stays the guest's.
  Tests:
  `a_bound_chord_is_consumed_for_its_subscriber_and_an_accelerator_is_the_hosts`,
  `a_shortcut_places_before_its_key_is_acknowledged`,
  `a_chord_the_guest_already_received_stays_the_guests`.
- The handler tests of `keyboard.rs` and `text_input.rs` (each in its
  `tests.rs`, the recovery's in `keyboard/tests/recovery.rs`) and the DST
  pin the wire behaviour.
