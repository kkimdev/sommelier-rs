# ARC task and restore-session IDs

This note records the ChromeOS application-ID namespaces relevant to the
opt-in window placement experiment (`SOMMELIER_WINDOW_BOUNDS_AS_ARC`). It
describes host behavior observed in Chromium/ChromeOS source; an ARC-looking
app ID is not a supported way to turn a Crostini window into an Android task.

## Why the ID matters

Sommelier normally forwards a Crostini app ID in the form
`org.chromium.guest_os.<vm>.wayland.<app>`. ChromeOS can apply the normal guest
window policy to that namespace, and current Exo builds may reject arbitrary
`zaura_toplevel.set_window_bounds` requests.

The host recognizes `org.chromium.arc.*` as an ARC application namespace. The
ARC policy path is currently what allows the opt-in bounds backend to work.
That classification can also affect restore/ghost bookkeeping, IME routing,
shelf and task observation, pointer lock, drag-and-drop, clipboard,
accessibility, and metrics. It is a compatibility workaround, not a generic
authorization mechanism: a Crostini client can provide its own app ID, so the
ARC prefix must not be treated as proof of trust.

## Namespace meanings

| Application ID | Real ChromeOS meaning | Suitable for placement? |
| --- | --- | --- |
| `org.chromium.arc.<task_id>` | An Android ARC task identity | Semantically the closest form, but a fabricated value is still not a real task |
| `org.chromium.arc.session.<session_id>` | An `app_restore` ARC restore-session/ghost identity | No; not semantically correct for an ordinary live window and never fabricated by Sommelier (see below) |
| `org.chromium.arc.session2.*` | No documented special namespace | Do not invent or depend on it |

The suffix is parsed as a signed decimal integer. UUIDs and names such as
`crostini-gedit` are not valid substitutes. A task ID is per task/window, not a
stable ID for every window of an application. A restore session ID is a
temporary pre-task identity that participates in ghost/restore mapping.

Both recognized forms can enter ARC classification, but their downstream
lifecycle is different. Sommelier does not perform the Android task or app
restore handshake, so neither spelling creates a genuine ARC task/session.

## IDs used by Sommelier

When `SOMMELIER_WINDOW_BOUNDS_AS_ARC` is enabled, each guest `wl_surface`
receives its own task-form identity:

```text
org.chromium.arc.<id>
```

It is sent only as Aura surface policy metadata
(`zaura_surface.set_application_id`). The host XDG role continues to receive
the normal Guest OS namespace, which preserves shelf/application matching:

```text
org.chromium.guest_os.<vm>.wayland.<app>
```

