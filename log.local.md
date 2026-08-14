# Local investigation log

## 2026-08-12 — resumed Sommelier-RS review

- Worktree: `_local/worktrees/review-sommelier-rs`
- Branch: `review-sommelier-rs`
- Starting commit: `0af8d1a`
- No commit or push has been performed for this review.
- Existing uncommitted changes are preserved; they touch Sommelier and Wayland code generation.
- Passing checks from the previous review pass:
  - `cargo test -p sommelier --all-targets` (231 passed)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo check --workspace --all-targets`
  - `cargo fmt --all -- --check`
  - `git diff --check`
- Still required before completion:
  - compare relevant behavior with ChromiumOS Sommelier
  - add regression tests before confirmed bug fixes
  - run `bun run verify`
  - create/update `walkthrough.md` with evidence
- ChromiumOS reference tree:
  `/home/kkimdev/workspaces/openstd/_local/chromiumos-platform2/vm_tools/sommelier/`
- Review risks recorded so far:
  1. ChromiumOS clamps damage coordinates to `INT_MIN / 10 .. INT_MAX / 10`; Rust damage handling may accept the full `i32` range.
  2. Buffer transform and viewport edge cases need explicit parity tests, especially 90/270-degree transforms, non-square buffers, fractional source offsets, and source-only/destination-only viewports.
  3. `wl_surface.offset` is stored but may not participate in full-copy/full-damage decisions.
  4. Internal registry reset may remove shadow registrations without sending host-side destroy/release requests.
  5. Async dmabuf create/created/failed lifecycle needs regression coverage for ID mapping, stale events, params destruction, plane FD cleanup, and invalid metadata.

Next action: inspect the current diff and ChromiumOS damage/transform/lifecycle implementations, then record each confirmed finding here before editing code.

## 2026-08-12 — ChromiumOS comparison pass

- ChromiumOS `sl_host_surface_damage()` stores the raw surface rectangle for
  SHM copying, then separately transforms/outsets the host compositor damage.
  `sl_host_surface_damage_buffer()` stores raw buffer damage and converts it
  to surface coordinates with `compute_buffer_scale_and_offset()`.
- ChromiumOS `compute_buffer_scale_and_offset()` semantics confirmed:
  - source offsets are used only when both source coordinates are non-negative;
  - destination scaling is applied only when both destination dimensions are
    positive;
  - a source rectangle affects scale only when destination is also set;
  - source-only viewports determine surface size but do not add a source-size
    scale factor.
- ChromiumOS `copy_damaged_rect()` clips the converted rectangle to the
  contents bounds before pointer arithmetic and uses the stored raw damage
  regions for copying.
- ChromiumOS creates a newly attached output buffer with full
  `MIN_SIZE..MAX_SIZE` damage, where `MIN_SIZE = INT_MIN / 10` and
  `MAX_SIZE = INT_MAX / 10`; this protects transformed damage arithmetic from
  overflow.
- The Rust path currently expands/stores surface damage in the translated
  host queue and maps buffer damage with floating-point/truncation logic. This
  needs numeric parity tests, especially source-only viewport and extreme
  damage values, before declaring it equivalent.
- The current diff also changes generated server-side `new_id` events to use a
  reserved guest server-ID range. This is correct only if the generated event
  mapping leaves the host ID registered long enough for later events and
  destructor cleanup; lifecycle tests must cover that path.

## 2026-08-12 — review resumed after context compaction

- Read this log before continuing; do not rely on conversational context alone.
- User explicitly permits using the ChromiumOS Sommelier implementation as the
  behavioral reference for finding and fixing incomplete fork behavior.
- Continue in `_local/worktrees/review-sommelier-rs` on `review-sommelier-rs`.
- Preserve all existing uncommitted edits. Do not commit or push unless the user
  explicitly requests it.
- Do not restart the primary Sommelier, Orca, or unrelated GUI applications;
  use unit/integration tests and isolated processes only.
- Current focused review order:
  1. damage/viewport numerical parity and overflow safety;
  2. `wl_surface.offset` damage/full-copy behavior;
  3. registry internal-binding teardown;
  4. linux-dmabuf async lifecycle and generated-ID cleanup.

## 2026-08-13 — registry and linux-dmabuf lifecycle review

- Compared `registry.rs` and `linux_dmabuf.rs` with ChromiumOS
  `sommelier.cc` registry removal and
  `compositor/sommelier-linux-dmabuf.cc`.
- Confirmed linux-dmabuf plane FD ownership is balanced: `on_add` duplicates
  the received FD, the proxy consumes/closes the original, and pending/queued
  duplicates are closed on destroy, invalid create, disconnect, or `Context`
  drop. Async `create` removes pending parameters before forwarding; generated
  `created` maps the host-generated `wl_buffer` ID into Wayland's reserved guest
  server-ID range, while `failed` carries no stale pending FD state.
- Found and fixed a registry teardown gap: when an internally bound dmabuf
  global disappears, the proxy now queues `zwp_linux_dmabuf_v1.destroy` before
  removing dispatch state. When `zaura_shell` disappears, v38+ child
  `zaura_surface.release` requests are queued before `zaura_shell.release`,
  then registrations are removed; pre-v38 objects remain reserved because the
  protocol has no destructor at that version.
- Added regression tests:
  `global_remove_destroys_internal_dmabuf_factory` and
  `global_remove_releases_aura_children_before_shell_binding`.
- Focused tests pass:
  `cargo test -p sommelier handler::registry::tests::global_remove_destroys_internal_dmabuf_factory -- --exact`
  and
  `cargo test -p sommelier handler::registry::tests::global_remove_releases_aura_children_before_shell_binding -- --exact`.
- No commit or push performed; all pre-existing uncommitted work remains intact.
- Registry-focused suite passes (`23/23`), and linux-dmabuf-focused suite passes
  (`11/11`). A full `cargo test -p sommelier --all-targets` run is currently
  blocked by an arithmetic-overflow compile error in the concurrently edited
  `compositor.rs` test (`-i32::MIN / 10`); this is outside the registry/dmabuf
  patch and should be expressed as `i32::MIN / 10` or otherwise corrected by
  the damage-review owner.

## 2026-08-13 — compositor damage review

- Compared `map_buffer_damage()` with ChromiumOS
  `compute_buffer_scale_and_offset()` and `sl_transform_damage_coord()`.
  The scale direction, source/destination viewport rules, fixed-point source
  truncation, and filtering outset are numerically aligned for validated
  viewport state.
- Confirmed a remaining correctness gap: `wl_surface.attach` offsets (legacy
  versions) and `wl_surface.offset` (v5+) change where the buffer is placed in
  surface coordinates, but `copy_damage_to_ptr()` treats surface damage as
  buffer-local whenever scale/transform/viewport are otherwise identity. A
  partial damage can therefore copy the wrong source pixels. ChromiumOS also
  does not account for this offset in its copy helper, but the Wayland damage
  arguments are surface-local, so the Rust bridge should conservatively force a
  complete copy for non-zero committed offsets until an offset-aware copy map
  is implemented.
- Next: add a failing regression test using a two-pixel SHM buffer and a
  non-zero attach offset, then make the smallest fix by including
  `current_offset != (0, 0)` in the full-mapping decision. Continue with
  transform/overflow tests after this isolated fix.

## 2026-08-13 — compositor fixes verified

- Added `nonzero_surface_offset_without_explicit_damage_forces_full_host_damage`.
  It failed before the fix because an offset-only commit emitted no translated
  damage; it passes after `current_offset != (0, 0)` participates in
  `full_mapping`. This keeps the copy path conservative until it can map
  surface damage through offsets.
- Added `extreme_surface_damage_is_clamped_before_host_encoding`. It failed
  before the fix because `i32::MIN/MAX` were forwarded directly. The minimal
  fix clamps outset damage edges to ChromiumOS' `INT_MIN/10..INT_MAX/10`
  headroom before conversion and encoding. Both focused tests pass.
- `cargo fmt --all` and both focused tests pass. Full workspace verification
  remains the parent agent's responsibility after all shared edits settle.

## 2026-08-13 — transform edge review

- Found a non-square `wl_surface.set_buffer_transform` gap: for 90°/270°
  (including flipped variants), the surface extent swaps buffer width and
  height. The current conservative full damage and transformed
  `damage_buffer` fallback use the unswapped buffer extent, so a non-square
  frame can leave part of the host surface undamaged. Add a failing regression
  test for an 8×6 buffer with transform 90° before applying a small extent
  helper that swaps dimensions for transforms 1, 3, 5, and 7.

- Added `transformed_extent()` and made both full-damage and transformed
  `damage_buffer` fallback use swapped dimensions for 90°/270° transforms.
  `rotated_non_square_buffer_full_damage_swaps_extent` failed before the
  helper and passes after it. This only changes the damage extent; pixel
  rotation remains deliberately conservative/unimplemented.
- Full focused package test after these changes: `cargo test -p sommelier
  --all-targets` — 239 passed, 0 failed. The existing xkbcommon parser
  diagnostics are expected test-fixture output.

- Further edge review found transform+buffer-scale interaction: the newly
  swapped transform extent still used raw buffer pixels, while
  `wl_surface.set_buffer_scale` means host damage is in logical surface
  coordinates. A 90° 8×6 buffer at scale 2 must full-damage 3×4, not 6×8.
  Add a failing test before adjusting the extent helper; viewport destination
  dimensions remain surface-coordinate values and must not be divided again.

- Added `logical_transformed_extent()` and made transformed full-damage and
  transformed `damage_buffer` fallback divide the swapped extent by the
  committed buffer scale with ceil/min-one semantics. The new regression test
  failed before the fix and passes after it. Final focused package run:
  `cargo test -p sommelier --all-targets` — 241 passed, 0 failed.

## 2026-08-13 — pending offset lifecycle review

- Confirmed a double-buffering bug in `wl_surface.offset` handling:
  `on_commit()` consumes `pending_offset` when present but leaves the older
  `pending_attach_offset` behind. After a legacy `attach(x,y)` followed by
  `offset(new_x,new_y)`, the first commit selects the explicit offset, but a
  later damage-only commit re-applies the stale attach coordinates.
- Add a failing two-commit regression test before fixing this by consuming both
  pending offset slots on every commit, while preserving explicit
  `wl_surface.offset` precedence for that commit.

- Added `consumed_attach_offset_is_not_reapplied_on_later_commit`; it failed
  before the fix (`(8,9)` reverted to stale `(3,4)` on commit two). The commit
  path now takes both pending slots first, then applies explicit offset or the
  legacy attach offset. Focused and full package tests pass (242 total).

## 2026-08-13 — final compositor parity pass

- Rechecked source-only and destination-only viewport handling against
  ChromiumOS `compute_buffer_scale_and_offset()`: source-only uses the source
  rectangle as the logical extent without an extra source-size scale factor;
  destination-only scales from buffer contents to destination dimensions;
  source+destination applies both. Fractional source offsets intentionally use
  ChromiumOS `wl_fixed_to_int()` truncation.
- Rechecked negative and extreme damage. Invalid/empty rectangles are ignored;
  valid rectangles are outset and clamped to `INT_MIN/10..INT_MAX/10` before
  host encoding, matching ChromiumOS overflow headroom.
- Rechecked SHM copy bounds in `copy_shm_damage()`: source/destination spans
  are validated before any copy, including NV12's Y/UV planes; no additional
  compositor-side issue was confirmed.
- Full package tests now pass (242). Clippy is currently blocked by a
  concurrent unrelated borrow error in `state.rs:531` (`PoolState::drop`
  borrows `self.inner` mutably and immutably in one expression); parent agent
  should resolve that shared change before final verification.

## 2026-08-13 — parallel deep review started

- Three isolated review passes are running in this same worktree:
  - compositor/SHM damage and transform parity;
  - registry/linux-dmabuf lifecycle and FD/ID ownership;
  - keyboard/text-input/protocol-codegen IME behavior.
- No GUI process or primary Sommelier service will be restarted. Findings must
  be backed by tests before implementation changes.

## 2026-08-13 — IME/keyboard/protocol pass

- Compared `handler/keyboard.rs` and `handler/text_input.rs` with ChromiumOS
  `sommelier-seat.cc` and `sommelier-text-input.cc`.
- Confirmed existing behavior is aligned for `MAP_SHARED` keymap mapping,
  per-keyboard XKB/pressed-key state, host compositor time for synthetic
  events, v1 delete-surrounding conversion, and v3 nullable text events.
- Found a concrete protocol gap: text-input-v1 `keysym` accepts the Wayland v10
  `repeated` state in its XML, but the handler only accepted pressed/released
  and would drop valid compositor-driven repeats. Added handling that forwards a
  repeated keysym only when a corresponding guest physical press is live; it
  drops malformed repeats and IME-consumed repeats without inventing a key pair.
- Added two regression tests:
  `repeated_keysym_forwards_only_for_an_existing_guest_press` and
  `repeated_keysym_without_a_guest_press_is_dropped`.
- Also aligned `zcr_extended_keyboard_v1.peek_key` with the same Wayland v10
  repeated state: repeated peek updates physical-key state but does not cancel
  an already-cancelled IME fallback. Added
  `repeated_peek_key_keeps_the_physical_key_down_without_rearming_ime_cancel`.
- Focused repeat tests pass (`3 passed`). Full `cargo test -p sommelier --all-targets`
  currently has one unrelated shared-worktree failure in
  `handler::compositor::tests::extreme_surface_damage_is_clamped_before_host_encoding`
  (expected 214748364, got 429496728); do not attribute this to the IME change.
- No commit or push performed.

## 2026-08-13 — unknown-event FD ownership audit

- Audited every current protocol event carrying an FD. Host→client FD events
  (`wl_keyboard.keymap`, linux-dmabuf `format_table`) are emitted by tracked
  host objects and therefore are parsed by the generated dispatcher, consuming
  their descriptors. `wl_data_source.send` is a client→host request, not an
  untracked host event.
- An event from an unknown host sender is intentionally dropped before parsing
  to prevent stale events from mutating handler state. If such malformed input
  carries an FD, `handle_msgs` sees the complete packet with an unconsumed
  descriptor and terminates that connection; `WaylandConnection::Drop` closes
  the buffered descriptor. No leak was found.
- Changing unknown-event drop to a generic dispatcher error would make delayed
  events from retired objects connection-fatal and diverge from the existing
  stale-event safety policy. No code/test change made for this audit.

## 2026-08-14 — invalid registry bind protocol error

- Confirmed against ChromiumOS `sommelier-display.cc`: malformed
  `wl_registry.bind` requests (unknown global, interface mismatch, zero or
  too-new version) must post `wl_display.error` with
  `WL_DISPLAY_ERROR_INVALID_OBJECT` and flush before session teardown.
- Rust previously returned `Action::Drop`, silently keeping a malformed guest
  connection alive.
- Added `Context::fatal_protocol_error`, a fallible `queue_protocol_error`
  helper, and registry bind validation that queues a guest
  `wl_display.error` (code 0) then causes `handle_msgs` to flush the queued
  diagnostic and terminate the session.
- Added regression test
  `handler::registry::tests::invalid_bind_queues_a_display_protocol_error`.
  Focused test passes after reproducing the pre-fix failure.
- Added end-to-end proxy regression
  `proxy::tests::invalid_registry_bind_flushes_display_error_before_disconnect`;
  it drives a malformed bind through `Client::handle_msgs`, reads the guest
  socket, and verifies the `wl_display.error` header was flushed before the
  fatal return.
- No commit or push performed.

## 2026-08-13 — continuation stream-boundary audit

- Read the local review log before continuing after context compaction.
- Re-ran `git diff --check` and the complete serial workspace matrix:
  `cargo test --workspace --all-targets -- --test-threads=1`.
- Current passing totals remain Sommelier 277, GUI test app 11, and Wayland
  codegen 5.
- Began a focused audit of `proxy.rs`, `connection.rs`, and `wire.rs` for
  multiple messages per receive, ancillary-FD ordering, malformed headers,
  string/array padding, fixed values, and output-queue failure cleanup.
- No new production defect has been confirmed yet. Continue only with a
  failing regression test for any candidate before changing behavior.

## 2026-08-13 — seat teardown parity audit

- Compared Rust `SeatHandler::on_release`
  (`sommelier/src/handler/seat.rs:36-78`) with the ChromiumOS seat/keyboard
  focus teardown in `sommelier-seat.cc` and the Rust keyboard leave path
  (`sommelier/src/handler/keyboard.rs:889-915`).
- Confirmed bug: seat release clears every matching text input's
  `active_surface` and invalidates its IME state, but does not queue the
  corresponding guest `zwp_text_input_v3.leave` event (opcode 1). The
  keyboard focus-leave path already queues this event, so the two teardown
  paths are asymmetric.
- Reproduction: create a seat with keyboard focus and an active
  `zwp_text_input_v3`; release `wl_seat` before the delayed host keyboard
  `leave` arrives. The Rust proxy removes the surface mapping first, so the
  delayed keyboard event cannot reliably repair the guest state. The guest
  text-input client can retain stale focus/transaction state and become
  inconsistent on the next `enter`.
- Minimal fix: for each matching text input, capture the existing
  `active_surface`; if it is `Some(surface)`, queue exactly one v3 `leave`
  before setting the field to `None`, invalidating focus state, and updating
  host activation. Do not emit a leave when there was no active surface.
  Add a regression that exercises seat-release-only teardown followed by a
  delayed keyboard leave and asserts one leave total.
- Additional lifecycle candidate (not confirmed): `SeatHandler::on_release`
  removes `keyboard_to_seat` and per-keyboard IME/routing state but does not
  remove `keyboard_to_extended_keyboard` /
  `extended_keyboard_to_keyboard` or retire the host-side extended-keyboard
  dispatch metadata. A delayed `peek_key` or key event could therefore resolve
  a stale child and send an ack or mutate state after seat teardown. Verify
  whether the protocol permits this release ordering before changing it; the
  normal `wl_keyboard.release` path already performs child-map cleanup.
- No new confirmed defect in this pass for SHM global retirement, dmabuf
  feedback/FD ownership, repeated keyboard state handling, or text-input-v3
  destruction. The classic virtgpu stride/modifier ioctl fixup remains a
  previously known parity gap.
- This audit only records findings; no Sommelier source code, tests, commit, or
  push was performed. Existing uncommitted worktree changes remain untouched.

## 2026-08-13 — synthetic wl_shm release lifecycle audit

- An initial cache-lifecycle suspicion was ruled out. The synthetic `wl_shm`
  global is deliberately advertised at version 1, while `wl_shm.release` is a
  version-2 request. Generated version validation therefore rejects that
  request before `WlShmHandler` runs; a legal guest cannot reach the suspected
  missing cleanup hook.
- No code or regression test was retained for this false positive.
- No primary Sommelier, Orca, or GUI process was restarted.

## 2026-08-13 — aura-shell manager/child global-remove parity follow-up

- Rechecked `zaura_shell` against `tmp/sommelier.cc` and
  `third_party/protocols/aura-shell.xml`.
- ChromiumOS `sl_registry_remover()` destroys only its internally bound
  `zaura_shell` manager/global wrapper. It does not destroy the
  `zaura_surface` children held by live windows; those children are destroyed
  from the window/surface lifecycle. The protocol also states that releasing
  `zaura_shell` while `zaura_surface` children remain alive is illegal.
- Rust `reset_internal_binding_for_global()` currently queues
  `zaura_surface.release` for every mapped surface and clears
  `wl_surface_to_zaura_surface` before releasing the shell. This is more
  aggressive than the reference and can stop app-ID/shelf updates for live
  surfaces after a transient global removal/re-advertisement. It is a
  remaining parity/design issue, not patched in this pass per parent request.
- Keyboard-extension, text-input manager/child, text-input extension/child,
  and linux-dmabuf factory/object global-removal lifetimes were reviewed;
  no additional confirmed bug was found.

## 2026-08-13 — synthetic guest destructor audit

- Confirmed a separate lifecycle gap for proxy-local objects: synthetic
  `zwp_text_input_manager_v3` binds and local `wl_shm_pool` objects have no
  host mapping, so generated destructor cleanup previously returned early and
  their handlers removed the object silently without emitting the guest
  `wl_display.delete_id`. `wl_shm` itself has no destroy request in the
  protocol and remains connection-resident.
- A local destructor must retire its guest interface and queue exactly one
  synthetic `wl_display.delete_id`; otherwise a client can retain a dead
  object ID indefinitely and later reuse/teardown behavior diverges from
  Wayland.
- Host-only temporary `wl_shm_pool` objects remain a separate path: they have a
  real host destructor and are covered by pending host-ID reservation until
  host `delete_id`.
- Next implementation: add a shared local-delete helper, make synthetic SHM,
  SHM-pool, and text-input-manager-v3 destroy paths use it, and add
  dispatch-level tests that reject a second request after local retirement.

## 2026-08-13 — destructor and seat lifecycle hardening

- Fixed generated destructor lifecycle: a forwarded destructor now marks the
  guest object as pending destruction instead of immediately removing its
  guest↔host mapping. The mapping remains resolvable until the host emits the
  matching `wl_display.delete_id`; requests from the pending object are
  rejected and stale host events are suppressed.
- Added explicit pending-destroy handling to `Client::handle_msgs`, including
  the `wl_buffer.release` exception needed for a retired SHM buffer whose host
  compositor-use interval is still ending.
- Updated custom `wl_surface.destroy` and SHM `wl_buffer.destroy`/release paths
  to preserve ordering and mappings through host acknowledgement. Retired SHM
  backing storage is released only after the compositor's `wl_buffer.release`
  event, while the numeric object mapping remains until the subsequent
  `delete_id`.
- Added `wl_seat.release` cleanup for seat-scoped focus, keyboard routing,
  pressed-key, repeat-cancellation, and IME state so delayed input cannot
  reactivate a released seat.
- Added regression coverage for destructor mappings, stale host-event
  filtering, deferred SHM buffer release, and seat cleanup.
- Final Rust verification for this review pass:
  - `cargo check --workspace --all-targets` passed.
  - `cargo clippy --workspace --all-targets -- -D warnings` passed.
  - `cargo test --workspace --all-targets -- --test-threads=1` passed:
    Sommelier 255, GUI 11, codegen 5.
  - `cargo fmt --all` completed; rerun the check form before handoff.
  - `git diff --check` completed; rerun before handoff.
- `bun run verify` remains unrelatedly blocked by existing monorepo issues
  (nested Biome configuration, missing Slidev path, and pre-existing
  hygiene/shebang/absolute-path checks); no Sommelier failure was reported by
  that command.

## 2026-08-13 — focused-surface text-input teardown

- Confirmed a compositor/IME lifecycle bug: destroying a focused `wl_surface`
  cleared `TextInputState.active_surface` and host activation, but did not
  emit the required guest `zwp_text_input_v3.leave` event.
- A delayed host `wl_keyboard.leave` cannot repair this because the surface
  mapping has already been removed and the stale-leave guard intentionally
  returns without emitting another text-input event.
- Added explicit v3 `leave` emission during surface destruction, before the
  guest surface ID is removed. The later keyboard leave remains idempotent.
- Added regression test
  `wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave`.
- Updated `sommelier-rs-standards.md` with the explicit focus-teardown
  invariant. Focused regression test passes; full verification is still
  required after this shared edit.

## 2026-08-13 — review continuation

- Loaded the repository rules relevant to this Rust/protocol review:
  `sommelier-rs-standards.md`, `rust-standards.md`,
  `testing-and-assertions-standards.md`, and `memory-protocol-standards.md`.
- The offset full-mapping fix is present and its regression test passes.
- Registry teardown changes now queue explicit `zwp_linux_dmabuf_v1.destroy`
  and `zaura_surface.release`/`zaura_shell.release` messages before removing
  hidden host registrations; this remains under focused lifecycle testing.
- No commit, push, or runtime service restart has been performed.

## 2026-08-13 — review verification complete

- Added `retired_host_ids` reservations for internal host proxies whose
  protocols have no destructor (`wl_shm`, text-input v1, keyboard extension,
  and legacy aura objects). Their dispatch metadata is removed on global
  removal, but the allocator cannot recycle the IDs during the connection.
- Added `WL_KEY_REPEATED` handling for compositor-driven keysym repeats,
  restricted to an existing physical guest press.
- Added non-square rotated damage extents and ChromiumOS-compatible damage
  headroom clamping.
- Verification passed:
  - `cargo test -p sommelier --all-targets` — 239 passed
  - `cargo check --workspace --all-targets`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
  - `git diff --check`
- No commit, push, runtime restart, or GUI disruption was performed.

- `bun run verify` was also attempted. It reaches the repository-wide Biome,
  Typst, subproject, and standards checks, but fails on existing monorepo
  hygiene/configuration issues outside this worktree's Sommelier changes
  (nested Biome roots, missing unrelated subproject paths, and pre-existing
  executable/shebang/absolute-path violations). No generated artifacts were
  retained.

## 2026-08-13 — final review continuation

- Added and verified the legacy attach-offset lifecycle regression:
  `consumed_attach_offset_is_not_reapplied_on_later_commit`.
- Rechecked ChromiumOS viewport source-only, destination-only, and combined
  source/destination mappings, flipped/non-square transforms, logical buffer
  scale, damage clamping, and NV12 copy bounds.
- Audited generated unknown-event handling with FD ownership. Current host
  event FD-bearing interfaces are tracked; stale unknown events are dropped,
  and a complete packet carrying an unconsumed FD terminates the connection so
  the connection drop closes the descriptor. No code change was justified.
- Fixed `PoolState::drop` cleanup for poisoned `RwLock`s using `get_mut()` and
  verified the poison-lock regression.
- Final Rust verification:
  - `cargo test --workspace --all-targets`: 243 Sommelier + 11 GUI +
    5 codegen tests passed
  - `cargo check --workspace --all-targets`: passed
  - `cargo clippy --workspace --all-targets -- -D warnings`: passed
  - `cargo fmt --all -- --check`: passed
  - `git diff --check`: passed
- Work remains uncommitted and unpushed by design.

## 2026-08-13 — registry/dmabuf second audit

- Recompared the current registry, linux-dmabuf, proxy, state, and codegen
  changes against baseline and ChromiumOS `sl_registry_remover()` /
  `sommelier-linux-dmabuf.cc`.
- Confirmed version ceilings match ChromiumOS: dmabuf guest v4/internal v2,
  xdg-shell v3, compositor v4, seat v5, output v3, and aura shell bind v6–38.
  No removable duplicate was identified in the lifecycle guards; each added
  state field has an allocator or dispatch invariant covered by tests.
- Found a concrete ID-reuse hole for globals whose bound host object has no
  destructor (`wl_shm`, text-input managers/extensions, keyboard extension,
  and legacy aura v<38). The prior reset removed dispatch metadata and made
  the allocator eligible to recycle IDs while host proxies could still exist.
  `retire_host_interface()` now removes active dispatch metadata while keeping
  IDs reserved until connection teardown; `is_host_id_available()` also rejects
  retired IDs before mapping host-generated `new_id` events.
- Added regression tests for retired host-generated IDs, SHM global removal,
  and legacy aura shell/surface removal. Focused registry suite now passes
  `25/25`; state suite passes `20/20`.
- Rechecked async dmabuf FD paths and found no further confirmed bug:
  duplicated plane FDs are consumed exactly once, invalid/stale params close
  owned FDs, feedback format-table FDs are duplicated for queue ownership,
  and generated `created` IDs use the reserved guest server range.
- No commit, push, runtime restart, or GUI disruption performed.

## 2026-08-13 — deep parallel review and keymap mapping correction

- Deep compositor review compared damage, viewport, transform, SHM pool
  lifetime, and NV12 copying against ChromiumOS. No additional confirmed bug
  was found; the focused compositor/SHM tests remain passing.
- Deep registry/dmabuf review found no additional confirmed lifecycle or FD
  ownership bug. Duplicate internal singleton globals are treated as a
  host-invariant condition matching ChromiumOS assertions, so no speculative
  rebinding was added.
- Deep input review found one concrete regression in this branch: the
  `wl_keyboard.keymap` mmap had been changed to `MAP_SHARED`. Wayland requires
  a read-only `MAP_PRIVATE` mapping for keymap FDs; ChromiumOS's implementation
  choice is not sufficient reason to violate the protocol. Restored
  `MAP_PRIVATE` and added `keymap_mmap_is_private_not_shared`, which passes.
- Re-ran the full workspace tests after the correction:
  - Sommelier: 246 passed
  - `sommelier_test_gui`: 11 passed
  - `wayland_codegen`: 5 passed
- No commit, push, runtime restart, or GUI disruption performed.

## 2026-08-13 — wire/codegen and transport audit

- Audited `wayland_codegen` generation and `sommelier` wire/proxy/connection
  against Wayland wire-format rules and ChromiumOS forwarding behavior.
- Confirmed generated request/event dispatch validates object versions, maps
  typed and generic `new_id` values, rejects unknown opcodes, checks complete
  payload consumption, and keeps incoming/output FD ownership balanced.
- Confirmed Unix `recvmsg` handles SCM_RIGHTS truncation, VirtWL validates its
  packed FD array, and proxy cleanup protects descriptors still owned by the
  connection.
- Found one concrete wire invariant gap: `MessageBuilder::try_build_message`
  accepted a payload producing a non-4-byte-aligned total length, while the
  Wayland header length must be 4-byte aligned. Added
  `try_builder_rejects_unaligned_wire_message_length` and made the builder
  return `ProtocolError::InvalidMessageLength` for that case.
- Focused regression test passes. No additional confirmed codegen/proxy/FD
  correctness bug was found in this pass; no commit, push, or process restart
  performed.

## 2026-08-13 — compositor/SHM audit continuation

- Re-read ChromiumOS `sommelier-compositor.cc`, `sommelier-shm.cc`,
  `sommelier-formats.cc`, `sommelier-mmap.cc`, `sommelier-transform.cc`, and
  `sommelier-viewporter.cc` against the Rust compositor/SHM paths.
- Initially suspected damage-only commits skipped SHM copying because the
  local variable is named `pending_buffer_id`; verified the tuple stores the
  *current* buffer after applying pending attach state, so the existing
  `if let Some(buffer_id)` copy block correctly runs for damage-only commits.
  This was a false positive; no change was made.
- Rechecked viewport source/destination mapping, damage outset/clamping,
  transform extents, pool resize semantics, NV12 plane bounds, and buffer
  lifetime. No additional confirmed bug or justified simplification found.
- Focused compositor tests: 29 passed. `git diff --check` passed.

## 2026-08-13 — allocator driver-selection parity

- ChromiumOS `open_virtgpu()` enumerates `/dev/dri/renderD*`, queries each
  node's DRM driver, and accepts only `virtio_gpu`; choosing the first usable
  GBM node can select a software or secondary GPU on multi-node systems.
- Updated `Allocator::new()` to perform the same driver-name filter during
  automatic discovery. An explicit `SOMMELIER_DRM_DEVICE` remains an
  unrestricted override, matching ChromiumOS `--force-drm-device`.
- Added `automatic_probe_accepts_only_chromiumos_virtio_gpu_driver`.
  Allocator-focused tests pass (4/4); no commit, push, or runtime restart.
## 2026-08-13 — unknown host FD stream audit
- Wire audit identified a real sequencing hazard: if a complete dropped/unknown host event carries an SCM_RIGHTS FD and the next Wayland message is partial, leaving `fd_offset` unchanged can associate the stale FD with the later message. Investigating parser-level ownership so complete dropped messages consume/close their own descriptors without stealing FDs belonging to a partial message.

## 2026-08-13 — unknown host FD safety decision
- A dropped message has no interface metadata, so the proxy cannot determine how
  many descriptors belong to it versus a following partial message. The safe
  minimal policy is to tear down the connection as soon as a complete
  untracked message is encountered while any received descriptor remains
  pending. `WaylandConnection::Drop` then closes every pending descriptor,
  preventing stale FD association on the next receive. Add a focused regression
  for this decision before changing `proxy.rs`.

- Implemented `untracked_message_with_pending_fds_is_fatal` in `proxy.rs` and
  added `proxy::tests::untracked_complete_message_with_fd_aborts_before_partial_followup`.
  The proxy now closes the client when an untracked complete message is
  encountered with any pending received FD; it never carries that ambiguous FD
  into a subsequent receive. Focused proxy tests (10) and the full Sommelier
  package suite (251) pass. The complete serial workspace run also passes:
  251 Sommelier tests, 11 GUI tests, and 5 codegen tests. Workspace check,
  clippy (`-D warnings`), formatting, and `git diff --check` pass. No commit,
  push, or runtime restart performed.
- Added an async regression that exercises `Client::handle_msgs` with a
  complete stale message, a four-byte partial follow-up, and a real pending
  descriptor; it confirms the descriptor is closed when the connection is
  dropped. Focused test passes after `cargo fmt --all`. The final workspace
  run passes 252 Sommelier tests, 11 GUI tests, and 5 codegen tests; workspace
  check, clippy (`-D warnings`), formatting, and `git diff --check` all pass.

## 2026-08-13 — final verification

- Re-ran the complete Rust matrix inside `nix develop` from the monorepo:
  `cargo test --workspace --all-targets -- --test-threads=1` passed 252
  Sommelier tests, 11 GUI tests, and 5 codegen tests.
- Also passed inside `nix develop`: workspace `cargo check`, workspace
  `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check`.
  `git diff --check` passed.
- Ran repository-wide `nix develop -c bun run verify`. It still fails on
  unrelated pre-existing monorepo conditions: nested Biome roots, a missing
  Slidev directory, and existing root-hygiene/shebang/absolute-path findings.
  No Sommelier files or runtime processes were changed by that check.
- No commit, push, primary Sommelier restart, or GUI disruption was performed.

## 2026-08-13 — `wl_display.delete_id` wire audit

- Inspected the generated `wl_display::dispatch_event` arm and
  `DisplayHandler::on_delete_id`.
- The XML argument is intentionally a raw `uint`, but this event is handled
  specially: the handler treats it as a host-side object ID, translates it
  through `ShadowTable::get_guest_id`, removes the paired mapping, and queues a
  guest-side `delete_id` carrying the translated ID. It returns `Action::Drop`,
  so generated raw-uint forwarding is bypassed.
- An unmapped host ID is dropped rather than forwarded as zero. The callback
  path likewise emits the already translated guest callback ID and removes its
  mapping.
- No `delete_id` code change or regression test was justified by this audit.

## 2026-08-13 — synthetic text-input delete reservation follow-up

- The generated local-destructor path already queues a guest
  `wl_display.delete_id` for synthetic `wl_shm`, `wl_shm_pool`, and
  `zwp_text_input_manager_v3` objects.
- `zwp_text_input_v3` is different: its guest object is paired with a
  host `zwp_text_input_v1` object whose protocol has no destructor. Its
  destroy handler must emit the guest `delete_id` while retaining the host
  v1 interface reservation until connection teardown.
- The existing handler queued the local delete through a helper that called
  `remove_id`, then attempted `remove_guest_mapping`; this removed the host
  reservation before the latter could preserve it. A dedicated cleanup path
  now queues the event and removes only the guest half, with a regression
  assertion that the host v1 interface remains registered.

## 2026-08-13 — legacy aura child retirement

- A legacy `zaura_surface` host object (aura-shell version <38) has no
  `release` destructor. The surface-destroy path previously kept its dispatch
  metadata live, so delayed host events could still enter a child whose guest
  `wl_surface` was gone.
- The path now calls `retire_host_interface`: metadata is removed immediately,
  while the numeric host ID remains permanently reserved. The compositor
  regression now checks both stale-event suppression metadata and allocator
  reservation.

## 2026-08-13 — keyboard-extension global lifecycle review

- ChromiumOS `sl_registry_remover()` destroys only the internally bound
  `zcr_keyboard_extension_v1` manager when its global disappears. Existing
  `zcr_extended_keyboard_v1` children are owned by each `wl_keyboard` and
  remain valid until that keyboard is released.
- The Rust registry reset incorrectly retired child dispatch metadata and
  cleared both child maps during manager `global_remove`. This could drop
  queued `peek_key` events and stop `ack_key` routing for a still-live
  keyboard.
- Fixed the reset to retire only the manager. Existing child mappings and
  `zcr_extended_keyboard_v1` metadata remain until `wl_keyboard.release`.
  `send_ack_key` now keys solely off the child map, so it continues working
  after the manager field is cleared.
- Added regressions:
  `keyboard_extension_global_remove_keeps_existing_children_alive` and
  `send_ack_key_keeps_existing_child_alive_after_manager_global_remove`.
- Full workspace verification now passes: 260 Sommelier tests, 11 GUI tests,
  and 5 codegen tests; workspace check, strict clippy, formatting, and
  `git diff --check` also pass. No commit, push, primary Sommelier restart,
  or GUI disruption was performed.

## 2026-08-13 — aura-shell child lifecycle parity

- ChromiumOS `sl_registry_remover()` destroys the bound `zaura_shell`
  manager/global but does not destroy `sl_window::aura_surface` children.
  Wayland global removal invalidates the name, not already-bound child objects.
- Rust was more aggressive: it released/retired every mapped
  `zaura_surface` and cleared the surface map during `zaura_shell` removal.
  That could stop shelf/application-ID updates for existing windows.
- Fixed the reset to retire/release only the shell manager. Existing aura
  children retain their map and negotiated object version until their owning
  `wl_surface.destroy`; new children are created only after a replacement shell
  global is bound.
- Surface cleanup now consults each child object's negotiated version rather
  than the current shell manager version, so a child from an older generation
  still receives `release` only when its own protocol supports it.
- Regression `global_remove_keeps_aura_children_until_surface_destroy` now
  covers the manager/child split; the existing surface-destroy release test
  remains passing. Full workspace verification passes: 260 Sommelier tests,
  11 GUI tests, and 5 codegen tests, plus workspace check, strict clippy,
  formatting, and `git diff --check`.

## 2026-08-13 — focused wire, codegen, allocator, and shadow-table audit

- Rechecked `wayland_codegen/src/generator.rs` and `lib.rs` for request/event
  version guards, typed object validation, server-generated ID translation,
  destructor mapping retention, and host sender filtering. No new
  reproducible codegen defect was found.
- Rechecked every current protocol XML message containing `new_id`: none also
  carries a variable-length string or array, so generated ID reservation cannot
  currently be left behind by a later oversized payload failure. Existing
  `try_build_message` propagation remains sufficient for current protocols.
- Rechecked `WireMessage` scalar/string/array bounds, padding, UTF-8 and
  terminator validation, FD cursor handling, trailing payload rejection,
  message-size/alignment checks, Unix SCM_RIGHTS cleanup, VirtWL FD packing,
  and proxy queue ownership. No additional confirmed wire or FD-lifecycle
  bug was found.
- Rechecked allocator render-node discovery against ChromiumOS
  `open_virtgpu()`, explicit DRM-device override behavior, GBM format
  conversion, dimensions/stride validation at SHM call sites, and BO/PRIME
  FD lifetime. No additional confirmed allocator defect was found.
- Rechecked all production `map_id`, `track_*`, `remove_*`, retirement, and
  host-generated allocation call sites. Pending destructor IDs, retired
  host-only IDs, synthetic objects, delayed `wl_buffer.release`, and
  connection-drop FD cleanup have corresponding guards/tests; no new
  reproducible state/lifecycle defect was found.
- No source edits, commit, push, service restart, or GUI disruption were made
  during this focused pass.

## 2026-08-13 — final review handoff

- Re-read the applicable repository rules and inspected the complete 19-file
  Rust/codegen diff. No temporary debug code, generated build output, or
  unrelated root-monorepo files were added to this worktree.
- Re-ran the serial workspace matrix:
  `cargo test --workspace --all-targets -- --test-threads=1` passed 260
  Sommelier tests, 11 GUI tests, and 5 codegen tests.
- Re-ran `cargo check --workspace --all-targets`, strict
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check`; all passed.
- Re-ran repository-wide `bun run verify`. It remains blocked only by
  pre-existing monorepo hygiene/configuration failures (nested Biome roots,
  the missing `grant_applications/gwanak_s_valley_startup_2026` Slidev path,
  and existing root hygiene/shebang/absolute-path findings). The Sommelier
  worktree introduced none of those files.
