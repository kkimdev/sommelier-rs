# Sommelier-RS review walkthrough

This review compares the Rust proxy with ChromiumOS Sommelier behavior and
keeps every confirmed fix behind a regression test.

Entries below are chronological research notes. Sections labeled
`Historical` describe superseded wire sequences and are retained to explain
why the current implementation changed; the latest dated section and the
configuration document define the current contract.

## Damage and SHM

- Surface damage is expanded and clamped to ChromiumOS' `INT_MIN / 10` through
  `INT_MAX / 10` headroom before host encoding.
- `damage_buffer` follows ChromiumOS viewport/source/destination scaling rules.
- Non-zero committed surface offsets force a complete copy until an
  offset-aware SHM copy map exists.
- Non-zero offsets also force complete host damage when explicit guest damage
  is present, keeping host repaint coverage consistent with that full copy.
- 90/270-degree transforms swap full-damage extents for non-square buffers.

Relevant tests include:
`extreme_surface_damage_is_clamped_before_host_encoding`,
`nonzero_surface_offset_without_explicit_damage_forces_full_host_damage`,
`rotated_non_square_buffer_full_damage_swaps_extent`, and
`damage_only_commit_copies_mutated_committed_shm_buffer`.

## Keyboard and IME

- Keymap mappings use private read-only memory (`MAP_PRIVATE`) and reject
  malformed descriptors.
- Pressed/released/repeated state is scoped per host keyboard.
- Repeated keysym events are forwarded only when a matching physical guest
  press exists, preventing duplicate synthetic pairs.
- Existing text-input v1/v3 delete, preedit, serial, and focus state tests
  remain passing.
- Destroying a focused surface sends the guest text-input-v3 `leave` event
  immediately; a delayed host keyboard leave cannot leave stale IME focus or
  duplicate that notification.

## Registry and dmabuf lifecycle

- Internal dmabuf factories are explicitly destroyed on global replacement.
- Aura child objects are released before the aura shell factory when the
  protocol version supports destructors.
- Internal objects without wire destructors retire their dispatch metadata
  while reserving their host IDs, preventing stale-event collisions after
  re-advertisement.
- Async dmabuf parameter FD ownership and generated ID mapping are covered by
  focused tests.
- Classic virtio-gpu PRIME plane-0 imports query local DRM resource metadata;
  when the extended resource-info ABI is available, host stride and explicit
  modifier replace stale guest layout metadata, with a safe fallback for
  non-virtio/older kernels.
- Synthetic `wl_shm_pool` and `zwp_text_input_manager_v3` destructors emit one
  local guest `wl_display.delete_id`; duplicate requests are rejected after
  retirement. `wl_shm` has no destroy request and remains reserved for the
  connection.
- Guest v3 text-input destruction emits its local `delete_id` while retaining
  the host v1 backing ID reservation (and pending host-extension destructor)
  until the corresponding host-side lifecycle completes.
- Generated destructors retain guest/host mappings until host
  `wl_display.delete_id`; pending objects reject new requests and stale host
  events are filtered. Retired SHM buffers allow only their deferred
  `wl_buffer.release` through this state.
- `wl_seat.release` clears seat-scoped focus, keyboard routing, pressed-key,
  repeat, and IME state.
- Keyboard-extension `global_remove` retires only the manager object. Existing
  `zcr_extended_keyboard_v1` children and their reverse mappings stay routable
  until the owning `wl_keyboard` is released, matching ChromiumOS lifecycle.
- Aura-shell `global_remove` likewise retires only the shell manager. Existing
  `zaura_surface` children remain mapped and keep their negotiated versions
  until the owning `wl_surface` is destroyed; a replacement shell global is
  used only for future child creation.
- Host `wl_shm` global removal marks pre-existing synthetic `wl_shm` objects
  and `wl_shm_pool` children stale. Their non-destructor requests are ignored
  so they cannot mmap, resize, or create buffers through a replacement host
  binding; pool destruction still releases local state and emits the generated
  guest `delete_id`. Replacement host format events are sent only to new
  synthetic SHM objects.

## Verification

`cargo test --workspace --all-targets -- --test-threads=1` passes 294 Sommelier
tests, 11 GUI tests, and 5 codegen tests. Workspace check, strict Clippy,
formatting, and diff whitespace checks also pass.

The repository-wide `bun run verify` command was also run. It still reports
unrelated pre-existing monorepo failures (nested Biome configurations, a
missing Slidev directory, and root hygiene/shebang/absolute-path findings);
none are in this Sommelier worktree.

The review branch remains uncommitted and unpushed until `/commit` is invoked.

The final parity follow-up added the virtio-gpu plane-0 metadata fixup and pure
fallback/success tests. The DRM query closes its temporary GEM handle on every
path; no runtime service or GUI process was restarted.

The final verification after that follow-up is 280/11/5 for Sommelier, GUI, and
codegen respectively; workspace check, strict Clippy, formatting, and diff
checks all pass.

The final ChromiumOS parity pass also compared the Rust SHM, registry,
keyboard/seat, clipboard, VirtWL, and generated ID/FD paths with the local
`vm_tools/sommelier` sources. No further reproducible production defect was
found in those paths after the 269/11/5 serial workspace matrix and strict
check, Clippy, format, and diff verification.

The follow-up v5 attach-offset audit found and fixed one additional lifecycle
edge: after a committed `wl_surface.offset(7, 8)`, a legal
`attach(buffer, 0, 0)` on a version-5 surface must leave the offset unchanged.
`on_attach` now keeps legacy attach coordinates only for pre-v5/permissive
objects, with regression coverage in
`v5_zero_attach_does_not_reset_committed_offset`.

Latest verification after that fix:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 280,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify` remains blocked by 10 unrelated monorepo-wide standards
  failures (2473 assertions passed), as detailed in `log.local.md`.

The latest allocator parity pass also mirrors ChromiumOS' virtgpu capability
probe: `DRM_IOCTL_VIRTGPU_GETPARAM(VIRTGPU_PARAM_3D_FEATURES)` must succeed
before the extended `RESOURCE_INFO` `EINVAL` probe can enable plane-0
stride/modifier fixups. This prevents false capability detection on forced or
non-virtio DRM nodes and is covered by a focused regression test.

Latest verification after that probe fix:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 280,
  GUI 11, and codegen 5 passed.
- No service, binary, Orca process, or unrelated GUI application was started
  or restarted. The review branch remains uncommitted and unpushed.

The final Unix transport pass replaced the truncation-blind `nix` ancillary
iterator with a bounded raw `recvmsg` parser. The receive buffer accepts the
Linux maximum 253 descriptors, malformed/truncated control data closes all
descriptors recovered by the kernel, and connection teardown closes any
descriptors still pending in the stream. An end-to-end Unix socket regression
test covers a four-descriptor `SCM_RIGHTS` batch received through a
one-descriptor control buffer.

Latest verification after the transport pass:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 280,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify`: 2473 assertions passed; the same 10 pre-existing
  monorepo-wide standards failures remain outside this worktree.

