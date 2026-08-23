# Window shortcut configuration and runtime reload

Status: implemented in the `window-placement-shortcuts-rewrite` worktree

This document defines how compositor-owned keyboard shortcuts are configured.
It deliberately separates shortcut bindings from the window-placement backend:
the bindings say *what key invokes what action*, while the backend decides
whether Sommelier can carry out that action on the host.

## Decisions

- There is no built-in config file and no automatic search under
  `$XDG_CONFIG_HOME`.
- Shortcuts are disabled unless the user explicitly opts in.
- Geometry is written inline on each binding. There are no presets or named
  regions.
- The first action is `window.place`; arbitrary shell commands, raw Wayland
  requests, and raw ARC IDs are not configuration values.
- Backend selection is startup-only. Reloading shortcuts must not change the
  ARC/guest policy or the geometry method of already-connected clients.

## Startup interface

The policy and geometry axes are independent:

```text
--experimental-window-placement
--window-host-policy=guest|arc
--window-geometry-method=none|bounds|self-parent
--window-shortcuts-config PATH       # optional
```

Window placement is disabled unless `--experimental-window-placement` is
present. Passing a placement axis or config path without the gate is a startup
error. With the gate and no hidden axis overrides, the default is equivalent
to:

```text
--window-host-policy=guest
--window-geometry-method=self-parent
```

When `--window-shortcuts-config` is omitted, Sommelier does not open, stat, or
watch a shortcut config file. The process still starts normally, but no
Sommelier-owned window shortcut is active.

When the option is present, Sommelier reads and validates the file before it
starts accepting guest clients. A malformed explicit startup config is a
startup error; it must not silently disable only some bindings.

`self-parent` remains experimental. It can move a window on hosts that support
the custom probe, but `zaura_surface.set_parent` has no width or height
arguments and therefore cannot implement a resize. Explicitly selecting
`--window-geometry-method=none` disables placement after the gate is enabled.

## Config file format

The file is TOML. A binding contains its accelerator, action, and geometry in
one place:

```toml
version = 1

[[bindings]]
chord = "<Alt>q"
action = "window.place"
rect = [0.0, 0.0, 0.5, 0.5]

[[bindings]]
chord = "<Alt>a"
action = "window.place"
rect = [0.0, 0.0, 0.5, 1.0]

[[bindings]]
chord = "<Alt>d"
action = "window.place"
rect = [0.5, 0.0, 0.5, 1.0]
```

`chord` reuses the existing accelerator spelling, such as
`<Alt>q`, `<Ctrl><Alt>F1`, and `<Super>space`. Matching is based on the
negotiated XKB state, not on a hardcoded physical keycode.

`rect` is `[x, y, width, height]` in normalized work-area coordinates:

```text
0.0 <= x, y, width, height <= 1.0
x + width  <= 1.0
y + height <= 1.0
width > 0
height > 0
```

The proxy converts the normalized rectangle to the current output work area
before sending the host placement request. A full work-area fill is
`[0.0, 0.0, 1.0, 1.0]`; this is ordinary placement, not native fullscreen.

The nine-key layout can be represented without names or presets:

| Chord | Rectangle |
| --- | --- |
| `<Alt>q` | `[0.0, 0.0, 0.5, 0.5]` |
| `<Alt>w` | `[0.0, 0.0, 1.0, 0.5]` |
| `<Alt>e` | `[0.5, 0.0, 0.5, 0.5]` |
| `<Alt>a` | `[0.0, 0.0, 0.5, 1.0]` |
| `<Alt>s` | `[0.0, 0.0, 1.0, 1.0]` |
| `<Alt>d` | `[0.5, 0.0, 0.5, 1.0]` |
| `<Alt>z` | `[0.0, 0.5, 0.5, 0.5]` |
| `<Alt>x` | `[0.0, 0.5, 1.0, 0.5]` |
| `<Alt>c` | `[0.5, 0.5, 0.5, 0.5]` |

The parser rejects duplicate chords, unknown actions, unsupported config
versions, malformed accelerator strings, non-finite numbers, and rectangles
outside the work area. The whole file is rejected rather than partially
applied.