- The parallel compositor, Rust/codegen, and ChromiumOS parity audits found no
  additional confirmed bug that could be fixed safely without a new focused
  regression test. The known virtgpu DRM plane fixup difference requires
  dedicated ioctl support and remains intentionally unmodified.
- No commit, push, worktree removal, primary Sommelier restart, Orca restart,
  or unrelated GUI disruption was performed. The branch is ready for the
  user's explicit `/commit` workflow.

## 2026-08-13 — seat-release parity audit reopened

- ChromiumOS/Rust parity review found a concrete focus-lifecycle gap:
  `SeatHandler::on_release` clears seat text-input state without first
  queuing the matching guest `zwp_text_input_v3.leave` event. If the guest
  releases the seat before a delayed host keyboard leave arrives, the guest
  can retain stale IME focus indefinitely and the delayed leave is correctly
  rejected as stale.
- A regression test must reproduce seat release with an active v3 text input,
  assert exactly one leave before state removal, and then feed the delayed
  keyboard leave to assert no duplicate event. Only after the failing test is
  present should the minimal seat-release fix be applied.
- The same audit identified possible extended-keyboard child cleanup on seat
  release, but that remains a candidate until protocol ordering and existing
  child lifetime tests establish a reproducible stale-dispatch failure.

## 2026-08-13 — clipboard transfer lifetime audit

