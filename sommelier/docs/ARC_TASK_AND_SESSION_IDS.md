# ARC task and restore-session IDs

This note records the ChromeOS application-ID namespaces used by the window
placement experiment in this worktree. It describes host behavior observed in
Chromium/ChromeOS source; an ARC-looking app ID is not a supported way to turn
a Crostini window into an Android task.

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
| `org.chromium.arc.session.<session_id>` | An `app_restore` ARC restore-session/ghost identity | Not semantically correct for an ordinary live window; used by this experiment only because the host policy recognizes it |
| `org.chromium.arc.session2.*` | No documented special namespace | Do not invent or depend on it |

The suffix is parsed as a signed decimal integer. UUIDs and names such as
`crostini-gedit` are not valid substitutes. A task ID is per task/window, not a
stable ID for every window of an application. A restore session ID is a
temporary pre-task identity that participates in ghost/restore mapping.

Both recognized forms can enter ARC classification, but their downstream
lifecycle is different. The current rewrite uses the session spelling for
per-surface uniqueness; it does not perform the Android restore handshake and
therefore does not create a genuine ARC restore session.

## IDs in the current rewrite

When `--window-host-policy=arc` is selected, the rewrite allocates one stable
ID for each guest `wl_surface`. The fabricated ID is sent only through the
Aura metadata path (`zaura_surface.set_application_id`) and GTK's Aura
metadata path. The host XDG role keeps Sommelier's normal
`org.chromium.guest_os.<vm>.wayland.<app>` identity; this prevents ordinary XDG
shelf, restore, and role bookkeeping from being misclassified as ARC:

```text
org.chromium.arc.session.<generated_id>
```

The split is intentional: `zaura_toplevel.set_window_bounds` is authorized
from the Aura surface's policy metadata on the tested host, while the XDG role
still needs its native guest namespace.

The allocator in `sommelier/src/state/window_placement.rs` currently uses:

```text
base          = 1,000,000,000
pid_component = process_id & 0x3fff
serial        = atomic_process_serial & 0x3fff
generated_id  = base + 1 + pid_component * 16,384 + serial
```

The process-wide serial prevents two `Context` instances in one process from
immediately reusing an ID. This is better than the old fixed
`org.chromium.arc.2147483647` value because separate surfaces no longer share
one fabricated identity.

This allocator is still only best effort:

- The 14-bit PID component can repeat after PID reuse or across process
  restarts.
- The 14-bit serial wraps after 16,384 allocations.
- IDs are not persisted, so a stale host window can outlive the allocator that
  created its ID.
- The numeric range overlaps the range owned by Chrome's real ARC restore
  allocator, so `.session.*` has no collision-free fabricated pool.
- The `+1` offset keeps every generated value strictly above
  `1,000,000,000`, which is the lower boundary used by restore helpers when
  classifying ghost/session IDs.

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

If a future implementation needs a fabricated task-form ID, keep a unique
positive private pool below `INT_MAX` (for example
`2,000,000,000..2,147,483,646`), avoid `0`, negative values, and `INT_MAX`, and
never reuse an ID while its host window may still exist. This is a namespace
convention, not a guarantee against future ChromeOS changes or ARC tracker
collisions.

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
