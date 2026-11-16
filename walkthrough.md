# Sommelier-RS review walkthrough

This review compares the Rust proxy with ChromiumOS Sommelier behavior and
keeps every confirmed fix behind a regression test.

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

## 2026-08-21 — GTK ARC metadata and direct left/right placement

The window-placement path now keeps one per-surface ARC policy identity when a GTK
client sends `gtk_surface1.set_dbus_properties`. That request can arrive after
`xdg_toplevel.set_app_id`; previously it overwrote the ARC metadata with the normal
Crostini namespace, so ChromeOS rejected arbitrary Aura bounds and the window
returned to its old `800x600` geometry. The GTK and XDG paths share the generated
identity, while the host XDG role keeps its native guest namespace. The workaround
and its compositor-owned shortcuts are disabled unless the ARC policy is enabled;
the explicit shortcut configuration remains empty by default, so no chord is
consumed until a deployment opts into one.

Alt+A and Alt+D now use the same direct
`unset fullscreen/maximized/snap → zaura_toplevel.set_window_bounds → sync`
sequence as the other seven layouts. They no longer invoke ChromeOS snap
animations. A regression test asserts that neither snap opcode is queued.

Verification:

- `cargo fmt --all -- --check`, `cargo check -p sommelier`, and
  `cargo clippy -p sommelier --all-targets -- -D warnings` pass.
- `cargo test -p sommelier -- --test-threads=1`: 540 passed, 1 ignored.
- `cargo build --release -p sommelier` produced the tested binary at
  `target/release/sommelier`.
- The isolated release proxy was restarted on
  `/run/user/1000/wayland-codex-ghostty-rewrite`; its startup log records distinct
  per-surface ARC task-form policy identities for GTK and Ghostty while their
  host XDG roles retain the native guest namespace.
- A parallel test run also exposed two pre-existing linux-dmabuf descriptor tests
  as flaky; each passed alone and in the serialized full suite.

## 2026-09-12 — PR #9 review hardening

The review pass now allocates ARC task-form identities from a process-shared,
file-locked block. The allocator validates private runtime directories, keeps a
stable generation guard across lock-directory replacement, and opens range locks
relative to the validated directory descriptor. If the allocator cannot reserve
an unambiguous block, the ARC bounds policy fails closed. The `.session.*`
restore namespace is never fabricated.

Late `zaura_shell` binding now repairs already-bound outputs and XDG toplevels,
and XDG role metadata is recorded only after a valid `get_toplevel` dispatch.
`wl_output.release`, negative Aura insets, and delayed output/surface events are
also covered by lifecycle regressions.

Verification from the final review snapshot:

- Nix source derivation: release build and `cargo test -p sommelier --all-targets`
  passed 572 tests, with 1 live-device test ignored.
- Nix development shell: workspace check and strict Clippy passed; formatting
  and `git diff --check` are clean.
- Host-level Aura/Wayland compositor validation remains intentionally unrun.

## 2026-11-12 — PR #9 final-path review and fractional display scaling

Continued the saved review through surface commit, placement request ordering,
paired Aura/XDG configures, callback completion, and teardown. No additional
confirmed defect was found in the surface-commit or configure-barrier paths.

The output review reproduced a fractional-scaling defect: a native 3840x2160
output with device factor 1250, current Aura scale 1000, and rounded
`wl_output.scale = 2` produced 1920x1080 placement bounds instead of 3072x1728.
The handler-level regression failed before the fix and passed afterward.

`OutputState` now requires both Aura scale factors and combines their
thousandths with checked integer arithmetic before applying logical insets.
Only current scale entries update geometry; preferred-only and malformed zero
entries are ignored. Output changes retain the existing `wl_output.done`
publication boundary. Eight new regressions cover fractional and combined
scales, rotation/insets, incomplete metadata, stale output events, version-one
outputs, and forwarding both edges of a placement chord while metadata is
incomplete. Existing placement fixtures now supply complete scale metadata.

The eight existing PR commits were rebased without conflicts onto current
`personal/virtwl` at `df23d4a1686b18c5be65a615df7d73ae902ec53f` (v0.2.6).
`git range-diff` reports all eight patches unchanged. The rebased local head is
`a9e5ac0e7f2300240766045cdef5e06e879d59d5`; the previous head is retained at
`backup/pr9-core-before-2026-11-12-rebase`. The scaling fix and this review
record were committed and published on 2026-11-16 (see below).

Fresh validation on the rebased code and scaling fix:

- Locked workspace check and strict Clippy across all targets passed.
- Locked serial workspace tests: Sommelier 582 passed / 1 ignored, sample GUI
  12 passed, codegen 7 passed; the separate GUI smoke test remains ignored.
- Formatting and `git diff --check` passed.
- Nix source derivation: release build and package checks passed, including
  582 Sommelier tests. Explicit `?shallow=1` was required for this Git checkout.
- The normally ignored live VirtWL nonblocking-receive/sync test passed using
  the freshly built test executable.
- A separate direct-host registry probe created no surfaces and reported one
  3840x2160 output, integer scale 1, Aura current/device scales 1000/1000,
  bottom inset 48, and advertised Aura version 65 (bound at supported v38).

Validation logs, host metadata, and the saved pre-rebase patch were moved out
of `/tmp` to the untracked local evidence directory
`_local/evidence/pr9-review-2026-11-12/` in the primary checkout.

The live transport and metadata checks do not establish host placement
acceptance. Actual Aura window placement and multi-monitor behavior remain
unverified; this host currently exposes only one output. Legacy Aura scale
factors are quantized to thousandths, so reconstruction can differ slightly
from exact host logical dimensions at unusual scale settings. The running
primary Sommelier service was not replaced or restarted.

## 2026-11-16 — Publishing the rebased PR #9

- Committed the fractional-scaling fix as its own change, then extracted the
  stale-output guard and the `wl_output.done`-aware publish step shared by the
  four `zaura_output` handlers into `live_aura_output_host_id` and
  `publish_aura_output_update` (no behavior change).
- Re-ran validation on the final head in the monorepo development shell
  (`nix develop <monorepo> -c ...`): locked serial workspace tests report
  Sommelier 582 passed / 1 ignored, sample GUI 12, codegen 7; strict Clippy
  across all targets, `cargo fmt --check`, and `git diff --check` pass. The
  repository's own development shell lacks `libxkbcommon` at test runtime, so
  the test binary cannot start there.
- Force-pushed the rebased branch to `feature/window-placement-shortcuts-core`
  with a lease on the previous head `c2b5208`; PR #9 remains Draft pending
  host-level Aura placement and multi-monitor acceptance.

## 2026-11-16 — Independent review of PR #9

Three read-only reviewers covered the ARC task-ID allocator, compositor
placement/output state, and shortcut key handling. Confirmed findings were
fixed with regression tests (Sommelier 582 → 600 tests):

- ARC allocator: random start offset inside the locked block (IDs no longer
  coincide across separate `XDG_RUNTIME_DIR`s on the same block), corrupt
  guard markers are rewritten under the exclusive lock, owner/mode checks use
  the opened descriptor, and directory creation cannot chmod through a
  symlink. The dead post-lock identity check was removed. Exclusivity is
  documented as holding only within one runtime directory; blocks are never
  refilled and exhaustion fails safe.
- Shortcuts: ownership is arbitrated before placement, so a key already
  delivered through text input or IME recovery, or a duplicate press of a
  consumed chord, no longer triggers placement; IME recovery never replays a
  consumed shortcut key.
- Compositor: synthesized `xdg_toplevel.configure` states are filtered to the
  guest's negotiated xdg-shell version (Aura-only 100/101 no longer leak), and
  `zaura_toplevel.release` is queued before `xdg_toplevel.destroy`.

Open for host validation before Ready: popup and subsurface positions while
`set_supports_screen_coordinates` is enabled for every opted-in toplevel
(including the late zaura_shell repair path), and work-area orientation on a
rotated or portrait-native panel. Two keyboards on one seat may still issue
a redundant placement batch for one chord (no key is lost).

Validation on the final head: serial and parallel workspace tests (Sommelier
600 passed / 1 ignored, sample GUI 12, codegen 7), strict Clippy,
`cargo fmt --check`, and `git diff --check` pass.

## 2026-11-16 — Host placement acceptance

CI on the review-fix head failed only on a new Rust 1.100 Clippy lint
(`chunks_exact_to_as_chunks`) in the Aura configure-state helpers; both sites
now use `as_chunks::<4>()`, which still drops a malformed trailing partial
entry. CI passes on x86_64 and aarch64.

The release build of that head ran as an isolated proxy on `wayland-9` with
`SOMMELIER_WINDOW_BOUNDS_AS_ARC=1` and the nine `<Alt>` shortcuts, serving
`gtk3-widget-factory` on the single 3840x2160 output. The primary Sommelier
service on `wayland-2` was not touched. An earlier attempt recorded no
placements because the test window never received keyboard focus; it was not
a placement failure.