- ChromiumOS owns each clipboard transfer's read/write event source from the
  compositor context and destroys both sources and descriptors on hangup or
  transfer completion.
- Rust's VirtWL `wl_data_offer.receive` path previously detached a Tokio pump
  task. If the client disconnected or sending the duplicated descriptor to the
  host failed, the task could retain the VirtWL read end and guest output FD
  indefinitely, with no `Context`/`Client` teardown path able to cancel it.
- The fix must track the task handle in `Context`, abort tracked pumps during
  context teardown, and retain streaming/error behavior. Add a focused
  cancellation regression before changing the production path.

## 2026-08-13 — executor safety audit

- `KeyboardHandler` contains xkbcommon state that is not `Send`, but the
  public `proxy::run` path used `tokio::spawn`, forcing an `unsafe impl Send`.
  The current-thread runtime in `main` is only a caller convention; a future
  multi-threaded caller could migrate a `Client` and violate the safety
  argument.
- Replace the detached client spawn with Tokio `LocalSet`/`spawn_local` in
  `proxy::run`, then remove the unsafe `Send` implementation and its
  executor-dependent comments. This keeps all client handlers on the owning
  thread and makes the invariant type-enforced. Verify with workspace
  compilation/tests and strict Clippy.

## 2026-08-13 — executor and clipboard fixes verified

- Implemented the executor fix in `proxy::run`: client tasks now use a
  `LocalSet` and `spawn_local`; the unsafe `KeyboardHandler: Send`
  implementation and its current-thread-only safety comments were removed.
  This is reachable through the public async `proxy::run` API and remains safe
  even if a caller uses a multi-thread Tokio runtime.
- The concurrent clipboard-lifetime fix now stores VirtWL pump
  `JoinHandle`s in `Context`, reaps completed handles, and awaits aborts when a
  `Client` disconnects. `Context::Drop` also aborts handles on synchronous
  protocol-error paths. This matches ChromiumOS' event-source ownership and
  prevents detached transfers from surviving the client connection.
- Verification after the combined changes:
  `cargo check --workspace --all-targets` passed (only a transient dead-code
  warning before all call sites settled), and
  `cargo test --workspace --all-targets -- --test-threads=1` passed 262
  Sommelier tests, 11 GUI tests, and 5 codegen tests. No service or GUI process
  was restarted.

## 2026-08-13 — damage-only mapped-pixel regression

- Added `handler::compositor::tests::damage_only_commit_copies_mutated_committed_shm_buffer`.
  The test uses real anonymous source/destination mappings, commits a buffer
  containing `0x11`, mutates the already committed source to `0x22`, then
  submits only surface damage plus `wl_surface.commit` and asserts that the
  destination changes to `0x22`.
- Focused command passed:
  `cargo test -p sommelier handler::compositor::tests::damage_only_commit_copies_mutated_committed_shm_buffer -- --exact --nocapture`.
- This confirms the existing `on_commit()` behavior copies from
  `current_buffer_id` even when no new attach is present; no production fix was
  needed for the SHM damage-only invariant.

## 2026-08-13 — wl_shm global-removal audit

- ChromiumOS destroys the internal `wl_shm` capability binding when its host
  global disappears. Wayland core specifies that a previously bound global
  object remains valid for teardown, while requests sent after removal are
  ignored.
- Rust previously left synthetic guest `wl_shm` objects fully requestable after
  `host_shm_id` was retired. `create_pool` could mmap and retain a guest FD
  even though `create_buffer` could no longer reach a host `wl_shm`; this is
  unnecessary resource work and violates the post-removal request behavior.
- The repository rule was updated before code changes. A focused regression
  will prove the stale synthetic object does not create a pool and still emits
  exactly one local `delete_id` on destruction.

- A second stale-generation hazard was confirmed: replacement host capability
  events were still broadcast to every `shm_guest_formats` entry, including
  synthetic `wl_shm` objects bound before the removed generation. ChromiumOS
  sends format events only to resources bound through the current internal
  `sl_shm`; old resources do not receive replacement capabilities. The
  capability fan-out must therefore skip stale synthetic guest IDs.

- Existing synthetic `wl_shm_pool` children also need a stale-generation
  guard. Without one, a pool created before removal could still resize its
  local mapping or create a buffer through the replacement singleton
  `host_shm_id`, aliasing an old child to a new host binding. The child remains
  destroyable, but non-destructor requests must be ignored after its parent
  host capability generation is retired.

- Implemented the minimal generation guard:
  `Context::stale_shm_guest_objects` and `stale_shm_pools` are marked when the
  internal host SHM binding is reset; stale capability fan-out is skipped;
  `wl_shm.create_pool`, `wl_shm_pool.create_buffer`, and
  `wl_shm_pool.resize` are ignored for stale objects, while pool destruction
  still removes local state and lets generated `delete_id` handling run.
- Focused SHM verification passed: all 21 `handler::shm` tests, including
  regressions for stale global objects, replacement capability routing, and
  pre-removal pool teardown.

## 2026-08-13 — post-change workspace verification

- `cargo test --workspace --all-targets -- --test-threads=1` passed 269
  Sommelier tests, 11 GUI tests, and 5 codegen tests.
- `cargo check --workspace --all-targets`, strict workspace Clippy
  (`-D warnings`), `cargo fmt --all -- --check`, and `git diff --check` all
  passed after the stale SHM generation changes.
- No commit, push, service restart, or GUI process disruption was performed.

## 2026-08-13 — final ChromiumOS parity and lifecycle audit

- Compared the current Rust paths with the local ChromiumOS
  `vm_tools/sommelier` implementations for SHM pools/buffers and capability
  events, registry global removal/re-advertisement, seat and keyboard child
  lifetimes, data-device transfer ownership, VirtWL framing, and generated
  Wayland object ID translation.
- The Rust implementation now preserves the relevant ChromiumOS lifetimes
  while adding protocol-safe guards that the C implementation does not need:
  stale synthetic `wl_shm` generations cannot service requests through a
  replacement binding; pending destructor IDs cannot be reused before
  `wl_display.delete_id`; and detached VirtWL clipboard pumps are cancelled
  with the client connection.
- Rechecked the complete shared diff and searched all production allocation,
  mapping, destructor, host-event, and FD ownership paths. No additional
  reproducible bug was found that could be fixed safely without a focused
  failing regression test.
- Final serial verification passed:
  `cargo test --workspace --all-targets -- --test-threads=1` (269 Sommelier,
  11 GUI, 5 codegen tests), `cargo check --workspace --all-targets`, strict
  Clippy, formatting, and `git diff --check`.
