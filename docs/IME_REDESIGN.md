# Design: a clean IME core and a deterministic simulation test for sommelier-rs

Status: proposal, revision 3, 2026-11-19. It is written against
`virtwl` at `4c45b04`, which has merged #72 to #78 and #81, plus the
open PRs #79, #80, #82 and #83 and the branch
`kkimdev/ime-region-over-region`. Revision 2 answered two adversarial
reviews. Revision 3 brings in what the first implementation PRs and
their reviews settled or corrected: the final wire order of a
transaction, Exo facts read in Chromium's source, client facts the
client models established, the core's API as #82 built it, the DST
decisions for D4, and the status of each PR.
[Appendix B](#appendix-b-review-responses) lists every finding and
change, with its source.

Audience: the implementers of the PRs in [section 5](#5-migration-plan),
who may work in parallel, and any reviewer who has not followed PRs
#59 to #83.

Citations name functions rather than line numbers wherever practical, so
they survive #72 and the PRs below. "INVARIANTS *Rule*" means the bullet
of that title in `docs/INVARIANTS.md`, § Keyboard and IME unless another
section is named. After #72 the press rule is titled "A press ends the
composition on its surface".

> This plan lives in the repository so that the PRs implementing it can
> cite it. It was written from working notes that are not in the
> repository: `RESEARCH-ime.md` (sources on Exo and the clients),
> `REVIEW2.md` (open findings), two independent reviews of an earlier
> draft (`claude-review-design.md`, `oc-review-design.md`), and the
> reviews of PRs #75 to #83. Every fact this
> plan relies on is restated here with its upstream source, and
> Appendix B records each review finding and what became of it. Once the
> switch (C3c) lands, the parts that still hold move into
> [DESIGN.md](DESIGN.md) and [INVARIANTS.md](INVARIANTS.md), and this file
> is deleted.

---

## 0. Overview

sommelier-rs bridges a guest application's `zwp_text_input_v3` to Exo's
`zwp_text_input_v1`, using `zcr_extended_text_input_v1` (bound at v11)
and `zcr_extended_keyboard_v1` (`peek_key` and `ack_key`).

In the rest of the proxy, bugs stopped recurring once structure ruled
them out. That structure is the typed wire layer, the object table with
its orphan checks, `OwnedFd`, and transactional `Effects`. IME bugs keep
coming, and reviewers find them by hand by reasoning about races. Two
things are missing:

1. **No executable model of the asynchronous system.** The fuzzer
   (`proxy::fuzz`) checks robustness and the content type Exo ends up
   holding. It never checks that the user's text arrives. Every race
   fixed in #65, #67 and #71 was found by a reviewer.
2. **No single owner of the composition.** The composition is spread
   over sixteen fields of `TextInputState` (`state/input.rs`) and over
   the free functions of `handler/text_input.rs`. Each new trigger (a
   pointer press, a fence, a delete gate) added a field, plus a check in
   each event handler.

The plan has two tracks that run in parallel and meet in a staged
switch.

- **Track D: the deterministic simulation test (DST).**
  - It runs in one process, with a virtual clock and a seeded
    discrete-event scheduler.
  - Models of the Chromium, GTK3 and GTK4 v3 clients talk to the
    **real** proxy through the existing `proxy::harness::Session`.
  - Behind the proxy is an Exo model with an asynchronous IME engine
    built on PR #73's Hangul `Composer`.
  - Every message travels a FIFO channel with a drawn latency, so the
    only reorderings are the ones the real system allows.
  - It has two oracles:
    - An **exact** one at the Exo boundary. It replays the order in
      which the proxy received Exo's events and the guest's reports,
      through the specification's text semantics, and the proxy's
      output must match.
    - A **user-intent** one, PR #73's `Reference`. It checks the
      client's final text, on cases where every click and tap has a
      settle before and after it, and every focus change a settle
      before it.
  - It drives the proxy only through the wire, so it runs against
    today's code first.
- **Track C: the IME core.**
  - It is a new, isolated, sans-IO module, `sommelier/src/ime/`.
  - Its entry point is `ImeCore::step(&mut self, Input, &impl Env) ->
    Effects`. Each arm decides before it mutates.
  - Its state is a few enums: the host link, the fence and the
    composition. They replace the sixteen fields.
  - One gate, `TextInput::current()`, admits every host composition
    event.
  - One encoder builds every v3 IME transaction.
  - A reconcile runs at the end of every step, unconditionally.
  - Other concerns reach it only through `Input` messages: press (pointer
    or touch), keyboard focus, surface end, seat bind and barrier
    completion.
  - It replaces `TextInputState` and most of `handler/text_input.rs` in
    four stages, C3-pre to C3c. Each stage must reproduce, byte for
    byte, the wire traces the DST recorded on the pre-switch code.

Buffers, surfaces, focus tracking and key generations stay as they are.
They gain only the messages listed in
[3.3](#33-the-boundary-with-the-rest-of-the-proxy).

**Expected size.** Counted as new or changed lines, excluding moved
ones:

| Area | Change |
|---|---|
| Production IME code | about −900 (≈2,500 → ≈1,650) |
| IME tests, after C7 | about −2,500 |
| DST, of which PR #73 already wrote 1,100 | about +3,400 |
| **Total** | **roughly flat** |

The trade is about a third of the production IME code for an executable
model of the system ([5.5](#55-code-size)). The first PRs ran over these
budgets, so revision 3 no longer expects the total to stay flat (5.5).

### How to read this document

- [Section 1](#1-problem-statement): which bug classes recur, and why.
- [Section 2](#2-behavioural-specification): the behaviour, as a table.
  It is the single source of truth for the core's row tests, the DST
  models and the exact oracle.
- [Section 3](#3-the-ime-core): the core's types, its boundary and its
  guarantees, each stated with how it is enforced, as #82 built them.
  3.7 lists where #82 departs from revision 2.
- [Section 4](#4-the-dst-harness): the harness.
- [Section 5](#5-migration-plan): the PRs, their merge order and the
  parallel groups.
- [Section 6](#6-risks-and-open-questions): risks, unknowns and manual
  checks.
- [Appendix A](#appendix-a-invariants-rules-kept-moved-or-amended): the
  INVARIANTS mapping.
- [Appendix B](#appendix-b-review-responses): the review responses.

External behaviour carries a tag:

- **[F]** is a fact read in source. The sources are:
  - Chromium, mutter and KWin, as cited in `RESEARCH-ime.md` and
    INVARIANTS;
  - a fresh read on 2026-11-18 of Chromium `main@ffb4f13519`, GTK
    `gtk-3-24` (`imwayland.c` at c780332c), GTK `main`
    (`gtkimcontextwayland.c` at 92348cd4), mutter `main@b766717a` and
    KWin `master@f213b9af`;
  - for Exo's input method (revision 3), Chromium `main` of 2026-11-18:
    `ui/base/ime/ash/input_method_ash.cc` (`InputMethodAsh`, "IMA"),
    `components/exo/text_input.cc`, `ui/base/ime/surrounding_text_tracker.cc`
    and `components/exo/wayland/wayland_server.cc`;
  - for the order clients need (revision 3, #76): winit `master@f0abec90a3f9`
    and 0.30.12, SDL3 `main@1fc62260fc81`, Qt and kitty;
  - one captured host trace, recorded as the test
    `held_space_repeats_after_ime_commit_until_physical_release`.
- **[U]** is an unknown that only a real Chromebook host can settle.
  [6.2](#62-unknowns-and-manual-host-checks) lists each one with its
  check. **The DST draws every [U] per case rather than fixing it.**

---

## 1. Problem statement

### 1.1 The recurring bug classes

Each row is a class that has produced at least one merged fix, the
structural cause in today's code, and what replaces it. The column
"Enforced by" in [3.6](#36-guarantees-and-how-each-is-enforced) says
honestly whether the type system, module privacy plus a test, or the DST
holds each guarantee.

| # | Class | Examples from history | Structural cause today | Replacement |
|---|---|---|---|---|
| B1 | **Hidden host state mispredicted.** Exo drops a content type while no input method is attached, spends a pending surrounding-text support on the next content type, and detaches on its own. | `74b10a6` (support consumed by `set_content_type`); `8875c50` (content type lost on a deferred activation); `a1fc8c6` (replay replaced by reconcile); `14a2241` (Exo's own detach ignored); `08d0453` (a reused seat ID sent as a seat). | Partly fixed already by `HostApplied` and the pure `reconcile`. But six call sites of `reconcile_host` decide when to reconcile (`on_leave`, `complete_host_activation_barrier`, `reconcile_seat`, `apply_text_input_focus_updates`, `forget_text_input`, `on_commit`). `HostApplied` is also mutated outside it, in `end_on_host_detach`, `forget_text_input` and `commit` (`new_field`). | The link is a private enum whose transitions are named methods of `TextInput`. `step` reconciles every text input at the end of **every** input, so no arm can forget. |
| B2 | **Events from an ended epoch** reach the guest as current. Such events were sent before a `deactivate`, before a `reset`, or before the guest reported its text. | `3e469cd` (Space plus click gave "각각"; a preedit or confirmation in flight after a click); `dc96777` (a delete measured against Exo's predicted text); the drain barrier (`fe66096`, `a1fc8c6`). | Epoch membership is three independent fields: `host` (`Draining`), `reset_fence` and `unreported_host_edit`. Each of five handlers must check each one (`host_composition_is_current`). A new trigger adds a field and five checks. | One gate, `TextInput::current()`, returns a `Current<'_>`. Only `Current`'s methods build a transaction for a host event or a press. |
| B3 | **Leftover partial-event state.** A v1 event built from several messages left its pieces behind when dropped, and they applied to the next event. | `849a935` (a dropped commit's `cursor_position` and deletes applied to the next commit). | Pieces live in `pending_*` fields. Every early return must call `discard_pending_*` (`drop_host_preedit`, `drop_host_commit`). | `take_assembly()` runs first in every terminal event. The assembly lives in the composition, which every exit from `Attached` (`TextInput::leave_attached`) clears. |
| B4 | **Serial drift.** A v3 `done` serial must count every guest `commit`. | REVIEW2 H6 (unfocused commits were not counted); INVARIANTS "Every v3 commit counts". | Fixed by a private counter (`TextInputState::count_guest_commit`), but `done` events are built in eight places. | `CommitCount` lives in its own module and has one call site that bumps it. `Serial` comes only from it. One encoder reads it. |
| B5 | **Malformed v3 transactions.** A transaction lacks its `done`, a `done` fails to repeat the shown preedit, or the preedit is cleared or kept wrongly. | `51ffc5e` (no `done` after a guest commit); `9bc1f37` (two copies of the commit of a shown preedit). | v3 events are pushed by hand in eight places: `on_preedit_string`, `on_commit_string`, `on_leave`, `on_delete_surrounding_text`, `on_set_preedit_region`, `on_confirm_preedit`, `commit_shown_preedit` and `answer_guest_commit`. | `GuestTxn::encode` is the only builder of the four v3 IME events. It always ends with `done`, and it restates the preedit. |
| B6 | **A cross-concern trigger missed, or handled twice.** | `75f2c5a` (a press never ended the composition); touch (fixed by #72); INVARIANTS "Focus teardown sends `leave` itself"; `repair_destroyed_surface_focus`. | Other concerns call free functions of `text_input.rs`, and nothing lists them. | Every trigger is an `Input` variant, and `step`'s match has no wildcard. A source that is never wired (as touch was) is caught by the DST, not by the compiler. |
| B7 | **Connection-wide constraints.** | `a67f11e` (two enabled text inputs; the first stayed deaf). | A scan inside `on_commit`, and `begin_destroy` clearing the enabled bit. | `enabled` is per field, inside `FocusedField`, so ending or refocusing a field clears it by construction. The connection's enabled input is derived by a scan, never stored. One arm sets `enabled = true` and checks "at most one", and `assert_invariants` checks it again. |
| B8 | **Nondeterministic order.** | No reported bug yet, but it blocks exact replays. | `Context::text_inputs` is a `HashMap`. `reconcile_seat`, `apply_keyboard_focus_changes` and `end_composition_on_press` iterate it. | `BTreeMap` in the core. P0 (#75) made the whole crate hash-free before the switch, and clippy now rejects `HashMap` and `HashSet` (INVARIANTS Wire messages "Nothing the proxy sends depends on hash order"). |
| B9 | **Races only a reviewer finds.** | Every item in `3e469cd`'s message; HANDOVER's manual checks for #67 and #71; the crossing of a guest commit with an edit in flight (HANDOVER). | No executable model of Exo's asynchronous engine or of real clients. The fuzzer's `exo.rs` models only activation and content type. | The DST. |

### 1.2 Why the structure produces them

The bridge is two reconciliations. Today they are written as
event-by-event translation.

- **Guest → host.** The host's state should follow from the guest's
  committed state. #61 to #64 moved this direction to `reconcile`, which
  is why B1 stopped recurring in content types. What is left of B1 is
  the six call sites deciding *when* to reconcile.
- **Host → guest.** What the guest shows, its preedit and its text,
  should follow from the host's composition events, but only for events
  of the current epoch. This direction is still translated event by
  event, with the epoch checks repeated in each handler. That is why B2,
  B3 and B5 recur.

Cross-concern inputs (B6) and asynchrony (B9) multiply the paths.
Without a model that runs them, the paths are reviewed rather than
tested.

### 1.3 Predicted findings

The DST should find or measure these automatically.
[4.8](#48-the-known-issue-list) covers how a known one is handled.

- **P1. Focus loss while composing** (user-intent oracle; a known issue
  until C5, which fixed it: the guest commits the preedit before the
  keyboard's `leave`, F1).
  - The guest shows "가", and the user switches windows.
  - Exo's detach makes its input method confirm the composition, and
    that confirmation comes before the text-input `leave` (E6) [F].
  - `wl_keyboard.leave` normally comes before both (E12) [F]: a
    keyboard bound before the text input's activation hears first.
    Revision 2 had this order as unknown, and the fuzzer drew it.
  - So the field reset runs first. It clears the composition without
    telling the guest (`TextInputState::apply_focus`), and the drain
    then drops the confirmation. The proxy never sends the syllable.
  - Whether the user keeps it depends on the client. GTK's model does
    not confirm on focus-out, so it loses it. Chromium's blur resets its
    input method, which confirms the composition locally (C11), if blur
    follows the keyboard focus and calls `ResetContext` (U13, U14).
- **P2.** Retired: a tap now ends the composition (#72).
- **P3. Engine latency** (counted by the exact oracle, never failed).
  - A key pressed just before a click can double the syllable: the
    engine's result for it arrives after the fence (INVARIANTS "A press
    ends the composition on its surface", last paragraph).
  - The exact oracle applies such results as the specification says,
    and the DST counts them.
  - Cases checked against user intent never click inside the window.
- **P4. A Chromium report crossing an edit in flight** (HANDOVER,
  deletes). It needs the engine's deletes, which D6 adds. The DST
  counts the reports that cross an edit; a delete (H9) and a region (X1)
  are measured in Exo's text, which holds the edit, or dropped.

---

## 2. Behavioural specification

This is the contract.

- **Core row tests.** Each row of this section is a row test of the
  core. A row test is named `row_<id>_<what>`, or it keeps the name of
  an INVARIANTS-cited test (5.4).
- **DST models.** The Exo and client models implement the E and C facts
  of 2.4 and 2.5.
- **Exact oracle.** It implements the text semantics of 2.2 and 2.3.

### Conventions

- **Count.** The text input's guest commit count (B4).
- **Txn{…}.** One v3 transaction ([3.2](#32-the-guest-transaction-and-effects)).
  The fields say what the guest applies: `delete`, `commit` and
  `preedit` (none means "no preedit after this `done`").
  - **Canonical wire order.** It is mutter's and KWin's order. #76
    and #80 made the old code send it, and INVARIANTS "A v3
    transaction's events go in one order" owns the rule and its
    citations. Each event goes out only under its condition:
    1. `preedit_string("", 0, 0)`, only when the guest shows a preedit
       and the transaction clears it, deletes or commits;
    2. `delete_surrounding_text`, only if it deletes something (not
       `(0, 0)`);
    3. `commit_string`, only if its text is non-empty;
    4. `preedit_string` with the new preedit, only if it is non-empty;
    5. `done(count)`, always.

    A transaction that replaces a shown preedit with another, and edits
    no text, sends only the new one. No transaction has both a delete
    and a commit yet: Exo's deletes stand alone (E10).
  - **No-op rules.** A transaction carries no event that changes
    nothing, because some clients delete the selected text on one [F]:
    - GTK emits `preedit-start` for an empty preedit over none
      (`text_input_preedit_apply`), and GTK4's `GtkText` and
      `GtkTextView` delete the selection on that signal.
    - GTK emits `commit` for `""`, and those widgets delete the
      selection before inserting it (`gtk_text_enter_text`,
      `gtk_text_view_commit_text`).
    - Chromium's `ZwpTextInputV3Impl::OnDone` checks only that a delete
      arrived, so `delete(0, 0)` reaches
      `InputMethodAuraLinux::OnDeleteSurroundingText`, which deletes the
      selection.

    A transaction left with nothing to send is a bare `done(count)`.
  - **An empty preedit is always `(0, 0)`.** [F] Chromium applies a
    `done`'s preedit only when it differs from the one it last applied,
    and a missing one counts as `("", 0, 0)`. So after an empty preedit
    with the hidden cursor `(-1, -1)`, the next bare `done` would apply
    an empty composition again, and `UpdateCompositionIfTextSelected`
    would delete the selection. In the core, an empty preedit cannot be
    represented: `Preedit::new` returns `None` for empty text (3.2).
  - **The G9c answer repeats an unchanged preedit.** A `done` without a
    preedit clears it, so the answer to a guest commit re-sends the
    shown preedit (step 4), unchanged and with no clear before it.
  - **Client evidence for the order** [F]:
    - **Order-neutral.** Chromium, GTK3, GTK4, Qt and kitty store each
      event and apply the transaction at `done`: delete, commit, then
      preedit. The sources are Chromium `ZwpTextInputV3Impl::OnDone`,
      GTK `text_input_done` (`imwayland.c`, `gtkimcontextwayland.c`),
      Qt `QWaylandTextInputv3::zwp_text_input_v3_done` and kitty
      `text_input_done`. GTK converts a delete's byte lengths when the
      delete arrives, against the surrounding text the application last
      set, and no event of the transaction changes that text.
    - **Order-sensitive.** winit (`master` and 0.30.12) drops the
      pending preedit at a `commit_string`, so a new preedit must come
      after any commit. SDL3 `main` acts on each event as it arrives and
      turns a delete into a Backspace, which its `testime` ignores while
      a preedit shows. So the clear must come before the delete, and a
      region sent preedit first would leave "나나".
  - **Wire differences from the old code: none left after #80.** T1
    was done in two PRs:
    - #76 (merged) put a reset commit after its preedit clear, and a
      region's delete before its new preedit.
    - #80 (in review) sends no empty preedit to a guest that shows
      none, no `delete(0, 0)` and no `commit_string("")`. It gives an
      empty preedit the cursor `(0, 0)`, and it clears a shown preedit
      before a region's delete.

    After #80 the old code sends the canonical form, so the switch
    changes nothing on the wire.
- **Reconcile.** It computes the `HostTarget` from the committed guest
  state:
  - the field is enabled and focused, on a surface the host knows;
  - there is a live host seat;
  - the content type, with its support.

  It then compares the target with the link and sends only the
  difference (INVARIANTS "The host gets each content type once it can
  apply it, and only then"). A reconcile's host requests go in one
  order: the activation step, `set_surrounding_text`, the content type
  with its support, `set_cursor_rectangle`, then `commit_state(count)`.
  The last is sent only after a guest commit or a content type. **Every
  `step` ends with a reconcile of every text input** (3.4). A row's
  "Host requests" column therefore lists what that reconcile sends
  because of the row.
- **States** (those of [3.1](#31-state)):
  - field: `Unfocused` or `Focused(f)`, where `f.enabled` is the
    committed enable;
  - link: `Detached`, `Attached{ct}` or `Draining{b}`;
  - fence: `None` or `Some(f)`;
  - composition: `shown`, the preedit the guest shows (`None` or
    `Some(p)`), plus the v1 assembly.
- **Gate.** The link is `Attached` and the fence is `None`.
- **Lease.** The Backspace repeat leased to the field. The keyboard
  concern owns it ([3.3](#33-the-boundary-with-the-rest-of-the-proxy)).
- **"On encode failure."** What happens when a string does not fit one
  message ([3.5](#35-failure-rules)).

### 2.1 Guest requests

| ID | Request | Precondition | Host requests | Guest events | Next state / notes | On encode failure | Sources |
|---|---|---|---|---|---|---|---|
| G1 | `zwp_text_input_manager_v3.get_text_input(id, seat)` | A host v1 manager exists (otherwise the request is dropped, with an error log, in the adapter) | `create_text_input`; `get_extended_text_input` if the extension is bound | `enter(surface)` if the seat's keyboard focuses a surface | `Focused` (not enabled), or `Unfocused`; link `Detached`; count 0. An Exo mirror (`ExoView`) only from v11 of the extension (row test `row_G1_a_text_input_has_a_mirror_from_v11`). | n/a (no strings) | `TextInputManagerV3Handler::on_get_text_input`; INVARIANTS Object lifetimes "Synthetic objects complete their own lifecycle" |
| G2 | `zwp_text_input_manager_v3.destroy` | none | none | `delete_id`, sent by the generated local destructor path, not by the core | none | n/a | same |
| G3 | `enable` | `Focused` | none | none | `pending = V3Pending::reset(true)`. Ignored while `Unfocused`. | n/a | `TextInputV3Handler::on_enable`; `V3Pending::reset` |
| G4 | `disable` | `Focused` | none | none | `pending = V3Pending::reset(false)`. Ignored while `Unfocused`. | n/a | `on_disable` |
| G5 | `set_surrounding_text(t, c, a)` | `Focused` | none | none | If `t` is UTF-8 and `c` and `a` are char boundaries within it, pending becomes `Some(t, c, a)`. Bad offsets are ignored, with a warning. Text that is not UTF-8 makes it `Some("", 0, 0)`. | n/a (checked at G9) | INVARIANTS Wire messages "A string that is not UTF-8 does not end the session"; `on_set_surrounding_text` |
| G6 | `set_text_change_cause` | any | none | none | Ignored. Chromium never sends it [F]. GTK sends it after every surrounding text [F]. See Q3. | n/a | `on_set_text_change_cause` |
| G7 | `set_content_type(h, p)` | `Focused` | none | none | pending `content_type` | n/a | `on_set_content_type` |
| G8 | `set_cursor_rectangle` | `Focused` | none | none | pending `cursor_rect` | n/a | `on_set_cursor_rectangle` |
| G9a | `commit` | `Unfocused` | none | none | Count += 1. Nothing else. | n/a | INVARIANTS "Every v3 commit counts" |
| G9b | `commit` whose pending surrounding text does not fit a v1 message | `Focused` | **none at all**: no activation step and no content type for this commit | If the field was already enabled: `Txn{preedit: shown}` (the answer) | Count += 1. **The pending state is consumed and nothing else changes**: the committed state, the link and the composition stay. *The old code kept the pending state, so every later commit retried the same text and failed again; it is fixed to match #82 (5.1).* | *(this row is the failure case)*. If the answer's repeated preedit does not fit either, the shown preedit is forgotten and a bare `done(count)` goes out. | `on_commit` (the `queue_or_drop` early return); `answer_guest_commit`; tests `oversized_surrounding_commit_does_not_partially_advance_state`, `the_commit_after_an_oversized_one_does_not_retry_it` and `an_answer_that_cannot_repeat_the_preedit_forgets_it` |
| G9c | `commit` | `Focused` | The reconcile, with this commit's `set_surrounding_text` and `set_cursor_rectangle`, then `commit_state(count)` | If the field is enabled after the commit: `Txn{preedit: shown}` | See the four steps below this table. | If the repeated preedit does not fit, the shown preedit is forgotten and a bare `done` goes out (as in G9b). | INVARIANTS "One text input on a connection is enabled at a time", "Every commit that leaves a field enabled gets a done" and "The host gets each content type…"; mutter (`text_input_commit_state` in `meta-wayland-text-input.c`) and KWin (`textinput_v3.cpp`, "Gtk … expect done to be sent after every commit") [F] |
| G10 | `zwp_text_input_v3.destroy` | any | The reconcile to inactive: `deactivate` and a barrier while attached with a live seat; otherwise nothing. Then the extension's `destroy`, and the extension is retired. Nothing goes to a v1 object the host deleted. | `delete_id`, from the generated path | The text input is removed, **with its `enabled` flag** (B7, C-1). The lease ends. A later completion of its barrier or fence is ignored. | n/a | INVARIANTS Object lifetimes; `forget_text_input`; `TextInputState::begin_destroy` |

**The four steps of G9c:**

1. **Count += 1.**
2. **Enable.**
   - An enable while another text input on the connection is enabled
     is ignored: the field stays disabled and counts as a new field. The
     rest of the commit (surrounding text, content type) still applies
     to the committed state. That is today's behaviour, pinned by a row
     test (REVIEW2; Q5).
   - Otherwise `new_field` is whether `enabled` changes. An enable of an
     enabled field keeps the composition and the host's content type.
3. **A new field** ends the lease. It leaves the field disabled or
   newly enabled, and neither can stay attached: a disable makes the
   step's reconcile leave `Attached`, which ends the composition and
   the assembly with no guest output, since the guest reset itself; an
   enable, or a rejected one, starts from a link that was not attached
   (I1a), so there is no composition and no content type to unset.
   Revision 2 cleared these here explicitly; #82 gets the same result
   from the reconcile (3.1).
4. **A stated surrounding text** becomes the committed one, which the
   reconcile forwards to Exo, glued to a barrier at whose `done` the
   Exo mirror applies it (or withholds it, below). Then the content type
   is applied.

**Report suppression** (with the Exo mirror, from v11; design
exo-mirror 5). The reconcile
withholds the commit's `set_surrounding_text`, and its barrier, when the
guest stated it (not the empty stand-in), the mirror is `Exact`, the
report names no state of the history Exo will hold once it has processed
the requests in flight, and one such state is the report with the
composition the guest shows inline at the report's cursor: Exo, which
holds every state with the composition inline, would only drop it (E4).
A report Exo matches changes nothing but its history, so withholding
one that matches only with the preedit leaves Exo where it would be had
the guest reported the preedit. The content type, the cursor rectangle
and `commit_state` still go out, and the guest is in step as after a
matched report, at that state; withholding changes nothing but the
core's view of the guest (`ExoView`), and an older report still in
flight can take the guest out of step only by resetting Exo. Below v11
every report goes out. Any other report goes out unchanged: one
that names a state is a no-op, and one that names none resets Exo to the
guest's text. A wrong mirror can only leave Exo stale, never inject
text, until the composition ends and the next report goes out. Row tests
`a_report_without_the_shown_preedit_is_withheld`,
`row_G9c_an_exact_report_goes_out_unchanged`,
`row_G9c_an_unmatched_report_goes_out_unchanged`,
`row_G9c_an_older_report_matched_after_a_withheld_one_keeps_it_in_step`.

If the field's support for surrounding text flips, the content type is
re-sent with the new support (test
`surrounding_text_support_follows_the_field_and_resends_only_on_change`).

### 2.2 Host events

Events from a host object that no live text input owns are dropped.
None of these rows sends a host request of its own, beyond the
reconcile that ends every step.

| ID | Event | Precondition | Guest events | Next state / notes | On encode failure | Sources |
|---|---|---|---|---|---|---|
| H1 | v1 `enter(surface)` | any | none | No change. While attached, it answers the proxy's own `activate`. | n/a | INVARIANTS "The host gets each content type…"; `TextInputV1Handler::on_enter` |
| H2 | v1 `leave` | `Attached` | If `shown = Some(p)`: `Txn{commit: p, preedit: none}` (**C5, done**; it was `Txn{preedit: none}`). Exo's detach confirms first (E6), which commits the shown preedit at v11 or later, and below v11 if Exo tracks the composition, so a preedit still shown is one Exo no longer tracked (E4): nothing confirmed it, and the guest commits it, so a restart of the input method keeps the user's text. Nothing is committed twice. | Link → `Detached` (Exo's own detach). The commit, as at X2, ends the lease; the composition and the assembly are cleared. The fence stays. The reconcile then activates again if the field still wants input. | A commit that does not fit clears the preedit as before C5. | INVARIANTS same rule; Exo `TextInput::OnSurfaceFocused`, `OnSurfaceDestroying` and `OnInputMethodDestroyed` [F]; `host_leave`; tests `row_H2_an_exo_detach_commits_the_preedit_and_activates_again`, `row_H2_a_confirmed_composition_is_committed_once`, `an_exo_detach_commits_the_guest_preedit` |
| H2' | v1 `leave` | `Draining` or `Detached` | none | Ignored: it answers the proxy's own `deactivate`. | n/a | same |
| H3 | `modifiers_map`, `input_panel_state`, `language`, `text_direction` | any | none | Ignored. | n/a | the handlers of the same names |
| H4 | `preedit_styling(i, l, style)` | `Attached` (dropped otherwise, as today) | none | Recorded in the assembly when `style == SELECTION`. | n/a | `on_preedit_styling` (records only while `host_is_active`) |
| H5 | `preedit_cursor(i)` | `Attached` | none | Recorded in the assembly. | n/a | `on_preedit_cursor` |
| H6 | `cursor_position(i, a)` | `Attached` | none | Logged at debug as having no v3 equivalent, and not recorded (today it is recorded and only logged later). Exo sends it only before a `commit_string` [F]. | n/a | `on_cursor_position` |
| H7 | `preedit_string(serial, t, commit)` | Gate; `t` is UTF-8 | If `t` is empty, `shown` was set and `commit` is non-empty: `Txn{commit, preedit: none}` (the reset commit; Exo never sends one, as its commit text is always the preedit's, so the Exo mirror diverges at it). Otherwise: `Txn{preedit: t, with the cursor from the assembly}`. An empty `t` with nothing shown is a bare `done(count)`, whatever `commit` holds (#80). | `shown := t`, or none if `t` is empty. The assembly is taken. **Lease:** `LeaseOutcome::ArmIfBackspaceHeld` if `t` is empty, `shown` was set and `commit` is empty; `End` otherwise. The host serial is only logged. | The event is dropped. The assembly is taken. Nothing else changes. | `on_preedit_string`; Exo sends the preedit text as its commit text [F]; tests `a_reset_commit_waits_for_the_guest_to_report_it`, `a_reset_commit_waits_for_the_guest_to_report_it` |
| H7' | `preedit_string` | Gate fails, or `t` is not UTF-8 | none | Dropped. The assembly is taken. | n/a | INVARIANTS press rule (the fence); Wire messages (byte-indexed strings) |
| H8 | `commit_string(serial, t)` | Gate | `Txn{commit: t, preedit: none}` | `shown := none`. An empty commit is not sent, so the guest has no edit to report, shown preedit or not (#80; tests `row_H8_an_empty_commit_is_no_edit` and `row_H8_an_empty_commit_over_a_preedit_is_no_edit` in #82). The lease ends. The assembly is taken. | Dropped; the assembly stays (B3, below H9). | `on_commit_string` |
| H9 | `delete_surrounding_text(index, length)` | Gate | `Txn{delete: (before, after), preedit: none}` | Measured in the Exo mirror's state before the event (`exo_view::Frame`) while the text input has a mirror (from v11 of the extension), the mirror was exact before the event and is after it, the guest has stated every key and press, no report on its way to Exo crosses the event, and the guest is in step with Exo (INVARIANTS "Exo's deletes and regions are measured in the Exo mirror, or dropped"): a range around Exo's cursor, with no composition, becomes `(cursor − start, end − cursor)`; `shown := none`; the lease ends; the assembly is taken. An empty range sends nothing and keeps the guest in step. **Dropped** otherwise, ending the lease as Exo edited (but for an empty range), with a debug log that names why (`exo_view::Unmeasured`; `Unsure` for a mirror that was unsure before the event, even one the event makes exact), and the guest is then out of step until its next report; below v11 the first drop warns. A negative index, a range not around Exo's cursor and one off its text's char boundaries are none Exo sends, so the mirror diverges at them. Row tests `row_H9_an_absolute_delete_becomes_lengths_around_the_cursor`, `row_H9_a_delete_after_an_unreported_edit_lands_in_exos_text`, `row_H9_after_set_editable_selection_range_drops`, `row_H9_after_an_empty_region_with_exos_cursor_inside_drops`, `row_H9_after_a_dropped_region_drops`, `row_H9_after_a_delete_over_a_shown_preedit_drops`, `row_H9_a_field_that_states_no_text_drops`, `row_H9_after_a_forwarded_key_drops`, `row_H9_in_step_again_after_a_matched_report`, `row_H9_a_delete_that_heals_an_unsure_mirror_drops_as_unsure`, `row_H9_X1_below_v11_deletes_and_regions_are_dropped`. *B3, as the old code does it: a preedit and a region (H7, X1) take the assembly first, whatever becomes of them; a commit and a delete (H8, H9) take it only once they reach the guest, so a stale or dropped one leaves it; a confirmation (X2) takes it once the gate admits it with a preedit shown, so a stale or empty one (X2') leaves it; a press (F4) takes it once its commit reaches the guest, so one whose commit does not fit leaves it. #82 took it at every terminal event and at every press; C3b and C3c made the core match the old code, which it was to replace without a wire change (row tests `row_H8_H9_X2_only_an_edit_that_reaches_the_guest_takes_the_assembly` and `row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly`). Exo sends styling and a cursor only right before their preedit [F], so no client sees the difference.* | Dropped; the assembly stays. | INVARIANTS "Exo's deletes are absolute and stand alone"; `on_delete_surrounding_text`. [F] Chromium drops a delete while a composition is shown (`InputMethodAuraLinux::OnDeleteSurroundingText`). |
| H10 | `keysym(...)` | `Attached` (the fence is **not** checked) | `wl_keyboard.key`, through the keyboard concern | Not a core input. `ime_keys` asks `ImeCore::keysym_target(ti)`. Unchanged. Exo sends none today [F, U1]. | n/a | `on_keysym`; INVARIANTS "A key waits for at most one late release per channel" |
| X1 | ext `set_preedit_region(index, length)` | Gate; measured as H9 is | `Txn{delete: (before, after), preedit: (region, cursor)}` | Measured in the mirror's state before the event, under H9's conditions: `G` = the region from Exo's selection end, in Exo's text, which holds its composition inline (E4); an empty `G` commits the composition Exo holds, which the guest shows (`Txn{commit: p, preedit: none}`), or else edits nothing; a `G` that covers the composition, or Exo's cursor with none, gives `Txn{delete: (H.start − G.start, G.end − H.end), preedit: G}`, with the preedit cursor from the assembly or Exo's cursor: it deletes only the bytes beyond the composition on each side, which the guest has, and shows them with it (#85's region over a region is the case where the composition is the last region). `shown := G`, or none; the lease ends. The assembly is taken. A `G` that does not cover the composition, one short on either side included, is dropped with a debug log: Exo would leave the rest of the composition as text. A dropped region, measured or not, ends the lease, as Exo edited; and Exo ends its composition at an empty one whether or not the core can measure it, so a dropped empty region with a preedit shown commits it as an empty `G` does (`Txn{commit: shown}`), which may double text but never loses it. Row tests `row_X1_a_preedit_region_turns_text_into_the_preedit`, `row_X1_an_unmeasured_empty_region_commits_the_shown_preedit`, `row_H9_X1_a_dropped_delete_or_region_ends_the_lease`, `row_X1_a_region_after_an_unreported_commit_is_measured_in_exos_text`, `row_X1_a_region_after_an_e4_drop_holds_no_preedit`, `row_X1_a_region_made_before_a_report_is_measured_before_it`, `row_X1_a_region_crossing_a_guest_edit_drops`, `row_X1_a_mirror_that_is_not_exact_drops`, `row_X1_a_mirror_that_diverges_warns_once`, `row_X1_after_a_confirm_inside_the_composition_drops`. | Dropped; the assembly is taken. | `on_set_preedit_region`; Exo's region is cursor-relative [F]; `SurroundingTextTracker::OnSetCompositionFromExistingText` keeps an empty composition [F]; tests `on_set_preedit_region_translates_correctly`, `a_region_over_a_region_drops`, `a_region_over_a_host_preedit_is_measured_from_exos_cursor`, `a_region_after_a_host_preedit_deletes_only_what_the_guest_has`, `a_region_that_does_not_cover_a_host_preedit_is_dropped`, `a_region_that_does_not_cover_the_shown_one_is_dropped`, `an_empty_region_over_a_held_preedit_commits_it`, `an_answer_that_cannot_repeat_the_preedit_forgets_it` |
| X1' | ext `set_preedit_region` | any other case | none | Dropped, as H9 drops; the assembly is taken. | n/a | same |
| X2 | ext `confirm_preedit` | Gate; `shown = Some(p)` | `Txn{commit: p, preedit: none}` | `shown := none`; the lease ends; the assembly is taken. | Dropped; the assembly is taken. (`p` was encoded once, so this cannot happen in practice.) | `on_confirm_preedit`; Exo at v11 or later confirms instead of committing [F] |
| X2' | ext `confirm_preedit` | Gate; `shown = None` | Through the keyboard concern: if the seat has an eligible held key, the recovery waits for the host's next event, which decides whether the confirmation was the key's autorepeat or Exo's before a new press (E13, E8). An autorepeat's: the key's press and release unless another channel already delivered it, then the G9c answer under the serial of then (`ImeCore::recovery_done`), if the text input still passes the gate and the key is held; otherwise nothing. **The `done` goes whenever the key is recovered, even when its pair is skipped.** A new press's, or with no held key: nothing is sent. | `KeyEffect::RecoverHeldKey`. **Decision**, before the host's next event is handled: before any peek, `wl_keyboard.modifiers` and the `peek_key` press of a key not held, or held only across a detach (H2), from any keyboard of the connection, keep it waiting; that key's `wl_keyboard.key` press on that keyboard with the peek's serial makes it a new press's, and the text input's `leave` the detach's (E6); any other event, the `done` of the sync the proxy sends with it included, an autorepeat's (`ime_keys::PendingRecovery`; INVARIANTS "An empty confirmation recovers a held key only once the host shows it repeated"; row test `row_X2prime_a_recovered_key_is_closed_under_the_serial_of_its_recovery`). Exo's own detach (H2) ends the recovery of every key the seat holds, whose releases may have died with the input method, until the key's next `peek_key` press; it ends nothing else, so the key's keysyms still reach the guest (test `a_key_held_across_a_detach_is_typed_and_recovered_again`). **Lease:** armed if a held Backspace's pair is sent; ended if a held Backspace's pair is skipped, if another held key's pair is sent, or if no key is held; left alone if another held key's pair is skipped (revision 3 corrects this from the old code). The eligible key is the held key with the highest sequence, if it is also the latest peek. A peek numbers its key when it arrives, a key held since `wl_keyboard.enter` included, which the enter had numbered in the order the host listed the keys (#75), so the order of the peeks decides (REVIEW2 "held-key peek sequence"; test `keys_held_since_enter_are_ordered_by_their_peeks`). | n/a | INVARIANTS "A recovered key carries the host's serial"; `on_confirm_preedit` with `held_repeat_key_for_seat` and `synthesize_ime_consumed_key_pair`. The old code recovered at once, so a key typed while Backspace or Space was held repeated the held key (`KnownIssue::RecoveryBeforeAPress`, retired). |
| X3 | ext grammar, autocorrect and occluded-bounds events | any | none | Ignored. | n/a | the handlers of the same names |
| W1 | `wl_callback.done` of a drain barrier | `Draining{b}` | none | Link → `Detached`. The final reconcile then activates if the field wants input. | n/a | `CallbackHandler::on_done`; `complete_host_activation_barrier` |
| W2 | `wl_callback.done` of a reset fence | `fence = Some(f)` | none | `fence := None`, **in any link state** (the fence lives beside the link, as today). | n/a | INVARIANTS press rule ("A focus change or the text input's end does not end it early"); `complete_reset_fence` |

### 2.3 Cross-concern inputs and reconcile outcomes

| ID | Input (owner that sends it) | Precondition | Host requests | Guest events | Next state / notes | Sources |
|---|---|---|---|---|---|---|
| F1 | Seat focus change: `wl_keyboard.enter` of another surface, `wl_keyboard.leave`, or a keyboard's end (`KeyboardFocusRegistry`, via `KeyboardHandler::on_enter`, `on_leave` and `forget_keyboard`) | Per text input on that seat, in `TiId` order | Through the reconcile: `deactivate` and a barrier, or `activate` | **C5, done:** first `Txn{commit: p, preedit: none}` if `shown = Some(p)` and the composition is current, then the keyboard events that moved the focus (`Input::FocusChanged::keyboard`: the guest's `wl_keyboard.leave`, or `enter` and its `modifiers`, which the keyboard handler hands the core rather than forwarding), then `leave(old)` and `enter(new)`, each if it changed. The commit goes before the keyboard events because both client models drop input-method events for a window that has lost the keyboard focus (C11, C12). | The field is reset: a new `FocusedField{surface, enabled: false, text: Synced}`, or `Unfocused`, with an empty pending state. The composition and the assembly are cleared. The keyboard ends the seat's leases and peek watermarks, as today. Exo's own confirmation of its detach comes after the keyboard's `leave` (E12), while the link drains, and is dropped. | INVARIANTS "Focus teardown sends `leave` itself", "A field the focus leaves commits its preedit first"; mutter and ibus commit a preedit at a focus-out; `apply_keyboard_focus_changes`; tests `row_F1_a_focus_change_commits_the_shown_preedit_before_leave`, `a_focus_loss_commits_the_shown_preedit_before_leave`; C5 fixes P1 |
| F2 | Surface destroyed (`forget_surface` in `compositor.rs`) | A text input focused on that surface | as F1 | `leave(s)`, while `s` is still valid. **No commit** (C5): the guest destroyed the window the text would go into. | The focus is projected again from `Env::keyboard_focus`, which the destroy has already moved off `s`. Since C5, `compositor::forget_surface` sends F2 before the F1 of each seat whose keyboard focused `s`, so that F1, which would commit, finds the field reset; F2 always sends `leave`. Old INVARIANTS described a repair that stays on the same surface, which `forget_surface` cannot produce (REVIEW2). | INVARIANTS "Focus teardown…"; `repair_destroyed_surface_focus` until C3c; tests `row_F2_a_destroyed_surface_is_left_while_it_is_valid`, `wl_surface_destroy_repairs_only_text_inputs_on_the_stale_surface` |
| F3 | `wl_seat` bound under a guest ID (`registry.rs`, `wl_seat` bind) | The text inputs of that seat | Through the reconcile: the deferred `activate`, with its content type | none | | INVARIANTS Object lifetimes "Releasing a parent does not end its children"; test `an_activation_deferred_for_an_unmapped_seat_sends_the_content_type` |
| F4 | Press: a `wl_pointer.button` press from a pointer whose `enter` named `s`, or a `wl_touch.down` on `s` (#72: `PointerHandler::on_button` and `TouchHandler::on_down`, both through `end_composition_on_press(ctx, surface, resend)`) | The enabled text input is focused on `s`; gate; `shown = Some(p)` | `zwp_text_input_v1.reset`, then a barrier (the fence) | `Txn{commit: p, preedit: none}`, then the press or down itself, re-sent by the core | `shown := none`; the assembly is taken; `fence := Some(f)`; the lease ends. A press whose commit does not fit changes nothing and is forwarded, and leaves the assembly (B3, below H9). A release, another surface (subsurfaces included), no composition, or an open fence changes nothing, and the press is forwarded. | INVARIANTS "A press ends the composition on its surface"; mutter (`meta_wayland_text_input_handle_event`), KWin (`InputMethodEventFilter::pointerButton` and `touchDown`, `InputMethod::commitPendingText`) [F]; Exo `TextInput::Reset` sends nothing back [F] |
| F5 | Key activity (`wl_keyboard.key`, `peek_key`) | n/a | n/a | n/a | **Not a core input.** Leases are keyboard state (`KeyGenerationRegistry::ime_repeat_owner`). The keyboard ends them on a non-Backspace press and on a Backspace release, as today. The core only emits `KeyEffect`s. | INVARIANTS "IME state is scoped to its keyboard or seat" |
| F6 | Host `delete_id` of the v1 object while the guest object is live (`forget_object`, with `host_deleted`) | any | Nothing to v1; the extension is destroyed | `delete_id`, as in G10 | Link → `Detached` with no requests. | test `a_host_destroyed_text_input_sends_nothing_to_its_host_object` |
| R1 | **Abandon** (reconcile outcome): the target is inactive, the link is `Attached`, and `Env::host_seat` is `None` | none | none | none | Link → `Detached` locally; the composition is cleared; **the fence stays**. Exo keeps its input method attached until its own focus moves; HANDOVER covers the cost. | `ActivationStep::Abandon`; HANDOVER "A text input whose guest `wl_seat` goes away…" |
| R2 | **Deferred activation** (reconcile outcome): the target is active, and the link is `Draining`, or `Detached` with no live seat | none | none | none | No change. The deferral is logged when it starts or changes its reason (`Deferral::Draining` or `NoSeat`), not at every step that finds it waiting (#82); a missing seat warns once per text input. | `reconcile_host` (the `None if target.active` arm); `TextInputState::warned_seat_missing` |

A seat that ends is **not** an input. Today's proxy notices it lazily, at
the text input's next reconcile, and the core does the same: the
unconditional end-of-step reconcile reads `Env::host_seat`. An `Attached`
link with a dead seat is therefore a normal state (I1 in 3.1;
INVARIANTS "Releasing a parent does not end its children").

Every step reconciles every text input, while the old code reconciles a
text input only at the inputs that change its target or its link. #82
recorded this as a deviation: the core would notice a dead seat at the
next input to *any* text input of the connection. C3b found that it
changes nothing on the wire (3.7, "Reconcile timing"). A seat's end
changes no target's surface, and an attached link with a dead seat needs
no request until its target turns inactive, which only an input that
both codes reconcile at can do (a commit, a focus change or a surface's
end); only then does the link end locally (R1).

### 2.4 Exo facts the models implement

Revision 3 settles four of revision 2's unknowns from Chromium's source
(E6, E8, E12, and U11, now E13), states the reset race of E5 and the
history replay of E4, and adds E14 and E15. Between Exo's text input and
the engine stands `InputMethodAsh` (IMA), so several facts are IMA's.
IMA buffers the engine's text for a key until the engine's callback for
that key (`ProcessKeyEventDone`).

| ID | Fact | Status, and how the DST treats it |
|---|---|---|
| E1 | `activate` on the surface it already has does nothing. Otherwise it detaches (sending `leave`), takes the surface, and attaches (sending `enter`) only if the seat focuses that surface. | [F] `TextInput::Activate` |
| E2 | Attaching resets the type and mode to text and keeps the flags, learning and support. Detaching sets the type to none. Exo detaches on its own on focus loss, on surface destruction and when the input method is destroyed, each time through `Deactivate`. | [F] `AttachInputMethod`, `DetachInputMethod` and the observers |
| E3 | A content type is dropped while nothing is attached. `set_input_type` and `set_content_type` consume the pending support and reset it to supported. | [F] `SetTypeModeFlags`; `zwp_text_input_manager.cc` |
| E4 | Surrounding text and caret bounds are kept with or without an input method. Exo predicts the text by applying the input method's edits. **A report that lags behind an edit does not lose it:** `SurroundingTextTracker::Update` matches the reported text against its history of states and replays the edits made after the matching one. | [F] `TextInput::SetSurroundingText`; `SurroundingTextTracker::Update` (`surrounding_text_tracker.cc`) |
| E5 | `reset` cancels Exo's composition (`SurroundingTextTracker::CancelComposition`) and calls IMA's `CancelComposition`. It sends nothing to the client. IMA's `ResetContext` drops IMA's buffered results (`pending_commit_`, `pending_composition_`), and it resets the engine **only if it was composing**. A key in flight at the reset therefore has **three outcomes**: (a) its text already went out, and the reset cancels its composition as usual; (b) its text is buffered in IMA, and the reset drops it; (c) the engine has not answered yet, and its answer, made from the old composition, arrives after the reset and is applied (the late answer, P3). | [F] `TextInput::Reset`; `InputMethodAsh::CancelComposition`, `ResetContext`; INVARIANTS press rule. The DST produces all three by timing; it does not draw them. |
| E6 | **A detach always confirms first.** `TextInput::DetachInputMethod` calls `DetachTextInputClient`. That reaches `InputMethodAsh::OnWillChangeFocusedClient`, which drops IMA's buffered commit and calls `ConfirmComposition(true)`, before `delegate_->Deactivated()` sends `leave`. At extension v11 or later, Exo's `ConfirmCompositionText` sends `confirm_preedit` **even with no composition**, since `SupportsAlwaysConfirmComposition` is `SupportsConfirmPreedit`. Below v11 it confirms only a tracked composition, as `cursor_position` and `commit_string`. | [F] `input_method_ash.cc` (`OnWillChangeFocusedClient`, `ConfirmComposition`); `text_input.cc` (`DetachInputMethod`, `ConfirmCompositionText`, `SupportsAlwaysConfirmComposition`). Was U3; no longer drawn. |
| E7 | A key goes through the input method before `Keyboard::OnKeyEvent`. A consumed key gets `peek_key` and no `wl_keyboard.key`. An unconsumed key gets `peek_key`, then `wl_keyboard.key` with the same serial. There are no repeats. **With no text input attached, keys skip the engine and reach the client raw**, as US-layout letters. | [F] RESEARCH Q3 (`keyboard.cc`). The raw path: [F] the input method has no client (`InputMethodAsh` with type `NONE`). |
| E8 | **The order of a key's text and its key events follows the consumed bit.** A key the engine handled gets its `peek_key` first, then its text. A key the engine did not handle gets its text first (after E13's confirmation), then its `peek_key` and `wl_keyboard.key`. | [F] IMA `ProcessKeyEventDone` and `ProcessKeyEventPostIME`. Was U2; no longer drawn. |
| E9 | When the engine commits the composition it tracks, it may use `confirm_preedit` (v11 and later) or `commit_string`. A commit of text that differs from the tracked composition, such as 각 + ㅏ committing 가, or E15's "가 ", is always `commit_string`. | [F] confirm confirms only the tracked composition. **The style of a confirmable commit is [U4]; drawn.** |
| E10 | `delete_surrounding_text` uses an absolute byte index into Exo's predicted text, comes with no `commit_string`, and happens only while nothing is composed. | [F] |
| E11 | Exo handles one client's requests in order, and its events to a client are in order. A `wl_display.sync`'s `done` follows every event of earlier requests and precedes every event of later ones. | [F] libwayland |
| E12 | **On a focus change, the seat tells its observers in the order they registered.** The keyboard and the text input both observe at priority 1 (`kKeyboardSeatObserverPriority`, `kTextInputSeatObserverPriority`). A keyboard registers when it is created. A text input registers at its first `activate` after a `Deactivate`, which unregisters it. So a keyboard bound before that `activate` hears first: `wl_keyboard.leave` and `enter` come before the text input's detach (E6's confirmation, then `leave`) and its attach. | [F] `Keyboard`'s constructor (`keyboard.cc`); `TextInput::Activate`, `Deactivate` and `OnSurfaceFocused` (`text_input.cc`); `Seat::AddObserver` (`seat.h`). Was U3; no longer drawn. A DST case binds its keyboard before the first `activate`, so the keyboard always goes first. |
| E13 | **An empty `confirm_preedit` comes before every key the engine did not handle**, at extension v11. For a press the engine did not handle that is not a modifier, IMA's `ProcessKeyEventDone` calls `ConfirmComposition(true)` before it passes the key on, and at v11 that confirms even with nothing composed (E6). This includes autorepeats, which `Keyboard::OnKeyEvent` then drops (E7). It is X2's source. | [F] IMA `ProcessKeyEventDone`. Was U11. The captured trace agrees: a held Space's repeats arrive only as empty `confirm_preedit`s (test `held_space_repeats_after_ime_commit_until_physical_release`). |
| E14 | Exo advertises `zcr_text_input_extension_v1` at **v14**. The proxy binds at most v11, so on a current host the extension is v11 and confirms with `confirm_preedit`. | [F] `GetTextInputExtensionV1Version` (`wayland_server.cc`). The DST still draws none, 7, 10 or 11, for older hosts. |
| E15 | **The Korean engine consumes Space.** With "가" composed, Space is handled, and the engine commits "가 ", the Space included, with `commit_string` (E9). | [F] captured host trace (test `held_space_repeats_after_ime_commit_until_physical_release`). D2's engine is to model it (4.4). |

### 2.5 Client facts the models implement

Revision 3 corrects C2, C3, C4 and C6 and adds C10 to C14. The client
models of D3 (#79) established them; each model cites its sources.

| ID | Fact | Client | Source [F] |
|---|---|---|---|
| C1 | Counts commits. Sends the cursor rectangle, surrounding text and content type (each deduplicated) **only** when `last_done_serial == commit_count`, then one `commit`. Retries after every `done`. | Chromium | `ZWPTextInputV3::DoneSerialEqualsCommitCount`, `SendPendingImeData`, `OnDone` |
| C2 | `enable` and `disable` are not gated: `enable` sends the content type, the cached text and rectangle, and a `commit`; `disable` sends a `commit`. The text input is enabled for a window only while the window is both focused (C11) and entered. A focus change clears what the text input was given, so an enable after a refocus carries no text. A change of focused node disables, then enables. There is **no** enable on a click in the same field. | Chromium | `ZWPTextInputV3::Enable`/`Disable`; `WaylandInputMethodContext::MaybeUpdateActivated`, `UpdateFocus` |
| C3 | At `done`, regardless of serial, it applies the delete (**dropped if a composition is shown**; clamped to the predicted text, or dropped if it splits a character), then the commit, then the preedit (a missing preedit clears it). Through C1 it then reports **the text its context predicts**, from Chromium's own `SurroundingTextTracker`, which keeps edits the window's input method dropped. The window re-reports its own text only when that changed. | Chromium | `ZWPTextInputV3::OnDone`; `InputMethodAuraLinux::OnDeleteSurroundingText`; `WaylandInputMethodContext::OnInputEventsApplied`; `surrounding_text_tracker.cc` |
| C4 | **Every press resets.** Every mouse press in a focused window resets the text input, not only one that ends a composition. The reset confirms a shown composition locally, sends nothing, and only then arms the guard: **commits and preedits are dropped for 100 ms**. The guard belongs to each window, since each window has its own input method, and any key ends it. **A touch confirms when it lifts as a tap**, not at the touch-down. With F4, the preedit is already gone at the press, so the guard arms only if F4 did not run. | Chromium | `RenderWidgetHostViewEventHandler::FinishImeCompositionSession`; `kIgnoreCommitsDuration`, `InputMethodAuraLinux::ResetContext`; `RenderWidgetHostViewAura` (gestures) |
| C5 | Counts deletes in bytes from the composition, or else the selection, and deletes the selection too. | Chromium | `WaylandInputMethodContext::OnDeleteSurroundingTextAroundCursor` |
| C6 | `serial` +1 per commit. At `done`, whatever the serial, it applies delete, commit, then preedit (a missing preedit clears it). **It answers a `done` with its state and a `commit` only when the `done` changed something:** always after an applied delete or commit; after a change to the preedit alone, only when the serial equals its commit count; never after an empty `done`. | GTK3, GTK4 | `text_input_done`, `notify_im_change` |
| C7 | Sends `enable`, with its full state and a `commit`, on `focus_in`/`enter`, and again on a single-click **release** that did not drag. GTK3 does so only if the caret moved less than its own height; GTK4 always, since the v3 protocol at v1 has no `show_input_panel`. | GTK3, GTK4 | `released_cb` (GTK3); `gtk_text_click_gesture_released`, `activate_osk_with_event` (GTK4) |
| C8 | On a primary press, before the cursor moves, GTK4 always resets; GTK3 resets only when `need_im_reset` is set and the cursor moves. A reset is the state plus a `commit` with cause `other`. The preedit is not cleared locally. GTK's gestures take a touch as a press. | GTK3, GTK4 | `gtk_text_click_gesture_pressed`; `gtk_entry_real_set_position`; `reset` |
| C9 | Deletes count bytes from the cursor and ignore the selection. GTK converts them to characters of the text the context last retrieved and applies them at the entry's cursor, clamped. Surrounding offsets are bytes; GTK3 sends `anchor = cursor`. | GTK3, GTK4 | `text_input_delete_surrounding_text`, `notify_surrounding_text` |
| C10 | **One text input per connection**, shared by all windows, and one input method per window. | Chromium | `WaylandConnection::EnsureTextInputV3` |
| C11 | **Blur resets and disables.** A window that loses the keyboard focus loses its input method's client. The input method resets, which arms the guard if it composed (C4). The text input disables without a report, and later input-method events for that window are dropped. | Chromium | `WaylandToplevelWindow::UpdateActivationState`; `RenderWidgetHostViewAura` (blur); `InputMethodAuraLinux`. Host checks U13 and U14 (6.2). |
| C12 | One text input per display, and one input method context per field: the context of the focused field is current, receives every event, and alone sends requests. GTK3 sends requests only between its own `enable` and `disable`; GTK4 whenever its context is current. GTK3 states a surrounding text that changed twice in one report. GTK4 reports the caret it last laid out, and a caret that moved after layout goes out with a `commit` of its own. | GTK3, GTK4 | `GtkIMContextWaylandGlobal`; `after_layout_cb` (GTK4) |
| C13 | **Key-binding resets.** A passed Backspace that deletes, Left and Right reset the context; Enter does so when the entry needs a reset, without inserting a line break. GTK3 needs a reset after a printable key; GTK4 does not. | GTK3, GTK4 | `gtkentry.c`, `gtktext.c` |
| C14 | **Backspace leaves the start of a Hangul syllable.** It deletes the grapheme before the cursor and re-inserts all but the last character of its NFD decomposition: a passed Backspace after 가 leaves the conjoining jamo ᄀ, and after 각 the conjoining sequence U+1100 U+1161, not the precomposed 가. | GTK3, GTK4 | `gtk_entry_backspace` (GTK3), `gtk_text_backspace` (GTK4), with Pango's `backspace_deletes_character` |

---

## 3. The IME core

### 3.0 File layout

#82 implements C1 and C2 in this layout. The line counts are #82's,
with license headers and doc comments; revision 2's budgets are in
brackets.

```
sommelier/src/ime/
  mod.rs      Public API: ImeCore (step, keysym_target,
              assert_invariants), Input, HostIme, Effects, HostOut,
              KeyEffect, LeaseOutcome, Env, the ID newtypes, HostTi,
              HostExt. Re-exports the helpers the old handlers share
              until the switch.                                  299 [~170]
  state.rs    TextInput, Field, FocusedField, LinkState, Composition,
              Assembly, Warned, Current<'_>, Planned<'_>.
              The fields are private to this file, and transitions
              are named methods of TextInput.                    659 [~300]
  serial.rs   CommitCount and Serial: no public constructor, and one
              bump.                                               49 [~40]
  step.rs     step(): one exhaustive match on Input; the end-of-step
              reconcile; removals; the replayable trace log.     588 [~420]
  host.rs     HostTarget, target(), reconcile() over LinkState, and
              the content-type mapping and requests
              (map_v3_content_type, content_type_requests,
              takes_surrounding_text_support), moved from
              handler/text_input.rs.                             278 [~320]
  guest.rs    Preedit, GuestTxn, encode(): the only builder of the v3
              preedit_string, delete_surrounding_text, commit_string
              and done. Also resolve_preedit_cursor, copied until
              #80 merges.                                        202 [~200]
  mirror.rs   ExoMirror: Exo's SurroundingTextTracker, driven by the
              wire (M1, specs/exo-mirror.md).
  exo_view.rs ExoView: a text input's mirror, from v11 of the
              extension, and whether the guest is in step with Exo;
              Frame, which measures X1 and H9, and Unmeasured, why
              the core drops one (M2, M3; design single-exo-model).
  tests.rs    One or more row tests per row of section 2, the encoder
              table, and the two source tests: pure, no Session.
                                                          1,542 test [~900]

handler/text_input.rs   Adapters: decode, build an Input, call step,
                        apply the Effects.                             ~300
handler/ime_keys.rs     Keysym routing, held-key recovery and Backspace
                        leases, moved out of text_input.rs (PR K1).    ~400 (moved, production only)
```

`ime` imports nothing from `handler`. From the rest of the crate it uses
only `wire` (`Outgoing` and `IndexedString`) and the generated
`protocols` encoders and constants. Until C3c it also shared three types
with the old code, `state::{HostContentType, V3Committed, V3Pending}`;
C3c moved them into `ime/host.rs` and `ime/state.rs`. Two source tests in
`ime/tests.rs` enforce this:

- **`ime_is_isolated`** checks that:
  - every `use crate::` line of `ime/` names only `protocols::` or
    `wire::`;
  - nothing in `ime/` names `crate::handler`;
  - the four v3 event builders (`zwp_text_input_v3::evt::{preedit_string,
    delete_surrounding_text, commit_string, done}`) appear only in
    `guest.rs`, among the files of `ime/` and, since C3c, in the whole
    crate, which no other file names or imports them in.
- **`count_bumps_only_in_the_commit_arm`** checks that
  `CommitCount::bump` has one call site, reached from the Commit arm
  through `TextInput::count_commit`.

### 3.1 State

```rust
// ime/mod.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct TiId(pub u32);      // guest zwp_text_input_v3
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct SeatId(pub u32);    // guest wl_seat ID (may be dead)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct SurfaceId(pub u32); // guest wl_surface

/// A host wl_display.sync the core waits for. The core numbers its own
/// barriers and never reuses a number. The adapter keeps the one map
/// from token to host callback (InternalCallbacks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct BarrierToken(u64);

/// The host objects behind a guest text input, fixed at its creation.
#[derive(Clone, Copy, Debug)] pub struct HostTi { pub v1: u32, pub ext: Option<HostExt> }
/// The extension carries the manager it is created from, so an extension
/// without a manager, or the reverse, cannot be expressed.
#[derive(Clone, Copy, Debug)] pub struct HostExt { pub id: u32, pub version: u32, pub manager: u32 }

#[derive(Debug, Default)]
pub struct ImeCore {
    inputs: BTreeMap<TiId, TextInput>,
    /// Extension object to text input. Updated only by Create and by
    /// removal. v1 senders arrive already resolved (3.3).
    by_ext: BTreeMap<u32, TiId>,
    next_barrier: u64,
}
```

```rust
// ime/state.rs. Fields are private to this file.
pub(super) struct TextInput {
    seat: SeatId,
    host: HostTi,
    commits: CommitCount,          // ime/serial.rs
    pending: V3Pending,            // ime/state.rs since C3c
    field: Field,
    link: LinkState,
    /// The reset fence of a press (F4) until its done (W2). It lives
    /// beside the link and survives every link change, as today's
    /// TextInputState::reset_fence does.
    fence: Option<BarrierToken>,
    composition: Composition,
    warned: Warned,                // { seat_missing, no_mirror, deferral }: log-only, ignored by the invariants
}

enum Field { Unfocused, Focused(FocusedField) }

/// A named struct, so that Current can borrow it.
struct FocusedField {
    surface: SurfaceId,
    /// The committed state: enabled, surrounding text and content type.
    /// The enabled flag lives here, so ending or refocusing the field
    /// clears it by construction (B7).
    committed: V3Committed,
}

/// What the host text input has applied (today: HostApplied). Only the
/// transitions of TextInput change it, and only TextInput::leave_attached
/// leaves Attached.
pub(super) enum LinkState {
    /// No input method is attached, and none is about to be.
    Detached,
    /// `activate` was sent. Exo attaches when the surface has its focus
    /// (E1); a later v1 `leave` is Exo's own detach (H2).
    Attached { content_type: Option<HostContentType> },
    /// `deactivate` was sent, and `barrier` drains Exo's events from
    /// before it.
    Draining { barrier: BarrierToken },
}

/// What the guest shows, and the v1 pieces of the next host event.
/// What Exo holds is the Exo mirror's (`ExoView`, beside the link).
#[derive(Default)]
struct Composition {
    shown: Option<Preedit>,
    assembly: Assembly,            // { cursor, selection }: preedit_cursor and preedit_styling
}
```

Revision 2 wrapped the link in a `HostLink` newtype whose methods
returned a `#[must_use]` `LeftAttached` token for `Composition::end`.
#82 dropped both: the newtype only wrapped the enum in one-line methods,
and one function now does what the token enforced (below).

**Link transitions.** This is the complete list. Each is a method of
`TextInput`, named after what happened.

| From | To | Method | Caller (row) |
|---|---|---|---|
| `Detached` | `Attached{ct: None}` | `activate_sent()` | reconcile: Activate |
| `Attached{ct}` | `Attached{ct'}` | `content_type_sent(ct')` | reconcile |
| `Attached` | `Draining{b}` | `deactivate_sent(b)` | reconcile: Deactivate |
| `Attached` | `Detached` | `abandoned()` | R1 (reconcile: Abandon) |
| `Attached` | `Detached` | `exo_detached()` | H2 |
| `Attached` | `Detached` | `host_deleted()` | F6, and G10 with `host_deleted` |
| `Draining{b}` | `Detached` | `barrier_done(b)` | W1 |

**`leave_attached(next) -> Option<Composition>`** is private to
`state.rs` and is the one writer of a non-`Attached` link from
`Attached`. The four exits above call it. It ends the composition with
the link and returns it, or changes nothing and returns `None` if the
link was not attached. So the rule "a composition exists only while
attached" (I2) is held by having one function, not by memory.

Revision 2 also had a `new_field()` transition, which unset the
content type on a new field. #82 dropped it. A new field is never
attached after its step: a disable makes the reconcile leave `Attached`,
and an enable, or a rejected one, starts from a link that was not
attached (I1a). The same holds for a refocus (F1, F2), which leaves the
field disabled.

**Composition transitions.**

- `shown` changes only in four ways:
  - through `Planned::apply` of a plan that `Current` made (H7 to X2,
    F4);
  - through `leave_attached`, on any exit from `Attached`;
  - through the reconcile's exit from `Attached` after a new field
    (G9c) or a focus reset (F1, F2);
  - through the answer forgetting a preedit that no longer fits (G9b,
    G9c).
- The assembly records only while `Attached` (H4 and H5). A preedit and
  a region take it first (H7, X1), a commit and a delete once they reach
  the guest (H8, H9), a confirmation once it commits a preedit (X2), a
  press once its commit reaches the guest (F4), and `leave_attached` clears it
  (B3, below H9).

**Fence transitions.**

- `None → Some(f)`: only F4, which requires the gate, so no fence is
  pending then.
- `Some(f) → None`: only W2, with the same `f` (`barrier_done`, which
  ends the drain or the fence the token is, in any link state).

The fence is independent of the link, so the gate stays closed across an
Abandon (R1) and a re-activation. This is the case the first revision's
A1 got wrong (see Appendix B).

**The gate.**

```rust
// ime/state.rs
impl TextInput {
    /// The only constructor of Current: the link is Attached, no fence
    /// is pending, and the field is focused.
    pub(super) fn current(&mut self, ti: TiId) -> Option<Current<'_>>;
}

/// Proof that the composition is current. Its fields are private to
/// state.rs, so no other module can forge one.
pub(super) struct Current<'a> {
    ti: TiId, seat: SeatId, serial: Serial,
    composition: &'a mut Composition,
    fence: &'a mut Option<BarrierToken>,
}
impl<'a> Current<'a> {
    pub(super) fn shown(&self) -> Option<&Preedit>;
    /// Each method plans the transaction of one row.
    pub(super) fn preedit(self, text: String, commit: String, assembly: Assembly) -> Planned<'a>; // H7
    pub(super) fn host_commit(self, text: String) -> Planned<'a>;                                 // H8
    /// H9 and X1: a transaction `exo_view::Frame` measured in Exo's text.
    pub(super) fn plan(self, txn: GuestTxn, lease: LeaseOutcome) -> Planned<'a>;               // H9, X1
    pub(super) fn confirm(self) -> Confirm<'a>;  // X2: Commit(Planned); X2': RecoverHeldKey(KeyEffect)
    pub(super) fn press_commit(self, fence: BarrierToken) -> Planned<'a>;                         // F4
}
/// A planned transaction and the change it makes. Dropping it changes
/// nothing.
pub(super) struct Planned<'a> { /* private: txn, lease, fence */ }
impl Planned<'_> {
    /// Encode the transaction and, only if it fits, make its change.
    pub(super) fn apply(self) -> Result<(Vec<Outgoing>, KeyEffect), TooLong>;
}
```

Revision 2 had the arm call `planned.txn.encode(..)` and then
`planned.commit()`. In #82, `Planned::apply` does both, so no arm can
apply a change whose encoding failed, and a plan stays pure.

**Invariants.** `ImeCore::assert_invariants(&self, env)` checks these
after every `step` in tests. `Context::assert_no_orphans` is to call it
from C3a.

- **I1a.** `Attached` implies `Focused(f)` with `f.committed.enabled`.
  This holds between steps, because every step ends with a reconcile:
  a target that turned inactive has just been deactivated (`Draining`)
  or abandoned (`Detached`).
- **I1b.** After a step, the link matches the target, except in three
  cases. #82 checks this declaratively, by matching the target against
  the link, not by asking `reconcile` whether anything is left to send:
  - `Draining` matches any target;
  - `Detached` with an active target is legal only while no live seat
    can carry the activation (R2);
  - `Attached` needs an active target and has its content type
    (`Some(target.content_type)`). Its seat may be dead: the seat was
    released while the field kept focus (INVARIANTS "Releasing a parent
    does not end its children", test
    `release_does_not_end_child_text_input_focus`). `Attached` with no
    target cannot occur after a step, since the reconcile deactivates
    or abandons it (R1).
- **I2.** `shown.is_some()` or a non-empty assembly implies `Attached`.
  `shown.is_some()` implies `fence.is_none()`.
- **I3.** At most one text input has `committed.enabled`.
- **I4.** `by_ext` is exactly the extensions of `inputs`.
- **I5.** Every token in `Draining{b}` or in `fence` is pending in
  `InternalCallbacks` as `InternalCallback::TextInput { token }`. The
  adapter checks this. It replaces today's reset-fence check in
  `assert_no_orphans`.
- **I6 (the barrier tokens; #82).** Every pending drain and fence is a
  token the core numbered (below `next_barrier`), and each is pending
  once across all text inputs. With I5, this makes the adapter's map
  from token to host callback exact.
- **I7 (#82).** An unfocused text input has nothing pending: a refocus
  drops the pending state (test `row_G9c_a_refocus_drops_the_pending_enable`).

`Env`'s documented precondition makes I1b hold after every step: every
change of an answer that can deactivate a target, a surface's end or a
focus change, reaches the core as an `Input` in the same message. A
seat's end is the one exception (2.3, after R2).

### 3.2 The guest transaction and effects

```rust
// ime/guest.rs
/// A preedit as the guest is shown it. Its text is never empty: an empty
/// preedit is no preedit, which new() makes impossible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Preedit { text: String, cursor: (i32, i32) }
impl Preedit {
    /// The preedit `text` with `cursor`, or None if `text` is empty.
    pub(super) fn new(text: String, cursor: (i32, i32)) -> Option<Self>;
}

/// What the guest applies at one done, in the protocol's order: the
/// delete, the commit, then the preedit. `preedit: None` clears a preedit
/// the guest shows; what tells "clear" from "omit" is whether it showed
/// one (`shown_before`).
#[derive(Debug, Default)]
pub(super) struct GuestTxn {
    pub(super) delete: Option<(u32, u32)>,
    pub(super) commit: Option<String>,
    pub(super) preedit: Option<Preedit>,
}

impl GuestTxn {
    /// The only builder of zwp_text_input_v3 preedit_string,
    /// delete_surrounding_text, commit_string and done, in mutter's and
    /// KWin's order (section 2, Conventions):
    /// 1. preedit_string("", 0, 0), only if `shown_before` and the
    ///    transaction deletes, commits or has no new preedit;
    /// 2. delete_surrounding_text, only if not (0, 0);
    /// 3. commit_string, only if non-empty;
    /// 4. the new preedit_string, if any (never empty, by Preedit::new);
    /// 5. done(serial).
    pub(super) fn encode(&self, ti: TiId, shown_before: bool, serial: Serial)
        -> Result<Vec<Outgoing>, TooLong>;
}
```

- `Serial` comes only from `CommitCount::serial()`, so a `done` cannot
  carry another number.
- The G9c answer is `GuestTxn { preedit: shown, .. }.encode(ti, false, ..)`.
  With `shown_before` false, it sends no clear, and it repeats the shown
  preedit even when unchanged.
- The transaction that the keyboard concern sends after a recovered key
  (X2') is the G9c answer, built when the key is recovered
  (`ImeCore::recovery_done`). With nothing shown it is the bare form of
  the same encoder, `bare_done`, not a second builder.
- **`guest_txn_encodes_in_canonical_order`** pins all 16 combinations of
  a shown preedit, a delete, a commit and a new preedit, and that empty
  commits and `(0, 0)` deletes send nothing.

```rust
// ime/mod.rs
#[derive(Debug, Default)]
pub struct Effects {
    /// Guest events, in order. Every IME transaction ends with its done.
    /// A press that ended a composition (F4) is re-sent here, after its
    /// transaction.
    pub to_guest: Vec<Outgoing>,
    /// Host requests and barriers, in order.
    pub to_host: Vec<HostOut>,
    /// Requests to the keyboard concern. The adapter applies them after
    /// to_guest.
    pub keys: Vec<KeyEffect>,
    /// Whether the press of an Input::Press is to be forwarded: it
    /// ended no composition. The core consumes every other input's
    /// message, so this is false for them (#82; revision 2 had the
    /// opposite default).
    pub forward: bool,
}

pub enum HostOut {
    Request(Outgoing),
    /// Queue a host wl_display.sync here. Its done comes back as
    /// Input::BarrierDone(token).
    Barrier(BarrierToken),
    /// Retire the host object that the previous Request destroyed. It is
    /// in this sequence because it must follow that request.
    Retire(u32),
}

pub enum LeaseOutcome { End, ArmIfBackspaceHeld }
pub enum KeyEffect {
    /// Today's finish_host_preedit arm-or-end, and every end_lease.
    Lease { ti: TiId, seat: SeatId, outcome: LeaseOutcome },
    /// X2': recover the seat's latest eligible held key
    /// (held_repeat_key_for_seat), once the host's next event shows the
    /// confirmation to be its autorepeat (ime_keys::PendingRecovery). Then
    /// send its pair unless another channel delivered it, then
    /// ImeCore::recovery_done, whether or not the pair went. The lease: a
    /// held Backspace whose pair is sent arms it; a held Backspace whose
    /// pair is skipped ends it; another held key whose pair is sent ends
    /// it; another held key whose pair is skipped leaves it alone; with no
    /// held key, or a new press's confirmation, it ends, and nothing is
    /// sent.
    RecoverHeldKey { ti: TiId, seat: SeatId },
}
```

### 3.3 The boundary with the rest of the proxy

```rust
// ime/mod.rs
pub enum Input {
    // Guest zwp_text_input_manager_v3 / zwp_text_input_v3 (G1–G10)
    /// The adapter has allocated `host`'s IDs: its v1 object from the host
    /// zwp_text_input_manager_v1 `manager`, and its extension from
    /// `HostExt::manager`.
    Create { ti: TiId, seat: SeatId, host: HostTi, manager: u32 },
    Enable(TiId),
    Disable(TiId),
    SurroundingText(TiId, IndexedString, i32, i32),
    ContentType(TiId, (u32, u32)),
    CursorRect(TiId, (i32, i32, i32, i32)),
    Commit(TiId),
    Destroy { ti: TiId, host_deleted: bool },

    // Host events (H, X, W). v1 senders arrive resolved to their TiId:
    // the adapter already looked them up in the ShadowTable, a hash map.
    // Extension senders go through ImeCore::by_ext.
    HostLeave(TiId),
    V1(TiId, HostIme),
    Ext { ext: u32, event: HostIme },
    BarrierDone(BarrierToken),

    // Cross-concern messages (F1–F4)
    FocusChanged { seat: SeatId, surface: Option<SurfaceId> },
    SurfaceDestroyed(SurfaceId),
    SeatBound(SeatId),
    /// A pointer press or a touch down on `surface`. `resend` is the
    /// guest's copy of the event. The core queues it after the
    /// transaction if it ends a composition (#72's shape).
    Press { surface: SurfaceId, resend: Outgoing },
}

pub enum HostIme {
    PreeditStyling { index: u32, length: u32, style: u32 },
    PreeditCursor(i32),
    CursorPosition { index: i32, anchor: i32 },
    Preedit { serial: u32, text: IndexedString, commit: String },
    Commit { serial: u32, text: String },
    Delete { index: i32, length: u32 },
    PreeditRegion { index: i32, length: u32 },
    Confirm,
}

/// Read-only facts the core needs and does not own. The adapter
/// implements this over ShadowTable and KeyboardFocusRegistry. Tests and
/// the DST use maps.
///
/// Precondition (#82): every change of an answer that can deactivate a
/// target, a surface's end or a focus change, reaches the core as an
/// Input in the same message, so that the reconcile ending that step
/// sees it. A seat's end is the one exception (R1, I1b).
pub trait Env {
    /// The host ID of a live guest surface (today host_target's lookup).
    fn host_surface(&self, s: SurfaceId) -> Option<u32>;
    /// The host wl_seat this guest ID can carry, while it names a live
    /// wl_seat (today live_host_seat).
    fn host_seat(&self, s: SeatId) -> Option<u32>;
    /// The surface the seat's keyboard focuses now (for G1 and F2).
    fn keyboard_focus(&self, s: SeatId) -> Option<SurfaceId>;
}

impl ImeCore {
    pub fn step(&mut self, input: Input, env: &impl Env) -> Effects;
    pub fn keysym_target(&self, ti: TiId) -> Option<SeatId>;   // H10: Attached, ignoring the fence
}
```

**Why `Env` and not more inputs.** Seat liveness and surface pairing
belong to the object table, and keyboard focus belongs to
`KeyboardFocusRegistry`. Copying them into the core would give them a
second owner. `Env` reads them during the same dispatch, so `step` stays
deterministic and does no I/O.

**The adapter** (`handler/text_input.rs` after C3c) is one function, plus
a one-line `on_*` per message:

```rust
pub(crate) fn ime(ctx: &mut Context, input: ime::Input) -> Action {
    let env = CtxEnv { table: &ctx.shadow_table, focus: &ctx.keyboard_focus };
    let fx = ctx.ime.step(input, &env);   // logs its input, Env answers and effects
    ctx.host_to_client_queue.extend(fx.to_guest);
    for out in fx.to_host {
        match out {
            HostOut::Request(m) => ctx.client_to_host_queue.push(m),
            HostOut::Barrier(token) => { queue_internal_sync(ctx, InternalCallback::TextInput { token }); }
            HostOut::Retire(id) => ctx.retire_destroyed_host_object(id),
        }
    }
    for k in fx.keys { crate::handler::ime_keys::apply(ctx, k); }
    if fx.forward { Action::Forward } else { Action::Drop }
}
```

`end_composition_on_press(ctx, surface, resend)` from #72 keeps its
signature. Its body becomes
`ime(ctx, Input::Press { surface, resend: resend() })`. The `resend`
closure is evaluated eagerly; it costs one small encode per press.

**ID mapping.**

- At `get_text_input`, the adapter still allocates the bridged host IDs
  (`ShadowTable::create_bridged` and `create_host_only`) and passes them
  in `Create`.
- The core emits `create_text_input` and `get_extended_text_input`, and
  records the extension in `by_ext`.
- v1 events: the adapter resolves the sender with
  `shadow_table.get_guest_id`, as today.
- Extension events: the core resolves them through `by_ext`. That
  replaces today's linear scan.
- Host surface and seat IDs come from `Env` when a request needs them.
- The core never sees host callback IDs. `InternalCallback::TextInputDrained
  { text_input, host_text_input }` and `TextInputResetFence { text_input }`
  merge into `InternalCallback::TextInput { token }`. The
  `host_text_input` guard goes away, since a token is never reused.
- `delete_id` for the guest object stays with the generated local
  destructor path (G2, G10). It is not an `Effect`.

**Who sends which input.**

| Sender (owner unchanged) | Input | Replaces |
|---|---|---|
| `TextInputManagerV3Handler::on_get_text_input` | `Create` | its body |
| `TextInputV3Handler::on_*` | `Enable` … `Commit` | their bodies |
| `lifecycle::forget_object`, `zwp_text_input_v3` arm | `Destroy` | `forget_text_input` |
| `TextInputV1Handler::on_*` | `HostLeave`, `V1` | their bodies |
| `ExtendedTextInputV1Handler::on_*` | `Ext` | their bodies |
| `CallbackHandler::on_done` | `BarrierDone` | two arms |
| `KeyboardHandler::on_enter`, `on_leave`, `forget_keyboard` | `FocusChanged`, one per `SeatFocusChange` | `apply_keyboard_focus_changes` |
| `compositor::forget_surface` | `SurfaceDestroyed`, after the `FocusChanged`s of `destroy_surface` | `repair_destroyed_surface_focus` |
| `registry.rs`, `wl_seat` bind | `SeatBound` | `reconcile_seat` |
| `PointerHandler::on_button`, `TouchHandler::on_down`, via `end_composition_on_press` | `Press` | its body |
| `ime_keys::on_keysym` | query `keysym_target` | `active_guest_for_host_text_input` |

**Ordering.**

- A message is still one transaction (DESIGN "Message effects").
- The adapter appends the core's output at the point of the call, so
  order within a direction is the handler's, exactly as today. For
  example, `KeyboardHandler::on_enter` queues `modifiers` before the
  IME's `leave` and `enter`, and a surface's Aura release goes before its
  text-input `leave` (INVARIANTS Object lifetimes).
- If the session ends during the message, the loop drops every effect,
  and the core's new state does not matter, because no further message
  is handled (`Client::handle_msgs`).

**Replayable trace log.**

- At `trace`, on target `sommelier::ime`, `step` itself logs each
  `Input`, each `Env` answer it reads (through a tracing wrapper of
  `Env`), and each `Effects` (#82). Revision 2 had the adapter log a
  snapshot of the answers instead.
- `sim::regressions::from_log` turns such a log, captured on a host
  during a manual U-check with `RUST_LOG=sommelier::ime=trace`, into a
  core-level replay against a recorded `Env`. A host-only failure then
  becomes a unit test.
- The log carries the user's text only at `trace`, which is never on by
  default.

### 3.4 The step function

`step` has three phases:

1. **Arm.** One match arm per `Input`, following the rows of section 2.
   Each arm computes everything it needs (plans, encodings) before it
   mutates anything (3.5). It then mutates and appends to the step's
   `Effects`.
2. **Reconcile all.** For every text input in `TiId` order:
   - compute the `HostTarget` (with `Env`);
   - call `reconcile(&target, &link)`;
   - apply the steps.

   This runs **unconditionally**, at the end of every input. `reconcile`
   is pure, does a few comparisons and lookups per text input, and is
   idempotent: when the link equals the target it returns nothing. The
   old code's exhaustive test,
   `reconcile_reaches_every_target_with_only_needed_requests`, covers
   the old copy; in #82, I1b checks the result of every row test
   declaratively (3.1). The
   Commit arm passes its `GuestCommit` (the encoded
   `set_surrounding_text` and the cursor rectangle) to this phase, so the
   wire order of 2's "Reconcile" convention is kept.
3. **Removals.** A text input that the Destroy arm marked ending (its
   field set to `Unfocused`) is reconciled to inactive in phase 2. It is
   then removed, with its `by_ext` entry, and its extension's `destroy`
   and `Retire` are appended. This is the order of today's
   `forget_text_input`.

The host-event arm shows the gate and the transaction shape:

```rust
// ime/step.rs (sketch)
fn host_event(&mut self, ti: TiId, event: HostIme, fx: &mut Effects) {
    let Some(t) = self.inputs.get_mut(&ti) else { return };
    match event {
        HostIme::PreeditStyling { .. } | HostIme::PreeditCursor(_) => t.record(event),   // only while Attached
        HostIme::CursorPosition { index, anchor } => log::debug!("no v3 cursor_position({index}, {anchor})"),
        terminal => {
            let assembly = t.take_assembly();                     // B3: a preedit's or region's, first
            let Some(cur) = t.current(ti) else { return log_stale(ti, &terminal) };   // B2: the gate
            let planned = match terminal { /* H7–X2 via Current's methods */ };
            let applied = planned.apply();                        // encodes, then changes state
            // B3: a commit or delete takes the assembly only if applied
            match applied {
                Ok((events, lease)) => { fx.to_guest.extend(events); fx.keys.push(lease); }
                Err(TooLong) => log::debug!("dropping {ti:?}'s event: too long"),   // nothing applied
            }
        }
    }
}
```

### 3.5 Failure rules

There are two failure domains, with one rule each.

- **A guest transaction that does not encode, for a host event or a
  press** (H7 to X2, F4). The event is dropped, and the assembly is
  taken but by a commit or a delete (B3, below H9). Nothing else changes, and no `done` is owed, because the
  trigger was a host event. A press is then forwarded unchanged.
- **A guest commit whose host request does not encode** (only
  `set_surrounding_text` carries a string). This is G9b:
  - the count advances and the pending state is consumed;
  - no host request is sent for this commit;
  - the answer `done` still goes out if the field was already enabled;
  - if the repeated preedit does not fit, the shown preedit is
    forgotten and a bare `done` is sent.

  Skipping the count or the `done` here would wedge Chromium's C1 gate.

Each arm builds its output in a local value. The step's `Effects` grows
only after the arm has decided. There is no clone of the core and no
rollback: an arm that decides first has nothing to roll back.

### 3.6 Guarantees and how each is enforced

| Class | Mechanism | Enforced by |
|---|---|---|
| B1 hidden host state | The link changes only through `TextInput`'s seven named transitions (3.1). `step` reconciles every text input after every input. | **The type system** for "no arm forgets to reconcile": the call is in `step`, not in the arms. **Module privacy plus review** for "only reconcile, H2, W1, F6 and G10 call the transition methods". The row tests pin each transition. |
| B2 stale epoch | `Current<'_>` is constructible only in `state.rs`, by `TextInput::current()`. Host-event and press transactions are planned only through `Current`'s methods. | **Module privacy (compiler)** inside `ime`: `step.rs` cannot build a `Current`, nor set `shown`. |
| B3 leftover pieces | `take_assembly()` comes first in the preedit and region arms, and after the plan in the commit, delete and confirmation arms and in the press (B3, below H9); `step.rs`'s `Takes` names which, per event kind. Every exit from `Attached` goes through `TextInput::leave_attached`, which ends the composition with the link. | **A convention plus row tests** for "first". **Module privacy** for the generation boundary: `leave_attached` is private to `state.rs` and is the one writer of a non-`Attached` link from `Attached`; I2 checks the result after every step. Row tests feed styling and a cursor before each terminal event in flight at a press, and before each dropped commit, delete and confirmation, then check which ones leave them to the next preedit (`row_F4_events_in_flight_at_a_press_are_dropped`, `row_H8_H9_X2_only_an_edit_that_reaches_the_guest_takes_the_assembly`, `row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly`). |
| B4 serial drift | `CommitCount` sits in `serial.rs` with a private field. `bump()` is `pub(super)` and called only through `TextInput::count_commit`, from the Commit arm. `Serial` has no other constructor. | **Module privacy** for the constructor. **A grep test** (`count_bumps_only_in_the_commit_arm`) for the one call site. |
| B5 malformed txn | `GuestTxn::encode` is the only builder of the four v3 events, and it always ends with `done`. | **A grep test** (`ime_is_isolated`). A crate-wide clippy `disallowed-methods` entry was considered and rejected: `sommelier/clippy.toml` notes that an allow for one rule lifts the fd and blocking guards too. |
| B6 missed trigger | Every trigger is an `Input` variant, and the match has no wildcard. | **The compiler**, for a variant that is unhandled. **The DST**, for a source that is never wired (as touch was before #72). |
| B7 one enabled | `enabled` lives in `FocusedField`, so a focus reset, a destroy or a repair clears it by construction (C-1). "At most one" is checked where `enabled := true` is set. | **The type** for staleness. **The Commit arm plus I3** for the count. |
| B8 order | `BTreeMap` in the core; P0 (#75) made the crate hash-free, and clippy's `disallowed-types` keeps it so. | **The type and clippy**. The DST's determinism check runs each case twice, which also covers the host ID allocator's LIFO reuse of freed IDs. |
| B9 races | The DST. | CI. |

What stays outside the core:

- key generations, `peek_key`, `ack_key` and keysym routing: the
  `GuestKeyOwner` reducer, plus `ime_keys` after K1;
- pointer focus (`PointerFocus`);
- keyboard focus (`KeyboardFocusRegistry`);
- surface state.

The core sees these only through `Input` messages and `Env` reads.

### 3.7 Where #82 departs from revision 2

#82 implements C1 and C2 and documents each departure in its commit
messages. Revision 3 adopts all of them; the sections above already
describe the result.

| Departure | Why | Where |
|---|---|---|
| `HostLink`, `LeftAttached`, `Composition::end` and the link's `new_field()` are gone; `TextInput::leave_attached` is the one exit from `Attached`. | The newtype only wrapped the enum in one-line methods. One function holds the rule that the token held. A new field never stays attached, so it needs no transition. | 3.1, 3.6 B1 and B3 |
| `Planned::apply()` encodes and then changes state; a press takes its assembly once its commit is applied (C3c; it took it through `Current::take_assembly` before). | A plan stays pure, and no arm can apply a change whose encoding failed. | 3.1, 3.4 |
| `HostExt` carries its `manager`, and `Input::Create` takes only the v1 manager. | An extension without a manager, or the reverse, cannot be expressed. | 3.1, 3.3 |
| `Preedit::new` returns `None` for empty text. An empty commit and an empty region make no edit. | Aligns with #80. An empty preedit cannot be represented, and no event that changes nothing goes out. | 2 Conventions, H8, X1, 3.2 |
| I1b is checked declaratively. I6 (barrier tokens) and I7 (nothing pending while unfocused) are added. | Asking `reconcile` whether anything is left to send would check the code against itself. | 3.1 |
| `step` writes the trace log, through a wrapper of `Env`. | It sees each answer it reads, not a snapshot taken beforehand. | 3.3 |
| `Effects::forward` is true only for a press that ended no composition. | The core consumes every other input's message. | 3.2 |
| An R2 deferral is logged when it starts or changes. | Not at every step that finds it waiting. | 2.3 R2 |
| `ime` shares `HostContentType`, `V3Committed` and `V3Pending` with `state`. It keeps a copy of `resolve_preedit_cursor` until #80 merges. | C1 moves only stateless helpers (M-1), and #80 edits the old copy. | 3.0, 5.1 |
| **G9b**: the core consumes an oversized commit's pending state, as the table says. The old code kept it. | The old code's retry loop was a bug. The old code is now fixed to match, so the goldens agree. | G9b, 5.1 |
| **Reconcile timing**: `step` reconciles every text input at the end of every input; the old code reconciles a text input only where its target or link changed: at a guest commit, a focus change, a surface's end, a seat's bind, the destroy, H2 and W1. | It follows from the unconditional phase 2 (3.4). **No wire change** (C3b, which routes H2 to X2, W1 and W2 through `step`): every message of the old code that changes a target or a link reconciles that text input, so between messages every link satisfies I1b, and on such a link `reconcile` returns no step: `Draining` waits; `Detached` with an active target has no live seat (R2); `Attached` has the target's content type, whatever its seat. A seat's end, the one change no input reports, changes no target's surface. A host event changes no target, since the target reads only the field, its surface and the seat, and changes a link only at H2 and W1, where the old code reconciled that text input too. So the extra reconciles send nothing; they only note the R2 deferral, which is logged. Since C3c every input goes through `step`, whose reconcile is the only one, so every link satisfies I1b after every message by construction, a seat's end aside, and the R2 deferral is logged alike at every input. | 2.3, after R2 |
| **The assembly**: `cursor_position` is only logged. #82 also took the assembly at every terminal event; C3b made the core take it where the old code does instead (B3, below H9). | The old code recorded `cursor_position` only to log it. The switch carries no wire change, and only a host that sends styling or a cursor before a commit, delete or confirmation could tell. | H6, H9 |
| **Answer order** (C3c): the Commit arm answers a guest commit (G9b, G9c) before the end-of-step reconcile; the old `on_commit` answered after its `reconcile_host`. | **No wire change.** The two commute unless the reconcile leaves `Attached`, which ends the composition the answer repeats. A commit that disables the field gets no answer either way, and one that leaves it enabled leaves `Attached` only if the host no longer knows the field's surface, which `Env`'s precondition rules out: a surface's end reaches the core as F2 in the same message and refocuses the field. | G9c |
| **The press's assembly** (C3c): #82's press took the assembly before it encoded its commit; the old code only once the commit applied. The core now matches the old code (row test `row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly`). | Only a press whose commit does not fit, which the gate's preedit fitting once makes unreachable in practice, could tell. | F4, H9 |

#82 has a row test for every row that is a core input and that some
input can reach. The rows without one are not core inputs: G2, G6, H1,
H3, X3 and F5, which the adapter handles or ignores.

---

## 4. The DST harness

### 4.0 Shape

```
                     user actions (seeded, timed)
                                |
                                v
 +-----------+  C->P   +--------------+  P->E   +-------------------------------+
 | client    |-------->|  real proxy  |-------->| Exo model                     |
 | model     |<--------| (harness::   |<--------|  seat, keyboard, pointer,     |
 | (Chromium |  P->C   |  Session)    |  E->P   |  touch, text inputs (E1-E15)  |
 |  / GTK)   |         +--------------+         |      |  ^   E->Eng / Eng->E     |
 +-----------+                                  |      v  |                       |
                                                |  IME engine (async, Composer) |
                                                +-------------------------------+
```

- **Channels.** Every arrow is a FIFO channel of typed messages.
- **Clock and readiness.** A virtual clock, in microseconds, orders
  everything. Each message gets a ready time: its send time plus a drawn
  latency, but never earlier than the ready time of the message ahead of
  it on the same channel (FIFO).
- **Step.** A step delivers the channel head with the earliest ready
  time, or injects the next user action when its time comes.
- **Determinism.** Everything runs in one thread, and every draw comes
  from a recorded choice stream, so a seed reproduces a run exactly.

### 4.1 File layout

PR #73 (D1) already provides `sim/mod.rs`, `sim/hangul.rs` and
`sim/editor.rs`. Its API is used as is:

- from `hangul.rs`: `Key { Char(char), Backspace, Enter, Left, Right }`,
  `keys(script)`, `Output { commit, preedit, consumed }`, and
  `Composer { key, preedit, reset, confirm }`. As merged, the
  composer also combines two lone consonants
  (ㄱ + ㅅ → ㄳ), as libhangul does by default; whether the ChromeOS
  engine does the same is [U12];
- from `editor.rs`: `TextModel { text, cursor, preedit }`, with
  `apply_txn(&Txn)`, `insert_key(Key)`, `click(Target)` and
  `delete_range(&self, before, after) -> Option<Range<usize>>`. The
  last is the one bounds check of a delete, which the client models
  share for their protocol assertion. #73 dropped selection support
  (`anchor`), so the simulation never has a selection;
- also from `editor.rs`: `Txn { delete: (usize, usize), commit,
  preedit }`; `Target { Start, End, BeforeMarker(u8) }`;
  `UserAction { Key(Key), Click { field, at }, Tap { field, at },
  Focus(usize), Settle }`; and `Reference { new, apply, fields,
  focus }`.

The simulation's `Txn` is the client-side view (byte counts as `usize`).
It is distinct from the core's wire-side `GuestTxn`.

```
sommelier/src/sim/            #[cfg(test)]
  mod.rs         Case, run(), replay(), the CI tests                  (exists; grows ~150)
  hangul.rs      Key, Output, Composer                                (exists, #73)
  editor.rs      TextModel, Txn, Target, UserAction, Reference        (exists, #73)
  clock.rs       Micros, the virtual clock's unit                     56 (#79 and #83 carry the same file)
  sched.rs       The Choose trait (#83); D4 adds Choices, Channel<T>
                 and the step loop                                    42 (#83) + ~250
  exo.rs         The Exo model: proxy/fuzz/exo.rs moved here and extended  ~1,570 (#83; 409 moved)
  engine.rs      The asynchronous IME engine over Composer            ~320 (#83)
  keymap.rs      Key <-> evdev keycode for the US layout; D4 adds the
                 click and tap positions                              ~130 (#83)
  client/mod.rs  The Client trait, Field, Dropped, DropReason, PressKind,
                 and the v3 protocol checks the models share          ~490 (#79)
  client/chromium.rs, client/gtk.rs   Client models over TextModel    ~840 each (#79)
  user.rs        The user action generator, free and settled modes     ~150
  display.rs     The display D4 plays: syncs, delete_id and globals    ~100
  oracle.rs      The exact oracle (SpecText) and the user-intent oracle  ~300
  surfaces.rs    Each field's surfaces, a generation each (F2)         ~170
  golden.rs      Golden trace hashes for the staged switch (C3-pre to C3c; deleted in C3c)  ~80
  shrink.rs      Minimizing a failing case                             ~150
  regressions.rs Replays: minimized cases and from_log                 grows
```

D2 (#83) and D3 (#79) came in at about 2,150 and 2,200 test lines,
against budgets of 700 and 450. #83 attributes most of the excess to
expected-event literals that rustfmt spreads out; the models also follow
their sources more closely than revision 2 planned (2.4, 2.5).

`proxy::fuzz` imports `sim::exo` for its content-type end check. There is
one Exo model, not two.

### 4.2 The scheduler, its clock and the legal reorderings

The real system's concurrency comes down to these FIFO channels:

| Channel | Carries | FIFO because | Latency drawn per message (µs) |
|---|---|---|---|
| `C→P` | guest requests | one Unix socket | 0–2,000 |
| `P→C` | proxy events to the guest | one Unix socket | 0–2,000 |
| `P→E` | proxy requests to Exo | one virtio-wl connection | 0–3,000 |
| `E→P` | Exo events to the proxy | same | 0–3,000 |
| `U→E` | user actions (key, click, tap, focus) | aura's event queue | from the action's time |
| `E→Eng` | keys and `Reset`/`Confirm` notices for the engine | mojo, in order | 0–1,000 |
| `Eng→E` | the engine's result per key | mojo, in order | the engine's service time (below) |

What each delivery does:

- **To the proxy.** A message from `C→P` or `E→P` is handed over with
  `Session::try_guest([m])` or `try_host([m])`, one message at a time.
  This is the finest interleaving that `Client::handle_msgs` can
  produce: it handles one message at a time from either buffer, and its
  32-message batches and fair dispatch only coarsen this. The proxy's
  output is read at once and sent on `P→C` and `P→E`, stamped with the
  current time.
- **To the client.** It handles one event atomically, and sends its
  requests on `C→P`.
- **To Exo, from `P→E` or `U→E`.** Exo handles one request or one input
  event atomically. Its events go to `E→P`, and engine notices go to
  `E→Eng`.
- **To the engine.** It works through keys one at a time. Each key takes
  a **service time** drawn from the case's lag profile:
  `fast` (0.2–2 ms), `normal` (2–15 ms) or `slow` (15–60 ms, with a
  1-in-20 spike to 150 ms). The result goes to `Eng→E` when the service
  ends.

**Ties** in ready time are broken by a canonical channel order, then by
a choice.

**Reorderings.** Nothing overtakes within a channel. All reordering comes
from the drawn latencies:

- A click, travelling `U→E` → Exo → `wl_pointer.button` on `E→P`, can
  overtake an engine result still in service. This is E5's latency race.
- A guest report, travelling `C→P` → proxy → `P→E`, can cross an engine
  edit on its way (HANDOVER, deletes).

**Time-based client behaviour** uses the same clock:

- Chromium's 100 ms guard (C4) is `now < press_at + 100_000 µs`.
- User actions carry inter-arrival times, drawn from 30–300 ms (normal
  typing) or 0–20 ms (bursts that provoke races).
- `Settle` waits until every channel is empty and the engine is idle,
  then adds 50 ms.

**The clock** is `sim::clock::Micros`, a newtype of microseconds since
the start of the case. D3 (#79) and D2 (#83) share it, and both carry
the identical file, so whichever merges second drops its copy.

**Choices.** Every draw (a latency, a service time, a tie, a [U]
variant) goes through the `Choose` trait that #83 put in `sim/sched.rs`.
Every model takes `&mut impl Choose`. In unit tests a closure implements
it, which scripts the choices. In D4, a recorded `Choices` stream does:

```rust
// sim/sched.rs (#83)
pub(super) trait Choose {
    /// A choice below `n`.
    fn pick(&mut self, n: u32) -> u32;
    /// A value from `low` to `high`, both included; choice 0 is `low`.
    fn between(&mut self, low: u64, high: u64) -> u64 { /* low + pick(span) */ }
}
impl<F: FnMut(u32) -> u32> Choose for F { /* scripted tests */ }

// D4
pub struct Choices { recorded: Vec<u32>, replay: Option<Vec<u32>>, rng: Rng, pos: usize }
impl Choose for Choices {
    /// replay[pos] % n when replaying (0 past the end); otherwise draws
    /// from rng and records the value.
    fn pick(&mut self, n: u32) -> u32;
}
```

Choice 0 is always the minimum latency, or the canonical order, so an
all-zero stream is the most sequential schedule. The shrinker aims for
it.

At the end the scheduler **quiesces**: it delivers until every channel
is empty. The system always settles, because clients only react, Exo
answers every sync, and the engine answers every key.

### 4.3 The Exo model

`sim/exo.rs` (D2, #83) starts from today's `proxy/fuzz/exo.rs`, which
models activation, content type, support, surrounding text, the cursor
rectangle and the input method's restart. The fuzzer imports it from
there, and its summary over seeds 0 to 4999 is unchanged by the move. It
adds:

- **Typed requests and one event queue.** `Exo::request(sender,
  &AnyRequest)` takes the requests a `Session` decodes. Every event Exo
  sends, from the text inputs, the seat and the input method, goes into
  one queue in the order Exo sends it. `Exo::take_events()` drains that
  queue.
- **No display.** The model does not answer `wl_display.sync` or a
  destructor's `delete_id`. Whoever plays the display does: the
  fuzzer's compositor, or D4's display (4.10). **The display takes
  `take_events()` after each input it hands the model, and before it
  answers any later sync**, so that a sync's `done` follows every event
  of earlier requests (E11).
- **Seat.** It tracks the focus. Each case gets one keyboard, with its
  extension. It sends:
  - `wl_keyboard.enter` and `leave`, with `modifiers` before `enter`, as
    Exo does. Modifiers go out only when they change, and Shift sends no
    key events of its own;
  - `peek_key` (keyboard extension v2), and it takes `ack_key`.

  `Exo::move_focus` moves the keyboard and the text inputs in E12's
  order: the keyboard first. The pointer, the touch and the click and
  tap positions belong to D4, their first user, and the engine's delete
  (E10) to D6; #83 is moving them out of D2.
- **Text input.** Each host `zwp_text_input_v1` gets one new field,
  `predicted`: a `TextModel` holding what `SurroundingTextTracker`
  predicts (E4), the last `set_surrounding_text` plus the input method's
  edits since. Its preedit is Exo's tracked composition.

  Its requests and events work like this:
  - `reset` cancels the tracked composition, sends nothing, and goes to
    the engine as a `Reset` notice on `E→Eng` (E5). Keys already in the
    engine are not cancelled.
  - A detach, whether from `deactivate` or Exo's own, does this in order
    (E6):
    1. It confirms: `confirm_preedit` at extension v11 or later, even
       with nothing composed; below v11, `commit_string` of the tracked
       composition, if there is one.
    2. It sends `leave` and queues a `Reset` notice for the engine.
- **IMA.** Between the engine and the text input, the model follows
  `InputMethodAsh`'s rules. #83 as pushed draws these behaviours as
  [U]; now that they are facts, D2 follows them instead:
  - IMA buffers a key's text until the engine's callback for that key;
  - a handled press goes out before its text, and an unhandled press
    after its text (E8);
  - an unhandled press is preceded by a confirmation (E13);
  - a reset drops IMA's buffer (E5).
- **Keys.**
  - A key on the focused surface with an attached text input goes to
    the engine as `E→Eng`.
  - **With no attached text input, the key goes raw (E7).** Exo sends
    `peek_key` and `wl_keyboard.key` at once, and the client inserts the
    US-layout character. This is the path during a drain or before the
    first `activate`.
  - Consumed keys share the serial that the next key sent frees
    (`MaybeNextKeySerial`).
- **Engine results** (`Eng→E`). `Exo::answer(&EngineOut)` applies them
  to the text input that IMA serves now. If it serves none, the text is
  dropped, because IMA has no client, but the key's events still go
  out, in E8's order. The text events are:
  - a preedit: `preedit_cursor(len)`, then `preedit_string(serial, t, t)`.
    An empty preedit that ends a composition is `preedit_string("", "")`.
  - a commit: if it equals the tracked composition and the extension is
    v11 or later, `confirm_preedit` or `commit_string`, as the case's E9
    draw says. Otherwise always `commit_string`, so a syllable split such
    as 각 + ㅏ, or E15's "가 ", never becomes a confirm.
  - a delete (D6): an absolute `delete_surrounding_text` measured
    against `predicted` (E10).

  `predicted` is updated as the events go out.
- **Lateness.** An answer is `late` when it sends text the engine made
  before a reset that IMA made after Exo handed it the key: outcome (c)
  of E5. **D4 decision: `Exo::answer` reports whether the answer was
  late**, as a `bool`, so the
  scheduler labels that delivery for the exact oracle and the P3
  counter. The model also keeps the total (`late_answers()`). The exact
  oracle applies late results by the specification.
- **Draws per case:**
  - the extension version: none, 7, 10 or 11 (<8 means
    `set_content_type` plus the deprecated input type; 8–10 means
    `set_input_type` with commits only; ≥11 means `confirm_preedit`).
    Exo advertises v14 (E14), so a current host is always 11;
  - E9 (the style of a confirmable commit).

  E6, E8 and E12 are facts now, and are no longer drawn (2.4).

### 4.4 The engine

`sim/engine.rs` (D2, #83):

```rust
// sim/engine.rs
pub(super) enum EngineIn { Key { key: Key, id: u64 }, Reset }
pub(super) struct EngineOut { pub id: u64, pub actions: Vec<TextAction>, pub consumed: bool }
pub(super) enum TextAction { Commit(String), Preedit(String), Delete { before: usize, after: usize } }
// Delete belongs to D6, its first user
pub(super) enum Mode { Korean, Latin }
pub(super) enum Lag { Fast, Normal, Slow }   // Lag::draw(&mut impl Choose), once per case

impl Engine {
    pub fn new(mode: Mode, lag: Lag) -> Self;
    /// Exo hands the engine `input` at `now`.
    pub fn send(&mut self, now: Micros, input: EngineIn, choices: &mut impl Choose);
    /// When the answer to the key in service is ready, if any.
    pub fn ready_at(&self) -> Option<Micros>;
    /// The answer to the key in service; the engine then starts on what follows.
    pub fn take(&mut self, choices: &mut impl Choose) -> EngineOut;
}
```

To follow E8, D2 splits an answer in two: the key's text, and the
engine's callback for the key with its handled bit, which is how IMA
sees them.

- **Korean mode** runs `hangul::Composer::key`. An `Output { commit,
  preedit, consumed }` becomes `[Commit(commit) if non-empty,
  Preedit(preedit)]`, where `preedit` is emitted when it changed,
  including to empty. Space ending a composition is consumed and
  committed with it (E15). With nothing composed, Space and Enter go to
  the client.
- **Latin mode** passes every key.
- **`Reset`** takes effect when the engine reaches it, after every key
  handed over before it. Exo confirms a detach itself, from the
  composition it tracks (E6), so the engine needs no `Confirm`.
- A key queued before a `Reset` is processed with the old composer
  state. That is outcome (c) of E5, and P3.
- **D6** adds an "accept a suggestion" user-visible action: a
  `Delete { before: last word }` and then a `Commit(replacement)`, only
  while nothing is composed (E10). It also adds a reconversion region
  (X1). This exercises H9, X1 and P4.
  - Both are `Request`s, which Exo hands the engine with the text before
    the cursor as it predicts it. The word runs back to a marker or a
    space, and its replacement is the word and a `~`. A reconversion
    takes one character, or one more before the region it has, and the
    next key commits the region as it is. Exo sends the region with
    `set_preedit_region` from its cursor, only with an extension; without
    one it composes the region all the same, which the guest never sees,
    so the guest keeps showing the composition the region replaced, and
    the next key commits its text a second time; and
    the engine learns the region's text from the field, since it
    measured the region in text that can lag. When IMA refuses a range
    that lies past the text by the time it comes, as after a report that
    moved the cursor, the engine keeps the region it had, as the field
    does (IMA's `SetComposingRange` returns false before it changes
    anything).
  - A free case makes half of these twice at once, for a delete after
    an edit the guest has yet to report (P4) and a region over a region
    (#85). A
    settled case makes them only right after a key, since GTK3 reports
    a click's cursor only with its next edit, and never twice, since Exo
    drops a reported region as a composition (E4).
- **D7** adds holds: `UserAction::KeyDown(Key)` and `KeyUp(Key)` next to
  #73's atomic `Key`.
  - The engine gets autorepeats (500 ms delay, 20 Hz). Exo forwards
    none of them (E7).
  - IMA precedes every unhandled key, autorepeats included, with an
    empty `confirm_preedit` at v11 (E13, formerly U11). This exercises
    X2' and the Backspace lease.
  - As built: a free case holds a key one action in 33, mostly Space or
    Backspace, and may type up to two other keys before it releases it,
    up to 1.5 s later. The key the user pressed last repeats until it is
    up or another key is typed. The display gives each keyboard the
    default keymap, without which the proxy recovers no key, since it
    recovers only a key the keymap repeats. Exo's keyboard passes a press
    only of a key it does not hold, the engine's answer included.
  - `SpecText` follows the keyboard concern as far as X2' needs it: the
    keys held, in the order of their peeks, which a `wl_keyboard.enter`
    clears, and the latest peek, which a focus move forgets; which key
    `wl_keyboard.key` delivered; and the Backspace lease, which H7 and a
    recovered Backspace arm, and which every other composition edit, a
    new field, a focus move and another key's press end, cancelling
    Backspace's repeat for the rest of its press. A recovery waits for the
    host's next event, as the keyboard concern's does, and Exo's own
    detach (H2) ends the recovery of every key held (X2').
  - Settled cases hold no key: the user's intent for a hold depends on
    the client's own repeat, which the client models do not have.

### 4.5 The client models

D3 (#79) built both models. Each implements one trait, so D4 can drive
either:

```rust
// sim/client/mod.rs (#79)
pub(super) trait Client {
    fn fields(&self) -> &[Field];                    // Field { surface, model: TextModel }
    fn focused_field(&self) -> Option<usize>;
    fn drops(&self) -> &[Dropped];                   // Dropped { at, part, reason }
    fn keyboard_focus(&mut self, now: Micros, surface: Option<u32>) -> Vec<Outgoing>;
    fn event(&mut self, now: Micros, sender: u32, event: Event) -> Vec<Outgoing>;
    fn key(&mut self, now: Micros, key: Key) -> Vec<Outgoing>;
    fn press(&mut self, now: Micros, surface: u32, at: Target, kind: PressKind) -> Vec<Outgoing>;
    fn release(&mut self, now: Micros, dragged: bool) -> Vec<Outgoing>;
}
```

Each model holds a `TextModel` per field and reacts only to what the
application learns:

- **A transaction.** v3 events are collected until `done`. The model
  then converts them into an `editor::Txn` and calls `apply_txn`, unless
  one of its own rules drops a part.
- **A key.** An unconsumed `wl_keyboard.key` maps back to a `Key`
  through `keymap.rs` and goes to the model.
- **A press.** `wl_pointer.button` or `wl_touch.down` resolves its
  position to a `Target` and a `PressKind`, then calls `press`. The
  release or lift calls `release`, saying whether it dragged.

| Model | Rules (2.5) | Drops it labels (H-5) | Setup |
|---|---|---|---|
| `Chromium` | C1 to C5, C10 and C11. Reports are gated on `last_done == commit_count` and carry the predicted text. The text input is enabled for a window only while the window is focused and entered. Every press in a focused window resets; a touch confirms on a tap. The 100 ms guard belongs to each window. | A delete while composing (C3). A delete that splits a character of the predicted text. Commits and preedits within 100 ms of a reset that ended a composition in the window (C4). Anything for a window without the keyboard focus (C11). Events held when the text input reset, enabled or disabled. | **One text input per connection** (C10), shared by its windows. Each field is a window on a surface of its own; two fields, one seat. |
| `Gtk { v4 }` | C6 to C9 and C12 to C14. A `done` is answered only when it changed something (C6). A context per field. Enable on a single-click release that did not drag (GTK3: caret moved less than its height). On press: reset (always on GTK4; on GTK3 with `need_im_reset` and a cursor move). Key-binding resets; Backspace leaves the NFD prefix. | Anything while no field has the keyboard focus (`NoFocusedField`). | One text input per display, a context per field, two fields on two surfaces. `GtkEntry` and `GtkText` only; `GtkTextView` differs and is not modelled. |

Every drop goes into the model's drop log with its time, its part and
its reason. The user-intent oracle reports the log; the exact oracle
never needs it (4.7).

Each model also asserts the v3 protocol as it receives events. A
violation panics, which fails the case. The rules:

- a `done` serial is at most the model's commit count (with wrapping);
- text-input events arrive only between `enter` and `leave`, `enter`
  does not repeat, and `leave` names the entered surface;
- every event names an object the model holds, and a surface it has;
- every string is UTF-8;
- a delete fits one of the surrounding texts the client sent since it
  last enabled (`TextModel::delete_range`). The proxy measures a delete
  against the text the client last committed, and a newer report may be
  in flight, so the client applies the delete clamped to its current
  text.

The models never hold a selection, because #73's `TextModel` has none.
The proxy's selection rules (H9's drop while the guest has a selection,
C5, C9's anchor, and #80's no-op rules) are therefore pinned only by row
tests and Session tests, not by the DST. 6.1 lists this gap.

**To reconcile in D4.** D3's Chromium model starts its prediction from
what the window last reported and does not replay
`SurroundingTextTracker`'s history, since a report there always catches
up at once. D2's Exo model does replay it (E4). D4 either models the
replay in Chromium too or shows that no case needs it.

### 4.6 User actions

The actions are #73's `UserAction` and `Target`, used unchanged:

- **Targets.** A target means the same text to the user and to a client
  that lags behind: the start, the end, or just before a marker digit
  seeded into the initial text (for example `"0ab1cd2"`).
- **Markers.** The generator keeps the markers. It generates a Backspace
  only when the `Reference` has something after the last marker, or
  after the start, to delete.
- **Mix.** About 59% keys (jamo for syllable patterns, plus Space and
  Backspace), 2% accepted suggestions, 2% reconversions (4.4), 15%
  clicks, 8% taps, 7% focus changes, 2% restarts of the input method,
  2% windows that open again (free cases only, below) and the rest
  `Settle`. A case has 10 to 60 actions and two fields.
- **Restarts.** `UserAction::RestartInputMethod` destroys Exo's input
  method (`Exo::destroy_input_method`), which confirms and detaches on
  its own (E2, E6); its confirmation resets the engine only if a text
  input was attached, and a key in flight never gets its callback. Ash
  destroys `InputMethodAsh` only at shutdown, so this is a model's
  trigger, not production's. Production reaches H2 when a keyboard is
  bound after the text input's `activate`: it registers as the seat's
  observer after the text input, so the text input's detach comes
  before the keyboard's `leave` at the next focus move (E12). Half the
  cases bind their keyboard again once the text input is active (the
  new one first, the old one's release once it entered, so the seat
  keeps its focus), which the counter `detaches_before_keyboard` counts.
  Before that, the restart was the only action
  that reaches H2: since E12, a focus move always reaches the keyboard
  first, so the proxy has already deactivated when Exo's `leave` comes
  (H2'). A surface the guest destroys (F2) cannot reach H2 either: the
  proxy resets the field and deactivates in the step that forwards the
  destroy, before Exo learns of it.
- **Windows that open again (F2).** `UserAction::Reopen` closes a
  field's window and opens it again on a new surface, with the same
  text, as a client recreates a window. The client model blurs if the
  field had the focus, forgets what it reported, and moves the field to
  the new surface (`Client::reopen`); it destroys the old surface and
  creates the new one. Exo drops the destroyed surface's focus with no
  keyboard `leave` and deactivates its text inputs; the pointer leaves
  it. If the field had the seat's focus, the new surface takes it at
  its first commit, as a window manager focuses a new window once
  mapped: the display has no shell roles or buffers, so that commit
  stands for the map. Input that Exo sent to the old surface before it
  learned of the destroy reaches the proxy, which drops what names that
  surface; a press whose pointer is on it lands nowhere. What the proxy
  sent before it got the destroy reaches the client after it and goes
  nowhere, as libwayland hands the client a null surface; after the
  proxy's `delete_id`, an event that names the surface fails the case,
  as does one the proxy sends after it got the destroy. A `Settle` follows each reopen. One map,
  `sim::surfaces::Surfaces`, holds every field's guest and host
  surfaces, a generation each, and the host surfaces the guest
  destroyed until the proxy reuses their IDs; the display, the world,
  `SpecText` and the shadow share it. Reopens run in free cases only:
  the reference has no window to close, and the focus the new window
  takes is the window manager's, not the user's.

There are two generator modes:

- **Free.** Any timing. Used by the exact oracle, which is valid for
  every timing.
- **Settled.** Used by both oracles. The rules:
  - **Every `Click` and `Tap` is preceded and followed by a `Settle`.**
    If Exo handles a key after the press but before the press's
    `reset`, the proxy drops that key's result by design (the fence).
    The real result then differs from the reference, which applies
    every key.
  - **Every `Focus` is preceded by a `Settle`, and followed by one.**
    The activation needs a round trip, and keys typed inside it go raw
    (E7). So is every restart, for the same reason.

Each mode gets half of the cases.

### 4.7 The oracles

**Every step:**

- the clients' and Exo's protocol assertions;
- `Session`'s `assert_no_orphans`, which after C3a includes
  `ImeCore::assert_invariants`;
- no `wl_display.error`.

**Tier 1: the exact oracle, at the Exo boundary. All cases.**

- **Log.** The DST records the proxy's log: every message delivered to
  the proxy and every message it sent, in order, with times.
- **Reference.** `oracle::SpecText` interprets that log with the text
  semantics of section 2, on one `TextModel` per field. It takes the
  proxy's own activation decisions (`activate`, `deactivate`, the
  barriers, `reset` and the fence) from its output, as given.
  Host-side correctness is checked separately, by the field-state check
  below and the core's row tests. From the log it tracks:
  - the gate: attached by the last activation step, with no later Exo
    `leave` or `deactivate`, and no open fence;
  - the shown preedit;
  - the focus.

  It then applies the events in the logged order:
  - H7 to X2, by their rows;
  - F4 at each press or down on the composing surface: commit the shown
    preedit, then move the cursor to the target;
  - forwarded `wl_keyboard.key`, through `insert_key` (raw keys
    included);
  - F1 and F2 on focus changes. Since C5, F1 commits the shown preedit
    before the field resets, as does Exo's own detach (H2) with a
    preedit its confirmation left. At F2, which commits nothing,
    `SpecText` sees the guest's `wl_surface.destroy`: a text input
    focused on that surface must leave it in the same step, while it is
    valid, and the field detaches, since the seat's focus projected
    again is no surface. It then skips the keyboard `enter` and `leave`,
    the pointer `enter` and the touch `down` that name a destroyed host
    surface, which the proxy drops; a press with no pointer surface
    lands nowhere. A repair that stays on the same surface cannot happen
    at F2: the focus registry retires every keyboard on the destroyed
    surface before the repair reads the seat's focus.
- **Shadow.** `oracle::Shadow` is an ideal `TextModel` per field fed the
  proxy's **actual** `P→C` output: transactions at `done`, keys and
  presses. It has no client drops.
- **Checks at quiescence:**
  1. **Text.** For each field, `Shadow` equals `SpecText` in text,
     cursor and preedit. This is exact: there are no fates and no
     inconclusive outcome. It is independent of client lag, because both
     sides are ideal clients.
  2. **Composition.** For the focused field's attached text input,
     Exo's `composition` equals `SpecText`'s preedit.
  3. **Field state.** Exo's attached field has the content type,
     support, surrounding text and cursor rectangle that the client last
     committed. This is today's fuzzer check, moved here.
- **Counters** (in the summary):
  - late results applied after a fence (P3);
  - text dropped by a fence;
  - raw keys;
  - deletes and preedit regions, those over a shown preedit, and those
    Exo's text measures to nothing; the proxy's drops by cause, and those
    of a guest that held Exo's text (4.8);
  - guest reports that crossed an edit in flight (P4);
  - autorepeats the engine got; empty confirmations that recovered a held
    key, skipped its pair as `wl_keyboard.key` had delivered it, found
    no eligible key, turned out to be Exo's before a new press or a
    detach, or were dropped as their text input left the gate first
    (X2'); Backspace leases armed, and ended;
  - surfaces the guest destroyed, and those that had the focus (F2);
  - client drops, by reason;
  - one counter per draw.

  A counter that stays at zero over a full run fails the run, as the
  fuzzer's coverage counters do.

**Tier 2: the user-intent oracle. Settled cases only.**

- The real client model's `TextModel` per field must equal
  `Reference::fields()` from #73, in text, cursor and preedit, after
  `Reference::apply` has run over the same actions.
- The client's focused field must equal `Reference::focus()`.
- A difference is reported with the client's drop log.
- **Per-client Backspace (D4 decision).** A passed Backspace after a
  precomposed Hangul syllable deletes the whole character in #73's
  `TextModel::insert_key`, but GTK re-inserts its NFD prefix (C14): 가
  becomes ᄀ. The `Reference` therefore takes a per-client flag for
  Backspace, set for the GTK models. The alternative, excluding GTK cases
  with a passed Backspace after Hangul, would leave C14 unchecked.
- **NFC normalisation (D4 decision).** After a Backspace inside a
  syllable, GTK leaves conjoining jamo: 각 then ⌫ gives U+1100 U+1161,
  not the precomposed 가. Tier 2 compares both sides after NFC
  normalisation, which maps that sequence to 가, and compares the cursor
  as the length of the normalised text before it. So the comparison
  does not depend on which form a client keeps.

**Determinism.** In CI, one case in 50 runs twice with the recorded
choices, and the two proxy logs must be byte-identical.

**Report** (one per failing case):

```
DST FAILURE seed=81321 mode=free client=Gtk{v4:false} ext=11 e6=confirm e8=text-first e9=commit lag=slow
  check: tier1 text, field 0
  spec:   "0가나|1"   preedit ""
  shadow: "0가가나|1" preedit ""
  minimized trace (23 steps, t in ms):
    12.0 U  Key(Char('r'))           12.4 Eng preedit "ㄱ"
    13.1 E→P preedit_string "ㄱ"     13.9 P→C preedit "ㄱ" done 3
    ...
  replay: sim::replay(Case { seed: 81321, actions: vec![...], choices: vec![...] })
```

### 4.8 The known-issue list

#72 retired P2. Reaching H2 added a second issue, D6 a third and D7 a
fourth. Measuring a region in Exo's text (X1) retired the third, the
region over a shown preedit, and deciding an empty confirmation at the
host's next event (X2') the fourth, the recovery before a press. C5
retired P1 (`FocusWhileComposing`) and the lost detach
(`RestartLosesUntrackedComposition`): the guest commits the shown
preedit at a focus loss (F1) and at Exo's own detach (H2). They were the
only issues of tier 2, so the classifier lost its prediction path, and
every failure of the user's intent fails the run.

Measuring a delete or region in the Exo mirror, or dropping it (design
single-exo-model), retired the last four, all of X1: a region made in
text older than a report the proxy had committed
(`RegionCrossesReport`), a range IMA sent after the commit of the key it
held it for (`StaleRegionAfterCommit`), a region over a composition Exo
had dropped (`RegionOverUnheldPreedit`), and a region in text holding
edits the guest had yet to report (`RegionWhileAhead`). Each came from
approximating Exo's text by the guest's last report. The list is empty;
the enum stays, so that a PR that finds an issue adds its variant and
its trigger to `classify`, and the PR that fixes it deletes both.

```rust
// sim/oracle.rs
/// A tier-1 failure the proxy is known to cause. It is counted, not
/// failed. The PR that fixes it deletes its variant.
enum KnownIssue {}
```

**The Exo mirror** (M2 to M4, `specs/exo-mirror.md`; PR A and PR B of
`specs/single-exo-model.md`). From v11 of the extension each text input
has a mirror, which measures X1 and H9 (2.2) while the guest is in step
with Exo; the proxy drops every other delete and region, and below v11
all of them. `SpecText` measures each delete and region in what the
simulated Exo really held before it (`RegionTruth`, which Exo's model
attaches to each delete and region), and the guest must get exactly that
transaction or none (`SpecText::resolve`); below v11, none. After a
region the guest did not get, the composition check waits for Exo's next
preedit, commit or confirmation (`SpecText::region_dropped`). The proxy
counts its drops by cause (`Counts::drops`: no mirror, an unsure mirror,
keys or presses the guest had not stated, a crossing report, a guest out
of step), and `SpecText` counts those where the guest held Exo's text,
without its composition, which it showed, cursor included
(`drops_truth_in_step`): the drops a sharper mirror could avoid. A
settled case may make no drop with a mirror; below v11 the reference
editor expects none of Exo's deletes and regions to reach the field
(`Typing::unmeasured`), so a reconverted region and an accepted
suggestion double their text there. An ideal client's text is no guide
to whether the guest is in step, since a client model's can differ
(GTK's Backspace leaves a jamo, C14; GTK3 reports a click's cursor
late), so the guest model is judged by tier 2 and by the mirror check: at
every `done` of a sync, a mirror that says it is exact must hold the
simulated tracker's prediction at that sync (`Exo::synced`, a `mirror`
check), and a history that fits the tracker's: each known state the
fingerprint of the tracker's, an unknown one any state, and a leading
unknown one any number of them. The display dispatches a surrounding
text or a `reset` and the sync glued to it in one read, as Exo's
libwayland does. Report suppression (M4, G9c) retired
`ReportDropsComposition`: a v3 guest's report leaves out its preedit,
which names no state, so Exo dropped its composition (E4). `SpecText`
accepts a withheld report only while the guest shows a preedit, and the
world holds it to the simulated tracker: its history must hold the
report with that preedit as its composition at the report's cursor
(`Exo::holds_with_composition`), which the field check asks of Exo's
surrounding text too.

**Mirror health** (PR A). The mirror keeps its history as fingerprints,
up to 4,096, so a long field no longer makes it unsure; a split report
makes it unsure only when an event of Exo's arrives while the report is
on its way; a highlight makes only the selection vague, until the next
event that sets it; and any report Exo must reset to heals it
(INVARIANTS, "Exo's deletes and regions are measured in the Exo mirror,
or dropped"). Before PR B, in 20,000 cases from seed 0 (release mode),
the events the mirror could not measure were 0 in the settled cases and
19 in the free ones: 11 for keys or presses the guest had not stated
(`Unseen`), 4 for a crossing report (`Crossing`), 4 for a guest out of
step (`OutOfStep`), and none for an unsure mirror (`Unsure`). PR B drops
them.

**Single model** (PR B). In 20,000 cases from seed 0 (release mode), with
no failure and no known issue, the proxy dropped, in the settled cases,
1,401 deletes and regions for want of a mirror (below v11) and none with
one; in the free cases, 1,433 for want of a mirror and 24 with one: 11
`Unseen`, 4 `Crossing`, 9 `OutOfStep` and 0 `Unsure`, 8 of them with the
guest holding Exo's text (`drops_truth_in_step`). The first of them in
each case are PR A's 19 events, in the same 18 cases. The other 5 follow
a drop, in 3 of those cases (seeds 104, 6916 and 15948): a dropped event
leaves the guest out of step until a report resets Exo with no edit of
Exo's since the guest's commit before it, where PR A's fallback, when it
agreed with the mirror, kept the guest in step.

### 4.9 Golden traces for the staged switch

C3-pre committed the goldens (`sim/golden.rs` and `sim/golden.txt`), and
C3c deleted them, with the harness's record of the bytes the proxy sent,
once every stage had matched them unblessed. They worked as follows:

- **Recording.** `SOMMELIER_DST_BLESS=1` writes `sim/golden.txt`, with
  one line per seed for 2,000 seeds: a hash of the full `P→C` and `P→E`
  byte streams, and of the case's choices. A keymap's size, which
  depends on the machine's XKB data, is left out.
- **The test.** `golden_traces_match` replays each seed and compares the
  hashes. For a mismatch it prints the seed and the 16 messages that
  hold the first difference, which a checkpoint every 16 messages
  locates. It runs in CI from C3-pre to C3c, and C3c deletes it and the
  file.
- **Freeze.** No DST model, generator or proxy behaviour change may
  merge between C3-pre and C3c. A golden then always describes the
  pre-switch code.

### 4.10 How it plugs into today's proxy

- It uses only `proxy::harness::Session`. That feeds bytes into the
  `Client`'s read buffers, calls the real `handle_msgs`, and decodes the
  output from socket pairs into `AnyRequest` and `AnyEvent`. The DST
  therefore tests today's handlers and, after the switch, the core, with
  no change.
- **Additions to `Session`:**
  - `try_guest` and `try_host` return `false` instead of asserting, so
    that a session end becomes a reported DST failure, not a panic;
  - `Session::touch` comes from #72;
  - `pointer_on` exists already.
- **Globals** are advertised through `Session::advertise` and `bind`:
  `wl_compositor`, `wl_seat`, `zwp_text_input_manager_v1`,
  `zcr_text_input_extension_v1` at the drawn version (or absent), and
  `zcr_keyboard_extension_v1` v2.
- **The display (D4 decision).** D4 plays the host's display around the
  Exo model, which leaves it out (4.3). It answers each `wl_display.sync`
  with `wl_callback.done` and `delete_id`, and each destructor with its
  `delete_id`. Before it answers a sync, it takes the Exo model's
  pending events (`Exo::take_events`), so the `done` follows every event
  of earlier requests (E11).
- **The keymap (D4 decision).** `keysym_to_evdev_keycode` in
  `handler/ime_keys.rs` compiles the system's default XKB keymap from
  names, so it reads the `XKB_DEFAULT_*` environment variables. The DST
  pins those variables for the test process, or injects the keymap, so
  that a replay on another machine sends the same keycodes.
- **Runtime.** Each case runs in
  `tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(..)`.
  `enable_all` is needed because `WaylandConnection` uses an `AsyncFd`.
  The DST commits no dma-buf, so no fence parks and every await
  completes at once.
- **Before the DST,** P0 (#75, merged) removed every hash collection
  from the crate, and clippy now rejects them (B8). The host ID
  allocator's LIFO reuse of freed IDs still makes later IDs depend on
  the order objects were retired; P0 made every bulk retirement walk an
  ordered map, and the determinism check covers what is left.

### 4.11 Seeds, shrinking, run time and CI

- **Environment.**
  - `SOMMELIER_DST_SEED` (default 0) and `SOMMELIER_DST_CASES`
    (default 600) work as the fuzzer's variables do.
  - `SOMMELIER_DST_CLIENT=chromium|gtk3|gtk4` restricts the client
    models.
  - `SOMMELIER_DST_MODE=free|settled` restricts the modes.
  - A seed fixes everything, timing included. CONTRIBUTING says the
    opposite of the fuzzer; the determinism check enforces it here.
- **Shrinking** (`shrink.rs`). It has a budget of 2,000 re-runs, and it
  keeps a reduction only if the same check still fails:
  1. ddmin over `actions`;
  2. zero runs of `choices` from the end, toward minimum latency;
  3. truncate `choices`.

  It prints the minimized trace and a `replay` snippet, which goes into
  `regressions.rs` as a test.
- **Run time.**
  - One delivery is one `handle_msgs` plus a few socket-pair syscalls.
  - One case is about 10 to 60 actions, at roughly 5 to 10 deliveries
    each.
  - D4 measures one case and records the range in CONTRIBUTING. The
    target is that the default run adds at most 15 s to the wall clock
    of the `sommelier` binary's tests on CI's runner, the slowest test
    binary.
  - Larger runs are local, for example
    `SOMMELIER_DST_CASES=100000 SOMMELIER_DST_SEED=1000000 cargo test -p sommelier --bins sim::`.
- **CI tests.**
  - `ime_reaches_the_specified_text` (tier 1) and
    `ime_reaches_the_intended_text` (tier 2) run in the normal
    `cargo test` job.
  - INVARIANTS cites them under the press rule, "Focus teardown" and
    "Every commit…", so `check-cited-tests.py` keeps them.
  - `golden_traces_match` ran from C3-pre to C3c, which deleted it.
  - CONTRIBUTING gets a "Simulation test" section next to "Randomized
    tests".

---

## 5. Migration plan

### 5.1 The PRs

Each PR passes `cargo fmt --check`, `clippy -D warnings`, every test and
`check-cited-tests.py` on its own.

**Size convention:** new or changed lines, as production / test. Moved
lines are listed separately. For a PR that exists, the size is its own
(doc comments and license headers included); otherwise it is the plan's.

**Status at revision 3** (2026-11-19): six PRs of the plan are merged or
in review.

| Step | PR | Status |
|---|---|---|
| D1 | #73 | merged |
| P0 | #75 | merged |
| T1 | #76 and #80 | #76 merged; #80 in review |
| C1 and C2 | #82, one PR | in review (draft) |
| D2 | #83 | in review (draft) |
| D3 | #79 | in review (draft) |

| PR | Scope | Depends on | Size (prod / test) |
|---|---|---|---|
| **#72** | Touch-down ends the composition. **Merged.** | — | merged |
| **#73 = D1** | `sim/hangul.rs`, `sim/editor.rs`. **Merged.** | — | 0 / +1,086 |
| **P0 = #75** | Determinism. **Merged.** The crate holds no `HashMap` or `HashSet`, and clippy's `disallowed-types` forbids them (INVARIANTS "Nothing the proxy sends depends on hash order"). One change of behaviour: the keys a `wl_keyboard.enter` reports held are numbered in the host's order, not by hash or keycode (test `keys_held_at_an_enter_are_numbered_in_the_hosts_order`). | — | merged |
| **T1 = #76 + #80** | Canonical v3 transactions on the **old** code, with the tests that pin the old form updated (section 2, Conventions). **#76 (merged)** sends a reset commit after its preedit clear, and a region's delete before its new preedit, in mutter's and KWin's order. **#80 (in review)** sends no empty preedit to a guest that shows none, no `delete(0, 0)` and no `commit_string("")`. It gives an empty preedit the cursor `(0, 0)` and clears a shown preedit before a region's delete. An empty region or commit no longer counts as an edit. This leaves the switch free of wire changes. | #72 | #80: ≈ +275 / −40 lines in all, code, tests and INVARIANTS |
| **C1 + C2 = #82** | **In review.** Both steps in one PR, not wired in: the only change to existing production code is `mod ime;` and the helpers' move. C1's move is partly done: `lengths_around_cursor`, `delete_lengths`, `map_v3_content_type` and the content-type requests now live in `ime/` and the old handlers call them. `resolve_preedit_cursor` is copied until #80 merges. `HostApplied` and the old `reconcile` stay in `state` until C3a (M-1). The rest is C2: `state.rs`, `step.rs`, `host::reconcile` over `LinkState`, and `tests.rs` with a row test per row. Its departures from revision 2 are in 3.7. | #76 | ≈ +2,075 / +1,542; −227 in `text_input.rs` (moved) |
| **#82 follow-up** | After #80 merges: delete the old `resolve_preedit_cursor` and point the old code at `ime/guest.rs`, so the copy goes. After region-over-region merges: make X1 subtract what an earlier region took, as the old code does (**done**; the single Exo model since measures X1 in the mirror). Either can ride in #82 if it is still open. | #80, region-over-region, #82 | small |
| **K1** | A pure move of the keyboard concern into `handler/ime_keys.rs`: keysym routing, held-key recovery, Backspace leases (`keysym_to_evdev_keycode`, `keyboard_for_seat`, `keyboard_for_keysym`, `held_repeat_key_for_seat`, `synthesize_ime_consumed_key_pair`, `end_backspace_repeat_*`, and `on_keysym`'s body). | #82, the old-code fixes below | 0 (moved ~400) |
| **D2 = #83** | **In review.** `sim/exo.rs` (moved from `proxy/fuzz/exo.rs` and extended; the fuzzer imports it), `sim/engine.rs`, `sim/keymap.rs`, `sim/sched.rs` (`Choose`), `sim/clock.rs`. Unit tests check each E fact. Revision 3 asks it to follow IMA's rules instead of drawing them (2.4, 4.3), and to leave clicks and taps to D4 and the engine's deletes to D6. | #73 | 0 / ≈ +2,150 (moved 409) |
| **D3 = #79** | **In review.** `sim/client/{mod,chromium,gtk}.rs` and `sim/clock.rs`. One test per client fact, mutation-checked. It corrected C2, C3, C4 and C6 and added C10 to C14 (2.5). | #73 | 0 / ≈ +2,200 |
| **C2** | Part of #82 (above). **This is the one window with two `reconcile`s:** the old one over `HostApplied`, and the new one. C3a deletes the old. | — | in #82 |
| **D4** | `sim/sched.rs` (`Choices` implementing `Choose`, the channels and the step loop), the display, `user.rs`, `oracle.rs`, `golden.rs` (recording only), and `mod.rs` with `ime_reaches_the_specified_text`, `ime_reaches_the_intended_text` and the `KnownIssue` classifier. Also CONTRIBUTING, DESIGN and the INVARIANTS citations. It records these decisions: the display answers syncs and drains `Exo::take_events` before each `done` (4.10); `Exo::answer` reports lateness (4.3); the `XKB_DEFAULT_*` variables are pinned, or the keymap injected (4.10); tier 2 has a per-client Backspace and NFC normalisation (4.7); Chromium's prediction and Exo's history replay are reconciled (4.5). It lands green on today's code. A finding outside the prediction is fixed in its own PR on the old code before C3-pre, or becomes a `KnownIssue` with a reason. | D2, D3 | 0 / +700 |
| **D5** | `sim/shrink.rs`, the replay snippet, `regressions.rs`. | D4 | 0 / +200 |
| **D6** | Engine phase 2: the engine's `Delete` and Exo's absolute delete (E10), suggestion accept against `predicted`, and reconversion regions, including a region over a region. Exercises H9, X1 and P4. | D4 | 0 / +200 |
| **D7** | Holds: `UserAction::KeyDown` and `KeyUp`, extending #73's enum; engine autorepeat; the empty confirm before each unhandled key (E13, X2') and the Backspace lease. | D4 | 0 / +150 |
| **C3-pre** | No production change. Tests outside `text_input.rs` that build or read `TextInputState`, `HostApplied` or `ctx.text_inputs` directly (`keyboard.rs`, `compositor.rs`, `seat.rs`, `display.rs`, `registry.rs`, `state.rs`, `lifecycle.rs`: about 52 references) move to Session wire level, or to one `#[cfg(test)]` fixture module, `test_support::text_input`, the only test code that touches the storage. Commits `sim/golden.txt` and `golden_traces_match`, blessed on this base (**done**, 4.9). | K1, D4, D5, D6, D7, the old-code fixes below, the #82 follow-up | 0 / ±600 |
| **C3a** | **The storage switch.** `Context::text_inputs` becomes `Context::ime: ImeCore`. `TextInputState`, `HostApplied`, `ShownPreedit`, the plan structs, `prepare_*`/`finish_*` and the old `reconcile` are deleted. The old handler bodies keep their shape, but call `ImeCore`'s state methods, made `pub(crate)` for this window only, and the core's `reconcile`. `InternalCallback` merges to `TextInput { token }`. The fixture module is rewritten. `Context::assert_no_orphans` calls `ImeCore::assert_invariants`. | #82, C3-pre | ≈ −650 / ±200 |
| **C3b** | Host events (H, X and W rows) go through `step`. The old v1, extension and callback handler bodies are deleted. **Done:** `text_input::ime` steps the core and applies its effects; first the core takes the preedit cursor and styling where the old code does (B3, below H9), and the reconcile of every text input is shown to send nothing (3.7, "Reconcile timing"). | C3a | ≈ −700 / 0 |
| **C3c** | Guest requests and cross-concern inputs go through `step`. The rest of the old code is deleted, the window's `pub(crate)` items go private, and the crate-wide form of `ime_is_isolated` holds. The three shared state types move into `ime`. INVARIANTS and DESIGN are updated (Appendix A). `golden.txt` and its test are deleted. **Done:** first the core's press takes the assembly only once its commit applies (B3, below H9); the test fixtures set up only states the core can reach; the core answers a guest commit before its reconcile, which is shown to change nothing (3.7, "Answer order"). | C3b | ≈ −450 / −80 |
| **C5** | Focus loss commits the shown preedit before `leave` (F1). `KnownIssue::FocusWhileComposing` goes, and `SpecText` changes with the proxy. INVARIANTS is amended with its reasons (E6, E12; mutter and ibus commit on focus-out). E6 and E12 are now facts, so it no longer waits for a host check of the order. U5 (should the composition survive blur, as native Chrome keeps it?) and U13 decide only whether it is wanted. **Done**, with three findings. The commit must reach the guest before the keyboard's `leave`, since both client models drop input-method events once their window has lost the keyboard focus (C11, C12): the keyboard handler hands its `leave` and `enter` to the core, which sends them after the commit. A destroyed surface commits nothing (F2), so `forget_surface` sends F2 before F1. And H2 commits the preedit Exo's detach left unconfirmed (E4, E6), which retires `RestartLosesUntrackedComposition` too; Exo's confirmation precedes its `leave`, so nothing is committed twice. Tier 2 has no known issue left. | C3c | ≈ +120 / +150, −290 in `sim/` |
| **C6** | Optional: a guest-initiated reset sends a host `reset` behind a fence (G6, Q3), if the DST shows that GTK's `cause=other` reset leaves Exo composing. C13's key-binding resets make this more likely to show. | C5 | +40 / +60 |
| **C7** | Test slimming. Session wire-trace tests that a row test covers are deleted. The PR body carries a mapping table, "cited name → where it lives", for all INVARIANTS-cited IME tests, #72's three and those #80 and region-over-region add. A cited name survives as a Session test or as a row test of the same name. The merge criterion is that the table is complete and `check-cited-tests.py` passes. The fuzzer's `typing.rs` drops its Exo end-state check, which the DST owns. | C3c (after C5 and C6 if they are open) | 0 / ≈ −2,500 |

**Old-code fixes.** These change the old code's behaviour, so each must
land before C3-pre blesses the goldens. The core must match each one.

| Fix | Where | Status | The core |
|---|---|---|---|
| Keys held at an enter are numbered in the host's order. | #75 (6cfb121) | merged | KeyEffect handling, unchanged |
| No event that changes nothing: no empty preedit over none, no `delete(0, 0)`, no `commit_string("")`. An empty preedit is `(0, 0)`. A shown preedit is cleared before a region's delete. An empty region or commit is not an edit. | #80 | in review | matches (#82) |
| A region over a region deletes only what the guest has. | `kkimdev/ime-region-over-region`, stacked on #80 | ready, no PR number yet | #82 follow-up |
| G9b: an oversized surrounding text consumes the pending state, instead of being retried by every later commit. Updates `oversized_surrounding_commit_does_not_partially_advance_state`. | `kkimdev/old-g9b` | ready, no PR number yet | matches (#82) |
| A reset commit gives back the bytes a region took; a region is measured in Exo's text, from Exo's cursor, and must cover the preedit Exo holds; an empty region at Exo's cursor after that preedit commits it. | #97 (4ae8277, 7227006, 8ee6ef9) | in review | matches (`kkimdev/core-align`) |
| The empty confirm's eligible key is chosen by peek order (X2'). | `kkimdev/x2-recovery` | ready, no PR number yet | KeyEffect handling |
| An empty confirm recovers its held key only once the host's next event shows it repeated, not Exo's confirmation before a new press (X2', E13). Retires `KnownIssue::RecoveryBeforeAPress`. | `kkimdev/x2-recovery` | ready, no PR number yet | `RecoverHeldKey` loses its `done`; `ImeCore::recovery_done` |

The other PRs merged since revision 2 do not touch the IME: #77 (small
correctness and docs fixes), #78 and #81 (the SHM split and its
follow-ups).

### 5.2 Parallel groups and the merge order

**`handler/text_input.rs` merge order.** It is a single serial chain,
because every PR in it edits that file:

```
#72 → #76 → #80 → region-over-region → #82 (rebased) → G9b fix → K1 (+ peek-order fix)
    → C3-pre → C3a → C3b → C3c → C5 → C6 → C7
```

#82 touches `text_input.rs` only to call the moved helpers. It merges
after #80 so that it can drop its copy of `resolve_preedit_cursor`, and
after region-over-region so that X1 can match it in the same rebase.

**Groups.** Work in a group runs in parallel. A group starts when its
dependencies have merged.

```
done               #72, #73 (D1), #74, #75 (P0), #76 (T1, first half), #77, #78, #81
G1  now (review)   #80 → region-over-region  ‖  #79 (D3)  ‖  #83 (D2)  ‖  #82 (C1+C2), review only
G2                 #82 rebased and merged, with its follow-up  ‖  D4 (after #79, #83)
G3                 G9b fix → K1 + peek-order fix  ‖  D5 ‖ D6 ‖ D7 (after D4)
G4  freeze starts  C3-pre (after K1, D4–D7)
G5                 C3a → C3b → C3c      (DST and proxy frozen until C3c)
G6                 C5 → C6 → C7
```

Three things can run in parallel now: the `text_input.rs` chain (#80,
then region-over-region), the two simulation PRs (#79 and #83, which
touch only `sim/` and the fuzzer's Exo import), and #82's review. The
simulation PRs share `sim/clock.rs`; the second to merge drops its copy.
D4 is the critical path of track D. Once #79 and #83 merge, it is the
only DST work that can start, and D5 to D7 wait for it.

The most implementers at once is about five, in G3:

- D5, D6 and D7 are confined to `sim/`;
- the G9b fix and K1 are on the `text_input.rs` chain, in that order.

### 5.3 The staged switch and coexistence

- **Before C3a.** The new core exists only as tested code. It shares
  `serial.rs`, `guest.rs`'s helpers and the content-type mapping with
  the old code (moved in #82). The duplicates are `reconcile`, from #82
  to C3a, and `resolve_preedit_cursor`, until #82's follow-up after #80.
- **The gate for C3a, C3b and C3c:**
  - fmt, clippy, every test and `check-cited-tests.py`;
  - **`golden_traces_match` byte-identical on 2,000 seeds**;
  - the DST's default run, plus 20,000 seeds run locally;
  - the fuzzer's default run.

  Before C3c merges, 100,000 DST seeds and 200,000 fuzz cases run
  locally as a soak. That is a recorded result, not a CI gate.
- **Review rule.** C3a to C3c carry no logic change. A golden mismatch,
  or a changed assertion in a test not listed in the PR, is a defect.
- **No runtime flag.** Two cores on one connection would both send, and
  a flag keeps two owners alive. The ranked alternatives were:
  1. **The staged switch under goldens (chosen).** Each stage is
     reviewable, and nothing lingers.
  2. One atomic switch. It is smaller in PR count, but about 4,000
     changed lines in one review, and C3-pre's test churn would land with
     it.
  3. A test-only shadow mode that runs both cores and diffs them. It
     gives stronger evidence than the goldens only for inputs the
     goldens do not cover, and costs about 300 temporary lines.
- **Rollback.** Each stage reverts on its own.
- **Behaviour changes come only after C3c** (C5, C6), one per PR, each
  visible in the DST.

### 5.4 Cited tests through the switch

- All 45 INVARIANTS-cited IME tests and #72's three are Session (wire)
  tests, or tests of pure functions that C1 and C2 move.
  - C3a to C3c must pass them **unchanged**, which the goldens and the
    review rule already require.
  - Tests of deleted internals, such as `TextInputState` unit tests in
    `state/input.rs`, are rewritten against `ImeCore` in C3a, under the
    same names.
- Row tests are named `row_<id>_<what>`, or keep a cited name when they
  replace a cited test in C7.

### 5.5 Code size

Counted as new or changed lines, excluding moved lines:

| Area | Today | After C7 | Change |
|---|---|---|---|
| IME production (`text_input.rs` IME parts, `state/input.rs` text-input parts) | ≈2,500 | `ime/` ≈1,350 + adapters ≈300 | **≈ −850** |
| Keyboard-concern code moved by K1 | ≈400 | ≈400 | 0 |
| IME tests (`text_input.rs` tests, text-input parts of `input.rs`) | ≈4,900 | row tests ≈900 + Session ≈1,500 | ≈ −2,500 |
| Fuzzer text-field model (`fuzz/exo.rs` moved; the end-state check in `typing.rs`) | ≈550 | ≈400 (inside `sim/exo.rs`) | ≈ −150 |
| DST (`sim/`, #73 included) | 0 | ≈3,450 (≈3,040 new + 409 moved) | +3,040 |
| **Total** | | | **≈ −450 to +50, roughly flat** |

The production IME code shrinks by about a third, and the per-handler
detail that section 2 now owns leaves INVARIANTS. The DST is the price
of B9. If the owner wants a net decrease, the next lever is C7's
Session tests: about 1,500 lines could go to about 800 if tier 1 is
trusted for wire traces. That is not planned.

**Measured at revision 3.** The first PRs ran over these budgets, and
the table above has not been recomputed until C3c gives the adapter's
real size:

- `ime/` in #82 is about 2,075 lines against ≈1,350. That count
  includes license headers and doc comments, which revision 2's budget
  did not.
- #82's row tests are 1,542 lines against ≈900.
- D2 (#83) and D3 (#79) are about 4,350 test lines against 1,150 (4.1).

So the DST row is now about +6,300 rather than +3,040, and the total is
no longer flat. The lever named above, C7's Session tests, is the one
place to win it back.

---

## 6. Risks and open questions

### 6.1 Risks

| Risk | Mitigation |
|---|---|
| **The models are wrong** (Exo, the engine or the clients), so the DST passes on traces real hosts never produce, or fails on impossible ones. | Every model rule cites a source (2.4, 2.5) or is marked [U] and **drawn** (E9, the extension version), so both branches stay covered. Revision 3 collapsed E6, E8, E12 and U11 from Chromium's source; the host checks in 6.2 collapse the rest. A DST failure is triaged against the citations before any proxy fix. |
| **`SpecText` and the core share a misreading of section 2.** | Tier 2 checks the outcome against user intent independently of section 2. Row tests pin each row against today's code until C3c, and the goldens pin the wire. |
| **The DST never exercises a selection**, because #73's `TextModel` has none. | H9's selection drop and the selection rules of C5 and C9 stay pinned by row tests and Session tests. If a selection-related bug ever escapes, adding `anchor` back to `TextModel` is a local change in `sim/`. |
| **The clock is unrealistic.** | Latencies come from three lag profiles with a spike, and the client's 100 ms guard and the user's timing use the same clock. The host check U6, with timestamps, calibrates the profiles. |
| **The freeze from C3-pre to C3c** blocks other DST or behaviour work. | The stages are small and serial. Unrelated proxy PRs (outside text input) can still land, if the goldens still match. |
| **Switch regressions.** | T1 removes every intended wire difference first. Each stage is golden-identical and passes the unchanged suite, with the local soak before C3c. Each stage reverts on its own. |
| **DST run time** in CI. | Measured in D4 against a 15 s budget. `SOMMELIER_DST_CASES` scales it. |
| **User text in logs.** | The replay log is `trace` only, off by default. HANDOVER's U-check instructions say to scrub it before sharing. |
| **Upstream drift.** Exo's v1 path is unsupported (RESEARCH Q4), and Chromium's v3 client changes (crrev 8817260 is pending). | Section 2 is the one place to update. The client models carry their revisions, and a changed fact is a row change plus a model change. |

### 6.2 Unknowns and manual host checks

Each check runs on a Chromebook against an isolated test proxy, never by
touching the primary Sommelier service (INVARIANTS Manual verification).
Use `RUST_LOG=sommelier::ime=trace`, which feeds `from_log` (3.3), and
`WAYLAND_DEBUG=1` on the guest client.

| ID | Unknown | Check | Collapses |
|---|---|---|---|
| U1 | Does Exo ever send `zwp_text_input_v1.keysym`? | Log a week of Korean and English typing. | H10, K1's size |
| U4 | Does the Korean engine commit a confirmable composition with `commit_string` or with `confirm_preedit`? E15's Space commits "가 " with `commit_string`, but that text is not the composition. | Type "가", then Enter, or click elsewhere. | E9 |
| U5 | Should the composition be kept on blur? Native Chrome keeps it. C5 commits it instead, as mutter and ibus do; keeping it would need Exo to keep its composition across the detach, which E6 rules out. | Type "가" in a native Chrome field, Alt-Tab, and compare with the guest. | C5 |
| U6 | How often does an engine commit for a key pressed just before a click arrive after the fence (P3), and what are real engine latencies? | HANDOVER's "Space and click at the same instant", counted, with timestamps. | Q1; the lag profiles |
| U7 | Does Exo send `wl_touch` to Crostini clients, and in what order relative to text-input events? | Tablet mode. | F4 for touch (#72) |
| U8 | Does the `done` that answers each commit move the candidate window with the caret? | HANDOVER's existing check. | G9c |
| U9 | Does the ARC placement policy turn off the IME for placed windows? | RESEARCH Q3. | Out of scope; document only. |
| U10 | In GTK3, does `GtkEntry` update its cursor location before the click's release? This decides whether C7's release-time enable happens. | `WAYLAND_DEBUG` on gedit while clicking. | The GTK3 model's C7 |
| U12 | Does the ChromeOS Korean engine combine two lone consonants (ㄱ + ㅅ → ㄳ), as libhangul's default does and #73's composer does? | Type ㄱ then ㅅ with no vowel. | The `Composer`'s lone-consonant rule |
| U13 | Does Chromium's blur follow the keyboard focus or the xdg `activated` state? If it follows `activated`, which can come before the keyboard's `leave`, Chromium blurs before C5's commit reaches it and drops that commit (C11), so it keeps the syllable only if its blur confirms it (U14). | `WAYLAND_DEBUG` on a Chromium guest while Alt-Tabbing, and while focusing another guest window. | C11; P1; C5 |
| U14 | Does Chromium's blur call `ResetContext`, and so confirm a composition locally? Since C5 the proxy's commit reaches it before the blur, so a local confirmation finds nothing composed; a double commit would mean the commit came after it. | Type "가" in a Chromium guest, Alt-Tab, and come back. | C11; P1; C5 |
| U15 | Does a Chromium tap-down finish the composition, or only the tap's lift? | Tablet mode: type "가", then touch the field and hold. | C4 for touch |

**Settled in revision 3, from Chromium's source** (2.4):

- U2 (a key's text against its key events) is E8: the consumed bit
  decides it.
- U3 (whether a detach confirms, and its order against the keyboard's
  `leave`) is E6 and E12. A detach always confirms first, and the
  keyboard hears of a focus change first.
- U11 (an empty `confirm_preedit` for a consumed autorepeat) is E13: one
  comes before every unhandled key at v11, and the captured trace agrees.

They are no longer drawn. A host check of any of them is still useful,
as a check of the reading.

### 6.3 Open design questions

- **Q1. Can the engine-latency window (E5, P3) be closed?**
  - Option 1: replace `reset` with a `deactivate`, a barrier and an
    `activate`, so that Exo's own confirm commits with the engine's final
    text. INVARIANTS rejects this today, because keys bypass the input
    method until the `activate`.
  - Option 2: correlate by `peek_key`. E8 is now a fact, so the order
    of a key's text and its `peek_key` is known.
  - Recommendation: measure with U6 and the P3 counter, then decide.
- **Q2. Mirror Exo's `SurroundingTextTracker` for deletes (P4)?**
  Resolved: the Exo mirror (`ime/mirror.rs`) is the one model of Exo's
  text, and measures every delete and region, or drops it (design
  single-exo-model).
- **Q3. Guest-initiated reset (G6, C6).** GTK's reset is a commit with
  `cause=other`, and GTK4 sends one on every press, which F4 already
  covers. Decide on DST evidence.
- **Q4. Subsurface presses.** mutter matches the surface and KWin the
  window. Chromium and Firefox subsurfaces have an empty input region.
  Leave as is. The DST does not model subsurfaces.
- **Q5.** Resolved into G9c: a rejected enable still applies the rest of
  its commit, as today. Tightening it would be a one-line change in the
  Commit arm, if wanted.
- **Q6.** Resolved: the recovered-key `done` is
  `GuestTxn::default().encode`, the same encoder.

---

## Appendix A. INVARIANTS rules: kept, moved or amended

C3c applied this table to INVARIANTS and DESIGN.

| Rule (Keyboard and IME unless noted) | In the new core | Change |
|---|---|---|
| Content hints come from the v3 XML; `set_input_type` replaces v1 `set_content_type` | `ime/host.rs` (moved) | none |
| The host gets each content type once it can apply it, and only then | `LinkState`, the end-of-step reconcile, H2, W1, R1, R2 | Names: `HostApplied` becomes `LinkState`; reconcile runs after every input |
| One text input on a connection is enabled at a time | `FocusedField.committed.enabled`, the Commit arm, I3 | Text updated; staleness now impossible |
| Focus teardown sends `leave` itself | F1, F2 | C5: F2 goes before F1 in `forget_surface`; a new rule, "A field the focus leaves commits its preedit first" (F1, H2's commit in "The host gets each content type…") |
| Every v3 commit counts | `CommitCount`, G9 | Names: `TextInputState::count_guest_commit` becomes `CommitCount::bump`, called in the Commit arm |
| Exo's deletes are absolute and stand alone | H9, X1, the Exo mirror | `unreported_host_edit` went with the single model: deletes and regions are measured in the mirror, or dropped |
| Every commit that leaves a field enabled gets a `done` | G9b, G9c | Wording: GTK3 sends its enable on the click's release, not right after the press [F]. The reasoning is unchanged, because the release follows the press, so the press has already ended the composition |
| A press ends the composition on its surface (#72) | F4, W2, the fence beside the link | `host_composition_is_current` becomes `TextInput::current`; `TextInputState::reset_fence` becomes `TextInput::fence`; the callback names merge. The fence's lifetime is **unchanged** |
| Wire messages: byte-indexed strings that are not UTF-8 | G5, H7' | none |
| A v3 transaction's events go in one order (#76, #80) | `GuestTxn::encode` | Names: the handlers that send each order become the one encoder; `guest_txn_encodes_in_canonical_order` pins every combination |
| Wire messages: nothing the proxy sends depends on hash order (#75) | `BTreeMap` in `ImeCore` | none |
| Object lifetimes: synthetic objects; releasing a parent; a host-destroyed text input | G1, G2, G10, F3, F6, R1, `Env::host_seat` | none |
| Keymap, key-generation, recovered-serial and modifier rules | The keyboard concern (`ime_keys`, `keyboard.rs`) | none |

---

## Appendix B. Review responses

Revision 3's changes come first, with their sources. Revision 2's
responses follow. Those reviews are `claude-review-design.md` (finding
IDs as written; this appendix labels its unnumbered Low items CL-1 to
CL-7 and its over-engineering rows OE-1 to OE-7, in order) and
`oc-review-design.md` (deepseek). Each of those findings was checked
against the code at `2546178` and #72's branch. Where revision 3
supersedes a revision 2 response (CL-3, CL-6, H-7, M1), the revision 3
table says so.

### Revision 3

| Change | Where | Source |
|---|---|---|
| The final canonical wire order: a clearing `preedit("")` only when a shown preedit is cleared, deleted around or committed over; `delete` only if non-zero; `commit` only if non-empty; a new non-empty preedit; `done`. An empty preedit is `(0, 0)`. The G9c answer repeats an unchanged preedit. Client evidence: Chromium, GTK, Qt and kitty are order-neutral; winit and SDL3 are order-sensitive. Supersedes CL-3. | 2 Conventions; 3.2 | #76 (merged); #80; REVIEW2 "Design doc canonical order" and "After #80 merges" |
| H7, H8 and X1: an empty `t`, commit or region sends nothing beyond the clear and makes no edit. | 2.2 | #80 |
| X1 over a region deletes only what the guest has, and a region over a shown preedit is measured in Exo's text; the core must match. | 2.2; 3.7; 5.1 | branch `kkimdev/ime-region-over-region`; REVIEW2 "Region over a region" |
| X2' lease outcomes corrected; the held-key peek sequence is open. | 2.2 | #82's `KeyEffect` doc; `on_confirm_preedit`; #75 (6cfb121); REVIEW2 "Held-key peek sequence" |
| E6, E8, E12 and U11 (now E13) settled as [F]. E5's reset race has three outcomes. E4 replays the tracker's history. E14: Exo advertises v14. E15: the Korean engine consumes Space. Supersedes CL-6's and H-7's "drawn". | 2.4; 4.3; 6.2 | Chromium `input_method_ash.cc`, `text_input.cc`, `surrounding_text_tracker.cc`, `wayland_server.cc`; REVIEW2 "Design doc: Chromium source settles several unknowns"; the captured trace in `held_space_repeats_after_ime_commit_until_physical_release` |
| P1 restated: the keyboard's `leave` comes first, so the proxy drops the confirmation; Chromium may still confirm on blur. | 1.3 | E6, E12, C11 |
| C6 corrected (GTK answers a `done` only when it changed something). C2, C3 and C4 corrected, and C10 to C14 added: one text input per connection, blur resets and disables, predicted-text reports, every press resets, a guard per window, touch confirms on tap, GTK's per-field contexts, key-binding resets and NFD Backspace. §4.5 says Chromium uses one text input per connection. | 2.5; 4.5 | #79 (D3) and its fidelity reviews; REVIEW2 "Design doc fixes from D3" |
| The core's sketches follow #82: `leave_attached` replaces `LeftAttached`, `HostLink` and `new_field()`; `HostExt` carries its manager; `Planned::apply`; `Preedit::new`; I1b is declarative; I6 and I7 are new; `ime_is_isolated` and `count_bumps_only_in_the_commit_arm` are described as built; #82's departures are listed. Supersedes M1's `LeftAttached`. | 3.0 to 3.7; 2.3 | #82 (C1 and C2), its commit messages and PR body |
| D4 decisions: per-client Backspace and NFC in tier 2; pin `XKB_DEFAULT_*` or inject the keymap; the display answers syncs and drains `take_events` before each `done`; `sim/clock.rs` is shared; the `Choose` trait; `Exo::answer` reports lateness; Chromium's prediction and Exo's history replay are reconciled. | 4.1 to 4.10; 5.1 | REVIEW2 "D4 tier-2 per-client Backspace", "D4 normalisation", "DST keymap env"; #83 and #79 |
| The Exo model and the engine are described as #83 built them, with what revision 3 asks of it: IMA's rules in place of draws; clicks and taps in D4; deletes in D6. | 4.3; 4.4 | #83 and its review |
| Status of each PR: P0 (#75), T1 (#76, #80), D1 (#73), D2 (#83) and D3 (#79), and C1 and C2 (#82, which also did part of C1's helper move). The old-code fixes that must land before C3-pre. The serial chain and parallel groups, derived again. | 5.1; 5.2 | `git log 249fffb..4c45b04`; the open PRs' branches |
| Measured sizes. | 3.0; 4.1; 5.5 | #82, #83, #79 |

### Claude

| ID | Verdict | Where, or why |
|---|---|---|
| C1 (oracle unsound) | Accepted | 4.7: a two-tier oracle. Tier 1 (`SpecText` against the shadow) is exact for every timing, raw keys while detached included (E7, 4.3). Tier 2 runs only in settled mode (4.6). The fates and the inconclusive bucket are removed. |
| H1 (A1 observable) | Accepted | Verified: `ActivationStep::Abandon` ends the generation with Exo still attached, so A1 would reopen the gate early. A1 is dropped, and the fence lives beside the link (3.1; W2; R1). |
| H2 (clone-and-swap against G9b/G9c) | Accepted | 3.5: two failure rules. `step(&mut self)`, each arm deciding first; an "On encode failure" column in section 2. |
| H3 (C3 gate unachievable) | Accepted | C3-pre, C3a, C3b and C3c, gated on goldens (4.9, 5.3). Counted again: 52 direct references outside `text_input.rs`. |
| H4 (impossible Exo traces) | Accepted | 4.3: a confirm only when the commit equals the tracked composition; the version is drawn from {none, 7, 10, 11}; the raw-key path is modelled (E7). |
| H5 (I1 false) | Accepted | I1 is split into I1a and I1b; I4 is added (3.1). A dead seat while `Attached` is legal. |
| H6 (#72) | Accepted | P2 is retired; C4 and `TapWhileComposing` are removed; F4 covers touch; `Input::Press { surface, resend }` with the core re-sending; the rule's new title is used; citations name functions. |
| M1 (privacy versus nesting) | Accepted | The link, the fence and the composition are separate (3.1). There is a full transition table, and `LeftAttached` ties the composition to the link's exits. The six reconcile sites are named in B1. |
| M2 (`enabled` staleness) | Accepted | `enabled` lives in `FocusedField` (B7, 3.1, I3). |
| M3 (rows wrong or missing) | Accepted | X2' (the `done` whenever a held key is found; the lease detail), G9b (the `done` only if already enabled), R1 and R2 added, H9's assembly difference stated, G9c's forget, and the `Attached`-on-send comment. |
| M4 (overstated guarantees) | Accepted | 3.6 separates the compiler from privacy plus a test from the DST. The press goes through `Current::press_commit`. `FocusedField` is a struct. B6 names the DST for unwired sources. |
| M5 (KNOWN_ISSUES keyed on the reference) | Accepted | 4.8: only tier-2 failures in settled cases are classified, so the reference and the real system have settled alike. One entry. |
| M6 (coverage gaps) | Accepted | D6 (engine phase 2) and D7 (holds) come before C3-pre. |
| M7 (collisions; C1 and `HostApplied`) | Accepted | The serial chain in 5.2. C1 no longer moves `reconcile` (M-1), the group sizes are recounted, and "four implementers" is corrected. |
| CL-1 (replayable logging) | Accepted | 3.3: the trace log and `from_log`. |
| CL-2 (production size) | Accepted | 5.5: about −850 production; the total is roughly flat, stated honestly. |
| CL-3 (T1: Qt and SDL) | Noted, not adopted | Not verified here. T1's safety rests on the clients in scope, Chromium and GTK, which are [F]. Qt and SDL are not cited. |
| CL-4 (GTK3 drag threshold) | Accepted | C7 and the GTK model: "within the drag threshold and no caret jump". |
| CL-5 (tokio `enable_all`) | Accepted | 4.10. Verified: the fuzzer's runtime uses `.enable_all()`. |
| CL-6 (wrong citations) | Accepted | The harness peer is cited as `Objects::learn`, B5 is counted as eight, E12 is marked "drawn, as the fuzzer does", and line numbers are replaced by function names throughout. |
| CL-7 (P0 scope) | Accepted | P0 covers whatever the determinism check finds. |
| OE-1 (Env) | Kept | 3.3 |
| OE-2 (`Current`) | Kept, with honest wording | 3.1 and 3.6 |
| OE-3 (clone-and-swap) | Removed | 3.4 and 3.5 |
| OE-4 (BarrierTokens) | Kept | One token→callback map, in `InternalCallbacks`. The `host_text_input` guard is dropped (3.3). |
| OE-5 (KNOWN_ISSUES) | Shrunk to one | 4.8 |
| OE-6 (A1) | Removed | 3.1 |
| OE-7 (C3) | Staged | 5.1 and 5.3 |

### Inputs from the review of PR #73 (coordinator)

| Input | Verdict | Where |
|---|---|---|
| Settled mode: a `Settle` before and after every `Click` and `Tap`, and before every `Focus` | Accepted | 4.6. A `Focus` is also followed by a `Settle`, for the raw-key reason given there. |
| The composer combines two lone consonants | Accepted | 4.1; U12 |
| `TextModel` loses `anchor` and gains `delete_range` | Accepted | 4.1 and 4.5; the selection gap is in 6.1 |
| `Reference` gains `focus()` | Accepted | 4.1 and 4.7 (tier 2 also compares the focus) |

### deepseek

| ID | Verdict | Where, or why |
|---|---|---|
| C-1 (G10 leaves `enabled` dangling) | Accepted | Verified: `begin_destroy` clears `committed.enabled` today. `enabled` is now per field, so removal clears it by construction. I3 and I4 apply, and the row test `row_G10_destroy_of_the_enabled_input_lets_another_enable` is added in C2. |
| H-1 (privacy overstated) | Accepted | `Current` and the link's inner enum live in `state.rs` with private fields; `CommitCount` lives in `serial.rs`; 3.6's "Enforced by" column is rewritten. |
| H-2 (conditional reconcile) | Accepted | A reconcile of every text input at the end of every step (3.4). It is idempotent and cheap: a few comparisons and `Env` lookups per text input. |
| H-3 (two failure domains) | Accepted | 3.5. |
| H-4 (no clock) | Accepted | 4.2: a virtual clock, lag profiles, and the 100 ms guard in time. |
| H-5 (client-side drops) | Accepted | Client models log their drops (4.5). Tier 1 compares an ideal shadow, so drops do not affect it; tier 2 reports them. |
| H-6 (fates mislabel) | Superseded | The fates are removed (C1 above). |
| H-7 (E8 hard-coded) | Accepted | E8 is drawn (2.4, 4.3), and so are E6, E9, E12 and the version. |
| H-8 (seat end) | Partly accepted | Accepted: I1 is restated, R1 (Abandon) and R2 (deferred) are rows, and a dead seat while `Attached` is legal. **Rejected:** a `SeatEnded` input. `Env::host_seat` already answers for a released seat (`None`, as `live_host_seat` does today), and the unconditional end-of-step reconcile notices it at the next input, as today's code does at the next reconcile. An input would add a message with no behaviour. |
| M-1 (C1 and `reconcile` cycle) | Accepted, option (b) | C1 moves only stateless helpers. C2 has its own `reconcile` over `HostLink`, and C3a deletes the old one. The one window with two copies is stated. |
| M-2 (sizes do not add up) | Accepted | One convention, recomputed from the per-file budgets (4.1, 5.1, 5.5). |
| M-3 (cited names versus `row_*`) | Accepted | 5.4: cited tests stay unchanged through C3, row tests may keep a cited name, and C7's PR body carries the mapping table as its merge criterion. |
| M-4 (T1 presence differences) | Accepted | Verified both (`on_preedit_string` sends `preedit_string("")` unconditionally; `on_set_preedit_region` sends `delete` unconditionally). T1 canonicalises all four. |
| M-5 (assembly outside the gate) | Accepted | H4 and H5 record only while `Attached`, as today. The assembly lives in `Composition`, cleared at every exit from `Attached` through `LeftAttached`, and taken by F4. |
| M-6 (linear scan for v1) | Accepted | The adapter passes the `TiId` for v1 events (a ShadowTable lookup); extension events go through `by_ext` (3.3). |
| M-7 (clone cost) | Superseded | No clone (3.4). No cap on text inputs is needed. |
| M-8 (clippy cannot scope) | Accepted | The grep test `ime_is_isolated` enforces B5. The clippy entry is rejected, citing `sommelier/clippy.toml`. |
| M-9 (exclusion hides failures) | Accepted | 4.8: scenarios are generated and known failures classified narrowly. Any other failure on that path fails. |
| M-10 (missing rows) | Accepted | R1, R2; G9c's support-flip resend with its cited test; the rejected-enable behaviour as a G9c assertion. `SeatEnded` is rejected, as in H-8. |
| L-1 (`cursor_position` dead state) | Accepted | It is dropped from the assembly and only logged (H6). |
| L-2 (`Current.shown` for idle) | Accepted | `shown: Option<Preedit>`, and `Current::shown() -> Option<&Preedit>`. |
| L-3 (`delete_id` in effects) | Accepted | It stays with the generated destructor path (G2, G10, 3.3). |
| L-4 (`reconcile` not verbatim) | Accepted | C2 writes it over `HostLink`, with R1 and R2 stated (M-1). |
| L-5 (GTK3 release changes the press premise) | Rejected as a behaviour concern | The release follows the press, so the press commit (F4) still comes first and the rule's reasoning holds. Appendix A changes only the wording. U10 covers whether the enable happens at all. |
| L-6 (the recovered `done` outside `GuestTxn`) | Accepted | `GuestTxn::default().encode` (3.2, Q6). |
| L-7 (citation errors) | Accepted | Function names replace line numbers; the counts are fixed; C4 is gone (#72 created `touch.rs`). |
| L-8 (`ranges.rs` too small) | Accepted | Folded into `guest.rs`. |
| N-1 (two lease effects) | Accepted | `KeyEffect::Lease { outcome: LeaseOutcome }`. |
| N-2 (`Warned` in the core) | Rejected | Moving it to the adapter adds a per-object map that needs its own orphan check. Logging in a pure core does no I/O that affects its result, and `assert_invariants` ignores `Warned`. |
| N-3 (`HostOut::Retire`) | Rejected | The retirement must follow the extension's `destroy` in order. As an element of the ordered `to_host` sequence, it is one concept fewer than a separate list plus an ordering rule. |
| N-4 (deliveries per case) | Accepted | D4 measures, then states the range (4.11). |
| N-5 (which time budget) | Accepted | The wall clock of the `sommelier` binary's tests (4.11). |
| N-6 (`Preedit` "none" ambiguity) | Accepted | `preedit: Option<Preedit>`. The encoder's doc says that `shown_before` decides clear versus omit (3.2). |
| Migration assessment (keep C3 atomic; soak not in the gate; sequence K1 and C1; T1 first) | Partly accepted | Accepted: the soak is local, not a CI gate; T1 → C1 → K1 is the order. **Not adopted:** keeping C3 atomic. The coordinator's decision to stage it stands, made safe by the goldens. |