- All nine shortcuts placed the window. At 100% display size the work area was
  3840x2112 (48 px shelf inset), e.g. Left `(0, 0, 1920, 2112)` and
  BottomRight `(1920, 1056, 1920, 1056)`. At the 125% display-size setting it
was 3200x1752 (combined Aura scale 1.2 on this panel), e.g.
  Fullscreen `(0, 0, 3200, 1752)` and Right `(1600, 0, 1600, 1752)`.
- The user confirmed the window landed in each half and quadrant, Fullscreen
  worked, and context menus, menubar submenus, combo popups, and tooltips
  opened next to the moved window at both display sizes.
- No placement was skipped and no errors were logged.

Still unverified: multi-monitor placement (this host exposes one output),
rotated or portrait-native panels, and popups on a toplevel repaired through
the late zaura_shell path. Logs are kept in the untracked local evidence
directory `_local/evidence/pr9-hostval-2026-11-16/` in the primary checkout.

## 2026-11-16 — Re-deriving PR #10 on the merged core

The old PR #10 refactor (`822a990`, revised by `50cd8cd`) was written against
the pre-review placement core and no longer cherry-picks. Its intent was
re-applied to the merged PR #9 core as a pure refactor: the placement state
that was spread across `Context` fields and handler-local bookkeeping now has
one owner, `WindowPlacementState` in `sommelier/src/state/window_placement.rs`
(`Context::window_placement`).

What moved into `WindowPlacementState`:

- Policy gate: `window_bounds_as_arc` and `arc_id_allocator`, including the
  `SOMMELIER_WINDOW_BOUNDS_AS_ARC` / allocator fail-closed resolution
  (`from_environment`, `uses_arc_bounds`).
- ARC identities: `arc_application_ids` and `arc_application_ids_applied`
  (`arc_application_id`, which replaces `compositor::ensure_arc_application_id`;
  `arc_application_id_applied`, `mark_…`, and `unmark_…` for the
  `apply_window_layout` rollback). `ARC_APPLICATION_ID_PREFIX` moved with them.
- Aura associations: `wl_surface_to_zaura_surface` and
  `xdg_toplevel_to_zaura_toplevel`, plus a reverse toplevel map that replaces
  the linear search in `zaura_toplevel.configure`.
- Barriers and held configures: `window_bounds_barriers`,
  `active_window_bounds_barriers`, `pending_window_bounds_configures`, and
  `pending_window_bounds_surface_configures` (`register_barrier`,
  `complete_barrier`, `has_active_barrier`, `hold_configure`,
  `hold_surface_configure`). The two `PendingWindowBounds*` types moved too.
- Cleanup: `release_surface` retires a surface's ARC identity, applied flag, and
  Aura surface association; `take_aura_toplevel` retires the toplevel
  association, its active barrier gate, and its held configures, while older
  callbacks stay owned until their terminal `done`.

Wire encoding, request ordering, and rollback stay in the handlers, so
shortcut ownership arbitration in `on_key`, the Aura scale work area,
version-filtered configure states, `zaura_toplevel.release` before
`xdg_toplevel.destroy`, and the placement-batch rollback with ARC identity
re-allow are unchanged.

Dropped from `822a990`/`50cd8cd` because they only applied to the old core:
the `SOMMELIER_WINDOW_BOUNDS_SELF_PARENT` backend and `WindowPlacementMode`,
per-toplevel origin tracking and prediction, the fixed
`org.chromium.arc.2147483647` / `.session.*` identities (superseded by the
reviewed `arc_task_ids` allocator), the `window_placement` renames, and the
GTK change that resolves the Aura surface lazily instead of using
`GtkSurfaceState::host_zaura_surface_id` (it would send startup IDs and D-Bus
metadata to Aura surfaces created after the GTK surface, which is a behavior
change). `sommelier/docs/ARC_TASK_AND_SESSION_IDS.md` is included with its
fixed-ID section rewritten to describe the current allocator.

Validation (monorepo dev shell, locked, serial): Sommelier 609 passed /
1 ignored (600 before plus 9 `WindowPlacementState` unit tests), sample GUI 12,
codegen 7, `gui_smoke` 0 run / 1 ignored; strict Clippy on all targets,
`cargo fmt --check`, and `git diff --check` pass. No Sommelier process was
started or restarted.

## 2026-11-16 — Shortcut configuration file

Ported the still-useful parts of the old shortcut-configuration PR #4
(`090126e`, `d6f0d24`, `252605b`) onto the merged placement design, without
its self-parent backend, placement-mode flags, or `window.place` action.

