# Development Handover

## Current status

- The active development line is the `virtwl` branch in
  [`kkimdev/sommelier-rs`](https://github.com/kkimdev/sommelier-rs).
- The fork is based on `google/sommelier-rs:virtwl` and carries ChromeOS
  VirtWL, shelf-ID, protocol-hardening, and Korean IME fixes.
- The current release and its assets are listed under
  [Current release](README.md#current-release), and the CI artifact and
  release policy under [CI and release workflow](README.md#ci-and-release-workflow).
- The `chromeos-dev-setup` flake in `monorepo-public` installs the package
  through this repository's flake overlay. It picks up a release once
  `nix/package-bin.nix` is updated here and the `chromeos-dev-setup`
  `flake.lock` moves to that `virtwl` revision.

## Architecture

- The README's [repository layout](README.md#repository-layout) lists the
  top-level directories, [`docs/DESIGN.md`](docs/DESIGN.md) explains the
  proxy's structure, and [`docs/INVARIANTS.md`](docs/INVARIANTS.md) holds
  the rules every change must keep.
- The opt-in window placement feature is described, with its code map, in
  [`sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md`](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).
- The IME core (`sommelier/src/ime`) bridges a guest's text-input-v3 to
  Exo's text-input-v1. Every IME input goes through `ImeCore::step`, and
  `handler/text_input.rs` is its adapter. The Exo mirror is the one
  model of Exo's text; a host without the text-input extension at v11
  gives text inputs no input method.
  [`docs/IME.md`](docs/IME.md) holds the behaviour table, the facts about
  Exo and the clients, and the deterministic simulation test (DST);
  [`docs/KEYBOARD.md`](docs/KEYBOARD.md) holds the key states.
- The keyboard core (`sommelier/src/keys`) is the IME core's twin: every
  keyboard input goes through `KeyboardCore::step`, which owns the host
  keyboards' records, their keys' generations, the Backspace lease, the
  held-key recovery and the host accelerators, and `handler/keyboard.rs`
  is its adapter. The two cores talk only through typed inputs and
  effects ([`docs/KEYBOARD.md`](docs/KEYBOARD.md#the-keyboard-core)).

## Known issues and debt

- Historical: the original `virtwl-v0.2.1` tag was consumed by a failed
  immutable GitHub Release attempt and cannot be recreated;
  `virtwl-v0.2.1-r1` replaced it.
- [Scope and limitations](README.md#scope-and-limitations) lists what
  `--gpu-accel` and the fork do not cover.
- Upstream review and eventual rebase/cherry-pick coordination remain
  separate from this fork's `virtwl` branch.
- Unconfirmed on a host: a GTK3 client may draw each frame into a new SHM
  buffer, since Exo releases a buffer only once it is detached, and each
  new buffer costs a VirtWL allocation and a full copy. With
  `RUST_LOG=debug` the proxy logs, at most once a second per surface,
  how many new SHM buffers the surface showed (`showed N new SHM
  buffers`). If it does, recycle released host storage by layout and
  move its `StalePixels` relation with it, which rewrites the INVARIANTS
  rule that a new buffer copies in full.
- Unconfirmed on a host: coalescing the output of consecutive fairness
  batches into full submissions (`Client::flush`) pays off as much as
  `VIRTWL_IOCTL_SEND` waits for crosvm, which the code review recalled
  but nobody has measured. `strace -T -e ioctl` on a live proxy while a
  client scrolls shows each send's time.
- A late owed `wl_buffer.release` can mark a buffer released while the
  host holds it, which skips its damage-only copies until the next attach;
  see [Surfaces, damage and SHM](docs/INVARIANTS.md#surfaces-damage-and-shm).
- ChromiumOS parity reviews left these differences unconfirmed, with no
  reproducer: commits on a surface with no role or window, cleanup of a
  keyboard leave for a pending surface, and two keyboards on one seat (one
  chord can issue a redundant placement batch).
- A text input whose guest `wl_seat` goes away while it is active ends its
  host generation without a `deactivate`, since none can name a live
  seat (R1 of [IME.md](docs/IME.md#23-cross-concern-inputs-and-reconcile-outcomes)).
  It does so at the end of the IME core's next step, not when the seat
  ends. Exo keeps its input method attached until its own focus moves,
  and if the guest later binds a new seat under the seat ID on that
  surface, Exo's `activate` does nothing and the stale attachment
  stays. Any live host seat would do for the
  `deactivate`; this was never seen on a host.
- A host that reuses a removed global's numeric name can make a registry
  created after the removal record the replacement under the old
  generation, if it sees the replacement before the registry that saw the
  removal; a bind through it then ends the session. The removal record
  (`removed_global_generations`), which waits for every registry that
  lists the removed generation, depends on this being fixed: until then
  it also waits for such a registry, and destroying that registry while
  it holds the record retires the replacement and resets its internal
  binding. libwayland-server never reuses a name, so Exo cannot reach
  this.
- Clipboard pumps rely on a VirtWL pipe supporting `poll` and returning
  `EAGAIN` under `O_NONBLOCK`. The ChromiumOS `virtio_wl.c` does both, but
  no transfer has been checked on a device since the pumps stopped
  blocking.
- A commit of a dma-buf the guest GPU is still writing is parked and
  dispatched again instead of waiting on the event loop
  ([native buffers are fenced](docs/INVARIANTS.md#dma-buf)). No device
  has exercised a pending fence since: neither the sync_file wait nor
  the `VIRTGPU_WAIT_NOWAIT` polling of the virtio-gpu fallback.
- A send awaits room in the peer's socket without a bound. A guest that
  stops reading its socket while its own writes block can therefore
  stall its connection for good, with the proxy waiting to send to it
  and the guest waiting to send to the proxy. This predates fence
  parking, which can lead to it (a parked stream is not read), and the
  fence deadline does not bound it, because the loop is waiting in the
  send. Only that client stalls.
- Replacing invalid UTF-8 can make a forwarded message too long, which
  ends the session ([Wire messages](docs/INVARIANTS.md#wire-messages)).
  No client is known to send one.
- An empty IME confirmation recovers a held key only once the host's next
  event shows it repeated
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)), and a
  text-input event before the next `peek_key` counts as that proof. So
  if Exo sent a new unhandled press's own text between its confirmation
  and its peek, the held key would still repeat once. IMA sends text
  there only when it had buffered a composition as well as a commit,
  which makes the confirmation not empty; no DST case does it. A key
  held across an input-method restart stops repeating until its next
  press, and while it is held no empty confirmation recovers another
  held key.
- No host trace is recorded yet ([traces/](traces/README.md)). The first
  should hold a Space held after 가 (E13, E15 of
  [IME.md](docs/IME.md#24-exo-facts-the-models-implement)), and replace
  the hand-written capture that `held_space_repeats_after_ime_commit_until_physical_release`
  holds as those facts' source; the checks of IME.md 5.2 follow. A
  replay cannot stand in for a render node, placement, accelerators or
  `--gpu-accel`, and gives a keymap the default US one.
- Exo's deletes and preedit regions that the Exo mirror cannot measure
  are dropped ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)). A drop
  never deletes or loses text, but the input method goes on as if it
  applied, so its next commit doubles text once (`word word~`, 한국韓國)
  until the guest's next report resets Exo. The mirror's gaps:
  - A silent edit the proxy does not see breaks the guest model: GTK3
    reports a click's new cursor only with its next edit, so a region
    right after the click is measured at Exo's old cursor. The DST's
    tier 1 holds the proxy to Exo's real text, not to the guest's, so it
    does not see it; tier 2 sees it only in free cases, which it does not
    judge.
  - A forwarded key or press suspends measuring until the guest states
    its text: a report it made after it, or a commit with no surrounding
    text, which the proxy reads as "unchanged" (Chromium reports its text
    only when it changed, C1). A client that commits without stating its
    changed text would break that, as GTK3 might with a commit of only a
    cursor rectangle after a click; check what GTK3 commits then.
  - A region that does not cover the composition Exo holds is dropped:
    the guest keeps showing the old preedit while Exo composes the new
    range and keeps the rest of the old one as text. The exact answer
    commits that rest after the region's delete.
  - Chromium applies a transaction's delete before its preedit clear and
    drops a delete while it shows a composition (C3 of
    [IME.md](docs/IME.md#25-client-facts-the-models-implement)), so a
    region over a shown preedit that deletes bytes beyond it leaves them
    in Chromium's text as well as in the new preedit, and the guest is
    out of step: the next
    region is dropped until the guest reports (#85; test:
    `a_region_over_a_region_drops`). Ending the old preedit with a `done`
    of its own first would serve Chromium, but the report Chromium
    answers it with lacks the preedit, so Exo would drop its composition
    (E4).
  - A withheld report hides a guest edit that happens to equal Exo's
    state without the composition the guest shows, as from a client that
    drops its preedit without a word (C4 of IME.md): Exo keeps
    composing until the composition ends and the next report goes out.
    Exo's own clients share that hazard whenever a report matches.
  - The mirror is only as right as its reading of Chromium
    (`6975bb48a8cd`). A host check, with `RUST_LOG=sommelier::ime=debug`,
    in gedit, a GTK4 app, and Chrome or VS Code with
    `--enable-wayland-ime --wayland-text-input-version=3`: type Korean
    with Shift; hold Backspace; reconvert with the hanja key inside a
    word and after it, then press Esc and Backspace; click while
    composing; move the focus while composing; accept a suggestion if
    one is offered. No "Exo mirror ... diverged" warning should show,
    and every "Dropping Exo's" debug line should have a cause the case
    explains.
- The pointer press or touch-down that ends a composition
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)) has not been
  checked on a host. In VS Code or Chrome started with
  `--enable-wayland-ime --wayland-text-input-version=3`, and in a GTK3 app
  such as gedit, type a Korean syllable and click elsewhere in the field
  while it is composing. The syllable must stay where it was, committed
  once. The next jamo must start a new syllable at the click point,
  without joining the old one, and the old syllable must not show up
  again there.
  - Press Space and click at the same instant, many times. The syllable
    should appear once.
  - The reset's fence covers only events already on the wire. A commit
    the input method makes after Exo handled the `reset`, for a key it
    was still processing, still doubles the syllable. Note how often
    that happens.
  - A key the input method takes within the click's round trip is lost
    by design (see the rule).
  - A click on a subsurface, which the keyboard focus never names, does
    not end the composition. mutter also matches the exact surface; KWin
    matches the window.
  - On a touchscreen, tap elsewhere in the field while a syllable is
    composing. The syllable must stay where it was, committed once, and
    the next jamo must start a new syllable at the tap point.
- The `done` that answers each guest text-input-v3 commit has not been
  checked on a host. In Chrome or VS Code started with
  `--enable-wayland-ime --wayland-text-input-version=3`, type Korean and
  check that the candidate window follows the caret after a click or an
  arrow key, before the next keystroke, and that a composition survives
  the caret-rectangle updates it causes, with no syllable dropped or
  doubled.
- An input method that replaces selected text through a delete loses
  the delete, since deletes are dropped while the guest has a selection
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)).
- Whether a composition should survive a blur, as native Chrome keeps
  it, is open (U5, U13 and U14 under
  [Unknowns and manual host checks](docs/IME.md#52-unknowns-and-manual-host-checks)).
  The proxy commits it, as mutter and ibus do.
- A guest's own reset (`set_text_change_cause`, G6) never resets Exo;
  only a press does (C6, Q3 of
  [IME.md](docs/IME.md#53-open-design-questions)).
- A focus-loss commit too long for one message loses the preedit (Exo's
  confirmation is dropped while the link drains).
- Key generations: an `Announced` peek accepts an older release, and a
  stale held Backspace can still be leased (`KeyGenerationRegistry::lease`
  ignores `stale`).
- `on_peek_key`'s owed release of a stale press can reach the guest
  before a waiting recovery's pair.
- The keyboard focus registry (`state::KeyboardFocusRegistry`) is still
  `Context`'s: the keyboard core reads it through `keys::Env` and hears of
  each change as `KeyInput::Focus`. Moving it into the core would make the
  core the one owner of every keyboard fact.
- The bindings watcher removes the watches of an arming attempt that
  started over (because a directory appeared while it armed), but no test
  reaches that race.
- Each empty confirmation while a taken key is held sends the host a
  `wl_display.sync` (about 30 a second).
- The mirror assumes Exo reads a report and its glued sync in one read;
  an IME event between them would leave it exact but wrong (never seen).
- `Keymap::is_modifier` reads only layout 0, level 0.
- Chromium's IMA keeps composing after a composition empties (ㄱ then
  Backspace), so an earlier key's text can be lost; host behaviour, not
  mitigated.
- The DST models only Chromium, GTK3 and GTK4 (no selection, one text
  input).
- The `zxdg_output_manager_v1` passthrough has not been checked on a host.
  That Exo advertises the global is inferred from ChromiumOS Sommelier,
  whose direct-scale mode binds it and assumes it is present.
- Placement's unverified cases are listed under
  [Output and work area](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#output-and-work-area);
  popups on a toplevel repaired through a late `zaura_shell` bind were also
  never checked on a host.
- Placement moves windows with `zaura_surface.set_parent(self, dx, dy)`,
  a Chromium behaviour that is not an API. The host check passed on
  2026-11-20: gedit, all nine named actions placed correctly (23
  `set_parent` moves, each followed by the expected configure, no
  warnings). The
  [manual test plan](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#manual-test-plan)
  lists what to check, and
  [How a window is moved](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#how-a-window-is-moved)
  its known limits. The ARC and remote-shell-v2 backends it replaced are
  at tag `placement-arc-remote-shell-final`.
- Placement is decided per connection as it connects, while its bindings
  follow the file live: an app that connected before the file existed
  is placed only once it reconnects, and with the file present every XDG
  toplevel gets an Aura toplevel whose configures the proxy translates.
  Attaching the Aura toplevel lazily, at a window's first placement
  chord, would remove both, but needs a host check first: whether Exo
  reports a toplevel's origin when `set_supports_screen_coordinates`
  arrives after its first commit (the "No origin" limit under
  [How a window is moved](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#how-a-window-is-moved)).
- The bindings file is reloaded through inotify
  ([Reloading](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#reloading)),
  which has not been checked against a Home Manager activation on a host
  yet; the unit tests swap links the way an activation does.

- The host display the DST and the fuzzer share (`sim::display`) does
  not answer the destroy of a `zcr_extended_keyboard_v1` with its
  `delete_id` in the DST (`Display::unanswered`), as libwayland-server
  would: answering it shifts every DST schedule, and the shrunk
  regressions in `sim/regressions.rs` replay theirs, so they need
  deriving again with it. Its pointers send no `wl_pointer.frame`, which
  Exo sends a v5 pointer, for the same reason.
- Tests that drive `Client::handle_msgs` directly, rather than through
  `proxy::harness::Session`, bypass the peers' protocol checks
  (`test_support::wire_peer`); they move to `Session` with the handler
  tests.
- Wheel steps for touchpad scrolling in Wine apps
  ([pointer scrolling](docs/INVARIANTS.md#pointer-scrolling)) have not
  been checked with KakaoTalk on a host yet; a scroll that feels too fast
  or slow would tune `WHEEL_STEP` in `handler/pointer.rs`.

## Next steps

1. Continue Crostini manual testing through the `wayland-2` service.
2. Run the host checks above and those under
   [Unknowns and manual host checks](docs/IME.md#52-unknowns-and-manual-host-checks).
3. Keep protocol and IME regressions covered by the test suite.
