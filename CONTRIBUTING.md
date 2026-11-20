# Contributing

This fork carries ChromeOS VirtWL support for
[sommelier-rs](https://github.com/google/sommelier-rs); the
[README](README.md) explains its scope. Issues and pull requests are
welcome at [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs).

## Branches

- `virtwl` is the only development branch. Open pull requests against it;
  a stacked pull request may target its parent branch, and CI runs on every
  pull request.
- Upstream `main` targets the virtio-gpu cross-domain runtime, not VirtWL.
  Do not base work on it.
- Releases are `virtwl-v*` tags; see
  [CI and release workflow](README.md#ci-and-release-workflow).

## Before you open a pull request

Install the build dependencies listed in the README. `rust-toolchain.toml`
pins the Rust toolchain CI uses, and rustup installs it on first use. Then
run the checks CI runs, plus `git diff --check`, from the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
python3 -I scripts/check-cited-tests.py
git diff --check
```

Tests run in parallel, so a test must not assume which descriptor
numbers or global state another test leaves alone; see
[Testing](docs/DESIGN.md#testing).

- Read [docs/INVARIANTS.md](docs/INVARIANTS.md) before changing transport,
  object lifetimes, SHM or dma-buf, keyboard and IME, or application-ID
  code. Keep every rule there, or change the rule and its tests in the same
  pull request. [docs/DESIGN.md](docs/DESIGN.md) explains the structure.
- Every behavior change needs a regression test that fails before the
  change and passes after it.
- State each documentation fact once and link to it elsewhere: the README
  owns usage and configuration, `docs/` the design and invariants, and
  `sommelier/docs/` each feature's contract.
- Never commit transient files; `*.local.*` and `_local/` are ignored for
  them.

## Writing tests

Pin what the proxy sends, not how it stores it. `proxy::harness::Session`
plays both peers of a whole `Client`: send it messages built with the
generated `req::` and `evt::` encoders, set objects up through real
protocol flows, and assert what each peer received, decoded into typed
messages that compare with `==`:

```rust
let mut s = Session::new();
let callback = s.new_id();
let sync = wl_display::req::sync(WL_DISPLAY_ID, callback);
let host_callback = s.create(sync).await;
s.host([wl_callback::evt::done(host_callback, 0)]).await;
assert_eq!(
    s.guest_received(),
    [
        (callback, wl_callback::Event::Done { callback_data: 0 }.into()),
        (WL_DISPLAY_ID, wl_display::Event::DeleteId { id: callback }.into()),
    ]
);
```

- `guest`, `host` and their `_fatal` forms send messages and say whether
  the session must survive them; `guest_received_and_error` checks that a
  session ended with one `wl_display.error`, its last event. A guest
  request that waits for a fence holds the guest's stream until
  `resume`.
- Read everything each peer received: a `Session` dropped with output
  the test never read fails.
- Each peer receives the proxy's bytes over a real socket, follows its
  objects as libwayland does, and fails on output the protocol forbids
  (`test_support::wire_peer`):
  - a message from, or naming, an object the peer does not have, one of
    another interface, one the sending side ended, or null where the
    argument may not be;
  - a message newer than its object's version;
  - a new object under an ID outside the sender's range, still in use,
    or more than one past the highest that range has used;
  - a `delete_id` of an object no destructor ended, or a second one: the
    host may end a live guest object on its own only if it is a frame
    callback of a surface that ended, or the test allows it with
    `Session::host_may_end`;
  - a request from, or naming, a host object after the proxy read the
    host's `delete_id` for it: the proxy reads each host event as the
    test sends it, so no request can cross one;
  - a header that cannot be framed, part of a message, or a descriptor
    no message took, once the proxy has sent all it holds.

  A test that gives the proxy an object no peer can have it create, such
  as a native dma-buf buffer, pairs it with `Session::pair_for_test`,
  which both peers learn.
- Learn host IDs from what the proxy forwards (`create`, `surface`), not
  from the object table. `Session::ctx()` reads state no peer can
  observe, or injects a fault no peer can cause; each use says which, in
  a comment. Every batch that keeps the session checks that no state
  outlives its object.
- A handler test decodes a queue with `test_support::decoded`.
- A module's tests live in its `tests.rs` beside it (`handler/keyboard.rs`'s
  in `handler/keyboard/tests.rs`).
- A handler test outside `handler/text_input/tests.rs` that sets up or reads
  text-input state no peer can observe goes through
  `test_support::text_input`, so that a change of the IME core's storage
  rewrites one module. It sets up only states the core can reach, which
  the core's invariants check.
  The tests in `handler/keyboard/tests/recovery.rs` share the `text_input`
  test fixture (`text_input::tests`) and need no other setup.
- A test outside the keyboard core (`sommelier/src/keys`) sets its state
  up through the core's step, as the guest's and the host's messages
  would (`handler::keyboard::for_test`), never through its transitions,
  which are private to it.
- Use `MessageBuilder` only for frames no encoder builds, such as
  malformed ones.
- The documentation cites tests by name in "Test:" and "Tests:" clauses.
  `scripts/check-cited-tests.py` checks, in every tracked Markdown file
  outside `third_party/`, that each names one test, and not a test of a
  DST model, which pins the model rather than the proxy.

## Randomized tests

`proxy::fuzz` feeds a whole proxy client random, protocol-shaped guest
request streams built from the vendored XML, some of them damaged, in
random pieces. Their strings are sometimes not UTF-8, and sometimes
nearly as long as a message, so that replacing their invalid bytes can
make a forward too long. Half the cases end like a client typing into
text fields (`proxy::fuzz::typing`): it moves the focus between its
surfaces, waits for each move with a roundtrip, and enables, edits and
disables the field; now and then it releases its seat while the field
is enabled, reuses the seat's ID for a surface once it is deleted, and
disables the field, which the proxy can then no longer deactivate. A
quarter of the cases run with window placement on, so every XDG
toplevel gets a host-only Aura toplevel; the run reports how many were
created.

Each case runs on the DST's scheduler (`proxy::fuzz::world`, below): the
pieces of the guest's stream, what the proxy sends each peer and the
host's answers each arrive after a latency the seed draws, so a seed
fixes a case's timing as well as its input, and a replay is exact; two
cases in 50 run twice and must leave the same trace. The proxy is a
`proxy::harness::Session`, so its two peers hold everything it sends
to the protocol (`test_support::wire_peer`, above). The host is the
display the DST plays too (`sim::display`), with the Exo model behind
it (`sim::exo`): it advertises every global, and one more output that
it removes at once, so a bind of it crosses the removal; answers each
sync and destructor; creates objects of its own (a data offer for each
data device, a buffer for each asynchronous dma-buf create), as weston
does; destroys a destroyed surface's pending frame callbacks without
`done`; and gives each new keyboard the keymap and each keyboard the
seat's focus. Exo drops a content type while no input method is
attached, spends a pending surrounding-text support on the next content
type, keeps surrounding text and the cursor rectangle regardless, and
sends `enter` and `leave` as an input method attaches and detaches. The
seed picks the host's `zcr_text_input_extension_v1` version: the
newest, or in a quarter of the cases one too old for the proxy to give
a text input an input method. Around the display, the host gives the
seat's focus to each surface the proxy commits; as it answers a sync,
composes on the focused field, clicks and taps there, and now and then
takes its input method away, as when its process restarts, so that the
proxy must activate the field again; holds its answer to a text input's
drain barrier until it answers anything else, so a transaction the guest
sends meanwhile reaches the proxy first and the proxy defers the
field's activation, unless the guest waited with a second roundtrip;
and in a quarter of the cases ends the live objects of one interface on
its own as it answers some syncs.

Each case must end without a panic, close every descriptor, and send at
most one `wl_display.error`, as its last event. A case whose framing is
intact must also either answer the stream's closing `wl_display.sync`
or send that error. After every batch that keeps the session, the proxy
must hold no state for an object that has ended
(`Context::assert_no_orphans`). The guest takes a `delete_id` of an
object it did not destroy only for a frame callback the host cancelled
with its surface, or an object of the interface the host ends on its
own; when its requests are whole and undamaged, every cancelled
callback must get one before the closing sync is answered. What
Exo holds of a field at the end is the DST's to check (`sim`, below).
The run reports how many input methods restarted, how many host
activations had their seat end while active, and how many deactivates
named a seat the host had ended.

A case that goes on for 100,000 deliveries hangs, and fails. One case
in 30 also runs through `Client::run` over real sockets (test:
`client_run_serves_guest_streams_over_sockets`), behind the same host,
where the kernel decides the timing: its event loop must serve both
sides until the guest leaves, keep the protocol with both, which the
guest of an undamaged case checks as the session's guest does, and close
every descriptor. A scheduled case has the proxy receive its bytes in
pieces of a size its seed draws.

The suite runs 6000 cases from seed 0; run more, from another seed,
after changing how the proxy reads guest requests or tracks objects:

```bash
SOMMELIER_FUZZ_CASES=200000 SOMMELIER_FUZZ_SEED=1000000 \
  cargo test -p sommelier --bins proxy::fuzz
```

A failure names its seed and prints the end of its trace
(`SOMMELIER_FUZZ_TRACE_LINES` sets how much); rerun that seed alone with
`SOMMELIER_FUZZ_SEED=<seed> SOMMELIER_FUZZ_CASES=1`, which replays it
exactly, and add `RUST_LOG=debug` to see the proxy's log, which the test
prints with the failure.

## Simulation test

`sim` is a deterministic simulation test (DST) of the IME bridge. It
types into models of Chromium's and GTK's text-input-v3 clients through
a whole proxy client (`proxy::harness::Session`), with a model of Exo
and its Korean input method behind it. Every message travels a FIFO
channel with a drawn latency on one virtual clock, so a seed fixes the
whole case, timing included; [docs/IME.md](docs/IME.md#4-the-dst-harness)
describes it. One test holds the proxy to the specification (section 2
of that document), whatever the timing: after every message the proxy
handles, the transactions the guest gets and what it then shows, the
preedit's cursor included, and the activations and resets the proxy asks
of the host (test: `ime_reaches_the_specified_text`). The other also
holds the client to the user's intent, in cases that settle around every
click, tap, focus move and restart of the input method, and after a key
the input method passes while it takes the engine to compose, whose
confirmation then resets the engine (test:
`ime_reaches_the_intended_text`). Exo's deletes and preedit regions must reach the
guest as the specification measures them in the simulated Exo's text,
or not at all; the summary counts the proxy's drops by cause
(`Counts::drops`) and those of a guest that held Exo's text
(`drops_truth_in_step`), and a settled case with a drop the Exo mirror
could have measured fails. Each test runs in a
process of its own, with the XKB defaults pinned.

The suite runs 600 seeds from seed 0, an even seed free and an odd one
settled, in about 5 s; two cases in 50 run twice and must leave the same
trace, and a run fails if it never met a variant a case draws or a path
the mode is there to reach, such as Exo's own detach (H2). Run more
after changing the IME code or a model:

```bash
SOMMELIER_DST_CASES=100000 SOMMELIER_DST_SEED=1000000 \
  cargo test -p sommelier --bins sim::
```

A failure prints its seed, each failed check with its tier and field, the
end of the trace (`SOMMELIER_DST_TRACE_LINES` sets how much) and the
case's choices. Rerun it with `SOMMELIER_DST_SEED=<seed>
SOMMELIER_DST_CASES=1`, which replays it exactly;
`SOMMELIER_DST_CLIENT=chromium|gtk3|gtk4` and
`SOMMELIER_DST_MODE=free|settled` narrow a run.

The first failed case of a run is shrunk (`sim/shrink.rs`) after its
report: ddmin over its actions, zeroing runs of its choices (0 is the
minimum latency or the first variant), then dropping the trailing zeros,
within 2,000 re-runs or 60 s, keeping a step only if the same check still
fails. The report ends with `SHRUNK from ...`, the shrunk case's own
report, and a `Regression { ... }` snippet that replays it. To add a
regression, fix the bug, then paste the snippet into `CASES` in
`sim/regressions.rs`, give it a name that says what it caught and a
`reaches` check of the counts that show the state the bug needs, and run
`cargo test -p sommelier --bins sim::regressions`; it must fail on the old
code and pass on the fix. A regression also fails, with what went wrong,
once its replay draws other choices than it stores: a draw the models
added or re-ranged, or a message the proxy added or dropped, makes it
replay another schedule. If the proxy changed, rule out a regression,
then shrink it again, or, once the code before its fix is gone, find a
schedule of its actions that reaches the state again. And it fails once
its own schedule no longer reaches that state, which a change to what the
proxy or the models do there causes: find out why before shrinking it
again, as a regression of the proxy shows here. A failure that does not
replay from its choices, or a panic, is not shrunk.

## End-to-end test

`sommelier/tests/weston_e2e.rs` runs real clients through
`sommelier --local-compositor` against a headless weston: `wayland-info`
must list the globals, and `weston-simple-shm` must draw for a few seconds
and exit cleanly while the proxy logs no error. It needs `weston` and
`wayland-info` on `PATH` (Debian and Ubuntu packages `weston` and
`wayland-utils`) and passes with a note without them; CI installs them on
x86_64 and sets `SOMMELIER_E2E_REQUIRED=1` so a missing one fails.

## Live VirtWL test

Unit tests reach the VirtWL channel only through the functions that
handle what an ioctl returned; the ioctls themselves need a real
`virtio_wl` device. A test of `virtwl_channel` drives one (test:
`live_try_recv_eagain_then_async_recv_wakes`): it opens a
VirtWL context, checks that a receive on the empty context returns at
once and that an asynchronous receive waits, then sends a
`wl_display.sync` and waits up to five seconds for the host compositor's
`wl_callback.done`. It is ignored by default, because CI has no such
device. Run it in a Crostini container, or another guest whose
`/dev/wl0` you can read and write and whose host compositor answers;
set `SOMMELIER_TEST_VIRTWL` to use another device path:

```bash
cargo test -p sommelier --bins live_try_recv -- --ignored
```

The test opens a context of its own, a new connection to the host
compositor, so a Sommelier already running in the guest keeps working.

## Commit messages

Make small commits, one logical change each, in the form
`<emoji> <type>: <summary>`: an imperative summary under 72 characters, and
a body that explains why for anything non-trivial.

| Type | Emoji | Use |
| --- | :---: | --- |
| `feat` | ✨ | New capability |
| `fix` | 🐛 | Bug fix |
| `refactor` | ♻️ | Restructuring without a behavior change |
| `perf` | ⚡ | Performance |
| `test` | ✅ | Tests only |
| `docs` | 📝 | Documentation only |
| `ci` | 💚 | CI and release workflows |
| `chore` | 🔧 | Build, dependencies, tooling |
| `style` | 🎨 | Formatting |

## License

Contributions are accepted under the project's
[Apache License 2.0](LICENSE).
