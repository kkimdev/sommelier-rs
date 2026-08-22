# Development Handover

## Current status

- The active development line is the `virtwl` branch in
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
  `support.rs` contains private link/barrier primitives. `mod.rs` is the sole
  connection-local mutable owner; keyboard, compositor, GTK, and callback
  handlers can only ask it for decisions or lifecycle transitions.
- Native Guest OS and ARC compatibility IDs for a surface are stored in one
  `SurfaceApplicationState` record. Surface teardown removes that record and
  its Aura association together, so a delayed transient cleanup cannot restore
  an ID belonging to a different surface generation.
- `WindowPlacementPlan::is_well_formed` validates that geometry, transient ARC
  identity, cleanup IDs, and target IDs describe one window. The wire adapter
  also rechecks the live guest-to-host XDG/surface mappings and host interface
  generations immediately before queueing, so a stale plan cannot create a
  barrier or send requests to a remapped object.
- `wayland_codegen/` generates Rust dispatch code from vendored protocol XML.
- `sommelier_test_app/` provides a standalone GUI/IME regression client.
- `packaging/systemd/` contains the user service template for display
  instances such as `wayland-2`.

## Known issues and debt

- The original `virtwl-v0.2.1` tag was consumed by a failed immutable GitHub
  Release attempt and cannot be recreated. `virtwl-v0.2.1-r1` is the usable
  rebuild tag.
- `--gpu-accel` remains disabled until the PRIME linux-dmabuf path is complete.
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

The latest refactor pass passes 617 Sommelier tests with one ignored test, 12
sample-GUI tests, and six Wayland-codegen tests. The test bodies take about
7.8 seconds in the required serialized workspace run and about 2.7 seconds
with four test threads; the cold build adds roughly ten seconds. No
sleep-based or hardware-dependent test was made weaker to achieve this timing.
Two FD-ownership tests were also made parallel-safe by checking procfs
resource identity instead of relying on a globally unique numeric descriptor.
Workspace check, strict Clippy, rustdoc with warnings denied, the release
build, formatting, and `git diff --check` also pass. The latest pass did not
restart or exercise the primary `/dev/wl0` compositor.
