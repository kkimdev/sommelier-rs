# Runtime invariants

These rules are binding. Each exists because breaking it caused, or would
cause, a real failure: a frozen or disconnected client, a leaked descriptor,
stale pixels, or lost IME input. A change that touches one keeps it, or
changes this document and the covering tests in the same pull request.

The named tests pin each rule; search for them with
`git grep -n <test_name>`. [DESIGN.md](DESIGN.md) explains the structure
the rules apply to. The window placement contract (a matched chord is
always consumed; a configuration problem never stops the proxy) is owned by
[WINDOW_SHORTCUT_CONFIGURATION.md](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).

## Transport and file descriptors

- **Descriptors follow the ordered stream, not byte-buffer boundaries.**
  One `sendmsg` or VirtWL transaction may carry several messages, and a
  descriptor may belong to a later message whose bytes have not arrived. A
  pending descriptor after a complete fd-free message therefore MUST NOT end
  the connection by itself. Descriptors attached to a complete message that
  takes none are a protocol-fatal ambiguity and MUST be closed during
  connection teardown.
  Tests: `untracked_message_fd_is_retained_when_followup_is_partial`,
  `untracked_message_fd_ambiguity_survives_separate_receive_calls`.
- **Truncated ancillary data is an ownership failure.** If `recvmsg`
  reports `MSG_CTRUNC`, every descriptor the kernel installed for that
  receive MUST be recovered and closed before the error is returned. A
  wrapper that refuses to iterate truncated control data MUST NOT be the
  only cleanup path. The receive buffer is sized for the protocol's maximum
  descriptor batch.
  Tests: `recvmsg_scm_rights_truncation_closes_kernel_installed_descriptors`,
  `truncated_scm_rights_closes_descriptors_recovered_from_control_buffer`.
- **Coalesced sends are correct.** Translated messages of one ordered
  stream MAY go out in one transport submission, because descriptors belong
  to the start of the ordered stream, not to a byte offset. Do not switch
  to per-message sends just to make ownership look explicit: extra VirtWL
  ioctls add latency and regress input throughput. Introduce
  message-granular sends only for a transport constraint that a regression
  test or device contract demonstrates.
- **VirtWL ioctl errors still own their descriptors.** Every descriptor
  returned in a transaction buffer belongs to the proxy, even when the
  ioctl reports an error after partially writing the buffer. Error paths
  MUST close and de-duplicate all non-negative descriptors before
  returning. The ChromiumOS kernel normally returns none on error; this
  guards against other implementations and malformed devices.
  Tests: `returned_ioctl_fd_is_closed_on_error_paths`,
  `recv_ioctl_error_closes_descriptors_written_before_failure`.

## Wire messages

- **Host data is untrusted.** A message that carries host-provided strings
  or arrays MUST be built with the fallible `try_build_message` and handle
  `MessageTooLarge` without panicking; generated forwarding already does.
  A message of only integers, object IDs and descriptors cannot exceed the
  wire limit and uses `build_message`.
- **Unknown senders are protocol errors.** A guest request from an object
  the proxy does not know ends the session with a protocol error, with its
  descriptors closed.
  Test: `unknown_client_sender_is_a_protocol_error`.

## Protocol errors

A request that violates a protocol error rule MUST queue the specified
`wl_display.error` on the offending object and end the connection after the
diagnostic is flushed. Silently returning `Action::Drop` leaves the client
believing its state was accepted and desynchronizes later requests. This
covers invalid `wl_surface.attach` offsets, non-positive buffer scales,
out-of-range transforms, malformed `wp_viewport` rectangles, invalid
`wl_registry.bind` requests, and the SHM and linux-dmabuf validation rules
(pool sizes and descriptors, buffer formats and layouts, duplicate,
out-of-range or missing planes, invalid dimensions, out-of-bounds
metadata), each with its interface's declared error code. Diagnostics are
bounded so the error event itself fits in a message. Tests assert the
object ID, the error code and that the session ends.
Tests: `invalid_shm_requests_queue_protocol_errors`,
`invalid_plane_requests_queue_declared_protocol_errors`,
`invalid_viewport_values_and_dead_surface_are_fatal`,
`invalid_bind_queues_a_display_protocol_error`,
`fatal_protocol_error_keeps_a_bounded_diagnostic`.