`<id>` comes from the process-wide allocator described in
[Sommelier's task-ID allocator](#sommeliers-task-id-allocator).

`WindowPlacementState` (`sommelier/src/state/window_placement.rs`) owns the
per-surface identity: it is allocated on first use (when an XDG toplevel is
created for the surface, or when GTK metadata or a placement first sends it),
shared by the XDG and GTK metadata paths, queued to the Aura surface at most
once, and retired when the guest destroys the surface.

These values are a compatibility workaround. A fabricated number is not a real
Android task, and it is not a guarantee against future ChromeOS changes or ARC
tracker collisions.

## Rejected `.session.*` experiment

An earlier, unmerged revision generated one
`org.chromium.arc.session.<generated_id>` value per surface using:

```text
base          = 1,000,000,000
pid_component = process_id & 0x3fff
serial        = atomic_process_serial & 0x3fff
generated_id  = base + 1 + pid_component * 16,384 + serial
```

The allocator was removed after direct `/dev/wl0` comparison showed that the
session namespace could restart the custom host compositor even when geometry
shortcuts were disabled. It also entered Chrome's app-restore/ghost path,
where no fabricated collision-free range exists. The experiment and its
limitations remain documented here so a future first-class host capability can
be evaluated against the evidence rather than reintroduced accidentally.

The feature remains opt-in because changing the namespace enables ARC-specific
host behavior beyond bounds placement.

## The real ARC task-ID allocator

`org.chromium.arc.<task_id>` uses an Android task ID, not a host-generated app
name. AOSP reserves `PER_USER_RANGE = 100000` IDs per Android user:

```text
[user_id * 100000, (user_id + 1) * 100000 - 1]
```

For user 0, task IDs normally begin at 1 and increase. Active/recent IDs are
skipped, and allocation wraps within the user's range. Chromium's host parser
generally converts the decimal suffix into an ARC task identity; it does not
create a real Android task for an arbitrary number.

## Sommelier's task-ID allocator

`sommelier/src/arc_task_ids.rs` implements this section; the facts live here
only.

- **Pool.** IDs come from `2,000,000,000..=2,147,483,646`: positive, far
  above the task ranges of realistic Android users (see
  [the real allocator](#the-real-arc-task-id-allocator)), and below `INT_MAX`,
  which the host parser treats as a sentinel. `0` and negative values are never
  produced.
- **Blocks.** The pool is split into 148 blocks of 1,000,000 IDs; the last
  block is partial.
- **Reservation at startup.** When `SOMMELIER_WINDOW_BOUNDS_AS_ARC` is set,
  the proxy reserves one block for the whole process while it resolves its
  placement configuration at startup, before it accepts a client. The search
  starts at a random block and walks every block once.
- **Lock.** A block is reserved by an exclusive, non-blocking `flock` on
  `$XDG_RUNTIME_DIR/sommelier/arc-task-blocks/<first>-<last>.lock`. Missing
  directories are created with mode `0700` and missing lock files with `0600`,
  both further restricted by the umask. The open descriptor holds the lock
  until the process exits, so the kernel releases it even after a crash. A
  block whose lock file is held by another process, is a symlink (skipped, not
  followed), or cannot be opened or locked for any other reason is skipped.
- **Private runtime directory.** `$XDG_RUNTIME_DIR` must be owned by the user
  and have no group or other permissions, as the XDG Base Directory
  specification requires. This is the trust boundary: no other user can
  create, replace, or remove lock files, and processes of the same user are
  trusted. Otherwise no block is reserved.
- **Exclusivity scope.** Two live proxies never hold the same block if they
  share one `$XDG_RUNTIME_DIR`. Proxies in other runtime directories (other
  containers or VMs attached to the same host compositor) coordinate nothing;
  the random block and the random offset only make an overlap unlikely.
  Deleting or replacing the lock directory, or a held lock file, while a proxy
  runs lets a later proxy lock a new file of the same name and reuse that
  block; nothing detects this.
- **Within a block.** IDs are handed out in order from a random offset,
  wrapping inside the block. A process never hands out an ID twice:
  destroying a surface forgets its identity without returning it, and a block
  is never refilled. Once it is exhausted (logged once), new surfaces skip the
  ARC policy until the proxy restarts.
- **Failure.** If `XDG_RUNTIME_DIR` is unset or not private, the lock directory
  cannot be created, `getrandom` fails, or no block can be locked, the ARC
  bounds policy is disabled for the whole process instead of using an
  identity that could collide. The failure is logged once, and the
  reservation is never retried.

## Restore-session threshold

Chrome's app-restore code starts its ARC session counter at `1,000,000,000` and
returns the incremented value, so genuine generated session IDs begin at
`1,000,000,001`. The same boundary is used internally when deciding whether a
session value is a ghost/restore candidate:

```text
session_id >= 1,000,000,000  → ghost/restore candidate
session_id <  1,000,000,000  → ordinary task candidate
```

This is why a low “session pool” such as
`org.chromium.arc.session.999999999` is unsafe: it may parse as an integer but
does not satisfy the restore invariant and can be handled as a task candidate.
Do not allocate from that range.

There is no safe fabricated `.session.*` range. If the host eventually exposes
a first-class guest-window bounds capability, it should replace this ARC
namespace workaround.

## Source references

Chromium ARC parsing and host classification:

- [`chromeos/ash/experiences/arc/arc_util.cc`](https://chromium.googlesource.com/chromium/src/+/main/chromeos/ash/experiences/arc/arc_util.cc)
- [`chrome/browser/ui/ash/shelf/app_service/exo_app_type_resolver.cc`](https://chromium.googlesource.com/chromium/src/+/main/chrome/browser/ui/ash/shelf/app_service/exo_app_type_resolver.cc)

ARC restore/session handling:

- [`components/app_restore/app_restore_utils.h`](https://chromium.googlesource.com/chromium/src/+/main/components/app_restore/app_restore_utils.h)
- [`components/app_restore/app_restore_utils.cc`](https://chromium.googlesource.com/chromium/src/+/main/components/app_restore/app_restore_utils.cc)
- [`components/app_restore/arc_read_handler.cc`](https://chromium.googlesource.com/chromium/src/+/main/components/app_restore/arc_read_handler.cc)
- [`components/app_restore/arc_save_handler.cc`](https://chromium.googlesource.com/chromium/src/+/main/components/app_restore/arc_save_handler.cc)

Android task allocation:

- [`ActivityTaskSupervisor.getNextTaskIdForUser`](https://cs.android.com/android/platform/superproject/+/main:frameworks/base/services/core/java/com/android/server/wm/ActivityTaskSupervisor.java)
- [`UserHandle.PER_USER_RANGE`](https://cs.android.com/android/platform/superproject/+/main:frameworks/base/core/java/android/os/UserHandle.java)
