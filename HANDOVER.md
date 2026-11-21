# Handover

The state of the `virtwl` branch, and what is left to do.
[CONTRIBUTING.md](CONTRIBUTING.md) says how to work on it,
[docs/DESIGN.md](docs/DESIGN.md) how it is built, and the README's
[current release](README.md#current-release) what is published. The
`chromeos-dev-setup` flake in `monorepo-public` installs the package
through this repository's flake overlay, so it picks up a release once
`nix/package-bin.nix` is updated here and its `flake.lock` moves to that
`virtwl` revision.

## Open issues and debt

Ranked by what a user would lose: text or keys first, then what no host
has confirmed, then latent faults never seen, then test and code debt.

1. **IME gaps that can double or lose text**:
   [IME.md 5.4](docs/IME.md#54-known-gaps).
2. **A held key can repeat once more, or stop repeating.** An empty
   confirmation recovers a held key once the host's next event shows it
   repeated (INVARIANTS "An empty confirmation recovers a held key only
   once the host shows it repeated"), and a text-input event before the
   next `peek_key` counts as that. So if Exo sent a new unhandled press's
   own text between its confirmation and its peek, the held key would
   repeat once; IMA sends text there only with a buffered composition,
   which makes the confirmation not empty, and no DST case does it. A key
   held across an input-method restart stops repeating until its next
   press.
3. **Host checks.** The IME's are in
   [IME.md 5.2](docs/IME.md#52-unknowns-and-manual-host-checks); each
   runs against an isolated proxy
   ([Manual verification](docs/INVARIANTS.md#manual-verification)).
   These are not yet confirmed on a host:
   - Data-control (`--data-control`): a copy button in Orca, whose
     `orca-wayland-clipboard` helper sets the selection through
     `ext_data_control_v1`, reaches the ChromeOS clipboard while Orca
     has the focus.
   - Clipboard transfers since the pumps stopped blocking: they rely on a
     VirtWL pipe supporting `poll` and returning `EAGAIN` under
     `O_NONBLOCK`, which ChromiumOS `virtio_wl.c` does.
   - A pending dma-buf fence: neither the sync_file wait nor the
     `VIRTGPU_WAIT_NOWAIT` polling of the virtio-gpu fallback has run on a
     device since commits park.
   - Whether a GTK3 client draws each frame into a new SHM buffer, each
     costing a VirtWL allocation and a full copy: with `RUST_LOG=debug`
     the proxy logs `showed N new SHM buffers` at most once a second per
     surface. If it does, recycle released host storage by layout and
     move its `StalePixels` with it.
   - What coalescing batches into full submissions saves:
     `strace -T -e ioctl` on a live proxy while a client scrolls shows
     each send's time.
   - The `zxdg_output_manager_v1` passthrough, which assumes Exo
     advertises the global, as ChromiumOS Sommelier's direct-scale mode
     does.
   - Wheel steps for touchpad scrolling in KakaoTalk; a wrong speed tunes
     `WHEEL_STEP` in `handler/pointer.rs`.
   - Placement: the cases under
     [Output and work area](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#output-and-work-area),
     popups on a toplevel repaired through a late `zaura_shell` bind, and
     a reload of the bindings file by a Home Manager activation. Placement
     moves windows with `zaura_surface.set_parent(self, dx, dy)`, a
     Chromium behaviour, not an API, so recheck the
     [manual test plan](sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md#manual-test-plan)
     after a ChromeOS update.
4. **Latent faults, never seen:**
   - The proxy forwards a `wl_keyboard.key` to the guest after the
     focused surface was destroyed and before the host's `leave`, so a
     key can reach a surface the guest already ended.
   - A host `wl_keyboard.key` whose key's earlier `peek_key` had another
     serial is not forwarded.
   - Key generations: an `Announced` peek accepts an older release, a
     stale held Backspace can still be leased
     (`KeyGenerationRegistry::lease` ignores `stale`), and `on_peek_key`'s
     owed release of a stale press can reach the guest before a waiting
     recovery's pair.
   - A send awaits room in the peer's socket without a bound, so a guest
     that stops reading while its own writes block stalls its connection
     for good (only that client); the fence deadline does not bound it.
   - ChromiumOS parity differences with no reproducer: commits on a
     surface with no role or window, cleanup of a keyboard leave for a
     pending surface, and two keyboards on one seat (one chord can issue
     a redundant placement batch).
   - Placement is decided per connection as it connects: an app that
     connected before the bindings file existed is placed only once it
     reconnects, and with the file present every XDG toplevel gets an
     Aura toplevel. Attaching it at a window's first placement chord
     would remove both, once a host check shows Exo reports a toplevel's
     origin when `set_supports_screen_coordinates` arrives after its
     first commit.
   - Each empty confirmation while a taken key is held sends the host a
     `wl_display.sync`, about 30 a second.
   - `Keymap::is_modifier` reads only layout 0, level 0.
   - The bindings watcher removes the watches of an arming attempt that
     started over, but no test reaches that race.
   - Known limits stated with their rules:
     [a late owed buffer release](docs/INVARIANTS.md#surfaces-damage-and-shm)
     and [UTF-8 replacement growth](docs/INVARIANTS.md#wire-messages).
5. **Test debt.** The host display the DST and the fuzzer share
   (`sim::display`) does not answer the destroy of a
   `zcr_extended_keyboard_v1` with its `delete_id` in the DST
   (`Display::unanswered`), and its pointers send no `wl_pointer.frame`:
   either would shift every DST schedule, so the shrunk regressions in
   `sim/regressions.rs` would need deriving again.
6. **Optional code debt.** Move the SHM copy and dma-buf fence code out
   of `handler/compositor.rs` into the modules that own them.