Latest serial verification after the parallel compositor, input, protocol, and
registry audits:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 282,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify` remains limited by 10 pre-existing monorepo failures
  (nested Biome roots, missing Slidev directory, root hygiene/prohibited-file/
  absolute-path findings, and existing shebang/executable-bit findings);
  2473 assertions passed and none reference this worktree.
- Runtime services and GUI processes were left untouched; the branch remains
  uncommitted and unpushed until the user invokes `/commit`.

Final direct verification from the current review worktree:

- `nix develop <monorepo> -c cargo test --workspace --all-targets
  -- --test-threads=1`: Sommelier 281, GUI sample 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `nix develop <monorepo> -c bun run verify` reports 2473 passing assertions
  and the same 10 pre-existing monorepo failures (nested Biome roots, the
  missing Slidev working directory, root hygiene/prohibited-file/absolute-path
  findings, and existing shebang/executable-bit findings); no failure points
  into the Sommelier review worktree.
- No runtime service, Sommelier binary, Orca process, or unrelated GUI process
  was started or restarted. The review branch is still uncommitted and
  unpushed pending an explicit `/commit`.

The final transport-boundary audit found one additional ordering bug: a
complete untracked host message must not terminate the connection merely
because a descriptor is pending. SCM_RIGHTS descriptors follow the ordered
stream independently of byte-buffer boundaries, so that descriptor may belong
to a later message whose bytes are still partial. The regression test
`untracked_message_fd_is_retained_when_followup_is_partial` failed before the
guard was removed and passes afterward; it also verifies that connection
teardown closes the still-pending descriptor.

Latest verification after that transport fix:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 281,
  GUI 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify` remains limited by 10 pre-existing monorepo-wide failures;
  2473 assertions passed and none reference this Sommelier worktree.

Latest verification after persisting descriptor ambiguity across recv calls:

- `nix develop <monorepo> -c cargo test --workspace --all-targets
  -- --test-threads=1`: Sommelier 290, GUI sample 11, and codegen 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- Added `untracked_message_fd_ambiguity_survives_separate_receive_calls`;
  it failed before the connection-level marker was added, then passed with
  the same-buffer ambiguity and partial-follow-up tests.
- `bun run verify` remains at 2473 passing assertions and 10 unrelated
  monorepo-wide failures. No runtime service or GUI process was restarted.

## 2026-08-14 — seat child lifetime parity fix

The ChromiumOS comparison found that `wl_seat.release` must not tear down
already-created child devices. The Rust handler previously cleared child
keyboard routing and IME state, causing a matching key release to be dropped
when a client released the parent seat first. The handler now forwards the
parent destructor without touching child state.

Regression coverage:

- `seat_release_does_not_break_live_child_keyboard_press_release_pair`
- `release_keeps_child_keyboard_routing_state`
- `release_does_not_end_child_text_input_focus`

All three pass. The complete current matrix is Sommelier 283 tests, GUI sample
11 tests, and codegen 5 tests, all passing. Workspace check, strict Clippy,
formatting, and `git diff --check` also pass. `bun run verify` still reports
2473 passes and the same 10 unrelated monorepo standards failures. No runtime
service, Sommelier process, Orca process, or unrelated GUI application was
started or restarted; the branch remains uncommitted and unpushed.

## 2026-08-14 — final verification after keyboard-release hardening

The final shared state preserves live child keyboard routing after
`wl_seat.release`, tears down stale seat-level IME focus only when the final
keyboard owner is released, and suppresses host v1 activation traffic after a
parent seat has been destroyed. The focused keyboard/text-input regressions
all pass.

Latest direct verification from the review worktree:

- `nix develop /home/kkimdev/workspaces/openstd/monorepo -c cargo test
  --workspace --all-targets -- --test-threads=1`: Sommelier 290, GUI sample 11,
  and Wayland codegen 5 passed.
- `cargo check --workspace --all-targets` passed.
- Strict Clippy (`cargo clippy --workspace --all-targets -- -D warnings`)
  passed.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- `bun run verify` remains limited by the unrelated monorepo failures
  documented in `log.local.md`.
- No service, rebuilt binary, Orca process, or unrelated GUI application was
  started or restarted; no commit or push was performed.

## 2026-08-14 — fresh audit fixes and final verification

The final input audit fixed stale per-keyboard state after a mapped old-surface
leave, preserving another keyboard's focus on the same seat. The protocol audit
changed unknown guest sender requests from silent drops to connection-fatal
protocol errors with FD cleanup. Both fixes have regression tests:
`mapped_stale_leave_clears_old_keyboard_state_without_disturbing_new_focus`
and `unknown_client_sender_is_a_protocol_error`.

The compositor/SHM/dmabuf/VirtWL audit found no additional confirmed bug.

Current verification: Sommelier 292 tests, `sommelier_test_gui` 11 tests, and
`wayland_codegen` 5 tests passed; workspace check, strict Clippy, formatting,
and `git diff --check` passed. `bun run verify` reported 2473 assertions and
10 unrelated monorepo failures, recorded in `log.local.md`.

No runtime or GUI process was restarted. The branch remains uncommitted and
unpushed.

## Final bounded re-audit

The fresh compositor/SHM/dmabuf/VirtWL and input/protocol audits found no
additional reproducible defect beyond the two already fixed regressions:
mapped stale keyboard leave state and unknown guest sender requests. The
role/window-less surface commit difference remains unconfirmed because this
fork has no matching window-role model or integration reproducer.

Final review-worktree verification:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 292,
  `sommelier_test_gui` 11, and `wayland_codegen` 5 passed.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify`: 2473 assertions passed; 10 existing monorepo-wide
  failures remain unrelated to this worktree (nested Biome configuration,
  missing Slidev directory, root hygiene/prohibited-file/absolute-path
  findings, and existing shebang/executable-bit issues).

No runtime or GUI process was restarted. The review branch remains
uncommitted and unpushed.

## 2026-08-14 — bounded audit completion

- Fresh ChromiumOS parity audits found no additional confirmed compositor,
  SHM/dmabuf, keyboard/IME, registry, object-lifetime, or FD defects.
- The role/window-less surface commit difference and a theoretical
  pending-surface keyboard leave cleanup gap remain unconfirmed because this
  fork has no matching role model or reachable reproducer.
- Final verification: Sommelier 292 tests, GUI sample 11 tests, and codegen 5
  tests passed; workspace check, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify` produced 2473 passes and 10 unrelated monorepo failures,
  recorded in `log.local.md`.

## 2026-08-14 — text-input global removal lifecycle fix

The input audit reproduced a lifecycle failure after host
`zwp_text_input_manager_v1`/`zcr_text_input_extension_v1` global removal:
the reset path retired the shared internal host objects, so already-bound
synthetic v3 managers could no longer create text-input children. ChromiumOS
keeps already-bound manager resources alive; only the global advertisement is
removed. The Rust reset now retains those host IDs and dispatch registrations
until connection teardown.

Regression `removed_text_input_globals_keep_bound_manager_usable`
failed before the change and passes after it, covering both manager and
extension host traffic. The focused test and formatting pass.

Final verification after the lifetime fix:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 294,
  GUI sample 11, codegen 5 passed.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- `bun run verify`: 2473 assertions passed; 10 pre-existing monorepo failures
  remain unrelated to this worktree.

## 2026-08-14 — final review-worktree verification

The current review worktree is fully verified after the final audit and
documentation correction:

- Rust tests: Sommelier 294, `sommelier_test_gui` 11, and `wayland_codegen` 5
  passed with serialized workspace execution.
- `cargo check --workspace --all-targets`, strict Clippy, formatting, and
  `git diff --check` passed.
