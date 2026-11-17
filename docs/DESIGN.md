# Design

How the proxy is built and why. The [README](../README.md) owns what the
fork is for, how to install and run it, and its configuration;
[INVARIANTS.md](INVARIANTS.md) owns the rules every change must keep. This
document explains the structure those rules live in.

Feature documents own their own design:

- [Window placement shortcuts](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md):
  the placement contract, configuration resolution and reload, and a code
  map of every placement owner.
- [ARC task and session IDs](../sommelier/docs/ARC_TASK_AND_SESSION_IDS.md):
  why placement needs an ARC application ID, and the task-ID allocator.
- [Keyboard shortcut inhibition](../sommelier/docs/KEYBOARD_SHORTCUT_INHIBITION.md):
  how Exo routes keys and what `zcr_keyboard_extension_v1` changes.

## The proxy in one paragraph

Sommelier listens on a Wayland socket in the guest. For every guest client
it opens one connection to the host compositor (Exo) through the ChromiumOS
`virtio_wl` device, then translates each message in both directions: object
IDs, file descriptors, versions, and the few protocols that cannot cross
VirtWL as they are. Most messages are forwarded after translation; a
handler intercepts the ones whose meaning changes across the boundary.

## Process and connections

- `main.rs` parses the command line and resolves every process-wide setting
  once, before the first client connects: the VM identifier
  (`state::vm_identifier_from_environment`) and the placement configuration
  (`PlacementConfig::from_environment`). Connections receive the results.
  `Context::from_parts` and the test constructors read no environment
  (`test_contexts_read_no_environment`), with two exceptions below `main`:
  `Context::new` creates the GBM allocator, which reads
  `SOMMELIER_DRM_DEVICE` (`allocator.rs`) on every connection, and the ARC
  task-ID reservation reads `XDG_RUNTIME_DIR` (`arc_task_ids.rs`).
- `proxy::run` accepts guest clients and gives each a `Client` with its own
  guest connection, host connection and `Context`. All clients run on one
  current-thread Tokio runtime inside a `LocalSet`. The keyboard handler
  owns xkb state, which is `!Send`; keeping every task on one thread avoids
  locks and makes that requirement part of the future's type. `SIGHUP`
  reloads run on the same loop, between accepts, so a reload never races a
  key event.
- The host side is VirtWL (`virtwl_channel.rs`, ioctls in `virtwl.rs`) or,
  for debugging only, a local compositor socket (`--local-compositor`).
  `connection.rs` hides the difference behind `WaylandConnection`.

### Fair dispatch

