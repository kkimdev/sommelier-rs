# Development Handover

## Current status

- The active review line is
  `feature/window-placement-shortcuts-generation-safe` in
  [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs), stacked on
  the modularization and self-parent PRs.
- The fork is based on `google/sommelier-rs:virtwl` and carries ChromeOS
  VirtWL, shelf-ID, protocol-hardening, and Korean IME fixes.
- The published binary release is the prerelease
  [`virtwl-v0.2.1-r1`](https://github.com/kkimdev/sommelier-rs/releases/tag/virtwl-v0.2.1-r1).
- Release assets include x86_64 and aarch64 binaries plus `SHA256SUMS`.
- Successful pushes create short-lived CI artifacts and non-release
  `virtwl-ci-*` tags. Artifacts expire after seven days and the cleanup job
  retains the ten newest build runs by default.

## Architecture

- `sommelier/` contains the proxy state machine, protocol handlers, and
  VirtWL channel.
- Window placement is layered under
  `sommelier/src/state/window_placement/`: `runtime.rs` owns process-wide
  configuration, `plan.rs` contains pure validated operation values, and
  `support.rs` contains private link/barrier primitives. `runtime.rs` exposes
  one closed `WindowPlacementMode` policy instead of three independently
  constructible axes, so incompatible geometry/ARC-lifetime combinations are
  rejected at startup. `mod.rs` is the sole connection-local mutable owner;
  keyboard, compositor, GTK, and callback handlers can only ask it for
  decisions or lifecycle transitions.
- `WindowPlacementPlan` and `TransientArcIdentity` keep their fields private.
  Only `WindowPlacementState` can construct a production plan; protocol
  adapters can inspect accessors and serialize it, but cannot combine an
  identity, target, and cleanup from different lifecycle generations.
- Native Guest OS and ARC compatibility IDs for a surface are stored in one
  `SurfaceApplicationState` record. Surface teardown resolves the host
  `wl_surface` through the authoritative `ShadowTable` before removing that
  record and its Aura association together, so a delayed transient cleanup
  cannot restore an ID belonging to a different surface generation. If ARC
  allocation preceded Aura-child creation, the authoritative
  `wl_surface.destroy` path explicitly retires the orphan identity record.
  A stale or mismatched shadow mapping leaves newer identity state untouched.
- XDG role teardown is also state-owned as one transition. `XdgToplevelRelease`
  removes the XDG/Aura links, origin prediction, and active barrier together;
  handlers only serialize the returned host Aura release and cannot forget one
  half of the role lifecycle. Older callback records remain reserved until
  their host `delete_id`, but no longer produce active placement cleanup.
- Role registration follows the protocol hierarchy: an `xdg_toplevel` requires
  a live `xdg_surface` link, and a `zaura_toplevel` requires its guest XDG
  role. Surface identity teardown also verifies the expected Aura-surface link
  before removing native/ARC IDs, so stale host IDs cannot clear a newer
  generation.
- `WindowPlacementPlan::is_well_formed` validates that geometry, transient ARC
  identity, cleanup IDs, and target IDs describe one window. The wire adapter
  also rechecks the live guest-to-host XDG/surface mappings and host interface
  generations immediately before queueing, so a stale plan cannot create a
  barrier or send requests to a remapped object.
- Native `set-parent` placement is a latest-target transaction while a resize
  or host barrier is in flight. If a newer shortcut supersedes an older
  self-parent move, the older callback may still complete its ordered
  self-parent-retention/IME barrier, but it must not retire the newer resize
  state. The `finish_self_parent_move` transition therefore only clears a
  state that is still in the completed parent-move phase; the regression is
  covered by both state and keyboard-handler tests. Without this guard, rapid
  `Alt+...` sequences intermittently resized but never issued the newer
  position probe.
- The reducer in `sommelier/src/state/window_placement/transaction.rs` is the
  sole owner of the resize/parent/cleanup phases. A placement generation
  requires the matching synthetic XDG `ack_configure` and a subsequent guest
  commit; host-first configures are buffered, stale serials are ignored, and
  ordinary Idle/cleanup configures continue through the normal guest XDG
  translation path. Host configure coordinates are not used to overwrite a
  settled origin unless the reducer accepts the event as the active target.
  When the host omits the final target `origin_change`, the ordered cleanup
  barrier promotes the requested target as the best available baseline and
  preserves a deferred shortcut. After completion, a short-lived origin guard
  ignores late focus/animation frames until the next explicit placement starts;
  this prevents the previously observed `z -> a -> a` lower-right drift.
- `wayland_codegen/` generates Rust dispatch code from vendored protocol XML.
- `sommelier_test_app/` provides a standalone GUI/IME regression client.
- `packaging/systemd/` contains the user service template for display
  instances such as `wayland-2`.
- The opt-in `remote-shell-v2` backend is implemented in
  `sommelier/src/handler/remote_shell.rs` and uses the vendored
  `third_party/protocols/remote-shell-unstable-v2.xml`. It keeps the global
  host-only, maps guest XDG roles to official remote-surface requests, and
  never fabricates ARC IDs.

## Known issues and debt

- The original `virtwl-v0.2.1` tag was consumed by a failed immutable GitHub
  Release attempt and cannot be recreated. `virtwl-v0.2.1-r1` is the usable
  rebuild tag.
- `--gpu-accel` remains disabled until the PRIME linux-dmabuf path is complete.
- The current `/dev/wl0` host did not advertise `zcr_remote_shell_v2` during
  the first runtime probe. The opt-in proxy therefore logged
  `host did not provide a bound zcr_remote_shell_v2` and closed the Ghostty
  client; this is a host capability limitation, not a fallback to Aura
  placement. A host with remote-shell access is still required for end-to-end
  geometry/IME validation.
- The placement state has fields for output insets, but the current stack does
  not yet bind `zaura_output` or consume host inset events. Runtime rectangles
  therefore use full output bounds; shelf/work-area exclusion needs a separate
  Aura-output PR.
- The separate `monorepo-public` Nix package still needs the personal release
  URL and SRI hashes updated to `virtwl-v0.2.1-r1`.
- Upstream review and eventual rebase/cherry-pick coordination remain
  separate from this fork's `virtwl` branch.

## Next steps

1. Update the Nix binary derivation with the `virtwl-v0.2.1-r1` assets and
   verified hashes.
2. Continue Crostini manual testing through the `wayland-2` service.
3. Keep protocol and IME regressions covered by the serial test suite.
4. Run the repository `/commit` workflow to stage the permanent placement
   modules and publish the verified changes; keep `_local/` runtime logs
   untracked.

## Latest verification

The current source passes 715 Sommelier unit tests with one ignored live-VirtWL
test. Workspace `cargo check --all-targets`, strict all-target Clippy,
`cargo fmt --check`, and `git diff --check` all pass. Runtime testing must
compare the release binary mtime with the isolated proxy start time before
accepting results; the currently running `/dev/wl0` process predates the latest
build and was intentionally not restarted by this handover.

## 2026-08-24 — independent PR review boundaries

- The published stack is linear: PR9 is based on the fork's current
  `virtwl=a5ea1a7`, PR10 is based on PR9, PR4–PR7 continue that line, PR11
  modularizes it, and PR8 adds generation-safe recovery. The earlier
  “stale-base/60-commit conflict” note referred to an abandoned PR and does
  not describe the current PR9.
- PR9's original core deliberately puts the ARC compatibility ID on the host
  XDG role and accepts late Aura origins. Those behaviors are corrected by
  PR10's identity split and PR8's generation/origin guards; review PR9 as a
  prerequisite, not as the production endpoint.
- The remote PR11 commit is a clean placement-module extraction. A local
  composite experiment also contains a no-op self-parent cleanup; that
  variant is unsafe with PR8 because `RetainSelfParent` carries the ordered
  IME-refresh and deferred-target phases. Keep that experiment out of PR11
  unless those phases are restored.
- PR6's published block allocator passes its focused and strict-Clippy checks
  and uses process-shared `flock` files. The generation-safe PR8 stack includes
  its parent-side guard and strict path validation. Those checks still cannot
  defend against deliberate same-UID unlink/recreate of its own guard inode
  (or parent); runtime cleanup must preserve the namespace until all Sommelier
  processes exit.
- The PR4 full test suite exposed a parallel-only fd assertion race. The
  regression now selects a high descriptor slot based on `RLIMIT_NOFILE`; the
  production rollback path was unchanged. Keep this test-only safety change
  outside the config PR if the PR is reconstructed from its clean commit
  boundary.
- PR10's direct `RenderBufferRegistry` exhaustive test removes the old
  60-second `Context`/GBM setup from the state suite. Its repeated
  `/proc/self/fd` identity helpers are test-only and should be shared or kept
  in a separate fd-ownership PR rather than mixed into the state API.