- `bun run verify`: 2473 assertions passed; 10 existing monorepo-wide
  failures remain unrelated (nested Biome roots, missing Slidev directory,
  root hygiene/prohibited-file/absolute-path findings, and shebang/permission
  findings).
- No runtime or GUI process was started or restarted. The branch is still
  uncommitted and unpushed.

## 2026-08-14 — final audit handoff

The compositor, input, and protocol re-audits found no additional confirmed,
reachable production bug. The remaining role-less surface commit difference
and pending-surface keyboard-leave concern are unconfirmed and were left
unchanged.

Latest serialized verification:

- Sommelier: 294 tests passed.
- `sommelier_test_gui`: 11 tests passed.
- `wayland_codegen`: 5 tests passed.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- `bun run verify`: 2473 assertions passed; 10 unrelated monorepo failures
  remain as documented in `log.local.md`.

No runtime service, rebuilt Sommelier, Orca process, or unrelated GUI process
was started or restarted. The review branch remains uncommitted and unpushed
until the user invokes `/commit`.

## 2026-08-14 — line-level audit closure

The final line-level comparison against ChromiumOS Sommelier found no new
reachable defect in compositor damage/SHM, dmabuf feedback/import, VirtWL
transport, keyboard/IME lifecycle, registry generations, object lifetimes, or
FD ownership. The temporary direct-handler keyboard test was removed because
generated dispatch rejects the stale surface before the handler can be called.

The known `--gpu-accel` GBM/SHM architecture remains explicitly marked broken
by the CLI and is outside the supported VirtWL branch; it was documented rather
than changed without an end-to-end host import design and regression fixture.

`cargo fmt --all -- --check` and `git diff --check` pass after the audit. No
runtime service, rebuilt Sommelier, Orca process, or unrelated GUI process was
started or restarted, and no commit or push was performed.

## 2026-08-14 — serialized verification after audit closure

- Rust workspace tests passed: Sommelier 294, test GUI 11, codegen 5.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- `bun run verify` produced 2473 passes and the same 10 pre-existing
  monorepo-wide failures; none reference this review worktree.

## 2026-08-14 — post-fix verification

- The v3 IME content-hint regression was fixed and covered by a focused
  assertion: `auto_correction (0x2)` now maps to `autocorrect_on (1 << 2)`,
  not `spellcheck_on (1 << 4)`.
- The serialized Rust workspace matrix passed: Sommelier 294, test GUI 11,
  and codegen 5 tests.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- Root verification again reported 2473 passes and 10 unrelated,
  pre-existing monorepo failures documented in `log.local.md`.

## 2026-08-14 — final serialized verification after keysym fix

- The full Rust workspace matrix passed: Sommelier 295 tests, sample GUI 11,
  and codegen 5.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- `bun run verify` produced 2473 passes and the same 10 unrelated,
  pre-existing monorepo failures documented in `log.local.md`.
- No runtime service or GUI process was restarted; no commit or push was made.

## 2026-08-14 — focused keyboard keysym routing correction

The input audit reproduced a multi-keyboard focus bug: host-originated
`keysym` events were selected by guest keyboard ID alone, so an older keyboard
object could receive text after another keyboard on the same seat had taken
focus. The new regression `keysym_prefers_keyboard_with_current_surface_focus`
failed before the fix and passes after `keyboard_for_keysym` prioritizes the
keyboard whose tracked surface matches `active_surface_for_seat`.

The deterministic IME Backspace fallback remains in place for seats without a
focused keyboard mapping. Sommelier tests now pass 295/295, with formatting and
`git diff --check` clean. No runtime service or GUI process was restarted, and
the branch remains uncommitted and unpushed.

## 2026-08-14 — SHM pending-attach and invalid-bind hardening

The compositor audit found that destroying a guest `wl_buffer` after attach but
before commit discarded the local SHM backing, even though the host had already
received the attach. The new regression
`destroyed_pending_shm_attach_survives_until_commit` failed before the fix and
passes after pending surface references are treated as compositor use.

The protocol audit also found invalid `wl_registry.bind` requests were silently
dropped. They now produce a fatal `wl_display.error` and terminate the session
after the diagnostic is flushed, matching ChromiumOS behavior. Focused
compositor, SHM, and registry suites pass; no runtime or GUI process was
restarted and no commit or push was performed.

## 2026-08-14 — serialized final verification after protocol integration

The proxy FD-ambiguity regression that failed once in an unconstrained
single-package run passes under the required serialized test mode. The final
workspace matrix is green:

- Sommelier: 298 tests passed.
- `sommelier_test_gui`: 11 tests passed.
- `wayland_codegen`: 5 tests passed.
- Workspace check, strict Clippy, formatting, and `git diff --check` passed.
- Root `bun run verify`: 2473 assertions passed, with the same 10 unrelated
  monorepo failures documented in `log.local.md`.

The review branch remains uncommitted and unpushed. No runtime service, proxy,
Orca process, or unrelated GUI process was restarted.

## 2026-08-14 — FD cleanup test parallelism hardening

Repeated parallel package runs showed that several cleanup tests asserted
`EBADF` on fixed high descriptor numbers. Other tests could legally reuse those
numbers after the close, producing false failures. The tests now verify that
the original `/proc/self/fd` resource target is gone instead. This is
test-only hardening; production FD cleanup behavior is unchanged. Eight
parallel package runs passed after the adjustment. The subsequent serialized
workspace matrix also passed: Sommelier 298, sample GUI 11, and codegen 5,
along with workspace check, strict Clippy, formatting, and `git diff --check`.

## 2026-08-14 — input/compositor audit handoff

The v3 content-hint mapping was checked against both vendored XML files:
v3 bit `0x2` is `spellcheck`, and the extension's `spellcheck_on` flag is
`1 << 4`; `autocorrect_on` is a separate extension flag at `1 << 2`. The
current implementation and regression assertion use `1 << 4`, so any older
note describing v3 bit `0x2` as autocorrection is stale and should not be
used as implementation guidance.

ChromiumOS comparison of compositor damage/viewport handling, SHM buffer
lifetime, keyboard focus state, and ordered FD handling found no additional
confirmed reachable issue. A possible multi-keyboard same-seat edge remains
unproven: synthetic Backspace chooses a seat-level keyboard while keysym
events prefer the focused keyboard. No runtime service or GUI process was
started or restarted, and no commit or push was made.

## 2026-08-14 — protocol-validation hardening

Fresh protocol review identified silent drops for malformed requests. The
proxy now emits fatal `wl_display.error` events for invalid surface
offset/scale/transform, bad or stale viewports, duplicate viewport objects,
malformed SHM pool/buffer requests, and invalid linux-dmabuf plane/create
metadata. Diagnostics are bounded so the error event itself cannot exceed
Wayland's message limit.

Regression tests cover the declared protocol error codes, and the serialized
workspace matrix passes: Sommelier 304, GUI 11, codegen 5. Workspace check,
strict Clippy, formatting, and `git diff --check` also pass. No runtime or
GUI process was restarted; changes remain uncommitted and unpushed.

Root `bun run verify` reports 2473 passing assertions and the same 10
pre-existing monorepo failures (nested Biome roots, missing Slidev path, root
hygiene/prohibited-file/absolute-path findings, and existing shebang/permission
findings); none references the review worktree.