Interface:

- `--window-shortcuts-config PATH` names a TOML file: `version = 1`, then
  `[[bindings]]` with `chord` and exactly one of `action` (the nine existing
  names) or `rect = [x, y, width, height]` in work-area fractions.
- The file is read only when the option is given and
  `SOMMELIER_WINDOW_BOUNDS_AS_ARC` is requested; with the gate off it is
  ignored with a warning. Supplying it together with a non-blank
  `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` is a startup error.
- Startup validation fails closed (exit 2): regular file only, 64 KiB bound,
  UTF-8, TOML syntax, unknown keys, version, at most 256 bindings, chord
  syntax, duplicate chords, `SOMMELIER_ACCELERATORS` conflicts (an invalid
  accelerator list is itself an error here), unknown actions, and rect bounds
  (NaN/inf, zero size, `x + width > 1`).
- `SIGHUP` re-reads the same absolute path. A valid document replaces the
  bindings atomically through a Tokio `watch` channel; an invalid one is
  logged and the last known-good bindings stay. Connections adopt a new
  generation on their next unowned key press. The handler is installed only
  when a file is in use, and a reload never re-reads the ARC gate.

Geometry: every placement, named or configured, now goes through
`NormalizedRect::to_work_area_bounds` (`sommelier/src/window_layout.rs`), with
edges at `floor(f * extent + 1e-6)`. That equals the previous `W / 2` half
split, which a unit test keeps as an oracle across small, odd, and extreme
work areas, so the named actions' bounds and the existing keyboard tests are
unchanged.

Commits: the preset-to-rect refactor, a shared
`SOMMELIER_ACCELERATORS`/ARC-gate environment refactor, the feature itself
(new dependencies `serde`, `toml` with only `parse`/`serde`/`std`, and
`thiserror`), and this documentation. See
`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`.

Validation (monorepo dev shell, locked, serial): Sommelier 640 passed /
1 ignored (609 before), sample GUI 12, codegen 7, `gui_smoke` 0 run /
1 ignored; strict Clippy on all targets, `cargo fmt --check`, and
`git diff --check` pass. Not host-validated: no Sommelier process was started
or signalled.

### Host acceptance (2026-11-16)

The release build ran as an isolated proxy on `wayland-9` with
`SOMMELIER_WINDOW_BOUNDS_AS_ARC=1` and `--window-shortcuts-config`, serving
`gtk3-widget-factory` on the 3840x2160 output (work area 3840x2112). The
primary Sommelier service on `wayland-2` was not touched.

- `SIGHUP` with an invalid edit (`x + width > 1`) logged the error and kept
  the last known-good bindings; a valid edit then reloaded 5 bindings. The
  proxy stayed up through both signals.
- Configured thirds placed the window at `(0, 0, 1280, 2112)`,
  `(1280, 0, 1280, 2112)` and `(2560, 0, 1280, 2112)`: shared edges, no gaps.
  The reloaded centre rect `[0.25, 0.25, 0.5, 0.5]` placed it at
  `(960, 528, 1920, 1056)`, and the named `top-left` action was unchanged at
  `(0, 0, 1920, 1056)`. The user confirmed each placement visually.
- No placement was skipped. Logs are kept in the untracked local evidence
  directory `_local/evidence/pr18-hostval-2026-11-16/` in the primary checkout.

## 2026-11-16 — Simplified ARC task-ID allocator

Cleanup PR B for review findings A3, C8, H5 (allocator part), H6, S8 and the
allocator parts of D1/D2.

What changed in `sommelier/src/arc_task_ids.rs` (826 → 414 lines; about
548 → 245 lines of non-test code):

- Removed the directory-generation guard file, the dev/ino rechecks, the
  non-atomic `LOCK_EX` → `LOCK_SH` conversions and the per-file uid/mode
  checks. All of them defended against writers inside the lock directory, and
  the one check kept, that `$XDG_RUNTIME_DIR` is owned by the user with no
  group/other access, already limits those writers to the user's own
  processes.
- Reservation walks the blocks from a random start, opens each lock file
  `O_CREAT | O_NOFOLLOW | O_NONBLOCK` (`0600`) and takes a non-blocking
  exclusive `flock`. Any per-block error skips the block, so a stray lock file
  no longer disables ARC placement; `O_NONBLOCK` keeps a stray FIFO from
  blocking the open forever. A held block is logged at debug, any other
  per-block error at warn. Lock file names are unchanged, so older and newer builds
  still exclude each other.
