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
python3 -I scripts/check-doc-links.py
git diff --check
```

Tests run in parallel, so a test must not assume which descriptor
numbers or global state another test leaves alone; see
[Testing](docs/DESIGN.md#testing).

- Read [docs/INVARIANTS.md](docs/INVARIANTS.md), and section 2 of
  [docs/IME.md](docs/IME.md#2-behavioural-specification) for IME code,
  before changing the code they cover. Keep every rule, or change the
  rule and its tests in the same pull request.
  [docs/DESIGN.md](docs/DESIGN.md) explains the structure.
- Every behavior change needs a regression test that fails before the
  change and passes after it.
- State each documentation fact once and link to it elsewhere: the README
  owns usage and configuration; `docs/` the design, the rules, the IME and
  the keyboard; `sommelier/docs/` the window placement contract;
  [HANDOVER.md](HANDOVER.md) the open issues; and the module docs the
  mechanism, which no document restates.
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
  session ended with one `wl_display.error`, its last event.
- Read everything each peer received: a `Session` dropped with output
  the test never read fails. Each peer holds the proxy to the protocol
  as libwayland would (`test_support::wire_peer` lists the checks). A
  test that gives the proxy an object no peer can have it create, such as
  a native dma-buf buffer, pairs it with `Session::pair_for_test`.
- Learn host IDs from what the proxy forwards (`create`, `surface`), not
  from the object table. The peers name objects by their wire `u32`s; state
  the proxy keeps for an object is keyed by a `GuestId` or a `HostId`,
  which a test makes with `Object::guest_id`, `Object::host_id` or
  `for_test`. `Session::ctx()` reads state no peer can
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
  The tests in `handler/keyboard/tests/recovery.rs` share the session
  fixture of the keyboard's tests, `Kb`.
- A test outside the keyboard core (`sommelier/src/keys`) sets its state
  up through the core's step, as the guest's and the host's messages
  would (`handler::keyboard::for_test`), never through its transitions,
  which are private to it.
- Use `MessageBuilder` only for frames no encoder builds, such as
  malformed ones.
- The documentation cites tests by name in "Test:" and "Tests:" clauses,
  and a rule cites at most three, the ones that fail when it breaks.
  `scripts/check-cited-tests.py` checks, in every tracked Markdown file
  outside `third_party/`, that each names one test, and not a test of a
  DST model, which pins the model rather than the proxy;
  `scripts/check-doc-links.py` checks the links, their anchors and the
  quoted rule titles.

## Randomized tests

`proxy::fuzz` feeds a whole proxy client random, protocol-shaped guest
streams on the DST's scheduler; its module docs say what a case does and
must keep. The suite runs 6000 cases from seed 0; run more, from another
seed, after changing how the proxy reads guest requests or tracks
objects:

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

`sim` is a deterministic simulation test (DST) of the IME bridge;
[docs/IME.md section 4](docs/IME.md#4-the-dst-harness) describes it, and
the `sim` module docs its settings. The suite runs 600 seeds from seed 0
in about 5 s. Run more after changing the IME code or a model, in
release mode, which is several times faster:

```bash
SOMMELIER_DST_CASES=100000 SOMMELIER_DST_SEED=1000000 \
  cargo test --release -p sommelier --bins sim::
```

A failure prints its seed, each failed check with its tier and field, the
end of the trace (`SOMMELIER_DST_TRACE_LINES` sets how much) and the
case's choices. Rerun it with `SOMMELIER_DST_SEED=<seed>
SOMMELIER_DST_CASES=1`, which replays it exactly;
`SOMMELIER_DST_CLIENT=chromium|gtk3|gtk4` and
`SOMMELIER_DST_MODE=free|settled` narrow a run.

The first failed case of a run is shrunk (`sim/shrink.rs`) after its
report, and the report ends with `SHRUNK from ...`, the shrunk case's own
report, and a `Regression { ... }` snippet that replays it. To add a
regression, fix the bug, then paste the snippet into `CASES` in
`sim/regressions.rs`, give it a name that says what it caught and a
`reaches` check of the counts that show the state the bug needs, and run
`cargo test -p sommelier --bins sim::regressions`; it must fail on the old
code and pass on the fix. A regression fails, with what went wrong, once
its replay draws other choices than it stores (a draw the models added or
re-ranged, or a message the proxy added or dropped), or once its schedule
no longer reaches its state: rule out a regression of the proxy, then
shrink it again, or find a schedule of its actions that reaches the state
again.

## Recorded host traces

`traces/` holds wire traces recorded on a Chromebook, which
`proxy::replay` replays through a `Session`;
[traces/README.md](traces/README.md) says what a replay checks and how to
record a trace. Run the replays with
`cargo test -p sommelier --bins proxy::replay`.

## End-to-end test

`sommelier/tests/weston_e2e.rs` runs real clients through
`sommelier --local-compositor` against a headless weston: `wayland-info`
must list the globals, and `weston-simple-shm` must draw for a few seconds
and exit cleanly while the proxy logs no error. It needs `weston` and
`wayland-info` on `PATH` (Debian and Ubuntu packages `weston` and
`wayland-utils`) and passes with a note without them; CI installs them on
x86_64 and sets `SOMMELIER_E2E_REQUIRED=1` so a missing one fails.

## Live VirtWL test

The VirtWL ioctls need a real `virtio_wl` device, so the test of
`virtwl_channel` that drives one (test:
`live_try_recv_eagain_then_async_recv_wakes`) is ignored by default. Run
it in a Crostini container, or another guest whose `/dev/wl0` you can
read and write and whose host compositor answers; set
`SOMMELIER_TEST_VIRTWL` to use another device path:

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