## 2026-08-14 — bounded compositor/SHM/dmabuf/allocator re-audit

The current Rust compositor, SHM, linux-dmabuf, and allocator implementations
were compared with the local ChromiumOS Sommelier counterparts. No additional
confirmed reachable bug was found. The pending-attach SHM lifetime fix remains
covered by `destroyed_pending_shm_attach_survives_until_commit`
(`compositor.rs:713-835`, `shm.rs:1048-1137`).

One protocol follow-up remains unmodified: `wp_viewport.set_source` accepts
fractional source dimensions with no destination
(`compositor.rs:884-898`), despite `viewporter.xml:90-96` requiring integer
dimensions and `bad_size=1` at state application. This was not classified as a
confirmed runtime regression in the bounded pass. No source code, runtime
process, commit, or push was changed.

## 2026-08-14 — fractional viewport source validation

The compositor now validates the committed double-buffered viewport state:
fractional source dimensions without a destination queue
`wp_viewport.bad_size=1` and prevent forwarding the invalid `wl_surface.commit`.
The new regression test passes alongside the existing compositor validation
tests. Fresh compositor, input, and protocol audits found no other confirmed
reachable bug.

## 2026-08-14 — post-fix verification

The complete Rust workspace matrix is green: Sommelier 305 tests, sample GUI
11 tests, and codegen 5 tests. Workspace check, strict Clippy, formatting, and
diff checks also pass. Root verification remains at 2473 passes with the same
10 unrelated pre-existing monorepo failures. No runtime service or GUI process
was restarted; the branch remains uncommitted and unpushed.

## 2026-08-14 — deep audit follow-ups

Parallel compositor, input, and transport audits found three actionable
lifecycle issues. `wl_shm_pool.resize` now reports `invalid_fd=2` when
`mremap` fails. Valid keymap replacement preserves outstanding physical
press/release pairing, and text-input-v3 requests are ignored after `leave`
until the next `enter`. Duplicate same-focus keyboard enters are consumed
locally like ChromiumOS.

The hidden `--gpu-accel` option was compared with ChromiumOS' compositor path:
our GBM fallback was submitting a PRIME fd through `wl_shm.create_pool`, while
ChromiumOS uses linux-dmabuf for that allocation. The option now fails fast
with a clear message instead of exposing an invalid path. VirtWL remains the
supported allocation path.

The serialized Rust workspace matrix passes with 308 Sommelier tests, 11
sample GUI tests, and 5 codegen tests; check, strict Clippy, formatting, and
diff checks are clean. No runtime process, commit, or push was performed.

## 2026-08-14 — sample GUI Korean font and VS Code verification

The sample GUI was rebuilt and launched through the existing `wayland-2`
session. Manual IME traffic reached the app, but Korean glyphs initially
rendered as tofu boxes because egui's bundled Latin fonts have no Hangul
coverage. The app now discovers a Korean-capable Noto/Nanum system font (or
uses `SOMMELIER_TEST_GUI_FONT`) and registers it as an egui fallback.

The rebuilt app logged `NanumGothic-Regular.ttf` registration and passed the
manual smoke run. Its focused test suite passes 12/12; the complete workspace
passes 308 Sommelier, 12 sample GUI, and 5 codegen tests, plus check, strict
Clippy, formatting, and diff checks. `code --reuse-window` opened the review
worktree in VS Code, and `code --status` confirmed the window with Korean
input in the active editor. No primary service or unrelated GUI process was
restarted.

## 2026-08-14 — rebase onto upstream virtwl

GitHub reported the fork's branch one commit behind
`google/sommelier-rs:virtwl`. The upstream-only commit was `922575c`, which
adds the virtwl systemd user service. All eleven local commits were rebased
onto that upstream branch without conflicts. The serialized Rust workspace
matrix remains green: 308 Sommelier tests, 12 sample-GUI tests, 5 codegen
tests, plus check, strict Clippy, formatting, and diff checks.

## 2026-08-14 — GitHub Actions and Nix packaging

The personal fork is configured around the sole `virtwl` branch. GitHub-hosted
Ubuntu 24.04 runners now build x86_64 and aarch64 in both CI and tagged
release workflows. Release jobs verify the Cargo version, collect both
architecture artifacts, and publish a single checksum file after the matrix
completes. Build jobs have read-only contents permissions; only the release
job can write releases. Dependabot is enabled for Cargo and GitHub Actions.

Repository settings were verified: Actions are enabled with read-only default
workflow permissions, vulnerability alerts and automated security fixes are
enabled, and merged branches are deleted automatically. No self-hosted runner
is registered; the workflow uses GitHub-hosted runners by design.

`actionlint` passes. The Nix flake evaluates for both supported systems, the
verified upstream binary derivation builds, and the source derivation builds
after serializing only its compositor test binary in the Nix check hook. The
first Nix attempt exposed workspace test-process FD/codegen races; the final
derivation avoids those package-check races while the full three-package test
matrix remains in CI. The binary derivation stays on upstream `virtwl-v0.2.0`
until a personal `virtwl-v0.2.1` release supplies new hashes.

The final monorepo `bun run verify` completed with 2473 passing assertions and
the same 10 unrelated baseline failures: nested Biome roots, a missing Slidev
path, root hygiene/prohibited-file/absolute-path findings, and pre-existing
shebang or executable-bit findings. None points to the Sommelier or Nix
changes.

## 2026-08-21 — Historical GTK ARC metadata and direct left/right placement

At this earlier point, the window-placement path kept the ARC application ID
when a GTK client sent `gtk_surface1.set_dbus_properties`. That request can
arrive after `xdg_toplevel.set_app_id`; previously it overwrote the ARC metadata
with the normal Crostini namespace, so ChromeOS rejected arbitrary Aura bounds
and the window returned to its old `800x600` geometry. A compatibility ARC
task-form ID was shared by the GTK and Aura paths, while the host XDG role
remained in the normal guest namespace. The previously attempted per-surface
ARC session ID was removed after it restarted the host compositor even with
geometry disabled.
At that point the workaround was still selected by
`SOMMELIER_WINDOW_BOUNDS_AS_ARC`; the current implementation uses explicit CLI
policy and geometry options instead.

An isolated self-parent probe was also available through
`SOMMELIER_WINDOW_BOUNDS_SELF_PARENT=1` at that stage.
It sends `zaura_surface.set_parent` with the same surface as both child and
parent, records a host-stream barrier, and intentionally preserves the current
surface size. The proxy now converts target screen coordinates to
parent-content-relative coordinates using the latest Aura origin and consumes
shortcuts received before that origin is available. This still is not a
placement backend: Exo explicitly rejects the transient cycle, and the
resulting movement is an unsupported side effect that can remain
unpredictable or destabilize a custom host build. It is independent of the ARC
policy flag, but the ARC-session geometry backend wins if both flags are set.
It must only be used with a uniquely named test display and never on the shared
system Sommelier instance.

All mutable placement state now lives in
`sommelier/src/state/window_placement/mod.rs`: one backend mode is resolved at
startup, Aura surface/toplevel associations are one-to-one, one per-toplevel
record owns the authoritative/predicted origin, and one barrier registry owns
callback supersession and retirement. Compositor, keyboard, GTK, registry, and
callback handlers use that API instead of updating parallel maps on `Context`.
Fallible state mutators are `#[must_use]`, and debug builds validate the
reverse-map and teardown invariants after each mutation.

