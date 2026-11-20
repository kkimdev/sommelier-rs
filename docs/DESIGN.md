# Design

How the proxy is built and why. The [README](../README.md) owns what the
fork is for, how to install and run it, and its configuration;
[INVARIANTS.md](INVARIANTS.md) owns the rules every change must keep. This
document explains the structure those rules live in.

Feature documents own their own design:

- [Window placement shortcuts](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md):
  the placement contract, configuration resolution and reload, and a code
  map of every placement owner.
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

- `main.rs` is the only code that reads the command line and the
  environment. It hands both to `Settings::resolve` (`settings.rs`, the
  one list of the variables the proxy reads and of what a value that
  cannot be used falls back to), raises the open-file soft limit
  (`raise_open_file_limit`), and passes the immutable `Settings` to
  `proxy::run` (test: `only_main_reads_the_environment`). The host is one
  `HostTransport`, a VirtWL device or, for debugging, a local
  compositor socket; the command line rejects both at once
  (test: `the_host_is_one_transport`).
- `proxy::run` builds what every connection shares (`Shared`): the
  settings behind an `Rc`, the virtio-gpu render node (`VirtGpuNode`;
  the node opens when a client first needs it, and one that fails to
  open is tried again for the next client until it opens; test:
  `a_render_node_that_fails_to_open_is_tried_again_then_kept`) and the
  placement configuration. Each connection's `Context::new` takes the
  settings and its `ConnectionFacts`: its VirtWL channel, the render
  node, whether its client gets wheel steps (`client_kind.rs`, the one
  owner of whether a client is Wine, starts deciding that from the
  client's process when it connects;
  [pointer scrolling](INVARIANTS.md#pointer-scrolling)) and the
  placement configuration. A `Context` is whole when it is built; no
  setting of it changes later.
- `proxy::run` claims the socket through its lock file, then `serve`
  accepts guest clients and gives each a `Client` with its own guest
  connection, host connection and `Context`. All clients run on one
  current-thread Tokio runtime inside a `LocalSet`. Each `Context` holds
  xkb state, which is `!Send`; keeping every task on one thread avoids
  locks and makes that requirement part of the future's type. One thread
  also means that a call that waits for a peer stalls every client, so
  nothing on it waits on a guest client or on host data (see
  [process invariants](INVARIANTS.md#process)).
- The host side is VirtWL (`virtwl_channel.rs`, ioctls in `virtwl.rs`) or,
  for debugging only, a local compositor socket (`--local-compositor`).
  `connection.rs` hides the difference behind `WaylandConnection`, which
  lends every receive and send one buffer of its own, a Unix socket's
  control buffer or a VirtWL `Transaction`, so neither allocates.
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
32 messages bound the wait to about one frame.

A batch costs no send of its own. `Client` keeps what the batches
produce for each peer (`to_host`, `to_guest`), appended in the order a
send per batch would have sent it, and sends it in as few transport
submissions as the 28-descriptor and VirtWL transaction-size limits
allow, each ending at a message boundary: a submission once a later
message no longer fits it, the rest (`Client::flush`) before a batch in
the other direction, before a fifth batch in the same one
(`MAX_BATCHES_PER_FLUSH`), before `run` waits, and when the session
ends. Each submission is a synchronous VirtWL round trip, so a burst in
one direction costs about one per 3,980 bytes rather than one per batch;
per-message VirtWL ioctls would add latency without changing the byte
and descriptor order (see
[transport invariants](INVARIANTS.md#transport-and-file-descriptors)).
A batch's output therefore waits behind no batch in the other direction,
so a host key event goes out before the next guest batch, as fair
dispatch intends, and behind at most three more batches in its own. A
long one-way stream still awaits a send every four batches, where it
awaited one every batch; as before, Tokio's cooperative budget decides
when such an await lets another client run. Interleaved directions send
once per switch, as one send per batch did. `proxy::harness::Session`
dispatches what one peer sent and then flushes, so tests of the
interleaving dispatch a batch at a time (`Session::batch`).

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
build time: `sommelier/build.rs` lists the protocols and writes one
`protocols.rs`, which `main.rs` includes as `protocols`. For each protocol
it generates:

- Typed decoders and a handler trait per interface (`WlSurfaceHandler`)
  with one `on_*` function per message, which takes the connection's
  `Context`, the message's sender and its arguments, and returns
  `Action::Forward` or `Action::Drop`; the default forwards. The sender
  and every ID argument come as the object table resolved them, typed by
  their side (see [Object translation](#object-translation)): a request
  has a `RequestSender`, a guest object, and an object argument is a
  `RequestObject` and a `new_id` a `GuestId`; an event has an
  `EventSender`, a host object, an object argument is an `EventObject`
  and a `new_id` a `HostId`. A nullable object is an `Option`; an object
  of no interface (`wl_display.error`'s) may name anything, and stays a
  wire `u32`. No handler reads an ambient sender. A request in `HAND_WRITTEN_REQUESTS` has no default and
  returns nothing, so the generated code never forwards it:
  `wl_registry.bind`, whose `new_id` names no interface, so only its
  handler knows what it creates. The generated code reaches the proxy
  only through the object table (`ctx.shadow_table`) and the `Context`
  lifecycle wrappers, never through a handler module.
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
  message the proxy builds itself, whose IDs, the sender's included, are
  of one ID space: a request's a `wire::RequestId`, the proxy's a
  `HostId`, which makes a `wire::ToHost`, and an event's a
  `wire::EventId`, the proxy's a `GuestId`, which makes a `wire::ToGuest`.
  A test peer names its objects by their wire `u32`s (see
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
  `offered_version` never offers a forwarded global above its generated
  `VERSION`.
- A `NAME` constant, the interface's name. Handlers name an interface
  by it, never by a copied literal.

`generate_routing` derives what spans every protocol from the same list,
so the proxy never restates which protocol owns an interface or how new
it is:

- `protocols::Iface`, one variant per vendored interface, with its
  `name()` (the `NAME`), its `version()` (the `VERSION`) and
  `from_name`, for the interface names the wire carries (a host global, a
  `wl_registry.bind`). The object table keeps each object's `Iface`, and
  a `match` on one is exhaustive, so code that must treat every
  interface, such as `state::lifecycle::forget_object`, names each.
- `ProtocolHandlers`, every interface's handler trait, which
  `handler::Handlers` implements.
- The one flat dispatch: `dispatch_request`, by the resolved sender's
  `Iface`, and `dispatch_event`, by the variant of the `AnyEvent` the
  proxy decoded the host event into. `proxy::dispatch` decodes a host
  event whole, once (`AnyEvent::decode`), before anything reads it: the
  keyboard core's recovery (`handler::keyboard::before_host_event`), the
  stale-event filter, and then its handler. The filter drops every event
  of an object the guest destroyed, except the ones its interface's owner
  declares end what the destroy left (`handler::outlives_destroy`: a
  buffer's `release` while the host shows it, and an asynchronous
  dma-buf create's `created` or `failed`). A host event that does not
  decode ends the session before any of them sees it, unless only the
  proxy uses its object (see
  [Wire messages](INVARIANTS.md#wire-messages)).

Encoders and forwards size their `MessageBuilder` up front, from their
arguments or the received payload, and it writes the payload after room
for the header, which building fills in: a message is one allocation and
is not copied again before it joins its peer's pending output.

### Adding an interface or a global

The compiler names every place that must change, and one test names the
one list it cannot check:

1. **A protocol.** Vendor its XML in `third_party/protocols/` and add its
   path to `sommelier/build.rs`. Each of its interfaces becomes an `Iface`
   variant, and the build stops at every exhaustive match until the
   interface has a row in it:
   - `handler::registry::policy`, the one table of host globals: whether
     the proxy forwards a host global of the interface (and up to which
     version), hides it, binds it for its own use, or advertises a global
     of its own in its place. A forwarded global needs nothing more. An
     interface that is no global is hidden.
   - `state::lifecycle::forget_object`: whether its objects have state of
     their own outside their table entry to forget when they end. State
     only the object owns goes in its entry instead (an `ObjectData`
     variant; see [Object ends](#object-ends)), whose `match` in
     `forget_object` says whether ending it does more than drop it.
   - `ProtocolHandlers`: `handler::Handlers` implements its handler
     trait, either in the module that owns the interface or, if every
     message forwards as it is, in the list in `handler/mod.rs`.
2. **A handler.** Override the trait's `on_*` functions for the messages
   whose meaning changes across the boundary, in the owning module, and
   keep its per-object state with that module (see
   [Object ends](#object-ends)).
3. **A global the proxy binds for itself, once per connection.** A
   `Singleton` variant (`state/singletons.rs`) and its interface
   (`Singleton::interface`, a `match`); the variant in `Singleton::ALL`,
   the order the proxy binds singletons in when several have none, which
   `every_interface_has_one_global_policy` holds to the table; its
   `BindInternal` row in `policy`; a `<global>_global` function in
   `registry.rs` that decides what the guest sees in its place, and its
   arm in `on_global`'s `match` on the singleton; its bind
   (`Singleton::bind`) and its release
   (`reset_internal_binding_for_global`), each a `match` on the
   singleton. Its binding is an entry of `Context::globals`, which other
   modules read (`ctx.globals.singletons().host(Singleton::Shm)`).

Tests: `every_interface_has_one_global_policy`,
`every_interface_resolves_by_its_name`.

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
  before the handler runs, which validates and translates it together.
  `resolve_request` and `resolve_event` resolve a message's sender before
  anything of it is read, into a `RequestSender` or `EventSender`: its
  ID, `Iface`, version and paired object, which is all dispatch and the
  handler need of it, so a dispatcher cannot run for a sender that did
  not resolve. `request_object` returns a `RequestObject`, the guest
  object and its `Link` (a host object, or local), and `event_object` an
  `EventObject`, the host object and its guest object. An event that names a
  stale object is dropped; one that names an object of another interface
  is dropped too, and the first on a connection warns, since the host
  broke the protocol (test: `an_event_object_of_another_interface_is_warned_of`).
  The warning is once per connection because each `Context` owns its own
  `ShadowTable`, which keeps the flag; later ones log at debug level.
  A guest `new_id` must be a free client ID (`guest_new_id`), and a
  host-created object must have a fresh server ID (`host_new_id`).

Guest and host ID spaces are independent, and so are their types: a
`GuestId` names a guest object and a `HostId` a host one, and only the
table makes either, from an ID it allocates or a wire value it resolves
(above), so state keyed by one, an encoder or an effect queue cannot take
the other. Tests make them with `for_test`. The proxy allocates host IDs
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
encode wire bytes ad hoc. `handler::Handlers` is the one handler type: a
unit struct that implements every interface's generated trait, each in
the module that owns the interface, and lists the interfaces whose
messages all forward as they are in `handler/mod.rs`. It holds no state,
so a handler acts only on the `Context` it is given; the XKB context the
keymaps are compiled in belongs to the keyboard core
(`keymap::Compiler`), next to the keymaps compiled from it.

`Context` holds sub-states, not loose collections: each is a type whose
fields are private to the module that owns its concern, so a write from
any other module does not compile, and others read it through narrow
methods. The registry owns the host's globals and the proxy's bindings
(`registry::Globals`), the keyboard core its keyboards, keys and focus
(`keys::KeyboardCore`), `handler::shm` the host's SHM formats
(`ShmFormats`), `handler::linux_dmabuf` the dma-buf capabilities
(`Dmabuf`), and so on for the IME core, outputs, internal callbacks,
placement and clipboard pumps. State that one object owns is in that
object's table entry instead ([Object ends](#object-ends)).

| Area | Handlers (`sommelier/src/handler/`) | State owners (`sommelier/src/state/`, `state.rs`) |
| --- | --- | --- |
| Display, registry, callbacks | `display.rs`, `registry.rs` (and `policy`, the table of host globals), `callback.rs` | `ShadowTable`, `Context`, `registries.rs` (`RegistryRecord`, one per host `wl_registry`), `singletons.rs` (the proxy's own bindings), `InternalCallbacks` |
| Surfaces, viewports, xdg-shell, SHM | `compositor.rs` (including `set_app_id` and the XDG toplevels' Aura objects), `buffer.rs` (every guest `wl_buffer`'s lifetime), `shm.rs` and a submodule per concern ([Buffers](#buffers)) | `render.rs` (`SurfaceState`, `SurfaceMapping`, `RenderBufferRegistry`), `host_mapping.rs` (`HostMapping`); `shm::ShmFormats`; each `wl_shm`, `wl_shm_pool` and `wp_viewport`'s entry |
| Dma-buf | `linux_dmabuf.rs` | `linux_dmabuf::Dmabuf` (capabilities), each params, factory and feedback object's entry, `allocator.rs` |
| Keyboard and IME | `keyboard.rs` (the keyboard core's adapter), `pointer.rs` (and wheel steps for touchpad scrolling), `touch.rs`, `text_input.rs` (the IME core's adapter), `seat.rs` | `input.rs` (pointer focus); `sommelier/src/keys` (the keyboard core: the keyboard focus, one `Keyboard` record per host keyboard, key generations, Backspace leases, held-key recovery, host accelerators); `sommelier/src/keymap.rs` (compiling a keymap, and its XKB state); the IME core, `sommelier/src/ime` (text inputs) |
| Clipboard | `data_device.rs` (including `pump`) | `ClipboardPumps` in `state.rs` |
| Outputs | `registry.rs`, `output.rs` | `outputs.rs` (`OutputRegistry`) |
| ChromeOS shell metadata | `aura_objects.rs`, `gtk_shell.rs` | `aura_surfaces.rs`, `xdg_roles.rs`, each GTK shell and surface's entry |
| Window placement | `window_placement.rs` | `window_placement.rs` |

Two mechanisms let a handler act outside its own message:

- `Context::defer_until_forwarded` queues work that must follow the bytes
  being forwarded, such as a request that must reach the host after the
  guest's own message.
- `callback::queue_internal_sync` issues a host-only `wl_display.sync` and
  `state::InternalCallbacks` records what it waits for, such as the
  text-input drain, the fence after a press's `reset`, or the
  placement barrier.

### Window placement

Window placement is opt-in and experimental: a connection has it exactly
when the bindings file is valid as the connection starts, and Sommelier
binds no chord of its own. The contract is in
[Window placement shortcuts](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).

- **Configuration.** `placement_config.rs` owns where the bindings file
  is, what it binds (`Bindings`, one validated version, each named action
  resolved to its rectangle) and which version is current
  (`PlacementConfig`, which every connection shares). The one writer is
  `BindingsFile::reload`, which an inotify watch on the event loop calls
  as the path, or a path its links lead to, changes
  (`placement_config/watch.rs`); it swaps the version
  whole, between two messages, and one that cannot be used keeps the last
  good bindings. The key path reads no file (tests:
  `the_watcher_follows_every_change_of_the_file`,
  `the_watcher_follows_a_swap_of_the_directory_link`,
  `a_new_file_is_read_once_written`,
  `a_file_being_written_is_not_read_for_another_event`,
  `the_watcher_follows_the_target_of_a_link`,
  `the_watcher_follows_a_link_in_a_directory_it_cannot_write`,
  `a_chain_follows_each_link_on_the_path`,
  `load_and_watch_reloads_on_the_event_loop`,
  `a_bindings_file_edit_takes_effect_on_the_next_press`,
  `reload_follows_the_file`,
  `reload_follows_a_link_to_files_of_the_same_mtime`,
  `a_connection_takes_placement_from_the_file_when_it_starts`).

- **Shortcuts.** The keyboard knows no placement: it asks for a chord's
  binding (`accelerator::BindingId`), consumes the chord of one as a
  shortcut, and hands the binding to its subscriber,
  `window_placement::shortcut`, which resolves the binding's rectangle and
  places the focused window.
- **One module.** `handler/window_placement.rs` is the only placement
  code the protocol handlers call, one entry point per event; the table
  in its module docs is the one list of them. Each does nothing while
  placement is off.
- **Target resolution.** Which toplevel the keyboard focuses, the output
  it entered last, and the bounds the bound rectangle maps to in that
  output's work area (`resolve_target`).
- **The move.** Exo lets only ARC windows set their bounds, and an ARC
  window loses the ChromeOS IME, so placement moves a window with
  `zaura_surface.set_parent(self, dx, dy)` and sizes it with a configure
  the proxy sends the guest. A placement takes three steps (restore,
  move, resize), each ended by a host barrier
  (`InternalCallback::PlacementBarrier`), because the offset and the
  size depend on the origin Exo reports after the step before. Every XDG
  toplevel gets a host-only `zaura_toplevel` in screen coordinates, so
  that Exo reports origins, and its configures replace the XDG ones,
  which placement translates for the guest.
- **State.** `state/window_placement.rs` owns, per connection, each
  toplevel's Aura toplevel, its last reported origin, the states and
  serial of its newest configure, and the placement in flight, keyed by
  `GuestId` and `HostId`. It exists only while placement is on: an entry
  point takes it with `Context::placement_mut`, which is `None` while
  placement is off, so no path can reach it then.
- **What placement needs from the host.** `zaura_shell` new enough for
  the work area (`shell_bound`, `output_bound`).
- **Lifecycle.** Per-toplevel state ends through `release_toplevel`, which
  the owners' `forget_*` functions call from `forget_object` (see
  [Object ends](#object-ends)), and `PlacementState::assert_no_orphans`
  checks it after every batch in tests.
- **Off is the absence of placement.** With placement off a session's
  traffic is what it would be without placement (test:
  `placement_off_traffic_is_unchanged`).

### Object ends

State that only one object owns lives in the object's own table entry
(`state/object_data.rs`): an `ObjectData` of the object's interface, such
as a `wl_shm`'s formats, a `wl_shm_pool`'s file, a dma-buf params
object's planes and pending create, or a viewport's or GTK surface's
link to its `wl_surface`, which that surface's end clears. Only a live entry keeps data, the
table attaches it only to an object of its interface (`ShadowTable::attach`,
`create_local_with`; an object the generated code creates gets it after
its forward, `PostDispatch::Attach`), and the owning module reads it by
kind (`ShadowTable::data::<T>`). So such state cannot outlive its object,
be read under a reused ID, or be checked after the fact: the transition
that ends the object takes it. State that a core owns (the keyboard and
IME cores), that spans objects (`OutputRegistry`, `XdgRoles`), that every
frame scans (`SurfaceState`, which buffer references read), or that
outlives its guest object (render buffers) lives with its owner, keyed by the object's
guest or host ID. `state/lifecycle.rs` ends both with the object, whichever
way the object ends: each `Context` wrapper of a table transition that
ends an object runs one hook. A `wl_surface`'s `SurfaceState` is created
with the surface, by `wl_compositor.create_surface`; a live surface without
its state is an internal divergence that ends the session.

- `forget_object` runs once per object, at its first end: a guest
  destructor, a terminal event, a local destructor, a bind that never
  reached the host, or a host `delete_id` while the guest object was live
  (`DeletedHostId::Paired` reports `was_live`). It gets the data the
  entry kept, whose `match` says what ending each kind does beyond
  dropping it (a dma-buf factory's or feedback's hold on its global's
  capabilities), and its `match` on the interface is the inventory of
  per-object state kept elsewhere: each arm calls the owner's `forget_*`
  function (`forget_surface`, `forget_keyboard`,
  `forget_output`, `forget_registry`, ...). It is the one place `state`
  calls into handlers, because ending an object can send requests, such
  as an Aura release or an extended keyboard's destructor.
- `forget_host_id` runs when the host releases the ID, for state that
  outlives the guest object: a render buffer the host still shows.

What a hook sends joins the current message's effects, so the wire order
is the handler's. A destructor request's handler never ends its object
itself: it returns a `wire::Destructor`, which says where the destructor
goes (ahead of the handler's output, after it, or after what the end
sends), or that the host object stays (in use, or without a destructor),
and the generated dispatcher ends the object accordingly
(`wl_surface.destroy` releases the surface's Aura objects, then has its
destructor sent after them; `xdg_toplevel.destroy` and `wl_output.release`
have theirs sent after their end releases their Aura child).
In tests, `Context::assert_no_orphans` checks every per-object key kept
outside the table, and every object such state names, against the table; a test's `Client`,
the fuzzer's included, runs it after every batch that keeps the session.
A seat reference is the exception: it outlives its seat
([Object lifetimes](INVARIANTS.md#object-lifetimes)), so only its copies
are checked, against their keyboard's. It also checks the IME core's
invariants (`ImeCore::assert_invariants`, I1a to I7 of
[IME.md](IME.md#31-state)), and that each text input's drain
and press fence wait for a pending `InternalCallback::TextInput`.

### Message effects

Each message is a transaction. Its handler writes what the message does
beyond its forward into the `Context`: requests for the host
(`send_host`, a `wire::ToHost`), events for the guest (`send_guest`, a
`wire::ToGuest`),
work deferred until the forward (`defer_until_forwarded`), and, if the
message ends the session, a `state::SessionEnd`. A handler ends one with
`return ctx.reject(object, code, message)` for a protocol error or
`return ctx.fail(why)` for the proxy's own failure; the first protocol
or host error wins over an internal failure.

`proxy::dispatch` is that transaction without I/O: it takes one framed
message (`Frame`: its header, its payload, and the descriptors its
connection has queued), dispatches it, takes its `state::Effects` once
(`Context::take_effects`), and reports whether it was handled, ends the
session or waits for a fence, with what a trace records of it (test:
`a_frame_dispatches_without_a_connection`).
`Client::handle_msgs` is the shell around it: it frames each
connection's bytes, records the trace, parks a request that waits, ends
the batch (`Context::end_batch`, the one place batch-scoped state is
reset) and sends. The core commits the effects:

- If the message did not end the session, it queues the forward, then the
  output in the same direction, and the output back toward the message's
  source, each for its peer ([Fair dispatch](#fair-dispatch) says when it
  is sent).
- If it did, it drops the forward and every effect, which closes their
  descriptors, and the shell ends the batch. After the batch's earlier, valid
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
  wait: its planes are memfds and its contexts have no virtio-gpu
  device, so no fence is ever pending. `Client` parks the guest stream
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

Encoded output, a `wire::ToHost` or `wire::ToGuest`, never outlives its
message. State
that a later message releases, such as a placement's next step or a
text-input drain, stays structured, and the message that
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
- `V3Pending` (`ime/state.rs`) follows it for a text-input-v3 object's
  enable, surrounding text, content type and cursor rectangle, which
  `ime::TextInput::apply_commit` applies to `V3Committed`, except the cursor
  rectangle, which only the host keeps and the commit forwards. A commit
  whose surrounding text does not fit a v1 message resets it unapplied.
- `OutputRecord` (`outputs.rs`) is a deliberate exception: an output event
  edits a copy-on-write `Option<OutputState>` of the whole state, which
  `wl_output.done` publishes.

### Synthetic objects

Some guest objects have no host counterpart and are implemented inside the
proxy: `wl_shm` and its pools, `zwp_text_input_manager_v3` and its
children, and an inert `wl_output` whose bind crossed its global's removal,
with any `zxdg_output_v1` created for it (see
[Registry globals](INVARIANTS.md#registry-globals)).
They follow the
guest lifecycle locally, including the `delete_id` a real compositor would
send.

## Buffers

VirtWL can carry only descriptors the `virtio_wl` device allocated, so a
guest memfd cannot reach the host. The proxy therefore implements guest
`wl_shm` itself:

- Guest pools are never mapped in the proxy: the copy reads pixels with
  `pread` and `preadv` from the pool's file, so a client that shrinks the file after
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
  which only the kernel writes, as a read fills its rows: the
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
  `handler/buffer.rs` handles every guest `wl_buffer`,
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

The IME core (`sommelier/src/ime`, `Context::ime`) owns the state of
every text input: its field, its link to the host text input, its
composition and its barriers. It is a sans-IO state machine
([IME.md](IME.md#3-the-ime-core) section 3, which implements the
behaviour table of its section 2): every message that can
change that state reaches it as one `ime::Input` through `ImeCore::step`,
which returns the guest events, host requests and barriers, and
keyboard-concern requests it makes, and reads what it does not own (the
object table's surfaces and seats, the keyboard focus) through
`ime::Env`. `handler/text_input.rs` is its adapter: `text_input::ime`
hands it an input and applies the effects where the message is handled.
The inputs are the guest's `zwp_text_input_v3` requests and the object's
end, the events of the host's `zwp_text_input_v1` and its extension, the
`done` of the core's drain barriers and reset fences, each seat's
keyboard focus change (`keyboard.rs`), a surface's end (`compositor.rs`),
a `wl_seat` bind (`registry.rs`) and a pointer press or touch-down
(`pointer.rs`, `touch.rs`), each key press the keyboard concern
forwards to the guest, and the held key it recovers for an empty
confirmation, each an input the keyboard core hands it. Nothing outside
`ime` changes a text input, and only `ime/guest.rs` builds the v3
transaction events.

The bridge reconciles state rather than translating requests, because Exo
applies a request against state the proxy cannot see: it drops a content
type while no input method is attached, and attaching one resets the
type. `ime::LinkState` models what the host text input has applied, by
those rules, and the pure `ime::reconcile` returns the requests that
make it match the target the guest committed (`HostTarget`): the
activation step and, when the attached input method lacks it, the content
type. `step` sends them for every text input at the end of each input, so
between messages every link matches its target, but one that drains, one
whose activation no live seat can carry, and an attached one whose seat
has ended (I1b of [IME.md](IME.md#31-state)).
The surrounding text and cursor rectangle are not reconciled: Exo keeps
both regardless and edits its copy of the text as the input method
commits, so each guest commit forwards the ones it states, but for a
surrounding text that would only make Exo drop its composition, which
the core withholds (below). In the other
direction, the edits of Exo's input method reach the guest as v3
transactions, each closed by a `done`. Exo measures its deletes and
preedit regions in its own prediction of the text, so the core keeps a
replica of it per text input, `ime/mirror.rs`, which takes every request
the core sends at the `done` of a `wl_display.sync` glued to it, and every
event Exo sends. `ime/exo_view.rs` owns the mirror and whether the guest is in step with
Exo: it is the one model of Exo's text. The adapter gives a text input
an input method only with the host's extension at v11, which the mirror
needs. A delete or a region is measured in the mirror while the guest is
in step, and dropped otherwise; and a guest report that leaves out the preedit it
shows, which Exo holds inline, is withheld when the mirror finds that
preedit the only difference from a state Exo will hold. `ime/state.rs`
keeps only what the guest is shown. A guest commit that leaves the
field enabled is answered with a `done`, which Chromium waits for before
it sends more. A pointer
press or a touch-down on the surface of a composition ends it before the
guest gets the press, as mutter and KWin do: `pointer.rs` follows each
pointer's surface, `touch.rs` reads a down's own, the guest commits the
preedit it shows, and Exo is told to `reset`.
Behind the `reset` goes a fence, a host `wl_display.sync`. Until it
returns, the field's host composition events are dropped, because Exo
sent them for the composition the press committed. The rules are under
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime).

The keyboard core (`sommelier/src/keys`, `Context::keys`) is the IME
core's twin: a sans-IO state machine that owns every fact of the host
keyboards and their keys, takes one `keys::KeyInput` through
`KeyboardCore::step`, returns `keys::KeyEffects`, and reads the shortcut
bindings and the IME core's gate through `keys::Env`. It owns the keyboard
focus, which a focus input moves in the same step as what the move ends of
the keys, and which the IME core and placement read.
`handler/keyboard.rs` is its adapter. The two cores talk only through
typed messages: the IME core's `ime::KeyEffect`s are keyboard inputs, and
the keyboard core hands the IME core `ime::Input`s.
Keys can reach the guest twice: as a `wl_keyboard.key` and as the press
and release the proxy recovers for an empty IME confirmation; the host's
text-input keysyms are dropped. The keyboard core assigns each physical
key a generation and an owner (guest keyboard, IME recovery, or a
shortcut), so a press is released through the path that
delivered it, and never twice. Once the key is pressed again, only one
older generation still waits for its release; see
[Keyboard and IME](INVARIANTS.md#keyboard-and-ime).
[KEYBOARD.md](KEYBOARD.md) describes the core, each key's states and the
one transition each input makes. Keymaps are compiled locally with
xkbcommon (`keymap.rs`) to classify chords, which `Accelerator::of_key`
computes; the keyboard core owns the host accelerators,
`SOMMELIER_ACCELERATORS`, whose chords are acknowledged as not handled
through `zcr_extended_keyboard_v1.ack_key`, so Exo keeps them as
accelerators.

## ChromeOS integration

- **Shelf IDs.** Every `xdg_toplevel.set_app_id` and GTK application ID is
  forwarded through `zaura_surface.set_application_id` as
  `org.chromium.guest_os.<vm-id>.wayland.<app-id>`, so the ChromeOS shelf
  matches windows to the guest's `.desktop` files.
  `aura_objects::set_application_id` is the one place that sends it.
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
a child process of its own. A flag that makes a message once per
process (`warn_once::WarnOnce`, the one such flag, such as
`window_placement`'s `OLD_SHELL`) is the process's too: no test resets
one. A test passes the function that reads it a `WarnOnce` of its own, or
runs in its own process (`test_support::in_own_process`). `proxy::harness::Session` drives a whole
`Client` as its guest and host, decoding what the proxy sends each into
the generated message types and holding it to the protocol, as one end
of a connection that follows its objects as libwayland does
(`test_support::wire_peer`); CONTRIBUTING says how to write a test with
it. `proxy::frame_cost` measures what a steady stream of frames costs a
session's proxy, in heap allocations and in the system calls that copy
pixels into or map a host copy, which the test binary counts per thread
(`test_support::counting`, which also counts the bytes allocated), and
holds each to a budget. In the same way, a test in `ime::tests` holds
typing in the IME core to the same allocations whatever the length of
Exo's text (test: `typing_costs_the_same_whatever_the_length_of_exos_text`). Run them with `--nocapture` to see the numbers. Beyond
unit tests, `proxy::fuzz` feeds whole clients random guest streams on the
DST's scheduler, behind the host display the DST plays (`sim::display`),
`sim` types into simulated text-input clients through a whole client on a
virtual clock and checks their text against the specification and the
user's intent, `proxy::replay` replays recorded host traces (below),
and
`sommelier/tests/weston_e2e.rs` runs
real clients through the proxy against a headless weston; CONTRIBUTING
says how to run them. `sommelier_test_app` is a standalone GUI for manual
IME and glyph checks, and `sommelier/tests/gui_smoke.rs` (ignored by
default) drives it through a real proxy on a Crostini VirtWL device; see
the README.

### Recorded host traces

With `SOMMELIER_TRACE_DIR` set, `start_client` gives each `Client` a
`trace::Recorder`, which writes a file of the connection's facts and each
message `Client::handle_msgs` handles, once it is handled or ends the
session, with the text of a host keymap, then what the batch gave each
peer (`trace.rs` owns the format). Unset, `Client` has no recorder and
the batch does nothing more. `proxy::replay` gives a `Session`'s proxy
the trace's facts, has each peer send it what the trace says the proxy
handled from that peer, a batch at a time, records the replay the same
way, and requires the same records, no Exo mirror divergence or drop,
and the protocol with both peers; a VirtWL channel is a stand-in
(`VirtWaylandChannel::stand_in_for_test`). `Trace::decoded` prints a
trace message by message.
The corpus is [`traces/`](../traces/README.md), which says how to record
on a Chromebook and what a replay stands in for (test:
`every_recorded_trace_replays`). A DST case's recording replays too, and
recording changes nothing the case sees (test:
`a_dst_cases_wire_trace_replays`).

## Not in scope

X11 proxying, the upstream virtio-gpu cross-domain path, and end-to-end
zero-copy SHM are out of scope; see
[Scope and limitations](../README.md#scope-and-limitations).
