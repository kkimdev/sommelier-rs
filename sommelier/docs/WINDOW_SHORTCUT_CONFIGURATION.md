# Window placement shortcut configuration

Compositor-owned window placement shortcuts move the focused XDG toplevel
to a configured region of its output's work area when a configured chord is
pressed. Exo grants Crostini no bounds request, so the proxy moves the
window with `zaura_surface.set_parent` and sizes it with a configure (see
[How a window is moved](#how-a-window-is-moved)). This document is the
single owner of the contract; the README and the code link here instead of
restating it.

## Settings

| Setting | Meaning |
| --- | --- |
| `$XDG_CONFIG_HOME/sommelier-rs/window-placement.toml` | The bindings file, a versioned TOML document (see [File format](#file-format)). Without `XDG_CONFIG_HOME` it is `$HOME/.config/sommelier-rs/window-placement.toml`. |
| `SOMMELIER_ACCELERATORS` | Chords the host compositor keeps. A binding may not use one of them. |

Sommelier has no built-in bindings and creates no file: with no valid
file, placement is off. There is no other switch. The settings
(`sommelier/src/settings.rs`) resolve the path once, from
`XDG_CONFIG_HOME` (unless it is empty or relative, which the XDG base
directory specification says to ignore) or else an absolute `HOME`; with
neither, placement stays off. `SOMMELIER_ACCELERATORS` is read once,
at startup, too. The earlier `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`,
`--window-shortcuts-config`, `SOMMELIER_WINDOW_PLACEMENT_BACKEND` and
`SOMMELIER_WINDOW_BOUNDS_AS_ARC` are gone. `SIGHUP` no longer reloads the
file: like `SIGTERM`, it ends the proxy.

## The file's states

Sommelier is the Wayland proxy for every app in the container, so **a
configuration problem never stops the proxy**: exiting would close every
app and could restart-loop under a service manager. A problem is logged
once per change of the file instead.

| The file | Result |
| --- | --- |
| missing | Placement off, logged at `debug` only. |
| valid | Placement on, with its bindings; logged at `info`. A valid file with no bindings turns placement on and binds nothing. |
| unusable at startup (unreadable, invalid, or binding a `SOMMELIER_ACCELERATORS` chord) | Placement off; one warning. |
| replaced by a valid version | Its bindings replace the last ones at once, in apps that connected while placement was on (see below). |
| replaced by an unusable version | The last good bindings stay, or placement stays off; one warning per version, not per key. |
| removed | Its bindings are cleared and placement is off for new connections; logged at `info`. |

A malformed `SOMMELIER_ACCELERATORS` logs one warning and disables key
filtering, so every key goes to the guest; bindings are then checked
against no host chord.

Placement is decided per connection, as the app connects: an app gets an
Aura toplevel for each window (see
[How a window is moved](#how-a-window-is-moved)) only if the file was
valid then. So an app that connected while the file was missing or
unusable is not placed until it reconnects (the log says so when the file
appears), and one that connected while placement was on keeps its Aura
toplevels after the file is removed, with no chord bound.

## How a window is moved

Exo accepts `zaura_toplevel.set_window_bounds` only from ARC windows, and
the ARC application ID that makes a window one also makes Chrome send its
keys past the ChromeOS IME (`exo_app_type_resolver.cc` sets
`kSkipImeProcessing` with the ARC app type), so Korean composition would
stop working. Exo offers Crostini no remote shell either. It does move any
mapped toplevel for `zaura_surface.set_parent(parent, x, y)`, with no
permission check: `ShellSurface::OnSetParent` puts the window's top-left
corner, frame included, at the parent's client-area origin plus `(x, y)`.
With the surface as its own parent, the guard against parenting a surface
to itself returns before any transient link is made, and the window moves
by `(x, y)` relative to its own client area. The window keeps its Guest OS
application ID, so the IME, the shelf icon and every other Guest OS
behaviour are untouched. The guest sets the size itself, from a configure.

Exo reports a window's origin only to a toplevel that has sent
`zaura_toplevel.set_supports_screen_coordinates`, through
`zaura_toplevel.configure` (which then replaces `xdg_toplevel.configure`)
and `origin_change`. So while placement is on, every XDG toplevel gets a
host-only Aura toplevel in screen coordinates when it is created, and the
proxy translates its configures into the guest's `xdg_toplevel.configure`,
dropping the Aura-only states. A placement then takes three steps, each
ended by a host `wl_display.sync` barrier:

1. **Restore.** `xdg_toplevel.unset_fullscreen` and `unset_maximized`
   leave fullscreen and maximized state; Exo's restore also leaves snapped
   and floated state. Exo answers each with a configure, even when the
   state does not change, so by the barrier the proxy knows the window's
   client-area origin in its restored state.
2. **Move.** `zaura_surface.set_parent(self, dx, dy)`, where `(dx, dy)` is
   the target's corner minus that origin. Exo puts the window's corner
   there whether or not it draws a frame. It does not report such a move,
   so another `unset_fullscreen`, which changes nothing now, asks it for a
   configure with the new origin.
3. **Resize.** The move is judged by where the window ended. If the
   origin neither changed nor lies where a window whose corner is at the
   target's has it (at the target's corner, or straight below it by a
   caption), Exo ignored the move, and the proxy stops. A window with a
   frame Exo draws whose corner already was at the target's does not
   count: its offset runs from below the caption to the corner, so Exo,
   with nothing to move, reports the same origin. Exo also ignores a move
   while the user drags the window, so an ignored move logs a warning
   (once per process) only while no move has placed a window yet, and
   after that an info line. Otherwise the guest gets an
   `xdg_toplevel.configure` of the target's width and height, less the
   caption height the new origin shows (its offset below the target's
   corner), with the states and serial of the newest configure. The
   guest's commit of that size resizes the window, which keeps its origin.

A newer chord replaces a placement in flight; the older one's barriers
then do nothing.

Known limits:

- **The move relies on a Chromium quirk.** `set_parent` with the surface
  as its own parent is not a documented way to move a window. A host spike
  moved gedit with it on 2026-11-20 and Korean input kept working, and the
  host check of the whole placement passed the same day (gedit, all nine
  named actions); a
  Chromium change to `ShellSurface::OnSetParent` or its self-parent guard
  could stop it. Then the origin does not change at step 3, and the log
  says `Exo ignored zaura_surface.set_parent(self, ...)`.
- **Chrome logs a warning per move.** The self-parent guard logs one
  `LOG(WARNING)` in Chrome's log for every placement.
- **Frames.** A frame Exo draws puts its caption inside the target: the
  window's corner, caption included, lands on the target's, and the client
  area gets the height below it. Client-side decorated windows (most GTK
  apps) have no such frame. Their shadow margins lie outside the window
  geometry Exo uses, so they do not count.
- **Size is the client's.** The guest may round the size (to character
  cells, for example) or enforce its own minimum, so the window can end a
  little smaller or larger than the target. A window that ignores the
  configure keeps its size.
- **Serial reuse.** The configure at step 3 repeats the newest configure's
  serial, so the guest may acknowledge it twice. Exo accepts that, but if
  a newer configure of Exo's is in flight at that moment, the second
  acknowledgement also acknowledges it early.
- **Configures in between.** A configure Exo sends between step 3 and the
  guest's commit of the new size (on a focus change, say) carries the old
  size, and the guest may adopt it.
- **No origin.** A toplevel created while no `zaura_shell` new enough was
  bound gets its Aura toplevel when the shell is bound or at its first
  placement, after its first commit; Exo is then expected to report its
  origin all the same, which no host has confirmed.

## Chord handling

While placement is on, a chord that matches a binding is always consumed:
it never reaches the guest, even when the placement is skipped (for
example, with no focused toplevel, before the output's metadata is
complete, or for a rectangle the output cannot represent). A chord listed in
`SOMMELIER_ACCELERATORS` stays with the host. A chord that the host IME
consumes is neither placed nor delivered.

Every skip is logged with its reason. A skip that persists until the
configuration or the proxy changes is logged at `info`, which the default
log filter shows: a rectangle the output cannot represent, or no Aura
toplevel (`zaura_shell` missing or too old). The other skips (no focused
toplevel, no usable output yet, no Aura surface) are logged at `debug`;
set `RUST_LOG=debug` to see them. `PlacementSkip::log_level` in
`sommelier/src/handler/window_placement.rs` owns this split. A placement
that stops later, because Exo reported no origin or ignored the move, is
logged at `info` or `warn`.

## File format

A file only describes bindings in this format; it is not a recommended
set. This **example** binds nine `Alt` chords to the named actions:

```toml
# Example only: Sommelier has no default bindings.
version = 1

[[bindings]]
chord = "<Alt>q"
action = "top-left"

[[bindings]]
chord = "<Alt>w"
action = "top"

[[bindings]]
chord = "<Alt>e"
action = "top-right"

[[bindings]]
chord = "<Alt>a"
action = "left"

[[bindings]]
chord = "<Alt>s"
action = "fullscreen"

[[bindings]]
chord = "<Alt>d"
action = "right"

[[bindings]]
chord = "<Alt>z"
action = "bottom-left"

[[bindings]]
chord = "<Alt>x"
action = "bottom"

[[bindings]]
chord = "<Alt>c"
action = "bottom-right"
```

A binding can name a rectangle instead of an action:

```toml
# A rectangle, as fractions of the output work area: [x, y, width, height].
[[bindings]]
chord = "<Alt>v"
rect = [0.1666666666666667, 0.0, 0.6666666666666666, 1.0]
```

Choosing chords:

- **A bound chord never reaches the focused app** (see
  [Chord handling](#chord-handling)). With the example above, `Alt+D` no
  longer reaches a terminal (readline's delete-word) and `Alt+X` no
  longer reaches Emacs (`M-x`).
- **ChromeOS keeps its own chords.** A chord the ChromeOS shell handles
  (an accelerator such as `Search+L` or `Alt+Tab`) never reaches
  Sommelier, so binding it does nothing.

- `version` is required and must be `1`. A file with a different version is
  rejected before its other fields are checked.
- `bindings` is an array of tables. It may be empty or omitted; an empty list
  binds no chord.
- Each binding has a `chord` and **exactly one** of `action` or `rect`.
  - `chord` uses the `SOMMELIER_ACCELERATORS` spelling: modifier tags
    (`<Control>`/`<Ctrl>`, `<Alt>`/`<Meta>`, `<Shift>`,
    `<Super>`/`<Win>`/`<Search>`, case-insensitive) followed by an XKB
    keysym name. Matching is exact on modifiers and case-insensitive on the
    keysym. This is the one list of modifier tags; `parse_accelerator` in
    `sommelier/src/accelerator.rs` implements it.
  - `action` is one of the [named actions](#named-actions)
    (case-insensitive).
  - `rect` is `[x, y, width, height]`. Integers such as `[0, 0, 1, 1]` are
    accepted.
- Unknown keys anywhere are rejected, so a misspelt field cannot be silently
  ignored.

### Named actions

`NormalizedRect::preset` in `sommelier/src/window_layout.rs` is the one
parser of these names. Each stands for a fixed rectangle, which a binding
resolves to as the file is loaded; the `PRESETS` table in the same file
defines the values, and a unit test keeps this table and the code in sync.

| Action | Rectangle |
| --- | --- |
| `top-left` | `[0.0, 0.0, 0.5, 0.5]` |
| `top` | `[0.0, 0.0, 1.0, 0.5]` |
| `top-right` | `[0.5, 0.0, 0.5, 0.5]` |
| `left` | `[0.0, 0.0, 0.5, 1.0]` |
| `fullscreen` | `[0.0, 0.0, 1.0, 1.0]` |
| `right` | `[0.5, 0.0, 0.5, 1.0]` |
| `bottom-left` | `[0.0, 0.5, 0.5, 0.5]` |
| `bottom` | `[0.0, 0.5, 1.0, 0.5]` |
| `bottom-right` | `[0.5, 0.5, 0.5, 0.5]` |

`fullscreen` fills the work area; it is ordinary placement, not native
fullscreen.

## Validation

One validator, `validate_bindings`, checks every binding. Nothing is
applied partially: the whole document is validated before anything is
used. It is rejected for:

| Check | Rule |
| --- | --- |
| File | Must be a regular file (checked on the opened descriptor, so a FIFO or device cannot block the proxy); at most 64 KiB; UTF-8. Symbolic links are followed. |
| TOML | Must parse; no unknown keys; `version = 1`. |
| Size | At most 256 bindings. |
| Chord | Must parse; must not duplicate an earlier binding's chord (compared after parsing, so `<Alt>q` and `<Meta>Q` are duplicates); must not be listed in `SOMMELIER_ACCELERATORS`. |
| Target | Exactly one of `action` / `rect`; `action` must be a known name. |
| Rect | All four values finite (no `nan`/`inf`); `0 <= x < 1`, `0 <= y < 1`; `0 < width <= 1`, `0 < height <= 1`; `x + width <= 1`, `y + height <= 1`. |

The edge tolerance is `1e-9`: an edge within it of `0` or `1` *is* that
border and snaps onto it, so `x + width` and `y + height` may exceed `1` by
at most the tolerance. This absorbs decimal-to-binary error (for example
`0.1 + 0.9000000000000001`).

Error messages name the offending binding as `bindings[N]`, counted from
zero.

## Precision

Write rectangle values with 16 significant digits, which is what most
languages print for a double (`1/3` prints as `0.3333333333333333`). Then
every edge lands on the pixel its exact fraction names, for every work-area
extent up to 1,000,000 pixels. Exact binary fractions such as `0.5` or
`0.25` need no extra digits. A shorter decimal that was rounded down can land
one pixel short: `0.3333333 * 1920 = 639.99994` floors to `639`, not `640`.
Both statements have unit tests in `sommelier/src/window_layout.rs`.

## Output and work area

A placement uses the output that the focused surface entered last, so a
window never jumps to another monitor; only a surface currently on no output
(before its first enter, or after its output went away) uses the first
usable output. `OutputRegistry::for_surface` in
`sommelier/src/state/outputs.rs` owns the exact rule, including when an
output counts as usable. The proxy learns an output only from the guest's
own `wl_output` bind, so a client that never binds one cannot be placed:
each chord is skipped as having no usable output.

The work area is the output's logical screen area minus Aura's insets (the
shelf and other reserved margins), in screen DIPs, the coordinates of
`zaura_toplevel.configure`. `OutputState::work_area` in the same
file owns how it is measured from the output's metadata, including why the
`wl_output.geometry` panel transform is used rather than
`zaura_output.logical_transform`. Placement waits until Aura has sent both
its current output scale and its device scale factor; the rounded
`wl_output.scale` cannot describe fractional display scaling. It also needs
a `zaura_shell` new enough to send `zaura_output.insets`, without which the
work area would include the shelf. With an older shell the proxy warns
once and creates no Aura toplevel, so every chord is skipped
(`PLACEMENT_SHELL_SINCE` in `sommelier/src/handler/window_placement.rs`).

Known limits:

- Aura reports both scale factors in thousandths, so unusual scale settings
  may still differ slightly from the host's exact logical dimensions.
- Exo's `wl_output.mode` is the panel's `bounds_in_native` size, which can
  include overscan, so on an overscanned display the work area may be
  slightly too large.
- Rotated displays and multi-monitor output selection follow Exo's source and
  are covered by unit tests, but have not been verified on a host.

## From rectangle to window bounds

At key time the focused surface's [work area](#output-and-work-area)
`(X, Y, W, H)` becomes the frame of the rectangle. Each rectangle edge
fraction `f` becomes a pixel offset:

```text
offset(f, W) = clamp(floor(f * W + bias), 0, W)

left   = offset(x, W)          right  = offset(x + width,  W)
top    = offset(y, H)          bottom = offset(y + height, H)
bounds = (X + left, Y + top, right - left, bottom - top)
```

- Both edges are floored from their own fraction, so two rectangles that share
  an edge fraction share the same pixel boundary, and a set of rectangles that
  tiles the unit square tiles the work area without gaps or overlap.
- The pixel bias is `1e-6`. It only corrects floating-point error (see
  [Precision](#precision)): it moves an edge by at most the bias, so only an
  edge less than the bias below a pixel boundary lands on the next pixel.
- For the halves used by the named actions, `floor(0.5 * W)` equals `W / 2`,
  so their bounds are exactly what the dedicated half-split code produced
  before (a unit test compares both across small, odd, and extreme sizes).
  For an odd width the left half gets the smaller share.
- The placement is skipped (the chord is still [consumed](#chord-handling))
  when the work area is unavailable, when an edge strictly inside `(0, 1)`
  rounds onto the work-area border (the output is too small to represent the
  rectangle), when a rounded extent is empty, or when `X + left` /
  `Y + top` overflows.

The owner of this rule is `NormalizedRect::to_work_area_bounds` in
`sommelier/src/window_layout.rs`.

## Reloading

Sommelier notices a changed file by itself, with inotify on the proxy's
event loop; there is no signal, socket, thread or timer, and a key press
never reads the file. The path may lead through symbolic links, of the
file or of a directory on the path, and each link adds a path: its
target's, with the rest of the path below it. On every such path the proxy
watches:

- the file, through symbolic links, for a write closed and a change of its
  attributes, so an edit in place counts;
- the deepest directory on the path that exists, for the entry below it on
  the path: a rename over the file (an editor or a dotfile tool), a swap of
  a link (a Home Manager activation, or a `current` link of a dotfiles
  repository), a removal, and a directory, the file or a link's target
  being created, so a link that dangles until its target appears, or
  whose target is deleted and created anew, counts too;
- that directory's parent, for the directory's own entry, which a Home
  Manager activation can swap as a whole link.

Only the directory that holds the file reports writes; the others report
only their entries' changes, so writes elsewhere in the home directory do
not wake the proxy. On a link's path a directory the proxy cannot write,
such as the Nix store, is not watched: only the system changes it, and a
swap of the link into it is seen where the link is.

Each event that concerns a path arms these watches again, since the files
behind the paths may have changed, and reloads the file. A regular file
that has just been created is read only once its writer closes it or it
is renamed into place, whatever other event comes with it, so a
half-written file is not reported. A reload loads the file only when its
identity changed, as `stat` sees it through symbolic links: the device and
inode, the modification and change times, and the size. So the several
events of one change load it once and warn once:

- Home Manager installs the file as a link into `/nix/store`, where every
  file's modification time is 1. An activation points the link at another
  store file with the same time, so the inode is what tells them apart.
- An editor that writes a new file and renames it over the old one gives it
  a new inode; one that rewrites it in place changes its times or size. An
  in-place rewrite to the same size within one tick of the kernel's file
  clock can go unnoticed until the next change.

Then:

- The new document is validated completely, with the same rules and the same
  startup `SOMMELIER_ACCELERATORS` list. A valid one replaces the bindings
  at once, whole, and the next key press sees them; the log says how many
  bindings are active.
- An unusable one is logged once and the last good bindings stay (none, if
  the file was never valid). Removing the file clears the bindings.
- A key already captured as a shortcut stays captured until it is released,
  even if its binding was removed, so the guest never sees an unpaired
  release. Key repeat never repeats a placement.
- A reload never changes Aura objects or placements in flight; whether a
  connection has placement is decided as it connects (see
  [The file's states](#the-files-states)).
- A reload lands between two messages the proxy handles, never during one,
  so a message sees one version of the bindings. A press's
  `zcr_extended_keyboard_v1.peek_key` and its `wl_keyboard.key`, which Exo
  sends one right after the other, see the same version unless the file
  changes in the instant between them.

A watch that cannot be made for another reason than a missing or
unreadable directory, for example because the per-user inotify watch limit
is reached, is warned about once. If no directory on the paths can be
watched, or inotify cannot be set up at all, the proxy says it stopped
watching and keeps the bindings it has; an edit then takes effect when it
restarts.

A reload runs on the proxy's one thread, between other tasks' steps. The
file is read synchronously there; the 64 KiB bound and the regular-file
check keep that read short. The file is expected on a local file system,
whose changes inotify reports.

## Manual test plan

Placement is verified by unit and Session tests only; this plan is for a
host check. Follow
[Manual verification](../../docs/INVARIANTS.md#manual-verification): run a
separate test proxy on an unused socket, never the primary service, and
make sure it runs the binary you built.

Give the test proxy a configuration directory of its own, so that it never
reads or changes yours:

```sh
export XDG_CONFIG_HOME="$(mktemp -d)"
mkdir "$XDG_CONFIG_HOME/sommelier-rs"
$EDITOR "$XDG_CONFIG_HOME/sommelier-rs/window-placement.toml"  # e.g. the example
RUST_LOG=sommelier=debug \
  ./target/release/sommelier --virtio-wl /dev/wl0 wayland-placement-test
WAYLAND_DISPLAY=wayland-placement-test <app>
```

With a GTK app (such as `gedit`), a non-GTK one (such as `foot`), and one
that asks Exo for a frame, if available:

1. The startup log names the loaded bindings, and no configuration error.
   Editing the file (in place, by rename, and by swapping a symbolic link)
   takes effect on the next chord; an invalid edit logs one warning and
   keeps the last bindings; removing the file clears them.
2. Every named action, and one `rect` from a file, moves the focused
   window to its region of the work area, which excludes the shelf, with
   no gap or overlap between neighbours; each placement logs
   `window layout ...`, and the debug log shows `set_parent(self, ...)`
   and the configure size. A window with Exo's frame keeps its caption
   inside the region.
3. Repeated chords, and a chord on a maximized, fullscreen or snapped
   window, land in the same region.
4. The window stays on its own monitor with two outputs, and placement is
   right on a rotated output and at a fractional display scale; the window
   content stays sharp.
5. Korean input in `sommelier_test_app` composes and commits before and
   after a placement, the shelf icon still matches the app, and pinning
   works.
6. A chord is never typed into the app, even when a placement is skipped
   (each skip is logged), and the log never says `Exo ignored
   zaura_surface.set_parent`.
7. Window states still work without placement chords: maximize,
   fullscreen, minimize, interactive move and resize, and menus.

Record the host's Chrome version and each result.

Host checks so far: 2026-11-20, gedit, all nine named actions placed
correctly, with no warning in the log.

## Code map

| File | Role |
| --- | --- |
| `sommelier/src/settings.rs` | `Settings`: the bindings file's path and the parsed `SOMMELIER_ACCELERATORS`, resolved once. |
| `sommelier/src/placement_config.rs` | `Bindings` (one validated version of the file), `PlacementConfig` (the current version, which every connection shares), `BindingsFile` (the identity check that reloads the file and publishes the result), the hardened file read, and the one validator (which takes the host accelerators as an input). |
| `sommelier/src/placement_config/watch.rs` | `load_and_watch`: the chain of paths through the bindings file's links, the inotify watches on them, and the event-loop task that reloads the file. |
| `sommelier/src/window_layout.rs` | `NormalizedRect`: the named presets and their parser, validation, and the rounding rule that maps a rectangle into a work area (a `Rect` of `sommelier/src/geometry.rs`, which the output registry shares) as window bounds. |
| `sommelier/src/accelerator.rs` | Chord types and parsing, and `BindingId`, the binding of a chord the keyboard consumes as a shortcut. |
| `sommelier/src/state.rs` | `Context` holds the connection's owners below and its `host_zaura_shell` binding; `Context::new` decides whether the connection has placement; `shortcut_for` and `placement_rect` ask the shared `PlacementConfig` for a chord's binding and a binding's rectangle, `is_host_accelerator` the settings, and `placement` and `placement_mut` give the placement state while placement is on. |
| `sommelier/src/state/window_placement.rs` | `WindowPlacement` (present only while placement is on) and `PlacementState`: each toplevel's Aura toplevel, its last reported origin, the states and serial of its newest configure, and the placement in flight, keyed by typed guest and host IDs. |
| `sommelier/src/state/aura_surfaces.rs` | `AuraSurfaces`: the one host Aura surface of each `wl_surface`, shared by XDG and GTK metadata and placement. |
| `sommelier/src/state/xdg_roles.rs` | `XdgRoles`: which `wl_surface` each `xdg_surface` and `xdg_toplevel` belongs to; placement finds the focused toplevel here. |
| `sommelier/src/state/internal_callbacks.rs` | `InternalCallbacks`: what each proxy-issued `wl_display.sync` waits for, including the placement barrier. |
| `sommelier/src/state/outputs.rs` | `OutputRegistry`: live outputs, their committed metadata, and the output a surface is placed on; `OutputState::work_area`. |
| `sommelier/src/handler/keyboard.rs` | `classify_key` turns a chord into host accelerator, shortcut (a `BindingId`), or guest input; a consumed shortcut's binding goes to `window_placement::shortcut`, placement being the keyboard's one shortcut subscriber. |
| `sommelier/src/handler/window_placement.rs` | Placement: the focused toplevel, its output and the bounds, the three steps and their origin math, the Aura toplevel of each XDG toplevel, and the translation of its configures; one entry point per protocol event, listed in its module docs. |
| `sommelier/src/handler/aura_objects.rs` | Creates, versions and releases every host Aura child (`AuraChild` table, `ensure_host_zaura_surface`, `ensure_zaura_output`), sends the shelf application ID, and releases the `zaura_shell` binding. |
| `sommelier/src/handler/callback.rs` | `queue_internal_sync` issues the placement barrier, and `wl_callback.done` dispatches it to `window_placement::complete_barrier`. |
| `sommelier/src/proxy.rs` | `Shared` loads and watches the bindings file once and shares the configuration with every connection. |

## Not carried over from the earlier draft

An earlier draft of this feature targeted a different placement architecture.
These parts were intentionally not reintroduced:

- `--experimental-window-placement`, `--window-host-policy` and
  `--window-geometry-method`. The bindings file is the only switch.
- `action = "window.place"`. A binding now names its target directly with
  `rect` or a named `action`.
- Rounding each edge to the nearest pixel. Flooring is required to keep the
  named actions' bounds identical on odd work-area sizes.
- Failing startup on any configuration error, including a malformed
  `SOMMELIER_ACCELERATORS`, and rejecting empty list elements in it. The
  proxy serves every app in the container, so a configuration error only
  disables what depends on it (see [The file's states](#the-files-states)).
- A binding list in an environment variable, a path option, `SIGHUP`
  reloads, and a per-instance control socket. One file at a fixed path,
  reloaded as it changes, replaces them all.