Alt+A and Alt+D now use the same direct
`unset fullscreen/maximized/snap → zaura_toplevel.set_window_bounds → sync`
sequence as the other seven layouts. They no longer invoke ChromeOS snap
animations. A regression test asserts that neither snap opcode is queued.

Verification:

- `cargo fmt --all -- --check`, `cargo check -p sommelier`, and
  `cargo clippy -p sommelier --all-targets -- -D warnings` pass.
- `cargo test -p sommelier --all-targets -- --test-threads=1`: 555 passed,
  1 ignored; the ignored GUI smoke test requires a live Wayland compositor.
- `cargo build --release -p sommelier -p sommelier-test-gui` produced the
  tested binaries at `target/release/`.
- The repository-wide `bun run verify` currently reports 1,853 passing
  assertions and 9 pre-existing monorepo failures (nested Biome configuration,
  missing Typst/Astro assets, and existing hygiene/shebang findings); none
  points to this Sommelier worktree.
- The isolated release proxy was restarted on
  `/run/user/1000/wayland-codex-ghostty-rewrite`; its Aura metadata path records
  GTK and Ghostty application IDs in the unique
  `org.chromium.arc.session.<id>` namespace, while each host XDG role remains
  in the normal `org.chromium.guest_os.<vm>.wayland.<app>` namespace.
- A separate release proxy on
  `/run/user/1000/wayland-codex-self-parent` accepted the test GUI and
  delivered all nine Alt shortcuts. The self-parent requests moved the window
  by an unexpected parent-content-relative offset while preserving its size.
  The proxy and GUI were stopped after the probe; subsequent host-UI
  instability was reported, so this backend must remain disabled by default
  and is not a production placement mechanism.
- A parallel test run also exposed two pre-existing linux-dmabuf descriptor tests
  as flaky; each passed alone and in the serialized full suite.

## 2026-08-21 — Inline shortcut configuration and SIGHUP reload

The nine hardcoded Alt bindings are now represented by an opt-in TOML file.
Each binding contains its chord, `window.place` action, and normalized
`[x, y, width, height]` rectangle inline; there are no named presets. The
policy and geometry axes are independent:

```text
--window-host-policy=guest|arc
--window-geometry-method=none|bounds|self-parent
--window-shortcuts-config PATH
```

The default is `guest + self-parent`, with no config file read and no shortcut
interception. A CLI-selected file is validated before accepting clients.
Sending `SIGHUP` reloads that same path atomically into all current and future
client contexts; malformed or conflicting reloads retain the last-known-good
generation. `SOMMELIER_ACCELERATORS` conflicts are hard errors. The
self-parent method remained experimental and position-only in that
implementation snapshot.

Verification for this implementation:

- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets`, and
  `cargo clippy --workspace --all-targets -- -D warnings` pass.
- `cargo test --workspace --all-targets -- --test-threads=1`: 566 passed,
  1 ignored; the ignored GUI smoke test requires a live Wayland compositor.
- `bun run verify` was executed from the enclosing monorepo: 1,853 assertions
  passed and 9 unrelated baseline checks failed (nested Biome configuration,
  missing Typst/Astro assets, and existing hygiene/shebang findings).

## 2026-08-21 — placement-state ownership review

The follow-up refactor makes `WindowPlacementState` the single owner for all
placement-adjacent lifecycle metadata: the complete Aura-shell generation,
ordered output records and work-area conversion, VM/application-ID formatting,
XDG/Aura associations, GTK shell/surface links, capability barriers, origin
predictions, and placement barriers. `Context` no longer exposes parallel
placement maps or metadata fields to handlers. GTK surface IDs are returned in
stable order so startup-token updates do not depend on `HashSet` iteration.
The reverse-link assertions remain in the owner and every mutator preserves
them.

Verification from the review worktree:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 571
  passed, 1 ignored; sample GUI 12 passed; Wayland codegen 6 passed; the GUI
  smoke test is also ignored because it requires a live compositor.
- `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `RUSTDOCFLAGS='-D warnings' cargo doc -p sommelier --no-deps` passed with no
  broken intra-documentation links.
- `cargo fmt --all -- --check`, `cargo build --release -p sommelier
  -p sommelier-test-gui`, and `git diff --check` passed.

## 2026-08-22 — Test-fixture allocator probe removal

The exhaustive render-buffer state tests were unexpectedly dominated by their
fixture setup rather than by the state machine. `Context::new_for_test()` was
calling the production constructor, which probes every DRM render node and
constructs a GBM allocator for every generated trace. The two exhaustive
tests create 15,625 and 46,656 contexts respectively, so this repeated
hardware probing made the suite appear hung and produced a test-runner timeout
event.

The production constructor still performs the normal allocator probe. The
test-only constructor now injects `None` for the allocator through a shared
constructor helper; these unit tests exercise ID mapping and render-buffer
lifecycle, not GBM allocation. No transition count or assertion was removed.
The previously slow `host_buffer_use_matches_all_short_transition_sequences`
test now completes in about 2.3 seconds, and the full workspace suite completes
with 613 Sommelier tests passed, one ignored, 12 sample-GUI tests passed, and
six Wayland-codegen tests passed.

## 2026-08-22 — Final placement validation and timing

The wire adapter now revalidates the prepared plan's XDG/Aura associations and
the three host object interfaces immediately before queueing. A stale
`zaura_surface` that is already pending destruction is rejected without
publishing a wire request or registering a placement barrier. Keyboard fixtures
now register the same XDG interface metadata as production dispatch, so the
regression tests exercise the real lifecycle invariant instead of bypassing it.

The final serialized workspace run passed 615 Sommelier tests, one ignored, 12
sample-GUI tests, and six Wayland-codegen tests. The Sommelier test body took
about 7.8 seconds; the longest exhaustive state-model test took about 2.3
seconds. `cargo check --workspace --all-targets`, strict Clippy, formatting,
`git diff --check`, and the release build also passed. For local iteration, the
same matrix is stable with `--test-threads=4` and completes in about 3.2
seconds of test execution.

## 2026-08-22 — placement runtime and request-plan ownership cleanup

The placement refactor now has one process-wide
`WindowPlacementRuntime` owned by
`sommelier/src/state/window_placement/runtime.rs`. It contains the selected backend,
the reloadable shortcut-generation handle, host accelerator policy, VM
namespace, and process-shared ARC task allocator. Each connection receives an
`Arc` reference to that runtime; its `WindowPlacementState` owns only
connection-local links, origins, outputs, GTK associations, and barriers.
`Context` and `ProxyRuntime` no longer carry parallel copies of placement
configuration.

Keyboard handling now asks the state owner for a validated
`WindowPlacementPlan`. The plan selects direct bounds versus the
bounds-first/self-parent sequence, calculates relative coordinates, and
allocates transient ARC identity when applicable. The keyboard handler
serializes that plan, queues the barrier, and then asks the state owner to
commit the origin prediction; it cannot independently choose a geometry
backend or mutate placement maps.

The public CLI is intentionally small: the native `set-parent` backend is the
default mode, but placement shortcuts remain disabled until
`--window-shortcuts-config PATH` is supplied. The older
`--window-host-policy`, `--window-geometry-method`, and
`--window-arc-id-lifetime` switches remain hidden compatibility options for
development experiments. SIGHUP reload is implemented by the placement
runtime and returns a typed result while preserving the last valid generation
on errors.

