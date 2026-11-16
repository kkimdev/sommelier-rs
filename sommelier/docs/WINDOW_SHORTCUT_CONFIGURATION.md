# Window shortcut configuration file

Compositor-owned window placement shortcuts can be configured in two ways:

- `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`, a comma-separated `CHORD=ACTION`
  list that is read once per client connection. Use it for short, fixed
  bindings.
- `--window-shortcuts-config PATH`, a versioned TOML file that accepts
  arbitrary rectangles as well as the named actions, and that can be reloaded
  with `SIGHUP` without restarting the proxy.

Both feed the same keyboard path: a matched chord on the focused XDG toplevel
becomes one `zaura_toplevel.set_window_bounds` request, which is gated by the
opt-in ARC policy (`SOMMELIER_WINDOW_BOUNDS_AS_ARC`). This document describes
the file.

## When the file is used

| `--window-shortcuts-config` | `SOMMELIER_WINDOW_BOUNDS_AS_ARC` | `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` | Result |
| --- | --- | --- | --- |
| absent | any | any | No file is read. Existing environment behavior. |
| given | any | set (not blank) | Startup error (exit 2). Pick one source. |
| given | off | unset or blank | Warning; the file is not opened. No shortcuts. |
| given | on | unset or blank | The file is loaded strictly; an invalid file is a startup error (exit 2). |

Notes:

- The gate is read only at startup. A reload never turns the ARC policy on or
  off, and never re-reads any environment variable.