- `bun run verify` remains blocked by unrelated pre-existing monorepo
  configuration/hygiene failures recorded above. No primary Sommelier service,
  Orca process, or unrelated GUI application was restarted or disrupted.

## 2026-08-13 — final verification rerun

- Re-ran `cargo test --workspace --all-targets -- --test-threads=1`:
  Sommelier 269, GUI 11, and codegen 5 tests passed.
- Re-ran `cargo check --workspace --all-targets`, strict workspace Clippy
  (`cargo clippy --workspace --all-targets -- -D warnings`),
  `cargo fmt --all -- --check`, and `git diff --check`; all passed.
- Re-ran repository-wide `bun run verify`. It still fails outside this
  worktree's Sommelier changes: nested Biome roots, the missing
  `grant_applications/gwanak_s_valley_startup_2026` Slidev directory, root
  hygiene/prohibited-file/absolute-path findings, and existing shebang or
  executable-bit findings. The command reported 2473 passing and 10 failing
  standards assertions.
- No production binary, Sommelier service, Orca process, or unrelated GUI
  application was started or restarted. No commit or push was performed.

## 2026-08-13 — compositor/codegen deep audit conclusion

- Compared the guest/host ID allocator and `ShadowTable` lifecycle, generated
  `new_id`/destructor/`delete_id` paths, object/interface/version validation,
  registry global generations, hidden singleton bindings, stale host-event
  filtering, and wire string/FD handling with ChromiumOS Sommelier.
- No additional reproducible production defect was found in these paths.
- Rechecked the proposed synthetic `wl_shm.release` concern: `release` is a
  protocol-v2 request, while the synthetic guest `wl_shm` is intentionally
  advertised as v1. A guest cannot legally send that request, and retaining
  the synthetic capability for the connection lifetime is consistent with
  its no-destructor lifecycle. Synthetic `wl_shm_pool` destruction remains a
  separate local `delete_id` path, including for stale generations.
- Existing changes preserve ChromiumOS semantics: global removal does not
  invalidate bound child objects, pending destructor IDs remain reserved until
  host `delete_id`, host-only destructor IDs remain reserved while dispatch
  metadata is retired, host-generated IDs are translated into the guest
  server-ID range, and stale host events are rejected before dispatch.
- No source changes, commit, push, binary launch, or service/GUI restart was
  performed by this audit.

## 2026-08-13 — deep input, clipboard, and VirtWL audit handoff

- Re-audited `handler/keyboard.rs`, `handler/text_input.rs`,
  `handler/seat.rs`, `handler/data_device.rs`, `virtwl_channel.rs`,
  `connection.rs`, and the generated Wayland dispatch code against the local
  ChromiumOS `vm_tools/sommelier` implementation.
- Confirmed the current input fixes and their coverage: IME/repeat state is
  scoped per keyboard/seat, synthetic key events use the compositor-relative
  monotonic millisecond domain, keymap FDs use read-only `MAP_PRIVATE`, seat
  teardown emits one v3 text-input leave before clearing focus, delayed
  keyboard leave is idempotent, and clipboard VirtWL pumps are tracked and
  cancelled with client teardown.
- Confirmed existing regression coverage for repeated keys, stale focus,
  v1-to-v3 text conversion, clipboard FD duplication/cancellation, VirtWL
  payload sizing, and FD validation. No additional reproducible input,
  text-input, clipboard, or VirtWL bug was established; no source fix was
  justified in this pass.
- Remaining review candidates are non-confirmed: an ioctl failure theoretically
  writing `txn.fds` before returning an error (Linux should not populate them),
  synchronous `Context::Drop` aborts which cannot await (normal async teardown
  awaits them), and generated leave events for already-destroyed surfaces
  being dropped by mapping validation (explicit compositor/seat teardown
  already emits the required leave).
- Serial verification remains green: `cargo test -p sommelier --all-targets
  -- --test-threads=1` and the complete workspace run pass 269 Sommelier, 11
  GUI, and 5 codegen tests; workspace check, strict Clippy, formatting, and
  `git diff --check` pass. No commit, push, service restart, or GUI restart
  was performed by this audit.

## 2026-08-13 — v5 attach-zero offset regression identified

- The Wayland protocol XML specifies that `wl_surface.attach` x/y arguments
  are ignored for version 5 and newer when they are `(0, 0)`; only non-zero
  coordinates raise `invalid_offset`.
- `CompositorHandler::on_attach` currently records every buffer attach as a
  pending legacy offset, including a legal v5 `attach(buffer, 0, 0)`. After a
  prior `wl_surface.offset(7, 8)` commit, a later zero-coordinate attach and
  commit therefore resets `current_offset` to `(0, 0)`.
- The production fix must record `pending_attach_offset` only for pre-v5
  surfaces (while retaining permissive behavior for unknown-version test
  registrations), and must add a regression that proves a v5 offset survives
  a later zero-coordinate attach.

## 2026-08-13 — v5 attach-zero offset regression fixed

- Added `v5_zero_attach_does_not_reset_committed_offset`, which first commits
  `wl_surface.offset(7, 8)`, then submits a legal v5
  `attach(buffer, 0, 0)` commit and asserts that `current_offset` remains
  `(7, 8)`. The test failed before the production change and passes now.
- `on_attach` now records `pending_attach_offset` only for pre-v5 (or
  explicitly unknown/permissive) surfaces. v5+ zero-coordinate attaches still
  forward normally, but cannot overwrite the dedicated offset state; non-zero
  coordinates remain rejected.
- Focused verification:
  `cargo test -p sommelier handler::compositor::tests::v5_zero_attach_does_not_reset_committed_offset -- --exact --nocapture`
  passed. The full serial workspace matrix and final static checks remain
  pending after this shared edit.

## 2026-08-13 — post-fix verification

- `cargo test -p sommelier --all-targets -- --test-threads=1`: 270 passed.
- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 270,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets` passed.
- `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- No service, binary, Orca process, or unrelated GUI application was started
  or restarted. No commit or push was performed.

## 2026-08-13 — final verification after v5 attach-offset fix

- Re-ran the complete serial Rust workspace matrix after the latest compositor
  change: Sommelier 270, GUI 11, and codegen 5 tests passed.
- `cargo check --workspace --all-targets` passed.
- Strict workspace Clippy (`cargo clippy --workspace --all-targets --
  -D warnings`) passed.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- Re-ran repository-wide `bun run verify`; it reported 2473 passing and 10
  failing standards assertions. The failures remain unrelated to this
  worktree: nested Biome root configurations, a missing
  `grant_applications/gwanak_s_valley_startup_2026` Slidev directory, root
  hygiene/prohibited-file/absolute-path findings, and pre-existing shebang or
  executable-bit findings.
- No primary Sommelier service, Orca process, or unrelated GUI application was
  started or restarted. No commit or push was performed.

## 2026-08-13 — VirtWL recv/error ownership audit in progress

- Compared `VirtWaylandChannel::recv` with the ChromiumOS
  `virtio_wl.c` implementation. Normal kernel error paths do not intentionally
  return VFDs: `virtwl_vfd_recv` closes VFD objects when a later userspace copy
  fails, and `virtwl_ioctl_recv` returns before publishing descriptors on read
  errors.
- The Rust boundary still has to defend against a malformed/alternate kernel
  that writes `txn.fds` and then returns an ioctl error. The current
  `Ok(Err(error))` branch returns without closing that array, while the buffer
  is initialized to `-1`; this is a narrow but real ownership gap at the
  untrusted ioctl boundary. I am adding a deterministic failure-path
  regression and closing/deduplicating any descriptors present on ioctl error.
- Allocator/lifecycle checks and the failure-path test are still pending.

## 2026-08-13 — focused input lifecycle regression pass

- Added a regression for a delayed `wl_keyboard.enter` whose surface is still
  numerically mapped but marked pending-destroy after guest
  `wl_surface.destroy`. Before the guard, the focused-surface handler test
  failed because the dead surface was installed in
  `keyboard_active_surfaces`/`active_surface_for_seat` and could reactivate
  IME. The fixed behavior returns `Action::Drop` and leaves both focus maps
  clear. Generated host-event object validation also rejects pending-destroy
  object arguments before a stateful handler runs.
- Added a two-keyboard regression for an unmapped delayed leave: keyboard A
  retained Backspace/IME state while keyboard B owned the seat's live focus.
  Before the fix, the test failed because A's pressed-key, repeat-cancel, and
  IME-suppression maps were preserved; this could make a later seat-scoped
  synthetic Backspace select the dead keyboard. The fixed path clears only A's
  per-keyboard state and preserves B's focus and the seat-level active surface.
- The unmapped-leave guard also preserves state when the same keyboard has
  already entered the current live surface; only a stale keyboard object whose
  peer owns no current focus is retired.
- Focused tests now pass:
  `delayed_enter_for_pending_destroy_surface_does_not_revive_focus`,
  `unmapped_leave_clears_stale_keyboard_state_when_another_keyboard_is_focused`.
- No service, binary, Orca process, or unrelated GUI application was started
  or restarted. No commit or push was performed in this pass.

## 2026-08-13 — virtio-gpu dmabuf metadata parity fix

- ChromiumOS `sl_linux_dmabuf_fixup_plane0_params()` was compared with the
  Rust linux-dmabuf path. The Rust proxy previously forwarded the guest stride
  and modifier unchanged, so classic virtio-gpu PRIME resources whose host
  stride or implicit modifier differs could fail host import/layout checks.
- Added a DRM resource-info query to the allocator. Plane 0 PRIME FDs are
  imported to a temporary GEM handle, queried with the ChromeOS extended
  virtgpu resource-info ABI, and always closed again. Successful virtio-gpu
  metadata replaces stride and modifier; non-virtio and unsupported-kernel
  paths preserve guest metadata.
- Added pure regression coverage for metadata-success, unsupported-kernel,
  zero-stride, and non-resource fallback decisions without requiring `/dev/dri`.
- Focused allocator and linux-dmabuf tests pass; `cargo check -p sommelier
  --all-targets` and strict package Clippy pass after the change.
- No service, binary, Orca process, unrelated GUI application, commit, or push
  was performed.

## 2026-08-13 — offset plus explicit-damage parity fix

- Confirmed that a non-zero committed surface offset with explicit guest
  damage caused an inconsistent pair: local SHM copying conservatively copied
  the complete buffer, but host forwarding emitted only the unadjusted partial
  damage rectangle.
- Added a failing regression,
  `nonzero_surface_offset_with_explicit_damage_forces_full_host_damage`, then
  made host damage conservative whenever the committed offset is non-zero.
  The test and the complete compositor-focused suite (33 tests) pass.
- This keeps host repaint coverage aligned with the local full-copy behavior
  until an offset-aware damage map is implemented.

## 2026-08-13 — final serial verification after all audits

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 275,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `bun run verify`: 2473 assertions passed and 10 unrelated monorepo
  assertions failed (nested Biome roots, missing Slidev working directory,
  root hygiene/prohibited-file/absolute-path findings, and existing shebang or
  executable-bit findings). No failure references the Sommelier review
  worktree.
- No service, binary, Orca process, unrelated GUI application, commit, or push
  was performed.

## 2026-08-13 — final compositor/dmabuf audit handoff

- Confirmed a non-zero surface-offset/damage mismatch in the Rust bridge.
  `sommelier/src/handler/compositor.rs:621-626` marks a non-zero committed
  offset as `full_mapping`, so the local SHM copy in
  `compositor.rs:701-717` conservatively copies the complete buffer.
  However, `compositor.rs:648-657` forces complete host damage only when both
  damage lists are empty; explicit damage is forwarded in unadjusted
  coordinates. `state.rs:732-734` has no offset-aware copy map. An
  offset-plus-explicit-damage commit can therefore leave host pixels stale or
  repaint the wrong region. ChromiumOS'
  `compositor/sommelier-compositor.cc` also lacks offset translation, but the
  Rust full-copy invariant makes this host-damage mismatch a valid follow-up
  bug candidate.
- Confirmed a VirtGPU linux-dmabuf metadata-fixup omission. Rust
  `sommelier/src/handler/linux_dmabuf.rs:193-203` and `:546-586` forward guest
  plane stride/modifier unchanged. ChromiumOS
  `compositor/sommelier-linux-dmabuf.cc:179-230` implements
  `sl_linux_dmabuf_fixup_plane0_params()`, importing the PRIME FD and querying
  VirtGPU DRM resource metadata before correcting stride/modifier for classic
  VirtGPU buffers. Rust `sommelier/src/allocator.rs:103-198` can select a
  `virtio_gpu` render node in Crostini, so this omission can cause host
  minigbm import failures or incorrect layout interpretation. It remains the
  highest-priority production follow-up and was not changed in this audit.
- Maintenance findings: `queue_message` is duplicated in
  `handler/shm.rs:27-57` and `handler/linux_dmabuf.rs:29-56`; viewport
  extent/transform math is duplicated by `map_buffer_damage()` and
  `full_surface_damage()` in `handler/compositor.rs:184-330`; SHM format and
  layout validation is split between `valid_shm_stride` and
  `valid_buffer_layout`, increasing future drift risk.
- Unconfirmed candidates and rationale:
  - linux-dmabuf feedback format-table handling preserves original indices,
    filters tranche indices, and balances incoming FD ownership, matching
    ChromiumOS semantics.
  - The GBM-backed SHM destination is architecturally different, but no
    runtime defect was established; VirtWL is the normal Crostini path and
    GBM is a fallback/local-GPU path.
  - Fractional `wp_viewport.set_source` validation could theoretically need a
    `bad_size` error when no destination is set, but the current handler
    abstraction has no protocol-error response mechanism.
  - `copy_shm_damage()` does not independently reject null pointers, but its
    production compositor wrapper validates mappings before calling it.
  - `create_immed` cleanup after a queued-message failure is theoretically
    relevant, but current messages are fixed-size and the failure is
    practically unreachable.
- Focused audit tests passed: compositor 32, SHM 21, linux-dmabuf 11, and
  VirtWL channel 8 (`cargo test -p sommelier <module>::tests -- --test-threads=1`).
  The audit then implemented the confirmed virtio-gpu metadata fixup described
  above; no service restart, binary launch, GUI action, commit, or push was
  performed.

## 2026-08-13 — ChromiumOS virtgpu capability-probe parity

- Rechecked the Rust `has_extended_virtgpu_resource_info()` probe against
  ChromiumOS `sl_linux_dmabuf_has_virtgpu_resource_info_type()`.
- ChromiumOS first issues `DRM_IOCTL_VIRTGPU_GETPARAM` with
  `VIRTGPU_PARAM_3D_FEATURES`; only a successful virtio-gpu query permits the
  intentionally invalid extended `RESOURCE_INFO` request to identify the
  `EINVAL` ABI. The Rust probe previously skipped this gate and could classify
  an arbitrary forced DRM node as supporting the virtio-gpu extension.
- Added the matching `GETPARAM` ABI struct/ioctl and a pure regression test
  covering the required feature-query gate, `EINVAL` detection, invalid-handle
  fallback, and successful-resource-info cases.
- Focused allocator tests: 7 passed. The complete serial workspace matrix:
  Sommelier 277, GUI 11, and codegen 5 passed.
- No Sommelier service, binary, Orca process, or unrelated GUI application was
  started or restarted. No commit or push was performed.

## 2026-08-14 — pending FD after an untracked message

- Static review found that `proxy.rs` still terminated a connection whenever a
  complete untracked host message was followed by any pending received FD.
- This conflicts with the transport invariant recorded in
  `.agent/rules/sommelier-rs-standards.md`: SCM_RIGHTS descriptors are ordered
  independently from byte-buffer boundaries, so the descriptor may belong to
  a later message whose bytes have not arrived yet. The safe behavior is to
  drop the untracked message, retain the pending descriptor, and let the next
  complete tracked message consume it; connection teardown closes it if no
  later message arrives.
- A regression test will be added and run failing before changing the helper or
  message loop.

## 2026-08-14 — pending-FD regression result

- Ran `proxy::tests::untracked_message_fd_is_retained_when_followup_is_partial`
  against the pre-fix behavior and confirmed the regression: the complete
  untracked message caused `handle_msgs()` to terminate the connection while
  the descriptor remained queued for a partial follow-up message.
