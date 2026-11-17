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
cargo test --locked --workspace --all-targets -- --test-threads=1
git diff --check
```

Tests must run serially (`--test-threads=1`).

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

## Randomized tests

`proxy::fuzz` feeds a whole proxy client random, protocol-shaped guest
request streams built from the vendored XML, some of them damaged, over
real sockets. Each case must end without a panic or hang, close every
descriptor, and either answer the stream's closing `wl_display.sync` or
send `wl_display.error`. The fake compositor behind the proxy also checks
that every request it receives comes from a live host object, creates
only fresh IDs and names live objects of the right interface; it deletes
destroyed objects at once, so the proxy's host ID reuse is exercised.
The suite runs 5000 cases from seed 0; run more, from another seed,
after changing how the proxy reads guest requests:

```bash
SOMMELIER_FUZZ_CASES=200000 SOMMELIER_FUZZ_SEED=1000000 \
  cargo test -p sommelier --bins proxy::fuzz
```

A failure names its seed; rerun that seed alone with
`SOMMELIER_FUZZ_SEED=<seed> SOMMELIER_FUZZ_CASES=1`, and add `RUST_LOG=debug`
and `-- --nocapture` to see the proxy's log. The seed fixes the case's
input, not its timing (how the pieces arrive, which side the proxy serves
first), so replay a failing seed a few times before concluding it passes.

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