## Object lifetimes

- **Destructors wait for `delete_id`.** A generated destructor request keeps
  its guest/host mapping until the host sends the matching
  `wl_display.delete_id`. Meanwhile the guest object rejects new requests
  and stale host events do not reach its handler, but the host ID stays
  resolvable so the `delete_id` can be translated and forwarded. Handlers
  that synthesize `delete_id` themselves (such as `wl_callback.done`) may
  retire their mapping when they queue it.
  Test: `destructor_mapping_survives_until_host_delete_id`.
- **Host-only objects keep their ID reservation.** A host-only proxy object
  with a wire destructor keeps its host ID reserved until the host's
  `delete_id` arrives. Its dispatch metadata may go at once to suppress
  stale events, but allocation skips the pending ID, and the display
  handler consumes the acknowledgment without forwarding an invalid guest
  ID. `Context::retire_destroyed_host_object` is the one way to retire such
  an object after its destructor is queued, and a refused retirement is
  fatal. An object with no wire destructor stays reserved for the
  connection, but `ShadowTable::retire_host_interface` retires its dispatch
  metadata as soon as its guest object or global is gone.
  Tests: `host_only_destructor_reservation_survives_until_delete_id`,
  `retiring_a_destroyed_host_object_requires_a_live_host_only_object`.
- **Synthetic objects complete their own lifecycle.** A guest object
  implemented entirely in the proxy (such as `wl_shm_pool` or
  `zwp_text_input_manager_v3`) MUST, on its destructor, retire its dispatch
  metadata and emit exactly one guest `wl_display.delete_id`. Its ID is not
  reused before that event is queued, and later requests on it are
  rejected. Forwarding the destroy to a nonexistent host object, or
  dropping it, leaks the guest object. A synthetic global without a
  destructor (`wl_shm`) stays reserved for the connection. A synthetic
  global that has a local destructor MUST remove its capability and
  event-routing state before its `delete_id` is queued.
- **A removed backing global leaves synthetic objects destroyable but
  inert.** When the host global behind a synthetic object is removed, the
  object MUST still accept its destructor and MUST ignore every other
  request. Children created before the removal (such as a `wl_shm_pool`)
  still complete their own lifecycle and emit exactly one `delete_id`.
  Tests: `removed_host_shm_rejects_pool_creation_from_existing_synthetic_object`,
  `removed_host_shm_ignores_requests_from_existing_pool_until_destroy`.
- **Releasing a parent does not end its children.** `wl_seat.release`
  forwards the seat's destructor without tearing down keyboards, key
  routing or IME state that child devices still own.
  Tests: `seat_release_does_not_break_live_child_keyboard_press_release_pair`,
  `release_keeps_child_keyboard_routing_state`,
  `release_does_not_end_child_text_input_focus`.

## Registry globals

- **`global_remove` invalidates only the advertised name.** It MUST NOT
  retire already-bound child objects that have their own lifetime, such as
  `zcr_extended_keyboard_v1` children of a `zcr_keyboard_extension_v1`
  manager or `zaura_surface` children of a `zaura_shell`. They stay mapped
  and routable until their own destructor or the connection closes; a
  replacement manager global may coexist with them. Already-bound manager
  objects (such as the text-input managers) stay usable.
  Tests: `keyboard_extension_global_remove_keeps_existing_children_alive`,
  `global_remove_keeps_aura_children_until_surface_destroy`,
  `removed_text_input_globals_keep_bound_manager_usable`.
- **A re-advertised name replaces its old generation.** When the host
  re-advertises a global under a previously removed numeric name, the
  connection-wide metadata for that name MUST be replaced before the new
  interface is processed, so stale entries cannot route a bind to the old
  interface or misclassify the replacement's `global_remove`. Live children
  of the old generation are not invalidated.
  Tests: `re_advertised_name_replaces_visible_metadata_before_hidden_global`,
  `re_advertised_hidden_name_replaces_hidden_metadata_before_visible_global`,
  `stale_remove_cannot_invalidate_a_re_advertised_global_generation`.
