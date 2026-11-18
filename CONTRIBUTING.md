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
- Learn host IDs from what the proxy forwards (`create`, `surface`), not
  from the object table. `Session::ctx()` reads state no peer can
  observe, or injects a fault no peer can cause; each use says which, in
  a comment. Every batch that keeps the session checks that no state
  outlives its object.
- A handler test decodes a queue with `test_support::decoded`.
- Use `MessageBuilder` only for frames no encoder builds, such as
  malformed ones.
- [INVARIANTS.md](docs/INVARIANTS.md) and [DESIGN.md](docs/DESIGN.md)
  cite tests by name in "Test:" and "Tests:" clauses;
  `scripts/check-cited-tests.py` checks that each names a test.

## Randomized tests

`proxy::fuzz` feeds a whole proxy client random, protocol-shaped guest
request streams built from the vendored XML, some of them damaged, over
real sockets. Their strings are sometimes not UTF-8, and sometimes
nearly as long as a message, so that replacing their invalid bytes can
make a forward too long. Half the cases end like a client typing into text fields
(`proxy::fuzz::typing`): it moves the focus between its surfaces, waits
for each move with a roundtrip, and enables, edits and disables the
field; now and then it releases its seat while the field is enabled,
reuses the seat's ID for a surface once it is deleted, and disables the
field, which the proxy can then no longer deactivate. A quarter of the
cases run with the remote-shell-v2 placement backend, so the proxy binds
the compositor's `zcr_remote_shell_v2` and implements the guest's XDG
objects itself; the run reports how many facade windows got a remote
surface. Each case must end
without a panic or hang, close every descriptor, and send at most one
`wl_display.error`, as its last event. A case whose framing is intact
must also either answer the stream's closing `wl_display.sync` or send
that error. The fake compositor behind the proxy also checks that every
request it receives comes from a live host object, creates only fresh IDs
and names live objects of the right interface; it deletes
destroyed objects at once, so the proxy's host ID reuse is exercised,
creates objects of its own (a data offer for each data device, a buffer
for each asynchronous dma-buf create), as weston does, destroys a
destroyed surface's pending frame callbacks without `done`, and gives the
keyboard focus to each surface the proxy commits. In a quarter of the
cases it also destroys the live objects of one interface on its own as
it answers some syncs. After every batch that keeps the session, the proxy
must hold no state for an object that has ended
(`Context::assert_no_orphans`). When a case's
requests are whole and undamaged, the guest checks that it receives what
the proxy should send, which is stricter than what libwayland-client
tolerates: every event comes from and names only objects it has, a
`wl_display.delete_id` names only an object it destroyed, a frame
callback the compositor cancelled, or an object the compositor destroyed
on its own, once (and every cancelled callback
gets one before the closing sync is answered), and each object the server
creates gets a fresh ID from the server range.

The compositor also models what Exo makes of the text-input requests
(`proxy::fuzz::exo`, which cites the Chromium source of each rule): it
drops a content type while no input method is attached, spends a pending
surrounding-text support on the next content type, keeps surrounding
text and the cursor rectangle regardless, and sends `enter` and `leave`
as an input method attaches and detaches. Its input method also goes
away now and then as it answers a sync, as when its process restarts,
and the proxy must activate the field again. The seed picks the host's
`zcr_text_input_extension_v1` version, one for each way the proxy sends
a content type. Whether an enable reaches the proxy before or after the
previous activation has drained follows from the seed too: the
compositor holds its answer to every drain barrier's sync until it
answers anything else, and the guest sometimes waits with a second
roundtrip, whose sync releases it. When such a case typed into text
fields, stayed healthy and had no objects destroyed by the compositor on
its own, Exo must end, once the proxy has closed the connection, with
the field the guest last committed on the focused surface: the content
type as the proxy maps it, the surrounding text and support for it, and
the cursor rectangle, or no active text input if the field was disabled.
The expected content type comes from the proxy's own mapping, so this
check finds ordering and state bugs, not mapping bugs; unit tests pin
the mapping. The run reports how many fields it checked, how many
activations the guest's plan deferred, how many input methods
restarted, how many host activations had their seat end while active,
and how many deactivates named a destroyed seat.

The suite runs 5000 cases from seed 0; run more, from another seed,
after changing how the proxy reads guest requests or tracks objects:

```bash
SOMMELIER_FUZZ_CASES=200000 SOMMELIER_FUZZ_SEED=1000000 \
  cargo test -p sommelier --bins proxy::fuzz
```

A failure names its seed; rerun that seed alone with
`SOMMELIER_FUZZ_SEED=<seed> SOMMELIER_FUZZ_CASES=1`, and add `RUST_LOG=debug`
to see the proxy's log. The test prints it with the failure; a replay
that passes prints only its summary. The seed fixes the case's
input, not its timing (how the pieces arrive, which side the proxy serves
first), so replay a failing seed a few times before concluding it passes.

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
`virtio_wl` device. `virtwl_channel`'s
`live_try_recv_eagain_then_async_recv_wakes` drives one: it opens a
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