- If the gate is requested but the proxy cannot reserve an ARC identity
  block, the ARC policy is disabled for the whole process and no connection
  consumes chords; the file still loads and reloads normally. See
  [Sommelier's task-ID allocator](ARC_TASK_AND_SESSION_IDS.md#sommeliers-task-id-allocator).
- `SOMMELIER_ACCELERATORS` is parsed at startup and every binding is checked
  against it. If that variable is malformed, the conflict check cannot run,
  so the file option is a startup error. (Without the file option, a malformed
  list still degrades to "no filtering" as before.)

## Format

```toml
version = 1

# A rectangle, as fractions of the output work area: [x, y, width, height].
[[bindings]]
chord = "<Alt>q"
rect = [0.0, 0.0, 0.5, 0.5]

# A centered column, two thirds wide.
[[bindings]]
chord = "<Alt>w"
rect = [0.1666667, 0.0, 0.6666667, 1.0]

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
    `<Super>`/`<Win>`/`<Search>`) followed by an XKB keysym name. Matching is
    exact on modifiers and case-insensitive on the keysym.
  - `action` is one of `top-left`, `top`, `top-right`, `left`, `fullscreen`,
    `right`, `bottom-left`, `bottom`, `bottom-right` (case-insensitive). These
    are the same names as the environment variable, so a list can be moved to
    the file unchanged. `fullscreen` fills the work area; it is ordinary
    placement, not native fullscreen.
  - `rect` is `[x, y, width, height]`. Integers such as `[0, 0, 1, 1]` are
    accepted.
- Unknown keys anywhere are rejected, so a misspelt field cannot be silently
  ignored.

Named actions were kept as an alternative to `rect` because they cost one
match arm (the name parser is shared with the environment variable) and they
make the common layouts readable. Each named action is defined as a rectangle:

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

## Validation

The whole document is validated before anything is used; nothing is applied
partially. It is rejected for:

| Check | Rule |
| --- | --- |
| File | Must be a regular file (checked on the opened descriptor, so a FIFO or device cannot block the proxy); at most 64 KiB; UTF-8. Symbolic links are followed. |
| TOML | Must parse; no unknown keys; `version = 1`; at most 256 bindings. |
| Chord | Must parse; must not duplicate an earlier binding's chord (compared after parsing, so `<Alt>q` and `<Meta>Q` are duplicates); must not be listed in `SOMMELIER_ACCELERATORS`. |
| Target | Exactly one of `action` / `rect`; `action` must be a known name. |
| Rect | All four values finite (no `nan`/`inf`); `0 <= x < 1`, `0 <= y < 1`; `0 < width <= 1`, `0 < height <= 1`; `x + width <= 1`, `y + height <= 1`. |

`x + width` and `y + height` may exceed `1` by at most `1e-9` to absorb
decimal-to-binary error (for example `0.1 + 0.9000000000000001`); an edge
within `1e-9` of `0` or `1` is snapped onto the border.

Error messages name the offending binding as `bindings[N]` (zero-based).

## From rectangle to window bounds

At key time the focused surface's output work area `(X, Y, W, H)` is computed
from the Aura scale factors, logical transform, and insets
(`OutputState::work_area`). Each rectangle edge fraction `f` becomes a pixel
offset:

```text
offset(f, W) = clamp(floor(f * W + 1e-6), 0, W)

left   = offset(x, W)          right  = offset(x + width,  W)
top    = offset(y, H)          bottom = offset(y + height, H)
bounds = (X + left, Y + top, right - left, bottom - top)
```

- Both edges are floored from their own fraction, so two rectangles that share
  an edge fraction share the same pixel boundary, and a set of rectangles that
  tiles the unit square tiles the work area without gaps or overlap.
- The `1e-6` pixel bias only corrects floating-point error (so `1/3` written
  as `0.3333333333333333` lands on the intended pixel); it never moves an
  edge that was meant to fall between pixels.
- For the halves used by the named actions, `floor(0.5 * W)` equals `W / 2`,
  so their bounds are exactly what the dedicated half-split code produced
  before (a unit test compares both across small, odd, and extreme sizes).
  For an odd width the left half gets the smaller share.
- The placement is skipped (logged at debug level; the chord is still
  consumed) when the work area is unavailable, when an edge strictly inside
  `(0, 1)` rounds onto the work-area border (the output is too small to
  represent the rectangle), when a rounded extent is empty, or when
  `X + left` / `Y + top` overflows.

The owner of this rule is `NormalizedRect::to_work_area_bounds` in
`sommelier/src/window_layout.rs`.

## Reloading

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
  last known-good bindings stay active. Deleting the file does not disable
  shortcuts; install a file with no bindings instead.
- Every connected client adopts the new generation the next time it matches
  a key press. A key already captured as a shortcut stays captured until it
  is released, even if its binding was removed, so the guest never sees an
  unpaired release. Key repeat never repeats a placement.
- The ARC policy, ARC identities, Aura objects, and pending bounds barriers
  are untouched by a reload.
- The `SIGHUP` handler is installed only when a configuration file is in use.
  Without `--window-shortcuts-config` (or with the gate off) `SIGHUP` keeps
  its default action and terminates the proxy, as before. With the file in
  use, closing a controlling terminal no longer terminates a proxy started
  from it.

Signal delivery goes through Tokio's signal driver (an internal self-pipe), and
the reload runs on the proxy's single-threaded event loop between client
accepts, so it never races a key event. The file is read synchronously there;
the 64 KiB bound and the regular-file check keep that read short.

## Code map

| File | Role |
| --- | --- |
| `sommelier/src/window_shortcuts.rs` | Schema, strict parser, hardened file read, `ShortcutConfigSource` (path + last good bindings + reload), `ShortcutSubscription`, and the startup gate. |
| `sommelier/src/window_layout.rs` | `NormalizedRect` validation and the work-area rounding rule; the named actions' rectangles. |
| `sommelier/src/accelerator.rs` | Chord parsing, `WindowLayoutAction` (named presets plus `Rect`), the shared action-name parser, and `SOMMELIER_ACCELERATORS` reading. |
| `sommelier/src/state.rs` | `Context::follow_window_placement_shortcuts` / `refresh_window_placement_shortcuts`. |
| `sommelier/src/handler/keyboard.rs` | Refreshes before matching a press; `bounds_for_action` delegates to the rounding rule. |
| `sommelier/src/proxy.rs` | Installs the `SIGHUP` stream when a file is in use and runs reloads in the accept loop. |
| `sommelier/src/main.rs` | The `--window-shortcuts-config` option; startup errors exit with status 2. |

## Not carried over from the earlier draft

An earlier draft of this feature targeted a different placement architecture.
These parts were intentionally not reintroduced:

- `--experimental-window-placement`, `--window-host-policy`,
  `--window-geometry-method`, and the position-only self-parent backend. The
  ARC policy environment gate is the only switch, and bounds are the only
  geometry method.
- `action = "window.place"`. A binding now names its target directly with
  `rect` or a named `action`.
- Rounding each edge to the nearest pixel. Flooring is required to keep the
  named actions' bounds identical on odd work-area sizes.
- Failing startup on a malformed `SOMMELIER_ACCELERATORS` and rejecting empty
  list elements in it. That would change existing environment-only
  deployments; the file option alone fails closed on it.
- A per-instance control socket for selecting a different path at runtime.
  `SIGHUP` reloads the startup path; a socket remains a possible follow-up.