- Removed that abort condition while preserving teardown ownership cleanup.
- Re-ran the same focused test after the fix: it passed and verified that the
  four-byte partial follow-up remains buffered, the descriptor remains ordered
  in `read_fds`, and `Client` teardown closes it.
- Full workspace verification is being rerun from this latest state.

## 2026-08-14 — latest verification after pending-FD fix

- `nix develop /home/kkimdev/workspaces/openstd/monorepo -c cargo test
  --workspace --all-targets -- --test-threads=1`: Sommelier 281, GUI sample
  11, and Wayland codegen 5 passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- The repository-wide `bun run verify` still reports 2473 passes and 10
  unrelated pre-existing monorepo failures: nested Biome roots, a missing
  Slidev directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang or executable-bit findings. None reference this review
  worktree or its Sommelier sources.
- No Sommelier service, binary, Orca process, unrelated GUI application,
  commit, or push was started/performed.

## 2026-08-13 — proxy output queue review

- Reviewed the pending `proxy.rs` change that preserves queued Wayland
  messages as individual send units instead of aggregating multiple messages
  into one byte/FD batch.
- This is a transport-clarity change motivated by VirtWL chunking: FD
  positions are not independently recoverable from an arbitrary byte stream,
  so retaining message boundaries makes each transport submission's FD
  association explicit.
- It is not yet backed by a failing regression. The change must remain only if
  the queue/transport tests demonstrate a concrete invariant or otherwise be
  reverted as speculative behavior.
- Package tests currently pass (`cargo test -p sommelier --all-targets
  -- --test-threads=1`: 277 tests). Full post-change workspace verification
  remains pending.

## 2026-08-13 — proxy queue decision

- Rechecked the queue change against Wayland's stream semantics and
  ChromiumOS' transport model. `sendmsg(SCM_RIGHTS)` and VirtWL carry an
  ordered byte stream plus an ordered descriptor list; descriptors are
  associated with the stream order, not with a per-message byte offset.
- VirtWL already passes descriptors on the first chunk and preserves the
  complete byte stream across subsequent chunks. Splitting every translated
  message into a separate ioctl would add transactions and latency without a
  demonstrated correctness benefit.
- Reverted the speculative per-message output queue change and restored
  coalesced forwarding. Kept the independently justified length,
  overflow, trailing-payload, stale-event, and FD-cleanup guards.
- Re-ran formatting and the Sommelier package suite after the rollback:
  277 tests passed. Workspace-wide verification is still pending.

## 2026-08-13 — SCM_RIGHTS truncation and final verification

- Replaced the Unix receive path's `nix::RecvMsg::cmsgs()` wrapper with a
  bounded raw `recvmsg` ancillary parser. `cmsg_space` now reserves Linux's
  maximum 253 `SCM_RIGHTS` descriptors, and `MSG_CTRUNC` or malformed control
  data closes every descriptor recovered before returning an error.
- The cleanup path de-duplicates raw descriptor numbers before closing them,
  and `WaylandConnection::Drop` closes descriptors that remain queued after a
  disconnect or protocol-abort path.
- Added an end-to-end Unix-socket regression test that sends four descriptors
  while receiving into a one-descriptor control buffer. The test verifies the
  kernel-installed descriptors are all closed after truncation, in addition to
  the synthetic malformed-control and pending-FD ownership tests.
- Hardened ancillary pointer arithmetic with checked address/length additions
  for payload, message, alignment, and control-chain boundaries.
- Focused connection tests: 4 passed. Full serial workspace matrix: Sommelier
  280, GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed from the review worktree.
- `bun run verify` still reports the same 10 unrelated monorepo failures
  (nested Biome roots, missing Slidev directory, root hygiene/prohibited-file/
  absolute-path findings, and existing shebang/executable-bit findings);
  2473 assertions passed and none reference the Sommelier worktree.
- No Sommelier service, binary, Orca process, unrelated GUI application,
  commit, or push was started/performed.

## 2026-08-14 — latest serial verification after parallel audits

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier
  282 passed, GUI sample 11 passed, and Wayland codegen 5 passed.
- `cargo check --workspace --all-targets`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- `bun run verify` from the monorepo root: 2473 assertions passed and 10
  pre-existing monorepo-wide standards assertions failed. The failures are
  confined to nested Biome roots, a missing Slidev working directory, root
  hygiene/prohibited-file/absolute-path findings, and existing shebang or
  executable-bit findings; none reference this Sommelier worktree.
- No Sommelier service, binary, Orca process, or unrelated GUI application was
  started or restarted. No commit or push was performed.

## 2026-08-14 — final direct verification after audit handoff

- Re-ran the complete Rust workspace matrix from the review worktree:
  `nix develop <monorepo> -c cargo test --workspace --all-targets
  -- --test-threads=1`. The current checkout reports Sommelier 281 passed,
  GUI sample 11 passed, and Wayland codegen 5 passed.
- `cargo check --workspace --all-targets`, strict
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` all passed.
- Re-ran the repository-wide `nix develop <monorepo> -c bun run verify`.
  It reports 2473 passing assertions and the same 10 pre-existing failures:
  nested Biome roots, the missing `grant_applications/gwanak_s_valley_startup_2026`
  Slidev working directory, root hygiene/prohibited-file/absolute-path findings,
  and existing shebang or executable-bit findings. None point into this
  Sommelier review worktree.
- No service, rebuilt binary, Orca process, or unrelated GUI application was
  started or restarted. The review branch remains uncommitted and unpushed;
  only an explicit `/commit` request should trigger publication.

## 2026-08-14 — seat/child input lifetime audit

- ChromiumOS reference behavior was checked directly. `sl_host_seat_removed`
  (`vm_tools/sommelier/sommelier.cc:464-469`) only clears the default-seat
  pointer. `sl_destroy_host_seat` (`sommelier-seat.cc:876-889`) then releases
  the host `wl_seat` proxy; it does not destroy or clear already-created
  `wl_keyboard` children. Child cleanup is performed only by
  `sl_destroy_host_keyboard` (`sommelier-seat.cc:724-750`) when each child
  resource itself is destroyed.
- Rust `SeatHandler::on_release` had removed `keyboard_to_seat` and all
  per-keyboard forwarding/pressed-key state. A child keyboard that was live
  before the parent `wl_seat.release` therefore lost its press/release
  pairing and dropped a later release.
- Added regression
  `handler::seat::tests::seat_release_does_not_break_live_child_keyboard_press_release_pair`.
  It reproduces the lifecycle: forwarded child press → parent seat release →
  matching child release. Before the fix it failed with `Drop` instead of
  `Forward` at the final release.
- The minimal fix makes `wl_seat.release` a parent-only lifecycle operation,
  matching ChromiumOS. Child routing and state remain until each child is
  released; focused seat tests pass. No commit, push, service restart, or GUI
  launch.

## 2026-08-14 — second compositor/SHM/dmabuf audit

- Recompared the Rust compositor, SHM, allocator, and linux-dmabuf paths with
  ChromiumOS `vm_tools/sommelier`: `sommelier-compositor.cc`,
  `sommelier-shm.cc`, `sommelier-formats.cc`, `sommelier-mmap.cc`,
  `sommelier-linux-dmabuf.cc`, `virtwl_channel.cc`, the transform and
  viewporter helpers, and `linux-headers/virtgpu_drm.h`.
- No new confirmed production bug was found in this scope, so no production
  code or regression test was added. Existing parity and safety checks cover
  buffer-scale/viewport damage mapping (including fixed-point source offsets
  and filtering outsets), transform plus viewport extents, damage clamping,
  NV12 layout/stride/plane bounds, SHM pool growth and `mremap` validation,
  GBM fallback stride handling, and linux-dmabuf feedback table/index and FD
  ownership.
- The following differences remain unmodified because they are not confirmed
  correctness bugs without a failing regression/integration test: Rust uses
  `NEW_ALLOC` with host `wl_shm` instead of ChromiumOS' capability-dependent
  `NEW_DMABUF` output path; host validation is relied on for fractional
  `wp_viewport` `bad_size` and out-of-buffer errors; GBM fallback conservatively
  excludes multi-plane formats; and ChromiumOS global/output scaling policy is
  outside this fork's current VirtWL architecture.
- Focused regression check:
  `cargo test -p sommelier
  handler::compositor::tests::damage_only_commit_copies_mutated_committed_shm_buffer
  -- --exact --nocapture` — 1 passed, 0 failed.
- No service, rebuilt binary, Orca process, unrelated GUI application, commit,
  or push was started/performed during this audit.

## 2026-08-14 — protocol audit regression

- Rechecked the Rust wire boundary against Wayland's string encoding: a
  non-null string's declared length includes a terminating NUL, and malformed
  host/client payloads must be rejected rather than decoded with lossy
  truncation.
- Added `wire_message_rejects_string_without_trailing_nul`, which reproduces a
  four-byte `abcd` payload and asserts `ProtocolError::InvalidString` while
  preserving the reader offset at the payload boundary.
- The existing `read_string_value` trailing-NUL and strict UTF-8 checks now
  satisfy this regression. No other confirmed protocol/ID/registry/transport
  bug was found in this audit; the replacement-global generation behavior and
  ignored `queue_internal_bind` result remain design-review candidates, not
  proven fixes.

## 2026-08-14 — protocol audit pass (follow-up)

- Rechecked `proxy.rs`, `connection.rs`, `wire.rs`, `state.rs`,
  `handler/registry.rs`, and generated dispatch code against the Wayland XML
  object lifetime rules and ChromiumOS Sommelier's registry/transport model.
- Confirmed that generated request validation rejects pending/destroyed IDs,
  invalid `new_id` reuse, wrong object interfaces, and unsupported request
  versions before invoking a handler. Host-generated `new_id` events reserve
  the guest server range and retain destructor mappings through
  `wl_display.delete_id`; callback and synthetic local destructors are explicit
  local-lifecycle exceptions.
- Found one unresolved transport ambiguity for the parent review: an unknown
  complete message with an attached descriptor is dropped with
  `consumed_fds = 0`. If a later complete known fd-bearing message is parsed
  before teardown, the retained descriptor can be consumed by the later
  message, even though it may belong to the unknown message. Existing behavior
  intentionally retains the descriptor when the next byte message is partial,
  so this needs a dedicated policy/regression test rather than an ad-hoc
  close.
- The previously suspected ignored `queue_internal_bind` result is not
  reachable with an oversized host-provided interface: internal bind paths use
  fixed protocol interface literals, while arbitrary oversized globals are
  filtered before that helper. No code fix was made for this path.
- No additional confirmed ID-lifetime, destructor, version-guard, registry,
  framing, or ownership bug was found. This pass made no production-code
  changes and did not start/restart any service or GUI process.
- Follow-up correction: the current `proxy.rs` already contains the
  `untracked_message_with_pending_fds` guard. It retains a pending descriptor
  only while the next message is partial and terminates before processing a
  second complete message, so the previously described FD association hazard
  is already fixed in this checkout. No remaining confirmed FD bug was found.

## 2026-08-14 — seat child-lifetime regression fixed

- The input audit reproduced a concrete lifetime mismatch against ChromiumOS:
  `sl_destroy_host_seat()` calls `sl_host_seat_removed()` and releases only the
  seat proxy; existing `wl_keyboard` children retain their own resources and
  state until their individual release requests.
- Rust `SeatHandler::on_release` had incorrectly cleared child keyboard
  routing, pressed-key/IME state, and text-input focus. A forwarded child key
  press followed by `wl_seat.release` then lost its matching child release.
- Added the regression
  `handler::seat::tests::seat_release_does_not_break_live_child_keyboard_press_release_pair`;
  it failed before the fix (`Drop` instead of `Forward` for the release).
- The minimal fix makes `wl_seat.release` a parent-only lifecycle operation.
  Child routing and text-input focus remain until the child object or its
  focus is explicitly released. Additional tests cover retained keyboard
  state and retained text-input focus.
- Focused seat tests pass: 3/3. No service, Sommelier binary, Orca process,
  or unrelated GUI application was restarted. No commit or push performed.

## 2026-08-14 — post-fix verification

- Full serial matrix after the seat-lifetime fix:
  `nix develop <monorepo> -c cargo test --workspace --all-targets
  -- --test-threads=1` — Sommelier 283, GUI sample 11, and Wayland codegen 5
  passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `nix develop <monorepo> -c bun run verify` remains at 2473 passes and 10
  unrelated monorepo failures (nested Biome roots, missing Slidev directory,
  root hygiene/prohibited-file/absolute-path findings, and existing
  shebang/executable-bit findings).
- No runtime service, Sommelier binary, Orca process, or unrelated GUI process
  was started or restarted. The review branch remains uncommitted and
  unpushed.

## 2026-08-14 — `wl_keyboard.release` stale IME focus candidate

- `KeyboardHandler::on_release` (`sommelier/src/handler/keyboard.rs:1225-1271`)
  removes `keyboard_to_seat`, clears per-keyboard state, ends the seat's
  Backspace-repeat fallback, destroys the extended keyboard, and removes
  `keyboard_active_surfaces`, but it does not reconcile the seat-level
  `active_surface_for_seat` or each matching `TextInputState.active_surface`.
- If that keyboard is the last owner of the focused surface, a legal guest
  `wl_keyboard.release` can therefore leave v3 IME focus and host v1
  activation stale. A delayed host `wl_keyboard.leave` cannot repair it:
  `KeyboardHandler::on_leave` (`keyboard.rs:845-855`) first loses the
  `keyboard_to_seat` route and returns after clearing only per-keyboard state.
- Reproduction shape: map one seat/keyboard/surface, set a v3 text input's
  active surface, enter/forward a key, issue guest `wl_keyboard.release`,
  then deliver host `wl_keyboard.leave`. Assert that release emits one v3
  `leave`, clears the seat/text-input active surfaces, deactivates host v1,
  and delayed leave does not emit a duplicate.
- The fix must preserve focus when another live keyboard on the same seat still
  owns the same surface; only the final keyboard owner may tear down the
  seat-scoped focus. No production code or regression test was changed pending
  parent approval. Complexity is bounded by one scan of the seat's live
  `keyboard_active_surfaces` and one scan of matching text inputs (O(K+T)).

## 2026-08-14 — compositor/SHM/dmabuf semantic audit

- Recompared the current Rust paths with ChromiumOS
  `vm_tools/sommelier`: `compositor/sommelier-compositor.cc`,
  `sommelier-transform.cc`, `compositor/sommelier-shm.cc`,
  `compositor/sommelier-formats.cc`, `compositor/sommelier-linux-dmabuf.cc`,
  `virtualization/virtwl_channel.cc`, `virtualization/virtgpu_channel.cc`,
  and `virtualization/linux-headers/virtgpu_drm.h`.
- No confirmed, reproducible production bug was identified in this pass, so
  no code or regression test was changed. Rust's translated
  `damage_buffer`/`commit` ordering and fixed-point viewport mapping match the
  corresponding C semantics; its `DAMAGE_MIN/MAX` constants match C's
  `MIN_SIZE/MAX_SIZE`. SHM layout/bounds checks, linux-dmabuf table
  filtering/FD ownership, VirtGPU probe/fixup ABI sizes, and VirtWL transaction
  sizing/FD-first-chunk ordering are stricter but semantically compatible.
- Maintainability observations (not actionable fixes without a failing test):
  `handler/compositor.rs` duplicates viewport/transform fallback calculations
  between `map_buffer_damage` and `full_surface_damage`; `handler/shm.rs`
  repeats bytes-per-pixel/minimum-stride logic across `valid_shm_stride` and
  `required_buffer_size`; and the large Rust handlers (~2.3k compositor,
  ~1.7k SHM lines) contain substantially more defensive lifecycle code than
  the C reference. These are refactoring candidates only.
- Known semantic differences remain unconfirmed correctness issues: the Rust
  bridge uses `NEW_ALLOC` contiguous host SHM instead of ChromiumOS'
  capability-dependent `NEW_DMABUF` output path; Rust has no global/direct
  scaling policy equivalent to `sl_transform_damage_coord`; and invalid
  linux-dmabuf requests are dropped rather than posting protocol errors because
  the current handler abstraction has no error-response path.
- Line anchors for follow-up: compositor damage/commit
  `sommelier/src/handler/compositor.rs:541-792` vs C
  `sommelier-compositor.cc:375-760`; SHM allocation/copy
  `sommelier/src/handler/shm.rs:585-1009` vs C `sommelier-shm.cc:73-103`
  and `sommelier-compositor.cc:255-304`; dmabuf feedback and params
  `sommelier/src/handler/linux_dmabuf.rs:374-712` vs C
  `sommelier-linux-dmabuf.cc:250-507`; allocator probe/fixup
  `sommelier/src/allocator.rs:304-515` vs C
  `virtgpu_channel.cc:56-90` and `sommelier-linux-dmabuf.cc:171-230`;
  VirtWL transport `sommelier/src/virtwl_channel.rs:226-349` vs C
  `virtualization/virtwl_channel.cc:92-153`.
- `git diff --check` passed. No service, binary, GUI process, commit, push,
  or restart was performed by this audit.

## 2026-08-14 — keyboard release and descriptor ambiguity hardening

- Added regressions for the remaining input-lifetime edges:
  `keyboard_release_after_seat_release_does_not_deactivate_destroyed_host_seat`
  verifies that a child keyboard released after its parent `wl_seat.release`
  clears local IME activation without queueing v1 traffic against the
  destroyed host seat; `keyboard_release_invalidates_all_same_seat_text_inputs_and_delayed_leave_is_not_duplicate`
  verifies that final-owner release invalidates every same-seat text-input,
  emits leave only for focused objects, and ignores the delayed host leave.
- `update_host_activation` now suppresses host activate/deactivate requests when
  the guest seat is pending destruction while still reconciling local
  `host_activated` state.
- Final-owner keyboard teardown now clears all same-seat text-input focus
  states. It emits a v3 leave only when an object had a concrete prior
  surface, then reconciles activation for stale/no-surface states.
- Added `proxy::tests::untracked_message_fd_ambiguity_terminates_before_followup_complete`.
  The existing partial-follow-up retention behavior remains intact; once a
  second complete message is available after an unknown complete message with
  pending SCM_RIGHTS, parsing terminates and connection teardown closes the
  ambiguous descriptor instead of misassigning it.
- Focused verification:
  `cargo test -p sommelier handler::keyboard::tests::keyboard_release -- --test-threads=1`
  — 5 passed; `cargo test -p sommelier proxy::tests::untracked_message -- --test-threads=1`
  — 2 passed.
- No runtime service, Sommelier binary, Orca process, or unrelated GUI process
  was started or restarted. No commit or push performed.

## 2026-08-14 — keyboard-release teardown review

- Parent agent added and verified `wl_keyboard.release` focus teardown at
  `sommelier/src/handler/keyboard.rs:1267-1304`. It removes only the released
  keyboard's focus entry, checks for another live keyboard on the same seat and
  surface, and emits one v3 `leave` plus host v1 deactivation only for the
  final owner. This matches the per-child lifetime observed in ChromiumOS
  (`sommelier-seat.cc:724-750`) while repairing stale Rust seat/IME focus.
- Focused tests pass: `keyboard_release_ends_focus_when_no_other_keyboard_owns_the_seat`,
  `keyboard_release_keeps_focus_owned_by_another_keyboard`, and
  `keyboard_release_does_not_cancel_another_keyboard_ime_repeat` (3/3).
  The latter verifies that seat-global Backspace-repeat cancellation is not
  performed when another keyboard still owns the focus.
- Reviewed remaining edge conditions. The host-mapping-missing branch
  (`keyboard.rs:1231-1238`) still ends seat-global repeat, but generated
  keyboard objects normally have a mapping; malformed/unmapped release is
  outside the normal lifecycle. Text-input cleanup filters matching
  `active_surface`, while normal invariants keep every active input aligned
  with `active_surface_for_seat`. A delayed host leave after guest release is
  stale because the keyboard route is removed, so it cannot duplicate v3 leave.
- No source changes made by this sub-review. Parent agent should decide whether
  to harden deactivation when the parent seat was already released; this would
  require an explicit pending-seat policy and regression, not a speculative
  change.

## 2026-08-14 — keyboard-release hardening verified

- Parent added the pending-seat guard in
  `handler/text_input.rs:1296-1310`, clearing local `host_activated` without
  queueing v1 traffic after the guest seat destructor has been sent.
- Additional regressions now cover:
  `keyboard_release_after_seat_release_does_not_deactivate_destroyed_host_seat`
  and
  `keyboard_release_invalidates_all_same_seat_text_inputs_and_delayed_leave_is_not_duplicate`.
- Verification from this worktree:
  - keyboard release subset: 5 passed;
  - text-input handler suite: 51 passed;
  - `git diff --check`: passed.

## 2026-08-14 — final serial verification after all shared edits

- Re-ran the complete workspace matrix from
  `review-sommelier-rs` with
  `nix develop /home/kkimdev/workspaces/openstd/monorepo -c cargo test
  --workspace --all-targets -- --test-threads=1`.
- Current totals: Sommelier 290 tests passed, `sommelier_test_gui` 11 passed,
  and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets` passed.
