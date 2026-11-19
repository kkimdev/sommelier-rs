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
- The IME core has been redesigned. [`docs/IME_REDESIGN.md`](docs/IME_REDESIGN.md)
  holds the plan: a deterministic simulation test first, then a pure IME
  core that replaced the old text-input handlers in staged,
  golden-trace-gated PRs. The switch (C3a to C3c) is done: every IME
  input goes through `ImeCore::step`, and `handler/text_input.rs` is its
  adapter. The first behaviour change, C5, is done: a field the focus
  leaves commits the preedit the guest shows before the keyboard's
  `leave`, and so does Exo's own detach with a preedit its confirmation
  left, so the DST knows no failure of the user's intent. C6 (an
  optional guest-initiated host `reset`) and C7 (test slimming) remain.
  Whether a composition should survive a blur, as native Chrome keeps
  it, is open (U5, U13, U14 of the design's 6.2); C5 commits it, as
  mutter and ibus do. The Exo mirror is done: from v11 of the text-input
  extension the core keeps a replica of Exo's text tracker per text
  input, the one model of Exo's text, and measures Exo's deletes and
  preedit regions in it, or drops them; below v11 it drops them all
  (INVARIANTS "Exo's deletes and regions are measured in the Exo mirror,
  or dropped"; `specs/single-exo-model.md`). A report that would only
  drop Exo's composition is withheld (same rule). C5 commits at Exo's
  detach a preedit Exo did not hold (H2), which retired
  `RestartLosesUntrackedComposition`.

## Known issues and debt

- Historical: the original `virtwl-v0.2.1` tag was consumed by a failed
  immutable GitHub Release attempt and cannot be recreated;
  `virtwl-v0.2.1-r1` replaced it.
- [Scope and limitations](README.md#scope-and-limitations) lists what
  `--gpu-accel` and the fork do not cover.
- Upstream review and eventual rebase/cherry-pick coordination remain
  separate from this fork's `virtwl` branch.
- A late owed `wl_buffer.release` can mark a buffer released while the
  host holds it, which skips its damage-only copies until the next attach;
  see [Surfaces, damage and SHM](docs/INVARIANTS.md#surfaces-damage-and-shm).
- ChromiumOS parity reviews left these differences unconfirmed, with no
  reproducer: commits on a surface with no role or window, cleanup of a
  keyboard leave for a pending surface, and two keyboards on one seat (the
  IME Backspace fallback picks a seat-level keyboard while keysyms prefer
  the focused one, and one chord can issue a redundant placement batch).
- A text input whose guest `wl_seat` goes away while it is active ends its
  host generation without a `deactivate`, since none can name a live
  seat. It does so at its next reconcile (a commit, a focus change, a
  completed drain barrier, a seat bind or its destroy), not when the seat
  ends. Exo keeps its input method attached
  until its own focus moves, and if the guest later binds a new seat
  under the seat ID on that surface, Exo's `activate` does nothing and
  the stale attachment stays. Any live host seat would do for the
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
- Replacing invalid UTF-8 in a forwarded string can make it up to three
  times longer, so a message near the 4096-byte limit with many invalid
  bytes still ends the session as too large
  ([Wire messages](docs/INVARIANTS.md#wire-messages)). No client is known
  to send one.
- An empty IME confirmation while a key the input method took is held
  recovers that key only once the host's next event shows it repeated,
  one round trip later at most
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)). A text-input
  event before the next `peek_key` counts as such proof. So if Exo sent a
  new unhandled press's own text between its confirmation and its peek,
  the held key would still repeat once. IMA sends text there only when it
  had buffered a composition as well as a commit, which it shows before
  it confirms, so the confirmation is then not empty; no DST case does it.
  Exo's own detach of the input method ends the recovery of the keys held
  then, whose releases may have died with it, so a key held across an
  input-method restart stops repeating until its next press.
- Exo's deletes and preedit regions are measured in the Exo mirror only
  while the text input has one (extension v11 or later), the guest is in
  step with Exo, the mirror is exact, and no report crosses the event;
  otherwise they are dropped
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)). A drop
  never deletes or loses text, but the input method goes on as if it
  applied, so its next commit doubles text once (`word word~`, 한국韓國)
  until the guest's next report resets Exo to the guest's text. Before
  the drops replaced the old fallback, 20,000 DST cases from seed 0 fell
  back on 19 events of the free cases and none of the settled ones; the
  DST now fails a settled case with a drop the mirror could measure. The
  mirror is unsure only after a divergence, an event that arrives while
  a split report is on its way, the 4,096-state or 256 KiB caps, or an
  ambiguous report; a press's `reset` then a report heal those, and a
  vague selection heals at the next event that sets it. Below v11 every
  delete and region is dropped, which warns once per text input. Its
  gaps:
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
    changed text would break that.
  - A region that does not cover the composition Exo holds is dropped:
    the guest keeps showing the old preedit while Exo composes the new
    range and keeps the rest of the old one as text. The exact answer
    commits that rest after the region's delete.
  - Chromium applies a transaction's delete before its preedit clear and
    drops a delete while it shows a composition (C3 of
    [IME_REDESIGN](docs/IME_REDESIGN.md)), so a region over a shown
    preedit that deletes bytes beyond it leaves them in Chromium's text
    as well as in the new preedit, and the guest is out of step: the next
    region is dropped until the guest reports (#85,
    `a_region_over_a_region_drops`). Ending the old preedit with a `done`
    of its own first would serve Chromium, but the report Chromium
    answers it with lacks the preedit, so Exo would drop its composition
    (E4).
  - A withheld report hides a guest edit that happens to equal Exo's
    state without the composition the guest shows, as from a client that
    drops its preedit without a word (C4 of the design): Exo keeps
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
- Deletes are dropped while the guest has a selection, because GTK and Qt
  count v3 delete lengths from the cursor and Chromium's v3 client from
  the selection's ends; no lengths serve both. An input method that
  replaces selected text through a delete loses the delete.
- The `zxdg_output_manager_v1` passthrough has not been checked on a host.
  That Exo advertises the global is inferred from ChromiumOS Sommelier,
  whose direct-scale mode binds it and assumes it is present.
- Placement's unverified cases are listed under
  [Output and work area](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#output-and-work-area);
  popups on a toplevel repaired through a late `zaura_shell` bind were also
  never checked on a host.
- Neither placement backend has been checked on a host since they were
  separated, and the remote-shell-v2 backend never has: stock Crostini
  does not advertise `zcr_remote_shell_v2`. The
  [manual test plan](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#manual-test-plan)
  lists what to check for each.

## Next steps

1. Continue Crostini manual testing through the `wayland-2` service.
2. Keep protocol and IME regressions covered by the test suite.
