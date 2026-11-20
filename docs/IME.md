# IME design

This document covers the IME bridge. It holds the behaviour as a table,
the facts about Exo and the clients that the table relies on, the shape
of the IME core, and how the deterministic simulation test (DST) holds
the proxy to the table. Other documents own the rest:

- [DESIGN.md](DESIGN.md#keyboard-and-ime) places the core in the proxy
  and names the module that owns each part.
- [INVARIANTS.md](INVARIANTS.md#keyboard-and-ime) holds the rules the
  core keeps, with their tests. This document links to a rule rather
  than restating it.
- [KEYBOARD.md](KEYBOARD.md) covers key generations, keysym routing and
  held-key recovery.
- [CONTRIBUTING](../CONTRIBUTING.md#simulation-test) says how to run the
  DST, replay a seed and shrink a failure.

Code and tests cite this document's IDs (G1, H9, E13, I1a and the like)
and its section numbers ("design 3.1"), so keep both stable. The plan
that built the core and the DST, with its history, sizes and review
responses, is in `git show 15e045e:docs/IME_REDESIGN.md`.

## 1. Conventions

- **[F]** marks a fact read in source: Chromium (Exo,
  `InputMethodAsh`, which this document calls IMA, and the v3 client),
  GTK, mutter, KWin, winit, SDL3, Qt and kitty, at the revisions
  INVARIANTS and the models cite. One captured host trace counts too:
  the test `held_space_repeats_after_ime_commit_until_physical_release`.
- **[U]** marks an unknown that only a real Chromebook can settle.
  [5.2](#52-unknowns-and-manual-host-checks) lists each one with its
  check. The DST draws a [U] per case rather than fixing it.
- "INVARIANTS *Rule*" means the bullet of that title in
  [INVARIANTS.md](INVARIANTS.md), under Keyboard and IME unless another
  section is named.
- The code also cites these IDs:
  - B1 to B9: the bug classes the core rules out
    ([3.6](#36-guarantees-and-how-each-is-enforced)).
  - I1a to I7: the core's invariants ([3.1](#31-state)).
  - P1, P3 and P4 are races the DST measures. P1 is a focus loss while
    composing, which F1 fixed. P3 is an engine answer that arrives after
    a press's fence (E5's outcome (c)). P4 is a guest report that
    crosses an edit in flight.
  - D6 and D7 are the DST's suggestions and reconversions, and its held
    keys ([4.4](#44-the-engine)).
  - M1 to M4 are the steps that built the Exo mirror. M4 is report
    suppression (G9c).

## 2. Behavioural specification

This is the contract:

- **Core row tests.** Each row of this section has a row test of the
  core, named `row_<id>_<what>` (`ime/tests.rs`), unless it keeps the
  name of a test INVARIANTS cites. G2, G6, H1, H3 and X3 have none: the
  adapter handles or ignores them.
- **DST models.** The Exo and client models implement the E and C facts
  of 2.4 and 2.5.
- **Exact oracle.** It implements the text semantics of 2.2 and 2.3.

### Conventions

- **Count.** The text input's count of guest commits (B4).
- **Txn{…}.** One v3 transaction ([3.2](#32-the-guest-transaction-and-effects)).
  Its fields say what the guest applies: `delete`, `commit` and
  `preedit`. `preedit: none` means there is no preedit after this
  `done`.
  - **Wire order and no-ops.** INVARIANTS "A v3 transaction's events go
    in one order" owns the order of a transaction's events. It also says
    which events a transaction leaves out because they change nothing,
    and gives the client evidence. `GuestTxn::encode` is the one
    encoder. An empty preedit cannot be represented (`Preedit::new`).
    No transaction has both a delete and a commit, because Exo's deletes
    stand alone (E10).
  - **The G9c answer repeats an unchanged preedit.** A `done` without a
    preedit clears it. So the answer to a guest commit sends the shown
    preedit again, unchanged, with no clear before it.
- **Reconcile.** It computes the `HostTarget` from the committed guest
  state:
  - the field is enabled and focused, on a surface the host knows;
  - there is a live host seat;
  - the content type, with its support.

  It compares the target with the link and sends only the difference
  (INVARIANTS "The host gets each content type once it can apply it, and
  only then"). A reconcile's host requests go in this order:
  1. the activation step;
  2. `set_surrounding_text`;
  3. the content type, with its support;
  4. `set_cursor_rectangle`;
  5. `commit_state(count)`, sent only after a guest commit or a content
     type.

  **Every `step` ends with a reconcile of every text input** (3.4). A
  row's "Host requests" column lists what that reconcile sends because
  of the row.
- **States** (see [3.1](#31-state)):
  - field: `Unfocused` or `Focused(f)`, where `f.enabled` is the
    committed enable;
  - link: `Detached`, `Attached{ct}` or `Draining{b}`;
  - fence: `None` or `Some(f)`;
  - composition: `shown`, the preedit the guest shows (`None` or
    `Some(p)`), plus the v1 assembly.
- **Gate.** The link is `Attached` and the fence is `None`.
- **Lease.** The Backspace repeat leased to the field. The keyboard
  concern owns it ([KEYBOARD.md](KEYBOARD.md)).
- **"On encode failure."** What happens when a string does not fit one
  message ([3.5](#35-failure-rules)).
- **The assembly (B3).** A preedit and a region take the assembly first,
  whatever becomes of them (H7, X1). A commit and a delete (H8, H9) take
  it only once they reach the guest, so a stale or dropped one leaves
  it. A confirmation (X2) takes it once the gate admits it with a
  preedit shown, so a stale or empty one (X2') leaves it. A press (F4)
  takes it once its commit reaches the guest, so a press whose commit
  does not fit leaves it. Exo sends styling and a cursor only right
  before their preedit [F], so no client sees the difference. Row tests
  `row_H8_H9_X2_only_an_edit_that_reaches_the_guest_takes_the_assembly`
  and `row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly`.

### 2.1 Guest requests

| ID | Request | Precondition | Host requests | Guest events | Next state / notes | On encode failure | Sources |
|---|---|---|---|---|---|---|---|
| G1 | `zwp_text_input_manager_v3.get_text_input(id, seat)` | The host has a v1 manager and the extension at v11 or later (otherwise the adapter makes the text input a local object with no input method, and warns once; INVARIANTS "A text input needs the host's extension at v11") | `create_text_input`; `get_extended_text_input` | `enter(surface)` if the seat's keyboard focuses a surface | `Focused` (not enabled), or `Unfocused`; link `Detached`; count 0; an Exo mirror (`ExoView`) of empty text. | n/a (no strings) | INVARIANTS Object lifetimes "Synthetic objects complete their own lifecycle" |
| G2 | `zwp_text_input_manager_v3.destroy` | none | none | `delete_id`, sent by the generated local destructor path, not by the core | none | n/a | same |
| G3 | `enable` | `Focused` | none | none | `pending = V3Pending::reset(true)`. Ignored while `Unfocused`. | n/a | `V3Pending::reset` |
| G4 | `disable` | `Focused` | none | none | `pending = V3Pending::reset(false)`. Ignored while `Unfocused`. | n/a | same |
| G5 | `set_surrounding_text(t, c, a)` | `Focused` | none | none | If `t` is UTF-8 and `c` and `a` are char boundaries within it, pending becomes `Some(t, c, a)`. Bad offsets are ignored, with a warning. Text that is not UTF-8 makes it `Some("", 0, 0)`. | n/a (checked at G9) | INVARIANTS Wire messages "A string that is not UTF-8 does not end the session" |
| G6 | `set_text_change_cause` | any | none | none | Ignored by the adapter. Chromium never sends it [F]. GTK sends it after every surrounding text [F]. See Q3. | n/a | `on_set_text_change_cause` |
| G7 | `set_content_type(h, p)` | `Focused` | none | none | pending `content_type` | n/a | |
| G8 | `set_cursor_rectangle` | `Focused` | none | none | pending `cursor_rect` | n/a | |
| G9a | `commit` | `Unfocused` | none | none | Count += 1. Nothing else. | n/a | INVARIANTS "Every v3 commit counts" |
| G9b | `commit` whose pending surrounding text does not fit a v1 message | `Focused` | **none at all**: no activation step and no content type for this commit | If the field was already enabled: `Txn{preedit: shown}` (the answer) | Count += 1. **The pending state is consumed and nothing else changes**: the committed state, the link and the composition stay, and no later commit retries the text. | *(this row is the failure case)*. If the answer's repeated preedit does not fit either, the shown preedit is forgotten and a bare `done(count)` goes out. | tests `oversized_surrounding_commit_does_not_partially_advance_state`, `the_commit_after_an_oversized_one_does_not_retry_it` and `an_answer_that_cannot_repeat_the_preedit_forgets_it` |
| G9c | `commit` | `Focused` | The reconcile, with this commit's `set_surrounding_text` and `set_cursor_rectangle`, then `commit_state(count)` | If the field is enabled after the commit: `Txn{preedit: shown}` | See the four steps below this table. | If the repeated preedit does not fit, the shown preedit is forgotten and a bare `done` goes out (as in G9b). | INVARIANTS "One text input on a connection is enabled at a time", "Every commit that leaves a field enabled gets a `done`" and "The host gets each content type…"; mutter (`text_input_commit_state` in `meta-wayland-text-input.c`) and KWin (`textinput_v3.cpp`, "Gtk … expect done to be sent after every commit") [F] |
| G10 | `zwp_text_input_v3.destroy` | any | The reconcile to inactive: `deactivate` and a barrier while attached with a live seat; otherwise nothing. Then the extension's `destroy`, and the extension is retired. Nothing goes to a v1 object the host deleted. | `delete_id`, from the generated path | The text input is removed, **with its `enabled` flag** (B7). The lease ends. A later completion of its barrier or fence is ignored. | n/a | INVARIANTS Object lifetimes; row test `row_G10_destroy_of_the_enabled_input_lets_another_enable` |

**The four steps of G9c:**

1. **Count += 1.**
2. **Enable.**
   - An enable while another text input on the connection is enabled
     is ignored: the field stays disabled and counts as a new field. The
     rest of the commit (surrounding text, content type) still applies
     to the committed state, pinned by a row test (Q5).
   - Otherwise `new_field` is whether `enabled` changes. An enable of an
     enabled field keeps the composition, and the host gets its content
     type again only if the commit changed it.
3. **A new field** ends the lease. It leaves the field disabled or
   newly enabled, and neither can stay attached. A disable makes the
   step's reconcile leave `Attached`, which ends the composition and
   the assembly with no guest output, since the guest reset itself. An
   enable, or a rejected one, starts from a link that was not attached
   (I1a), so there is no composition and no content type to unset.
4. **A stated surrounding text** becomes the committed one. The
   reconcile forwards it to Exo, glued to a barrier at whose `done` the
   Exo mirror applies it. Then the content type is applied.

**Report suppression** (M4). The reconcile withholds a
report that would only make Exo drop its composition (E4), and its
barrier. The content type, the cursor rectangle and `commit_state` still
go out. INVARIANTS "Exo's deletes and
regions are measured in the Exo mirror, or dropped" owns the rule. Row
tests:

- `a_report_without_the_shown_preedit_is_withheld`
- `row_G9c_an_exact_report_goes_out_unchanged`
- `row_G9c_an_unmatched_report_goes_out_unchanged`
- `row_G9c_an_older_report_matched_after_a_withheld_one_keeps_it_in_step`

If the field's support for surrounding text flips, the content type is
sent again with the new support (test
`surrounding_text_support_follows_the_field_and_resends_only_on_change`).
A refocus drops the pending state (I7).

### 2.2 Host events

Events from a host object that no live text input owns are dropped.
None of these rows sends a host request of its own, beyond the
reconcile that ends every step. The Exo mirror takes every event of its
text input before the gate, as each one changed Exo's text.

| ID | Event | Precondition | Guest events | Next state / notes | On encode failure | Sources |
|---|---|---|---|---|---|---|
| H1 | v1 `enter(surface)` | any | none | No change. While attached, it answers the proxy's own `activate`. | n/a | INVARIANTS "The host gets each content type…" |
| H2 | v1 `leave` | `Attached` | If `shown = Some(p)`: `Txn{commit: p, preedit: none}`. Exo's detach confirms first (E6), which commits the shown preedit (X2). So a preedit still shown is one that confirmation did not commit, which the DST reaches only in rare races; the guest commits it, so a restart of the input method keeps the user's text. Nothing is committed twice. | Link → `Detached` (Exo's own detach). The commit, as at X2, ends the lease. The composition and the assembly are cleared. The fence stays. The reconcile then activates again if the field still wants input. | A commit that does not fit only clears the preedit. | INVARIANTS same rule; Exo `TextInput::OnSurfaceFocused`, `OnSurfaceDestroying` and `OnInputMethodDestroyed` [F]; tests `row_H2_an_exo_detach_commits_the_preedit_and_activates_again`, `row_H2_a_confirmed_composition_is_committed_once`, `an_exo_detach_commits_the_guest_preedit` |
| H2' | v1 `leave` | `Draining` or `Detached` | none | Ignored: it answers the proxy's own `deactivate`. | n/a | same |
| H3 | `modifiers_map`, `input_panel_state`, `language`, `text_direction` | any | none | Ignored. | n/a | the adapter's handlers of the same names |
| H4 | `preedit_styling(i, l, style)` | `Attached` (dropped otherwise) | none | Recorded in the assembly when `style == SELECTION`. | n/a | |
| H5 | `preedit_cursor(i)` | `Attached` | none | Recorded in the assembly. | n/a | |
| H6 | `cursor_position(i, a)` | `Attached` | none | Logged at debug level as having no v3 equivalent, and not recorded. Exo sends it only before a `commit_string` [F], where the mirror reads the pair as a selection range. | n/a | |
| H7 | `preedit_string(serial, t, commit)` | Gate; `t` is UTF-8 | `Txn{preedit: t, with the cursor from the assembly}`. `commit` is not read: Exo always sends the preedit's text there [F], and the mirror diverges at any other. An empty `t` with nothing shown is a bare `done(count)`. | `shown := t`, or none if `t` is empty. The assembly is taken. **Lease:** `LeaseOutcome::ArmIfBackspaceHeld` if `t` is empty and `shown` was set; `End` otherwise. The host serial is only logged. | The event is dropped. The assembly is taken. Nothing else changes. | Exo sends the preedit text as its commit text [F]; row tests `row_H7_a_preedit_shows_and_its_clear_arms_the_lease`, `row_H7_a_preedit_that_does_not_fit_is_dropped` |
| H7' | `preedit_string` | Gate fails, or `t` is not UTF-8 | none | Dropped. The assembly is taken. | n/a | INVARIANTS press rule (the fence); Wire messages (byte-indexed strings) |
| H8 | `commit_string(serial, t)` | Gate | `Txn{commit: t, preedit: none}` | `shown := none`. An empty commit is not sent, so the guest has no edit to report, whether or not it shows a preedit (row tests `row_H8_an_empty_commit_is_no_edit` and `row_H8_an_empty_commit_over_a_preedit_is_no_edit`). The lease ends. The assembly is taken. | Dropped; the assembly stays (B3). | |
| H9 | `delete_surrounding_text(index, length)` | Gate | `Txn{delete: (before, after), preedit: none}` | Measured in the Exo mirror's state before the event (`exo_view::Frame`) whenever INVARIANTS "Exo's deletes and regions are measured in the Exo mirror, or dropped" allows it. A range around Exo's cursor, with no composition, becomes `(cursor − start, end − cursor)`; `shown := none`; the lease ends; the assembly is taken. An empty range sends nothing and keeps the guest in step. **Dropped** otherwise, ending the lease as Exo edited (but for an empty range), with a debug log that names why (`exo_view::Unmeasured`). The guest is then out of step until its next report. A negative index, a range not around Exo's cursor and one off its text's char boundaries are none Exo sends, so the mirror diverges at them. Row tests `row_H9_an_absolute_delete_becomes_lengths_around_the_cursor`, `row_H9_a_delete_after_an_unreported_edit_lands_in_exos_text`, `row_H9_after_set_editable_selection_range_drops`, `row_H9_after_an_empty_region_with_exos_cursor_inside_drops`, `row_H9_after_a_dropped_region_drops`, `row_H9_after_a_delete_over_a_shown_preedit_drops`, `row_H9_a_field_that_states_no_text_drops`, `row_H9_after_a_forwarded_key_drops`, `row_H9_in_step_again_after_a_matched_report`, `row_H9_a_delete_that_heals_an_unsure_mirror_drops_as_unsure`. | Dropped; the assembly stays. | INVARIANTS "Exo's deletes are absolute and stand alone". [F] Chromium drops a delete while a composition is shown (`InputMethodAuraLinux::OnDeleteSurroundingText`). |
| H10 | `keysym(...)` | `Attached` (the fence is **not** checked) | `wl_keyboard.key`, through the keyboard concern | Not a core input. `ime_keys` asks `ImeCore::keysym_target(ti)`. Exo is not known to send one [F, U1]. | n/a | INVARIANTS "A key waits for at most one late release per channel" |
| X1 | ext `set_preedit_region(index, length)` | Gate; measured as H9 is | `Txn{delete: (before, after), preedit: (region, cursor)}` | Measured in the mirror's state before the event, under H9's conditions. `G` is the region from Exo's selection end, in Exo's text, which holds its composition inline (E4). An empty `G` commits the composition Exo holds, which the guest shows (`Txn{commit: p, preedit: none}`), or else edits nothing. A `G` that covers the composition, or Exo's cursor with none, gives `Txn{delete: (H.start − G.start, G.end − H.end), preedit: G}`, with the preedit cursor from the assembly or Exo's cursor. It deletes only the bytes beyond the composition on each side, which the guest has, and shows them with it. A region over a region is the case where the composition is the last region. `shown := G`, or none; the lease ends. The assembly is taken. A `G` that does not cover the composition, one short on either side included, is dropped with a debug log: Exo would leave the rest of the composition as text. A dropped region, measured or not, ends the lease, as Exo edited; and Exo ends its composition at an empty one whether or not the core can measure it, so a dropped empty region with a preedit shown commits it as an empty `G` does (`Txn{commit: shown}`), which may double text but never loses it. Row tests `row_X1_a_preedit_region_turns_text_into_the_preedit`, `an_unmeasured_empty_region_commits_the_shown_preedit`, `row_H9_X1_a_dropped_delete_or_region_ends_the_lease`, `row_X1_a_region_after_an_unreported_commit_is_measured_in_exos_text`, `row_X1_a_region_after_an_e4_drop_holds_no_preedit`, `row_X1_a_region_made_before_a_report_is_measured_before_it`, `row_X1_a_region_crossing_a_guest_edit_drops`, `row_X1_a_mirror_that_is_not_exact_drops`, `row_X1_a_mirror_that_diverges_warns_once`, `row_X1_after_a_confirm_inside_the_composition_drops`. | Dropped; the assembly is taken. | Exo's region is cursor-relative [F]; `SurroundingTextTracker::OnSetCompositionFromExistingText` keeps an empty composition [F]; tests `on_set_preedit_region_translates_correctly`, `a_region_over_a_region_drops`, `a_region_over_a_host_preedit_is_measured_from_exos_cursor`, `a_region_after_a_host_preedit_deletes_only_what_the_guest_has`, `a_region_that_does_not_cover_a_host_preedit_is_dropped`, `a_region_that_does_not_cover_the_shown_one_is_dropped`, `an_empty_region_over_a_held_preedit_commits_it`, `an_answer_that_cannot_repeat_the_preedit_forgets_it` |
| X1' | ext `set_preedit_region` | any other case | none | Dropped, as H9 drops; the assembly is taken. | n/a | same |
| X2 | ext `confirm_preedit` | Gate; `shown = Some(p)` | `Txn{commit: p, preedit: none}` | `shown := none`; the lease ends; the assembly is taken. | Dropped; the assembly is taken. (`p` was encoded once, so this cannot happen in practice.) | Exo confirms instead of committing [F] |
| X2' | ext `confirm_preedit` | Gate; `shown = None` | Through the keyboard concern, if the seat has an eligible held key and the host's next event shows the confirmation to be that key's autorepeat, not Exo's confirmation before a new press (E13, E8). Then the key's press and release go, unless another channel already delivered it, then the G9c answer under the serial of then (`ImeCore::recovery_done`), if the text input still passes the gate and the key is held. Otherwise nothing is sent. **The `done` goes whenever the key is recovered, even when its pair is skipped.** | `KeyEffect::RecoverHeldKey`. [KEYBOARD.md](KEYBOARD.md#the-recovery-decision) owns the decision, and INVARIANTS "An empty confirmation recovers a held key only once the host shows it repeated" its rule, Exo's own detach (H2) included. **Lease:** armed if a held Backspace's pair is sent; ended if a held Backspace's pair is skipped, if another held key's pair is sent, or if no key is held; left alone if another held key's pair is skipped. The eligible key is the held key with the latest peek. | n/a | INVARIANTS "A recovered key carries the host's serial"; row test `row_X2prime_a_recovered_key_is_closed_under_the_serial_of_its_recovery`; tests `a_key_held_across_a_detach_is_typed_and_recovered_again`, `keys_held_since_enter_are_ordered_by_their_peeks` |
| X3 | ext grammar, autocorrect and occluded-bounds events | any | none | Ignored. | n/a | the adapter's handlers of the same names |
| W1 | `wl_callback.done` of a drain barrier | `Draining{b}` | none | Link → `Detached`. The final reconcile then activates if the field wants input. | n/a | |
| W2 | `wl_callback.done` of a reset fence | `fence = Some(f)` | none | `fence := None`, **in any link state**: the fence lives beside the link. | n/a | INVARIANTS press rule ("A focus change or the text input's end does not end it early") |

### 2.3 Cross-concern inputs and reconcile outcomes

| ID | Input (owner that sends it) | Precondition | Host requests | Guest events | Next state / notes | Sources |
|---|---|---|---|---|---|---|
| F1 | Seat focus change: `wl_keyboard.enter` of another surface, `wl_keyboard.leave`, or a keyboard's end (`KeyboardFocusRegistry`, via the keyboard handler's `enter`, `leave` and `forget_keyboard`) | Per text input on that seat, in `TiId` order | Through the reconcile: `deactivate` and a barrier, or `activate` | First `Txn{commit: p, preedit: none}` if `shown = Some(p)` and the composition is current. Then the keyboard events that moved the focus (`Input::FocusChanged::keyboard`: the guest's `wl_keyboard.leave`, or `enter` and its `modifiers`, which the keyboard handler hands the core rather than forwarding). Then `leave(old)` and `enter(new)`, each if it changed. | The field is reset: a new `FocusedField{surface, enabled: false}`, or `Unfocused`, with an empty pending state. The composition and the assembly are cleared. The keyboard ends the seat's leases and peek watermarks. Exo's own confirmation of its detach comes after the keyboard's `leave` (E12), while the link drains, and is dropped. | INVARIANTS "Focus teardown sends `leave` itself", "A field the focus leaves commits its preedit first"; tests `row_F1_a_focus_change_commits_the_shown_preedit_before_leave`, `a_focus_loss_commits_the_shown_preedit_before_leave` |
| F2 | Surface destroyed (`compositor::forget_surface`) | A text input focused on that surface | as F1 | `leave(s)`, while `s` is still valid. **No commit**: the guest destroyed the window the text would go into. | The focus is projected again from `Env::keyboard_focus`, which the destroy has already moved off `s`. `forget_surface` sends F2 before the F1 of each seat whose keyboard focused `s`, so that F1, which would commit, finds the field reset. | INVARIANTS "Focus teardown…"; tests `row_F2_a_destroyed_surface_is_left_while_it_is_valid`, `wl_surface_destroy_repairs_only_text_inputs_on_the_stale_surface` |
| F3 | `wl_seat` bound under a guest ID (`registry.rs`) | The text inputs of that seat | Through the reconcile: the deferred `activate`, with its content type | none | | INVARIANTS Object lifetimes "Releasing a parent does not end its children"; test `an_activation_deferred_for_an_unmapped_seat_sends_the_content_type` |
| F4 | Press: a `wl_pointer.button` press from a pointer whose `enter` named `s`, or a `wl_touch.down` on `s` (`pointer.rs` and `touch.rs`, through `text_input::end_composition_on_press`) | The enabled text input is focused on `s`; gate; `shown = Some(p)` | `zwp_text_input_v1.reset`, then a barrier (the fence) | `Txn{commit: p, preedit: none}`, then the press or down itself, re-sent by the core | `shown := none`; the assembly is taken; `fence := Some(f)`; the lease ends. A press whose commit does not fit changes nothing and is forwarded, and leaves the assembly (B3). A release, another surface (subsurfaces included), no composition, or an open fence changes nothing, and the press is forwarded. | INVARIANTS "A press ends the composition on its surface" |
| F5 | Key activity: a key press or repeat the keyboard concern forwards to the guest (`Input::GuestKey`, from `keyboard.rs` and `ime_keys.rs`) | The text inputs of that seat | none | none | The guest may have edited its text where Exo does not see, so the mirror stops measuring until the guest states its text (INVARIANTS "Exo's deletes and regions are measured in the Exo mirror, or dropped"). Leases are keyboard state: the keyboard ends them on a non-Backspace press and on a Backspace release. | INVARIANTS "IME state is scoped to its keyboard or seat"; row test `row_H9_after_a_forwarded_key_drops` |
| F6 | Host `delete_id` of the v1 object while the guest object is live (`forget_object`, as `Input::Destroy` with `host_deleted`) | any | Nothing to v1; the extension is destroyed | `delete_id`, as in G10 | Link → `Detached` with no requests. | test `a_host_destroyed_text_input_sends_nothing_to_its_host_object` |
| R1 | **Abandon** (reconcile outcome): the target is inactive, the link is `Attached`, and `Env::host_seat` is `None` | none | none | none | Link → `Detached` locally; the composition is cleared; **the fence stays**. Exo keeps its input method attached until its own focus moves; HANDOVER covers the cost. | `ActivationStep::Abandon`; HANDOVER "A text input whose guest `wl_seat` goes away…" |
| R2 | **Deferred activation** (reconcile outcome): the target is active, and the link is `Draining`, or `Detached` with no live seat | none | none | none | No change. The deferral is logged when it starts or changes its reason (`Deferral::Draining` or `NoSeat`), not at every step that finds it waiting. A missing seat warns once per text input. | `ime/host.rs`; `Warned` |

A seat that ends is **not** an input. The core notices it at the
reconcile that ends its next step, through `Env::host_seat`. An
`Attached` link with a dead seat is therefore a normal state (I1b;
INVARIANTS "Releasing a parent does not end its children"). A seat's
end changes no target's surface, so the link needs no request until its
target turns inactive. Only a commit, a focus change or a surface's end
can do that, and only then does the link end locally (R1).

### 2.4 Exo facts the models implement

Between Exo's text input and the engine stands `InputMethodAsh` (IMA),
so several facts are IMA's. IMA buffers the engine's text for a key
until the engine's callback for that key (`ProcessKeyEventDone`).

| ID | Fact | Status, and how the DST treats it |
|---|---|---|
| E1 | `activate` on the surface it already has does nothing. Otherwise it detaches (sending `leave`), takes the surface, and attaches (sending `enter`) only if the seat focuses that surface. | [F] `TextInput::Activate` |
| E2 | Attaching resets the type and mode to text and keeps the flags, learning and support. Detaching sets the type to none. Exo detaches on its own on focus loss, on surface destruction and when the input method is destroyed, each time through `Deactivate`. | [F] `AttachInputMethod`, `DetachInputMethod` and the observers |
| E3 | A content type is dropped while nothing is attached. `set_input_type` and `set_content_type` consume the pending support and reset it to supported. | [F] `SetTypeModeFlags`; `zwp_text_input_manager.cc` |
| E4 | Surrounding text and caret bounds are kept with or without an input method. Exo predicts the text by applying the input method's edits. **A report that lags behind an edit does not lose it:** `SurroundingTextTracker::Update` matches the reported text against its history of states and replays the edits made after the matching one. A report that matches no state resets the tracker to it, which drops the composition. | [F] `TextInput::SetSurroundingText`; `SurroundingTextTracker::Update` (`surrounding_text_tracker.cc`) |
| E5 | `reset` cancels Exo's composition (`SurroundingTextTracker::CancelComposition`) and calls IMA's `CancelComposition`. It sends nothing to the client. IMA's `ResetContext` drops IMA's buffered results (`pending_commit_`, `pending_composition_`), and it resets the engine **only if it was composing**. A key in flight at the reset therefore has **three outcomes**: (a) its text already went out, and the reset cancels its composition as usual; (b) its text is buffered in IMA, and the reset drops it; (c) the engine has not answered yet, and its answer, made from the old composition, arrives after the reset and is applied (the late answer, P3). | [F] `TextInput::Reset`; `InputMethodAsh::CancelComposition`, `ResetContext`; INVARIANTS press rule. The DST produces all three by timing; it does not draw them. |
| E6 | **A detach always confirms first.** `TextInput::DetachInputMethod` calls `DetachTextInputClient`. That reaches `InputMethodAsh::OnWillChangeFocusedClient`, which drops IMA's buffered commit and calls `ConfirmComposition(true)`, before `delegate_->Deactivated()` sends `leave`. At extension v11 or later, which the proxy requires (E14), Exo's `ConfirmCompositionText` sends `confirm_preedit` **even with no composition**, since `SupportsAlwaysConfirmComposition` is `SupportsConfirmPreedit`. | [F] `input_method_ash.cc` (`OnWillChangeFocusedClient`, `ConfirmComposition`); `text_input.cc` (`DetachInputMethod`, `ConfirmCompositionText`, `SupportsAlwaysConfirmComposition`) |
| E7 | A key goes through the input method before `Keyboard::OnKeyEvent`. A consumed key gets `peek_key` and no `wl_keyboard.key`. An unconsumed key gets `peek_key`, then `wl_keyboard.key` with the same serial. There are no repeats. **With no text input attached, keys skip the engine and reach the client raw**, as US-layout letters. | [F] `keyboard.cc`. The raw path: [F] the input method has no client (`InputMethodAsh` with type `NONE`). |
| E8 | **The order of a key's text and its key events follows the consumed bit.** A key the engine handled gets its `peek_key` first, then its text. A key the engine did not handle gets its text first (after E13's confirmation), then its `peek_key` and `wl_keyboard.key`. | [F] IMA `ProcessKeyEventDone` and `ProcessKeyEventPostIME` |
| E9 | When the engine commits the composition it tracks, it may use `confirm_preedit` (v11 and later) or `commit_string`. A commit of text that differs from the tracked composition, such as 각 + ㅏ committing 가, or E15's "가 ", is always `commit_string`. | [F] confirm confirms only the tracked composition. **The style of a confirmable commit is [U4]; drawn.** |
| E10 | `delete_surrounding_text` uses an absolute byte index into Exo's predicted text, comes with no `commit_string`, and happens only while nothing is composed. | [F] (INVARIANTS "Exo's deletes are absolute and stand alone") |
| E11 | Exo handles one client's requests in order, and its events to a client are in order. A `wl_display.sync`'s `done` follows every event of earlier requests and precedes every event of later ones. | [F] libwayland |
| E12 | **On a focus change, the seat tells its observers in the order they registered.** The keyboard and the text input both observe at priority 1 (`kKeyboardSeatObserverPriority`, `kTextInputSeatObserverPriority`). A keyboard registers when it is created. A text input registers at its first `activate` after a `Deactivate`, which unregisters it. So a keyboard bound before that `activate` hears first: `wl_keyboard.leave` and `enter` come before the text input's detach (E6's confirmation, then `leave`) and its attach. A keyboard bound after it hears second. | [F] `Keyboard`'s constructor (`keyboard.cc`); `TextInput::Activate`, `Deactivate` and `OnSurfaceFocused` (`text_input.cc`); `Seat::AddObserver` (`seat.h`). The DST binds a keyboard late in half its cases to reach H2 (4.6). |
| E13 | **An empty `confirm_preedit` comes before every key the engine did not handle**, at extension v11. For a press the engine did not handle that is not a modifier, IMA's `ProcessKeyEventDone` calls `ConfirmComposition(true)` before it passes the key on, and at v11 that confirms even with nothing composed (E6). This includes autorepeats, which `Keyboard::OnKeyEvent` then drops (E7). It is X2's source. | [F] IMA `ProcessKeyEventDone`. The captured trace agrees: a held Space's repeats arrive only as empty `confirm_preedit`s (test `held_space_repeats_after_ime_commit_until_physical_release`). |
| E14 | Exo advertises `zcr_text_input_extension_v1` at **v14**; v11, the first with `confirm_preedit`, shipped around M116 (August 2023). The proxy binds at most v11, so on a supported host the extension is v11 and confirms with `confirm_preedit`. The proxy gives a text input no input method on an older host (G1). | [F] `GetTextInputExtensionV1Version` (`wayland_server.cc`). The DST's host advertises v11. |
| E15 | **The Korean engine consumes Space.** With "가" composed, Space is handled, and the engine commits "가 ", the Space included, with `commit_string` (E9). | [F] captured host trace (test `held_space_repeats_after_ime_commit_until_physical_release`). The DST's engine models it (4.4). |

### 2.5 Client facts the models implement

The DST's client models (4.5) implement these facts, and each model
cites its sources.

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
| C11 | **Blur resets and disables.** A window that loses the keyboard focus loses its input method's client. The input method resets, which arms the guard if it composed (C4). The text input disables without a report, and later input-method events for that window are dropped. | Chromium | `WaylandToplevelWindow::UpdateActivationState`; `RenderWidgetHostViewAura` (blur); `InputMethodAuraLinux`. Host checks U13 and U14 (5.2). |
| C12 | One text input per display, and one input method context per field: the context of the focused field is current, receives every event, and alone sends requests. GTK3 sends requests only between its own `enable` and `disable`; GTK4 whenever its context is current. GTK3 states a surrounding text that changed twice in one report. GTK4 reports the caret it last laid out, and a caret that moved after layout goes out with a `commit` of its own. | GTK3, GTK4 | `GtkIMContextWaylandGlobal`; `after_layout_cb` (GTK4) |
| C13 | **Key-binding resets.** A passed Backspace that deletes, Left and Right reset the context; Enter does so when the entry needs a reset, without inserting a line break. GTK3 needs a reset after a printable key; GTK4 does not. | GTK3, GTK4 | `gtkentry.c`, `gtktext.c` |
| C14 | **Backspace leaves the start of a Hangul syllable.** It deletes the grapheme before the cursor and re-inserts all but the last character of its NFD decomposition: a passed Backspace after 가 leaves the conjoining jamo ᄀ, and after 각 the conjoining sequence U+1100 U+1161, not the precomposed 가. | GTK3, GTK4 | `gtk_entry_backspace` (GTK3), `gtk_text_backspace` (GTK4), with Pango's `backspace_deletes_character` |

## 3. The IME core

[DESIGN.md](DESIGN.md#keyboard-and-ime) owns the core's place in the
proxy: its inputs and the modules that send them, the adapter
(`handler/text_input.rs`), and the Exo mirror's owners. The module docs
of `sommelier/src/ime` own each type. This section covers what the code
cannot show at a glance: how the state is split, its transitions and
invariants, and how each guarantee is enforced.

### 3.0 Files

| File | Holds |
|---|---|
| `mod.rs` | The public API: `ImeCore` (`step`, `keysym_target`, `recovery_done`, `assert_invariants`), `Input`, `HostIme`, `Effects`, `HostOut`, `KeyEffect`, `LeaseOutcome`, `Env` and the ID newtypes |
| `state.rs` | `TextInput` and its parts (`Field`, `LinkState`, `Composition`, `Assembly`, `Warned`), `V3Pending`, `V3Committed`, and the gate (`Current`, `Planned`). The fields are private to this file, and every transition is a named method of `TextInput`. |
| `serial.rs` | `CommitCount` and `Serial`: no public constructor, and one bump |
| `step.rs` | `step`: one exhaustive match on `Input`, the end-of-step reconcile, removals and the trace log |
| `host.rs` | `HostTarget`, `target`, `reconcile` over `LinkState`, and the content-type mapping and requests |
| `guest.rs` | `Preedit` and `GuestTxn`, whose `encode` is the only builder of the v3 transaction events |
| `mirror.rs` | `ExoMirror`, the replica of Exo's `SurroundingTextTracker` |
| `exo_view.rs` | `ExoView`: a text input's mirror and whether the guest is in step with Exo; `Frame`, which measures X1 and H9, and `Unmeasured`, why the core drops one |
| `tests.rs` | The row tests of section 2, the encoder table and the source tests |

`ime` imports nothing from `handler`. From the rest of the crate it
uses only `wire`, the generated `protocols` and
`connection::MAX_BYTES_PER_SEND`. Two source tests in `ime/tests.rs`
enforce this:

- **`ime_is_isolated`** checks that every `use crate::` line of `ime/`
  names only those, that nothing in `ime/` names `crate::handler`, and
  that the four v3 event builders appear only in `guest.rs`, in the
  whole crate.
- **`count_bumps_only_in_the_commit_arm`** checks that
  `CommitCount::bump` has one call site, reached from the Commit arm
  through `TextInput::count_commit`.

### 3.1 State

`ImeCore` holds the text inputs of one connection in a `BTreeMap` by
`TiId`, and the next barrier number. `BarrierToken` numbers a host `wl_display.sync` the
core waits for. The core never reuses a number, and the adapter keeps
the one map from token to host callback (`InternalCallback::TextInput`).

A `TextInput` keeps its parts apart, so that no part can be left stale
when another changes:

- `field`: `Unfocused`, or `Focused` with the surface and the committed
  state (`V3Committed`). The committed `enabled` flag lives here, so
  ending or refocusing the field clears it by construction (B7).
- `pending`: the requests since the guest's latest commit (`V3Pending`).
- `link` (`LinkState`): what the host text input has applied.
  - `Detached`: no input method is attached, and none is about to be.
  - `Attached { content_type }`: `activate` was sent. Exo attaches when
    the surface has its focus (E1); a later v1 `leave` is Exo's own
    detach (H2).
  - `Draining { barrier }`: `deactivate` was sent, and the barrier
    drains Exo's events from before it.
- `fence`: the reset fence of a press (F4) until its `done` (W2). It
  lives beside the link and survives every change of it.
- `exo` (`ExoView`): Exo's text, which the host text input keeps for its
  whole life, so it lives beside the link too.
- `composition`: `shown`, the preedit the guest shows, and the assembly,
  the v1 `preedit_cursor` and `preedit_styling` that the next preedit or
  region applies.
- `commits` (`CommitCount`), and `warned`, which only makes a warning
  once per text input and which the invariants ignore.

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

`leave_attached` is private to `state.rs` and is the one writer of a
non-`Attached` link from `Attached`; the four exits above call it. It
ends the composition with the link. So a composition exists only while
attached (I2), and one function holds that rule. A new field is never
attached after its step, so it needs no transition of its own: a
disable makes the reconcile leave `Attached`, and an enable, or a
rejected one, starts from a link that was not attached (I1a). The same
holds for a refocus (F1, F2), which leaves the field disabled.

**Composition transitions.**

- `shown` changes in only four ways:
  - through `Planned::apply` of a plan that `Current` made (H7 to X2,
    F1, H2, F4);
  - through `leave_attached`, on any exit from `Attached`;
  - through the reconcile's exit from `Attached` after a new field
    (G9c) or a focus reset (F1, F2);
  - through the answer forgetting a preedit that no longer fits (G9b,
    G9c).
- The assembly records only while `Attached` (H4 and H5). Section 2's
  conventions say which events take it (B3). `leave_attached` clears it.

**Fence transitions.**

- `None → Some(f)`: only F4, which requires the gate, so no fence is
  pending then.
- `Some(f) → None`: only W2, with the same `f` (`barrier_done`, which
  ends the drain or the fence the token is, in any link state).

The fence is independent of the link, so the gate stays closed across an
Abandon (R1) and a new activation.

**The gate.** `TextInput::current` is the only constructor of
`Current`: it returns one only while the link is `Attached`, no fence is
pending and the field is focused. `Current`'s fields are private to
`state.rs`, so no other module can forge one. Each of its methods plans
the transaction of one row: `preedit` (H7), `host_commit` (H8), `plan`
(H9 and X1, a transaction `exo_view::Frame` measured), `confirm` (X2, or
X2' with nothing shown), `press_commit` (F4) and `commit_shown` (F1,
H2). A plan (`Planned`) changes nothing until `Planned::apply`, which
encodes the transaction and makes its change only if the encoding fits.
So no arm can apply a change whose encoding failed.

**Invariants.** `ImeCore::assert_invariants(&self, env)` checks I1a to
I3, I6 and I7 after every `step` in tests, and
`Context::assert_no_orphans` calls it after every batch (DESIGN
[Object ends](DESIGN.md#object-ends)).

- **I1a.** `Attached` implies `Focused(f)` with `f.committed.enabled`.
  This holds between steps, because every step ends with a reconcile:
  a target that turned inactive has just been deactivated (`Draining`)
  or abandoned (`Detached`).
- **I1b.** After a step, the link matches the target, except in three
  cases. It is checked by matching the target against the link, not by
  asking `reconcile` whether anything is left to send:
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
- **I5.** Every token in `Draining{b}` or in `fence` is pending in
  `InternalCallbacks` as `InternalCallback::TextInput { token }`. The
  adapter's side checks this, in `Context::assert_no_orphans`.
- **I6.** Every pending drain and fence is a token the core numbered
  (below its next number), and each is pending once across all text
  inputs. With I5, this makes the adapter's map from token to host
  callback exact.
- **I7.** An unfocused text input has nothing pending: a refocus drops
  the pending state (test `row_G9c_a_refocus_drops_the_pending_enable`).

`Env`'s documented precondition makes I1b hold after every step: every
change of an answer that can deactivate a target, a surface's end or a
focus change, reaches the core as an `Input` in the same message. A
seat's end is the one exception (2.3, after R2).

### 3.2 The guest transaction and effects

- A `Preedit` is a preedit as the guest is shown it. `Preedit::new`
  returns `None` for empty text, so an empty preedit is no preedit.
- A `GuestTxn` is what the guest applies at one `done`: a delete, a
  commit and a preedit, each optional. `preedit: None` clears a preedit
  the guest shows; whether the guest showed one (`shown_before`) tells
  "clear" from "omit".
- `GuestTxn::encode(ti, shown_before, serial)` is the only builder of
  `zwp_text_input_v3` `preedit_string`, `delete_surrounding_text`,
  `commit_string` and `done`, in the order INVARIANTS "A v3
  transaction's events go in one order" gives.
  **`guest_txn_encodes_in_canonical_order`** pins all 16 combinations
  of a shown preedit, a delete, a commit and a new preedit, and that
  empty commits and `(0, 0)` deletes send nothing.
- `Serial` comes only from `CommitCount`, so a `done` cannot carry
  another number.
- The G9c answer is a `GuestTxn` of the shown preedit alone, encoded
  with `shown_before` false: it sends no clear, and it repeats the shown
  preedit even when unchanged. The transaction after a recovered key
  (X2', `ImeCore::recovery_done`) is the same answer; with nothing shown
  it is the bare `done` of the same encoder.

A step returns its `Effects`, each list in order:

- `to_guest`: guest events. Every IME transaction ends with its `done`.
  A press that ended a composition (F4) is re-sent here, after its
  transaction, and so are the keyboard events of a focus change (F1).
- `to_host`: `HostOut`s. A `Request`; a `Barrier`, a host
  `wl_display.sync` whose `done` comes back as `Input::BarrierDone`; a
  `Glued`, a request and a barrier that go out in one submission
  (INVARIANTS "Exo's deletes and regions are measured in the Exo mirror,
  or dropped"); and `Destroy`, a destructor request, after which the
  adapter retires the host object it destroyed. Each carries its
  request, so the pairing holds by construction.
- `keys`: `KeyEffect`s for the keyboard concern, applied after the
  other two: a `Lease` outcome (`End` or `ArmIfBackspaceHeld`), or
  `RecoverHeldKey` (X2').
- `forward`: whether the press of an `Input::Press` is forwarded, since
  it ended no composition. The core consumes every other input's
  message.

### 3.3 The boundary with the rest of the proxy

`ime::Input` has one variant per trigger. The guest's requests are G1 to
G10, the host's events H, X and W, and the cross-concern messages F1 to
F5. [DESIGN.md](DESIGN.md#keyboard-and-ime) lists the module that sends
each one.

**Why `Env` and not more inputs.** Seat liveness and surface pairing
belong to the object table, and keyboard focus belongs to
`KeyboardFocusRegistry`. Copying them into the core would give them a
second owner. `Env` reads them during the same dispatch, so `step` stays
deterministic and does no I/O.

**ID mapping.**

- At `get_text_input`, the adapter allocates the bridged host IDs
  (`ShadowTable::create_bridged` and `create_host_only`) and passes them
  in `Create`. The core sends `create_text_input` and
  `get_extended_text_input`.
- The adapter resolves the sender of a v1 event through the object
  table; the core resolves an extension's sender among its text inputs.
- Host surface and seat IDs come from `Env` when a request needs them.
- The core never sees host callback IDs, only `BarrierToken`s.
- The guest object's `delete_id` stays with the generated local
  destructor path (G2, G10). It is not an effect.

**Ordering.**

- A message is still one transaction (DESIGN
  [Message effects](DESIGN.md#message-effects)).
- The adapter appends the core's output at the point of the call, so
  order within a direction is the handler's. For example, a surface's
  Aura release goes before its text-input `leave` (INVARIANTS Object
  lifetimes).
- If the session ends during the message, the loop drops every effect.
  The core's new state does not matter then, because no further message
  is handled.

**Trace log.** At `trace` level, on target `sommelier::ime`, `step` logs
each `Input`, each `Env` answer it reads (through a tracing wrapper of
`Env`), and each `Effects`. It carries the user's text, so it is only
ever at `trace`.

### 3.4 The step function

`step` has three phases:

1. **Arm.** One match arm per `Input`, following the rows of section 2.
   Each arm computes everything it needs (plans, encodings) before it
   changes anything (3.5). It then changes the state and appends to the
   step's `Effects`.
2. **Reconcile all.** For every text input in `TiId` order, it computes
   the `HostTarget` (with `Env`), calls `reconcile(&target, &link)`, and
   applies the steps. This runs **unconditionally**, at the end of every
   input. `reconcile` is pure, cheap and idempotent: when the link
   equals the target it returns nothing. The Commit arm hands this phase
   its encoded `set_surrounding_text` and cursor rectangle, so the wire
   order of the "Reconcile" convention (section 2) is kept.
3. **Removals.** The Destroy arm marks a text input as ending, and phase
   2 reconciles it to inactive. It is then removed, and its extension's
   `Destroy` is appended.

A host composition event first goes to the mirror, then through the
gate: without a `Current`, it is stale and dropped. With one, the arm
plans its row's transaction, applies it, and appends the events and the
lease.

### 3.5 Failure rules

There are two failure domains, with one rule each.

- **A guest transaction that does not encode, for a host event or a
  press** (H7 to X2, F4). The event is dropped, and the assembly is
  taken but by a commit or a delete (B3). Nothing else changes, and no
  `done` is owed, because the trigger was a host event. A press is then
  forwarded unchanged.
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

| Class | What goes wrong | Mechanism | Enforced by |
|---|---|---|---|
| B1 hidden host state | The proxy mispredicts what Exo applied (E2, E3). | The link changes only through `TextInput`'s seven named transitions (3.1). `step` reconciles every text input after every input. | **The structure** for "no arm forgets to reconcile": the call is in `step`, not in the arms. **Module privacy plus review** for "only reconcile, H2, W1, F6 and G10 call the transition methods". The row tests pin each transition. |
| B2 stale epoch | An event sent before a `deactivate`, a `reset` or a report reaches the guest as current. | `Current<'_>` is constructible only in `state.rs`, by `TextInput::current()`. Host-event and press transactions are planned only through `Current`'s methods. | **Module privacy (compiler)** inside `ime`: `step.rs` cannot build a `Current`, nor set `shown`. |
| B3 leftover pieces | A v1 event's pieces (cursor, styling) apply to the next event. | `take_assembly()` comes first in the preedit and region arms, and after the plan in the commit, delete and confirmation arms and in the press; `step.rs`'s `Takes` names which, per event kind. Every exit from `Attached` goes through `leave_attached`. | **A convention plus row tests** for "first". **Module privacy** for the generation boundary, and I2 checks the result after every step. Row tests feed styling and a cursor before each terminal event in flight at a press, and before each dropped commit, delete and confirmation (`row_F4_events_in_flight_at_a_press_are_dropped`, `row_H8_H9_X2_only_an_edit_that_reaches_the_guest_takes_the_assembly`, `row_F4_a_press_whose_commit_does_not_fit_keeps_the_assembly`). |
| B4 serial drift | A v3 `done` serial misses a guest commit. | `CommitCount` sits in `serial.rs` with a private field. `bump()` is `pub(super)` and called only through `TextInput::count_commit`, from the Commit arm. `Serial` has no other constructor. | **Module privacy** for the constructor. **A source test** (`count_bumps_only_in_the_commit_arm`) for the one call site. |
| B5 malformed transaction | A transaction lacks its `done`, or keeps or clears a preedit wrongly. | `GuestTxn::encode` is the only builder of the four v3 events, and it always ends with `done`. | **A source test** (`ime_is_isolated`). A crate-wide clippy `disallowed-methods` entry would not do: `sommelier/clippy.toml` notes that an allow for one rule lifts the descriptor and blocking guards too. |
| B6 missed trigger | A cross-concern trigger is missed, or handled twice. | Every trigger is an `Input` variant, and the match has no wildcard. | **The compiler**, for a variant that is unhandled. **The DST**, for a source that is never wired. |
| B7 one enabled | Two text inputs are enabled, and the first stays deaf. | `enabled` lives in `FocusedField`, so a focus reset or a destroy clears it by construction. "At most one" is checked where `enabled := true` is set. | **The type** for staleness. **The Commit arm plus I3** for the count. |
| B8 order | Output depends on hash order. | `BTreeMap` in the core; INVARIANTS Wire messages "Nothing the proxy sends depends on hash order" covers the crate. | **The type and clippy**. The DST's determinism check runs some cases twice, which also covers the host ID allocator's reuse of freed IDs. |
| B9 races | A race only a reviewer finds. | The DST. | CI. |

What stays outside the core, and reaches it only through `Input`
messages and `Env` reads:

- key generations, `peek_key`, `ack_key`, keysym routing and held-key
  recovery ([KEYBOARD.md](KEYBOARD.md));
- pointer focus (`PointerFocus`);
- keyboard focus (`KeyboardFocusRegistry`);
- surface state.

## 4. The DST harness

[CONTRIBUTING](../CONTRIBUTING.md#simulation-test) says how to run it,
what a failure prints, and how a failure is shrunk and kept as a
regression. The module docs of `sommelier/src/sim` hold the details
this section leaves out, such as each channel's latency and the mix of
user actions.

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
  it on the same channel.
- **Step.** A step delivers the channel head with the earliest ready
  time, or injects the next user action when its time comes.
- **Determinism.** Everything runs in one thread, and every draw comes
  from a recorded choice stream, so a seed reproduces a run exactly.
- **The proxy.** The DST drives it only through the wire, with
  `proxy::harness::Session`, so it tests the real handlers and the core
  unchanged. A session end is a reported failure, not a panic.

### 4.1 Files

| File (`sommelier/src/sim/`) | Holds |
|---|---|
| `mod.rs` | `Case`, the run and its summary, and the two CI tests |
| `sched.rs` | The `Choose` trait, the recorded `Choices`, the channels and the step loop (`World`) |
| `clock.rs` | `Micros`, the virtual clock's unit |
| `exo.rs` | The Exo model, which the fuzzer imports too: there is one Exo model |
| `engine.rs` | The asynchronous IME engine over `Composer` |
| `hangul.rs` | `Key`, `Output` and `Composer`, the Hangul composer |
| `keymap.rs` | `Key` to evdev keycode for the US layout, and the click and tap positions |
| `client/` | The `Client` trait and the Chromium and GTK models |
| `editor.rs` | `TextModel`, `Txn`, `Target`, `UserAction` and `Reference`, the ideal editor |
| `user.rs` | The user action generator, free and settled |
| `display.rs` | The host display the DST plays: syncs, `delete_id` and globals |
| `surfaces.rs` | Each field's surfaces, a generation each (F2) |
| `oracle.rs` | The exact oracle (`SpecText`, `Shadow`) and the user-intent oracle |
| `shrink.rs` | Minimizing a failing case |
| `regressions.rs` | Shrunk cases kept as regressions |

The simulation's `Txn` is the client-side view, with byte counts. It is
distinct from the core's wire-side `GuestTxn`.

### 4.2 The scheduler, its clock and the legal reorderings

The real system's concurrency comes down to FIFO channels: the guest's
socket (C→P, P→C), the VirtWL connection (P→E, E→P), aura's event queue
for user actions (U→E), and the mojo pipe between Exo and the engine
(E→Eng, Eng→E). `sim/sched.rs` lists each channel's latency.

- **To the proxy.** A message from C→P or E→P is handed over one at a
  time. That is the finest interleaving `Client::handle_msgs` can
  produce: its batches and fair dispatch only coarsen it. The proxy's
  output is read at once and sent on P→C and P→E.
- **To the client, Exo and the engine.** Each handles one message
  atomically. The engine works through keys one at a time, each taking
  a service time drawn from the case's lag profile (`engine::Lag`).
- **Ties** in ready time are broken by a canonical channel order, then by
  a choice.
- **Reorderings.** Nothing overtakes within a channel. All reordering
  comes from the drawn latencies. A click can overtake an engine result
  still in service (E5's race, P3), and a guest report can cross an
  engine edit on its way (P4).
- **Time.** Chromium's 100 ms guard (C4), the user's timing and
  autorepeat use the same clock. A `Settle` waits until every channel is
  empty and the engine is idle, then a little longer.
- **Choices.** Every draw (a latency, a service time, a tie, a [U]
  variant) goes through `Choose`. Unit tests script it with a closure;
  a case records its `Choices` and replays them. Choice 0 is always the
  minimum latency, or the canonical order, so an all-zero stream is the
  most sequential schedule, which the shrinker aims for.
- **Quiescence.** At the end the scheduler delivers until every channel
  is empty. The system always settles, because clients only react, Exo
  answers every sync, and the engine answers every key.

### 4.3 The Exo model

`sim/exo.rs` models what the proxy sees of Exo:

- **Typed requests and one event queue.** It takes the requests a
  `Session` decodes. Every event Exo sends, from the text inputs, the
  seat and the input method, goes into one queue in the order Exo sends
  it.
- **No display.** The model does not answer `wl_display.sync` or a
  destructor's `delete_id`; whoever plays the display does (the fuzzer's
  compositor, or `sim/display.rs`). The display takes the model's events
  after each input and before it answers any later sync, so that a
  sync's `done` follows every event of earlier requests (E11). It reads
  a surrounding text or a `reset` and the sync glued to it in one go, as
  Exo's libwayland does.
- **Seat.** It tracks the focus. It sends `wl_keyboard.enter` and
  `leave`, with `modifiers` before `enter`, as Exo does, and `peek_key`,
  and it takes `ack_key`. A focus move reaches the keyboard and the text
  inputs in E12's order.
- **Text input.** Each host `zwp_text_input_v1` keeps the text that
  `SurroundingTextTracker` predicts (E4), with its history, and its
  composition. `reset` cancels the composition, sends nothing, and
  reaches the engine as a `Reset` notice (E5). A detach confirms first
  (E6), then sends `leave`.
- **IMA.** Between the engine and the text input, the model follows
  IMA's rules: it buffers a key's text until the engine's callback for
  that key; a handled press goes out before its text, and an unhandled
  press after its text (E8); an unhandled press is preceded by a
  confirmation (E13); a reset drops IMA's buffer (E5).
- **Keys.** A key on the focused surface with an attached text input
  goes to the engine. **With no attached text input, the key goes raw
  (E7)**: during a drain or before the first `activate`. The keyboard
  passes a press only of a key it does not hold, so a key whose release
  died with the input method gets only the confirmation of its next
  press (E13), until the keyboard's next `enter`: `Keyboard::SetFocus`
  takes the keys it holds from the seat, which the model's `enter`
  lists as none.
- **Engine results.** A preedit, a commit (`confirm_preedit` or
  `commit_string`, as E9 and the case's draw say), or a delete measured
  in the predicted text (E10). If IMA serves no text input, the text is
  dropped, but the key's events still go out, in E8's order. An answer
  that sends text the engine made before a reset is **late** (E5's
  outcome (c)); the scheduler labels it for the oracle and the P3
  counter.
- **Draws per case:** E9's style of a confirmable commit.

### 4.4 The engine

`sim/engine.rs` answers each key in two parts, as IMA sees them: the
key's text, and the engine's callback for the key with its handled bit.

- **Korean mode** runs `hangul::Composer`. Space ending a composition is
  consumed and committed with it (E15). With nothing composed, Space and
  Enter go to the client. **Latin mode** passes every key.
- **`Reset`** takes effect when the engine reaches it, after every key
  handed over before it. A key queued before a `Reset` is processed with
  the old composer state (E5's outcome (c), P3).
- **D6, suggestions and reconversions.** The user can accept a
  suggestion, which deletes the last word and commits a replacement,
  only while nothing is composed (E10), or reconvert text into a
  preedit region (X1). In a free case, half of each is followed at once
  by another: a delete after an edit the guest has yet to report (P4),
  or a region over a region. This exercises H9, X1 and P4.
- **D7, held keys.** A free case can hold a key while it types up to
  two others. The engine gets its autorepeats (500 ms delay, 20 Hz),
  which Exo forwards to no client (E7), and IMA precedes each unhandled
  one with an empty `confirm_preedit` (E13). This exercises X2' and the
  Backspace lease. Settled cases hold no key: the user's intent for a
  hold depends on the client's own repeat, which the client models do
  not have.

### 4.5 The client models

Each model implements the `Client` trait (`sim/client/mod.rs`), holds a
`TextModel` per field, and reacts only to what the application learns:
a transaction at `done`, an unconsumed key, and a press and its release.

| Model | Rules (2.5) | Drops it labels | Setup |
|---|---|---|---|
| `Chromium` | C1 to C5, C10 and C11. Reports are gated on `last_done == commit_count` and carry the predicted text. The text input is enabled for a window only while the window is focused and entered. Every press in a focused window resets; a touch confirms on a tap. The 100 ms guard belongs to each window. | A delete while composing (C3). A delete that splits a character of the predicted text. Commits and preedits within 100 ms of a reset that ended a composition in the window (C4). Anything for a window without the keyboard focus (C11). Events held when the text input reset, enabled or disabled. | **One text input per connection** (C10), shared by its windows. Each field is a window on a surface of its own; two fields, one seat. |
| `Gtk { v4 }` | C6 to C9 and C12 to C14. A `done` is answered only when it changed something (C6). A context per field. Enable on a single-click release that did not drag. Reset on a press. Key-binding resets; Backspace leaves the NFD prefix. | Anything while no field has the keyboard focus. | One text input per display, a context per field, two fields on two surfaces. `GtkEntry` and `GtkText` only; `GtkTextView` differs and is not modelled. |

Each model also asserts the v3 protocol as it receives events, and a
violation fails the case:

- a `done` serial is at most the model's commit count (with wrapping);
- text-input events arrive only between `enter` and `leave`, `enter`
  does not repeat, and `leave` names the entered surface;
- every event names an object the model holds, and a surface it has;
- every string is UTF-8;
- a delete fits one of the surrounding texts the client sent since it
  last enabled (`TextModel::delete_range`).

The models never hold a selection, because `TextModel` has none. The
proxy's selection rules (H9's drop while the guest has a selection, C5,
C9's anchor, and the no-op rules) are therefore pinned only by row tests
and Session tests, not by the DST.

### 4.6 User actions

`sim/user.rs` generates the actions and documents their mix: keys,
holds, suggestions, reconversions, clicks, taps, focus moves, restarts
of the input method and windows that open again. Targets name marker
digits seeded into the initial text, so a target means the same place
to the user and to a client that lags behind.

- **Restarts** destroy Exo's input method, which confirms and detaches
  on its own (E2, E6). Production reaches H2 another way: a keyboard
  bound after the text input's `activate` hears of a focus move after
  the text input (E12), so the text input's detach comes before the
  keyboard's `leave`. Half the cases bind their keyboard again once the
  text input is active, which the counter `detaches_before_keyboard`
  counts.
- **Windows that open again (F2)** close a field's window and open it
  again on a new surface, with the same text. Exo drops the destroyed
  surface's focus with no keyboard `leave` and deactivates its text
  inputs; the new surface takes the focus at its first commit. An event
  that names the old surface after the proxy's `delete_id`, or one the
  proxy sends after it got the destroy, fails the case.

There are two generator modes, half of the cases each:

- **Free.** Any timing. The exact oracle judges it, which is valid for
  every timing.
- **Settled.** Both oracles judge it. A `Settle` comes before and after
  every click, tap, focus move, restart, suggestion and reconversion.
  Otherwise Exo could hand the engine a key after a press but before its
  `reset`, whose result the proxy drops by design (the fence), while the
  reference applies every key; or a key could reach the client raw while
  an activation is in flight (E7). `sim/user.rs` lists the other
  settles. Settled cases hold no key and reopen no window.

### 4.7 The oracles

**Every step:**

- the clients' and Exo's protocol assertions;
- `Session`'s `assert_no_orphans`, which includes
  `ImeCore::assert_invariants`;
- no `wl_display.error`.

**Tier 1: the exact oracle, at the Exo boundary. All cases.**

- **Reference.** `oracle::SpecText` reads every message delivered to the
  proxy, in order, and applies the text semantics of section 2 to an
  ideal client per field. From the inputs alone it decides what the
  proxy must ask of the host: an `activate` or a `deactivate` and its
  barrier where the field's wish and the link differ, and a `reset` and
  its fence for a press that ends a composition. Each step's decisions
  must be the proxy's own. It applies H7 to X2 by their rows, F4 at each
  press on the composing surface, forwarded keys (raw ones included),
  and F1, F2 and H2 at focus changes, surface ends and Exo's own detach.
- **Shadow.** `oracle::Shadow` is the same ideal client fed what the
  proxy actually sent the guest. It has no client drops.
- **Checks at quiescence:**
  1. **Text.** For each field, `Shadow` equals `SpecText` in text,
     cursor, preedit and preedit cursor. This is exact for every timing,
     because both sides are ideal clients.
  2. **Composition.** For the focused field's attached text input,
     Exo's composition equals `SpecText`'s preedit.
  3. **Field state.** Exo's attached field has the content type,
     support, surrounding text and cursor rectangle that the client last
     committed.
- **Counters** (in the summary): late results applied after a fence
  (P3), text a fence dropped, raw keys, deletes and regions and the
  proxy's drops by cause, reports that crossed an edit (P4), autorepeats
  and what each empty confirmation became (X2'), Backspace leases,
  surfaces destroyed (F2), client drops by reason, and one counter per
  draw. A counter `check` (`sim/mod.rs`) lists that stays at zero over a
  full run fails the run; the summary prints the rest.

**Tier 2: the user-intent oracle. Settled cases only.**

- The client model's `TextModel` per field must equal
  `Reference::fields()`, in text, cursor and preedit, after
  `Reference::apply` has run over the same actions, and the client's
  focused field must equal `Reference::focus()`. A difference is
  reported with the client's drop log.
- **Per-client Backspace.** GTK re-inserts a syllable's NFD prefix on a
  passed Backspace (C14), so `Reference` takes a per-client flag for
  Backspace, set for the GTK models.
- **NFC.** Tier 2 compares both sides after NFC normalisation, and the
  cursor as the length of the normalised text before it, so the
  comparison does not depend on which form a client keeps.

### 4.8 The mirror checks

The DST also holds the proxy to Exo's real text:

- `SpecText` measures each delete and region in what the simulated Exo
  really held before it (`RegionTruth`). The guest must get exactly that
  transaction or none.
- The proxy counts its drops by cause (`Counts::drops`). `SpecText`
  counts those where the guest held Exo's text (`drops_truth_in_step`).
  Those are the drops a sharper mirror could avoid. A settled case may
  make no drop.
- At every `done` of a sync, a mirror that says it is exact must hold
  the simulated tracker's prediction at that sync (`Exo::synced`). Its
  history must also fit the tracker's: each known state is the
  fingerprint of the tracker's state, an unknown state fits any state,
  and a leading unknown one fits one or more of them.
- A withheld report (G9c) is accepted only while the guest shows a
  preedit, and the simulated tracker's history must hold the report with
  that preedit as its composition at the report's cursor
  (`Exo::holds_with_composition`), whether or not an input method is
  attached: Exo keeps the tracker without one (E4), so a report withheld
  after Exo moved the focus, which reaches Exo after its detach, is held
  to it too.

## 5. Open questions

### 5.1 Risks

| Risk | Mitigation |
|---|---|
| **The models are wrong** (Exo, the engine or the clients), so the DST passes on traces real hosts never produce, or fails on impossible ones. | Every model rule cites a source (2.4, 2.5) or is marked [U] and **drawn** (E9), so both branches stay covered. The host checks in 5.2 settle the rest. A DST failure is checked against the citations before any proxy fix. |
| **`SpecText` and the core share a misreading of section 2.** | Tier 2 checks the outcome against user intent, independently of section 2. |
| **The DST covers only some clients.** It models Chromium, GTK3 and GTK4, without selections, highlights, Qt, Firefox, terminals or several text inputs. | Row tests and Session tests pin the rest. Adding `anchor` to `TextModel` for selections is a local change in `sim/`. |
| **The clock is unrealistic.** | Latencies come from three lag profiles with a spike. The host check U6, with timestamps, calibrates them. |
| **User text in logs.** | The trace log is at `trace` only, which is off by default. Scrub a log before sharing it. |
| **Upstream drift.** Exo's v1 path is not supported upstream, and Chromium's v3 client changes. | Section 2 is the one place to update. The client models carry their revisions, and a changed fact is a row change plus a model change. |

### 5.2 Unknowns and manual host checks

Each check runs on a Chromebook against an isolated test proxy, never by
touching the primary Sommelier service (INVARIANTS
[Manual verification](INVARIANTS.md#manual-verification)). Use
`RUST_LOG=sommelier::ime=trace` and `WAYLAND_DEBUG=1` on the guest
client.

| ID | Unknown | Check | Settles |
|---|---|---|---|
| U1 | Does Exo ever send `zwp_text_input_v1.keysym`? | Log a week of Korean and English typing. | H10, and whether the keysym channel stays |
| U4 | Does the Korean engine commit a confirmable composition with `commit_string` or with `confirm_preedit`? E15's Space commits "가 " with `commit_string`, but that text is not the composition. | Type "가", then Enter, or click elsewhere. | E9 |
| U5 | Should the composition be kept on blur? Native Chrome keeps it. F1 commits it instead, as mutter and ibus do; keeping it would need Exo to keep its composition across the detach, which E6 rules out. | Type "가" in a native Chrome field, Alt-Tab, and compare with the guest. | F1 |
| U6 | How often does an engine commit for a key pressed just before a click arrive after the fence (P3), and what are real engine latencies? | HANDOVER's "Press Space and click at the same instant", counted, with timestamps. | Q1; the lag profiles |
| U7 | Does Exo send `wl_touch` to Crostini clients, and in what order relative to text-input events? | Tablet mode. | F4 for touch |
| U8 | Does the `done` that answers each commit move the candidate window with the caret? | HANDOVER's check. | G9c |
| U10 | In GTK3, does `GtkEntry` update its cursor location before the click's release? This decides whether C7's release-time enable happens. | `WAYLAND_DEBUG` on gedit while clicking. | The GTK3 model's C7 |
| U12 | Does the ChromeOS Korean engine combine two lone consonants (ㄱ + ㅅ → ㄳ), as libhangul's default and the DST's `Composer` do? | Type ㄱ then ㅅ with no vowel. | The `Composer`'s lone-consonant rule |
| U13 | Does Chromium's blur follow the keyboard focus or the xdg `activated` state? If it follows `activated`, which can come before the keyboard's `leave`, Chromium blurs before F1's commit reaches it and drops that commit (C11), so it keeps the syllable only if its blur confirms it (U14). | `WAYLAND_DEBUG` on a Chromium guest while Alt-Tabbing, and while focusing another guest window. | C11; F1 |
| U14 | Does Chromium's blur call `ResetContext`, and so confirm a composition locally? F1's commit reaches it before the blur, so a local confirmation finds nothing composed; a double commit would mean the commit came after it. | Type "가" in a Chromium guest, Alt-Tab, and come back. | C11; F1 |
| U15 | Does a Chromium tap-down finish the composition, or only the tap's lift? | Tablet mode: type "가", then touch the field and hold. | C4 for touch |

Whether the ARC placement backend bypasses the IME (U9) is covered under
[The ARC backend and the IME](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#the-arc-backend-and-the-ime).

### 5.3 Open design questions

- **Q1. Can the engine-latency window (E5, P3) be closed?**
  - Option 1: replace `reset` with a `deactivate`, a barrier and an
    `activate`, so that Exo's own confirmation commits with the
    engine's final text. INVARIANTS rejects this, because keys bypass
    the input method until the `activate`.
  - Option 2: match results to keys by `peek_key`. E8 fixes the order
    of a key's text and its `peek_key`.
  - Recommendation: measure with U6 and the P3 counter, then decide.
- **Q3. Guest-initiated reset (G6).** A guest that resets its own
  composition does not reset Exo. GTK's reset is a commit with
  `cause=other`, and GTK4 sends one on every press, which F4 already
  covers. Decide on DST evidence whether a host `reset` behind a fence
  is needed (HANDOVER lists it).
- **Q4. Subsurface presses.** mutter matches the surface and KWin the
  window. Chromium and Firefox subsurfaces have an empty input region.
  Leave as is. The DST does not model subsurfaces.
- **Q5. A rejected enable** still applies the rest of its commit (G9c).
  Tightening it would be a one-line change in the Commit arm, if
  wanted.