- Strict Clippy (`cargo clippy --workspace --all-targets -- -D warnings`)
  passed.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- No source edits, commit, push, runtime service restart, rebuilt binary
  launch, Orca launch, or unrelated GUI disruption occurred during this
  verification pass.
- The repository-wide `bun run verify` remains known to report the unrelated
  monorepo failures documented above; it was not changed by this pass.

## 2026-08-14 — cross-recv SCM_RIGHTS ambiguity fix

- A transport regression exposed that the complete-untracked-message marker
  was local to one `handle_msgs` call. If the partial follow-up arrived in a
  later Unix `recvmsg`, the marker disappeared and a pending descriptor could
  be assigned to the follow-up.
- `WaylandConnection::ambiguous_untracked_fd` now persists this state across
  receive/parse calls. A later complete message is rejected before dispatch,
  and `Drop` closes the still-owned descriptors.
- Regression
  `proxy::tests::untracked_message_fd_ambiguity_survives_separate_receive_calls`
  failed before the fix and now passes, alongside the same-buffer ambiguity
  and partial-follow-up retention tests (3/3).
- Final serial matrix after this fix:
  `nix develop <monorepo> -c cargo test --workspace --all-targets
  -- --test-threads=1` — Sommelier 290, GUI sample 11, and codegen 5 passed.
  Workspace check, strict Clippy, formatting, and `git diff --check` also pass.
- `bun run verify` remains at 2473 passes and 10 pre-existing monorepo
  failures outside this worktree. No service, Sommelier, Orca, or GUI process
  was started or restarted; no commit or push performed.

## 2026-08-14 — fresh input audit: mapped stale-leave candidate

- Re-read the keyboard/seat/text-input state paths and compared focus
  transitions with ChromiumOS `sl_keyboard_set_focus()` and
  `sl_destroy_host_keyboard()` in `tmp/sommelier-seat.cc`.
- A new candidate was found in `KeyboardHandler::on_leave`
  (`sommelier/src/handler/keyboard.rs`): when a mapped keyboard leaves its
  tracked old surface while another keyboard owns a different current seat
  surface, the handler removes `keyboard_active_surfaces` but returns before
  clearing that keyboard's pressed-key, repeat-cancellation, IME-suppression,
  dropped-key, and modifier state. The stale state can then be selected by
  seat-scoped Backspace/keysym routing.
- Existing tests cover an unmapped stale leave and same-surface multi-keyboard
  leave, but not this mapped old-surface/different-current-surface ordering.
  The new regression
  `mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus`
  failed before the fix because keyboard A's pressed state remained after the
  stale leave. The minimal fix in `on_leave` now clears only A's
  per-keyboard state (`keyboard_*` maps, dropped keys, and modifiers) while
  retaining keyboard B and the seat's current focus.
- The focused keyboard suite passes 58/58, including the new regression and
  existing mapped/unmapped stale-leave, multi-keyboard, repeat, and delayed
  focus tests. No runtime service or GUI process was restarted.

## 2026-08-14 — compositor/SHM/dmabuf/VirtWL parity audit

- Re-read the Rust compositor, SHM, Linux-dmabuf, allocator, and VirtWL
  channel paths against the corresponding ChromiumOS Sommelier sources.
- No new confirmed production bug was found. Damage/viewport mapping, SHM
  stride and plane bounds, damage-only commit copying, dmabuf feedback/FD
  ownership, VirtGPU ABI probing, and VirtWL chunk/descriptor cleanup remain
  consistent with the current fork's architecture.
- A semantic candidate remains unmodified: Rust queues a host
  `wl_surface.commit` for a mapped surface without a role/window, while
  ChromiumOS defers that commit until a role/window with `xdg_surface` exists.
  Rust has no equivalent window/role state model, so this needs an integration
  reproducer before any production change.
- Other differences are non-confirmed/performance or abstraction gaps:
  SHM uses `NEW_ALLOC` rather than ChromiumOS's `NEW_DMABUF` path; global/direct
  scaling policy is absent; malformed dmabuf requests are dropped rather than
  protocol-error'd; and damage bookkeeping uses the fork's one-buffer-per-guest
  structure. These were not changed speculatively.
- Possible cleanup-only refactors include duplicated damage/transform,
  stride/size, and FD queue/cleanup helpers; defer until parity tests cover
  them. No source change, commit, push, service restart, or GUI restart was
  performed during this audit.

## 2026-08-14 — client sender validation audit

- Audited `Client::handle_msgs` against generated request dispatch. A
  Client→Host request with an unknown sender was previously treated like a
  stale Host→Client event and silently dropped. This is unsafe: the guest
  believes the request was accepted while proxy state and the client diverge.
- Added regression
  `proxy::tests::unknown_client_sender_is_a_protocol_error`, which constructs a
  valid `wl_display.get_registry` request from sender `999`; it failed before
  the fix and passes after the fix.
- The minimal fix rejects only unknown Client→Host senders (terminating the
  connection); unknown Host→Client senders remain droppable for stale host
  events. Existing ordered-FD ambiguity and teardown cleanup paths remain
  unchanged.
- Focused command:
  `cargo test -p sommelier proxy::tests::unknown_client_sender_is_a_protocol_error
  -- --exact` — 1 passed.
- No runtime service, Sommelier, Orca, or GUI process was started/restarted.

## 2026-08-14 — final fresh audit and serial verification

- Input audit fixed mapped stale `wl_keyboard.leave` state: when keyboard A
  left an old surface while keyboard B owned a different current surface on
  the same seat, A's pressed/repeat/IME/dropped/modifier state remained live.
  The fix clears only A's per-keyboard state and preserves B and seat focus.
  Regression: `mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus`.
- Protocol audit fixed unknown guest sender requests being silently dropped.
  Unknown Client→Host senders now terminate the connection with pending-output
  FD cleanup; unknown Host→Client senders remain droppable for stale-event
  races. Regression: `unknown_client_sender_is_a_protocol_error`.
- Compositor/SHM/dmabuf/VirtWL audit found no additional confirmed production
  defect. A role/window-less surface commit remains an unconfirmed semantic
  candidate because this Rust fork has no equivalent role model or integration
  reproducer; no speculative change was made.
- Current verification:
  - `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier
    292, GUI 11, and codegen 5 passed.
  - Workspace check, strict Clippy, formatting, and `git diff --check` passed.
  - `bun run verify`: 2473 assertions passed; 10 pre-existing monorepo
    failures remain outside this worktree.
- No runtime service, rebuilt Sommelier, Orca, or unrelated GUI process was
  started or restarted. No commit or push was performed.

## 2026-08-14 — final verification after lifetime and FD fixes

- Full Rust matrix passes after the final changes:
  `cargo test --workspace --all-targets -- --test-threads=1` — Sommelier 294,
  `sommelier_test_gui` 11, and `wayland_codegen` 5.
- `cargo check --workspace --all-targets`, strict Clippy,
  `cargo fmt --all -- --check`, and `git diff --check` all pass.
- `bun run verify` completed with 2473 passing assertions and the same 10
  pre-existing monorepo failures (nested Biome roots, missing Slidev path,
  root hygiene/prohibited-file/absolute-path findings, and existing
  shebang/executable-bit findings); none involve this worktree's Sommelier
  changes.
- No runtime service, Sommelier, Orca, or unrelated GUI process was started or
  restarted. The review branch remains uncommitted and unpushed.

## 2026-08-14 — final review-worktree verification

- Re-ran the complete Rust matrix after the documentation-name correction:
  `nix develop /home/kkimdev/workspaces/openstd/monorepo -c cargo test
  --workspace --all-targets -- --test-threads=1` — Sommelier 294,
  `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy with `-D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` all passed.
- `bun run verify` produced 2473 passing assertions and the same 10
  pre-existing monorepo failures: nested Biome roots, missing Slidev
  directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/executable-bit findings. None point to this worktree.
- The three bounded ChromiumOS audits found no additional confirmed
  compositor, SHM/dmabuf/VirtWL, keyboard/IME, registry, protocol, or
  lifetime defect. The role-less surface-commit difference remains
  unconfirmed and intentionally unchanged.
- No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI
  application was started or restarted. The branch remains uncommitted and
  unpushed.

## 2026-08-14 — text-input manager global-remove lifecycle finding (superseded)

- Final registry/text-input audit found one reachable lifecycle mismatch:
  `reset_internal_binding_for_global()` currently takes and retires the
  internal host `zwp_text_input_manager_v1` and
  `zcr_text_input_extension_v1` IDs when their host globals emit
  `global_remove`.
- The synthetic guest `zwp_text_input_manager_v3` object is deliberately kept
  alive, and Wayland `global_remove` invalidates only the advertisement; an
  already-bound manager resource remains usable. ChromiumOS likewise keeps
  each already-bound v1 manager/extension proxy alive and only destroys the
  global wrapper. In the Rust path, a subsequent
  `zwp_text_input_manager_v3.get_text_input` sees
  `host_text_input_manager_v1_id == None` and is dropped. Even if the manager
  ID were retained alone, new text inputs would lose their extended IME child
  because `host_text_input_extension_v1_id` is also cleared.
- This is a reachable bug for a host compositor that removes and re-advertises
  the text-input globals while a guest manager object remains bound. A focused
  regression should bind the synthetic v3 manager, process host
  `global_remove`, then call `get_text_input` and assert a host v1
  `create_text_input` plus extension child request is still queued.
- This finding was subsequently reproduced by the focused regression below
  and fixed by retaining the internal manager/extension IDs until connection
  teardown.

## 2026-08-14 — text-input manager global lifetime regression

- Fresh input audit found a reachable lifecycle bug: `global_remove` for the
  host `zwp_text_input_manager_v1` or `zcr_text_input_extension_v1` global
  retired the single internal host object even though already-bound synthetic
  guest managers still depended on it. A later v3 manager request then
  silently failed to create a text-input child (and lost the extended child
  when the extension manager was removed).
- ChromiumOS keeps already-bound manager resources and their host proxies alive
  after global removal; only the advertisement is invalidated. The Rust reset
  path now clears only the global-name marker and retains the internal manager
  IDs/dispatch metadata until connection teardown.
- Added regression
  `handler::registry::tests::removed_text_input_globals_keep_bound_manager_usable`.
  It failed before the fix (`host_text_input_manager_v1_id` became `None`) and
  now verifies both manager and extension traffic after removal.
- Focused test passes. No commit, push, service restart, or GUI restart was
  performed.

- A duplicate clean regression name,
  `handler::registry::tests::removed_text_input_globals_keep_bound_manager_usable`,
  was added during the final audit and also passes; it may be folded into the
  existing regression before commit.

## 2026-08-14 — bounded compositor/SHM/dmabuf/VirtWL parity audit

- Performed a fresh read-only comparison of
  `handler/compositor.rs`, `handler/shm.rs`, `handler/linux_dmabuf.rs`,
  `allocator.rs`, and `virtwl_channel.rs` against the ChromiumOS
  `sommelier-compositor.cc`, `sommelier-shm.cc`,
  `sommelier-linux-dmabuf.cc`, `sommelier-formats.cc`,
  `sommelier-transform.cc`, and `virtwl_channel.cc` implementations.
