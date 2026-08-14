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
