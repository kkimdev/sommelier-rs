# Window placement shortcut configuration

Compositor-owned window placement shortcuts move the focused XDG toplevel
to a configured region of its output's work area when a configured chord is
pressed. A placement backend moves it: the ARC backend with one
`zaura_toplevel.set_window_bounds` request, the remote-shell-v2 backend with
`zcr_remote_surface_v2.set_bounds_in_output` (see [Backends](#backends)). This document is the single owner of their contract; the README
and the code link here instead of restating it.

## Settings

| Setting | Meaning |
| --- | --- |
| `SOMMELIER_WINDOW_PLACEMENT_BACKEND` | The placement backend: `arc`, `remote-shell-v2` or `off` (case-insensitive, surrounding whitespace ignored). Unset or blank, `SOMMELIER_WINDOW_BOUNDS_AS_ARC` decides. `off` wins over that gate. Any other value, a value that is not UTF-8, or `remote-shell-v2` while that gate is on is a configuration problem (see [Resolution at startup](#resolution-at-startup)). |
| `SOMMELIER_WINDOW_BOUNDS_AS_ARC` | The legacy gate, kept for compatibility: while `SOMMELIER_WINDOW_PLACEMENT_BACKEND` is unset, it chooses the ARC backend when on. On for `1`, `true`, `yes` or `on` (case-insensitive, surrounding whitespace ignored); off for anything else, including a value that is not UTF-8. |
| `SOMMELIER_ACCELERATORS` | Chords the host compositor keeps. A binding may not use one of them. |
| `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` | Binding source: a `CHORD=ACTION` list with named actions only. It never reloads. |
| `--window-shortcuts-config PATH` | Binding source: a versioned TOML file with named actions or arbitrary rectangles. `SIGHUP` reloads it. |

The two binding sources are mutually exclusive; giving both is a
configuration problem (see [Resolution at startup](#resolution-at-startup)).
`PlacementConfig::from_environment` reads every setting once, at startup, so
a change to the environment needs a restart. A variable that is empty or only
whitespace counts as unset. For `SOMMELIER_WINDOW_PLACEMENT_BACKEND`,
`SOMMELIER_ACCELERATORS` and `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`, a value
that is not UTF-8 counts as set and invalid; for the gate it means off, as
the table says.

## Resolution at startup

At startup the settings resolve to one process-wide configuration that every
client connection shares. **A configuration problem never stops the proxy**:
Sommelier is the Wayland proxy for every app in the container, so exiting
would close them all and could restart-loop under a service manager. Each
problem is logged once per process instead. The rules apply in this order:

1. **Placement needs a backend.** The backend setting, or the legacy gate,
   chooses one; an unusable backend setting logs one error and leaves
   placement off. Without a backend nothing is reserved. The ARC backend
   reserves the proxy's block of ARC task IDs (see
   [Sommelier's task-ID allocator](ARC_TASK_AND_SESSION_IDS.md#sommeliers-task-id-allocator));
   if that fails, placement is off for the whole process. While placement
   is off, any requested binding source (one or both) is ignored with one
   warning and is not read or validated, and no chord is consumed.
2. **An unusable binding source disables shortcuts.** While a backend is
   active, a requested source is unusable in these cases:
   - both sources are given;
   - the environment list is not UTF-8 or fails validation;
   - the file path cannot be resolved, or the file is missing, unreadable,
     or fails validation;
   - `SOMMELIER_ACCELERATORS` is invalid, so no binding can be checked
     against it.

   Each case logs one error that says why. The proxy then runs with placement
   shortcuts disabled: no binding is active and no chord is consumed. The
   backend itself stays active. With an invalid `SOMMELIER_ACCELERATORS`, key
   filtering is disabled too.
3. **A file can recover by reload.** If the file was the only source, its
   path resolved, and `SOMMELIER_ACCELERATORS` is valid, `SIGHUP` re-reads
   it, so a fixed file activates its bindings without a restart (see
   [Reloading](#reloading)). Every other unusable case needs a restart; the
   table below says which.
4. **Host accelerators alone only lose filtering.** Without a usable binding
   source (no source, or placement is off), a malformed
   `SOMMELIER_ACCELERATORS` logs one warning and disables key filtering, so
   every key goes to the guest.

| Backend | Binding source | Result |
| --- | --- | --- |
| off (none chosen, an unusable setting, or no ARC block) | none | No placement. |
| off | one or both | One warning; no source is read. No placement. |
| active | none | The backend's metadata path only (for ARC, the ARC identity); no chord is consumed. |
| active | one, valid | The bindings are active. |
| active | both | One error; shortcuts disabled; no reload. |
| active | environment list invalid or not UTF-8 | One error; shortcuts disabled; no reload. |
| active | file missing, unreadable or invalid | One error; shortcuts disabled until a valid `SIGHUP` reload. |
| active | file path cannot be resolved | One error; shortcuts disabled; no reload. |
| active | one, with `SOMMELIER_ACCELERATORS` invalid | One error; shortcuts and key filtering disabled; no reload. |

## Backends

Every backend shares the configuration, the chord handling, the output and
work area, and the rounding rule below; it differs only in how the window
is moved, and in what that costs:

| | `arc` | `remote-shell-v2` |
| --- | --- | --- |
| Host requirement | Any Exo whose `zaura_shell` sends `zaura_output.insets` | An Exo whose security delegate allows remote shell, which Crostini's does not |
| Window classification | ARC app: the surface's Aura application ID is an ARC identity | Unchanged: the Guest OS application ID |
| ChromeOS IME (Korean and other composition) | Expected to be bypassed in every window, so composition does not work (unverified on a host; see [The ARC backend and the IME](#the-arc-backend-and-the-ime)) | Expected to keep working, since the window is not ARC-classified (unverified on a host) |
| Other side effects | Clipboard, drag and drop, restore and more may change ([ARC task and session IDs](ARC_TASK_AND_SESSION_IDS.md)) | XDG popups, window states and window geometry are not bridged (below) |

So for using the ChromeOS IME in Linux apps the ARC backend is not usable
as it stands, and the remote-shell-v2 backend needs a host that offers it.

- **`arc`** sends `zaura_toplevel.set_window_bounds` in screen
  coordinates. Exo accepts it only from a surface it classifies as an ARC
  window, so every surface's Aura application ID becomes a per-surface
  `org.chromium.arc.<task_id>` identity while the backend is active (see
  [ARC task and session IDs](ARC_TASK_AND_SESSION_IDS.md)). That
  classification also changes how Chrome treats the window beyond its
  bounds, most of all its keyboard input: the ChromeOS IME is expected to
  be bypassed (see [The ARC backend and the IME](#the-arc-backend-and-the-ime)).
  Before the bounds, the window leaves fullscreen, maximized and snapped
  state; a host barrier holds the configures Exo sends until it has applied
  them.
- **`remote-shell-v2`** gives each toplevel's surface a host
  `zcr_remote_surface_v2` role from a host `zcr_remote_shell_v2` (v4 or
  newer), which the proxy binds for itself and never advertises to the
  guest. Exo gives a surface one shell role, so while that global is bound
  the guest's `xdg_surface`, `xdg_toplevel` and `xdg_popup` are implemented
  by the proxy and never reach the host. A placement sends
  `set_bounds_in_output`, relative to the output, and the guest a configure
  of the new size; the guest's next commit applies the bounds. The remote
  surface's scale factor is 1.0, so its coordinates are DIPs, like the work
  area. The surface keeps the Guest OS application ID, so none of the ARC
  side effects apply. The backend has these limits:
  - Stock Crostini does not advertise `zcr_remote_shell_v2`: Exo offers it
    only to a server whose security delegate allows remote shell, and
    Crostini's does not. Without it, windows take the normal XDG path,
    one warning is logged, and every placement is skipped.
  - XDG popups (menus, tooltips) are dismissed at once with `popup_done`:
    Exo parents a popup only to an XDG surface. Since nothing reaches the
    host, their parent and positioner are not checked as Exo would.
  - Window states (maximize, fullscreen, minimize), interactive move and
    resize, and size limits are not bridged; the guest's configures carry
    no states. `xdg_surface.set_window_geometry` is dropped, since a remote
    surface has no window geometry, so the bounds of a client-side
    decorated window include its shadow margin, and windows placed side by
    side show a gap between their visible edges.
  - Exo maps a remote surface without a frame (its default frame type is
    none), so a client that draws no decorations has none. It would also
    map it at the output's origin and send it no bounds request until its
    client sets bounds, so the commit that first shows a toplevel sets its
    bounds, centred in the work area at the committed size. If that
    output's Aura metadata is not complete yet, the window waits at the
    origin until it is, and then gets those bounds with a configure.
  - Exo's own bounds requests (`bounds_changed_in_output`, in the display's
    pixels, scaled by the display zoom and the device scale factor) are
    accepted, converted to DIPs. Aura reports both factors in truncated
    thousandths, so at a scale such as 16/9 a converted edge can be one
    DIP off. A close request reaches the guest.
  - With `--xdg-decoration`, a decoration for one of these toplevels is
    the proxy's own and never configures, so the client keeps its own
    decorations: the host has no XDG toplevel to decorate.
  - A toplevel created while no remote shell is bound stays an XDG
    toplevel and is never placed, even after one is bound.

### The ARC backend and the IME

**The ARC backend is expected to disable the ChromeOS IME, including
Korean composition, in every window while it is active.** This follows
from Chromium's source and has not been verified on a host:

- [`exo_app_type_resolver.cc`](https://chromium.googlesource.com/chromium/src/+/main/chrome/browser/ui/ash/shelf/app_service/exo_app_type_resolver.cc)
  classifies a window whose application ID parses as an ARC task or
  session, such as the `org.chromium.arc.<task_id>` identity this backend
  gives every surface, as an ARC app, and sets
  `aura::client::kSkipImeProcessing` on it.
- [`window_event_dispatcher.cc`](https://chromium.googlesource.com/chromium/src/+/main/ui/aura/window_event_dispatcher.cc)
  (`WindowEventDispatcher::PreDispatchKeyEvent`) then sends that window's
  key events past the IME.
- [`keyboard.cc`](https://chromium.googlesource.com/chromium/src/+/main/components/exo/keyboard.cc) treats the key as not
  consumed: Exo expects an ARC client to send the events back to the IME
  over its own bridge, which a Linux app does not have.

A remote-shell-v2 window keeps the Guest OS application ID, so it is not
ARC-classified and the IME is expected to keep working there; that backend
needs a host whose security delegate allows remote shell
(`CanAccessRemoteShell`), which Crostini's does not.

## Chord handling

While a backend is active, a chord that matches a binding is always
consumed: it never reaches the guest, even when the placement is skipped
(for example, with no focused toplevel, before the output's metadata is
complete, or for a rectangle the output cannot represent). A chord listed in
`SOMMELIER_ACCELERATORS` stays with the host. A chord that the host IME
consumes is neither placed nor delivered.

Every skip is logged with its reason. A skip that persists until the
configuration or the proxy changes is logged at `info`, which the default
log filter shows: a rectangle the output cannot represent, no Aura
toplevel (ARC identities exhausted, or `zaura_shell` missing or too old), or
no remote surface (remote-shell-v2 with no usable `zcr_remote_shell_v2`
when the toplevel was created).
The other skips (no focused toplevel, no usable output yet, no Aura surface)
are logged at `debug`; set `RUST_LOG=debug` to see them.
`PlacementSkip::log_level` in `sommelier/src/handler/window_placement.rs`
owns this split.

## Environment list format

```text
SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS="<Alt>q=top-left,<Alt>w=top,<Alt>s=fullscreen"
```

A comma-separated list of `CHORD=ACTION` entries. Whitespace around each
chord and action is ignored, and so are empty entries. Only the
[named actions](#named-actions) are available; use the file for rectangles.
A list can move to the file unchanged.

## File format

```toml
version = 1

# A rectangle, as fractions of the output work area: [x, y, width, height].
[[bindings]]
chord = "<Alt>q"
rect = [0.0, 0.0, 0.5, 0.5]

# A centered column, two thirds wide (see "Precision").
[[bindings]]
chord = "<Alt>w"
rect = [0.1666666666666667, 0.0, 0.6666666666666666, 1.0]

# One of the named actions.
[[bindings]]
chord = "<Alt>s"
action = "fullscreen"
```

- `version` is required and must be `1`. A file with a different version is
  rejected before its other fields are checked.
- `bindings` is an array of tables. It may be empty or omitted; an empty list
  is the supported way to disable every shortcut.
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

The environment list and the file share these names
(`WindowLayoutAction::from_name` is the one name parser). Each stands for a
fixed rectangle: `WindowLayoutAction::rect` in
`sommelier/src/window_layout.rs` defines the values, and a unit test keeps
this table and the code in sync.

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

Both sources go through one validator, `validate_bindings`. Nothing is
applied partially: the whole list or document is validated before anything
is used. It is rejected for:

| Check | Rule |
| --- | --- |
| File (file only) | Must be a regular file (checked on the opened descriptor, so a FIFO or device cannot block the proxy); at most 64 KiB; UTF-8. Symbolic links are followed. |
| TOML (file only) | Must parse; no unknown keys; `version = 1`. |
| Syntax (list only) | Every non-empty entry has the form `CHORD=ACTION`. |
| Size | At most 256 bindings. |
| Chord | Must parse; must not duplicate an earlier binding's chord (compared after parsing, so `<Alt>q` and `<Meta>Q` are duplicates); must not be listed in `SOMMELIER_ACCELERATORS`. |
| Target | Exactly one of `action` / `rect`; `action` must be a known name. |
| Rect | All four values finite (no `nan`/`inf`); `0 <= x < 1`, `0 <= y < 1`; `0 < width <= 1`, `0 < height <= 1`; `x + width <= 1`, `y + height <= 1`. |

The edge tolerance is `1e-9`: an edge within it of `0` or `1` *is* that
border and snaps onto it, so `x + width` and `y + height` may exceed `1` by
at most the tolerance. This absorbs decimal-to-binary error (for example
`0.1 + 0.9000000000000001`).

Error messages name the offending binding as `bindings[N]`, counted from
zero; in the environment list, empty entries are not counted.

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
shelf and other reserved margins), in the coordinates that
`zaura_toplevel.set_window_bounds` uses. `OutputState::work_area` in the same
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

Only the file reloads; the environment list never does.

```sh
# Write the new file atomically, then ask the proxy to reload it.
cp shortcuts.toml shortcuts.toml.new && $EDITOR shortcuts.toml.new
mv shortcuts.toml.new shortcuts.toml
kill -HUP "$SOMMELIER_PID"
# For the user service: systemctl --user kill -s HUP sommelier-rs@wayland-2.service
```

- `SIGHUP` re-reads the path given at startup (resolved to an absolute path
  once, against the startup working directory). There is no file watcher.
- The new document is validated completely, with the same rules and the same
  startup `SOMMELIER_ACCELERATORS` list. Only a valid document replaces the
  bindings, as one atomic swap; the log says how many bindings are active.
- If the file is missing, unreadable, or invalid, the error is logged and the
  last known-good bindings stay active. If the file was unusable at startup,
  those are no bindings, and the first valid reload activates the file.
  Deleting the file does not disable shortcuts; install a file with no
  bindings instead.
- Every connection sees the new generation from its next key press. A key
  already captured as a shortcut stays captured until it is released, even
  if its binding was removed, so the guest never sees an unpaired release.
  Key repeat never repeats a placement.
- A reload never changes the backend, ARC identities, Aura
  objects, or pending bounds barriers, and it never re-reads an environment
  variable.
- The `SIGHUP` handler is installed in every configuration, right after
  argument parsing and before the file is first read, so `SIGHUP` never
  terminates the proxy. If no file is in use (no option, placement is off,
  or startup could not use the file at all), `SIGHUP` is logged and ignored.
  If the handler cannot be installed, that is logged and the proxy runs
  without reloads.

Signal delivery goes through Tokio's signal driver (an internal self-pipe), and
the reload runs in `reload_on_signal`, a task of its own on the proxy's one
thread: it runs between other tasks' steps, so it never races a key event,
and it never waits for the accept loop. The file is read synchronously
there; the 64 KiB bound and the regular-file check keep that read short. If
that task ends, an error says reloads have stopped.

## Manual test plan

Both backends are verified by unit and Session tests only; this plan is
for a host check. Follow
[Manual verification](../../docs/INVARIANTS.md#manual-verification): run a
separate test proxy on an unused socket, never the primary service, and
make sure it runs the binary you built.

```sh
WIN=shortcuts.toml   # for example the file in "File format"
RUST_LOG=sommelier=debug SOMMELIER_WINDOW_PLACEMENT_BACKEND=<backend> \
  ./target/release/sommelier --virtio-wl /dev/wl0 \
  --window-shortcuts-config "$WIN" wayland-placement-test
WAYLAND_DISPLAY=wayland-placement-test <app>
```

For each backend, with a GTK app (such as `gedit`) and a non-GTK one
(such as `foot`):

1. The startup log names the backend and the loaded bindings, and no
   configuration error.
2. Every named action, and one `rect`, moves the focused window to its
   region of the work area, which excludes the shelf, with no gap or
   overlap between neighbours (under remote-shell-v2, a client-side
   decorated window's shadow margin is part of its bounds, so its visible
   edges leave a gap); each placement logs `window layout ...`.
3. The window stays on its own monitor with two outputs, and placement is
   right on a rotated output and at a fractional display scale; the window
   content stays sharp.
4. The shelf icon still matches the app, and pinning works.
5. A chord is never typed into the app, even when a placement is skipped
   (each skip is logged).

**ARC** (`SOMMELIER_WINDOW_PLACEMENT_BACKEND=arc`): Chrome treats every
window as an ARC window, so also check, against a run with placement off:

- IME: type Korean in an ARC-placed window (`sommelier_test_app`);
  expect the IME to be bypassed, so no composition appears (see
  [The ARC backend and the IME](#the-arc-backend-and-the-ime)). Record
  whether it is, which no host has confirmed yet.
- Clipboard: copy and paste in both directions, text and an image.
- Drag and drop between a guest window and a Chrome window.
- Window restore: after signing out and in, or a restart of the VM, no
  ghost or duplicate windows appear, and no window is restored as an
  Android app.
- Pointer lock in a game or a remote-desktop client, and the accessibility
  tree (ChromeVox reads the window).
- Overview, Alt+Tab and the shelf's window list show each window once.

**Remote shell** (`SOMMELIER_WINDOW_PLACEMENT_BACKEND=remote-shell-v2`):
it needs a host whose Exo offers `zcr_remote_shell_v2` to the guest's
server, which stock Crostini does not; the log then says
`Bound zcr_remote_shell_v2 v<N>`. If it warns that the host has no
`zcr_remote_shell_v2`, the backend cannot be tested on that host. Then
also check:

- A new window maps with its title, centred in the work area at its own
  size, before any placement, and without a frame of Exo's: a client that
  draws no decorations shows none.
- Its content is sharp at 1x, 2x and a fractional scale (the remote
  surface's scale factor of 1.0 against the guest's buffer scale).
- A placement takes effect at the app's next frame; an app that ignores a
  same-size configure may not move on a placement that only changes the
  position.
- Exo's own moves (a display change, a snap from the window's frame
  menu) leave the window where Exo put it, without the debug log showing
  `set_bounds_in_output` repeating, which would mean the proxy and Exo
  disagree about units.
- Closing the window from the shelf or overview closes the app.
- Menus and tooltips are dismissed at once (a known limit), and
  maximize, fullscreen and interactive move or resize do nothing.
- None of the ARC side effects above appear; in particular, Korean input
  in `sommelier_test_app` composes and commits, and renders as glyphs.
- Exo's remote-shell code sets its default scale cancellation for the
  whole Exo server when a client binds `zcr_remote_shell_v2`; check that
  Android apps and other guest windows still scale correctly while the
  test proxy runs.

Record the host's Chrome version, the backend, and each result.

## Code map

| File | Role |
| --- | --- |
| `sommelier/src/placement_config.rs` | `PlacementConfig`: startup resolution of every setting, both binding parsers, the one validator, the hardened file read, the current binding generation, and reload. |
| `sommelier/src/window_layout.rs` | `WindowLayoutAction` (named presets plus a configured rect) and its name parser; `NormalizedRect` validation and the rounding rule that maps it from a work-area `Rect` to window bounds. |
| `sommelier/src/accelerator.rs` | Chord types and parsing only. |
| `sommelier/src/state.rs` | `Context` holds the connection's owners below and its `host_zaura_shell` binding; `placement_action_for` / `is_host_accelerator` ask the shared `PlacementConfig`. |
| `sommelier/src/state/window_placement.rs` | `WindowPlacementState`: which backend a connection uses, with that backend's state; `Off` holds nothing. |
| `sommelier/src/state/window_placement/arc.rs` | `ArcPlacementState`, the ARC backend's per-connection state: ARC identities (from the `PlacementConfig` block), placement Aura toplevels, the barrier that gates each one, and the configures held behind it. |
| `sommelier/src/state/aura_surfaces.rs` | `AuraSurfaces`: the one host Aura surface of each `wl_surface`, shared by XDG and GTK metadata and placement. |
| `sommelier/src/state/xdg_roles.rs` | `XdgRoles`: which `wl_surface` each `xdg_surface` and `xdg_toplevel` belongs to; placement finds the focused toplevel here. |
| `sommelier/src/state/internal_callbacks.rs` | `InternalCallbacks`: what each proxy-issued `wl_display.sync` waits for, including the placement barrier. |
| `sommelier/src/state/outputs.rs` | `OutputRegistry`: live outputs, their committed metadata, and the output a surface is placed on; `OutputState::work_area`. |
| `sommelier/src/handler/keyboard.rs` | `classify_key` turns a chord into host accelerator, placement, or guest input; a consumed placement chord calls `window_placement::place_focused`. |
| `sommelier/src/handler/window_placement.rs` | The placement boundary: the focused toplevel, its output and the bounds (shared by every backend), `set_aura_application_id` (the guest's ID, or the one the backend chooses, for XDG and GTK), and one entry point per protocol event (listed in its module docs), each of which dispatches to the connection's backend. |
| `sommelier/src/handler/window_placement/remote_shell.rs` | The remote-shell-v2 backend: the proxy-local XDG objects, the remote surface and `set_bounds_in_output`, and the host's remote-shell events. |
| `sommelier/src/state/window_placement/remote_shell.rs` | `RemoteShellPlacementState`, its per-connection state: the bound manager, each toplevel's remote surface, and the configure serial. |
| `sommelier/src/handler/window_placement/arc.rs` | The ARC backend: the ARC identity, the placement Aura toplevel, `set_window_bounds` and its barrier, and the configures held behind it. |
| `sommelier/src/handler/aura_objects.rs` | Creates, versions and releases every host Aura child (`AuraChild` table, `ensure_host_zaura_surface`, `ensure_zaura_output`), and releases the `zaura_shell` binding. |
| `sommelier/src/handler/callback.rs` | `queue_internal_sync` issues the placement barrier, and `wl_callback.done` dispatches it to `window_placement::complete_barrier`. |
| `sommelier/src/main.rs` | The `--window-shortcuts-config` option, and when the `SIGHUP` handler is installed. |
| `sommelier/src/proxy.rs` | Shares the configuration with every connection; `install_reload_signal` installs the `SIGHUP` handler and `reload_on_signal` runs reloads. |

## Not carried over from the earlier draft

An earlier draft of this feature targeted a different placement architecture.
These parts were intentionally not reintroduced:

- `--experimental-window-placement`, `--window-host-policy`,
  `--window-geometry-method`, and the position-only self-parent backend. The
  backend setting is the only switch, and bounds are the only
  geometry method.
- `action = "window.place"`. A binding now names its target directly with
  `rect` or a named `action`.
- Rounding each edge to the nearest pixel. Flooring is required to keep the
  named actions' bounds identical on odd work-area sizes.
- Failing startup on any configuration error, including a malformed
  `SOMMELIER_ACCELERATORS`, and rejecting empty list elements in it. The
  proxy serves every app in the container, so a configuration error only
  disables what depends on it (see [Resolution at startup](#resolution-at-startup)).
- A per-instance control socket for selecting a different path at runtime.
  `SIGHUP` reloads the startup path; a socket remains a possible follow-up.