- No new reproducible production defect was found. Damage/viewport/source
  mapping, transform and scale bounds, SHM stride/plane size checks and
  damage-only copying, dmabuf format-table index filtering and descriptor
  ownership, virtgpu resource-info ABI probing/fixup, and VirtWL transaction
  sizing/chunk ordering all remain consistent with this fork's architecture.
- The known semantic candidate remains unchanged: Rust queues host
  `wl_surface.commit` for a mapped surface even before it has a role/window,
  whereas ChromiumOS defers that commit until a role or a window with an
  `xdg_surface` exists. This fork has no equivalent window-role model, so no
  safe production fix can be made without an integration reproducer.
- Non-confirmed differences are intentional or performance-oriented:
  Rust SHM uses `NEW_ALLOC` even when dmabuf capability exists, global/direct
  compositor scaling policy is outside this fork, malformed dmabuf requests
  are dropped because the handler abstraction has no protocol-error response,
  and Rust uses a one-buffer-per-guest damage structure rather than C's
  busy/released output-buffer regions. No cleanup refactor was justified
  without parity tests.
- No source edits, tests, commit, push, service restart, or GUI restart were
  performed during this audit.

## 2026-08-14 — final bounded re-audit and verification

- Re-ran the three bounded audits against the ChromiumOS Sommelier sources:
  compositor/SHM/dmabuf/allocator/VirtWL, keyboard/IME/text-input, and
  protocol/registry/object-lifetime/FD paths.
- No additional reproducible production defect was found. The only remaining
  difference is the previously documented role/window-less surface commit
  behavior, which has no equivalent state model or integration reproducer in
  this Rust fork and was intentionally left unchanged.
- Verified in the review worktree:
  `cargo test --workspace --all-targets -- --test-threads=1` — Sommelier 292,
  GUI sample 11, and codegen 5 passed; `cargo check --workspace
  --all-targets`, strict Clippy, `cargo fmt --all -- --check`, and
  `git diff --check` also passed.
- `bun run verify` completed with 2473 passing assertions and the same 10
  pre-existing monorepo failures (nested Biome roots, missing Slidev
  directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/executable-bit findings). No failure points to Sommelier
  source changes.
- No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI
  application was started or restarted. No commit or push was performed.

## 2026-08-14 — completion verification

- Re-ran the complete Rust test matrix after updating the audit documents:
  Sommelier 294, `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy with
  `-D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed.
- Re-ran `bun run verify` from the monorepo root: 2473 assertions passed and
  10 pre-existing failures remain (nested Biome roots, missing Slidev
  directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/permission findings). No failure references Sommelier.
- No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI
  application was started or restarted. No commit or push was performed.

## 2026-08-14 — final audit handoff

- The compositor, input, and protocol re-audits are complete. No additional
  confirmed, reachable production bug was found against the local ChromiumOS
  Sommelier sources.
- Confirmed fixes retained in this worktree include per-keyboard IME state,
  explicit text-input focus teardown, damage-only SHM copying, v5
  attach-offset semantics, descriptor/FD ownership hardening, VirtWL error
  cleanup, virtio-gpu plane-0 metadata fixup, registry replacement lifetimes,
  synthetic destructor `delete_id` handling, and DRM `O_CLOEXEC`.
- The role-less surface commit difference and a theoretical pending-surface
  keyboard-leave cleanup gap remain unconfirmed; no speculative production
  change was made for either.
- Final serialized Rust verification passed: Sommelier 294 tests, GUI sample
  11 tests, and Wayland codegen 5 tests. Workspace check, strict Clippy,
  formatting, and `git diff --check` are green.
- `bun run verify` remains at 2473 passing assertions with the same 10
  pre-existing monorepo failures (nested Biome configuration, missing Slidev
  directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/permission findings); none reference this worktree.
- No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI
  application was started or restarted. The branch remains uncommitted and
  unpushed pending the user's `/commit`.

## 2026-08-14 — fresh compositor/allocator parity audit

- Compared the current Rust compositor, SHM, Linux-dmabuf, allocator, and
  VirtWL paths against
  `/home/kkimdev/workspaces/openstd/_local/chromiumos-platform2/vm_tools/sommelier`.
- No new reachable compositor or SHM correctness regression was confirmed:
  damage-only commits reuse the committed buffer, viewport/scale/transform
  damage mapping follows ChromiumOS' conservative policy, NV12 spans are
  validated before copying, and dmabuf feedback/index/FD handling matches the
  current format-table contract.
- The VirtGPU resource-info probe retains distinct 16-byte probe and 56-byte
  extended ioctl encodings, matching ChromiumOS' `virtgpu_drm.h`.
- One concrete cleanup/security parity issue was found: Rust
  `open_drm_device()` does not request `O_CLOEXEC`, while ChromiumOS
  `open_virtgpu()` opens every render node with `O_RDWR | O_CLOEXEC`.
  Added `allocator::tests::drm_device_fd_is_close_on_exec` and applied the
  minimal `O_CLOEXEC` fix. The focused test, formatting check, and
  `git diff --check` pass. No commit, push, service restart, or GUI restart
  was performed during this audit.

## 2026-08-14 — input dispatcher candidate disposition

- A generated-dispatch candidate was investigated where
  `wl_keyboard.leave` for a pending-destroy surface is rejected before
  `KeyboardHandler::on_leave`, which could theoretically leave handler-local
  XKB/drop/modifier state stale.
- The only direct regression setup required re-inserting Context keyboard state
  after `CompositorHandler::on_destroy`; current teardown synchronously removes
  the active-surface link and every Context per-keyboard state map. No
  reachable lifecycle reproducer was found, so the candidate remains
  unconfirmed and no generator or production change was made.
- The temporary regression was removed rather than retained as a misleading
  ignored test. Focused compilation, strict Clippy, formatting, and
  `git diff --check` pass after cleanup.

## 2026-08-14 — surface-destruction keyboard-state regression

- Fresh input audit identified that `wl_surface.destroy` removed only the
  per-keyboard focus map. Pressed keys, repeat-cancellation markers,
  synthetic-release tracking, and event-time state remained in `Context`.
- Added a failing regression assertion to
  `wl_surface_destroy_removes_keyboard_focus_links`; the focused test fails
  before the cleanup fix.
- The generated host-event validator also rejects a non-null `wl_keyboard.leave`
  whose surface is pending destruction before `KeyboardHandler::on_leave` can
  clear handler-local XKB/drop state. The fix must preserve stale-event
  suppression while allowing this leave to perform cleanup, then drop it
  rather than forwarding it to the destroyed guest surface.

## 2026-08-14 — surface-destruction keyboard-state audit conclusion

- Confirmed that `wl_surface.on_destroy` already clears all reachable
  per-keyboard `Context` input maps synchronously, and the existing compositor
  regression covers this behavior.
- A generated-dispatch test only failed after artificially reinserting state
  after teardown; no valid lifecycle reproducer was found for handler-local
  XKB/drop/modifier cleanup. The generator was therefore left unchanged and no
  speculative protocol change was retained.

## 2026-08-14 — final verification after bounded audit

- Re-ran `bun run verify` from the monorepo root: 2473 assertions passed and
  the same 10 pre-existing failures remain outside this Sommelier worktree
  (nested Biome roots, missing Slidev directory, root hygiene/prohibited
  files/absolute paths, and existing shebang/executable-bit issues).
- Final review-worktree Rust verification remains green:
  `cargo test --workspace --all-targets -- --test-threads=1` (292 Sommelier,
  11 GUI, 5 codegen), workspace check, strict Clippy, formatting, and
  `git diff --check`.
- No runtime service, rebuilt Sommelier, Orca, or unrelated GUI process was
  started or restarted. No commit or push was performed.

## 2026-08-14 — final completion snapshot (latest)

- The compositor, input, and protocol audits are complete; no additional
  reachable production bug was confirmed against the local ChromiumOS
  Sommelier implementation.
- Current Rust verification is green: Sommelier 294 tests, GUI sample 11
  tests, and Wayland codegen 5 tests. Workspace check, strict Clippy,
  formatting, and `git diff --check` passed.
- `bun run verify` completed with 2473 passing assertions and 10 unrelated
  pre-existing monorepo failures (nested Biome roots, missing Slidev
  directory, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/permission findings).
- No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI
  application was started or restarted. The review branch remains dirty,
  uncommitted, and unpushed pending an explicit `/commit`.

## 2026-08-14 — final completion snapshot

- All three bounded ChromiumOS parity audits (compositor/SHM/dmabuf/VirtWL,
  keyboard/IME/text-input, and protocol/registry/FD lifetime) are complete.
  No additional reachable production defect was confirmed.
- Current Rust matrix is green: Sommelier 294, GUI sample 11, and codegen 5
  tests passed; workspace check, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify` reports 2473 passes and 10 unrelated, pre-existing
  monorepo failures; none reference this worktree.
- Runtime services and GUI applications were not restarted. Changes remain
  uncommitted and unpushed until the user invokes `/commit`.

## 2026-08-14 — line-level audit closure

- Rechecked the current Rust implementation against the local ChromiumOS
  Sommelier sources with separate compositor/SHM/dmabuf/VirtWL,
  keyboard/IME/text-input, and protocol/registry/FD audits.
- No additional reachable production defect was confirmed, and no temporary
  test or source edit from the audits remains in the worktree.
- The only concrete architectural caveat is the existing `--gpu-accel` GBM
  path, which forwards a GBM PRIME FD through the SHM bridge. The CLI already
  marks this mode as broken, and it is outside the supported VirtWL path; no
  speculative redesign was made.
- The stale/unmapped `wl_keyboard.enter` candidate is rejected by generated
  dispatch before `KeyboardHandler::on_enter`; the direct handler-only
  reproducer was removed as unreachable test scaffolding.
- Final source checks before the serialized matrix: `cargo fmt --all -- --check`
  and `git diff --check` pass. No service, proxy, or GUI process was restarted.

## 2026-08-14 — serialized verification after audit closure

- `nix develop /home/kkimdev/workspaces/openstd/monorepo -c cargo test
  --workspace --all-targets -- --test-threads=1`: Sommelier 294,
  `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- Workspace `cargo check --workspace --all-targets`, strict Clippy with
  `-D warnings`, `cargo fmt --all -- --check`, and `git diff --check` passed.
- Root `bun run verify` again reported 2473 passes and the same 10
  pre-existing monorepo failures (nested Biome roots, missing Slidev path,
  root hygiene/prohibited-file/absolute-path findings, and existing
  shebang/permission findings). No failure references this review worktree.

## 2026-08-14 — SHM backing-fd resize reachability follow-up

- Rechecked the remaining `backing_fd_has_size()`/`mremap()` caveat against
  ChromiumOS `vm_tools/sommelier/compositor/sommelier-shm.cc` and
  `virtualization/wayland_channel.h`.
- In the supported Crostini flow, the guest's `wl_shm.create_pool` descriptor
  is the client-provided shared-memory fd (normally a memfd or shm-backed
  regular file). The non-regular descriptor branch in this Rust fork is only a
  defensive allowance for providers with no meaningful `st_size`; it is not
  the VirtWL intermediate allocation path. VirtWL `NEW_ALLOC` fds are created
  for host-side intermediate buffers, not accepted as guest pool fds.
- The regular-file path checks the declared pool extent before both initial
  `mmap()` and every growth `mremap()`. No supported non-regular guest-pool
  path or reproducible SIGBUS condition was found, so this remains an
  unconfirmed portability caveat rather than a production bug. No code change
  was justified.
- No service, proxy, or GUI process was restarted; the review worktree remains
  dirty and uncommitted.

## 2026-08-14 — final input/IME read-only re-audit

- Rechecked Rust keyboard, seat, text-input v1/v3, accelerator, data-device,
  and VirtWL clipboard paths against ChromiumOS
  `sommelier-seat.cc`, `sommelier-text-input.cc`,
  `sommelier-data-device-manager.cc`, and
  `virtualization/virtwl_channel.cc`.
- No additional reachable input or IME defect was confirmed. Per-keyboard
  pressed/repeat/drop state, repeated-key handling, XKB keymap lifetime,
  text-input transaction ordering, v1→v3 preedit/commit/delete conversion,
  content-type version gating, synthetic Backspace timing, clipboard pipe
  ownership, and child seat lifetimes remain consistent with the reference
  behavior and current regression coverage.
- Cursor-rectangle and data-device coordinate transforms differ only where
  this Rust fork has no ChromiumOS global/direct scaling or output-position
  model; no safe fix can be made without adding that architecture and an
  integration reproducer.
- The stale/unmapped `wl_keyboard.enter` candidate is unreachable in normal
  operation: generated dispatch validates the `wl_surface` mapping before
  invoking `KeyboardHandler::on_enter`. The direct handler-only reproducer
  was removed as misleading.
- No production edits, new regression tests, commit, push, service restart,
  or GUI restart were made during this re-audit.

## 2026-08-14 — text-input content-hint correction

- Compared `map_v3_content_type` with the vendored
  `text-input-unstable-v3.xml` and `text-input-extension-unstable-v1.xml`.
  The v3 `content_hint` bit `0x2` is `auto_correction`, while Chrome's
  extension flags distinguish `autocorrect_on` (`1 << 2`) from
  `spellcheck_on` (`1 << 4`).
- The mapper incorrectly emitted only `spellcheck_on` for this bit, so a
  normal editor requesting automatic correction lost the correction flag.
  Added a focused regression assertion, observed it fail (`left: 16,
  right: 4`), then changed the mapping to emit `autocorrect_on`.
- Focused text-input test now passes; `cargo fmt --all -- --check` and
  `git diff --check` pass. No commit, push, service restart, or GUI restart
  was performed.

## 2026-08-14 — fresh protocol/FD/VirtWL deep audit

- Rechecked the generated protocol dispatcher, object-ID allocation and
  destructor bookkeeping, registry global generations, wire parsing, Unix
  ancillary-FD ordering, and VirtWL ioctl ownership against ChromiumOS
  `virtualization/virtwl_channel.cc` and the Wayland protocol rules.
- ChromiumOS's 28-FD sentinel layout, 4096-byte transaction bound, and stream
  chunking match the Rust implementation. Rust's additional malformed/error
  descriptor cleanup is covered by focused tests.
- No additional confirmed, reachable production bug or justified
  simplification was found. No source changes were made during this audit.
- Existing verification remains green: Sommelier package tests (294),
  workspace check, strict Clippy, formatting, and `git diff --check`.

## 2026-08-14 — post-fix serialized verification

- After correcting v3 `content_hint=0x2` to emit Chrome's
  `autocorrect_on (1 << 2)` flag, the complete Rust matrix passed:
  Sommelier 294 tests, `sommelier_test_gui` 11 tests, and `wayland_codegen`
  5 tests.
- `cargo check --workspace --all-targets`, strict Clippy with `-D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` all passed.
- Root `bun run verify` completed with 2473 passing assertions and the same
  10 pre-existing monorepo failures: nested Biome roots, missing Slidev
  directory/path, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/executable-bit findings. No failure references the
  Sommelier review worktree.
- No service, proxy, Orca, or GUI process was started or restarted. The
  review branch remains uncommitted and unpushed pending the user's
  `/commit`.

## 2026-08-14 — focused keyboard keysym routing correction

- The fresh input audit found a reachable multi-`wl_keyboard` routing bug:
  `keyboard_for_keysym` selected the lowest guest keyboard ID even when a
  different keyboard on the same seat owned the current focused surface.
- Added the regression `keysym_prefers_keyboard_with_current_surface_focus`;
  the pre-fix behavior routed the event to guest keyboard `100` while the
  focused keyboard was `200`.
- The selector now prioritizes a keyboard whose tracked host surface equals
  `active_surface_for_seat`, while retaining the existing IME Backspace
  fallback preference and deterministic selection when no focused mapping is
  available.
- Focused text-input, keyboard, and seat tests passed; the complete Sommelier
  suite now passes 295 tests. Formatting and `git diff --check` pass.
