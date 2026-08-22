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
lifecycle is different. The current rewrite deliberately uses the task
spelling for compatibility; it does not perform the Android task handshake and
therefore does not create a genuine ARC task.

## IDs in the current rewrite

When the public `--window-placement-backend` selects an ARC-backed mode
(`set-parent`, `transient-arc`, or `persistent`), Sommelier reserves one
numeric block before it accepts clients. The lower-level
`--window-host-policy=arc` spelling is retained only as a hidden development
switch. The block is claimed by an exclusive filesystem lock under:

```text
$XDG_RUNTIME_DIR/sommelier/arc-task-blocks/<start>-<end>.lock
```

The block files are deliberately retained after process exit. The kernel
releases the `flock` automatically when the owning process closes its
descriptor; deleting a locked pathname could let another process create a
different inode and accidentally hold the same numeric range concurrently.

The current private best-effort pool is:

```text
2,000,000,000 .. 2,147,483,646
```

Every guest surface receives the next numeric suffix from the process block:

```text
org.chromium.arc.<allocated_task_id>
```

The ID is retained in placement state and is installed on the Aura surface as
the steady-state compatibility identity. The host XDG role remains the native
Guest OS ID. The ordered metadata stream is:

```text
zaura_surface.set_application_id(org.chromium.arc.<allocated_task_id>)
```

Placement then sends only:

```text
zaura_toplevel.set_window_bounds(...)
wl_display.sync(...)
```

Separate Sommelier processes contend on the same block files, so they cannot
select the same block while both are alive. `INT_MAX` is excluded because
`org.chromium.arc.2147483647` was the exact PR #2/custom-host compatibility
sentinel; it is retained only as historical evidence, not as a general
allocation endpoint. A generated task ID is unique within the live Sommelier
block, but it is still not a genuine Android task identity.

The host XDG role keeps Sommelier's normal
`org.chromium.guest_os.<vm>.wayland.<app>` identity. This split prevents
ordinary XDG role/restore bookkeeping from being classified as ARC. Aura shelf
or icon classification may still be generic because the compatibility task ID
is visible there; this is the current trade-off for stable placement and IME
behavior. The allocator is still only a convention: ChromeOS does not provide
a query through this Wayland path, so Sommelier cannot prove that a fabricated
number is absent from Android's own task table.

The named `--window-placement-backend=set-parent` mode uses this same
persistent task-form Aura identity and the experimental
`zaura_surface.set_parent` probe. It first sends
`zaura_toplevel.set_window_bounds(current_x, current_y, width, height, output)`
while the window is still top-level, then sends `set_parent` for the target
position. This order matters because `set_parent` supplies no size and a
later bounds request may be rejected. After the ordered sync barrier completes,
Sommelier sends `set_parent(NULL, 0, 0)` to release the self-parent probe. The
persistent ARC identity is still required for the bounds request to change
width and height. The host XDG identity remains native, and no
`org.chromium.arc.session.*` value is generated.

The feature remains opt-in because the task-form namespace enables
ARC-specific host behavior beyond bounds placement. A process that loses its
host windows without a corresponding compositor teardown could make a newly
reused block overlap stale metadata; the block scheme therefore assumes normal
Wayland connection teardown.

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

The allocator research was more specific than “pick a random large integer”:

1. `getNextTaskIdForUser(user_id)` starts in that user's
   `[user_id * PER_USER_RANGE, (user_id + 1) * PER_USER_RANGE)` interval
   (user 0 normally starts at 1), advances through the interval, skips task
   IDs that are still active/recent, and wraps within the interval. The value
   is therefore an Android task identity allocated by the Android task
   supervisor, not a globally unique application name.
2. Low values in the user-0 interval (`1..99,999`) are the most semantically
   genuine but also have the highest chance of colliding with a real ARC task.
   Values below `1,000,000,000` are still task candidates; being below the
   restore threshold does not make them a private pool.
3. A high positive pool such as `2,000,000,000..2,147,483,646` was considered
   as a best-effort fabricated task pool because it is far from normal AOSP
   per-user allocation and leaves headroom below signed `INT_MAX`. It is not
   guaranteed safe: ChromeOS can change its tracker, another producer can use
   the range, and the host does not ask Android whether the number belongs to
   the guest.
4. `0`, negative values, and values that overflow the host's signed 32-bit
   parser are invalid candidates. `INT_MAX` is also not a generally safe
   allocator endpoint. The exact `org.chromium.arc.2147483647` value is kept
   only because PR #2 and the custom host proved that compatibility sentinel
   stable; that observation must not be generalized into an ARC allocation
   rule.

The current block allocator follows the minimum safe convention available
without a host capability: it reserves a positive private pool, never reuses
an ID within a live process block, and coordinates all local Sommelier
instances through `flock`. A real host capability remains preferable because
only ARC/Android can establish global task ownership.

## Rejected experiment: fabricated ARC session IDs

The rewrite briefly used one `org.chromium.arc.session.<id>` value per guest
surface. The historical allocator was:

```text
base          = 1,000,000,000
pid_component = process_id & 0x3fff
serial        = process_wide_atomic_serial & 0x3fff
generated_id  = base + 1 + pid_component * 16,384 + serial
```

The process-wide serial avoided immediate reuse between simultaneous
`Context` instances, while the live ID-to-surface map remained connection-owned
and was retired with the surface. This gave local uniqueness, not global
ownership:

- the 14-bit PID component repeats after PID reuse, across process restarts, or
  across independent hosts;
- the 14-bit serial wraps after 16,384 allocations;
- IDs are not persisted, so a stale host window can outlive the allocator that
  created its ID;
- the range overlaps Chrome's real ARC restore/session allocator, so there is
  no collision-free fabricated `.session.*` pool;
- a low `.session.*` value is worse, because values below
  `1,000,000,000` can be classified as ordinary task candidates rather than
  restore/ghost candidates.

On the meaningful `/dev/wl0` runtime, `--window-host-policy=arc
--window-geometry-method=none` was enough to make the host UI/compositor
restart, even though no shortcut or bounds request was enabled. No core dump
was observed. This was the strongest available evidence that the
`.session.*` namespace itself (or its restore metadata path), not
`set_window_bounds`, was destabilizing the custom host. The experiment was
therefore rolled back to the PR #2 task-form compatibility ID.

This section is retained as a rejected experiment so that a future contributor
does not reintroduce the allocator merely because it appears more unique.

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
