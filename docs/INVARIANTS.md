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

- **Every descriptor has one owner.** A descriptor the proxy holds is one
  `OwnedFd` from the receive that installed it to the send that passes a
  copy on, and whoever holds it last closes it by dropping it: the
  connection's queue (`WaylandConnection::read_fds`), then the decoded
  message (`WireMessage::read_fd` takes the next one), then the outgoing
  message (`wire::Outgoing`, `OutgoingMessages`). A message that is
  dropped, suppressed or rejected therefore closes its descriptors, and a
  batch closes the ones it sent once the send returns. Handlers get a
  `BorrowedFd` and duplicate a descriptor to keep it. Only the kernel and
  device boundaries (the `recvmsg` decoder, `virtwl_channel.rs` and the
  allocator's fence export) take over or close a raw descriptor; elsewhere
  a raw number is only lent to a system call while its owner lives.
  `sommelier/clippy.toml` rejects `close`, libc's `dup`, `dup2`, `dup3`,
  `from_raw_fd`, `into_raw_fd` and `borrow_raw` outside those boundaries;
  a test module that builds descriptors by hand allows
  `clippy::disallowed_methods` itself.
  Tests: `early_output_cleanup_preserves_incoming_fd_ownership`,
  `context_drop_closes_queued_and_pending_fds`,
  `drops_unconsumed_received_fds`,
  `test_generate_descriptor_ownership`.
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
- **Received descriptors are bounded.** A connection holds at most
  `MAX_PENDING_FDS` (1024, libwayland's incoming descriptor ring) received
  descriptors that no message has consumed yet; one more ends that
  connection, whose teardown closes them. The bound is per connection and
  the number of clients is not, so it cannot keep the process within its
  descriptor limit; `main` raises the soft `RLIMIT_NOFILE` to the hard
  limit to leave room for many clients at their bound.
  Test: `too_many_unconsumed_fds_end_the_connection`.
- **Coalesced sends are correct.** Translated messages of one ordered
  stream MAY go out in one transport submission, because descriptors belong
  to the start of the ordered stream, not to a byte offset. Do not switch
  to per-message sends just to make ownership look explicit: extra VirtWL
  ioctls add latency and regress input throughput. Introduce
  message-granular sends only for a transport constraint that a regression
  test or device contract demonstrates.
- **A submission carries at most 28 descriptors.** That is the VirtWL
  transaction limit (`VIRTWL_SEND_MAX_ALLOCS`) and libwayland's
  `MAX_FDS_OUT`, and `MAX_FDS_PER_SEND` applies it to both transports.
  `WaylandConnection::send_messages` is the only way to send, and splits a
  batch only between messages, so each message's descriptors travel with
  its bytes and the receiver never meets a message whose descriptors have
  not arrived. No single message carries more than 28.
  Tests: `batches_send_at_most_the_transport_descriptor_limit_at_once`,
  `send_messages_splits_only_between_messages`.
- **VirtWL ioctl errors still own their descriptors.** Every descriptor
  returned in a transaction buffer belongs to the proxy, even when the
  ioctl reports an error after partially writing the buffer. Error paths
  MUST close and de-duplicate all non-negative descriptors before
  returning. The ChromiumOS kernel normally returns none on error; this
  guards against other implementations and malformed devices.
  Tests: `returned_ioctl_fd_is_closed_on_error_paths`,
  `recv_ioctl_error_closes_descriptors_written_before_failure`.

## Wire messages

- **One owner for the header.** Production code reads and writes the
  8-byte header only through `wire::MessageHeader`: the sender ID word, then
  one native-endian word with the length in its upper 16 bits and the
  opcode in its lower 16 bits. `MessageHeader::frame` alone decides whether
  a buffered message is complete: one whose bytes have not all arrived
  waits for the next receive, and an invalid length ends the connection.
  A message is at most `MessageHeader::MAX_LEN` (4096) bytes in both
  directions, libwayland's `WL_MAX_MESSAGE_SIZE`: libwayland refuses a
  longer one from either peer, even though the length field could describe
  up to 65532. A longer message received is an invalid length, and the
  proxy builds none.
  Tests: `run_receives_the_rest_of_a_partially_buffered_message`,
  `buffered_message_readiness_distinguishes_partial_and_complete_frames`,
  `header_length_validity_matches_the_wire_limits`,
  `a_request_over_4096_bytes_is_an_invalid_method`.
- **Messages come from the XML.** Production code builds every message it
  sends itself with a generated encoder, `<interface>::req::<request>` or
  `<interface>::evt::<event>`, which takes the arguments in XML order and
  type, and names protocol values with the generated enum constants.
  `MessageBuilder` is for generated code: `sommelier/clippy.toml` rejects
  `MessageBuilder::new` everywhere else, and its private fields leave no
  other way to make one. A test module that needs frames no encoder makes,
  such as malformed ones, allows `clippy::disallowed_methods` itself.
  Tests: `encoders_match_hand_built_scalar_messages`,
  `encoded_messages_decode_to_the_same_arguments`.
- **Host data is untrusted.** The encoder of a message with a string or an
  array returns a `Result`, because host or guest data can exceed the
  wire's length limit; every other encoder cannot fail. A message the
  proxy builds itself that does not fit is dropped without panicking,
  usually through `wire::queue_or_drop`, and the session goes on. A
  forwarded message that does not fit is a `MessageTooLarge` protocol
  error that ends the session instead of truncating the length.
  An encoder takes descriptors as `OwnedFd`s, so a message that cannot be
  built closes them.
  Tests: `oversized_host_text_is_dropped_without_panicking`,
  `generated_forwarding_rejects_messages_that_exceed_wire_length`,
  `an_encoder_that_cannot_build_its_message_closes_its_descriptors`.
- **A rejected request is reported.** A guest request the proxy cannot
  frame, decode or dispatch ends the session, with its descriptors closed,
  after one `wl_display.error` reaches the guest. The error is always on
  `wl_display` (object 1), as in libwayland-server's
  `wl_client_connection_data`:
  - `invalid_object` when the sender is not a usable object: unknown, or
    destroyed and awaiting `delete_id`. The sender is checked before the
    opcode and arguments (`ProtocolError::InvalidSender`).
  - `invalid_method` for anything else: an invalid length (libwayland
    rejects one under 8 or over 4096; the alignment check is the proxy's
    own), an unknown opcode, a request too new for its object, arguments
    that do not decode, or an object or `new_id` argument that cannot be
    used (even one that names the sender).
  - `invalid_method` for trailing bytes after the arguments too. This one
    is the proxy's own rule: libwayland-server ignores trailing bytes, but
    the proxy re-encodes requests and must not drop data silently.

  Every session that is closed for a protocol reason gets exactly one
  error, like a libwayland client (`wl_resource_post_error_vargs`):
  `queue_protocol_error` queues only the first, so a handler's error is
  not followed by a decoding one, and a host's forwarded error counts as
  the one. A session an internal failure or the host ended without an
  error of its own (a failed dma-buf sync during a commit, say, a host
  error too long to forward or naming an object only the proxy uses, or
  a host event the proxy rejects or whose descriptors are ambiguous) gets
  `implementation` on `wl_display` when its batch ends. The messages
  before the rejected one in the same batch were valid and are forwarded
  first; nothing the rejected message's handler queued is sent, except
  an error of its own.
  Tests: `unknown_client_sender_is_a_protocol_error`,
  `undecodable_request_is_reported_after_forwarding_earlier_requests`,
  `rejected_requests_report_invalid_method_on_the_display`,
  `a_destroyed_sender_is_an_invalid_object_whatever_it_sends`,
  `a_session_gets_exactly_one_error`,
  `an_internal_failure_ends_the_session_with_an_implementation_error`,
  `a_host_error_is_the_sessions_only_error`,
  `an_unforwardable_host_error_leaves_the_implementation_error`,
  `a_host_error_on_a_host_only_object_ends_the_session`,
  `an_invalid_host_event_is_reported_after_the_valid_ones`,
  `descriptor_ambiguity_is_reported_after_the_valid_events`,
  `a_rejected_host_event_sends_nothing_of_its_own`,
  `a_rejected_request_sends_nothing_of_its_own`,
  `an_invalid_message_length_is_reported_before_closing`.

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
  its guest/host pairing until the host sends the matching
  `wl_display.delete_id` (`ShadowTable::guest_destroyed`). Meanwhile the
  guest object rejects new requests and stale host events do not reach its
  handler, but the host ID stays resolvable so the `delete_id` can be
  translated and forwarded. Handlers that synthesize `delete_id` themselves
  (such as `wl_callback.done`) may end their guest object when they queue
  it.
  Test: `destructor_mapping_survives_until_host_delete_id`.
- **An ID is reused only once the peer has forgotten it.** An ID the
  proxy allocated on the host connection becomes reusable only through
  `ShadowTable::release_deleted_host_id`, on the host's `delete_id`: the
  host has then forgotten it, so every mapping of it is dropped and no
  later event can name it. The host may destroy a guest's object on its
  own, as compositors do with the pending frame callbacks of a destroyed
  surface, so its `delete_id` ends the object and reaches the guest. A live
  object only the proxy uses (a binding, an Aura child, an internal
  callback, host-only params) is one the host never destroys on its own: a
  `delete_id` for one, or for an ID the proxy does not track, changes
  nothing, so a misbehaving host cannot hand a live object's ID to a new
  one. A host ID whose object never reached the host returns at once
  (`ShadowTable::abandon`). An ID the host allocated (from `0xff000000`)
  gets no `delete_id`; it stops being reserved as soon as the proxy
  destroys the object. The host creates objects only with server IDs: a
  host `new_id` from the client range, which the proxy allocates from,
  ends the connection like a reused one. A guest ID from the server range
  returns when its guest object ends. Generated dispatchers create a
  `new_id`'s object only when its message's sender is paired, so a message
  dropped for a local sender consumes no ID; one dropped later, for a
  local object in a non-nullable argument, still does (a nullable one is
  sent as null). Freed IDs are reused first, so the ID
  spaces stay as dense as the live objects, and each stays within
  libwayland's map limit: index `WL_MAP_MAX_OBJECTS` (`0xF00000`)
  inclusive, the ID itself on the client side and its offset from
  `0xff000000` on the server side.
  A client that exhausts a range ends; allocation runs in its own task, so
  no other client is affected.
  Tests: `host_ids_are_recycled_through_delete_id`,
  `host_ids_are_not_reused_before_delete_id`,
  `destroyed_host_allocated_object_frees_its_id_at_once`,
  `every_end_of_a_server_created_guest_object_frees_its_id`,
  `cycled_data_offers_keep_their_guest_id_small`,
  `delete_id_for_an_untracked_host_id_changes_nothing`,
  `a_host_destroyed_frame_callback_reaches_the_guest`,
  `a_delete_id_for_a_live_host_only_object_changes_nothing`,
  `an_abandoned_object_frees_both_ids`,
  `host_data_offer_rejects_a_client_range_id`,
  `event_from_an_unmapped_sender_maps_no_new_id`,
  `host_ids_stay_within_libwayland_servers_client_range`.
- **Host-only objects keep their ID reservation.** A host-only proxy object
  with a wire destructor keeps its host ID reserved until the host's
  `delete_id` arrives. Its dispatch metadata may go at once to suppress
  stale events, and the display handler consumes the acknowledgment
  without forwarding an invalid guest ID.
  `Context::retire_destroyed_host_object` is the one way to retire such
  an object after its destructor is queued, and a refused retirement is
  fatal. An object with no wire destructor stays reserved for the
  connection, but `ShadowTable::retire_host` retires its dispatch
  metadata as soon as its guest object or global is gone.
  Tests: `host_only_destructor_reservation_survives_until_delete_id`,
  `retiring_a_destroyed_host_object_requires_a_live_host_only_object`,
  `retiring_a_host_object_awaiting_delete_id_keeps_its_id_reserved`.
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
  objects (such as the text-input managers) stay usable. The one exception
  is the host-only `zaura_output` of a removed `wl_output`: the proxy
  releases it and forgets the output at once, because nothing reads them
  again and a `wl_output` before v3 has no release request that could free
  them later. The guest's `wl_output` itself stays valid.
  Tests: `keyboard_extension_global_remove_keeps_existing_children_alive`,
  `global_remove_keeps_aura_children_until_surface_destroy`,
  `removed_text_input_globals_keep_bound_manager_usable`,
  `global_remove_forgets_the_output_and_releases_its_aura_child`.
- **A bind that crosses its global's removal is not an error.** A client
  can bind a global it saw advertised after the compositor has already
  removed it, because the bind and the `global_remove` cross on the wire;
  libwayland-server makes this harmless. A bind of a `wl_output` name that
  the binding registry advertised and then removed therefore creates an
  inert guest object that never reaches the host and completes its
  destructor locally, like a synthetic object; so does a `zxdg_output_v1`
  created for it. A request that names an inert output reaches the host
  with a null output, as if the guest had named none. An inert output
  never receives `geometry`, `mode` or `done`; that is harmless, because
  the guest receives the global's `global_remove` on the same registry and
  drops the output. A name the registry never
  advertised, a wrong interface or version, and a removed global of any
  other interface (whose requests create objects an inert parent cannot
  serve) stay protocol errors.
  Tests: `bind_crossing_an_output_removal_creates_an_inert_output`,
  `inert_output_release_completes_locally`,
  `bound_output_release_reaches_the_host_without_a_local_delete_id`,
  `set_fullscreen_on_an_inert_output_is_forwarded_without_an_output`,
  `xdg_output_of_an_inert_output_is_inert`,
  `global_remove_forwards_only_visible_globals_and_invalidates_bind`.
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
  into its host storage before the host `wl_surface.commit` is queued. If
  the guest's pool cannot supply them (a short read, as after the client
  truncated the file), the commit queues `wl_shm.invalid_fd` on the
  `wl_buffer` and the session ends, as with libwayland's SIGBUS guard. If
  the host storage fails (mapping, dma-buf sync, EFAULT), the commit is
  dropped and the next one copies in full. A buffer attached and then
  destroyed before the commit keeps its backing until that commit.
  Tests: `commit_from_a_truncated_pool_is_a_protocol_error_not_a_crash`,
  `copy_into_an_unwritable_destination_blames_the_host_storage`,
  `destroyed_pending_shm_attach_survives_until_commit`.
- **Pools are read, never mapped, and must be regular files.** The proxy
  reads pool pixels with `pread`, because a mapping read past a truncated
  file's end raises SIGBUS and kills every guest GUI app. A pool fd MUST be
  a regular file, such as a memfd, tmpfs or `shm_open` file, of at least
  the declared size; any other is rejected with `wl_shm.invalid_fd` at
  `create_pool` and `resize`.
  Test: `pool_backing_must_be_a_long_enough_regular_file`.
- **Damage-only commits copy from a buffer the host holds.** A commit
  without a new `attach` still damages the surface's current buffer, which
  the host samples again, so its damaged pixels MUST be copied before the
  commit is forwarded; gating the copy on a new attach leaves the host
  image stale while its damage is signaled. The one exception is a buffer
  the host has released (`Released`): per `wl_buffer.release` the client
  then owns the storage and may be drawing its next frame into it, so
  reading it could tear, and the host no longer samples it. That commit's
  damage stays stale in the buffer's host copy (see the next rule) and is
  copied at its next attach. Exo releases a buffer only after it is
  detached, so with Exo every damage-only commit copies. A commit with
  nothing to copy (no damage, or damage outside the buffer) skips the copy
  path entirely (no dma-buf sync or BO map), so a damage-free commit, such
  as one acknowledging a configure, is never dropped by a host storage
  failure.
  Known limit: a release carries no serial. If a surface is destroyed while
  buffer A awaits release (`ReleaseOwed`) and A is attached elsewhere before
  the owed release arrives, that release marks A `Released` while the host
  holds it, and damage-only commits skip A's copy until its next attach.
  Tests: `damage_only_commit_copies_the_mutated_current_shm_buffer`,
  `damage_only_commit_does_not_read_a_released_shm_buffer`,
  `commit_without_damage_on_a_held_buffer_skips_the_copy`.
- **Each host copy tracks the pixels it lacks.** Damage is relative to the
  surface's current contents, so a client alternating buffers A, B, A
  damages only what changed since B, while each guest SHM buffer has its
  own host copy. Every commit therefore stales its buffer-space damage in
  the host copies of all the surface's local buffers (`StalePixels`), and a
  buffer copies everything it lacks before it is shown. A host copy that
  cannot be related pixel for pixel copies in full: a buffer that is new,
  new to the surface, of another size than the shown buffer, after a
  failed copy, or after a commit that changed the scale, transform or
  viewport (for every buffer but a held one shown without an attach).
  Destroying a surface drops the relation, because its ID can be reused.
  Tests: `reattached_buffer_copies_the_damage_of_the_frames_it_missed`,
  `reattached_buffer_copies_the_damage_of_every_frame_it_missed`,
  `damage_skipped_on_a_released_buffer_is_copied_at_its_next_attach`,
  `a_frame_of_another_size_stales_the_whole_host_copy`,
  `a_buffer_moving_between_surfaces_copies_everything`,
  `a_failed_copy_stales_the_whole_host_copy`,
  `a_mapping_change_stales_the_other_host_copies`,
  `a_buffer_attached_with_a_new_mapping_copies_everything`,
  `destroying_a_surface_stales_the_host_copies_it_showed`.
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
- **Every v3 commit counts.** A text-input-v3 `done` serial is the number
  of `commit` requests the guest made on the object, including commits made
  while unfocused or whose state could not be encoded; those are still not
  forwarded to the host. The count is a private field of
  `TextInputState`, so only code in `state/input.rs` can change it, and it
  does so only in `TextInputState::count_guest_commit`.
  Tests: `v3_done_serial_counts_commits_made_while_unfocused`,
  `v3_requests_after_leave_are_ignored_until_next_enter`.

## Process

The proxy serves every Linux GUI app, so no single client and no signal may
end it.

- **One client's failure ends only that client.** Setting up an accepted
  client returns errors instead of panicking. A failed accept (such as
  EMFILE) pauses the accept loop for `ACCEPT_RETRY_DELAY` (100 ms) instead
  of spinning on the shared thread, and is logged once until an accept
  succeeds.
  Tests: `unregistrable_descriptor_is_an_error_not_a_panic`,
  `failing_accepts_back_off_instead_of_spinning`.
- **`SIGHUP` never ends the proxy.** `prepare_process` in `main` installs
  the handler first, in every configuration, before the shortcut file is
  first read. Reloads run in their own task, so they never wait for the
  accept loop, and an error is logged if that task ever ends.
  Tests: `sighup_after_process_preparation_reaches_the_reload_handler`,
  `sighup_without_a_shortcuts_file_does_not_end_the_proxy`,
  `reload_does_not_wait_for_the_accept_retry_pause`.
- **The socket belongs to the process holding its lock.** Like libwayland,
  the proxy takes `flock(LOCK_EX | LOCK_NB)` on `<socket>.lock` for its
  lifetime and only then removes a stale socket. If another process holds
  the lock, the proxy exits and leaves that socket alone.
  Test: `a_held_socket_lock_protects_the_live_socket`.

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