- Random seeds come from `getrandom(2)` instead of opening `/dev/urandom`.
- `ArcIdAllocator` is now a pure range, offset and issued-ID counter (behind a `Mutex`; `AtomicU32::fetch_update` is deprecated in Rust 1.100). The lock
  descriptor sits beside it in a never-dropped static, so the `Option<File>`
  test fields, the no-op `Drop` and the test-only guard-directory parameter
  are gone, and reservation tests run in a temporary runtime directory.
- A failed reservation is logged once and still cached for the process: its
  causes do not clear at runtime, and retrying would let connections disagree.
  The per-connection warning in `WindowPlacementState` is gone, and block
  exhaustion is logged once instead of being dropped by `ok()?`.
- `WindowPlacementState::for_test` sizes its block with `ARC_ID_BLOCK_SIZE`.
- The "placement shortcuts ignored" warning now names both causes: the gate
  is unset, or no ARC task-ID block was reserved.

Docs: the "Sommelier's task-ID allocator" section of
`sommelier/docs/ARC_TASK_AND_SESSION_IDS.md` now owns every allocator fact;
the README, the shortcut configuration doc and the module comment link to it.
The duplicated pool paragraph is gone, and the doc now says reservation
happens on first use and that a failure disables the policy for the whole
process. It also states the caveat that deleting or replacing the lock
directory, or a held lock file, while a proxy runs lets a later proxy reuse
that block undetected.

Validation (monorepo dev shell, locked, serial): Sommelier 640 passed /
1 ignored (644 before: 14 old allocator tests replaced by 10), sample GUI 12,
codegen 7, `gui_smoke` 0 run / 1 ignored; strict Clippy on all targets,
`cargo fmt --check` and `git diff --check` pass. Not host-validated: no
Sommelier process was started.

## 2026-11-16 — Placement correctness fixes

Behavior fixes from the consolidated placement review (PR A). Each fix has a
regression test that failed before it and passes after.

- **One chord, one classification.** `KeyboardHandler::current_chord` is the
  only place a key's chord is computed. `classify_key` turns it into
  `HostAccelerator`, `Placement(action)` (only while the ARC policy is
  active), or `Guest`. `wl_keyboard.key` and `peek_key` both use it. The two
  earlier chord computations had already diverged.
- **Placement chords never go through IME recovery.** `peek_key` marks a
  generation eligible for IME recovery only when it is guest input. Before,
  when `peek_key` arrived first, text-input could deliver the chord or
  recover it. The guest then owned the key, so the app received Alt+q and the
  window did not move. The remaining contract is now in the test names: a
  key the guest received before it could be classified belongs to the guest.
- **A matched chord is always consumed.** While the policy is active, a
  chord that matches a binding is Sommelier's even when placement is skipped:
  no focused toplevel, incomplete output metadata, or a rect the output
  cannot represent. `apply_window_layout` returns a `PlacementSkip` reason.
  A reason that persists until the configuration or the proxy changes
  (unrepresentable rect, no attachable Aura toplevel) is logged at info, and
  the rest at debug. Repeat and release follow the consumed owner. A chord
  that the host IME consumes never produces a `wl_keyboard.key`, so it is
  neither placed nor delivered; `peek_key` logs such a chord at debug level.
- **No jumps to another monitor.** `Context::output_for_surface` falls back
  to the first usable output only when the surface has entered no output. A
  surface whose entered outputs are not usable yet is skipped. The duplicate
  fallback in the keyboard handler is gone.
- **One gate for placement Aura toplevels.**
  `compositor::attach_placement_toplevel` checks the policy, allocates the
  ARC identity, and then creates the `zaura_toplevel`. It is used by
  `get_toplevel`, by the late `zaura_shell` retry (which used to skip the
  identity check), and by placement. `ensure_zaura_toplevel` is private.
- **No early GTK identity.** `get_gtk_surface` no longer allocates an ARC
  identity that it discarded. Identities stay lazy and surface-keyed.
- **No leaked output entries.** A `wl_output` bind registers output state
  only after its host bind is queued. An unmapped registry therefore leaves
  nothing behind. The output and dmabuf bind paths share one
  `bound_global_generation` lookup.

Left for later PRs:

- C: the `PlacementConfig`/environment pipeline.
- D: moving placement out of the keyboard handler.
- E: the output registry and Aura object owners, including the rest of C5:
  `inactive_output_host_ids` still only grows, and
  `bound_global_generation` still falls back to generation 0 with
  `unwrap_or_default()`. That fallback is unreachable (see its comment), but
  the registry should make it unrepresentable.

