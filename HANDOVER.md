# Development Handover

## Current status

- The active development line is the `virtwl` branch in
  [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs).
- The fork is based on `google/sommelier-rs:virtwl` and carries ChromeOS
  VirtWL, shelf-ID, protocol-hardening, and Korean IME fixes.
- The current release and its assets are listed under
  [Current release](README.md#current-release), and the CI artifact and
  release policy under [CI and release workflow](README.md#ci-and-release-workflow).
- The `chromeos-dev-setup` flake in `monorepo-public` installs the package
  through this repository's flake overlay. It picks up a release once
  `nix/package-bin.nix` is updated here and the `chromeos-dev-setup`
  `flake.lock` moves to that `virtwl` revision.

## Architecture

- The README's [repository layout](README.md#repository-layout) lists the
  top-level directories, [`docs/DESIGN.md`](docs/DESIGN.md) explains the
  proxy's structure, and [`docs/INVARIANTS.md`](docs/INVARIANTS.md) holds
  the rules every change must keep.
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
- A late owed `wl_buffer.release` can mark a buffer released while the
  host holds it, which skips its damage-only copies until the next attach;
  see [Surfaces, damage and SHM](docs/INVARIANTS.md#surfaces-damage-and-shm).
- ChromiumOS parity reviews left these differences unconfirmed, with no
  reproducer: commits on a surface with no role or window, cleanup of a
  keyboard leave for a pending surface, and two keyboards on one seat (the
  IME Backspace fallback picks a seat-level keyboard while keysyms prefer
  the focused one, and one chord can issue a redundant placement batch).
- Clipboard pumps rely on a VirtWL pipe supporting `poll` and returning
  `EAGAIN` under `O_NONBLOCK`. The ChromiumOS `virtio_wl.c` does both, but
  no transfer has been checked on a device since the pumps stopped
  blocking.
- The dma-buf fence wait still waits on the guest GPU on the event loop,
  the one exception to the
  [event-loop rule](docs/INVARIANTS.md#process); the fence re-dispatch
  follow-up named at `wait_sync_file` removes it.
- The `zxdg_output_manager_v1` passthrough has not been checked on a host.
  That Exo advertises the global is inferred from ChromiumOS Sommelier,
  whose direct-scale mode binds it and assumes it is present.
- Text input's double-buffered state still lives in separate `pending_*`
  fields and dirty flags of `TextInputState`; moving it to the
  [double-buffered state](docs/DESIGN.md#double-buffered-state) convention
  is the next cleanup.
- Placement's unverified cases are listed under
  [Output and work area](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#output-and-work-area);
  popups on a toplevel repaired through a late `zaura_shell` bind were also
  never checked on a host.

## Next steps

1. Continue Crostini manual testing through the `wayland-2` service.
2. Keep protocol and IME regressions covered by the serial test suite.
