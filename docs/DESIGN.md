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

- `main.rs` parses the command line, installs the `SIGHUP` handler and
  raises the open-file soft limit (`prepare_process`), and resolves every
  process-wide setting once, before the first client connects: the VM
  identifier (`state::vm_identifier_from_environment`) and the placement
  configuration (`PlacementConfig::from_environment`). Connections receive
  the results.
  `Context::from_parts` and the test constructors read no environment
  (test: `test_contexts_read_no_environment`), with two exceptions below
  `main`: `Context::new` creates the GBM allocator, which reads
  `SOMMELIER_DRM_DEVICE` (`allocator.rs`) on every connection, and the ARC
  task-ID reservation reads `XDG_RUNTIME_DIR` (`arc_task_ids.rs`).
- `proxy::run` claims the socket through its lock file, then `serve`
  accepts guest clients and gives each a `Client` with its own guest
  connection, host connection and `Context`. All clients run on one
  current-thread Tokio runtime inside a `LocalSet`. Each `Context` holds
  xkb state, which is `!Send`; keeping every task on one thread avoids
  locks and makes that requirement part of the future's type. `SIGHUP`
  reloads run in a task of their own on the same thread, so a reload never
  races a key event and never waits for an accept. One thread also means
  that a call that waits for a peer stalls every client, so nothing on it
  waits on a guest client or on host data (see
  [process invariants](INVARIANTS.md#process)).
- The host side is VirtWL (`virtwl_channel.rs`, ioctls in `virtwl.rs`) or,
  for debugging only, a local compositor socket (`--local-compositor`).
  `connection.rs` hides the difference behind `WaylandConnection`.
  `wire::MessageHeader::frame` cuts the received byte stream into messages,
  both for `Client::handle_msgs` and for the connection's readiness check.

### Fair dispatch

`Client::run` handles at most `MAX_MESSAGES_PER_BATCH` (32) messages per
batch. When both directions have buffered messages it alternates between
them: after a batch in one direction it prefers the other, and
`next_buffered_direction` first tries a non-blocking receive on that
preferred side so bytes still in the kernel queue are not starved. A
client that streams render requests would otherwise hold keyboard and IME
events behind several frames on the single thread. A parked guest stream
(see [Message effects](#message-effects)) is never offered.
32 messages still amortize a send while bounding the wait to about one
frame. Translated messages of one batch are sent in as few transport
submissions as the 28-descriptor limit allows, usually one; per-message
VirtWL ioctls would add latency without changing the byte and descriptor
order (see
[transport invariants](INVARIANTS.md#transport-and-file-descriptors)).

### Descriptors

A received descriptor is an `OwnedFd` from the moment `recvmsg` or the
VirtWL receive returns it, and its ownership moves with the message it
belongs to:

1. The connection queues it in stream order (`WaylandConnection::read_fds`).
2. Decoding an `fd` argument moves the next one into the generated
   `Request` or `Event`, which closes it if the message is not forwarded.
   The handler sees a `BorrowedFd`; one that keeps the descriptor (an SHM
   pool, a dma-buf plane, a clipboard pipe) duplicates it.
3. Forwarding moves it into the `wire::Outgoing` message, and the batch's
   `OutgoingMessages` closes it after the send, which passes the peer a
   copy.

A descriptor the proxy creates enters at step 3: the encoder that takes it
returns a `wire::Outgoing` that owns it. Outside the kernel and device
boundaries nothing takes over or closes a descriptor by number, so a
descriptor cannot be closed twice or leak on an early return; the rule is
under [transport invariants](INVARIANTS.md#transport-and-file-descriptors).

## Protocol code generation

`wayland_codegen` turns the vendored XML in
[`third_party/protocols/`](../third_party/protocols/README.md) into Rust at
build time (`sommelier/build.rs`). For each protocol it generates:

- Typed decoders and a `ProtocolHandler` trait with one `on_*` method per
  message. A handler returns `Action::Forward` or `Action::Drop`; the
  default forwards.
- Pre-dispatch validation: the sender, object and `new_id` arguments are
  resolved through the shadow table before the handler runs, so a
  repeated, null or reserved ID cannot overwrite an entry, and each
  message is checked against the version that introduced it (see
  [Object translation](#object-translation)).
- String arguments as `String`s, with sequences that are not UTF-8
  replaced, except the few that other arguments index by byte offset
  (`BYTE_INDEXED_STRINGS`), which decode to `wire::IndexedString` so their
  handler can ignore one that is not UTF-8 (see
  [Wire messages](INVARIANTS.md#wire-messages)).
- An encoder per message, `req::<request>` and `evt::<event>`, for every
  message the proxy builds itself (see
  [Wire messages](INVARIANTS.md#wire-messages)).
- Enum constants named `<ENUM>_<ENTRY>`, and `<enum>_since`, the version
  that introduced a value. A constant is `i32` when an `int` argument of
  the same protocol file carries its enum and `u32` otherwise, so
  `wl_output.transform` is `i32` although `preferred_buffer_transform`
  carries it as a `uint`.
- `*_SINCE` and `VERSION` constants. Since-version checks use these, never
  copied literals, and a child object takes its parent's negotiated
  version. A global's version bound is either the generated `VERSION` or a
  named constant in `registry.rs` that documents why it deliberately
  differs: `wl_seat` v5 and `wl_output` v3 match ChromiumOS Sommelier's
  caps, `wl_compositor` and `xdg_wm_base` are capped at the versions whose
  semantics the proxy implements, `wl_shm` is offered at v1, which the
  proxy implements itself, and `zaura_shell` has a v6 floor.
  `forwarded_global_version` never offers a forwarded global above its
  generated `VERSION`.
- A `NAME` constant, the interface's name. Handlers name an interface
  by it, never by a copied literal.

`generate_routing` derives the interface-to-protocol routing table, and
`interface_version`, each interface's `VERSION` by name, from the same
list, so the proxy never restates which protocol owns an interface or how
new it is.

## Object translation

`state::ShadowTable` (`state/objects.rs`) holds one entry per object on
each side of a connection, with the interface and negotiated version that
side knows it by:

- A guest object is local (implemented in the proxy; see
  [Synthetic objects](#synthetic-objects)) or paired with a host object,
  live or destroyed by the guest and awaiting its host's `delete_id`.
- A host ID is in use exactly while it has an entry: a paired object, a
  host-only object the proxy uses itself, or a retired object whose ID
  is kept until its `delete_id` or for the rest of the connection.
- A pairing is recorded in both entries.

The table's API keeps those entries consistent by construction:

- **Constructors** create an object with all its facts, under IDs that
  nothing holds: `create_paired` (and `create_bridged`, for a v3 text
  input backed by a v1 host object), `adopt_host_created` for an object a
  host event created, `create_local`, `create_host_only` and
  `create_display`.
- **Transitions** are named after what happened and change both sides at
  once: `guest_destroyed`, `guest_destroyed_in_use`,
  `guest_ended_host_kept`, `guest_ended_host_destroyed`,
  `local_destroyed`, `abandon` (for an object that never reached the
  host), `host_destroy_queued`, `retire_host` and
  `release_deleted_host_id`. Those that end a guest object are visible
  only in `state`; everything else ends one through its `Context` wrapper
  (see [Object ends](#object-ends)).
- **Generated dispatch** resolves each sender and object argument once,
  before the handler runs, which validates and translates it together:
  `request_sender` and `request_object` return a `Link` (a host ID, or
  local), and `event_object` returns the guest ID. An event that names a
  stale object is dropped; one that names an object of another interface
  is dropped too, and the first on a connection warns, since the host
  broke the protocol (test: `an_event_object_of_another_interface_is_warned_of`).
  The warning is once per connection because each `Context` owns its own
  `ShadowTable`, which keeps the flag; later ones log at debug level.
  A host-created object must have a fresh server ID (`accepts_host_new_id`).

Guest and host ID spaces are independent: the proxy allocates host IDs
itself, and host-created objects get guest IDs in the server range
(`0xff000000` and above). Both come from a `state::IdAllocator`, which,
like libwayland's object map, reuses the last freed ID before a fresh one,
so a long session never outgrows the peer's map.

Destruction is asymmetric. A guest destructor request is forwarded, but the
pairing survives until the host's `wl_display.delete_id` arrives, so late
host events can still be translated (and are filtered) and the `delete_id`
itself can be forwarded. Host-only objects the proxy created keep their ID
reserved the same way; `Context::retire_destroyed_host_object` is the one
way to retire one. The `delete_id` then ends everything the proxy knows
about the host ID and frees it (`Context::release_deleted_host_id`).

## Handlers and state

Handlers translate wire events and delegate policy; policy modules never
encode wire bytes ad hoc.

| Area | Handlers (`sommelier/src/handler/`) | State owners (`sommelier/src/state/`, `state.rs`) |
| --- | --- | --- |
| Display, registry, callbacks | `display.rs`, `registry.rs`, `callback.rs` | `ShadowTable`, `Context`, `registries.rs` (`RegistryRecord`, one per host `wl_registry`), `InternalCallbacks` |
| Surfaces, viewports, xdg-shell, SHM | `compositor.rs` (including `set_app_id` and the XDG toplevels' Aura objects), `buffer.rs` (every guest `wl_buffer`'s lifetime), `shm.rs` and a submodule per concern ([Buffers](#buffers)) | `render.rs` (`SurfaceState`, `SurfaceMapping`, `RenderBufferRegistry`), `host_mapping.rs` (`HostMapping`) |
| Dma-buf | `linux_dmabuf.rs` | `Context` dma-buf state, `allocator.rs` |
| Keyboard and IME | `keyboard.rs`, `pointer.rs`, `touch.rs`, `text_input.rs`, `seat.rs` | `input.rs` (focus, key generations, text input) |
| Clipboard | `data_device.rs` (including `pump`) | `ClipboardPumps` in `state.rs` |
| Outputs | `registry.rs`, `output.rs` | `outputs.rs` (`OutputRegistry`) |
| ChromeOS shell metadata | `aura_objects.rs`, `gtk_shell.rs` | `aura_surfaces.rs`, `xdg_roles.rs` |
| Window placement | `window_placement.rs` and a submodule per backend | `window_placement.rs` and a submodule per backend |

Two mechanisms let a handler act outside its own message:

- `Context::defer_until_forwarded` queues work that must follow the bytes
  being forwarded, such as a request that must reach the host after the
  guest's own message.
- `callback::queue_internal_sync` issues a host-only `wl_display.sync` and
  `state::InternalCallbacks` records what it waits for, such as the
  text-input drain, the fence after a press's `reset`, or the
  placement barrier.

### Window placement

Window placement is opt-in and experimental, and it is built so that
the rest of the proxy does not depend on how a window is moved. The
contract is in
[Window placement shortcuts](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).

- **One boundary.** `handler/window_placement.rs` is the only placement
  code the protocol handlers call, one entry point per event; the table
  in its module docs is the one list of them. Each dispatches on the
  connection's `WindowPlacementState` and does nothing while placement is
  off.
- **Shared target resolution.** Which toplevel the keyboard focuses, the
  output it entered last, and the bounds the bound rectangle maps to in
  that output's work area (`resolve_target`) are the same for every
  backend; a backend gets the resolved `PlacementTarget` and only moves
  the window.
- **Backends own their state.** `WindowPlacementState` has one variant
  per backend and `Off`, the default, which holds nothing. A backend's
  per-connection state (`state/window_placement/<backend>.rs`), its host
  objects and its requests (`handler/window_placement/<backend>.rs`) are
  reachable only through its variant, so another backend, or placement
  off, cannot touch them. There are two backends:
  - `arc`: an ARC policy identity, a host-only `zaura_toplevel` and
    `set_window_bounds`.
  - `remote_shell`: a host-only `zcr_remote_surface_v2` role from a
    `zcr_remote_shell_v2` bound like the other singletons, and
    `set_bounds_in_output`. Exo gives a surface one shell role, so the
    guest's XDG objects become local objects ("facades",
    [Synthetic objects](#synthetic-objects)): the compositor handler lets
    the boundary claim a new `xdg_surface`, and `Client::dispatch_request`
    lets it dispatch their requests, and the decoration manager's, whose
    decoration for a facade could not name it on the host. They keep
    their links in `XdgRoles`, so target resolution and the lifecycle
    hooks treat them as any XDG role.
- **What a backend needs from the host.** Both need `zaura_shell` for the
  work area (`shell_bound`, `output_bound`); `remote_shell` also needs its
  manager global (`wants_remote_shell` and the registry's `Singleton`).
- **Lifecycle through the boundary.** A backend's per-object state ends
  through the boundary's `release_toplevel` and `forget_surface`, which
  the owners' `forget_*` functions call from `forget_object` (see
  [Object ends](#object-ends)), and `WindowPlacementState::assert_no_orphans`
  checks it after every batch in tests.
- **Off is the absence of placement.** With placement off a session's
  traffic is what it would be without placement (test:
  `placement_off_traffic_is_unchanged`).

### Object ends

Per-object state lives with the module that owns it (`surfaces`,
`OutputRegistry`, the keyboard maps, and so on), keyed by the object's
guest or host ID. `state/lifecycle.rs` ends it with the object, whichever
way the object ends: each `Context` wrapper of a table transition that
ends an object runs one hook. A `wl_surface`'s `SurfaceState` is created
with the surface, by `wl_compositor.create_surface`; a live surface without
its state is an internal divergence that ends the session.

- `forget_object` runs once per object, at its first end: a guest
  destructor, a terminal event, a local destructor, a bind that never
  reached the host, or a host `delete_id` while the guest object was live
  (`DeletedHostId::Paired` reports `was_live`). Its one `match` on the
  interface is the inventory of per-object state: each arm calls the
  owner's `forget_*` function (`forget_surface`, `forget_keyboard`,
  `forget_output`, `forget_registry`, ...). It is the one place `state`
  calls into handlers, because ending an object can send requests, such
  as an Aura release or an extended keyboard's destructor.
- `forget_host_id` runs when the host releases the ID, for state that
  outlives the guest object: a render buffer the host still shows. It
  drops an asynchronous dma-buf create too, as a backstop, though
  `forget_params` already did at the params' end.

What a hook sends joins the current message's effects, so the wire order
is the handler's: a destructor handler that must order its own destructor
against that output queues the part that goes first before it calls the
wrapper (`wl_surface.destroy` releases the surface's Aura objects first).
In tests, `Context::assert_no_orphans` checks every per-object key, and
every object such state names, against the table; a test's `Client`,
the fuzzer's included, runs it after every batch that keeps the session.
A seat reference is the exception: it outlives its seat
([Object lifetimes](INVARIANTS.md#object-lifetimes)), so only its copies
are checked, against their keyboard's.

### Message effects

Each message is a transaction. Its handler writes what the message does
beyond its forward into the `Context`: requests for the host
(`client_to_host_queue`), events for the guest (`host_to_client_queue`),
work deferred until the forward (`defer_until_forwarded`), and, if the
message ends the session, a `state::SessionEnd`. A handler ends one with
`return ctx.reject(object, code, message)` for a protocol error or
`return ctx.fail(why)` for the proxy's own failure; the first protocol
or host error wins over an internal failure. After dispatch,
`Client::handle_msgs` takes the message's `state::Effects` once
(`Context::take_effects`):

- If the message did not end the session, it sends the forward, then the
  output in the same direction, and, once the batch ends, the output back
  toward the message's source.
- If it did, it drops the forward and every effect, which closes their
  descriptors, and ends the batch. After the batch's earlier, valid
  messages, `handler::display::session_error` encodes the session's one
  `wl_display.error`. Nothing else writes one.
- If it waits for a guest GPU fence (`return ctx.wait_for_fence(wait)`,
  which a `wl_surface.commit` of a busy dma-buf does before it changes
  anything), it leaves the message and everything after it buffered and
  ends the batch. Only a guest request with no descriptor arguments may
  wait: only the guest stream can be parked, and the descriptors would
  be gone when the request is dispatched again. A message that waits
  otherwise, or with a forward, output or deferred work, is a proxy bug:
  the session ends as an internal failure, before the deferred work runs
  (test: `a_wait_with_output_ends_the_session`). `proxy::fuzz` counts
  such an end like any other session end, but never reaches a fence
  wait: its planes are memfds and its contexts have no allocator, so no
  fence is ever pending. `Client` parks the guest stream
  (`ParkedRequest`): it neither reads nor dispatches guest messages, and
  fair dispatch does not offer them, while host events and other clients
  go on. Once the exported sync_file is readable, or, for the virtio-gpu
  fallback, which has no descriptor to wait on, a poll interval after it
  parked, it dispatches the same message again, which
  checks its fence afresh. The interval starts at `FENCE_POLL_INTERVAL`
  (2 ms) and doubles each time the message waits again, up to
  `MAX_FENCE_POLL_INTERVAL` (16 ms), since each ask costs several
  ioctls; it ends at a fixed instant, which host traffic waking the loop
  does not postpone. The deadline is set when the message first waits
  and kept across those dispatches. At the deadline the message is
  dispatched once more: if its fence has signalled by then, it goes
  through; if it still waits, the session ends as an internal failure.

A session that ends never handles another message (`Client` remembers
that it ended), so a fatal branch rolls nothing back: only output could
escape, and the loop drops it.

Effects cover only wire output and the fence wait. What a handler does
outside the wire takes effect at once: it starts clipboard pumps,
allocates a buffer's host storage and copies pixels into it. A session
that ends aborts its pumps (`Client::run`), and its `Context` frees the
rest.

Encoded output, a `wire::Outgoing`, never outlives its message. State
that a later message releases, such as the configures a placement barrier
holds or a text-input drain, stays structured, and the message that
releases it encodes it into its own effects.

### Double-buffered state

The convention for state that a request sets and a later commit applies
is one `#[derive(Default)]` struct per site, named for it. Requests write
its fields, and the commit applies them by destructuring
`std::mem::take(&mut self.pending)`, naming every field. Taking it resets
the pending state in one step, and a new field the commit never applies
becomes an unused variable, which CI rejects. There is no generic
`DoubleBuffered<T>`: each commit applies its fields in its own way.

- `SurfacePending` (`render.rs`) follows it for a `wl_surface`'s
  attachment, damage, scale, transform, offsets and viewport.
- `V3Pending` (`input.rs`) follows it for a text-input-v3 object's
  enable, surrounding text, content type and cursor rectangle, which
  `TextInputState::commit` applies to `V3Committed`, except the cursor
  rectangle, which only the host keeps and the commit forwards.
- `OutputRecord` (`outputs.rs`) is a deliberate exception: an output event
  edits a copy-on-write `Option<OutputState>` of the whole state, which
  `wl_output.done` publishes.

### Synthetic objects

Some guest objects have no host counterpart and are implemented inside the
proxy: `wl_shm` and its pools, `zwp_text_input_manager_v3` and its
children, and an inert `wl_output` whose bind crossed its global's removal,
with any `zxdg_output_v1` created for it (see
[Registry globals](INVARIANTS.md#registry-globals)), and, under the
remote-shell-v2 placement backend, the guest's `xdg_surface`,
`xdg_toplevel` and `xdg_popup` (see [Window placement](#window-placement)).
They follow the
guest lifecycle locally, including the `delete_id` a real compositor would
send.

## Buffers

VirtWL can carry only descriptors the `virtio_wl` device allocated, so a
guest memfd cannot reach the host. The proxy therefore implements guest
`wl_shm` itself:

- Guest pools are never mapped in the proxy: the copy reads pixels with
  `pread` from the pool's file, so a client that shrinks the file after
  `create_pool` makes a read fail instead of raising SIGBUS in the proxy.
  Each guest `wl_buffer` gets host storage, chosen once by
  `select_shm_backing` (`shm/storage.rs`): VirtWL shared memory laid out
  exactly like the guest buffer and imported by the host as a `wl_shm`
  pool, or, with `--gpu-accel` and a kernel that supports VirtWL dma-buf
  allocation, a host-backed PRIME dma-buf imported through the host's
  linux-dmabuf.
  With `--local-compositor` and no VirtWL, the shared memory is a memfd,
  with or without `--gpu-accel` (see
  [Surfaces, damage and SHM](INVARIANTS.md#surfaces-damage-and-shm)).
  `ShmBacking` records the choice, so the copy path re-checks nothing.
- On commit, the damaged region is copied from the guest pool into the host
  storage before the host `wl_surface.commit` is queued, so the host never
  samples stale or uninitialized pixels. A pool that cannot supply the
  pixels (a short read) is the client's error and ends the session with
  `wl_shm.invalid_fd`; a host storage failure (mapping, dma-buf sync) drops
  the commit and forces a full copy next time. This is a copy, not
  zero-copy. The proxy's mapping of the host storage is a `HostMapping`,
  which only the kernel writes, as `pread` fills a range of it: the
  host may read that memory at any time, so the proxy never holds a Rust
  reference to it.
- Damage carries its coordinate space in its type: `BufferRect` for
  `damage_buffer` and the copy, `SurfaceRect` for `wl_surface.damage` and
  the host, which takes nothing else. `SurfaceMapping` (`render.rs`) is
  the one conversion between them; see
  [Surfaces, damage and SHM](INVARIANTS.md#surfaces-damage-and-shm).
- `RenderBufferRegistry` tracks each buffer's use by the host
  (`NeverSubmitted`, `AwaitingRelease`, `ReleaseOwed`, `Released`), so
  `wl_buffer.release` reaches the guest exactly when the host is done,
  including after the surface that showed it is destroyed.
  `BufferHandler` (`handler/buffer.rs`) handles every guest `wl_buffer`,
  SHM or dma-buf: its destroy and release, and its deferred retirement.

`handler/shm.rs` holds a submodule per concern, and re-exports what other
handlers call:

| Submodule (`handler/shm/`) | Concern |
| --- | --- |
| `pool.rs` | The guest's `wl_shm` and `wl_shm_pool`: pool creation and resize, buffer validation, retiring and forgetting the objects |
| `formats.rs` | The one list of supported formats (`SUPPORTED_SHM_FORMATS`, also the source of the linux-dmabuf shim's DRM list); the ones the guest's `wl_shm` offers, from the host's capabilities; each format's DRM code and bytes per pixel |
| `storage.rs` | A buffer's host storage: choosing and allocating it, and creating the host buffer that imports it |
| `dmabuf.rs`, `dmabuf_layout.rs` | VirtWL dma-buf allocation and mapping; the validated dma-buf layout |
| `copy.rs` | The copy of damaged pixels from the pool into the host storage |
| `damage.rs` | Damage rectangles: clipping to a buffer and merging |

Guest dma-buf (`linux_dmabuf.rs`) is forwarded rather than copied. For
classic virtio-gpu PRIME buffers, `allocator.rs` replaces the guest's
plane-0 stride and modifier with the kernel-reported values, because the
guest's are stale for host import. Before forwarding a commit, the proxy
waits for the guest GPU's fence, matching ChromiumOS Sommelier, but
without stalling the event loop: the commit is parked and dispatched
again (see [Message effects](#message-effects)).

The same descriptor problem applies to clipboard pipes: for each
`wl_data_offer.receive`, `data_device.rs` sends the host a VirtWL pipe
and pumps what the host writes into the guest's descriptor.
`data_device::pump` waits on the VirtWL pipe, and on a guest pipe or
socket, through `AsyncFd` (a regular file it writes directly), without
touching the flags the guest's descriptor shares with the client. A peer
that stalls parks only its own transfer, until `PUMP_IDLE_TIMEOUT` ends
it. Ending it closes the guest's pipe as a whole transfer does, so the
guest cannot tell a cut-off paste from a complete one; a pipe has no way
to carry an error (see [process invariants](INVARIANTS.md#process)).
`Context::clipboard_pumps` (`ClipboardPumps`) owns a connection's pumps:
it gives a `PumpSlot`, the only way to start one, while fewer than
`MAX_CLIPBOARD_PUMPS` are in flight, and aborts them all when the
connection ends.

## Keyboard and IME

Exo delivers IME input through `zwp_text_input_v1` and the
`zcr_text_input_extension_v1` extension, while GTK and other toolkits speak
`zwp_text_input_v3`. The proxy exposes a synthetic text-input-v3 manager
and bridges each guest v3 object to a host v1 object plus its extension.

The bridge reconciles state rather than translating requests, because Exo
applies a request against state the proxy cannot see: it drops a content
type while no input method is attached, and attaching one resets the
type. `HostApplied` (`input.rs`) models what the host text input has
applied, by those rules, and the pure `reconcile` returns the requests
that make it match the target the guest committed (`HostTarget`): the
activation step and, when the attached input method lacks it, the content
type. `text_input::reconcile_host` sends them wherever the target or the
host's ability to apply it changes: a guest commit, a focus change, a
drain barrier's completion, Exo detaching the input method on its own (a
host `leave` while active), a `wl_seat` bind and the object's destroy.
The surrounding text and cursor rectangle are not reconciled: Exo keeps
both regardless and edits its copy of the text as the input method
commits, so each guest commit forwards the ones it states. In the other
direction, the edits of Exo's input method reach the guest as v3
transactions, each closed by a `done`; a delete, which Exo counts from the
start of the surrounding text, is measured against the text the guest last
committed. A guest commit that leaves the field enabled is answered
with a `done`, which Chromium waits for before it sends more. A pointer
press or a touch-down on the surface of a composition ends it before the
guest gets the press, as mutter and KWin do: `pointer.rs` follows each
pointer's surface, `touch.rs` reads a down's own, the guest commits the
preedit it shows, and Exo is told to `reset`.
Behind the `reset` goes a fence, a host `wl_display.sync`. Until it
returns, the field's host composition events are dropped, because Exo
sent them for the composition the press committed. The rules are under
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime).

Keys can reach the guest twice: as a `wl_keyboard.key` and as a
host-generated keysym from the IME. `state/input.rs` assigns each physical
key a generation and an owner (guest keyboard, IME, or a compositor
shortcut), so a press is released through the path that delivered it,
and never twice. Once the key is pressed again, only one older
generation per path still waits for its release, and a late IME keysym
release does not close its press; see
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime). Keymaps are
compiled locally with xkbcommon to map keysyms back to keycodes and to
classify chords; chords listed in
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
- **Outputs.** `OutputRegistry` owns live outputs and their Aura metadata.
  The work area follows the `wl_output` geometry (panel) transform, not
  `zaura_output.logical_transform`, as ChromiumOS Sommelier does. The
  guest sees each `wl_output` unchanged, and the host's
  `zxdg_output_manager_v1` passes through: on a fractionally scaled
  display, `wl_output.scale` is rounded up, so only `zxdg_output_v1` gives
  toolkits the logical monitor size (a 2560x1600 panel at 1.6x is
  1600x1000, not 1280x800).

## Testing

Each fix lands with a regression test that fails before it; the test names
in [INVARIANTS.md](INVARIANTS.md) are anchors for the rules they pin. Tests
run in parallel. A descriptor number can be reused by another test the
moment it is closed, so a test that checks a descriptor was closed checks
that no descriptor still refers to its object
(`test_support::any_fd_has_identity`), never its number; the randomized
`proxy::fuzz` test, which counts every open descriptor, runs its cases in
a child process of its own. A flag that makes a warning once per
process, such as `window_placement`'s `OLD_SHELL_WARNED`, is the
process's too: no test resets one. A test passes the function that reads
it a flag of its own, or runs in its own process
(`test_support::in_own_process`). `proxy::harness::Session` drives a whole
`Client` as its guest and host, decoding what the proxy sends each into
the generated message types; CONTRIBUTING says how to write a test with
it. Beyond unit tests, `proxy::fuzz` feeds whole
clients random guest streams, and `sommelier/tests/weston_e2e.rs` runs
real clients through the proxy against a headless weston; CONTRIBUTING
says how to run them. `sommelier_test_app` is a standalone GUI for manual
IME and glyph checks, and `sommelier/tests/gui_smoke.rs` (ignored by
default) drives it through a real proxy on a Crostini VirtWL device; see
the README.

## Not in scope

X11 proxying, the upstream virtio-gpu cross-domain path, and end-to-end
zero-copy SHM are out of scope; see
[Scope and limitations](../README.md#scope-and-limitations).