## 2026-08-22 — Historical self-parent resize follow-up

The earlier self-parent experiment intentionally sent only
`zaura_surface.set_parent`, so it moved the window while preserving its old
size. The first resize follow-up kept that custom position probe but completed
each shortcut with the same screen-coordinate
`zaura_toplevel.set_window_bounds` request used by the direct backend:

```text
unset fullscreen/maximized/snap
set_parent(relative_x, relative_y)
set_window_bounds(screen_x, screen_y, width, height, output)
sync
```

The bounds request is necessary because `set_parent` has no size arguments.
That ordering was later found insufficient on the custom host: the position
side effect worked, but the post-parent bounds request was rejected.

## 2026-08-22 — Historical self-parent resize authorization correction

This section records an intermediate experiment and is not the current
backend contract.

The first implementation of the sequence above still selected the Guest OS
application policy for the `set-parent` backend. On the custom host that
allowed the self-parent side effect to move the window, but ChromeOS rejected
the accompanying `set_window_bounds` size change; `zaura_toplevel.configure`
continued to report the original `800x600` bounds. This was a policy failure,
not a wire-encoding failure.

The named `set-parent` backend now selects the native Guest OS identity
together with the self-parent probe. The host XDG role remains the normal
`org.chromium.guest_os.<vm>.wayland.<app>` identity. This preserves
shelf/taskbar matching; persistent ARC task metadata authorized the size
request but produced generic or missing shelf icons in manual testing. The
transient ARC variant also caused an IME focus reset after the first shortcut,
while `.session.*` metadata had already been observed to destabilize the host
compositor. The Guest + self-parent path was therefore treated as a
position-only experiment at this point, with resize authorization left as an
explicit host-specific limitation to verify. It was later superseded by the
native XDG resize handshake described below; the current `set-parent` backend
does not rely on this intermediate direct-Aura conclusion.

## 2026-08-22 — Self-parent bounds-first resize fix (superseded)

The first production attempt cleared compositor-owned state, resized the
top-level window at its last authoritative screen origin with a direct Aura
bounds request, and only then used the self-parent probe for the target
position:

```text
unset fullscreen/maximized/snap
set_window_bounds(current_x, current_y, width, height, output)
set_parent(relative_x, relative_y)
sync
set_parent(NULL, 0, 0)   # queued after sync.done
```

The regression test captured the wire order and requested dimensions. On the
custom host this direct Aura resize remained policy-sensitive, so the current
implementation supersedes that size phase with the native XDG configure/commit
handshake described below. The historical wire sequence is retained here
because it explains why the companion direct-bounds request was removed.

## 2026-08-23 — Native XDG resize before self-parent position

The current `set-parent` backend keeps the Guest OS identity and applies size
through the host's ordinary XDG surface state:

```text
synthetic guest xdg_toplevel.configure(width, height)
host xdg_surface.set_window_geometry(0, 0, width, height)
guest ack_configure + wl_surface.commit
host zaura_toplevel.configure(width, height)
set_parent(self, target - authoritative_origin)
sync.done -> retain self-parent; queue an ordered IME-refresh barrier
```

This avoids `ChromeSecurityDelegate::CanSetBounds()` for the size phase while
retaining the native shelf identity. The nullable `set_parent(NULL, 0, 0)`
request is intentionally not sent: on the tested custom host it starts a new
focus/activation animation and can invalidate IME state. The state machine
suppresses stale configures and intermediate origins while a generation is
active. If the host omits the final target `origin_change`, the ordered cleanup
barrier treats the requested target as the best available baseline and keeps
any deferred shortcut retryable. After completion, a short-lived origin guard
ignores late focus/animation frames until the next explicit placement starts;
this prevents the earlier `z -> a -> a` lower-right drift.

## 2026-08-21 — Historical preserve the native XDG identity in ARC bounds mode

The earlier ARC bounds workaround changed only the Aura application identity.
`xdg_toplevel.set_app_id` continues to use the normal guest namespace, while
`zaura_surface.set_application_id` and GTK Aura metadata use the per-surface
`org.chromium.arc.session.<id>` identity. This matches the established
ChromiumOS Sommelier split and avoids routing the host XDG role through ARC
restore/task classification.

The regression test
`handler::compositor::tests::arc_policy_keeps_xdg_app_id_in_guest_namespace`
failed against the previous rewrite and passes after the split.
- The enclosing monorepo `bun run verify` completed with 1853 passing
  assertions and 9 unrelated baseline failures (nested Biome configuration,
  missing Typst assets, missing Astro/Slidev tooling or paths, and existing
  hygiene/permission findings).

## 2026-08-22 — Historical transient ARC identity experiment

This experiment kept the native Guest OS application ID on the
Aura surface during normal operation, including XDG and GTK metadata updates.
The allocated numeric `org.chromium.arc.<task_id>` value is retained only as a
placement capability. For each bounds shortcut, Sommelier queues one ordered
host sequence:

```text
set_application_id(ARC task ID)
set_window_bounds(...)
wl_display.sync(...)
sync.done -> set_application_id(native Guest OS ID)
```

The native identity is deliberately queued only from the `sync.done` callback,
so the restore cannot race the bounds request or be mistaken for part of the
pre-barrier wire sequence. This preserves the earlier task-ID allocator and
the `/dev/wl0` observations while avoiding a persistent ARC shelf/icon
identity. Unit coverage asserts the exact sequence and verifies that the
native ID is restored after every placement.

It is retained as a comparison path, but is not the named `set-parent`
backend: changing the Aura identity around a shortcut caused an IME focus
reset on the custom host. The current `set-parent` backend keeps the native
Guest OS identity, uses the ordinary XDG configure/commit handshake for size,
and uses `set_parent` only for its position probe.

## 2026-08-22 — Historical transient ARC nullable-parent follow-up

The transient comparison path now tests a complete post-barrier cleanup:

```text
set_application_id(ARC task ID)
set_window_bounds(...)
wl_display.sync
sync.done -> set_parent(NULL, 0, 0)
sync.done -> set_application_id(native Guest OS ID)
```

This explicitly releases any parent relationship before returning to the
native Guest OS identity. The restore uses the latest native ID recorded for
the surface at barrier completion, rather than a stale placement-time copy.
The order is covered by unit tests, and the path requires `zaura_surface`
version 5 or newer: v2 provides nullable `set_parent`, while v5 is also
needed for the temporary `set_application_id` update. It remains experimental
because
the custom host may move the window again or reset IME focus when the nullable
parent and application identity are changed. Runtime verification must use an
isolated `/dev/wl0` proxy; the primary Sommelier instance is not a test target.

## 2026-08-21 — bidirectional placement-link ownership