Validation (monorepo dev shell, locked, serial), rebased on the simplified
ARC allocator: Sommelier 650 passed / 1 ignored (640 on that base),
sample GUI 12, codegen 7, `gui_smoke` 0 run / 1 ignored. Strict Clippy on all targets, `cargo fmt --check`, and
`git diff --check` pass. These fixes have not been validated on a host: no
Sommelier process was started.

## 2026-11-16 — Placement configuration has one owner

Cleanup PR C for review findings S1, S7, A2, A5, A6, A7, H7 and the
configuration and test-seam part of H5. The contract itself lives in
`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`; this section records what
changed and why.

**Before.** The environment bindings were parsed in `Context::new` on every
connection, warn-only, with their own duplicate and conflict checks. The file
was parsed once in `main`, fail-closed, with a second copy of those checks.
`SOMMELIER_ACCELERATORS` and `SOMMELIER_WINDOW_BOUNDS_AS_ARC` were read in
four places. Each `Context` copied the bindings into a `Vec` and had to call
`refresh_*` before matching a key. `accelerator.rs` and `window_layout.rs`
imported each other, and so did `window_shortcuts.rs` and `state.rs`.

**After.**

- `placement_config.rs` (was `window_shortcuts.rs`) owns `PlacementConfig`.
  `main` resolves it once from the gate, the ARC block,
  `SOMMELIER_ACCELERATORS` and exactly one binding source, then shares it
  with every `Context` as an `Arc`. Nothing else reads these variables.
  Bindings live inside the active ARC policy, so "policy off" and "chord
  consumed" cannot coexist. The environment source is a policy without a
  reload path.
- Both sources produce raw bindings for one validator: size, chord syntax,
  duplicates after parsing, host-accelerator conflicts, actions and rects.
  `configured_text` is the only "blank means unset" rule.
- `Context` asks the shared config through `placement_action_for` and
  `is_host_accelerator`. A reload swaps the generation's `Arc`, and the
  next lookup sees it, so `classify_key` takes `&Context`.
- `WindowPlacementState::new` takes the ARC block from the config, and
  "policy active" means a block is present. The environment constructor,
  the gate helper and the `set_*_for_test` seams are gone.
  `Context::new_for_test` / `with_placement_for_test` read no environment
  and reserve no real block; tests build configs through the production
  resolver.
- Module graph: `accelerator` (chords only) and `window_layout` (actions and
  geometry) depend on nothing. `placement_config` depends on both and on
  `arc_task_ids`. `state` depends on `placement_config`. `keyboard`, `proxy`
  and `main` sit on top. There are no cycles.
- A7: `main` installs the `SIGHUP` handler before the file is first read.
- A5: `GuestKeyOwner::admits_compositor_shortcut` is the only statement of
  the shortcut arbitration contract. The key reducer's `arbitrate` routes
  every event to the channel that owns its generation, and rejects a
  shortcut event the contract does not admit instead of acking it. Routing
  has one owner: `on_key` sends `CompositorShortcutPress` only for the press
  that claims an unowned key and runs placement, and it sends repeats,
  releases and duplicate presses as physical events.
- A6: both model-based key tests now produce the `CompositorShortcut`
  owner. The extended trace test found that calling the reducer API directly
  with a physical press of a shortcut-owned key tripped an assertion.
  `on_key` never sent that combination, so this was not a production bug;
  `arbitrate` now defines it.
- H7: the precision a rect needs is documented and tested (16 significant
  digits land on the intended pixel up to 1,000,000 pixels; `0.3333333`
  can land one pixel short). A test pins every preset to
  `NormalizedRect::new` of an independent table. `EDGE_TOLERANCE` now has a
  single role, a border band, with unchanged results.

**Behavior changes.**

- A placement configuration problem never stops the proxy (lead decision,
  which replaces an earlier exit-2 draft of this PR). Both sources follow one
  policy: both sources given, an invalid or non-UTF-8 environment list, an
  unusable file path or file, or an invalid `SOMMELIER_ACCELERATORS` while a
  source is requested each log one error per process, and the proxy runs
  with placement shortcuts disabled. Before, a bad environment list logged a
  warning on every connection, while a bad file or both sources stopped
  startup with exit status 2. A file that was unusable at startup is
  activated by a later valid `SIGHUP` reload. A `SIGHUP` handler that cannot
  be installed is logged instead of exiting.
- Shortcuts requested while the gate is off, or while no ARC block can be
  reserved, log one warning per process, and neither source is read.