- No service, proxy, Orca, or GUI process was started or restarted. No commit
  or push was performed.

## 2026-08-14 — final serialized verification after keysym fix

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 295,
  `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy with `-D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` passed.
- Root `bun run verify` again reported 2473 passes and the same 10
  pre-existing monorepo failures (nested Biome roots, missing Slidev path,
  root hygiene/prohibited-file/absolute-path findings, and existing
  shebang/executable-bit findings). None reference the Sommelier worktree.
- No service, proxy, Orca, or GUI process was started or restarted. The
  review branch remains uncommitted and unpushed.

## 2026-08-14 — SHM pending-attach and invalid-bind hardening

- Compositor audit reproduced a legal Wayland lifecycle race:
  `wl_surface.attach(buffer)` followed by `wl_buffer.destroy` before
  `wl_surface.commit` previously dropped the Rust SHM backing and cleared the
  pending attachment because `submitted_buffers` is set only at commit time.
- Added `surface_references_buffer` and defer the destroyed buffer through the
  pending attach as well as committed references. The attach path also resets
  `host_released` for every buffer reuse, including v5+ surfaces. Regression
  `destroyed_pending_shm_attach_survives_until_commit` mutates the source after
  guest destroy and verifies the destination receives the frame on commit.
- Protocol audit found invalid `wl_registry.bind` requests were silently
  dropped, unlike ChromiumOS which posts a fatal `wl_display.error`. The proxy
  now queues the diagnostic, marks the session fatal, flushes the reverse
  direction, and closes the session without forwarding the invalid bind.
  Regression `invalid_bind_queues_a_display_protocol_error` covers this path.
- Focused compositor (35), SHM (21), and registry (31) tests pass. No service,
  proxy, Orca, or GUI process was started or restarted; no commit or push was
  performed.

## 2026-08-14 — serialized final verification after protocol integration

- Re-ran the previously failing proxy FD-ambiguity regression. The complete
  Sommelier package suite now passes: 298 tests.
- Full workspace test matrix passed with 298 Sommelier tests, 11
  `sommelier_test_gui` tests, and 5 `wayland_codegen` tests.
- `cargo check --workspace --all-targets`, strict Clippy with `-D warnings`,
  `cargo fmt --all -- --check`, and `git diff --check` all passed.
- Root `bun run verify` completed with 2473 passing assertions and the same 10
  unrelated monorepo failures: nested Biome roots, missing Slidev directory,
  root hygiene/prohibited-file/absolute-path findings, and existing
  shebang/executable-bit findings. No failure references this Rust worktree.
- The review worktree remains uncommitted and unpushed. No Sommelier,
  compositor, Orca, or GUI process was started or restarted.

## 2026-08-14 — FD regression test parallelism hardening

- A repeated parallel package run exposed test-only races caused by asserting
  that a fixed descriptor number was `EBADF` after cleanup while another test
  could reuse that number.
- Updated the affected proxy, connection, and context-drop tests to record the
  descriptor's `/proc/self/fd` target and assert that the owned resource is no
  longer present, without changing production FD ownership behavior.
- Eight consecutive parallel Sommelier package runs passed after the test
  hardening.

## 2026-08-14 — protocol-validation hardening after fresh audit

- Added fatal `wl_display.error` handling for invalid `wl_surface.attach`
  offsets, buffer scale/transform values, malformed `wp_viewport` source and
  destination rectangles, stale viewport requests, and duplicate
  `wp_viewporter.get_viewport` bindings. Error codes follow the vendored XML:
  `invalid_offset=3`, `invalid_scale=0`, `invalid_transform=1`,
  `wp_viewport.bad_value=0`, `wp_viewport.no_surface=3`, and
  `viewport_exists=0`.
- Bounded `queue_protocol_error` diagnostics to preserve the error event when
  an untrusted client string is too large for Wayland's 16-bit message length.
- Added protocol errors for malformed SHM pool/buffer sizes, formats, strides,
  layouts, and backing descriptors (`invalid_format=0`, `invalid_stride=1`,
  `invalid_fd=2`).
- Added linux-dmabuf fatal validation for plane index/set, incomplete or
  unsupported planes, invalid dimensions/format, and out-of-bounds metadata
  using the vendored error values. Added regression coverage for duplicate and
  out-of-range planes and invalid dimensions.
- Verification after these edits:
  `cargo test --workspace --all-targets -- --test-threads=1` passed with
  Sommelier 304, GUI 11, and codegen 5 tests; workspace check, strict
  Clippy, formatting, and `git diff --check` passed. No runtime service or
  GUI process was restarted, and no commit or push was performed.
- Root `bun run verify` completed with 2473 passing assertions and the same
  10 unrelated pre-existing monorepo failures (nested Biome roots, missing
  Slidev path, root hygiene/prohibited-file/absolute-path findings, and
  existing shebang/permission findings).

## 2026-08-14 — final matrix after FD test hardening

- The serialized full workspace matrix passed: Sommelier 298,
  `sommelier_test_gui` 11, and `wayland_codegen` 5.
- Workspace check, strict Clippy, formatting, and `git diff --check` all
  passed after the test-only FD assertion changes.
- No production FD behavior, service, proxy, Orca process, or GUI process was
  started or restarted. The branch remains uncommitted and unpushed.

## 2026-08-14 — bounded compositor/SHM/dmabuf/allocator re-audit

- Compared the current Rust paths against the local ChromiumOS
  `vm_tools/sommelier` implementation:
  `sommelier/src/handler/compositor.rs` (surface commit, damage mapping,
  viewport state, attach offsets), `shm.rs` (pool/buffer bounds, NV12 planes,
  deferred buffer lifetime), `linux_dmabuf.rs` (plane validation, feedback
  table forwarding, FD ownership), and `allocator.rs` (render-node probing,
  virtgpu ioctl encodings and PRIME metadata fixup).
- No additional confirmed reachable production bug or unnecessary
  correctness-affecting complexity was found. The pending-attach
  `wl_buffer.destroy` lifetime fix remains covered by
  `destroyed_pending_shm_attach_survives_until_commit`
  (`compositor.rs:713-835`, `shm.rs:1048-1137`).
- Follow-up candidate only (not changed or classified as a confirmed runtime
  bug): `wp_viewport.set_source` accepts fractional source dimensions when no
  destination is set (`compositor.rs:884-898`), while the vendored
  `viewporter.xml:90-96` specifies a `bad_size=1` error when those dimensions
  are non-integer at state application. Source-out-of-buffer (`out_of_buffer=2`)
  is likewise left to host validation.
- No source edits, service/proxy/GUI restarts, commits, pushes, or new test
  runs were performed during this bounded re-audit.

## 2026-08-14 — input/compositor audit handoff

- Re-checked the v3 content-hint mapping against the vendored protocol XML:
  `text-input-unstable-v3.xml` defines `0x2` as `spellcheck`, while the
  Chrome extension defines `spellcheck_on` as `1 << 4` and keeps
  `autocorrect_on` (`1 << 2`) separate. The current
  `map_v3_content_type` implementation and regression assertion therefore
  have the correct `0x2 -> 1 << 4` behavior. The similarly named v1 bit is
  `auto_correction`, and must not be used for v3 translation.
- Compared compositor damage, SHM lifetime, keyboard focus, and proxy FD
  handling with ChromiumOS Sommelier. No additional confirmed reachable bug
  was found. The only remaining lead is unproven: synthetic Backspace
  selection is seat-scoped while ordinary keysym routing prefers the focused
  keyboard in a multi-keyboard/same-seat setup; this needs a reproducer before
  changing behavior.
- Existing focused and serialized workspace verification remains green as
  recorded above. No service, proxy, Orca, or GUI process was started or
  restarted; no commit or push was performed.

## 2026-08-14 — protocol audit final findings

- Remaining protocol-semantics gaps are concentrated in validation handlers
  that return `Action::Drop` without queuing the mandated fatal
  `wl_display.error`: `wl_surface.attach` v5 non-zero offsets
  (`invalid_offset=3`), non-positive buffer scale (`invalid_scale=0`), and
  out-of-range transform (`invalid_transform=1`); `wp_viewport` bad source or
  destination values (`bad_value=0`), plus stale viewport requests after the
  associated surface is destroyed (`no_surface=3`).
- SHM malformed pool/buffer requests currently drop silently despite
  `wl_shm.invalid_format=0`, `invalid_stride=1`, and `invalid_fd=2`. Linux
  dmabuf `add` likewise defers invalid/duplicate plane errors instead of
  reporting `plane_idx=1` or `plane_set=2`; invalid create dimensions/format,
  incomplete planes, and out-of-bounds metadata map to the protocol's fatal
  codes (5/4/3/6) but are currently dropped. These are follow-up hardening
  candidates.
- `queue_protocol_error` accepts arbitrary diagnostic strings. A maximally
  long malformed interface string can make the error event exceed Wayland's
  16-bit message limit; the helper then drops the diagnostic while still
  terminating the session. Cap diagnostics to a short fallback before
  encoding.
- Confirmed pending-attach `wl_buffer.destroy` lifetime fix and regression
  remain green. No production process was started/restarted. Verification
  remains: Sommelier 298 tests; full workspace 298 + 11 + 5 tests; check,
  strict Clippy, format, and diff checks pass. Branch remains uncommitted and
  unpushed.
- A final pass found one more compositor gap: duplicate
  `wp_viewporter.get_viewport` requests still return `Action::Drop`, but the
  protocol requires fatal `wp_viewporter.error.viewport_exists=0`; add a
  queued display error and regression before declaring viewport validation
  complete.

## 2026-08-14 — fractional viewport source validation

- ChromiumOS/viewporter semantics require `wp_viewport.bad_size=1` when a
  fractional source width or height is committed without a destination.
- Added commit-time validation against the double-buffered viewport state.
  The proxy queues the fatal error on the viewport object and suppresses the
  invalid surface commit.
- Added `fractional_source_without_destination_is_bad_size_on_commit`.
- Fresh compositor, input, and protocol audits found no other confirmed
  reachable bugs. No runtime service, GUI, Orca, commit, or push was started.

## 2026-08-14 — post-fix verification

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 305,
  `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy
  (`-D warnings`), `cargo fmt --all -- --check`, and `git diff --check`
  passed.
- Root `bun run verify`: 2473 passed and the same 10 pre-existing monorepo
  failures (nested Biome roots, missing Slidev path, root hygiene/prohibited
  file/absolute-path findings, and existing shebang/executable-bit findings).
- No runtime service, proxy, Orca, or GUI process was started or restarted.
  The review branch remains uncommitted and unpushed.

## 2026-08-14 — final deep audit follow-ups

- Three parallel ChromiumOS comparisons were completed for compositor/SHM,
  keyboard/IME, and transport/FD/codegen paths.
- Fixed `wl_shm_pool.resize`: an `mremap` failure now queues fatal
  `wl_shm.invalid_fd=2` instead of silently leaving the old mapping while the
  client believes the resize succeeded. Added
  `failed_pool_resize_queues_invalid_fd_error`.
- Fixed keymap reload lifecycle: a valid `wl_keyboard.keymap` replacement now
  resets only XKB interpretation state and preserves physical
  pressed/forwarded key sets, so a key held across a resend still gets its
  release. Added `keymap_reload_preserves_forwarded_key_release_pairing`.
- Enforced text-input-v3's post-`leave` rule: state-changing requests and
  `commit` are ignored until the next `enter`; added
  `v3_requests_after_leave_are_ignored_until_next_enter`.
- Duplicate keyboard enters for an already-focused resource are consumed
  locally, matching ChromiumOS and preventing duplicate guest focus events.
- Confirmed the hidden `--gpu-accel` path was unsafe: its GBM fallback produced
  a PRIME fd but submitted it as a `wl_shm.create_pool` fd. The CLI now fails
  fast with an explicit diagnostic until a real linux-dmabuf host submission
  path exists. VirtWL SHM remains the supported path.
- Fresh serialized matrix passed: Sommelier 308, test GUI 11, and
  wayland_codegen 5. Workspace check, strict Clippy, formatting, and
  `git diff --check` also passed. No runtime service, proxy, Orca, or GUI
  process was started or restarted; no commit or push was performed.

## 2026-08-14 — root verification snapshot

- `bun run verify` from the monorepo root completed with 2473 passes and the
  same 10 pre-existing failures (nested Biome roots, missing Slidev path, root
  hygiene/prohibited-file/absolute-path findings, and existing shebang or
  executable-bit findings). None references the Sommelier review changes.

## 2026-08-14 — sample GUI runtime and Korean font verification

- Built and launched `sommelier_test_gui` against the existing
  `WAYLAND_DISPLAY=wayland-2` socket without restarting the primary Sommelier
  or Orca processes.
- The first direct run needed the local Wayland/EGL/Mesa runtime paths because
  the development shell did not expose `libwayland-client` or GLVND. The
  two-second smoke run then created and closed a Wayland window successfully.
- Manual input reached the client (`Preedit`, `Commit`, and held Backspace
  events), but Korean text initially rendered as tofu boxes because egui's
  bundled fonts do not contain Hangul.
- Added runtime Korean font discovery and registration to the sample app.
  It honors `SOMMELIER_TEST_GUI_FONT` and falls back to standard Noto/Nanum
  paths. The rebuilt run logged registration of
  `NanumGothic-Regular.ttf`; the window was then reopened for manual testing.
- Sample GUI tests now pass 12/12. The complete Rust workspace matrix passes
  Sommelier 308, sample GUI 12, and codegen 5 tests; workspace check, strict
  Clippy, formatting, and `git diff --check` also pass.
- VS Code was launched with `code --reuse-window` on the review worktree.
  `code --status` confirmed the active window title includes the Korean input
  `가나다라` and the `review-sommelier-rs` folder.
- Root `bun run verify` remains at 2473 passes and 10 unrelated pre-existing
  monorepo failures; none references the Sommelier or sample-GUI changes.

## 2026-08-14 — upstream virtwl rebase

- Fetched `origin/virtwl` (`google/sommelier-rs:virtwl`) and found one
  upstream-only commit, `922575c` (`Add systemd user service for sommelioer-rs
  for virtwl (#24)`), which explained GitHub's "1 commit behind" status.
- Rebased the eleven local commits onto `origin/virtwl` successfully.
- Re-ran the serialized workspace matrix: Sommelier 308, sample GUI 12, and
  codegen 5 passed; workspace check, strict Clippy, formatting, and
  `git diff --check` passed.
- The personal fork's `virtwl` ref is the only intended publish target.

## 2026-08-14 — GitHub Actions and Nix release packaging

- Prepared GitHub-hosted `ubuntu-24.04` workflows for the personal
  `virtwl` branch. CI now builds x86_64 and aarch64, runs the serialized
  regression suite, formatting, and strict Clippy. Release builds use the
  `virtwl-v*` tag convention, verify the Cargo version, publish both
  architectures, and generate one `SHA256SUMS` file without matrix upload
  races.
- Added weekly Cargo and GitHub Actions Dependabot configuration and updated
  the README's personal release links and maintainer tag instructions.
- GitHub settings verified/applied: default branch `virtwl`, Actions enabled
  with read-only default workflow permissions, vulnerability alerts and
  automated security fixes enabled, and automatic branch deletion after merge.
  The repository has no self-hosted runners; workflows intentionally use
  GitHub-hosted runners.
- `actionlint` passes. Nix `flake check --all-systems --no-build`, the existing
  upstream binary package build, and the source package build pass. The source
  package now runs the compositor test binary serially in Nix to avoid the
  workspace FD-test race; the full CI matrix remains responsible for GUI and
  code-generator tests.
- GitHub workflow, README, Dependabot, and Nix changes remain uncommitted and
  unpushed pending the explicit `/commit` workflow. The Nix binary package
  remains pinned to Google's verified `virtwl-v0.2.0` release until the
  personal `virtwl-v0.2.1` release exists and its hashes can be recorded.
- Final monorepo `bun run verify` completed with 2473 passes and the same 10
  unrelated baseline failures (nested Biome roots, missing Slidev path, root
  hygiene/prohibited-file/absolute-path findings, and existing shebang or
  executable-bit findings). None references the Sommelier or Nix changes.
