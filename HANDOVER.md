# Development Handover

## Current status

- The active development line is
  `feature/window-placement-shortcuts-rewrite` in
  [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs).
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
  self-parent move, the older callback may still queue its own nullable
  unparent, but it must not retire the newer resize state. The
  `finish_self_parent_move` transition therefore only clears a state that is
  still in the completed parent-move phase; the regression is covered by both
  state and keyboard-handler tests. Without this guard, rapid `Alt+...`
  sequences intermittently resized but never issued the newer position probe.
- The reducer in `sommelier/src/state/window_placement/transaction.rs` is the
  sole owner of the resize/parent/cleanup phases. A placement generation
  requires the matching synthetic XDG `ack_configure` and a subsequent guest
  commit; host-first configures are buffered, stale serials are ignored, and
  ordinary Idle/cleanup configures continue through the normal guest XDG
  translation path. Host configure coordinates are not used to overwrite a
  settled origin unless the reducer accepts the event as the active target.
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

The current source passes 667 unit tests with one ignored live-VirtWL test.
`cargo fmt --check`, `cargo check -p sommelier`, strict all-target Clippy,
and `cargo build -p sommelier --release` all pass. Runtime testing must compare
the release binary mtime with the isolated proxy start time before accepting
results; the currently running `/dev/wl0` process predates the latest build
and was intentionally not restarted by this handover.