The placement-state follow-up now stores every guest/host association in a
private `BidirectionalLinks` value. Aura-surface, Aura-toplevel, XDG-surface,
and XDG-toplevel maps are inserted and removed atomically, reject conflicting
rebindings without mutating live state, and share one consistency invariant.
Output mode and scale updates also use one state-owned insertion path.
`SOMMELIER_VM_IDENTIFIER` resolution is performed when the placement state is
constructed, keeping application-ID namespace ownership out of `Context`.
The wl-surface teardown API is named
`take_aura_surface_for_wl_surface` to make its host-side ownership explicit.
Placement callbacks are retained and superseded through a private
`PlacementBarrierRegistry`, so stale completions cannot clear a newer active
barrier. Aura-shell binding generations also reject live replacement until the
old global has been retired. Output records are retired at `wl_output.release`
and delayed mode/scale events cannot recreate them. The release handler resolves
the guest output ID back to its distinct host ID before retiring that record;
the regression fixture uses different guest/host IDs to cover this boundary.
An output `global_remove` does not retire an already-bound output object:
Wayland keeps that object valid until the client releases it, so placement state
follows the bound-object lifetime rather than the advertisement lifetime.
Failed output binds roll back both the guest/host mapping and the placement
record before reporting the protocol failure.
The process-wide ARC task allocator now reserves one high numeric block with a
shared `flock` file under `$XDG_RUNTIME_DIR/sommelier/arc-task-blocks`. Every
surface consumes the next ID in that block, while independent Sommelier
instances contend on the same block files. The lock file remains after exit;
the kernel releases the lock when the owning descriptor closes, avoiding an
unlink/recreate inode race.

Regression coverage includes atomic conflict rejection and both-direction
teardown; the VM namespace test derives its expected value from the active
environment so it remains deterministic in configured CI shells.

The task-ID research is preserved in
`sommelier/docs/ARC_TASK_AND_SESSION_IDS.md`: real ARC task IDs come from
Android's per-user `PER_USER_RANGE` allocator, while the current
`2,000,000,000..2,147,483,646` range is only a best-effort fabricated-task
pool coordinated between local Sommelier instances. The exact
`org.chromium.arc.2147483647` value remains documented as the PR #2/custom-host
compatibility sentinel and is excluded from the allocator. The per-surface
`.session.*` allocator and its PID/serial formula are also retained there as a
rejected experiment, including the `/dev/wl0` host-compositor restart observed
with `arc + none`.

Final verification before the identity-map consolidation:

- `cargo test --workspace --all-targets -- --test-threads=1`: Sommelier 618
  passed, 1 ignored; sample GUI 12 passed; Wayland codegen 6 passed; the GUI
  smoke test is ignored because it requires a live compositor.
- The parallel workspace run (`--test-threads=4`) also passed with 618 tests
  successful and one ignored.
- `cargo check --workspace --all-targets` and
  `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `cargo fmt --all -- --check`, `cargo build --release -p sommelier
  -p sommelier-test-gui`, and `git diff --check` passed.
- Placement plans now have an explicit lifecycle-pairing invariant check;
  malformed target/cleanup/identity combinations are rejected before any
  host wire message or barrier state is created.

## 2026-08-22 — identity ownership consolidation and timing recheck

The placement state now stores native and ARC application IDs for each guest
surface in one `SurfaceApplicationState` record instead of two independent
maps. The record is removed together with the Aura-surface association during
surface teardown, keeping the two identity lifetimes atomic. The duplicate
output-geometry tests were moved into `plan.rs`, which owns the pure
`OutputState` value and its invariants.

After that change the workspace matrix passed 617 Sommelier tests with one
ignored test, 12 sample-GUI tests, and six Wayland-codegen tests. The serialized
test bodies took 7.76 seconds (the same cached matrix took 2.67–2.71 seconds
with `--test-threads=4`; a cold compile adds roughly ten seconds). Two
descriptor-lifetime tests were hardened to compare `/proc/self/fd` targets,
eliminating false failures caused by numeric FD reuse in parallel tests.
`cargo check`,
strict Clippy, rustdoc with warnings denied, the release build, formatting, and
`git diff --check` all passed. No `/dev/wl0` runtime test was run in this pass.

## 2026-08-22 — Historical type-safe placement policy

The runtime configuration now represents placement as one closed policy value:

```text
Disabled
IdentityOnly
Bounds { host_policy, arc_id_lifetime }
SelfParent { host_policy }
```

Startup still accepts the hidden axis flags for compatibility, but
`WindowPlacementMode::from_axes` validates them once and collapses them into
that policy. This prevents combinations such as Guest + transient ARC,
self-parent + transient identity, or identity-only + transient cleanup from
reaching connection-local placement state. The public named backends construct
the same policy directly. This was a historical snapshot: at that point the
transient post-barrier `set_parent(NULL)` cleanup was still present. It was
later superseded by the persistent self-parent and native XDG resize sequence
documented in the later sections.

Verification for this pass:

- `cargo test -p sommelier --all-targets -- --test-threads=1`: 619 passed,
  0 failed, 1 ignored.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps`,
  `cargo build --release -p sommelier -p sommelier-test-gui`, and
  `git diff --check`: all passed.
- The enclosing monorepo `bun run verify` still reports 1,853 passing
  assertions and 9 unrelated baseline failures (nested Biome configuration,
  missing Typst/Astro assets or tooling, and existing hygiene/shebang findings).
- No `/dev/wl0` process was restarted for this structural-only pass.

## 2026-08-22 — opaque placement plans and test timing recheck

`WindowPlacementPlan` and `TransientArcIdentity` now expose no mutable fields
outside the placement-state module. Production plans can only be constructed by
`WindowPlacementState::prepare_placement`; the wire adapter receives read-only
accessors and cannot pair a target, temporary ARC identity, and barrier cleanup
from different surface generations. Test-only constructors keep malformed-plan
coverage without widening the production construction boundary.

The final cached verification matrix passed 619 Sommelier tests, one ignored,
12 sample-GUI tests, and six Wayland-codegen tests. The required serialized
run took 11.08 seconds of test body time (11.91 seconds including the harness).
The same matrix passed with four test threads in 2.85 seconds of test body time
(3.15 seconds including the harness); the README now documents that as the
faster local iteration command while CI/release remains serialized for
conservative descriptor ownership. Formatting, workspace check, strict Clippy,
rustdoc with warnings denied, release build, and whitespace validation remain
passing. No primary `/dev/wl0` process was restarted.

## 2026-08-22 — atomic XDG/Aura role teardown

XDG role destruction now has one state-owned transition for both the guest
`xdg_toplevel` association and its host `zaura_toplevel` child. The new
`XdgToplevelRelease` record is returned after the state owner has removed the
role link, Aura reverse link, origin/pending-origin state, and active placement
barrier. Surface- and `xdg_surface`-level cleanup use the same bulk transition,
so malformed destroy ordering cannot leave an Aura child or placement record
attached to a reused guest surface ID.

The wire adapter accepts only that returned release record and serializes the
optional host `zaura_toplevel.release`; it no longer mutates placement state.
Older callbacks remain callback-owned until their host `delete_id`, but their
cleanup is no longer active after role teardown. This keeps the state owner as
the single source of truth while preserving host object reservation semantics.

Focused state/compositor tests and the full Sommelier suite pass. The current
four-thread full run completes in about 3.0 seconds with 620 tests passing and
one ignored GUI smoke test. No `/dev/wl0` process was restarted for this
structural change.

## 2026-08-22 — lifecycle cardinality and parent-link audit

The surface teardown API now returns one optional `XdgToplevelRelease`, matching
Wayland's one-role-per-surface rule instead of allocating a collection that
could suggest multiple live roles. Aura-surface teardown removes native and ARC
application identity only after the expected host-side surface association is
found; a stale host ID therefore cannot erase a newer surface generation's
identity. A regression test covers the mismatched-ID path.

