# sommelier-rs — virtwl fork

This repository is a ChromeOS/Crostini-focused fork of
[Google's sommelier-rs](https://github.com/google/sommelier-rs). It runs a
Wayland proxy inside a guest and carries Wayland traffic across the
ChromiumOS `virtio_wl` device to the host compositor.

The fork is maintained at
[`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs). The
development branch is `virtwl`; the upstream `main` branch targets the
different virtio-gpu cross-domain path and is not the ChromeOS runtime
described here.

## What this fork is for

The `virtwl` branch is intended for Linux guests with a ChromiumOS-style
`virtio_wl` kernel interface, especially Crostini/Baguette. Fork-specific work
includes:

- VirtWL transport, shared-memory, dma-buf, and file-descriptor handling.
- ChromeOS shelf application-ID mapping through `zaura_shell`.
- ChromeOS keyboard and text-input integration, including Korean IME behavior.
- Protocol validation and lifecycle hardening with regression tests.
- A standalone IME sample GUI with explicit Korean/CJK font coverage.
- A user systemd template at
  [`packaging/systemd/sommelier-rs@.service`](packaging/systemd/sommelier-rs@.service).

This remains an independent fork while these changes are reviewed upstream.
The original Sommelier project and ChromiumOS implementation are linked for
reference:

- [Google sommelier-rs](https://github.com/google/sommelier-rs)
- [ChromiumOS Sommelier](https://chromium.googlesource.com/chromiumos/platform2/+/main/vm_tools/sommelier/)
- [ChromiumOS virtio_wl driver](https://chromium.googlesource.com/chromiumos/third_party/kernel/+/refs/heads/chromeos-5.4/drivers/virtio/virtio_wl.c)

## Current release

The current published binary is the
[`virtwl-v0.2.5` prerelease](https://github.com/kkimdev/sommelier-rs/releases/tag/virtwl-v0.2.5).

| Architecture | Binary |
| --- | --- |
| x86_64 | `sommelier_rs_virtwl-v0.2.5-x86_64` |
| aarch64/arm64 | `sommelier_rs_virtwl-v0.2.5-aarch64` |

Every release also includes `SHA256SUMS`. The release workflow builds on
GitHub-hosted Ubuntu 24.04 runners, runs the x86_64 regression suite, and
publishes the release only after both architecture builds succeed.

## Install and run in Crostini

### Prerequisites

The guest must provide `/dev/wl0` and a host Wayland compositor must be
available through VirtWL:

```bash
test -e /dev/wl0
printf '%s\n' "${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is required}"
```

`virtio_wl` is not part of the mainline Linux kernel, so ordinary
distribution kernels generally cannot use this mode.

### Download the current prerelease

For x86_64:

```bash
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.2.5/sommelier_rs_virtwl-v0.2.5-x86_64
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.2.5/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
chmod +x sommelier_rs_virtwl-v0.2.5-x86_64
```

For aarch64/arm64, use the `aarch64` asset instead:

```bash
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.2.5/sommelier_rs_virtwl-v0.2.5-aarch64
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.2.5/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
chmod +x sommelier_rs_virtwl-v0.2.5-aarch64
```

### Install with Nix

The repository flake defaults to the prebuilt release binary and supports
x86_64 and aarch64 Linux:

```bash
nix profile install github:kkimdev/sommelier-rs/virtwl#sommelier-rs-bin
```

Use `#sommelier-rs` instead to build the current `virtwl` source with Nix.
Both packages install a `sommelier-rs` executable.

### Start the proxy

The last positional argument is the guest display socket to create. The
current Crostini setup uses `wayland-2`:

```bash
./sommelier_rs_virtwl-v0.2.5-x86_64 \
  --virtio-wl /dev/wl0 --gpu-accel wayland-2
```

Use the aarch64 binary on arm64. Set the same display name for clients:

```bash
export WAYLAND_DISPLAY=wayland-2
your-wayland-application
```

`wayland-0` or another unused name is also valid when it matches the local
setup. If no display argument is supplied, the binary uses
`wayland-proxy-0`.

If an existing Sommelier instance owns the display, stop only that instance
before starting the replacement:

```bash
systemctl --user stop sommelier@0 sommelier@1
```

### Run as a user service

Install the binary and the provided template:

```bash
install -Dm755 ./sommelier_rs_virtwl-v0.2.5-x86_64 \
  "${HOME}/.local/bin/sommelier-rs"
install -Dm644 packaging/systemd/sommelier-rs@.service \
  "${HOME}/.config/systemd/user/sommelier-rs@.service"
systemctl --user daemon-reload
systemctl --user enable --now sommelier-rs@wayland-2.service
```

The service passes `--virtio-wl /dev/wl0` and the instance name to the proxy.
Use `systemctl --user status sommelier-rs@wayland-2.service` to inspect it.

## Build, test, and run from source

### Dependencies

On Debian-compatible guests:

```bash
sudo apt-get update
sudo apt-get install --yes \
  build-essential \
  pkg-config \
  libdrm-dev \
  libexpat1-dev \
  libgbm-dev \
  libxkbcommon-dev
```

### Commands

From the repository root:

```bash
# Build
cargo build --release

# Test
cargo test --workspace --all-targets -- --test-threads=1

# Verify formatting and lints
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Run the proxy from the local build
./target/release/sommelier --virtio-wl /dev/wl0 --gpu-accel wayland-2
```

The standalone IME GUI is useful for manual input and glyph verification:

```bash
WAYLAND_DISPLAY=wayland-2 cargo run -p sommelier-test-gui
```

If the system font is not sufficient for Korean text, set
`SOMMELIER_TEST_GUI_FONT` to a font file with Korean/CJK coverage.

For a repeatable runtime check, build both binaries and run the ignored GUI
smoke test. It starts a temporary `sommelier --gpu-accel` socket, launches the
sample GUI with `--auto-exit`, and verifies the connection, text-input/keymap
handshake, clean exit, and selected buffer transport:

```bash
cargo build --release -p sommelier -p sommelier-test-gui
cargo test -p sommelier --test gui_smoke -- --ignored --nocapture
```

The test accepts the validated SHM fallback when the current kernel reports
that VirtWL dma-buf allocation is unavailable. Set
`SOMMELIER_GUI_SMOKE_REQUIRE_GPU=1` to require a real linux-dmabuf allocation.
The GUI is bounded by a 15-second timeout; override it with
`SOMMELIER_GUI_SMOKE_TIMEOUT` when testing a slow compositor.
For a host-side compositor smoke test, set
`SOMMELIER_GUI_SMOKE_COMPOSITOR=/run/user/$(id -u)/wayland-0`; otherwise the
test uses `/dev/wl0`.

## Configuration

| Variable/option | Purpose |
| --- | --- |
| `XDG_RUNTIME_DIR` | Directory where the proxy creates its Wayland socket; required. |
| `WAYLAND_DISPLAY` | Display socket used by guest clients. |
| `SOMMELIER_VM_IDENTIFIER` | ChromeOS VM namespace used for shelf IDs; defaults to `termina`. |
| `SOMMELIER_ACCELERATORS` | Comma-separated host-handled accelerator keysyms. |
| `SOMMELIER_DRM_DEVICE` | Optional DRM render node override. |
| `SOMMELIER_TEST_GUI_FONT` | Font path used by the IME sample GUI. |
| `--virtio-wl PATH` | VirtWL device path; Crostini normally uses `/dev/wl0`. |
| `--xdg-decoration` | Enable XDG decoration forwarding. |
| `--local-compositor PATH` | Use a local compositor for debugging instead of VirtWL. |
| `--window-host-policy guest\|arc` | Startup-only application-ID policy for placement shortcuts; defaults to `guest`. `arc` rewrites IDs even when geometry is `none`; use `guest + none` for a completely inactive feature. |
| `--window-geometry-method none\|bounds\|self-parent` | Startup-only geometry operation; defaults to `none`. `self-parent` is experimental and position-only. |
| `--window-shortcuts-config PATH` | Explicit TOML binding file. No file is read unless this option is supplied. |

Window shortcuts are disabled by default. To enable ordinary work-area
placement, select a geometry method and provide a config file:

```bash
./target/release/sommelier \
  --virtio-wl /dev/wl0 \
  --window-host-policy arc \
  --window-geometry-method bounds \
  --window-shortcuts-config "$HOME/.config/sommelier/window-shortcuts.toml" \
  wayland-2
```

The inline TOML schema, nine-grid example, validation rules, and `SIGHUP`
reload procedure are documented in
[`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).
Reloading a malformed file keeps the last known-good generation. A binding
that overlaps a parsed `SOMMELIER_ACCELERATORS` entry is a hard startup/reload
error because the host and Sommelier cannot safely own the same chord.

The older `SOMMELIER_WINDOW_BOUNDS_AS_ARC` and
`SOMMELIER_WINDOW_BOUNDS_SELF_PARENT` environment switches are retained only
for legacy in-process compatibility paths; the production CLI no longer reads
them. Use the explicit options above, and never enable the experimental
`self-parent` method on the shared system Sommelier instance.

## Design notes

- `sommelier/src/state/window_placement.rs` is the single owner of backend
  selection, wl_surface/Aura associations, per-toplevel origins, self-parent
  convergence, ARC session IDs, and host-sync barrier lifetimes; handlers
  cannot mutate those maps directly. Association/origin/barrier mutators are
  fallible and `#[must_use]`, and debug builds assert the reverse-map and
  teardown invariants after every mutation.
- [`sommelier/docs/ARC_TASK_AND_SESSION_IDS.md`](sommelier/docs/ARC_TASK_AND_SESSION_IDS.md) documents ARC task IDs, restore-session IDs, and the allocator used by this experiment.
- [`sommelier/docs/KEYBOARD_SHORTCUT_INHIBITION.md`](sommelier/docs/KEYBOARD_SHORTCUT_INHIBITION.md) documents ChromeOS accelerator acknowledgement and shortcut inhibition.
- [`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md) documents the inline shortcut schema and runtime reload contract.

## CI and release workflow

The `virtwl` branch CI builds x86_64 and aarch64, runs tests on x86_64, and
checks formatting and Clippy. Successful pushes also upload debug binaries to
the Actions run and create non-release tags such as
`virtwl-ci-<run-id>-<attempt>-<sha>`. Those artifacts expire after seven days;
the cleanup job keeps the ten newest CI build runs by default. Set the
repository variable `CI_KEEP_COUNT` to change that limit.

Maintainer releases use tags whose base version matches
`sommelier/Cargo.toml`. Optional `-rN` suffixes are accepted for rebuilds:

```bash
git fetch personal virtwl
git tag -a virtwl-v0.2.5 personal/virtwl -m "Release virtwl-v0.2.5"
git push personal refs/tags/virtwl-v0.2.5
```

The release workflow builds both architectures, creates `SHA256SUMS`, uploads
all assets while the release is a draft, and then publishes the prerelease.
The fork currently keeps `virtwl` as its only development branch.

## Repository layout

- `sommelier/` — proxy executable, protocol state, handlers, and VirtWL bridge.
- `nix/` — source and prebuilt-release Nix packages exposed by `flake.nix`.
- `wayland_codegen/` — build-time Wayland protocol code generator.
- `sommelier_test_app/` — standalone GUI/IME test client.
- `third_party/protocols/` — vendored Wayland and ChromeOS protocol XML.
- `packaging/systemd/` — user service template for VirtWL instances.
- `sommelier/docs/` — focused protocol and keyboard design notes.

## Scope and limitations

- X11 proxying is out of scope; this project is Wayland-focused.
- `--gpu-accel` enables the VirtWL linux-dmabuf PRIME bridge when the guest
  kernel and host compositor provide it. Guest `wl_shm` pixels are copied into
  the host-backed dma-buf; if allocation or mapping is unavailable, the proxy
  falls back to its validated VirtWL shared-memory path. This is not
  end-to-end zero-copy.
- A compatible guest kernel and host compositor are required.
- This is an independent fork, not an officially supported Google product or
  ChromiumOS component.

## License

The project retains the upstream Apache-2.0 license. See [LICENSE](LICENSE).