- The ARC block is reserved at startup instead of on the first connection.
- With `--window-shortcuts-config`, `SIGHUP` is caught even when the file
  ends up unused; it is then logged and ignored.

Deliberately not done: `Context::new` still reads `SOMMELIER_VM_IDENTIFIER`
(the test constructors do not), and the GBM allocator it creates reads
`SOMMELIER_DRM_DEVICE`. Neither is placement configuration.

Validation (monorepo dev shell, locked, serial): Sommelier 664 passed /
1 ignored (650 before), sample GUI 12, codegen 7, `gui_smoke` 0 run /
1 ignored. Strict Clippy on all targets, `cargo fmt --check`, and
`git diff --check` pass. Not host-validated: no Sommelier process was started.

## 2026-11-16 — Output registry

Cleanup PR E1 for review findings S2, S9, S10, the rest of C5, C6, and the
output parts of H3 and H4.

**Before.** Output state lived in seven public `Context` fields
(`output_host_ids`, `output_states`, `pending_output_states`,
`output_host_global_bindings`, `inactive_output_host_ids`,
`surface_output_ids`, `zaura_output_to_wl_output`) that the registry,
compositor and state code each mutated. The "is this output live" rule was
restated about nine times. `inactive_output_host_ids` grew on every release.
The `wl_output.geometry` transform and `zaura_output.logical_transform` both
wrote one field, and the last event won.

**After.** `OutputRegistry` in `sommelier/src/state/outputs.rs` is the only
owner. Its fields are private, and it keeps one record per bound host output:
the `GlobalBinding` (name and generation) it came from, whether its bound
version batches events until `wl_output.done`, its `zaura_output`, and a
`Live { committed, pending }` / `Inactive` state. A
reverse `zaura_output` index replaces the scans, and the surface-to-output
associations moved in too. Handlers call `bind`, `release` (which returns the
`zaura_output` to tear down), `deactivate`, `update` (the one publish rule
for `wl_output` and `zaura_output` events), `commit`, `aura_event_target`,
`ensure_aura_output` (which alone decides whether a `zaura_output` may be
created), and `surface_entered` / `surface_left` / `surface_destroyed`.
`admit_guest_bind` returns the generation a bind refers to, so a bind cannot
proceed without one. Placement asks
`for_surface` and `primary`. The same module owns `is_valid_transform` and
`transformed_extent`, which surface buffer transforms use too.

- The output's liveness is the record itself, so the inactive set is gone.
  The `wl_output` handlers' pending-destructor checks were dead: the proxy
  suppresses host events for such objects before dispatch, and a proxy test
  now pins that.
- The unread `OutputState.scale` is gone. `wl_output.scale` is still
  forwarded.
- Named since-versions replace the literals 2 and 38 for `get_aura_output`
  and `zaura_output.release`.
- `state.rs` shrank from 2834 to 2465 lines, and `compositor.rs` from 6172 to
  6147. The new `outputs.rs` has 757 lines, of which 428 are production code.

**Behavior changes**, each with a regression test that failed before:

- *C6, the work area follows the panel transform.* Exo sends
  `wl_output.mode` as `ManagedDisplayInfo::bounds_in_native()` (native panel
  pixels) and the geometry transform as `display.panel_rotation()`, which is
  the active rotation plus the panel's mounting orientation.
  `logical_transform` is `display.rotation()` alone. On a portrait-native
  panel, the logical transform left the work area in the wrong orientation.
  ChromiumOS Sommelier also ignores `logical_transform` and rotates by the
  geometry transform. When the two transforms agree (any panel in its natural
  mounting), results are unchanged
  (`work_area_rotates_native_mode_by_the_panel_transform`).
- *C5, released outputs leave nothing behind*
  (`released_outputs_leave_no_liveness_bookkeeping`).
- *C5, a bind without an advertisement generation is refused* instead of
  being recorded under a made-up generation 0 that no `global_remove` could
  match. Real traffic never reaches this path
  (`bind_without_an_advertisement_generation_is_refused`).

Validation (monorepo dev shell, locked, serial): Sommelier 675 passed /
1 ignored (664 before), sample GUI 12, codegen 7, `gui_smoke` 0 run /
1 ignored. Strict Clippy on all targets, `cargo fmt --check`, and
`git diff --check` pass. Not host-validated: no Sommelier process was started,
and rotation and multi-monitor selection have never been checked on a host.

## 2026-11-16 — Placement module

