# Window placement shortcut configuration

Compositor-owned window placement shortcuts move the focused XDG toplevel
with one `zaura_toplevel.set_window_bounds` request when a configured chord is
pressed. This document is the single owner of their contract; the README
and the code link here instead of restating it.

## Settings

| Setting | Meaning |
| --- | --- |
| `SOMMELIER_WINDOW_BOUNDS_AS_ARC` | The gate for the opt-in ARC bounds policy that placement needs. On for `1`, `true`, `yes` or `on` (case-insensitive, surrounding whitespace ignored); off for anything else, including a value that is not UTF-8. |
| `SOMMELIER_ACCELERATORS` | Chords the host compositor keeps. A binding may not use one of them. |
| `SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS` | Binding source: a `CHORD=ACTION` list with named actions only. It never reloads. |
| `--window-shortcuts-config PATH` | Binding source: a versioned TOML file with named actions or arbitrary rectangles. `SIGHUP` reloads it. |

The two binding sources are mutually exclusive; giving both is a
configuration problem (see [Resolution at startup](#resolution-at-startup)).
`PlacementConfig::from_environment` reads every setting once, at startup, so
a change to the environment needs a restart. A variable that is empty or only
whitespace counts as unset. For `SOMMELIER_ACCELERATORS` and
`SOMMELIER_WINDOW_PLACEMENT_SHORTCUTS`, a value that is not UTF-8 counts as
set and invalid; for the gate it means off, as the table says.

## Resolution at startup

At startup the settings resolve to one process-wide configuration that every
client connection shares. **A configuration problem never stops the proxy**:
Sommelier is the Wayland proxy for every app in the container, so exiting
would close them all and could restart-loop under a service manager. Each
problem is logged once per process instead. The rules apply in this order:

1. **The policy needs the gate and an ARC block.** With the gate off nothing
   is reserved. With the gate on, the proxy reserves its block of ARC task
   IDs (see
   [Sommelier's task-ID allocator](ARC_TASK_AND_SESSION_IDS.md#sommeliers-task-id-allocator));
   if that fails, the policy is off for the whole process. While the policy
   is off, any requested binding source (one or both) is ignored with one
   warning and is not read or validated, and no chord is consumed.
2. **An unusable binding source disables shortcuts.** While the policy is
   active, a requested source is unusable in these cases:
   - both sources are given;
   - the environment list is not UTF-8 or fails validation;
   - the file path cannot be resolved, or the file is missing, unreadable,
     or fails validation;
   - `SOMMELIER_ACCELERATORS` is invalid, so no binding can be checked
     against it.

   Each case logs one error that says why. The proxy then runs with placement
   shortcuts disabled: no binding is active and no chord is consumed. The ARC
   policy itself stays active. With an invalid `SOMMELIER_ACCELERATORS`, key
   filtering is disabled too.
3. **A file can recover by reload.** If the file was the only source, its
   path resolved, and `SOMMELIER_ACCELERATORS` is valid, `SIGHUP` re-reads
   it, so a fixed file activates its bindings without a restart (see
   [Reloading](#reloading)). Every other unusable case needs a restart; the
   table below says which.
4. **Host accelerators alone only lose filtering.** Without a usable binding
   source (no source, or the policy is off), a malformed
   `SOMMELIER_ACCELERATORS` logs one warning and disables key filtering, so
   every key goes to the guest.

| Policy (gate and ARC block) | Binding source | Result |
| --- | --- | --- |
| off | none | No placement. |
| off | one or both | One warning; no source is read. No placement. |
| active | none | The ARC metadata path only; no chord is consumed. |
| active | one, valid | The bindings are active. |
| active | both | One error; shortcuts disabled; no reload. |
| active | environment list invalid or not UTF-8 | One error; shortcuts disabled; no reload. |
| active | file missing, unreadable or invalid | One error; shortcuts disabled until a valid `SIGHUP` reload. |
| active | file path cannot be resolved | One error; shortcuts disabled; no reload. |
| active | one, with `SOMMELIER_ACCELERATORS` invalid | One error; shortcuts and key filtering disabled; no reload. |

## Chord handling

While the policy is active, a chord that matches a binding is always
consumed: it never reaches the guest, even when the placement is skipped
(for example, with no focused toplevel, before the output's metadata is
complete, or for a rectangle the output cannot represent). A chord listed in
`SOMMELIER_ACCELERATORS` stays with the host. A chord that the host IME
consumes is neither placed nor delivered.

Every skip is logged with its reason. A skip that persists until the
configuration or the proxy changes (a rectangle the output cannot represent,
or a toplevel that cannot get an Aura toplevel) is logged at `info`, which
the default log filter shows. Skips that depend on transient focus or output
state are logged at `debug`; set `RUST_LOG=debug` to see them.
`PlacementSkip::log_level` in `sommelier/src/handler/keyboard.rs` owns this
split.

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

An edge within `1e-9` of `0` or `1` *is* that border: it snaps onto it, so
`x + width` and `y + height` may exceed `1` by at most `1e-9`. This absorbs
decimal-to-binary error (for example `0.1 + 0.9000000000000001`).

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

A placement uses an output that the focused surface has entered, so a window
never jumps to another monitor; only a surface currently on no output (before
its first enter, or after all its outputs went away) uses the first usable
output. `OutputRegistry::for_surface` in
`sommelier/src/state/outputs.rs` owns the exact rule, including which output
wins when the surface spans several and when an output counts as usable.

The work area is the output's logical screen area minus Aura's insets (the
shelf and other reserved margins), in the coordinates that
`zaura_toplevel.set_window_bounds` uses. `OutputState::work_area` in the same
file owns how it is measured from the output's metadata, including why the
`wl_output.geometry` panel transform is used rather than
`zaura_output.logical_transform`. Placement waits until Aura has sent both
its current output scale and its device scale factor; the rounded
`wl_output.scale` cannot describe fractional display scaling.

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
offset(f, W) = clamp(floor(f * W + 1e-6), 0, W)

left   = offset(x, W)          right  = offset(x + width,  W)
top    = offset(y, H)          bottom = offset(y + height, H)
bounds = (X + left, Y + top, right - left, bottom - top)
```

- Both edges are floored from their own fraction, so two rectangles that share
  an edge fraction share the same pixel boundary, and a set of rectangles that
  tiles the unit square tiles the work area without gaps or overlap.
- The `1e-6` pixel bias only corrects floating-point error (see
  [Precision](#precision)); it never moves an edge that was meant to fall
  between pixels.
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
- A reload never changes the gate, the ARC policy, ARC identities, Aura
  objects, or pending bounds barriers, and it never re-reads an environment
  variable.
- With `--window-shortcuts-config`, the `SIGHUP` handler is installed right
  after argument parsing, before the file is first read. If no file is in use
  (the policy is off, or startup could not use the file at all), `SIGHUP` is
  logged and ignored. If the handler cannot be installed, that is logged and
  the proxy runs without reloads. Without the option, `SIGHUP` keeps its
  default action and terminates the proxy.

Signal delivery goes through Tokio's signal driver (an internal self-pipe), and
the reload runs on the proxy's single-threaded event loop between client
accepts, so it never races a key event. The file is read synchronously there;
the 64 KiB bound and the regular-file check keep that read short.

## Code map

| File | Role |
| --- | --- |
| `sommelier/src/placement_config.rs` | `PlacementConfig`: startup resolution of every setting, both binding parsers, the one validator, the hardened file read, the current binding generation, and reload. |
| `sommelier/src/window_layout.rs` | `WindowLayoutAction` (named presets plus a configured rect) and its name parser; `NormalizedRect` validation and the rounding rule that maps it from a work-area `Rect` to window bounds. |
| `sommelier/src/accelerator.rs` | Chord types and parsing only. |
| `sommelier/src/state.rs` | `Context::placement_action_for` / `is_host_accelerator` ask the shared `PlacementConfig`. |
| `sommelier/src/state/window_placement.rs` | Per-connection policy state; it gets the ARC block from `PlacementConfig`. |
| `sommelier/src/state/outputs.rs` | `OutputRegistry`: bound outputs, their committed metadata and liveness, and the output a surface is placed on; `OutputState::work_area`. |
| `sommelier/src/handler/keyboard.rs` | `classify_key` turns a chord into host accelerator, placement, or guest input; a consumed placement chord calls `window_placement::place_focused`. |
| `sommelier/src/handler/window_placement.rs` | Placement mechanics: the focused toplevel and its output, the Aura toplevel and ARC identity, `set_window_bounds` and its barrier, and the configures held behind it. Protocol handlers call one entry point per event (listed in its module docs). |
| `sommelier/src/main.rs` | The `--window-shortcuts-config` option and the `SIGHUP` handler. |
| `sommelier/src/proxy.rs` | Shares the configuration with every connection and runs reloads in the accept loop. |

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
- Failing startup on any configuration error, including a malformed
  `SOMMELIER_ACCELERATORS`, and rejecting empty list elements in it. The
  proxy serves every app in the container, so a configuration error only
  disables what depends on it (see [Resolution at startup](#resolution-at-startup)).
- A per-instance control socket for selecting a different path at runtime.
  `SIGHUP` reloads the startup path; a socket remains a possible follow-up.
