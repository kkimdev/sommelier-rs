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
   press. Only a key that ended a composition the guest showed is
   recovered (INVARIANTS "An empty confirmation recovers only a key that
   ended a composition"), which leaves two cases:
   - **A dead key, then a held letter, with an engine that answers
     reports (E17).** On US-International, IMA's character composer
     shows the dead key as a preedit, which the letter's press ends
     (`InputMethodAsh::ExecuteCharacterComposer`), so the letter may be
     recovered. If the engine passes the letter's autorepeats, recovering
     them is right, as native Chrome repeats the letter; if it also
     answers each report with an empty confirmation, as English (US)
     does, those answers would repeat the letter as fast as the guest
     reports, until its release. Whether US-International's engine
     answers reports is unknown (host check below).
   - **A composition the guest never saw.** A late answer across a focus
     change (E5 (c)) can leave the engine composing what the guest never
     showed; the key that ends it is not recovered, so a held Space after
     it does not repeat. A repeat is lost, not text; the DST counts these
     (`repeats_of_no_ended_composition`).
3. **Host checks.** The IME's are in
   [IME.md 5.2](docs/IME.md#52-unknowns-and-manual-host-checks); each
   runs against an isolated proxy
   ([Manual verification](docs/INVARIANTS.md#manual-verification)).
   These are not yet confirmed on a host:
   - English (US) in gedit: a short tap of a letter types it once, and a
     held letter repeats at the host's rate, never faster; a long-press
     accent types the accent once.
   - US-International in gedit: type a dead key (`'` or `` ` ``), then
     hold `a`: it should type "á" (or "à") once, then repeat `a` at the
     host's rate and stop at the release. With `RUST_LOG=sommelier::keys=debug`,
     also type a few letters with no key held and check whether empty
     `confirm_preedit`s answer the reports (E17); if they do and `a`
     runs away, the input method answers reports for keys that end a
     composition too.
   - Data-control reading an image: text was confirmed on a host both
     ways (`wl-copy`, Orca's `orca-wayland-clipboard`, `wl-paste`), and
     that a ChromeOS window's focus empties what it reads.
   - A Flatpak app through `wp_security_context_v1`: Flatpak is not
     installed on the machine the proxy was checked on.
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
   - On VirtWL without `--gpu-accel`, a guest that binds `wl_shm` and
     syncs before the internal linux-dmabuf binding's formats arrive gets
     those formats after the sync's `done` (`guest_shm_format_available`,
     `handler/shm/formats.rs`); narrow and pre-existing.
   - Known limits stated with their rules:
     [a late owed buffer release](docs/INVARIANTS.md#surfaces-damage-and-shm)
     and [UTF-8 replacement growth](docs/INVARIANTS.md#wire-messages).
5. **Test debt.** The host display the DST and the fuzzer share
   (`sim::display`) does not answer the destroy of a
   `zcr_extended_keyboard_v1` with its `delete_id` in the DST
   (`Display::unanswered`), and its pointers send no `wl_pointer.frame`:
   either would shift every DST schedule, so the shrunk regressions in
   `sim/regressions.rs` would need deriving again. Neither the DST nor
   the fuzzer reaches a sync that waits for the globals
   (`Parked::Globals`): their contexts have no render node, so only the
   session tests of `handler::registry` and `proxy` cover it.
6. **Optional code debt.** With `--gpu-accel` and a render node, a sync
   after each `get_registry` waits one extra host round trip
   ([a sync's `done` follows the earlier registries' globals](docs/INVARIANTS.md#registry-globals)),
   even when the dma-buf capabilities are already ready; skipping the
   barrier then is an optional optimisation.