Cleanup PR D for review findings A1, H2, H3 (placement objects) and A4,
rebased onto the output registry (#23), plus one protocol fix found in
review. Apart from that fix, behavior is unchanged; this section records the
new structure and why.

**Before.** Placement was spread over five modules. The keyboard handler
resolved the focused toplevel, computed bounds, and built the request batch
with its own rollback. `compositor.rs` created the Aura toplevel, queued the
ARC identity and the barrier, and held configures inline in three event
handlers. `callback.rs` completed barriers through a compositor helper, and
`registry.rs` and `proxy.rs` attached Aura toplevels and dropped the result
with `let _`. Fixed-size requests went through `try_build_message` with
rollback branches that could never run, version thresholds were copied
literals, and bounds were bare tuples.

**After.**

- A1: `sommelier/src/handler/window_placement.rs` owns placement mechanics.
  Each protocol handler translates its event and makes one call:
  `place_focused`, `attach_toplevel`, `attach_recorded_toplevels`,
  `release_toplevel`, `apply_arc_identity`, `complete_barrier`,
  `hold_surface_configure`, `hold_toplevel_configure` and
  `translate_aura_configure` (the module docs map events to entry points).
  `PlacementSkip` is its only other export. `ensure_host_zaura_surface`
  stays in `compositor.rs` because XDG and GTK metadata share the Aura
  surface; `record_xdg_toplevel` stays there because the XDG role links also
  route application IDs. Both are candidates for the Aura-objects and
  XDG-roles owners.
- H2: placement's fixed-size messages (u32/i32 payloads of at most
  20 bytes, or the proxy-generated ARC identity of at most 27 bytes) use
  `build_message`. This removes `BarrierUnavailable`, the batch rollback,
  `unmark_arc_application_id_applied` and their two tests. Host data (the
  configure states array) keeps the fallible builder, as the sommelier-rs
  rule requires. `ArcIdentityUnavailable` is reachable (an Aura surface
  created under an older `zaura_shell` generation), and a test pins that it
  queues no part of the batch. `UnmappedToplevel` was unreachable (only
  mapped toplevels are recorded, and every unmapping path removes the
  record first) and is gone; focus resolution asserts the invariant.
- H3: `wayland_codegen` emits `REQ_*_SINCE` / `EVT_*_SINCE` for every
  message, so the vendored XML owns each threshold. The zaura_surface,
  zaura_toplevel and zaura_output checks use them, replacing the output
  registry's hand-written `ZAURA_SHELL_GET_AURA_OUTPUT_SINCE` and
  `ZAURA_OUTPUT_RELEASE_SINCE`. `PLACEMENT_TOPLEVEL_SINCE` is the
  set_window_bounds version, checked at compile time against the other
  requests placement sends. `compositor::aura_object_version` is the one
  version rule for every Aura child, including the zaura_output release; it
  asserts in debug builds that the child has a recorded version.
- A4: `window_layout::Rect` carries work areas and bounds, and
  `OutputState::work_area` returns it.

**Behavior change.** `clear_window_state` sent `zaura_surface.unset_snap`
(since 16) without a version check. An Aura surface keeps the version of the
shell generation that created it, so after a re-advertised newer shell a
placement could send it to a v5-v15 surface, which Exo rejects by
disconnecting the client. It is now sent only to surfaces that have it
(`unset_snap_is_sent_only_to_an_aura_surface_that_has_it`, which failed
before the fix).

Line counts against the rebased base (total / production before
`#[cfg(test)]`): `keyboard.rs` 6388/1405 → 5854/1209, `compositor.rs`
6147/2098 → 5405/1756, `callback.rs` 257/124 → 248/115,
`window_placement.rs` 1689/729 (new).

Deliberately not done: `ensure_zaura_output` still builds its fixed-size
request with `try_build_message`, which belongs with the output registry; the
`zaura_shell` bind cap (38) is not a since-version; the `xdg_toplevel` state
table already mirrors the XML in one place. Other handlers' fixed-size
`try_build_message` calls (`wl_surface.destroy`, `xdg_toplevel.destroy`,
callback events) are outside placement.

Validation (monorepo dev shell, locked, serial): Sommelier 678 passed /
1 ignored (675 before: two rollback tests deleted; skip, `unset_snap`,
zaura_output retire and two version-boundary tests added), sample GUI 12,
codegen 8 (7 before), `gui_smoke` 0 run / 1 ignored. Strict Clippy on all
targets, `cargo fmt --check`, and `git diff --check` pass, and every commit
builds with strict Clippy. Not host-validated: no Sommelier process was
started.
