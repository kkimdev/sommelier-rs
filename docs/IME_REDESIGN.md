# Design: a clean IME core and a deterministic simulation test for sommelier-rs

Status: proposal, revision 2, 2026-11-18. It is written against
`virtwl` at `2546178`, plus PR #72 (a touch-down ends the composition,
about to merge) and PR #73 (the simulation's Hangul composer and
reference editor, which is D1 below). Nothing in the repository was
changed to write it. Revision 2 answers two adversarial reviews;
[Appendix B](#appendix-b-review-responses) lists every finding and what
became of it.

Audience: the implementers of the PRs in [section 5](#5-migration-plan),
who may work in parallel, and any reviewer who has not followed PRs
#59 to #73.

Citations name functions rather than line numbers wherever practical, so
they survive #72 and the PRs below. "INVARIANTS *Rule*" means the bullet
of that title in `docs/INVARIANTS.md`, § Keyboard and IME unless another
section is named. After #72 the press rule is titled "A press ends the
composition on its surface".

> This plan lives in the repository so that the PRs implementing it can
> cite it. It was written from working notes that are not in the
> repository: `RESEARCH-ime.md` (sources on Exo and the clients),
> `REVIEW2.md` (open findings), and two independent reviews of an earlier
> draft (`claude-review-design.md`, `oc-review-design.md`). Every fact this
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
model of the system ([5.5](#55-code-size)).

### How to read this document

- [Section 1](#1-problem-statement): which bug classes recur, and why.
- [Section 2](#2-behavioural-specification): the behaviour, as a table.
  It is the single source of truth for the core's row tests, the DST
  models and the exact oracle.
- [Section 3](#3-the-ime-core): the core's types, its boundary and its
  guarantees, each stated with how it is enforced.
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
    KWin `master@f213b9af`.
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
| B1 | **Hidden host state mispredicted.** Exo drops a content type while no input method is attached, spends a pending surrounding-text support on the next content type, and detaches on its own. | `74b10a6` (support consumed by `set_content_type`); `8875c50` (content type lost on a deferred activation); `a1fc8c6` (replay replaced by reconcile); `14a2241` (Exo's own detach ignored); `08d0453` (a reused seat ID sent as a seat). | Partly fixed already by `HostApplied` and the pure `reconcile`. But six call sites of `reconcile_host` decide when to reconcile (`on_leave`, `complete_host_activation_barrier`, `reconcile_seat`, `apply_text_input_focus_updates`, `forget_text_input`, `on_commit`). `HostApplied` is also mutated outside it, in `end_on_host_detach`, `forget_text_input` and `commit` (`new_field`). | The link is a newtype whose transitions are named methods. `step` reconciles every text input at the end of **every** input, so no arm can forget. |
| B2 | **Events from an ended epoch** reach the guest as current. Such events were sent before a `deactivate`, before a `reset`, or before the guest reported its text. | `3e469cd` (Space plus click gave "각각"; a preedit or confirmation in flight after a click); `dc96777` (a delete measured against Exo's predicted text); the drain barrier (`fe66096`, `a1fc8c6`). | Epoch membership is three independent fields: `host` (`Draining`), `reset_fence` and `unreported_host_edit`. Each of five handlers must check each one (`host_composition_is_current`). A new trigger adds a field and five checks. | One gate, `TextInput::current()`, returns a `Current<'_>`. Only `Current`'s methods build a transaction for a host event or a press. |
| B3 | **Leftover partial-event state.** A v1 event built from several messages left its pieces behind when dropped, and they applied to the next event. | `849a935` (a dropped commit's `cursor_position` and deletes applied to the next commit). | Pieces live in `pending_*` fields. Every early return must call `discard_pending_*` (`drop_host_preedit`, `drop_host_commit`). | `Assembly::take()` runs first in every terminal event. The assembly lives in the composition, which every exit from `Attached` clears. |
| B4 | **Serial drift.** A v3 `done` serial must count every guest `commit`. | REVIEW2 H6 (unfocused commits were not counted); INVARIANTS "Every v3 commit counts". | Fixed by a private counter (`TextInputState::count_guest_commit`), but `done` events are built in eight places. | `CommitCount` lives in its own module and has one call site that bumps it. `Serial` comes only from it. One encoder reads it. |
| B5 | **Malformed v3 transactions.** A transaction lacks its `done`, a `done` fails to repeat the shown preedit, or the preedit is cleared or kept wrongly. | `51ffc5e` (no `done` after a guest commit); `9bc1f37` (two copies of the commit of a shown preedit). | v3 events are pushed by hand in eight places: `on_preedit_string`, `on_commit_string`, `on_leave`, `on_delete_surrounding_text`, `on_set_preedit_region`, `on_confirm_preedit`, `commit_shown_preedit` and `answer_guest_commit`. | `GuestTxn::encode` is the only builder of the four v3 IME events. It always ends with `done`, and it restates the preedit. |
| B6 | **A cross-concern trigger missed, or handled twice.** | `75f2c5a` (a press never ended the composition); touch (fixed by #72); INVARIANTS "Focus teardown sends `leave` itself"; `repair_destroyed_surface_focus`. | Other concerns call free functions of `text_input.rs`, and nothing lists them. | Every trigger is an `Input` variant, and `step`'s match has no wildcard. A source that is never wired (as touch was) is caught by the DST, not by the compiler. |
| B7 | **Connection-wide constraints.** | `a67f11e` (two enabled text inputs; the first stayed deaf). | A scan inside `on_commit`, and `begin_destroy` clearing the enabled bit. | `enabled` is per field, inside `FocusedField`, so ending or refocusing a field clears it by construction. The connection's enabled input is derived by a scan, never stored. One arm sets `enabled = true` and checks "at most one", and `assert_invariants` checks it again. |
| B8 | **Nondeterministic order.** | No reported bug yet, but it blocks exact replays. | `Context::text_inputs` is a `HashMap`. `reconcile_seat`, `apply_keyboard_focus_changes` and `end_composition_on_press` iterate it. | `BTreeMap` in the core. P0 also changes it before the switch. |
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

- **P1. Focus loss while composing** (user-intent oracle; known issue
  until C5).
  - The guest shows "가", and the user switches windows.
  - Exo's detach makes its input method confirm the composition, and
    that confirmation comes before the text-input `leave` (E6).
  - The order between that confirmation and `wl_keyboard.leave` is not
    known [U3]. Today's fuzzer already draws it at random.
  - If the confirmation arrives first, the proxy commits "가".
  - If `wl_keyboard.leave` arrives first, the field reset clears the
    composition without telling the guest (`TextInputState::apply_focus`),
    and the drain then drops the confirmation. Chromium confirms locally
    only on a mouse press, not on `leave` [F], so the syllable is lost.
- **P2.** Retired: a tap now ends the composition (#72).
- **P3. Engine latency** (counted by the exact oracle, never failed).
  - A key pressed just before a click can double the syllable: the
    engine's result for it arrives after the fence (INVARIANTS "A press
    ends the composition on its surface", last paragraph).
  - The exact oracle applies such results as the specification says,
    and the DST counts them.
  - Cases checked against user intent never click inside the window.
- **P4. A Chromium report crossing an edit in flight** (HANDOVER,
  deletes). It needs the engine's deletes, which D6 adds.

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
  - **Canonical wire order:** `preedit_string?`,
    `delete_surrounding_text?`, `commit_string?`, `done(count)`.
  - `preedit_string` goes out when the new preedit is non-empty, or the
    guest showed one.
  - `delete_surrounding_text` goes out only when it deletes something.
  - **Why the order is safe.** Clients apply a transaction at `done`,
    in the v3 protocol's fixed order (delete, commit, preedit), whatever
    the wire order. [F] Chromium's `ZWPTextInputV3::OnDone` buffers
    every event until `done`. [F] GTK3 `text_input_done` (`imwayland.c`)
    and GTK4 (`gtkimcontextwayland.c`) store pending values and apply
    them at `done`.
  - **Wire differences from today.** Today's handlers differ from the
    canonical form in four ways. T1 makes the old code canonical first,
    so that the switch changes nothing:
    1. A reset commit sends `commit` before `preedit`.
    2. A region sends `delete` before `preedit`.
    3. A preedit that clears nothing still sends `preedit_string("")`.
    4. An empty region sends `delete(0, 0)`.
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
| G1 | `zwp_text_input_manager_v3.get_text_input(id, seat)` | A host v1 manager exists (otherwise the request is dropped, with an error log, in the adapter) | `create_text_input`; `get_extended_text_input` if the extension is bound | `enter(surface)` if the seat's keyboard focuses a surface | `Focused` (not enabled), or `Unfocused`; link `Detached`; count 0 | n/a (no strings) | `TextInputManagerV3Handler::on_get_text_input`; INVARIANTS Object lifetimes "Synthetic objects complete their own lifecycle" |
| G2 | `zwp_text_input_manager_v3.destroy` | none | none | `delete_id`, sent by the generated local destructor path, not by the core | none | n/a | same |
| G3 | `enable` | `Focused` | none | none | `pending = V3Pending::reset(true)`. Ignored while `Unfocused`. | n/a | `TextInputV3Handler::on_enable`; `V3Pending::reset` |
| G4 | `disable` | `Focused` | none | none | `pending = V3Pending::reset(false)`. Ignored while `Unfocused`. | n/a | `on_disable` |
| G5 | `set_surrounding_text(t, c, a)` | `Focused` | none | none | If `t` is UTF-8 and `c` and `a` are char boundaries within it, pending becomes `Some(t, c, a)`. Bad offsets are ignored, with a warning. Text that is not UTF-8 makes it `Some("", 0, 0)`. | n/a (checked at G9) | INVARIANTS Wire messages "A string that is not UTF-8 does not end the session"; `on_set_surrounding_text` |
| G6 | `set_text_change_cause` | any | none | none | Ignored. Chromium never sends it [F]. GTK sends it after every surrounding text [F]. See Q3. | n/a | `on_set_text_change_cause` |
| G7 | `set_content_type(h, p)` | `Focused` | none | none | pending `content_type` | n/a | `on_set_content_type` |
| G8 | `set_cursor_rectangle` | `Focused` | none | none | pending `cursor_rect` | n/a | `on_set_cursor_rectangle` |
| G9a | `commit` | `Unfocused` | none | none | Count += 1. Nothing else. | n/a | INVARIANTS "Every v3 commit counts" |
| G9b | `commit` whose pending surrounding text does not fit a v1 message | `Focused` | **none at all**: no activation step and no content type for this commit | If the field was already enabled: `Txn{preedit: shown}` (the answer) | Count += 1. **The pending state is consumed and nothing else changes**: the committed state, the link and the composition stay. | *(this row is the failure case)*. If the answer's repeated preedit does not fit either, the shown preedit is forgotten and a bare `done(count)` goes out. | `on_commit` (the `queue_or_drop` early return); `answer_guest_commit`; tests `oversized_surrounding_commit_does_not_partially_advance_state` and `an_answer_that_cannot_repeat_the_preedit_forgets_it` |
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
3. **A new field** clears the composition and the assembly with no
   guest output, since the guest reset itself. It also unsets the link's
   content type and ends the lease.
4. **A stated surrounding text** sets the text to `Synced`. Then the
   content type is applied.

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
| H2 | v1 `leave` | `Attached` | If `shown`: `Txn{preedit: none}` | Link → `Detached` (Exo's own detach). The composition and the assembly are cleared. The fence stays. The reconcile then activates again if the field still wants input. | n/a (no strings) | INVARIANTS same rule; Exo `TextInput::OnSurfaceFocused`, `OnSurfaceDestroying` and `OnInputMethodDestroyed` [F]; `on_leave`, `end_on_host_detach` |
| H2' | v1 `leave` | `Draining` or `Detached` | none | Ignored: it answers the proxy's own `deactivate`. | n/a | same |
| H3 | `modifiers_map`, `input_panel_state`, `language`, `text_direction` | any | none | Ignored. | n/a | the handlers of the same names |
| H4 | `preedit_styling(i, l, style)` | `Attached` (dropped otherwise, as today) | none | Recorded in the assembly when `style == SELECTION`. | n/a | `on_preedit_styling` (records only while `host_is_active`) |
| H5 | `preedit_cursor(i)` | `Attached` | none | Recorded in the assembly. | n/a | `on_preedit_cursor` |
| H6 | `cursor_position(i, a)` | `Attached` | none | Logged at debug as having no v3 equivalent, and not recorded (today it is recorded and only logged later). Exo sends it only before a `commit_string` [F]. | n/a | `on_cursor_position` |
| H7 | `preedit_string(serial, t, commit)` | Gate; `t` is UTF-8 | If `t` is empty, `shown` was set and `commit` is non-empty: `Txn{commit, preedit: none}` (the reset commit; the text becomes `Ahead`). Otherwise: `Txn{preedit: t, with the cursor from the assembly}`. | `shown := t`, or none if `t` is empty. The assembly is taken. **Lease:** `LeaseOutcome::ArmIfBackspaceHeld` if `t` is empty, `shown` was set and `commit` is empty; `End` otherwise. The host serial is only logged. | The event is dropped. The assembly is taken. Nothing else changes. | `on_preedit_string`; Exo sends the preedit text as its commit text [F] |
| H7' | `preedit_string` | Gate fails, or `t` is not UTF-8 | none | Dropped. The assembly is taken. | n/a | INVARIANTS press rule (the fence); Wire messages (byte-indexed strings) |
| H8 | `commit_string(serial, t)` | Gate | `Txn{commit: t, preedit: none}` | `shown := none`. The text becomes `Ahead` if a preedit was shown or `t` is non-empty. The lease ends. The assembly is taken. | Dropped; the assembly is taken. | `on_commit_string` |
| H9 | `delete_surrounding_text(index, length)` | Gate | `Txn{delete: (before, after), preedit: none}` | `shown := none`; the text becomes `Ahead`; the lease ends. The assembly is taken. **Dropped without other change** when: there is no committed surrounding text; there is a selection; `index ≥ 0` while the text is `Ahead`; the range does not hold the cursor or does not lie on char boundaries (the first such drop warns, later ones log at debug); or the range is empty. A negative index counts from the cursor. *Intended difference: a dropped delete now takes the assembly. Today it leaves it, which is unobservable because Exo sends no styling or cursor before a delete [F].* | Dropped; the assembly is taken. | INVARIANTS "Exo's deletes are absolute and stand alone"; `on_delete_surrounding_text`. [F] Chromium drops a delete while a composition is shown (`InputMethodAuraLinux::OnDeleteSurroundingText`). |
| H10 | `keysym(...)` | `Attached` (the fence is **not** checked) | `wl_keyboard.key`, through the keyboard concern | Not a core input. `ime_keys` asks `ImeCore::keysym_target(ti)`. Unchanged. Exo sends none today [F, U1]. | n/a | `on_keysym`; INVARIANTS "A key waits for at most one late release per channel" |
| X1 | ext `set_preedit_region(index, length)` | Gate; a committed surrounding text; the range lies around the cursor on char boundaries | `Txn{delete: (before, after), preedit: (region, cursor)}` | `shown := region`; the text becomes `Ahead`; the lease ends. The assembly is taken. Not gated by `Ahead`, on purpose. | Dropped; the assembly is taken. | `on_set_preedit_region`; Exo's region is cursor-relative [F] |
| X1' | ext `set_preedit_region` | any other case | none | Dropped; the assembly is taken. A missing surrounding text warns. | n/a | same |
| X2 | ext `confirm_preedit` | Gate; `shown = Some(p)` | `Txn{commit: p, preedit: none}` | `shown := none`; the text becomes `Ahead`; the lease ends; the assembly is taken. | Dropped; the assembly is taken. (`p` was encoded once, so this cannot happen in practice.) | `on_confirm_preedit`; Exo at v11 or later confirms instead of committing [F] |
| X2' | ext `confirm_preedit` | Gate; `shown = None` | Through the keyboard concern: if the seat has an eligible held key, its press and release unless another channel already delivered it, then `done(count)`. **The `done` goes whenever a held key is found, even when its pair is skipped.** With no held key, nothing is sent. | `KeyEffect::RecoverHeldKey`. **Lease:** armed if a recovered key was Backspace; ended if a non-Backspace key was recovered, if a Backspace pair was skipped, or if no key was held. | n/a | INVARIANTS "A recovered key carries the host's serial"; `on_confirm_preedit` with `held_repeat_key_for_seat` and `synthesize_ime_consumed_key_pair` |
| X3 | ext grammar, autocorrect and occluded-bounds events | any | none | Ignored. | n/a | the handlers of the same names |
| W1 | `wl_callback.done` of a drain barrier | `Draining{b}` | none | Link → `Detached`. The final reconcile then activates if the field wants input. | n/a | `CallbackHandler::on_done`; `complete_host_activation_barrier` |
| W2 | `wl_callback.done` of a reset fence | `fence = Some(f)` | none | `fence := None`, **in any link state** (the fence lives beside the link, as today). | n/a | INVARIANTS press rule ("A focus change or the text input's end does not end it early"); `complete_reset_fence` |

### 2.3 Cross-concern inputs and reconcile outcomes

| ID | Input (owner that sends it) | Precondition | Host requests | Guest events | Next state / notes | Sources |
|---|---|---|---|---|---|---|
| F1 | Seat focus change: `wl_keyboard.enter` of another surface, `wl_keyboard.leave`, or a keyboard's end (`KeyboardFocusRegistry`, via `KeyboardHandler::on_enter`, `on_leave` and `forget_keyboard`) | Per text input on that seat, in `TiId` order | Through the reconcile: `deactivate` and a barrier, or `activate` | `leave(old)`, then `enter(new)`, each if it changed. **Proposed (C5):** first, `Txn{commit: p, preedit: none}` if `shown = Some(p)` and the old surface is still live. | The field is reset: a new `FocusedField{surface, enabled: false, text: Synced}`, or `Unfocused`, with an empty pending state. The composition and the assembly are cleared. The keyboard ends the seat's leases and peek watermarks, as today. | INVARIANTS "Focus teardown sends `leave` itself"; `apply_keyboard_focus_changes`, `apply_text_input_focus_updates`; C5 is P1 |
| F2 | Surface destroyed (`forget_surface` in `compositor.rs`) | A text input focused on that surface | as F1 | `leave(s)`, while `s` is still valid. A repair that stays on the same surface sends no `leave` or `enter`, and the field counts as disabled until enabled again. | The focus is projected again from `Env::keyboard_focus`. | INVARIANTS "Focus teardown…" and "Every commit…" (the repair case); `repair_destroyed_surface_focus` |
| F3 | `wl_seat` bound under a guest ID (`registry.rs`, `wl_seat` bind) | The text inputs of that seat | Through the reconcile: the deferred `activate`, with its content type | none | | INVARIANTS Object lifetimes "Releasing a parent does not end its children"; test `an_activation_deferred_for_an_unmapped_seat_sends_the_content_type` |
| F4 | Press: a `wl_pointer.button` press from a pointer whose `enter` named `s`, or a `wl_touch.down` on `s` (#72: `PointerHandler::on_button` and `TouchHandler::on_down`, both through `end_composition_on_press(ctx, surface, resend)`) | The enabled text input is focused on `s`; gate; `shown = Some(p)` | `zwp_text_input_v1.reset`, then a barrier (the fence) | `Txn{commit: p, preedit: none}`, then the press or down itself, re-sent by the core | `shown := none`; the assembly is taken; `fence := Some(f)`; the text becomes `Ahead`; the lease ends. A release, another surface (subsurfaces included), no composition, or an open fence changes nothing, and the press is forwarded. | INVARIANTS "A press ends the composition on its surface"; mutter (`meta_wayland_text_input_handle_event`), KWin (`InputMethodEventFilter::pointerButton` and `touchDown`, `InputMethod::commitPendingText`) [F]; Exo `TextInput::Reset` sends nothing back [F] |
| F5 | Key activity (`wl_keyboard.key`, `peek_key`) | n/a | n/a | n/a | **Not a core input.** Leases are keyboard state (`KeyGenerationRegistry::ime_repeat_owner`). The keyboard ends them on a non-Backspace press and on a Backspace release, as today. The core only emits `KeyEffect`s. | INVARIANTS "IME state is scoped to its keyboard or seat" |
| F6 | Host `delete_id` of the v1 object while the guest object is live (`forget_object`, with `host_deleted`) | any | Nothing to v1; the extension is destroyed | `delete_id`, as in G10 | Link → `Detached` with no requests. | test `a_host_destroyed_text_input_sends_nothing_to_its_host_object` |
| R1 | **Abandon** (reconcile outcome): the target is inactive, the link is `Attached`, and `Env::host_seat` is `None` | none | none | none | Link → `Detached` locally; the composition is cleared; **the fence stays**. Exo keeps its input method attached until its own focus moves; HANDOVER covers the cost. | `ActivationStep::Abandon`; HANDOVER "A text input whose guest `wl_seat` goes away…" |
| R2 | **Deferred activation** (reconcile outcome): the target is active, and the link is `Draining`, or `Detached` with no live seat | none | none | none | No change. Draining logs at debug. A missing seat warns once per text input, then logs at debug. | `reconcile_host` (the `None if target.active` arm); `TextInputState::warned_seat_missing` |

A seat that ends is **not** an input. Today's proxy notices it lazily, at
the text input's next reconcile, and the core does the same: the
unconditional end-of-step reconcile reads `Env::host_seat`. An `Attached`
link with a dead seat is therefore a normal state (I1 in 3.1;
INVARIANTS "Releasing a parent does not end its children").

### 2.4 Exo facts the models implement

| ID | Fact | Status, and how the DST treats it |
|---|---|---|
| E1 | `activate` on the surface it already has does nothing. Otherwise it detaches (sending `leave`), takes the surface, and attaches (sending `enter`) only if the seat focuses that surface. | [F] `TextInput::Activate` |
| E2 | Attaching resets the type and mode to text and keeps the flags, learning and support. Detaching sets the type to none. Exo detaches on its own on focus loss, on surface destruction and when the input method is destroyed, each time through `Deactivate`. | [F] `AttachInputMethod`, `DetachInputMethod` and the observers |
| E3 | A content type is dropped while nothing is attached. `set_input_type` and `set_content_type` consume the pending support and reset it to supported. | [F] `SetTypeModeFlags`; `zwp_text_input_manager.cc` |
| E4 | Surrounding text and caret bounds are kept with or without an input method. Exo predicts the text by applying the input method's edits. | [F] `SetSurroundingText` (`SurroundingTextTracker`) |
| E5 | `reset` cancels Exo's composition and calls `CancelComposition`. It sends nothing to the client, and it does **not** cancel a key the engine is still processing. | [F] `TextInput::Reset`; INVARIANTS press rule |
| E6 | A detach makes the input method confirm Exo's tracked composition: with `confirm_preedit` when the extension is v11 or later, with `commit_string` otherwise. The confirmation comes before the text-input `leave`. | [F] for the style. **Whether there is a confirmation at all is [U3]; drawn.** |
| E7 | A key goes through the input method before `Keyboard::OnKeyEvent`. A consumed key gets `peek_key` and no `wl_keyboard.key`. An unconsumed key gets `peek_key`, then `wl_keyboard.key` with the same serial. There are no repeats. **With no text input attached, keys skip the engine and reach the client raw**, as US-layout letters. | [F] RESEARCH Q3 (`keyboard.cc`). The raw path: [F] the input method has no client (`InputMethodAsh` with type `NONE`). |
| E8 | The order of an engine's text events for a key relative to that key's `peek_key` and `wl_keyboard.key`. | **[U2]; drawn per case: `TextBeforeKey` or `KeyBeforeText`.** |
| E9 | When the engine commits the composition it tracks, it may use `confirm_preedit` (v11 and later) or `commit_string`. A commit of text that differs from the tracked composition, such as 각 + ㅏ committing 가, is always `commit_string`. | [F] confirm confirms only the tracked composition. **The style of a confirmable commit is [U4]; drawn.** |
| E10 | `delete_surrounding_text` uses an absolute byte index into Exo's predicted text, comes with no `commit_string`, and happens only while nothing is composed. | [F] |
| E11 | Exo handles one client's requests in order, and its events to a client are in order. A `wl_display.sync`'s `done` follows every event of earlier requests and precedes every event of later ones. | [F] libwayland |
| E12 | On a focus change, Exo sends `wl_keyboard.leave`/`enter`, the text inputs' detach and attach, and, under E6, the confirmation. The relative order of the keyboard's observers and the text input's observers is not fixed. | [U3]; **drawn**, as `fuzz.rs` already does (`rng.one_in(2)` in `compositor`) |

### 2.5 Client facts the models implement

| ID | Fact | Client | Source [F] |
|---|---|---|---|
| C1 | Counts commits. Sends the cursor rectangle, surrounding text and content type (each deduplicated) **only** when `last_done_serial == commit_count`, then one `commit`. Retries after every `done`. | Chromium | `ZWPTextInputV3::DoneSerialEqualsCommitCount`, `SendPendingImeData`, `OnDone` |
| C2 | `enable` and `disable` are not gated: `enable` sends the content type, the cached text and rectangle, and a `commit`; `disable` sends a `commit`. They are sent on `enter` and `leave`, and on a change of focused node (disable, then enable). There is **no** enable on a click in the same field. | Chromium | `ZWPTextInputV3::Enable`/`Disable`; `WaylandInputMethodContext::MaybeUpdateActivated`, `UpdateFocus` |
| C3 | At `done`, regardless of serial, it applies the delete (**dropped if a composition is shown**), then the commit, then the preedit (a missing preedit clears it). It then re-sends the predicted surrounding text through C1. | Chromium | `ZWPTextInputV3::OnDone`; `InputMethodAuraLinux::OnDeleteSurroundingText`; `WaylandInputMethodContext::OnInputEventsApplied` |
| C4 | On a mouse press while it shows a preedit, it confirms the preedit locally, sends nothing, and **drops `commit_string` and preedit events for 100 ms**. With F4, the preedit is already gone at the press, so the guard arms only if F4 did not run. | Chromium | `RenderWidgetHostViewEventHandler::FinishImeCompositionSession`; `kIgnoreCommitsDuration`, `InputMethodAuraLinux::ResetContext` |
| C5 | Counts deletes in bytes from the composition, or else the selection, and deletes the selection too. | Chromium | `WaylandInputMethodContext::OnDeleteSurroundingTextAroundCursor` |
| C6 | `serial` +1 per commit. At `done` it applies delete, commit, then preedit (a missing preedit clears it). Each applied commit or delete immediately sends its state and a `commit`, regardless of serial. | GTK3, GTK4 | `text_input_done`, `notify_im_change` |
| C7 | Sends `enable`, with its full state and a `commit`, on `focus_in`/`enter`, and again on a single-click **release**. GTK3 does so only within the drag threshold and when the caret did not jump; GTK4 always, since the v3 protocol at v1 has no `show_input_panel`. | GTK3, GTK4 | `released_cb` (GTK3); `gtk_text_click_gesture_released`, `activate_osk_with_event` (GTK4) |
| C8 | On a primary press, GTK4 always resets; GTK3 resets only when `need_im_reset` is set. A reset is the state plus a `commit` with cause `other`. The preedit is not cleared locally. | GTK3, GTK4 | `gtk_text_click_gesture_pressed`; `gtk_entry_real_set_position`; `reset` |
| C9 | Deletes count bytes from the cursor and ignore the selection. Surrounding offsets are bytes; GTK3 sends `anchor = cursor`. | GTK3, GTK4 | `text_input_delete_surrounding_text`, `notify_surrounding_text` |

---

## 3. The IME core

### 3.0 File layout

```
sommelier/src/ime/
  mod.rs      Public API: ImeCore (inputs, step, keysym_target,
              assert_invariants), Input, HostIme, Effects, HostOut,
              KeyEffect, LeaseOutcome, Env, the ID newtypes.          ~170
  state.rs    TextInput, Field, FocusedField, HostLink (a newtype over
              a private enum), Composition, Assembly, ExoText,
              V3Pending, V3Committed, Current<'_>. The fields are
              private to this file, and transitions are named methods. ~300
  serial.rs   CommitCount and Serial: no public constructor, and one
              bump.                                                    ~40
  step.rs     step(): one exhaustive match on Input; the end-of-step
              reconcile; removals.                                     ~420
  host.rs     HostTarget, reconcile() over HostLink, and content-type
              mapping and encoding (map_v3_content_type and
              push_content_type, moved from handler/text_input.rs).    ~320
  guest.rs    Preedit, GuestTxn, encode(): the only builder of the v3
              preedit_string, delete_surrounding_text, commit_string
              and done. Also the range helpers (lengths_around_cursor,
              delete_lengths, resolve_preedit_cursor), moved.          ~200
  tests.rs    One row test per row of section 2: pure, no Session.     ~900 (test)

handler/text_input.rs   Adapters: decode, build an Input, call step,
                        apply the Effects; the ime trace log.          ~300
handler/ime_keys.rs     Keysym routing, held-key recovery and Backspace
                        leases, moved out of text_input.rs (PR K1).    ~400 (moved)
```

`ime` imports nothing from `handler` or `state`. It uses only `wire`
(the `Outgoing` type and `IndexedString`) and the generated `protocols`
encoders and constants. A test, `ime_is_isolated`, enforces this. It
checks two things:

- the `use` lines of `ime/`;
- that the four v3 event encoders (`zwp_text_input_v3::evt::{preedit_string,
  delete_surrounding_text, commit_string, done}`) appear in production
  code only in `ime/guest.rs`. This applies once C3c has removed the old
  handlers.

### 3.1 State

```rust
// ime/mod.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct TiId(pub u32);      // guest zwp_text_input_v3
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct SeatId(pub u32);    // guest wl_seat ID (may be dead)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct SurfaceId(pub u32); // guest wl_surface

/// A host wl_display.sync the core waits for. The core numbers its own
/// barriers. The adapter keeps the one map from token to host callback
/// (InternalCallbacks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct BarrierToken(u64);

/// The host objects behind a guest text input: immutable facts recorded
/// at creation.
#[derive(Clone, Copy, Debug)] pub struct HostTi { pub v1: u32, pub ext: Option<HostExt> }
#[derive(Clone, Copy, Debug)] pub struct HostExt { pub id: u32, pub version: u32 }

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
    pending: V3Pending,            // moved from state/input.rs
    field: Field,
    link: HostLink,
    /// The reset fence of a press (F4) until its done (W2). It lives
    /// beside the link and survives every link change, as today's
    /// TextInputState::reset_fence does.
    fence: Option<BarrierToken>,
    composition: Composition,
    warned: Warned,                // { seat_missing, delete_range }: log-only, ignored by the invariants
}

pub(super) enum Field { Unfocused, Focused(FocusedField) }

/// A named struct, so that Current can borrow it.
pub(super) struct FocusedField {
    surface: SurfaceId,
    /// The committed state: enabled, surrounding text and content type.
    /// The enabled flag lives here, so ending or refocusing the field
    /// clears it by construction (B7).
    committed: V3Committed,
    /// Whether Exo's predicted text may hold input-method edits the
    /// guest has not yet reported (today: unreported_host_edit).
    text: ExoText,
}

pub(super) enum ExoText { Synced, Ahead }

/// What the host text input has applied (today: HostApplied).
pub(super) struct HostLink(LinkState);
enum LinkState {
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
#[derive(Default)]
pub(super) struct Composition {
    shown: Option<Preedit>,
    assembly: Assembly,            // preedit_cursor and preedit_selection
}
```

**Link transitions.** This is the complete list. Each is a method of
`HostLink`, named after what happened.

| From | To | Method | Caller (row) |
|---|---|---|---|
| `Detached` | `Attached{ct: None}` | `activate_sent()` | reconcile: Activate |
| `Attached{ct}` | `Attached{ct'}` | `content_type_sent(ct')` | reconcile |
| `Attached{ct}` | `Attached{None}` | `new_field()` | G9c (new field) |
| `Attached` | `Draining{b}` | `deactivate_sent(b)` | reconcile: Deactivate |
| `Attached` | `Detached` | `abandoned()` | R1 (reconcile: Abandon) |
| `Attached` | `Detached` | `exo_detached()` | H2 |
| `Attached` | `Detached` | `host_deleted()` | F6, and G10 with `host_deleted` |
| `Draining{b}` | `Detached` | `barrier_done(b)` | W1 |

Every method that leaves `Attached` returns a `LeftAttached` token. The
caller must pass it to `Composition::end(LeftAttached) -> Option<Preedit>`,
which clears `shown` and the assembly. The token is `#[must_use]`, and
only `HostLink`'s methods construct it. So the rule "a composition
exists only while attached" is held by the type of the transition, not
by memory.

**Composition transitions.**

- `shown` changes only through `Current` methods (H7 to X2, F4), through
  `end` (any exit from `Attached`), through `new_field` (G9c), through a
  focus reset (F1, F2), and through `forget_unencodable` (a G9 answer
  that does not fit).
- The assembly records only while `Attached` (H4 and H5). It is taken
  first by every terminal event (H7 to X2) and by F4, and cleared by
  `end`.

**Fence transitions.**

- `None → Some(f)`: only F4, which requires the gate, so no fence is
  pending then.
- `Some(f) → None`: only W2, with the same `f`.

The fence is independent of the link, so the gate stays closed across an
Abandon (R1) and a re-activation. This is the case the first revision's
A1 got wrong (see Appendix B).

**The gate.**

```rust
// ime/state.rs
impl TextInput {
    /// The only constructor of Current: the link is Attached, no fence
    /// is pending, and the field is focused.
    pub(super) fn current(&mut self) -> Option<Current<'_>>;
}

/// Proof that the composition is current. Its fields are private to
/// state.rs, so no other module can forge one.
pub(super) struct Current<'a> {
    ti: TiId, seat: SeatId,
    field: &'a FocusedField,
    composition: &'a mut Composition,
    text: &'a mut ExoText,
    serial: Serial,
}
impl Current<'_> {
    pub(super) fn shown(&self) -> Option<&Preedit>;
    pub(super) fn surrounding(&self) -> Option<&(String, i32, i32)>;
    pub(super) fn text_ahead(&self) -> bool;
    /// Each method plans the transaction of one row. The arm encodes the
    /// result and then calls `commit`, or drops it.
    pub(super) fn preedit(self, text: String, commit: String) -> Planned<'_>;   // H7
    pub(super) fn host_commit(self, text: String) -> Planned<'_>;               // H8
    pub(super) fn delete(self, index: i32, length: u32) -> Result<Planned<'_>, DeleteDrop>; // H9
    pub(super) fn region(self, index: i32, length: u32) -> Option<Planned<'_>>; // X1
    pub(super) fn confirm(self) -> ConfirmPlan<'_>;                             // X2, X2'
    pub(super) fn press_commit(self) -> Option<Planned<'_>>;                    // F4
}
/// A planned transaction and the state change it makes. `commit()`
/// applies the change; dropping the plan applies nothing.
pub(super) struct Planned<'a> { pub txn: GuestTxn, /* private: next shown, text, lease */ }
```

**Invariants.** `ImeCore::assert_invariants` checks these after every
`step` in tests. `Context::assert_no_orphans` calls it.

- **I1a.** `Attached` implies `Focused(f)` with `f.committed.enabled`.
  This holds between steps, because every step ends with a reconcile:
  a target that turned inactive has just been deactivated (`Draining`)
  or abandoned (`Detached`).
- **I1b.** After a step, the link equals the target, except in three
  cases:
  - it is `Draining`;
  - the target is active and no live seat can carry the activation
    (R2);
  - the target is inactive and the link is `Attached` with a dead seat.
    That last case cannot occur after a step, since R1 abandons it.
  - An `Attached` link with a dead seat and an active target is legal.
    The seat was released while the field kept focus: INVARIANTS
    "Releasing a parent does not end its children", test
    `release_does_not_end_child_text_input_focus`.
- **I2.** `shown.is_some()` or a non-empty assembly implies `Attached`.
  `shown.is_some()` implies `fence.is_none()`.
- **I3.** At most one text input has `committed.enabled`.
- **I4.** `by_ext` is exactly the extensions of `inputs`.
- **I5.** Every token in `Draining{b}` or in `fence` is pending in
  `InternalCallbacks` as `InternalCallback::TextInput { token }`. The
  adapter checks this. It replaces today's reset-fence check in
  `assert_no_orphans`.

### 3.2 The guest transaction and effects

```rust
// ime/guest.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preedit { pub text: String, pub cursor: (i32, i32) }   // never empty text

/// What the guest applies at one done. `preedit: None` and an absent
/// preedit are the same value; what tells "clear" from "omit" is whether
/// the guest showed one.
#[derive(Debug, Default)]
pub struct GuestTxn {
    pub delete: Option<(u32, u32)>,
    pub commit: Option<String>,
    pub preedit: Option<Preedit>,
}

impl GuestTxn {
    /// The only builder of zwp_text_input_v3 preedit_string,
    /// delete_surrounding_text, commit_string and done. Canonical order:
    /// preedit (if Some, or if `shown_before` was), delete (if non-zero),
    /// commit (if Some), done(serial).
    pub(super) fn encode(&self, ti: TiId, shown_before: bool, serial: Serial)
        -> Result<Vec<Outgoing>, TooLong>;
}
```

- `Serial` comes only from `CommitCount::serial()`, so a `done` cannot
  carry another number.
- The `done` that the keyboard concern sends after a recovered key (X2')
  is `GuestTxn::default().encode(..)`. That is the bare form of the same
  encoder, not a second builder.

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
    /// Whether the input's own message must be forwarded. False only for
    /// a press the core re-sent (F4).
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
    /// X2': recover the held key (held_repeat_key_for_seat). If one is
    /// found, send its pair unless another channel delivered it, then
    /// `done`. Arm the lease if Backspace was recovered, and end it
    /// otherwise. If no key is held, end the lease and send nothing.
    RecoverHeldKey { ti: TiId, seat: SeatId, done: Outgoing },
}
```

### 3.3 The boundary with the rest of the proxy

```rust
// ime/mod.rs
pub enum Input {
    // Guest zwp_text_input_manager_v3 / zwp_text_input_v3 (G1–G10)
    Create { ti: TiId, seat: SeatId, host: HostTi, managers: HostManagers },
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
    log::trace!(target: "sommelier::ime", "in {input:?} env {:?}", env.snapshot_for(&input));
    let fx = ctx.ime.step(input, &env);
    log::trace!(target: "sommelier::ime", "out {fx:?}");
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

- At `trace`, the adapter logs each `Input` with the `Env` answers the
  step will read, and each `Effects`.
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
   idempotent: when the link equals the target it returns nothing. Its
   exhaustive test, `reconcile_reaches_every_target_with_only_needed_requests`,
   gains the assertion that a second reconcile returns nothing. The
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
            let pieces = t.take_assembly();                       // B3: first, unconditionally
            let Some(cur) = t.current() else { return log_stale(ti, &terminal) };   // B2: the gate
            let planned = match terminal { /* H7–X2 via Current's methods */ };
            match planned.txn.encode(ti, cur_shown_before, serial) {
                Ok(events) => { fx.to_guest.extend(events); fx.keys.extend(planned.commit()); }
                Err(TooLong) => log::debug!("dropping {ti:?}'s event: too long"),   // nothing applied
            }
        }
    }
}
```

### 3.5 Failure rules

There are two failure domains, with one rule each.

- **A guest transaction that does not encode, for a host event or a
  press** (H7 to X2, F4). The event is dropped and the assembly is
  taken. Nothing else changes, and no `done` is owed, because the
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
| B1 hidden host state | The link changes only through `HostLink`'s eight named methods (3.1). `step` reconciles every text input after every input. | **The type system** for "no arm forgets to reconcile": the call is in `step`, not in the arms. **Module privacy plus review** for "only reconcile, H2, W1, F6 and G10 call the transition methods". The row tests pin each transition. |
| B2 stale epoch | `Current<'_>` is constructible only in `state.rs`, by `TextInput::current()`. Host-event and press transactions are planned only through `Current`'s methods. | **Module privacy (compiler)** inside `ime`: `step.rs` cannot build a `Current`, nor set `shown`. |
| B3 leftover pieces | `take_assembly()` comes first in every terminal arm. Every exit from `Attached` returns a `#[must_use]` `LeftAttached` that `Composition::end` consumes. | **A convention plus row tests** for "first". **The type system** (must-use token) for the generation boundary. A row test feeds styling and a cursor before each stale terminal event and before a press, then checks that the next preedit uses defaults. |
| B4 serial drift | `CommitCount` sits in `serial.rs` with a private field. `bump()` is `pub(super)` and called only in the Commit arm. `Serial` has no other constructor. | **Module privacy** for the constructor. **A grep test** (`count_bumps_only_in_the_commit_arm`) for the one call site. |
| B5 malformed txn | `GuestTxn::encode` is the only builder of the four v3 events, and it always ends with `done`. | **A grep test** (`ime_is_isolated`). A crate-wide clippy `disallowed-methods` entry was considered and rejected: `sommelier/clippy.toml` notes that an allow for one rule lifts the fd and blocking guards too. |
| B6 missed trigger | Every trigger is an `Input` variant, and the match has no wildcard. | **The compiler**, for a variant that is unhandled. **The DST**, for a source that is never wired (as touch was before #72). |
| B7 one enabled | `enabled` lives in `FocusedField`, so a focus reset, a destroy or a repair clears it by construction (C-1). "At most one" is checked where `enabled := true` is set. | **The type** for staleness. **The Commit arm plus I3** for the count. |
| B8 order | `BTreeMap` in the core; P0 before the switch. | **The type**. The DST's determinism check runs each case twice. |
| B9 races | The DST. | CI. |

What stays outside the core:

- key generations, `peek_key`, `ack_key` and keysym routing: the
  `GuestKeyOwner` reducer, plus `ime_keys` after K1;
- pointer focus (`PointerFocus`);
- keyboard focus (`KeyboardFocusRegistry`);
- surface state.

The core sees these only through `Input` messages and `Env` reads.

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
 | (Chromium |  P->C   |  Session)    |  E->P   |  touch, text inputs (E1-E12)  |
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
  `Composer { key, preedit, reset, confirm }`. In the revision of #73
  under review, the composer also combines two lone consonants
  (ㄱ + ㅅ → ㄳ), as libhangul does by default; whether the ChromeOS
  engine does the same is [U12];
- from `editor.rs`: `TextModel { text, cursor, preedit }`, with
  `apply_txn(&Txn)`, `insert_key(Key)`, `click(Target)` and
  `delete_range(&self, before, after) -> Option<Range<usize>>`. The
  last is the one bounds check of a delete, which the client models
  share for their protocol assertion. The revision of #73 drops
  selection support (`anchor`), so the simulation never has a
  selection;
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
  sched.rs       Clock, Channel<T>, Choices, the step loop            ~250
  exo.rs         The Exo model: proxy/fuzz/exo.rs moved here and extended  ~800 (409 moved)
  engine.rs      The asynchronous IME engine over Composer             ~200
  client/chromium.rs, client/gtk.rs   Client models over TextModel     ~450
  keymap.rs      Key <-> evdev keycode for the US layout; click and tap positions  ~80
  user.rs        The user action generator, free and settled modes     ~150
  oracle.rs      The exact oracle (SpecText) and the user-intent oracle  ~300
  golden.rs      Golden trace hashes for the staged switch (C3-pre to C3c)  ~80
  shrink.rs      Minimizing a failing case                             ~150
  regressions.rs Replays: minimized cases and from_log                 grows
```

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

**Choices.** Every draw (a latency, a service time, a tie, a [U]
variant) is read from a `Choices` stream:

```rust
pub struct Choices { recorded: Vec<u32>, replay: Option<Vec<u32>>, rng: Rng, pos: usize }
impl Choices {
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

`sim/exo.rs` starts from today's `proxy/fuzz/exo.rs`, which models
activation, content type, support, surrounding text, the cursor
rectangle and the input method's restart. It adds:

- **Objects.** It learns each object from the decoded `AnyRequest`, as
  `Session`'s peer does (`Objects::learn` in `proxy/harness.rs`). It
  answers `wl_display.sync` with `wl_callback.done` and `delete_id`, and
  it deletes destroyed objects.
- **Seat.** It tracks the focus. Each case gets one keyboard, one
  pointer and one touch. It sends:
  - `wl_keyboard.enter` and `leave`, with `modifiers` before `enter`, as
    Exo does;
  - `wl_pointer.enter` and `leave`; for a click, `wl_pointer.motion` with
    the position that encodes the click's `Target` (`keymap.rs`), then
    `button` press and release;
  - for a tap, `wl_touch.down` with the position, then `up`;
  - `peek_key` (keyboard extension v2), and it takes `ack_key`.
- **Text input.** Each host `zwp_text_input_v1` gets two new fields:
  - `composition`, Exo's tracked composition text;
  - `predicted`, the text and cursor that `SurroundingTextTracker`
    predicts (E4): the last `set_surrounding_text` plus the input
    method's edits since.

  Its requests and events work like this:
  - `reset` clears `composition` and queues a `Reset` notice on `E→Eng`
    (E5). Keys already in the engine are not cancelled.
  - A detach, whether from `deactivate` or Exo's own, does three things
    in order:
    1. If the case drew "detach confirms" [U3] and `composition` is
       non-empty, it confirms: `confirm_preedit` at extension v11 or
       later, `commit_string` before that (E6).
    2. It clears the composition, sends `leave`, and queues a
       `Confirm`/`Reset` notice.
    3. The order of the keyboard's events and the text input's events
       on a focus change is drawn per focus change (E12).
- **Keys.**
  - A key on the focused surface with an attached text input goes to
    the engine as `E→Eng`.
  - **With no attached text input, the key goes raw (E7).** Exo sends
    `peek_key` and `wl_keyboard.key` at once, and the client inserts the
    US-layout character. This is the path during a drain or before the
    first `activate`.
- **Engine results** (`Eng→E`). Exo applies them to the text input
  attached now. If none is attached, the text actions are dropped,
  because `InputMethodAsh` has no client, but the key's events still go
  out. The case's E8 draw decides whether the text events go before or
  after the key's `peek_key` and `wl_keyboard.key` (the latter only if
  not consumed). The text events are:
  - a preedit: `preedit_cursor(len)`, then `preedit_string(serial, t, t)`.
    An empty preedit that ends a composition is `preedit_string("", "")`.
  - a commit: if it equals `composition` and the extension is v11 or
    later, `confirm_preedit` or `commit_string`, as the case's E9 draw
    says. Otherwise always `commit_string`, so a syllable split such as
    각 + ㅏ never becomes a confirm.
  - a delete (D6): an absolute `delete_surrounding_text` measured
    against `predicted`.

  `composition` and `predicted` are updated as the events go out.
- **Labels for the counters.** A result is `late` when its key reached
  the engine before a `Reset` that Exo handled before the result. The
  exact oracle applies late results by the specification. The counters
  show how often the E5 window (P3) occurred.
- **Draws per case:**
  - the extension version: none, 7, 10 or 11 (<8 means
    `set_content_type` plus the deprecated input type; 8–10 means
    `set_input_type` with commits only; ≥11 means `confirm_preedit`);
  - E6 (does a detach confirm?);
  - E8 (text events before or after the key);
  - E9 (the style of a confirmable commit);
  - E12, per focus change.

### 4.4 The engine

```rust
// sim/engine.rs
pub enum EngineIn { Key { key: Key, id: u64 }, Reset, Confirm }
pub struct EngineOut { pub id: u64, pub actions: Vec<TextAction>, pub consumed: bool }
pub enum TextAction { Commit(String), Preedit(String), Delete { before: usize, after: usize } /* D6 */ }
```

- **Korean mode** runs `hangul::Composer::key`. An `Output { commit,
  preedit, consumed }` becomes `[Commit(commit) if non-empty,
  Preedit(preedit)]`, where `preedit` is emitted when it changed,
  including to empty.
- **`Reset`** calls `Composer::reset()`; **`Confirm`** calls
  `Composer::confirm()`, and its text goes to E6.
- A key queued before a `Reset` is processed with the old composer
  state. That is the doubling of E5 and P3.
- **D6** adds an "accept a suggestion" user-visible action: a
  `Delete { before: last word }` and then a `Commit(replacement)`, only
  while nothing is composed (E10). It also adds a reconversion region
  (X1). This exercises H9, X1 and P4.
- **D7** adds holds: `UserAction::KeyDown(Key)` and `KeyUp(Key)` next to
  #73's atomic `Key`.
  - The engine gets autorepeats (500 ms delay, 33 Hz). Exo forwards
    none of them (E7).
  - The engine answers a consumed repeat with nothing to confirm with
    an empty `confirm_preedit`. This exercises X2' and the Backspace
    lease.
  - Whether the ChromeOS engine really sends that for every consumed
    repeat is [U11].

### 4.5 The client models

Each model holds one `TextModel` per field and the v3 objects it
created. It reacts only to events:

- **A transaction.** v3 events are collected until `done`. The model
  then converts them into an `editor::Txn` and calls `apply_txn`, unless
  one of its own rules drops a part.
- **A key.** An unconsumed `wl_keyboard.key` maps back to a `Key`
  through `keymap.rs` and goes to `insert_key`.
- **A press.** `wl_pointer.button` or `wl_touch.down` resolves its
  position to a `Target`, then calls `click`.

| Model | Rules (2.5) | Drops it labels (H-5) | Setup |
|---|---|---|---|
| `Chromium` | C1 to C5. Reports are gated on `last_done == commit_count`. Enable and disable happen on `enter`/`leave` and on a field switch, never on a click. | A delete while composing (C3). Commits and preedits within 100 ms of a press that found a preedit (C4). | One text input per surface, two surfaces, one seat |
| `Gtk { v4 }` | C6 to C9. Each applied commit or delete is answered with state and a `commit`. Enable on single-click release (GTK3: no drag, no caret jump). On press: reset (always on GTK4; on GTK3 with `need_im_reset`). | none | One text input, two surfaces |

Every drop goes into the model's drop log with its time and its reason.
The user-intent oracle reports the log; the exact oracle never needs it
(4.7).

Each model also asserts the v3 protocol as it receives events, and a
violation fails the case. The rules:

- a `done` serial is at most the model's commit count;
- text-input events arrive only between `enter` and `leave`;
- every event names an object the model holds;
- the delete lengths fit its text (`TextModel::delete_range` returns
  `Some`).

The models never hold a selection, because #73's `TextModel` has none.
The proxy's selection rules (H9's drop while the guest has a selection,
C5, C9's anchor) are therefore pinned only by row tests and Session
tests, not by the DST. 6.1 lists this gap.

### 4.6 User actions

The actions are #73's `UserAction` and `Target`, used unchanged:

- **Targets.** A target means the same text to the user and to a client
  that lags behind: the start, the end, or just before a marker digit
  seeded into the initial text (for example `"0ab1cd2"`).
- **Markers.** The generator keeps the markers. It generates a Backspace
  only when the `Reference` has something after the last marker, or
  after the start, to delete.
- **Mix.** About 65% keys (jamo for syllable patterns, plus Space and
  Backspace), 15% clicks, 8% taps, 7% focus changes and 5% `Settle`. A
  case has 10 to 60 actions and two fields.

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
    (E7).

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
  - whether the text is `Ahead`, by the guest's surrounding-text
    commits;
  - the focus.

  It then applies the events in the logged order:
  - H7 to X2, by their rows;
  - F4 at each press or down on the composing surface: commit the shown
    preedit, then move the cursor to the target;
  - forwarded `wl_keyboard.key`, through `insert_key` (raw keys
    included);
  - F1 and F2 on focus changes, as the specification says today. C5
    changes the reference with the proxy.
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
  - dropped deletes, by reason;
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

Only P1 remains, since #72 retired P2.

```rust
// sim/oracle.rs
/// A tier-2 failure the proxy is known to cause. It is counted, not
/// failed. The PR that fixes it deletes its variant.
enum KnownIssue {
    /// P1: a focus change while the client shows a preedit loses or
    /// keeps the syllable depending on Exo's event order. C5 fixes it.
    FocusWhileComposing,
}
```

A failure is classified as known only when all three hold:

- tier 1 passed;
- tier 2 failed on the text of a field that a `Focus` (or a click or tap
  on the other field) left while that field's client showed a preedit;
- no other check failed.

Everything else fails the run. The scenario keeps being generated, so
any other bug on that path stays visible. The summary prints the known
count.

### 4.9 Golden traces for the staged switch

D4 records goldens; C3-pre commits them and C3c deletes them:

- **Recording.** `SOMMELIER_DST_BLESS=1` writes `sim/golden.txt`, with
  one line per seed for 2,000 seeds: a hash of the full `P→C` and `P→E`
  byte streams, and the case's choices.
- **The test.** `golden_traces_match` replays each seed and compares the
  hashes, and prints the first differing message for a mismatch. It
  runs in CI from C3-pre to C3c, and C3c deletes it and the file.
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
- **Runtime.** Each case runs in
  `tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(..)`.
  `enable_all` is needed because `WaylandConnection` uses an `AsyncFd`.
  The DST commits no dma-buf, so no fence parks and every await
  completes at once.
- **Before the DST,** P0 replaces any `HashMap` iteration that the
  determinism check finds affecting output order. `text_inputs` is the
  known one (B8).

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
  - `golden_traces_match` runs from C3-pre to C3c.
  - CONTRIBUTING gets a "Simulation test" section next to "Randomized
    tests".

---

## 5. Migration plan

### 5.1 The PRs

Each PR passes `cargo fmt --check`, `clippy -D warnings`, every test and
`check-cited-tests.py` on its own.

**Size convention:** new or changed lines, as production / test. Moved
lines are listed separately.

| PR | Scope | Depends on | Size (prod / test) |
|---|---|---|---|
| **#72** | Touch-down ends the composition (in flight). | — | merged |
| **#73 = D1** | `sim/hangul.rs`, `sim/editor.rs` (in flight). | — | 0 / +1,086 |
| **P0** | Determinism: `Context::text_inputs` becomes a `BTreeMap`, plus any other `HashMap` iteration the D4 determinism check finds. No behaviour change. | — | +5 / +20 |
| **T1** | Canonical v3 transactions on the **old** code, with the tests that pin the old form updated. Four changes: (1) a reset commit's order; (2) a region's order; (3) no `preedit_string("")` when nothing was shown; (4) no `delete(0, 0)` for an empty region. All four are semantically identical for the clients in scope (2.5 C3, C6). This leaves the switch free of wire changes. | #72 | ±40 / ±60 |
| **C1** | The `ime/` skeleton: `serial.rs` (`CommitCount`, `Serial`), the ID newtypes, `guest.rs` (`Preedit`, `GuestTxn::encode`), and the content-type mapping, **moved** into `host.rs`. The range helpers are **moved** into `guest.rs`. Old code calls the moved helpers. `HostApplied` and `reconcile` stay in `state` until C3a (M-1). The module carries `#[cfg_attr(not(test), expect(dead_code))]` until C3a. | T1 | +150 (moved 260) / +150 |
| **K1** | A pure move of the keyboard concern into `handler/ime_keys.rs`: keysym routing, held-key recovery, Backspace leases (`keysym_to_evdev_keycode`, `keyboard_for_seat`, `keyboard_for_keysym`, `held_repeat_key_for_seat`, `synthesize_ime_consumed_key_pair`, `end_backspace_repeat_*`, and `on_keysym`'s body). | C1 | 0 (moved ~400) |
| **D2** | `sim/exo.rs` (moved from `proxy/fuzz/exo.rs` and extended; the fuzzer imports it), `sim/engine.rs`, `sim/keymap.rs`. Unit tests check each E fact against scripted request sequences. | #73 | 0 / +700 (moved 409) |
| **D3** | `sim/client/{chromium,gtk}.rs`. Unit tests replay scripted event sequences against each C fact. | #73 | 0 / +450 |
| **C2** | `ime/state.rs`, `ime/step.rs` and `ime::host::reconcile` over `HostLink`, complete, with `tests.rs`: one test per section 2 row ID and `assert_invariants`. Not wired in yet. **This is the one window with two `reconcile`s:** the old one over `HostApplied`, and the new one. C3a deletes the old. | C1 | +800 / +900 |
| **D4** | `sim/sched.rs` (clock), `user.rs`, `oracle.rs`, `golden.rs` (recording only), and `mod.rs` with `ime_reaches_the_specified_text`, `ime_reaches_the_intended_text` and the `KnownIssue` classifier. Also CONTRIBUTING, DESIGN and the INVARIANTS citations. It lands green on today's code. A finding outside the prediction is fixed in its own PR on the old code before C3-pre, or becomes a `KnownIssue` with a reason. | D2, D3, P0 | 0 / +700 |
| **D5** | `sim/shrink.rs`, the replay snippet, `regressions.rs`. | D4 | 0 / +200 |
| **D6** | Engine phase 2: deletes and suggestion accept against `predicted`, and reconversion regions. Exercises H9, X1 and P4. | D4 | 0 / +200 |
| **D7** | Holds: `UserAction::KeyDown` and `KeyUp`, extending #73's enum; engine autorepeat; the empty confirm (X2') and the Backspace lease. | D4 | 0 / +150 |
| **C3-pre** | No production change. Tests outside `text_input.rs` that build or read `TextInputState`, `HostApplied` or `ctx.text_inputs` directly (`keyboard.rs`, `compositor.rs`, `seat.rs`, `display.rs`, `registry.rs`, `state.rs`, `lifecycle.rs`: about 52 references) move to Session wire level, or to one `#[cfg(test)]` fixture module, `test_support::text_input`, the only test code that touches the storage. Commits `sim/golden.txt` and `golden_traces_match`, blessed on this base. | K1, D4, D5, D6, D7 | 0 / ±600 |
| **C3a** | **The storage switch.** `Context::text_inputs` becomes `Context::ime: ImeCore`. `TextInputState`, `HostApplied`, `ShownPreedit`, the plan structs, `prepare_*`/`finish_*` and the old `reconcile` are deleted. The old handler bodies keep their shape, but call `ImeCore`'s state methods, made `pub(crate)` for this window only, and the core's `reconcile`. `InternalCallback` merges to `TextInput { token }`. The fixture module is rewritten. | C2, C3-pre | ≈ −650 / ±200 |
| **C3b** | Host events (H, X and W rows) go through `step`. The old v1, extension and callback handler bodies are deleted. | C3a | ≈ −700 / 0 |
| **C3c** | Guest requests and cross-concern inputs go through `step`. The rest of the old code is deleted, the window's `pub(crate)` items go private, and `ime_is_isolated` is fully enabled. INVARIANTS and DESIGN are updated (Appendix A). `golden.txt` and its test are deleted. | C3b | ≈ −450 / −80 |
| **C5** | Focus loss commits the shown preedit before `leave` (F1, proposed). `KnownIssue::FocusWhileComposing` goes, and `SpecText` changes with the proxy. INVARIANTS is amended with its reasons (E6; mutter and ibus commit on focus-out). It needs the U3 host check first, or it lands with the manual check recorded in HANDOVER. | C3c | +40 / +80 |
| **C6** | Optional: a guest-initiated reset sends a host `reset` behind a fence (G6, Q3), if the DST shows that GTK's `cause=other` reset leaves Exo composing. | C5 | +40 / +60 |
| **C7** | Test slimming. Session wire-trace tests that a row test covers are deleted. The PR body carries a mapping table, "cited name → where it lives", for all 45 INVARIANTS-cited IME tests and #72's three. A cited name survives as a Session test or as a row test of the same name. The merge criterion is that the table is complete and `check-cited-tests.py` passes. The fuzzer's `typing.rs` drops its Exo end-state check, which the DST owns. | C3c (after C5 and C6 if they are open) | 0 / ≈ −2,500 |

### 5.2 Parallel groups and the merge order

**`handler/text_input.rs` merge order.** It is a single serial chain,
because every PR in it edits that file:

```
#72 → T1 → C1 → K1 → C3-pre → C3a → C3b → C3c → C5 → C6 → C7
```

**Groups.** Work in a group runs in parallel. A group starts when its
dependencies have merged.

```
G0  in flight      #72, #73 (D1)
G1  now            P0  ‖  T1 (after #72)  ‖  D2 (after #73)  ‖  D3 (after #73)
G2                 C1 (after T1)  ‖  D4 (after D2, D3, P0)
G3                 K1 (after C1)  ‖  C2 (after C1)  ‖  D5 ‖ D6 ‖ D7 (after D4)
G4  freeze starts  C3-pre (after K1, D4–D7)
G5                 C3a (after C2, C3-pre) → C3b → C3c      (DST and proxy frozen until C3c)
G6                 C5 → C6 → C7
```

The most implementers at once is about five, in G3:

- C2 is confined to `ime/`;
- D5, D6 and D7 are confined to `sim/`;
- K1 is the only one of them on the `text_input.rs` chain.

### 5.3 The staged switch and coexistence

- **Before C3a.** The new core exists only as tested code. It shares
  `serial.rs`, `guest.rs`'s helpers and the content-type mapping with
  the old code (moved in C1). The one duplicate is `reconcile`, during
  C2 → C3a.
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

---

## 6. Risks and open questions

### 6.1 Risks

| Risk | Mitigation |
|---|---|
| **The models are wrong** (Exo, the engine or the clients), so the DST passes on traces real hosts never produce, or fails on impossible ones. | Every model rule cites a source (2.4, 2.5) or is marked [U] and **drawn** (E6, E8, E9, E12, the extension version), so both branches stay covered. The host checks in 6.2 collapse the draws one by one. A DST failure is triaged against the citations before any proxy fix. |
| **`SpecText` and the core share a misreading of section 2.** | Tier 2 checks the outcome against user intent independently of section 2. Row tests pin each row against today's code until C3c, and the goldens pin the wire. |
| **The DST never exercises a selection**, because #73's `TextModel` has none. | H9's selection drop and the selection rules of C5 and C9 stay pinned by row tests and Session tests. If a selection-related bug ever escapes, adding `anchor` back to `TextModel` is a local change in `sim/`. |
| **The clock is unrealistic.** | Latencies come from three lag profiles with a spike, and the client's 100 ms guard and the user's timing use the same clock. The host checks U2 and U6 calibrate the profiles. |
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
| U2 | Are an engine's text events for a key sent before or after its `peek_key` and `wl_keyboard.key`? | Type "가", then Space. | E8 |
| U3 | Does a detach while composing confirm, and in what order relative to v1 `leave` and `wl_keyboard.leave`? | Type "가", then Alt-Tab. | E6, E12, P1, C5 |
| U4 | Does the Korean engine commit a confirmable composition with `commit_string` or with `confirm_preedit`? | Type "가", then Space or Enter. | E9 |
| U5 | Should the composition be kept on blur? Native Chrome keeps it. | Type "가" in a native Chrome field, Alt-Tab, and compare with the guest. | C5 |
| U6 | How often does an engine commit for a key pressed just before a click arrive after the fence (P3), and what are real engine latencies? | HANDOVER's "Space and click at the same instant", counted, with timestamps. | Q1; the lag profiles |
| U7 | Does Exo send `wl_touch` to Crostini clients, and in what order relative to text-input events? | Tablet mode. | F4 for touch (#72) |
| U8 | Does the `done` that answers each commit move the candidate window with the caret? | HANDOVER's existing check. | G9c |
| U9 | Does the ARC placement policy turn off the IME for placed windows? | RESEARCH Q3. | Out of scope; document only. |
| U10 | In GTK3, does `GtkEntry` update its cursor location before the click's release? This decides whether C7's release-time enable happens. | `WAYLAND_DEBUG` on gedit while clicking. | The GTK3 model's C7 |
| U11 | Does the engine answer a consumed autorepeat with an empty `confirm_preedit`? | Hold Backspace past a composition. | D7's engine rule |
| U12 | Does the ChromeOS Korean engine combine two lone consonants (ㄱ + ㅅ → ㄳ), as libhangul's default does and #73's composer will? | Type ㄱ then ㅅ with no vowel. | The `Composer`'s lone-consonant rule |

### 6.3 Open design questions

- **Q1. Can the engine-latency window (E5, P3) be closed?**
  - Option 1: replace `reset` with a `deactivate`, a barrier and an
    `activate`, so that Exo's own confirm commits with the engine's final
    text. INVARIANTS rejects this today, because keys bypass the input
    method until the `activate`.
  - Option 2: correlate by `peek_key`, needing U2.
  - Recommendation: measure with U6 and the P3 counter, then decide.
- **Q2. Mirror Exo's `SurroundingTextTracker` for deletes (P4)?**
  `ExoText` could hold the predicted text instead of a flag. Do it only
  if D6 shows deletes lost in normal flows. The enum makes it a local
  change.
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

| Rule (Keyboard and IME unless noted) | In the new core | Change |
|---|---|---|
| Content hints come from the v3 XML; `set_input_type` replaces v1 `set_content_type` | `ime/host.rs` (moved) | none |
| The host gets each content type once it can apply it, and only then | `HostLink`, the end-of-step reconcile, H2, W1, R1, R2 | Names: `HostApplied` becomes `HostLink`; reconcile runs after every input |
| One text input on a connection is enabled at a time | `FocusedField.committed.enabled`, the Commit arm, I3 | Text updated; staleness now impossible |
| Focus teardown sends `leave` itself | F1, F2 | none |
| Every v3 commit counts | `CommitCount`, G9 | Names: `TextInputState::count_guest_commit` becomes `CommitCount::bump`, called in the Commit arm |
| Exo's deletes are absolute and stand alone | H9, `ExoText` | `unreported_host_edit` becomes `ExoText::Ahead` |
| Every commit that leaves a field enabled gets a `done` | G9b, G9c | Wording: GTK3 sends its enable on the click's release, not right after the press [F]. The reasoning is unchanged, because the release follows the press, so the press has already ended the composition |
| A press ends the composition on its surface (#72) | F4, W2, the fence beside the link | `host_composition_is_current` becomes `TextInput::current`; `TextInputState::reset_fence` becomes `TextInput::fence`; the callback names merge. The fence's lifetime is **unchanged** |
| Wire messages: byte-indexed strings that are not UTF-8 | G5, H7' | none |
| Object lifetimes: synthetic objects; releasing a parent; a host-destroyed text input | G1, G2, G10, F3, F6, R1, `Env::host_seat` | none |
| Keymap, key-generation, recovered-serial and modifier rules | The keyboard concern (`ime_keys`, `keyboard.rs`) | none |

---

## Appendix B. Review responses

The reviews are `claude-review-design.md` (finding IDs as written; this
appendix labels its unnumbered Low items CL-1 to CL-7 and its
over-engineering rows OE-1 to OE-7, in order) and `oc-review-design.md`
(deepseek). Each finding was checked against the code at `2546178` and
#72's branch.

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
