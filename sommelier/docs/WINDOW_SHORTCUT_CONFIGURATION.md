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
- The supported public startup surface has one backend switch and one explicit
  config-path switch. Lower-level policy/geometry/lifetime axes are hidden
  compatibility options reserved for development experiments.

## Startup interface

The supported interface is:

```text
--experimental-window-placement
--window-placement-backend=set-parent|transient-arc|persistent|remote-shell-v2
--window-shortcuts-config PATH       # optional
```

Window placement is disabled unless `--experimental-window-placement` is
present. The gate is required even when selecting no backend; it prevents the
current experimental protocol paths from changing the production default.
After the gate is enabled, the default backend is the native Guest OS identity
plus the experimental self-parent geometry path. The shortcut feature itself
remains inactive until an explicit config file is supplied:

```text
native Guest OS identity + XDG resize/self-parent placement (experimental gate)
no shortcut file read
```

Passing a backend, a hidden compatibility axis, or a config path without the
gate is a startup error. This is intentional: a malformed or unstable
placement implementation must not be reachable through an accidental service
configuration.

`--window-host-policy`, `--window-geometry-method`, and
`--window-arc-id-lifetime` still exist as hidden compatibility switches for
focused host experiments. They are not required for normal use and should not
be combined with `--window-placement-backend`.

When `--window-shortcuts-config` is omitted, Sommelier does not open, stat, or
watch a shortcut config file. The process still starts normally, but no
Sommelier-owned window shortcut is active.

When the option is present, Sommelier reads and validates the file before it
starts accepting guest clients. A malformed explicit startup config is a
startup error; it must not silently disable only some bindings.

`self-parent` remains experimental and is the geometry path of the public
`set-parent` backend. It keeps the native Guest OS application identity so
ChromeOS can continue matching the guest window to its shelf/taskbar metadata.
It first performs a normal XDG resize handshake: Sommelier queues
`xdg_surface.set_window_geometry(0, 0, width, height)` on the host and a
synthetic guest `xdg_toplevel.configure`/`xdg_surface.configure`. The guest's
acknowledgement and commit apply the new size at the current origin. Only after
the matching host Aura configure does Sommelier issue the custom
`zaura_surface.set_parent(self, relative_x, relative_y)` position probe. The
split is required because `set_parent` has no width or height arguments, while
the native Guest OS policy may reject direct Aura bounds. The native Guest
identity is deliberate: the ARC task-form ID can authorize bounds on some
hosts but may prevent the guest `.desktop` entry from producing the expected
shelf/taskbar icon. If the host rejects the XDG resize handshake, the backend
remains position-only; it never falls back to a direct ARC/Aura bounds request.
Internally, the public `set-parent` backend is equivalent to:

```text
--window-host-policy=guest --window-geometry-method=self-parent
```

The self-parent request itself is emitted only for a shortcut. Its sync barrier
is completed before Sommelier sends the protocol's nullable-parent form
(`set_parent(NULL, 0, 0)`), so the custom same-surface cycle is not
intentionally kept as a parent relationship. A superseded or released
toplevel does not receive stale cleanup from an older barrier. Identical
rectangles are consumed without another wire request while the resize,
self-parent, or delayed host-origin phase is still converging; this prevents
the small `z -> a -> a` drift seen in the earlier implementation.

The comparison backend `--window-placement-backend=transient-arc` uses a
different cleanup sequence:

```text
set_application_id(ARC task ID)
set_window_bounds(...)
wl_display.sync
sync.done -> set_application_id(native Guest OS ID)
identity sync.done -> refresh host text-input generation
```

The bounds path does not emit a speculative `set_parent(NULL)`: the window was
never parented, and doing so creates another host focus/IME transition.
Sommelier resolves the latest native ID from the surface state when
the barrier completes, so an app-ID update that arrives while the bounds
request is in flight is not overwritten by an old snapshot. This is an
experimental host-compatibility probe: ChromeOS may
recompute the window's placement or reset IME focus when either the parent or
the Aura application ID changes. The backend therefore requires
`zaura_surface` version 5 or newer: v2 provides nullable `set_parent`, while
v5 provides `set_application_id`, which is also needed to install the
temporary ARC identity. It is not currently considered IME-safe on the custom
host: ChromeOS' ARC property resolver adds `kSkipImeProcessing` and restore
properties but does not remove them when the native Guest OS ID is restored.
The runtime diagnostic path logs this sequence with a `placement#N` correlation
ID. Keep this backend available for comparison, but do not treat the native-ID
restore as proof that host ARC state was cleared.

Transient placement also arms a per-`wl_keyboard` focus guard for the two
identity transitions above. Custom Exo builds can deliver each corresponding
`wl_keyboard.leave` several seconds late; forwarding that stale leave would
tear down the guest text-input generation even though focus never moved to
another guest surface. A same-surface `enter` completes one guarded cycle,
while an enter for a different surface releases the guard and projects the
normal guest leave/enter transition. This protects Korean IME focus from the
known delayed-event ordering, but it does not remove ChromeOS'
sticky `kSkipImeProcessing` ARC property; hosts with that resolver behavior
still require the native Guest identity path or a host-side fix for full IME
semantics.

If the guest surface is destroyed before `sync.done`, the cleanup path drops
both post-barrier requests instead of sending them to the released Aura object;
the host ID remains reserved for its normal `delete_id` lifecycle.

The opt-in `--window-placement-backend=remote-shell-v2` backend uses the
official ChromeOS `zcr_remote_shell_v2` protocol instead of Aura bounds or ARC
application IDs:

```text
host zcr_remote_shell_v2.get_remote_surface(wl_surface, container=default)
guest XDG role -> local configure facade
window.place -> zcr_remote_surface_v2.set_bounds_in_output(...)
             -> wl_surface.commit
host bounds event -> synthetic guest XDG configure
```

The remote-shell global is deliberately kept host-only; it is never advertised
to guest applications. XDG `set_app_id` and `set_title` are translated to the
remote surface, and host close/bounds events are translated back to the guest
XDG role. This path does not use `zaura_toplevel.set_window_bounds`, ARC task
IDs, or the self-parent probe. The host must advertise the global and permit the
connection through its remote-shell security policy. If the global is absent,
the opt-in backend logs the missing capability and rejects the affected client;
it does not silently switch to the transient-ARC or self-parent path. This
backend remains experimental until a custom host with the global enabled has
been tested for resize, shelf icon, IME, and teardown behavior.

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
experimental `self-parent` method, the current known x/y plus the requested
width/height are sent by `set_window_bounds` first; the calculated target x/y
is then sent by `set_parent`. This does not change the config format, so the
same file can be tested with either backend, provided the ARC policy is
enabled when resize is required.

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

A shortcut binding must not overlap a parsed `SOMMELIER_ACCELERATORS` entry.
The two settings have deliberately different owners: the shortcut config
requests a Sommelier action, while the environment list reserves a key for a
ChromeOS host accelerator. Silently choosing one would make a global host
shortcut disappear or make a configured binding appear broken.

The conflict behavior is therefore:

| Situation | Result |
| --- | --- |
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
- [x] Keep `self-parent` experimental while pairing its position probe with a
  bounds request for resize.
- [x] Preserve the last known-good bindings after an invalid reload.
- [x] Reject overlap with `SOMMELIER_ACCELERATORS`.
- [x] Defer a control socket until runtime path selection is required.
