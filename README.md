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
[`virtwl-v0.4.4` prerelease](https://github.com/kkimdev/sommelier-rs/releases/tag/virtwl-v0.4.4).

0.4.4 fixes keys typing many times after a short tap with an input method
that types letters itself, such as English (US): the proxy took Exo's empty
confirmations for autorepeats and recovered them as keys, and since that
engine answers every text update with such a confirmation, a held key fed a
loop. A held key is now recovered only if it ended a composition the client
showed (Korean Space after 가, Backspace emptying a composition). 0.4.3 fixed
a rare doubled Hangul character; 0.4.2 fixed Wayland EGL falling back to
llvmpipe with `--gpu-accel`; 0.4.1 added `ext-data-control-v1`.
See [configuration](#configuration).

| Architecture | Binary |
| --- | --- |
| x86_64 | `sommelier_rs_virtwl-v0.4.4-x86_64` |
| aarch64/arm64 | `sommelier_rs_virtwl-v0.4.4-aarch64` |

Every release also includes `SHA256SUMS`. See
[CI and release workflow](#ci-and-release-workflow) for how releases are
built.

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

The prebuilt binary links against libgbm and libxkbcommon and needs glibc
2.34 or newer. On Debian-compatible guests:

```bash
sudo apt-get install --yes libgbm1 libxkbcommon0
```

### Download the current prerelease

For x86_64:

```bash
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.4.4/sommelier_rs_virtwl-v0.4.4-x86_64
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.4.4/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
chmod +x sommelier_rs_virtwl-v0.4.4-x86_64
```

For aarch64/arm64, use the `aarch64` asset instead:

```bash
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.4.4/sommelier_rs_virtwl-v0.4.4-aarch64
curl -fLO https://github.com/kkimdev/sommelier-rs/releases/download/virtwl-v0.4.4/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
chmod +x sommelier_rs_virtwl-v0.4.4-aarch64
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
./sommelier_rs_virtwl-v0.4.4-x86_64 \
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
install -Dm755 ./sommelier_rs_virtwl-v0.4.4-x86_64 \
  "${HOME}/.local/bin/sommelier-rs"
install -Dm644 packaging/systemd/sommelier-rs@.service \
  "${HOME}/.config/systemd/user/sommelier-rs@.service"
systemctl --user daemon-reload
systemctl --user enable --now sommelier-rs@wayland-2.service
```

The service passes `--virtio-wl /dev/wl0 --gpu-accel` and the instance name to
the proxy.
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
  libxkbcommon-dev
```

### Commands

From the repository root:

```bash
# Build
cargo build --locked --release -p sommelier

# Test
cargo test --locked --workspace --all-targets

# Run the proxy from the local build
./target/release/sommelier --virtio-wl /dev/wl0 --gpu-accel wayland-2
```

[CONTRIBUTING.md](CONTRIBUTING.md#before-you-open-a-pull-request) lists the
full set of checks CI runs, and
[`rust-toolchain.toml`](rust-toolchain.toml) pins the Rust toolchain;
rustup installs it on first use.

The standalone IME GUI is useful for manual input and glyph verification:

```bash
WAYLAND_DISPLAY=wayland-2 cargo run -p sommelier-test-gui
```

If the system font is not sufficient for Korean text, set
`SOMMELIER_TEST_GUI_FONT` to a font file with Korean/CJK coverage.

For a repeatable runtime check, build the sample GUI and run the ignored GUI
smoke test. It starts the proxy that Cargo built for the test on a temporary
`--gpu-accel` socket, launches the newest local build of the sample GUI with
`--auto-exit`, and verifies the connection, text-input/keymap handshake,
clean exit, and selected buffer transport:

```bash
cargo build --release -p sommelier-test-gui
cargo test --release -p sommelier --test gui_smoke -- --ignored --nocapture
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

Sommelier reads the command line and the variables once, at startup.

| Setting | Purpose |
| --- | --- |
| The positional argument (`[DISPLAY]` in `--help`) | Name of the Wayland socket the proxy serves, under `XDG_RUNTIME_DIR`; defaults to `wayland-proxy-0`. An absolute path is the socket's path itself, as libwayland clients read an absolute `WAYLAND_DISPLAY`. Guest clients connect with `WAYLAND_DISPLAY` set to it; the proxy itself does not read `WAYLAND_DISPLAY`. |
| `XDG_RUNTIME_DIR` | Directory where the proxy creates its Wayland socket; required, and not empty. |
| `SOMMELIER_VM_IDENTIFIER` | ChromeOS VM namespace used for shelf IDs; defaults to `termina`. |
| `SOMMELIER_ACCELERATORS` | Comma-separated chords, such as `Super_L` or `<Alt>tab`, that the host compositor keeps. See [host accelerators](docs/KEYBOARD.md#host-accelerators). |
| `SOMMELIER_WHEEL_FROM_TOUCHPAD` | Which clients get mouse-wheel steps (`wl_pointer.axis_discrete`) for touchpad scrolling, which Wine apps such as KakaoTalk need to scroll: `auto` (the default) Wine clients, `all` every client, `off` none. With `all`, a client that scrolls on both the steps and the smooth `axis`, as GTK, Qt and Firefox may, can scroll twice as far. See [the rule](docs/INVARIANTS.md#pointer-scrolling). |
| `SOMMELIER_DRM_DEVICE` | Optional DRM render node override. |
| `SOMMELIER_TRACE_DIR` | Record each client connection's Wayland traffic, as a file of its own in this local directory, for tests to replay. Off when unset. A trace holds everything typed into the client and the text of the field it was typed into, so set it only on a separate proxy, for a scenario typed for it: see [recorded host traces](traces/README.md). |
| `SOMMELIER_TEST_GUI_FONT` | Font path used by the IME sample GUI. |
| `--virtio-wl PATH` | VirtWL device to reach the host compositor through; defaults to `/dev/wl0`. |
| `--local-compositor PATH` | Reach a compositor socket on this machine instead, for debugging. It cannot be combined with `--virtio-wl`. |
| `--gpu-accel` | Copy guest `wl_shm` buffers into host dma-bufs when VirtWL can allocate them (see [Scope and limitations](#scope-and-limitations)). |
| `--xdg-decoration` | Enable XDG decoration forwarding. |
| `--no-data-control` | Do not offer `ext-data-control-v1`. It is offered by default, so a client without keyboard focus, such as `wl-copy`, `wl-paste` or Orca's copy buttons, can set and read the ChromeOS clipboard while a window of this display has the focus; with a ChromeOS window focused, a read finds the clipboard empty and a copy is cancelled. Any client that reaches the socket can then read and replace the clipboard without the focus, except a sandboxed one that came through `wp_security_context_v1`, as a Flatpak app does; see [data-control](docs/DESIGN.md#data-control) for why that is offered by default. |
| `$XDG_CONFIG_HOME/sommelier-rs/window-placement.toml` | Optional bindings for window placement, which is opt-in and experimental: chords that move the focused window to a region of its output's work area. Without the file, placement is off. See [placement configuration](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md), which owns the contract. |

## CI and release workflow

The `virtwl` branch CI builds, tests, and checks formatting and Clippy on
x86_64 and aarch64, each on a native GitHub-hosted Ubuntu 24.04 runner, with
the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml).
Successful pushes also upload the debug `sommelier` binaries to the Actions
run, where they expire after seven days.

Maintainer releases use tags whose base version matches
`sommelier/Cargo.toml`. Optional `-rN` suffixes are accepted for rebuilds:

```bash
git fetch personal virtwl
git tag -a virtwl-v0.4.4 personal/virtwl -m "Release virtwl-v0.4.4"
git push personal refs/tags/virtwl-v0.4.4
```

The release workflow builds both architectures on the same native runners
and runs the regression suite on each. It then runs each binary it will
upload with `--version` and rejects one that needs a newer glibc than the
current baseline. Once both architectures pass, it creates `SHA256SUMS` and
publishes the prerelease with all assets; `gh release create` keeps the
release a draft until every upload has finished.

## Repository layout

- `sommelier/` — proxy executable, protocol state, handlers, and VirtWL bridge.
- `nix/` — source and prebuilt-release Nix packages exposed by `flake.nix`.
- `wayland_codegen/` — build-time Wayland protocol code generator.
- `sommelier_test_app/` — standalone GUI/IME test client.
- `third_party/protocols/` — vendored Wayland and ChromeOS protocol XML.
- `packaging/systemd/` — user service template for VirtWL instances.
- `docs/` — [design](docs/DESIGN.md), the binding
  [runtime invariants](docs/INVARIANTS.md), the [IME](docs/IME.md) and
  the [keyboard](docs/KEYBOARD.md).
- `traces/` — [recorded host traces](traces/README.md), which tests
  replay.
- `sommelier/docs/` — the
  [window placement](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md)
  contract.

## Scope and limitations

- X11 proxying is out of scope; this project is Wayland-focused.
- `--gpu-accel` enables the VirtWL linux-dmabuf PRIME bridge when the guest
  kernel and host compositor provide it. Guest `wl_shm` pixels are copied into
  the host-backed dma-buf; if allocation or mapping is unavailable, the proxy
  falls back to its validated VirtWL shared-memory path. This is not
  end-to-end zero-copy.
- A compatible guest kernel and host compositor are required.
- The IME needs the host's `zcr_text_input_extension_v1` at v11 or later
  (ChromeOS M116 or so, August 2023 on). On an older host, text inputs
  get no input method, and the proxy warns once.
- This is an independent fork, not an officially supported Google product or
  ChromiumOS component.

## License

The project retains the upstream Apache-2.0 license. See [LICENSE](LICENSE).