- **Versions come from the XML or a documented constant.** Since-version
  checks use the generated `*_SINCE` constants, never a copied literal. A
  global's version bound is the generated `VERSION` unless a named
  constant in `registry.rs` documents a deliberate difference and its
  reason (such as the ChromiumOS caps `wl_seat` v5 and `wl_output` v3, or
  the `zaura_shell` v6 floor). A child object takes its parent's version,
  and a request is sent to a host object only if that object's version
  has it: an Aura surface keeps the version of the shell generation that
  created it.
  Tests: `child_objects_inherit_their_manager_versions`,
  `unset_snap_is_sent_only_to_an_aura_surface_that_has_it`.

## Surfaces, damage and SHM

- **Copy before commit.** Damaged pixels are copied from the guest buffer
  into its host storage before the host `wl_surface.commit` is queued. A
  failed copy drops that commit and forces a full copy on the next one.
  A buffer attached and then destroyed before the commit keeps its backing
  until that commit.
  Test: `destroyed_pending_shm_attach_survives_until_commit`.
- **Damage-only commits (open).** The rule this project has followed says
  a damage-only commit (no new `attach`) MUST also copy the damaged pixels
  of the currently committed SHM buffer, since gating the copy on a new
  attach leaves the host image stale while its damage is signaled. The
  current code copies only on a new attach: commit `227206b` deliberately
  renamed `damage_only_commit_copies_mutated_committed_shm_buffer` to
  `damage_only_commit_does_not_recopy_mutated_released_shm_buffer`, which
  asserts that behavior. The two disagree, and a separate pull request is
  resolving it; until it lands, do not change either without resolving it
  here.
- **`attach` offsets at version 5.** For a `wl_surface` at version 5 or
  newer, `attach(buffer, 0, 0)` MUST NOT create a pending legacy offset, so
  a committed `wl_surface.offset` survives a later zero-coordinate attach.
  Non-zero coordinates are a protocol error. Objects with an unknown
  negotiated version keep the permissive pre-v5 behavior.
  Tests: `v5_zero_attach_does_not_create_an_attach_offset`,
  `v5_attach_rejects_nonzero_offsets_but_legacy_attach_tracks_them`.
- **A non-zero offset forces full damage.** Surface-local damage cannot yet
  be translated through the SHM copy map when the committed offset is
  non-zero, so such a commit MUST send complete host surface damage, even
  when the guest sent a partial rectangle.
  Test: `nonzero_surface_offset_with_explicit_damage_forces_full_host_damage`.
- **Damage is clamped** to ChromiumOS' `INT_MIN / 10` to `INT_MAX / 10`
  headroom before it is encoded for the host, and 90/270-degree transforms
  swap full-damage extents for non-square buffers.
  Tests: `extreme_surface_damage_is_clamped_before_host_encoding`,
  `rotated_non_square_buffer_full_damage_swaps_extent`.

## Dma-buf

- **Virtio-gpu PRIME imports use kernel metadata.** Before forwarding
  plane 0 of a classic virtio-gpu PRIME buffer, the proxy MUST query the
  local DRM resource metadata. When it identifies a virtio-gpu buffer, the
  kernel-reported stride and explicit modifier replace the guest's stride
  and implicit modifier; for non-virtio or unsupported kernels the guest
  values are kept.
  Tests: `virtgpu_resource_metadata_replaces_guest_plane0_layout`,
  `virtgpu_layout_fixup_preserves_guest_metadata_on_fallback`.
- **Resource-info probing keeps two ABI sizes.** The deliberately invalid
  capability probe uses the standard 16-byte `drm_virtgpu_resource_info`
  ioctl encoding; real stride and modifier queries use the 56-byte ChromeOS
  `drm_virtgpu_resource_info_cros` encoding. The command number is shared
  but the size bits are not, so one constant derived from the extended
  struct makes a supported kernel look unsupported. The probe runs only
  after `VIRTGPU_PARAM_3D_FEATURES` succeeds.
  Tests: `virtgpu_resource_info_probe_and_extended_abi_sizes_remain_distinct`,
  `extended_resource_info_probe_requires_virtgpu_feature_query`.