For `bounds`, all four rectangle components are applied. For the
experimental `self-parent` method, only the calculated x/y position can be
sent; width and height are ignored and a warning is emitted. This does not
change the config format, so the same file can be tested with either backend.

## Runtime reload

The first implementation uses an explicit reload signal:

```text
SIGHUP  -> read the current contents of the CLI-selected PATH
```

Reload is not a file watcher and does not read a config file on every key
press. The reload sequence is:

1. Read the current file contents.
2. Parse and validate the complete document off the key-event path.
3. Replace the shared immutable binding set only after validation succeeds.
4. Make the new binding generation visible to existing and future clients.

The process and its Wayland clients remain connected. Existing window
placement state, ARC IDs, backend choice, and pending host barriers are not
recreated.

If the file is missing, unreadable, or invalid during reload, Sommelier keeps
the last known-good configuration and logs the error. To disable all
shortcuts, install a valid config with an empty binding list and reload it;
deleting the file is not treated as an implicit disable operation.

If no `--window-shortcuts-config` was supplied, `SIGHUP` is a no-op with a
diagnostic log. It must not cause Sommelier to discover or load a default file.

The recommended update sequence is to write a complete temporary file and
atomically rename it over the selected path, then send `SIGHUP`. This prevents
the proxy from observing an editor's partially-written TOML document.

Shortcut matching uses the active config generation at key press time. A
reload never replays a held key or emits a placement by itself. Key repeat
does not repeat placement. Pending key bookkeeping is reset or generation
tagged so a binding removed during reload cannot leave a stale accelerator
captured.

`SOMMELIER_ACCELERATORS` remains separate. It describes which accelerators the
host should handle; it is not the action/geometry configuration and is not
changed by shortcut reload.
The environment list is parsed once at startup. If it is malformed, Sommelier
exits with status 2 rather than silently treating the list as empty. Changing
the host accelerator environment therefore requires a proxy restart; `SIGHUP`
only reparses the explicit shortcut file.

A shortcut binding must not overlap a parsed `SOMMELIER_ACCELERATORS` entry.
The two settings have deliberately different owners: the shortcut config
requests a Sommelier action, while the environment list reserves a key for a
ChromeOS host accelerator. Silently choosing one would make a global host
shortcut disappear or make a configured binding appear broken.

The conflict behavior is therefore:

| Situation | Result |
| --- | --- |
| Malformed `SOMMELIER_ACCELERATORS` at startup | Startup fails with exit status 2 |
| Startup config overlaps a host accelerator | Startup fails and names every conflicting chord |
| Reloaded config overlaps a host accelerator | Reload fails; last-known-good bindings remain active |
| No overlap | Both policies operate independently |

An explicit host-accelerator override is out of scope for v1. A future version
may add a separate opt-in policy if overriding ChromeOS global shortcuts is
needed.

## Future path selection without restart

`SIGHUP` intentionally has no path payload. If selecting a completely
different path while the process is running becomes a required user
interface, add an explicit per-instance Unix control socket rather than
overloading a signal:

```text
--window-shortcuts-control PATH

sommelierctl --socket PATH load-config /path/to/new.toml
sommelierctl --socket PATH reload
sommelierctl --socket PATH clear
sommelierctl --socket PATH status
```

The control socket would be opt-in, live under `XDG_RUNTIME_DIR`, be created
with mode `0600`, and be namespaced per Sommelier display. `load-config` would
read the file at command time and ask the proxy to apply one complete
validated document. No socket is created when neither a config path nor a
control option is supplied. This is a follow-up interface; it is not required
for the initial `SIGHUP` implementation.

## Non-goals

- Changing the placement backend or ARC policy without restarting.
- Per-window user-selectable backends.
- Native fullscreen or Chrome snap animations for `window.place`.
- Executing arbitrary commands from a TOML file.
- Adding host ChromeOS changes or relying on a privileged host service.

## Implementation checklist

- [x] Use `--window-shortcuts-config` as the explicit config path.
- [x] Use normalized work-area geometry instead of pixel coordinates.
- [x] Keep `self-parent` position-only and experimental.
- [x] Preserve the last known-good bindings after an invalid reload.
- [x] Reject overlap with `SOMMELIER_ACCELERATORS`.
- [x] Defer a control socket until runtime path selection is required.
