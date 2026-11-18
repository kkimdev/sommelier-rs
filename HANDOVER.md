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
- The IME core is being redesigned. [`docs/IME_REDESIGN.md`](docs/IME_REDESIGN.md)
  holds the plan: a deterministic simulation test first, then a pure IME
  core that replaces `TextInputState` in staged, golden-trace-gated PRs.

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
- Exo measures a `delete_surrounding_text` against its predicted text,
  which includes input-method edits the guest has not yet reported. The
  proxy drops deletes until the guest commits its surrounding text after
  such an edit, but a guest commit that crosses an edit in flight still
  counts as caught up, so the next delete can land wrong
  ([Keyboard and IME](docs/INVARIANTS.md#keyboard-and-ime)). A Chromium
  or Electron v3 guest crosses one after a delete then a commit, as when
  a suggestion is accepted: it answers the delete's `done` with its text,
  which clears the gate, before it applies the commit. The `done` that
  answers that report lets it report the text after the commit at once
  (`a_client_that_waits_for_done_reports_the_text_an_edit_leaves`), so
  only a delete within that round trip can land wrong. Mirroring Exo's
  `SurroundingTextTracker` would close this; no host has shown it.
- A preedit region over a preedit an earlier region made deletes only the
  bytes the guest still has, but one case stays approximate. A region
  that does not cover the shown one, an empty one included, is dropped:
  the guest keeps showing the old preedit while Exo composes the new
  range, as the right answer, giving the old text back before the new
  delete, needs a commit before a delete, which one v3 transaction
  cannot order.
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