- **Native buffers are fenced.** A commit of a guest dma-buf is forwarded
  only after the guest GPU's writes have completed; a failed wait is fatal
  and never commits.
  Test: `unexpected_native_sync_failure_is_fatal_and_never_commits`.

## Keyboard and IME

- **Keymaps are mapped privately.** `wl_keyboard.keymap` descriptors MUST be
  mapped read-only with `MAP_PRIVATE`, as the protocol requires, on both
  the VirtWL and plain memfd paths. Do not switch to `MAP_SHARED` because
  one host happens to allow it.
  Test: `keymap_mmap_is_private_not_shared`.
- **IME state is scoped to its keyboard or seat.** Pressed-key sets, repeat
  flags and focus markers used to bridge ChromeOS IME events belong to the
  originating keyboard or seat, never to the process. Synthetic
  `wl_keyboard` events use the compositor's monotonic millisecond time
  domain of the real keyboard events, never wall-clock time. Host keysyms
  go to the keyboard whose surface has the seat's current focus.
  Tests: `ime_repeat_state_is_scoped_to_the_originating_keyboard`,
  `synthetic_ime_key_uses_the_host_compositor_time_domain`,
  `keysym_prefers_keyboard_with_current_surface_focus`.
- **Content hints come from the v3 XML.** Text-input-v3 `content_hint` bits
  are read from the vendored `text-input-unstable-v3.xml`, not the
  similarly named v1 enum. v3 bit `0x2` is `spellcheck` and maps to
  Chrome's `spellcheck_on` (`1 << 4`); Chrome's `autocorrect_on` (`1 << 2`)
  is a separate flag and is never inferred from it.
  Test: `content_type_mapping_preserves_pin_privacy_and_inline_composition`.
- **Focus teardown sends `leave` first.** When a focused text-input surface
  is destroyed or loses focus, the proxy MUST send the guest
  `zwp_text_input_v3.leave` before clearing the active-surface state. A
  delayed host keyboard leave cannot be relied on, since it may be rejected
  as stale once the surface mapping is gone. Teardown is explicit and
  idempotent: the later delayed leave does not duplicate the event.
  Test: `wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave`.

## ChromeOS shelf application IDs

For the shelf to match a guest window with its `.desktop` file (icon and
pinning), the application ID the host receives MUST be
`org.chromium.guest_os.<vm-id>.wayland.<app-id>` for native Wayland
clients:

- `<vm-id>` is `SOMMELIER_VM_IDENTIFIER`, or `termina` (the standard
  Crostini VM name) when it is unset or empty. `main` reads it once.
- Every `xdg_toplevel.set_app_id` (and GTK application ID) retrieves or
  creates the surface's host `zaura_surface` and sends the formatted ID
  through `zaura_surface.set_application_id`.
- The one exception is the opt-in ARC placement policy, which sends an ARC
  identity instead; see
  [ARC task and session IDs](../sommelier/docs/ARC_TASK_AND_SESSION_IDS.md).

Tests: `set_app_id_creates_zaura_surface_and_sets_app_id`,
`dbus_application_id_is_namespaced_on_aura_surface`,
`set_app_id_uses_arc_namespace_when_bounds_policy_is_enabled`.

## Manual verification

- **Test the binary you built.** A rebuilt binary does not replace a proxy
  that is already running. Before accepting a runtime result, compare the
  binary's modification time with the test proxy's start time, and restart
  the isolated test proxy and its client if the binary is newer. Never
  restart the primary Sommelier service or disturb unrelated clients for an
  isolated test.
- **Check glyphs, not just events.** For Korean input checks, the sample
  GUI (`sommelier_test_app`) MUST register a font with Korean/CJK coverage,
  and verification MUST confirm the committed text renders as glyphs, not
  tofu boxes; receiving preedit or commit events is not enough. Font
  loading fails clearly or falls back to a documented system font
  (`SOMMELIER_TEST_GUI_FONT` overrides it).