`Client::run` handles at most `MAX_MESSAGES_PER_BATCH` (32) messages per
batch. When both directions have buffered messages it alternates between
them: after a batch in one direction it prefers the other, and
`next_buffered_direction` first tries a non-blocking receive on that
preferred side so bytes still in the kernel queue are not starved. A
client that streams render requests would otherwise hold keyboard and IME
events behind several frames on the single thread.
32 messages still amortize a send while bounding the wait to about one
frame. Translated messages of one batch are sent in one transport
submission; per-message VirtWL ioctls would add latency without changing
the byte and descriptor order (see
[transport invariants](INVARIANTS.md#transport-and-file-descriptors)).

## Protocol code generation

`wayland_codegen` turns the vendored XML in
[`third_party/protocols/`](../third_party/protocols/README.md) into Rust at
build time (`sommelier/build.rs`). For each protocol it generates:

- Typed decoders and a `ProtocolHandler` trait with one `on_*` method per
  message. A handler returns `Action::Forward` or `Action::Drop`; the
  default forwards.
- Pre-dispatch validation: object and `new_id` arguments are checked
  against the shadow table, so a repeated, null or reserved ID cannot
  overwrite a mapping, and each message is checked against the version
  that introduced it.
- `*_SINCE` and `VERSION` constants. Since-version checks use these, never
  copied literals, and a child object takes its parent's negotiated
  version. A global's version bound is either the generated `VERSION` or a
  named constant in `registry.rs` that documents why it deliberately
  differs: `wl_seat` v5 and `wl_output` v3 match ChromiumOS Sommelier's
  caps, `wl_compositor` and `xdg_wm_base` are capped at the versions whose
  semantics the proxy implements, and `zaura_shell` has a v6 floor.

`generate_routing` derives the interface-to-protocol routing table from the
same list, so the proxy never restates which protocol owns an interface.

## Object translation

`state::ShadowTable` maps every guest object ID to its host ID and records
each side's interface and negotiated version. Guest and host ID spaces are
independent: the proxy allocates host IDs itself, and host-created objects
get guest IDs in the server range (`0xff000000` and above).

Destruction is asymmetric. A guest destructor request is forwarded, but the
mapping survives until the host's `wl_display.delete_id` arrives, so late
host events can still be translated (and are filtered) and the `delete_id`
itself can be forwarded. Host-only objects the proxy created keep their ID
reserved the same way; `Context::retire_destroyed_host_object` is the one
way to retire one.

## Handlers and state

Handlers translate wire events and delegate policy; policy modules never
encode wire bytes ad hoc.

| Area | Handlers (`sommelier/src/handler/`) | State owners (`sommelier/src/state/`, `state.rs`) |
| --- | --- | --- |
| Display, registry, callbacks | `display.rs`, `registry.rs`, `callback.rs` | `ShadowTable`, `Context`, `InternalCallbacks` |
| Surfaces, viewports, xdg-shell, SHM | `compositor.rs` (including `set_app_id` and the XDG toplevels' Aura objects), `shm.rs` | `render.rs` (`SurfaceState`, `RenderBufferRegistry`) |
| Dma-buf | `linux_dmabuf.rs` | `Context` dma-buf state, `allocator.rs` |
| Keyboard and IME | `keyboard.rs`, `text_input.rs`, `seat.rs` | `input.rs` (focus, key generations, text input) |
| Clipboard | `data_device.rs` | clipboard pumps in `Context` |
| Outputs | `registry.rs`, `compositor.rs` | `outputs.rs` (`OutputRegistry`) |
| ChromeOS shell metadata | `aura_objects.rs`, `gtk_shell.rs`, `window_placement.rs` | `aura_surfaces.rs`, `xdg_roles.rs`, `window_placement.rs` |

Two mechanisms let a handler act outside its own message:

- `Context::defer_until_forwarded` queues work that must follow the bytes
  being forwarded, such as a request that must reach the host after the
  guest's own message.
- `callback::queue_internal_sync` issues a host-only `wl_display.sync` and
  `state::InternalCallbacks` records what it waits for, such as the
  text-input drain or the placement barrier.

### Synthetic objects

Some guest objects have no host counterpart and are implemented inside the
proxy: `wl_shm` and its pools, and `zwp_text_input_manager_v3` and its
children. They follow the guest lifecycle locally, including the
`delete_id` a real compositor would send.

## Buffers

VirtWL can carry only descriptors the `virtio_wl` device allocated, so a
guest memfd cannot reach the host. The proxy therefore implements guest
`wl_shm` itself:

- Guest pools are mapped in the proxy. Each guest `wl_buffer` gets host
  storage, chosen once by `select_shm_backing` (`shm.rs`): VirtWL shared
  memory laid out exactly like the guest buffer and imported by the host as
  a `wl_shm` pool, or, with `--gpu-accel` and a kernel that supports VirtWL
  dma-buf allocation, a host-backed PRIME dma-buf imported through the
  host's linux-dmabuf. `ShmBacking` records the choice, so the copy path
  re-checks nothing.
- On commit, the damaged region is copied from the guest pool into the host
  storage before the host `wl_surface.commit` is queued, so the host never
  samples stale or uninitialized pixels. A failed copy drops the commit and
  forces a full copy next time. This is a copy, not zero-copy.
- `RenderBufferRegistry` tracks each buffer's use by the host
  (`NeverSubmitted`, `AwaitingRelease`, `ReleaseOwed`, `Released`), so
  `wl_buffer.release` reaches the guest exactly when the host is done,
  including after the surface that showed it is destroyed.

Guest dma-buf (`linux_dmabuf.rs`) is forwarded rather than copied. For
classic virtio-gpu PRIME buffers, `allocator.rs` replaces the guest's
plane-0 stride and modifier with the kernel-reported values, because the
guest's are stale for host import. Before forwarding a commit, the proxy
waits for the guest GPU's fence, matching ChromiumOS Sommelier.

The same descriptor problem applies to clipboard pipes: `data_device.rs`
creates a VirtWL pipe and pumps bytes between it and the guest pipe.

## Keyboard and IME

Exo delivers IME input through `zwp_text_input_v1` and the
`zcr_text_input_extension_v1` extension, while GTK and other toolkits speak
`zwp_text_input_v3`. The proxy exposes a synthetic text-input-v3 manager
and bridges each guest v3 object to a host v1 object plus its extension.

Keys can reach the guest twice: as a `wl_keyboard.key` and as a
host-generated keysym from the IME. `state/input.rs` assigns each physical
key a generation and an owner (guest keyboard, IME, or a compositor
shortcut), so every press gets exactly one matching release no matter
which path delivers it. Keymaps are compiled locally with xkbcommon to map
keysyms back to keycodes and to classify chords; chords listed in
`SOMMELIER_ACCELERATORS` are acknowledged as not handled through
`zcr_extended_keyboard_v1.ack_key`, so Exo keeps them as accelerators.

## ChromeOS integration

- **Shelf IDs.** Every `xdg_toplevel.set_app_id` and GTK application ID is
  forwarded through `zaura_surface.set_application_id` as
  `org.chromium.guest_os.<vm-id>.wayland.<app-id>`, so the ChromeOS shelf
  matches windows to the guest's `.desktop` files.
  `window_placement::set_aura_application_id` is the one place that chooses
  this ID or the ARC identity placement needs.
- **Aura objects.** `aura_objects.rs` creates, versions and releases every
  host-only Aura child (`zaura_surface`, `zaura_toplevel`, `zaura_output`)
  from one table. The guest never sees them.
- **Outputs.** `OutputRegistry` owns bound outputs and their Aura metadata.
  The work area follows the `wl_output` geometry (panel) transform, not
  `zaura_output.logical_transform`, as ChromiumOS Sommelier does.

## Testing

Each fix lands with a regression test that fails before it; the test names
in [INVARIANTS.md](INVARIANTS.md) are anchors for the rules they pin. Tests
run serially (`--test-threads=1`) because some observe process-wide state
such as open file descriptors. `sommelier_test_app` is a standalone
GUI for manual IME and glyph checks, and `sommelier/tests/gui_smoke.rs`
(ignored by default) drives it through a real proxy; see the README.

## Not in scope

X11 proxying, the upstream virtio-gpu cross-domain path, and end-to-end
zero-copy SHM are out of scope; see
[Scope and limitations](../README.md#scope-and-limitations).