Role registration now enforces the protocol hierarchy in the placement state:
`xdg_toplevel` requires an existing `xdg_surface → wl_surface` link, and
`zaura_toplevel` requires the corresponding guest XDG role. Handler fixtures
were updated to register those parent links explicitly, so tests exercise the
same lifecycle ordering as production. The generated-dispatch post-hook for
`get_toplevel` remains a narrowly documented retry because the guest→host ID
mapping is installed after the handler; it now skips the retry if dispatch
returned a protocol error.

The four-thread workspace matrix passes 622 Sommelier tests with one ignored
live-VirtWL test, 12 sample-GUI tests, and six Wayland-codegen tests; the GUI
smoke test remains ignored because it needs a live compositor. This pass is
structural only and did not restart or exercise the primary `/dev/wl0`
compositor.

## 2026-08-22 — orphaned application-identity teardown

ARC task identity allocation can happen when an XDG role is created, before
any XDG/GTK metadata request creates the corresponding host `zaura_surface`.
Previously, destroying that surface without an Aura link left the native/ARC
record in the connection-local map until connection teardown. The
`wl_surface.destroy` handler now uses a separate state-owned orphan cleanup
operation when the expected Aura link is absent. The existing mismatched-host
ID guard remains unchanged, so a stale teardown cannot erase a newer
generation's identity. State and compositor regression tests cover both paths.

## 2026-08-22 — final serialized workspace verification

The final required serialized command,
`cargo test --workspace --all-targets -- --test-threads=1`, passed:

- Sommelier: 624 passed, 0 failed, 1 ignored (the live VirtWL test).
- `sommelier_test_gui`: 12 passed.
- `wayland_codegen`: 6 passed.
- The GUI smoke integration test remained ignored because it needs a live
  compositor.

The Sommelier test binary completed in 8.57 seconds after compilation. The
faster four-thread command remains available for local iteration; the
serialized command is retained for CI/release validation because it is the
conservative descriptor-ownership configuration. No `/dev/wl0` process was
restarted or exercised during this verification.

The enclosing monorepo `bun run verify` was also rerun: 1,853 assertions
passed and the same nine unrelated baseline checks failed. Those failures are
the nested Biome configuration, missing Typst diagrams/fonts, unavailable
Astro/Slidev tools or paths, and pre-existing root hygiene/shebang findings;
none reference the Sommelier worktree.

## 2026-08-22 — Historical shadow-owned surface teardown and fast test iteration

Surface identity teardown now resolves the host `wl_surface` from the
authoritative `ShadowTable` inside `WindowPlacementState`. The compositor
handler still uses its mapping to serialize `wl_surface.destroy`, but it no
longer supplies an independently assembled guest/host pair to placement state.
The mismatched-generation regression now exercises a stale shadow mapping and
confirms that the live Aura association and native/ARC identities remain
untouched.

The placement barrier cleanup path was explicitly ordered after
`wl_display.sync`: transient ARC placement queued nullable
`zaura_surface.set_parent(0, 0, 0)` first, then restored the latest native
application ID. This sequence is retained as historical evidence; the current
transient path does not unparent, and the current self-parent path retains its
relationship. Callback tests from this snapshot verify the old null-parent
ordering, superseded-barrier behavior, and released-surface guards.

After this change, the cached Sommelier package suite passes 625 tests with one
ignored live-VirtWL test. The serialized run takes about eight seconds; the
same suite with four test threads takes about 2.6 seconds. The faster command
is suitable for local iteration, while the serialized command remains the
conservative descriptor-ownership check. No `/dev/wl0` process was restarted.

## 2026-08-23 — remote-shell-v2 opt-in backend probe

The implementation now includes an explicit
`--window-placement-backend=remote-shell-v2` backend. It vendors the official
`remote-shell-unstable-v2.xml` request/event ordering, binds
`zcr_remote_shell_v2` only on the host side, and maps each guest XDG role to a
host `zcr_remote_surface_v2` role. Placement uses the protocol's
`set_bounds_in_output` request followed by `wl_surface.commit`; no ARC task
identity, Aura `set_window_bounds`, or self-parent relationship is involved.
Remote bounds/close events are converted into the guest XDG configure/close
stream, and app ID/title metadata is forwarded to the remote surface.

The backend is intentionally opt-in and still requires the explicit shortcut
config path. On the live custom `/dev/wl0` host, the remote-shell global was
not advertised. A Ghostty client therefore reached normal registry/keyboard
initialization but was rejected at its first XDG surface with:

```text
remote-shell: ... host did not provide a bound zcr_remote_shell_v2
```

The isolated proxy exited without restarting the primary Sommelier process.
This runtime result confirms the capability gate and safe failure path; it
does not yet validate remote-shell geometry, shelf icon, IME, or teardown.
A custom host/security delegate that advertises the global is required for that
next end-to-end test.

## 2026-08-24 — persistent self-parent cleanup

The first PR8 runtime path sent `zaura_surface.set_parent(NULL)` after every
self-parent placement. On the custom `/dev/wl0` host, Exo/Ash treated that as a
new activation transition: it emitted a long origin animation toward the
bottom-right and could leave focus/IME state unusable. The persistent experiment
in `/tmp/pr8-persist-runtime.log` remained stable across repeated shortcuts and
focus changes.

The production self-parent path now retains the self-parent relationship for
the proxy lifetime. Its `wl_display.sync` callback still orders the follow-up
host-IME refresh and transaction settlement, but never emits the nullable
unparent request. The older nullable-unparent implementation and its observed
runtime failure remain documented above as historical context.

Follow-up manual testing exposed a second limitation that must remain visible
in this record: a placement can appear correct initially, then a later host
focus transition can animate the window toward the lower-right and leave
subsequent shortcuts unhandled. This is not a normal Wayland lifecycle event;
it is another observable consequence of using a self-parent cycle as a
positioning probe. The placement reducer now ignores those late animation
coordinates instead of rebasing the next relative delta, and the cleanup
barrier uses the requested target as a liveness baseline when Aura omits
`origin_change`. This keeps repeated shortcuts retryable without claiming that
the host emitted geometry. Treat the backend as host-specific and experimental
until a supported bounds or remote-shell capability is available.

## 2026-08-24 — generation fallback verification

The omitted-`origin_change` liveness fallback now settles the requested target
after the ordered cleanup barrier. It preserves and promotes a deferred latest
shortcut instead of dropping it, and arms a short-lived guard after completion
so late focus/animation coordinates cannot rebase the next relative request.
The regression fixtures cover both fallback promotion and repeated-shortcut
deduplication.

The final serialized Sommelier verification passed:

- `cargo test -p sommelier --bin sommelier -- --test-threads=1`: Sommelier 716
  passed, 0 failed, 1 ignored (live VirtWL). This includes direct coverage for
  live transient-ARC restore completion followed by role teardown.
- The workspace check and strict all-target Clippy pass; sample GUI and Wayland
  codegen remain covered by the earlier serialized workspace run.
- `cargo fmt --check`, `cargo check --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `git diff --check` passed.

The enclosing monorepo `bun run verify` still has unrelated baseline failures:
1,853 assertions passed and 10 failed due to the nested Biome configuration,
missing Typst assets/fonts, unavailable Astro/Slidev tools or paths, and
pre-existing root hygiene/shebang findings (including the user's untracked
`rust_out`). No `/dev/wl0` process was restarted for this structural pass.
