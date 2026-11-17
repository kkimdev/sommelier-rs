# Runtime invariants

These rules are binding. Each exists because breaking it caused, or would
cause, a real failure: a frozen or disconnected client, a leaked descriptor,
stale pixels, or lost IME input. A change that touches one keeps it, or
changes this document and the covering tests in the same pull request.

The named tests pin each rule; search for them with
`git grep -n <test_name>`, and `scripts/check-cited-tests.py` checks that
each still exists. [DESIGN.md](DESIGN.md) explains the structure
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
  not arrived. No Wayland message has more than 28 descriptor arguments;
  `send_messages` refuses one that carries more, in every build, and the
  session ends without an error, like a failed send (see the transport
  exception under [Wire messages](#wire-messages)).
  Tests: `batches_send_at_most_the_transport_descriptor_limit_at_once`,
  `send_messages_splits_only_between_messages`,
  `a_message_with_more_descriptors_than_a_submission_is_not_sent`.
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

  Every session the proxy ends gets exactly one error, like a libwayland
  client (`wl_resource_post_error_vargs`), written in one place after its
  batch (see [Message effects](DESIGN.md#message-effects)); once a
  session has ended, the proxy handles nothing more from it. The first
  protocol error wins, so a handler's error is not followed by a decoding
  one, and a host's forwarded error counts as the one, its diagnostic
  bounded like the proxy's own. A session an internal failure or the host
  ended without an error the guest can be given (a failed dma-buf sync
  during a commit, say, a host error naming an object only the proxy
  uses, or a host event the proxy rejects or whose descriptors are
  ambiguous) gets `implementation` on `wl_display`. The messages before
  the one that ended the session in the same batch were valid and are
  sent first. Nothing the ending message produced is sent: not its
  forward, and nothing its handler queued in either direction, before or
  after it ended the session. The exception is a session that ends
  because it lost a transport (a send or receive fails, or either peer
  closes its connection): the proxy closes the guest's connection without
  an error.
  Tests: `unknown_client_sender_is_a_protocol_error`,
  `undecodable_request_is_reported_after_forwarding_earlier_requests`,
  `rejected_requests_report_invalid_method_on_the_display`,
  `a_destroyed_sender_is_an_invalid_object_whatever_it_sends`,
  `a_session_gets_exactly_one_error`,
  `an_internal_failure_ends_the_session_with_an_implementation_error`,
  `a_host_error_is_the_sessions_only_error`,
  `a_host_error_is_forwarded_with_a_bounded_diagnostic`,
  `a_host_error_on_a_host_only_object_ends_the_session`,
  `an_invalid_host_event_is_reported_after_the_valid_ones`,
  `descriptor_ambiguity_is_reported_after_the_valid_events`,
  `a_rejected_host_event_sends_nothing_of_its_own`,
  `a_rejected_request_sends_nothing_of_its_own`,
  `a_failed_handler_forwards_nothing`,
  `a_handler_error_discards_its_earlier_output`,
  `a_failed_message_sends_no_reverse_output`,
  `earlier_reverse_output_reaches_the_guest_before_the_error`,
  `a_forward_that_ends_the_session_discards_its_deferred_work`,
  `an_invalid_message_length_is_reported_before_closing`.

## Protocol errors

A request that violates a protocol error rule MUST be rejected with the
specified `wl_display.error` on the offending object (`ctx.reject`), which
ends the connection after the diagnostic is flushed. Silently returning
`Action::Drop` leaves the client believing its state was accepted and
desynchronizes later requests. This
covers invalid `wl_surface.attach` offsets, non-positive buffer scales,
out-of-range transforms, malformed `wp_viewport` rectangles, invalid
`wl_registry.bind` requests, and the SHM and linux-dmabuf validation rules
(pool sizes and descriptors, buffer formats and layouts, duplicate,
out-of-range or missing planes, invalid dimensions, out-of-bounds
metadata), each with its interface's declared error code. Diagnostics are
bounded so the error event itself fits in a message. Tests assert the
object ID, the error code and that the session ends.
Tests: `invalid_shm_requests_are_protocol_errors`,
`invalid_plane_requests_queue_declared_protocol_errors`,
`invalid_viewport_values_and_dead_surface_are_fatal`,
`invalid_bind_queues_a_display_protocol_error`,
`fatal_protocol_error_keeps_a_bounded_diagnostic`.

Deliberately left unenforced, matching Exo: `wl_surface.invalid_size`, a
buffer that is not a multiple of its scale; `wp_viewport.bad_size`, a
fractional source size without a destination, which Exo takes as a
fractional surface size; and `wp_viewport.out_of_buffer`, a viewport
source outside a non-NULL buffer. Exo accepts all three, so enforcing any
would end the session of Crostini apps that work today, with nothing on
screen to explain why. The proxy forwards such a commit unchanged and logs
the first viewport violation on each surface, with the source, buffer
size, scale and transform.
Tests: `viewport_source_must_fit_the_transformed_scaled_buffer`,
`viewport_source_outside_the_buffer_is_logged_and_forwarded`,
`fractional_source_without_destination_is_logged_and_forwarded`.

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
  surface, so its `delete_id` ends the object and reaches the guest. A
  guest request from it after that, a destructor included, ends the
  session with `invalid_object`, as libwayland-server answers a request
  to an ID whose resource it destroyed. A live
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
- **An object's state ends with it, once.** Every end of an object goes
  through a `Context` wrapper of its table transition (the transitions
  are `pub(super)`), which forgets the object's per-interface state at its
  first end: a guest destructor, terminal event, local destructor,
  abandoned bind, or a host `delete_id` for a live object, which forgets
  what the guest's destructor would but sends nothing to the deleted
  object. The `delete_id` that acknowledges a destructor forgets nothing
  again, and a render buffer, which outlives its guest object while the
  host shows it, ends when the host releases its ID. An asynchronous
  dma-buf create ends with its params; releasing the host ID drops it
  too, as a backstop. A new map of per-object
  state gets its arm in `state::lifecycle::forget_object` and its check in
  `Context::assert_no_orphans`. Ending an object sends its teardown in the
  current message's effects, in the wire order its destructor handler
  had: the Aura objects of a surface or toplevel are released before its
  destructor, and a surface's text-input leave and buffer retirement
  follow it.
  Tests: `a_host_destroyed_output_is_forgotten`,
  `a_host_destroyed_keyboard_is_forgotten`,
  `a_host_destroyed_surface_is_forgotten`,
  `a_host_destroyed_text_input_sends_nothing_to_its_host_object`,
  `a_delete_id_after_the_destructor_forgets_nothing_again`,
  `wl_surface_destroy_releases_its_aura_children_first`,
  `wl_surface_destroy_releases_zaura_surface`,
  `xdg_toplevel_destroy_reserves_aura_id_until_host_delete_id`,
  `wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave`,
  `guest_streams_cannot_crash_hang_or_leak`.
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
  routing or IME state that child devices still own. A keyboard or text
  input stays a member of the seat it was created from after the seat
  ends, through its release or a host `delete_id`, and a `wl_seat` the
  guest later binds under the same ID takes it over (Exo has one seat).
  Until then the guest may reuse the ID for any object, so a text input
  sends it to the host as its seat only while it names a live `wl_seat`.
  Tests: `seat_release_does_not_break_live_child_keyboard_press_release_pair`,
  `release_keeps_child_keyboard_routing_state`,
  `release_does_not_end_child_text_input_focus`,
  `children_keep_an_ended_seat_whose_id_the_guest_reuses`,
  `an_activation_deferred_for_an_unmapped_seat_sends_the_content_type`,
  `a_seat_id_reused_for_another_object_is_not_sent_as_a_seat`,
  `a_released_seat_whose_id_becomes_a_surface_carries_no_deactivate`.

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
- **A removed global waits for every registry that lists it.** Its
  connection-wide metadata, and the internal binding it backs, stay until
  every registry that listed it has seen its `global_remove`, so each can
  still be sent its own; then the name is tombstoned and new binds of it
  are refused. A registry destroyed through `wl_fixes` before it sees the
  removal no longer counts, and if it was the last, its destruction
  retires the global. The removal is on record connection-wide from the
  first `global_remove` until the global is retired or advertised anew,
  so this holds even when every registry that saw it was destroyed first.
  A registry destroyed while the global is still advertised retires
  nothing.
  Tests: `destroying_the_last_registry_to_list_a_removed_global_retires_it`,
  `a_removal_outlives_the_registry_that_saw_it`,
  `the_removal_of_a_dropped_global_waits_for_every_registry`,
  `destroying_a_registry_keeps_a_global_no_registry_saw_removed`.
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
  the `zaura_shell` v6 floor). Every forwarded host global but
  `zwp_linux_dmabuf_v1` is offered through `forwarded_global_version`;
  the proxy advertises that one itself, at `GUEST_DMABUF_VERSION` when it
  can synthesize feedback and otherwise at the host's version capped at
  v3. `forwarded_global_version` never exceeds the generated `VERSION`
  (`protocols::interface_version`), so a host newer than the vendored XML
  cannot make the guest send requests the proxy cannot decode. A child
  object takes its parent's version, and a request is sent to a host
  object only if that object's version has it: an Aura surface keeps the
  version of the shell generation that created it.
  Tests: `child_objects_inherit_their_manager_versions`,
  `unset_snap_is_sent_only_to_an_aura_surface_that_has_it`,
  `every_forwarded_global_is_capped_at_its_xml_version`.

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
- **Surface damage copies the pixels it touches.** Guests see
  `wl_compositor` v3, so most damage arrives in surface units, and the
  copy maps it back to buffer pixels: exactly under an integer buffer
  scale, and through a viewport by inverting the buffer damage mapping
  below, grown by one buffer pixel and rounded outward, so no pixel whose
  image meets the damage is left out. Only a transformed buffer, or an
  attach offset, copies in full; copying a 4K buffer in full every frame
  costs about 2 GB/s.
  Tests: `surface_damage_maps_to_every_buffer_pixel_it_meets`,
  `destination_only_viewport_copies_only_the_damaged_pixels`,
  `surface_damage_under_an_integer_buffer_scale_copies_its_scaled_pixels`,
  `surface_damage_under_a_buffer_transform_copies_the_whole_buffer`.
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
- **Buffer damage reaches the host in surface coordinates.** Exo offers
  `wl_compositor` v3, which has no `damage_buffer`, and reads
  `wl_surface.damage` after the buffer scale and the viewport. The proxy
  forwards both unchanged, so it maps buffer damage in the order of
  `viewporter.xml`: the buffer transform, then the buffer scale, then the
  viewport crop and scale, keeping the source's 24.8 fraction. Each
  rectangle first grows by one buffer pixel, and its mapped edges round
  outward. A transformed buffer damages the whole surface, whose extent
  swaps under a 90 or 270 degree rotation, because the bridge does not
  rotate damage. Mapping in another order puts the damage where nothing
  changed, and the host shows stale pixels.
  Tests: `buffer_damage_maps_through_buffer_scale_before_the_viewport`,
  `whole_buffer_damage_covers_the_whole_destination`,
  `buffer_damage_subtracts_the_source_offset_in_surface_units`,
  `source_only_viewport_maps_buffer_damage`,
  `viewport_damage_uses_committed_crop_and_destination`,
  `rotated_buffer_damage_covers_the_transformed_surface`.
- **Damage is clamped.** Every edge of every rectangle, full damage
  included, lies within ChromiumOS' `INT_MIN / 10` to `INT_MAX / 10`
  headroom before it is encoded for the host, so the host's `x + width`
  cannot overflow. A rectangle stays at least 1x1, so one pushed past the
  end shrinks to the headroom's last unit.
  Tests: `extreme_surface_damage_is_clamped_before_host_encoding`,
  `host_damage_is_clamped_on_both_edges`.

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
  only after the guest GPU's writes have completed; a failed check is
  fatal and never commits. The check never waits: a busy buffer parks
  the commit before it changes anything, and the proxy dispatches it
  again once the fence may have signalled (see
  [Message effects](DESIGN.md#message-effects)). Nothing from the guest
  is dispatched before the parked commit, so the host sees the order the
  guest sent, while host events and other clients go on. Each mechanism
  keeps the bound of its old blocking wait, per plane and counted from
  the first time the commit waits: a second for a sync_file, as
  ChromiumOS Sommelier bounds its wait, and 15 s for the virtio-gpu
  fallback, the kernel's bound for a blocking `DRM_IOCTL_VIRTGPU_WAIT`.
  Then the session ends with one `implementation` error.
  Tests: `unexpected_native_sync_failure_is_fatal_and_never_commits`,
  `a_commit_waits_for_a_pending_fence_before_it_changes_anything`,
  `a_pending_fence_parks_the_guest_stream_while_host_events_flow`,
  `a_parked_commit_is_forwarded_once_when_its_fence_signals`,
  `run_forwards_a_parked_commit_once_its_sync_file_signals`,
  `host_traffic_does_not_postpone_polling_a_busy_fence`,
  `a_fence_that_never_signals_ends_the_session_at_its_deadline`,
  `a_fallback_fence_that_never_signals_ends_the_session_at_its_deadline`,
  `a_slow_fallback_fence_outlasts_the_sync_file_bound`,
  `a_fence_without_a_descriptor_is_polled_less_often_as_it_waits`.

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
- **Host modifiers last as long as their keyboard.** Exo sends
  `wl_keyboard.modifiers` only when they change, before `enter` rather than
  after it, and none with a new keymap. The proxy therefore keeps each host
  keyboard's last modifiers through focus changes and keymap reloads,
  applies them to every keymap it compiles, including one that arrives
  after them, and forgets them only when the keyboard ends. Otherwise a
  chord pressed with a modifier held across a focus change or a layout
  switch would be classified without it.
  Tests: `modifiers_sent_before_enter_survive_it`,
  `modifiers_survive_a_focus_change`,
  `modifiers_carry_over_to_a_new_keymap`,
  `modifiers_sent_before_the_keymap_apply_to_it`.
- **Content hints come from the v3 XML.** Text-input-v3 `content_hint` bits
  are read from the vendored `text-input-unstable-v3.xml`, not the
  similarly named v1 enum. v3 bit `0x2` is `spellcheck` and maps to
  Chrome's `spellcheck_on` (`1 << 4`); Chrome's `autocorrect_on` (`1 << 2`)
  is a separate flag and is never inferred from it.
  Test: `content_type_mapping_preserves_pin_privacy_and_inline_composition`.
- **`set_input_type` replaces v1 `set_content_type`.** When the host's
  `zcr_extended_text_input_v1` has `set_input_type` (v8), a content type
  reaches Exo through it alone, as the extension XML says ("used in place
  of"), right after `set_surrounding_text_support` (v9). Exo applies that
  support to the next of the two requests and resets it, so a v1
  `set_content_type` in between would report every field without
  surrounding text as supporting it. Older extensions get
  `set_content_type`, then `deprecated_set_input_type` (v2). The mapping
  to `set_input_type` is lossy: the `latin` hint, `multiline` outside the
  normal, alpha and name purposes, and the `terminal` purpose have no
  equivalent. They had no effect before either, because `set_input_type`
  replaced what Exo derived from `set_content_type`.
  Test: `content_type_takes_one_host_request_per_extension_version`.
- **The host gets each content type once it can apply it, and only
  then.** Exo applies a content type only while an input method is
  attached: it drops one after `deactivate` and before `activate`, and
  attaching resets the type and mode to text while keeping the previous
  field's flags, learning mode and surrounding-text support, so a password
  field would be treated as text the IME may learn. It learns whether the
  field has surrounding text only with a content type. So the proxy keeps
  what the host applied (`HostApplied`) and `reconcile` sends the
  committed content type, with its support, whenever the attached input
  method lacks it: after every `activate`, including one a drain barrier
  or a seat without a host object deferred, when the guest changes it,
  when the field's surrounding text appears or goes, and when an enable
  starts a new field. It sends none while no input method is attached and
  none the host already has: with a v8 extension Exo ignores an unchanged
  type, but before v8 `set_content_type` and `deprecated_set_input_type`
  map flags differently, so a resend changes the type twice, and each
  change can reset the input method's composition. Exo keeps surrounding
  text and the cursor rectangle without an input method, so they are
  forwarded as committed and never replayed.
  Exo reports every attach of its input method as
  `zwp_text_input_v1.enter` and every detach as `leave`, including those
  it makes on its own. It detaches on its own when its seat's focus
  leaves the surface, the surface is destroyed or the input method is
  destroyed, each time through `Deactivate`, so it attaches again only on
  a new `activate`. A `leave` while the proxy's host generation is active
  is such a detach, since the proxy's own `deactivate` starts draining
  first: the generation ends there without a barrier, its composition is
  dropped, a preedit the guest shows is cleared with an empty
  `preedit_string` and `done`, and reconciling activates again if the
  guest still wants input. If that `leave` beats the `wl_keyboard.leave`
  of the same focus change, the activation finds Exo unfocused and the
  keyboard leave drains it. Exo attaches on its own only when focus
  reaches a surface an `activate` found unfocused, after a
  `wl_keyboard.leave` that set the proxy draining, so an `enter` while
  active answers the proxy's own `activate` and resends nothing.
  `HostApplied` is therefore sound only while every host
  `zwp_text_input_v1.leave`, `wl_keyboard.leave` and `wl_keyboard.enter`
  reaches the text-input state in order with the events around it, never
  dropped, delayed or coalesced with its pair: otherwise the proxy would
  believe an input method attached that Exo detached, or the reverse, and
  send content types Exo drops or withhold the one it needs.
  Tests: `reconcile_reaches_every_target_with_only_needed_requests`,
  `surrounding_text_support_follows_the_field_and_resends_only_on_change`,
  `a_host_without_surrounding_text_support_gets_no_resend`,
  `a_deferred_activation_replays_the_committed_content_type`,
  `an_activation_deferred_for_an_unmapped_seat_sends_the_content_type`,
  `a_content_type_waits_for_the_host_to_attach`,
  `the_enter_answering_an_activate_resends_nothing`,
  `an_exo_detach_activates_again_with_the_content_type`,
  `a_content_type_waits_for_exo_to_attach_again`,
  `the_leave_answering_a_deactivate_is_ignored`,
  `an_exo_detach_clears_the_guest_preedit`,
  `an_exo_detach_before_the_keyboard_leave_activates_then_drains`, and
  the fuzzer's end state against its model of Exo, whose input method
  sometimes restarts (`guest_streams_cannot_crash_hang_or_leak`; see
  [Randomized tests](../CONTRIBUTING.md#randomized-tests)).
- **One text input on a connection is enabled at a time.** Exo has one
  seat, and its `Activate` gives the input method to the text input that
  activated last without telling the one it replaced. That one keeps its
  activation but gets no input, and `Activate` on the same surface does
  nothing, so it would stay deaf until deactivated. A text-input-v3 enable
  while another text input on the connection is enabled, for any guest
  `wl_seat`, is therefore ignored: the commit counts, closes the
  composition and changes nothing else.
  Test: `second_text_input_enable_on_the_connection_is_ignored`.
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
- **Nothing waits on a guest client or on host data.** Every client runs
  on the loop's one thread (see
  [Process and connections](DESIGN.md#process-and-connections)), so a call
  that waits for a peer to act stalls them all. The VirtWL device is the
  exception the proxy cannot avoid. A send goes through the non-blocking
  context descriptor and awaits room, but every other ioctl
  (`create_pipe`, `allocate`, `allocate_dmabuf`, `dmabuf_sync`) and every
  vfd close is a synchronous round trip to the host: it waits up to a
  second for room in a full queue, then as long as the host takes to
  answer. A descriptor the proxy waits on is non-blocking and polled
  through Tokio's `AsyncFd`, as the clipboard pumps do
  (`data_device::pump`):
  - A pump watches each end for the one direction it uses. A peer that
    stalls parks only its pump, except that host data the pump has not
    read stays in VirtWL's receive queue, which every VirtWL connection
    shares. So a pump that moves no data for `PUMP_IDLE_TIMEOUT` (60 s)
    ends. That includes the wait for the first byte, so a host that takes
    longer to produce its data is cut off. The guest's pipe then closes
    as after a whole transfer: a pipe cannot carry an error, so the guest
    cannot tell a cut-off paste from a complete one. This is deliberate.
    A regular-file destination is the exception: its writes never report
    "would block", so the timeout does not bound them.
  - A pump never changes the flags of the guest's open file description,
    which the client shares, and so may another pump into the same pipe.
    It writes a socket with `MSG_DONTWAIT`, and an anonymous pipe on
    Linux 6.5 or later with `pwritev2(RWF_NOWAIT)`. A named FIFO, or any
    pipe on an older kernel, refuses `RWF_NOWAIT`, so it is reopened
    through `/proc/self/fd` as a private non-blocking description. That
    needs write permission on the pipe, so such a pipe from a client
    running as another user (through `sudo -E`, say) has its transfer
    refused.
  - A regular file, such as a memfd, is written directly. A local one
    never waits on another process, but one on a network or FUSE file
    system (virtiofs, 9p, NFS) can, and no flag prevents that. Any other
    destination, such as a device, is refused.
  - A guest that closes its end (EPIPE, or ECONNRESET on a socket) ends
    the transfer like EOF.
  - Stopping the pumps ends them without waiting on any peer, even
    mid-transfer, and closes their descriptors.
  - A connection runs at most `MAX_CLIPBOARD_PUMPS` (16) pumps:
    `ClipboardPumps::slot` gives no room beyond that, which the cap test
    shows. `on_receive` then drops the `wl_data_offer.receive` before it
    duplicates anything, and the dropped request closes the guest's
    descriptor, so the guest reads EOF. The first refusal on a connection
    warns and later ones log at debug level.

  `sommelier/clippy.toml` rejects the blocking calls Clippy can name, but
  it cannot tell a blocking `read` or `ioctl` from a non-blocking one, and
  its lint also guards descriptors, so an allow for either lifts both.
  The dma-buf fence check polls with a zero timeout and asks virtio-gpu
  with `VIRTGPU_WAIT_NOWAIT`, so a commit the guest GPU is still writing
  waits off the loop ([native buffers are fenced](#dma-buf)).
  Tests: `stop_clipboard_pumps_aborts_and_closes_owned_fds`,
  `clipboard_pumps_are_capped_per_connection`,
  `a_transfer_the_host_stalls_ends_after_the_idle_timeout`,
  `a_transfer_the_guest_stalls_ends_after_the_idle_timeout`,
  `a_slow_reader_parks_the_pump_without_blocking_the_loop`,
  `pumps_into_one_pipe_never_change_its_flags`,
  `pump_writes_into_a_pipe_it_cannot_reopen`,
  `pump_writes_into_a_named_fifo`,
  `a_reopened_pipe_is_private_and_non_blocking`,
  `a_closed_reader_ends_the_pump`,
  `pump_writes_into_a_regular_file`,
  `pump_refuses_a_character_device`,
  `pump_copies_a_large_transfer_whole`.
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
