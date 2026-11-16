# Development Handover

## Current status

- The active development line is the `virtwl` branch in
  [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs).
- The fork is based on `google/sommelier-rs:virtwl` and carries ChromeOS
  VirtWL, shelf-ID, protocol-hardening, and Korean IME fixes.
- The current release and its assets are listed under
  [Current release](README.md#current-release), and the CI artifact and tag
  policy under [CI and release workflow](README.md#ci-and-release-workflow).
- The `chromeos-dev-setup` flake in `monorepo-public` installs the package
  through this repository's flake overlay. It picks up a release once
  `nix/package-bin.nix` is updated here and the `chromeos-dev-setup`
  `flake.lock` moves to that `virtwl` revision.

## Architecture

- The README's [repository layout](README.md#repository-layout) lists the
  top-level directories.
- The opt-in window placement feature is described, with its code map, in
  [`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).

## Known issues and debt

- Historical: the original `virtwl-v0.2.1` tag was consumed by a failed
  immutable GitHub Release attempt and cannot be recreated;
  `virtwl-v0.2.1-r1` replaced it.
- [Scope and limitations](README.md#scope-and-limitations) lists what
  `--gpu-accel` and the fork do not cover.
- Upstream review and eventual rebase/cherry-pick coordination remain
  separate from this fork's `virtwl` branch.

## Next steps

1. Continue Crostini manual testing through the `wayland-2` service.
2. Keep protocol and IME regressions covered by the serial test suite.
