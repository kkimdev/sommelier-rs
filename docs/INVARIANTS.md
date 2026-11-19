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
  only cleanup path. The receive buffer has room for Linux's `SCM_MAX_FD`
  (253) descriptors, the most one `SCM_RIGHTS` message carries, so a valid
  peer never causes truncation.
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
  `dmabuf_probe_closes_its_descriptor_whatever_the_result`,
  `recv_ioctl_error_closes_descriptors_written_before_failure`,
  `recv_rejects_an_oversized_length_and_closes_its_descriptors`,
  `raw_fds_are_closed_once_each`.

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
- **A string that is not UTF-8 does not end the session.** Wayland
  requires UTF-8, but libwayland does not check it, so a client that sets
  its title from arbitrary bytes, or names a Latin-1 file, works with
  other compositors. `WireMessage::read_string` decides for every string
  in both directions: it replaces each invalid sequence with U+FFFD (and
  logs it at debug), and the proxy forwards and matches the replaced
  string. A replaced interface name or MIME type matches nothing: a host
  global of one is not advertised, a guest bind of one is an invalid bind,
  and a type offered under one is asked for by a name its owner does not
  know, so that transfer gets no data. The exception is a string that
  other arguments index by byte offset, whose offsets a replacement would
  move: the text of `set_surrounding_text` and `preedit_string` in both
  text-input versions (the generator's `BYTE_INDEXED_STRINGS`) decodes to
  `wire::IndexedString`, and a handler drops one that is not UTF-8,
  logging it at debug. A guest `zwp_text_input_v3` surrounding text makes
  the pending one empty, so that its commit empties the host's rather than
  leave the input method editing text the guest no longer has, while the
  field keeps its support for surrounding text and the host its content
  type. A host `zwp_text_input_v1` preedit reaches the guest as nothing,
  taking the `preedit_cursor` and `preedit_styling` sent for it with it.
  The host's `commit_string` is replaced: the only offsets into it, in
  `cursor_position`, have no v3 equivalent. A NUL inside a string is still
  an `invalid_method` (`ProtocolError::InvalidString`), as libwayland
  rejects it. A replacement can make a string up to three times longer, so
  a forwarded message near the length limit can still end the session as
  `MessageTooLarge` (see above).
  Tests: `wire_message_replaces_invalid_utf8_in_strings`,
  `a_guest_string_that_is_not_utf8_reaches_the_host_with_replacement_characters`,
  `a_host_string_that_is_not_utf8_reaches_the_guest_with_replacement_characters`,
  `an_interface_name_that_is_not_utf8_names_no_global`,
  `indexed_strings_keep_bytes_that_are_not_utf8`,
  `test_generate_byte_indexed_strings`,
  `test_byte_indexed_strings_each_name_one_argument`,
  `surrounding_text_that_is_not_utf8_clears_the_hosts`,
  `host_ime_events_become_v3_transactions`,
  `a_string_with_a_nul_inside_is_an_invalid_method`,
  `wire_message_rejects_string_with_embedded_nul`.
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
- **Nothing the proxy sends depends on hash order.** No message the
  proxy sends, and no state that later decides one (an ID or number it
  allocates, or which of several objects it picks), depends on the order
  of a hash collection. When a message makes the proxy send something for
  each of several objects, it visits them in ascending order of a key the
  session's messages decide, such as an ID. The crate therefore uses no
  hash collection: every map and set is a `BTreeMap` or `BTreeSet`, and
  `sommelier/clippy.toml` makes `std::collections::HashMap` and `HashSet`
  `clippy::disallowed_types` errors. So the same messages always make the
  same bytes, and a session can be replayed exactly.
  Tests: `the_text_inputs_of_a_seat_follow_its_focus_in_id_order`,
  `a_host_format_reaches_every_guest_shm_in_id_order`,
  `keys_held_at_an_enter_are_numbered_in_the_hosts_order`.

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
`an_invalid_bind_is_a_protocol_error`,
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
  `new_id`'s object only once its message's sender and every object
  argument have resolved, so a message dropped for a local sender, for a
  local object in a non-nullable request argument (a nullable one is sent
  as null) or for an event object with no guest counterpart maps nothing
  and consumes no ID. An event object of an interface resolves before the
  handler runs; the one of no interface, `wl_display.error`'s, resolves as
  the event is forwarded, also before any mapping. A handler that returns
  `Forward` must not map the request's own `new_id`; mapping belongs to
  the generated arm. Handlers that map it themselves (`on_bind`,
  `on_get_registry`, `on_sync`, `on_get_text_input`) return `Drop`. Freed
  IDs are reused first, so the ID
  spaces stay as dense as the live objects, and each stays within
  libwayland's map limit: index `WL_MAP_MAX_OBJECTS` (`0xF00000`)
  inclusive, the ID itself on the client side and its offset from
  `0xff000000` on the server side. A `new_id` either peer sends past that
  limit is refused like a reused one, as libwayland refuses it.
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
  `guest_id_availability_rejects_reserved_and_reused_ids`,
  `host_new_ids_must_be_fresh_server_ids`,
  `event_from_an_unmapped_sender_maps_no_new_id`,
  `request_dropped_for_a_local_argument_maps_no_new_id`,
  `test_request_object_arguments_are_checked_before_new_id_mapping`,
  `test_event_object_arguments_are_checked_before_new_id_mapping`,
  `test_no_forward_drops_after_mapping_a_new_id`,
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
  request. A request that creates an object still creates it, because the
  guest owns its ID, but inert: a `wl_shm_pool` that ignores its requests
  in turn, or a `wl_buffer` that attaches as no buffer. Children created
  before the removal (such as a `wl_shm_pool`) still complete their own
  lifecycle and emit exactly one `delete_id`. Only the objects bound from
  the removed global, and their children, go inert: a guest `wl_shm` bound
  from a replacement name keeps working.
  Tests: `removed_host_shm_creates_only_inert_pools_and_buffers`,
  `only_objects_bound_from_a_retired_shm_global_go_inert`,
  `a_guest_shm_bound_from_an_unbound_name_goes_inert_when_it_retires`,
  `a_re_advertised_shm_name_retires_only_its_old_generation`,
  `removed_host_shm_ignores_requests_from_existing_pool_until_destroy`,
  `attaching_an_inert_buffer_detaches_the_surface`.
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
  `a_seat_id_reused_for_another_object_is_not_sent_as_a_seat`.

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
  `an_invalid_bind_is_a_protocol_error`.
- **A removed global waits for every registry that lists it.** Its
  connection-wide metadata, and the internal binding it backs, stay until
  every registry that listed it has seen its `global_remove`, so each can
  still be sent its own; then the metadata goes, and a new bind of the
  name is refused through any registry. A registry destroyed through
  `wl_fixes` before it sees the removal no longer counts, and if it was
  the last, its destruction retires the global. The removal is on record
  connection-wide from the first `global_remove` until the global is
  retired or advertised anew, so this holds even when every registry that
  saw it was destroyed first.
  A registry destroyed while the global is still advertised retires
  nothing. Only a registry that lists the current advertisement keeps it
  alive; one that still lists an older advertisement of a re-advertised
  name neither delays retirement nor receives the proxy's bind of the
  replacement. A guest bind through such a registry is currently a
  protocol error; whether it should be is an open question, unpinned
  because libwayland-server never reuses a name (see the name-reuse item
  in HANDOVER.md). A guest bind through a registry that has not yet seen
  the removal is forwarded. That relies on the host keeping a removed
  global bindable until `wl_global_destroy`, as Exo does: it calls
  `wl_global_remove` and defers `wl_global_destroy`. A host that destroyed
  the global at once would make that bind an `INVALID_OBJECT` error, as
  it would for a native client.
  Tests: `destroying_the_last_registry_to_list_a_removed_global_retires_it`,
  `a_removal_outlives_the_registry_that_saw_it`,
  `an_invalid_bind_is_a_protocol_error`,
  `the_removal_of_a_dropped_global_waits_for_every_registry`,
  `destroying_a_registry_keeps_a_global_no_registry_saw_removed`,
  `a_singleton_binding_moves_to_the_lowest_live_candidate`.
- **A re-advertised name replaces its old generation.** When the host
  re-advertises a global under a previously removed numeric name, the
  connection-wide metadata for that name MUST be replaced before the new
  interface is processed, so stale entries cannot route a bind to the old
  interface or misclassify the replacement's `global_remove`. Live children
  of the old generation are not invalidated.
  Tests: `re_advertised_name_replaces_visible_metadata_before_hidden_global`,
  `re_advertised_hidden_name_replaces_hidden_metadata_before_visible_global`,
  `stale_remove_cannot_invalidate_a_re_advertised_global_generation`.
- **A singleton binding moves to its replacement.** The proxy binds
  `wl_shm`, `zwp_linux_dmabuf_v1`, `zcr_keyboard_extension_v1`,
  `zaura_shell` and, for the remote-shell-v2 placement backend,
  `zcr_remote_shell_v2` once per connection for its own use. A host may advertise
  one under a new name while the old name is bound; when the old one
  retires, the proxy binds the lowest name (possibly the same name at a
  new generation) that a registry still lists and that no registry has
  seen removed, since a compositor may destroy a global as soon as it
  removes it. A retired `zcr_remote_shell_v2` is not destroyed: Exo
  sends its remote surfaces' events through it.
  Tests: `a_singleton_advertised_anew_is_bound_when_the_old_name_retires`,
  `a_removed_remote_shell_keeps_its_windows`,
  `a_singleton_binding_moves_to_the_lowest_live_candidate`,
  `a_promoted_dmabuf_publishes_its_deferred_guest_global`.
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
  Tests: `pool_backing_must_be_a_long_enough_regular_file`,
  `create_pool_rejects_a_pipe_or_a_short_file_as_invalid_fd`,
  `a_pool_resize_must_keep_or_grow_its_size_within_its_file`.
- **Without VirtWL, guest SHM is a memfd.** With no VirtWL channel
  (`--local-compositor`), each guest `wl_buffer` gets a memfd of its size,
  which the host imports as a `wl_shm` pool, with or without
  `--gpu-accel`; the guest's linux-dmabuf buffers are unaffected. The
  guest's `wl_shm` then offers an optional format only once the host's
  `wl_shm` lists it, not when only the host's linux-dmabuf does, or a
  strict host fails the buffer with `wl_shm.invalid_format`.
  Tests: `without_virtwl_a_buffer_is_backed_by_a_memfd_of_its_size`,
  `without_virtwl_guest_shm_offers_only_the_host_wl_shm_formats`.
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
  path entirely (no dma-buf sync), so a damage-free commit, such
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
  `wl_compositor` v4; the proxy keeps `damage_buffer` local over Exo's v3.
  Buffer damage is already in buffer pixels. Surface damage is mapped back
  to buffer pixels: exactly under an integer buffer scale, and through a
  viewport by inverting the buffer damage mapping below, grown by one
  buffer pixel and rounded outward, so no pixel whose image meets the
  damage is left out. Only a transformed buffer, or an attach offset,
  copies in full; copying a 4K buffer in full every frame costs about
  2 GB/s.
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
  negotiated version keep the permissive pre-v5 behavior. No guest reaches
  v5 today (the `wl_compositor` ceiling is v4); this keeps the path correct
  if the ceiling is raised, which would also need `wl_surface.offset`,
  forwarded today, to stay local.
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
- **Keymaps are bounded.** A host keymap larger than `MAX_KEYMAP_SIZE`
  (1 MiB; Exo's are about 60 KiB) is refused before it is mapped. Like any
  other keymap the proxy cannot use, it clears the keyboard's XKB state, so
  no chord is classified until a usable keymap arrives, and it is still
  forwarded. The size of a VirtWL keymap cannot be checked against its
  file, and the proxy copies a keymap to compile it, so a size near 4 GiB
  would otherwise abort every client.
  Tests: `on_keymap_ignores_a_keymap_over_the_size_limit`,
  `an_unusable_keymap_clears_the_loaded_one_and_is_forwarded`.
- **A keymap ends no held key.** A keymap, whether the proxy can use it or
  not, replaces only what was compiled from the one before it: the XKB
  state and the keysym and repeat tables. The keys held and their
  generations stay, so each key's release goes where its press went, and
  so does the suppression of a held accelerator's key, which its press set
  and only its release, an `enter` or the keyboard's end takes. Otherwise a
  keymap that arrives while a key is held would send its release to the
  host, and the guest, which got the press, would hold the key and repeat
  it; and Exo, which switches the layout on Ctrl+Space while Space is
  held, would let the IME hand Space to the guest as a keysym or a
  recovered key. This holds for a release on the `wl_keyboard` channel: a
  keysym release finds its key through the current keymap's table, so one
  after a keymap may find none. Exo sends no keysym releases today.
  Tests: `a_new_keymap_keeps_held_keys_and_modifiers`,
  `a_rejected_keymap_keeps_held_keys`,
  `an_unusable_keymap_clears_the_loaded_one_and_is_forwarded`,
  `an_accelerator_held_across_a_keymap_is_not_recovered`,
  `an_accelerator_held_across_a_keymap_takes_no_keysym`.
- **IME state is scoped to its keyboard or seat.** Pressed-key sets, repeat
  flags and focus markers used to bridge ChromeOS IME events belong to the
  originating keyboard or seat, never to the process. Synthetic
  `wl_keyboard` events use the compositor's monotonic millisecond time
  domain of the real keyboard events, never wall-clock time. Host keysyms
  go to the keyboard whose surface has the seat's current focus.
  Tests: `ime_repeat_state_is_scoped_to_the_originating_keyboard`,
  `an_empty_confirmation_recovers_the_latest_eligible_held_key`,
  `keysym_prefers_keyboard_with_current_surface_focus`.
- **A recovered key carries the host's serial.** When an empty IME
  confirmation stands for a held key the input method consumed, the guest
  gets a `wl_keyboard.key` press and release the host never sent. Both
  carry the serial and time of the key's latest `peek_key`. The extension
  defines a peek's arguments, serial included, as those of the
  `wl_keyboard.key` it announces, and Exo caches that serial
  (`MaybeNextKeySerial`) until it sends a `wl_keyboard.key`, so Exo's
  next real key event carries it too. The release has no host serial of
  its own, since the key is still held, so it reuses the press's. A
  request that quotes the serial, such as
  `wl_data_device.set_selection`, is accepted: Exo checks only that a
  serial is among the last 1024 it issued (`kMaxEventsTracked`). A
  counter of the proxy's own, starting from 1, falls outside that window
  once Exo has issued more serials, and Exo ignores such a request.
  Tests: `a_recovered_key_carries_the_latest_peek_serial`,
  `an_empty_confirmation_recovers_the_latest_eligible_held_key`.
- **An empty confirmation recovers a held key only once the host shows it
  repeated.** At extension v11 Exo confirms before every press its input
  method does not handle (E13), and sends that press's `peek_key` and
  `wl_keyboard.key` after the confirmation, in the same task (E8). So an
  empty `confirm_preedit` while a key the input method took is held is
  either that key's autorepeat or a new press's confirmation. The
  keyboard concern holds the recovery of the eligible held key, the one
  with the latest peek, numbered when its peek arrives
  (`ime_keys::PendingRecovery`), and decides it before it handles the
  host's next event. Before any peek, `wl_keyboard.modifiers` leave it
  waiting, and so does the `peek_key` press of a key the keyboard does not
  hold. That key's `wl_keyboard.key` press, with the peek's serial, drops
  it, as a confirmation that finds no held key, and so does the text
  input's `leave`: Exo confirms as it detaches the input method (E6). Any
  other host event recovers the key first, so that its pair and `done`
  precede that event.
  With a recovery that waits the proxy sends the host a `wl_display.sync`,
  whose `done` follows the whole task (E11), so one round trip decides it
  at the latest, with no timer. The pair goes only if the text input still
  passes the gate and the key is still held on its keyboard, and the
  `done` carries the guest's state under the serial of then. Exo's own
  detach (H2), as when the input method's process restarts, also ends the
  recovery of every key the seat holds: the key events in flight die with
  the input method, so a held key's release may never come, and only the
  key's next press can be recovered. Otherwise a key typed while Backspace
  or Space is held would also repeat the held key, and so would a restart.
  Tests: `a_confirmation_before_an_unhandled_press_recovers_no_held_key`,
  `a_confirmation_before_a_handled_press_recovers_the_held_key`,
  `a_waiting_recovery_ends_with_its_text_input_or_keyboard`,
  `an_input_method_detach_ends_the_recovery_of_held_keys`,
  `keys_held_since_enter_are_ordered_by_their_peeks`.
- **A key waits for at most one late release per channel.** When a key
  is pressed again before the channel that gave the guest its previous
  press (`wl_keyboard` or an IME keysym) has released it, the proxy keeps
  that retired release pending beside the new generation, so a late
  `wl_keyboard` release still closes the old press. It keeps at most one
  retired release per key and channel, plus the current generation's: a
  newer retired release replaces an older one. For `wl_keyboard` this drops
  nothing Exo sends: Exo sends each `wl_keyboard.key` right after its
  `peek_key`, and a release only for a key it sent pressed, so the one
  retired release already tolerates a host that delays a release past
  the key's next press. For keysyms the bound is only defensive, and a
  late release is not supported: a keysym release that arrives after the
  key's next press does not close the retired press, since the proxy
  bounds the retired press by a peek serial while Exo numbers each keysym
  when it sends it. Exo sends keysym presses only (its one `SendKey`
  caller, `InsertChar`, handles presses), so a retired keysym release
  waits for an event Exo never sends, and the bound only keeps such
  entries from piling up; making a late keysym release close its press is
  a known follow-up. A host that never sends a release leaves one pending release
  per key and channel, not one per press.
  Tests: `a_channel_keeps_one_pending_release_per_key`,
  `a_delayed_release_ends_only_its_generation`.
- **Host modifiers last as long as their keyboard.** Exo sends
  `wl_keyboard.modifiers` only when they differ from the last it sent that
  keyboard, and again after each Num Lock press; it sends them before
  `enter` rather than after it, and none with a new keymap. The proxy
  therefore keeps each host keyboard's last modifiers through focus
  changes and keymap reloads, applies them to every keymap it compiles,
  including one that arrives after them, and forgets them only when the
  keyboard ends. Otherwise a chord pressed with a modifier held across a
  focus change or a layout switch would be classified without it. The
  protocol resets a client's modifiers at `leave` and requires a
  `modifiers` event after every `enter`, which Exo omits, so the proxy
  fills that in: it sends the guest the keyboard's modifiers after each
  `enter` it forwards.
  Tests: `modifiers_sent_before_enter_survive_it`,
  `modifiers_survive_a_focus_change`,
  `modifiers_carry_over_to_a_new_keymap`,
  `modifiers_sent_before_the_keymap_apply_to_it`,
  `every_forwarded_enter_is_followed_by_the_keyboards_modifiers`.
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
  what the host applied (`ime::LinkState`), and at the end of every input
  of the IME core (`ImeCore::step`) `ime::reconcile` brings every text
  input to the state its guest committed: it sends the committed content
  type, with its support, whenever the attached input method lacks it:
  after every `activate`, including one a drain barrier
  or a seat without a host object deferred, when the guest changes it,
  when the field's surrounding text appears or goes, and when an enable
  or a disable starts a new field (an enable of an enabled field does
  not). It sends none while no input method is attached and
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
  `LinkState` is therefore sound only while every host
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
  `wl_seat`, is therefore ignored: the commit still counts, closes the
  composition and applies the rest of its state, but the text input stays
  disabled and is not activated. The flag lives in the committed state of
  the focused field, so a focus change, a surface's end and the object's
  end clear it with the field, and the IME core's invariants check that
  one text input at most has it (I3 of
  [IME_REDESIGN.md](IME_REDESIGN.md) 3.1).
  Test: `second_text_input_enable_on_the_connection_is_ignored`.
- **Focus teardown sends `leave` itself.** When a focused text-input
  surface is destroyed or loses focus, the proxy MUST send
  `zwp_text_input_v3.leave` as part of that teardown, while the surface ID
  is still valid, rather than waiting for the host's keyboard leave. A
  delayed host keyboard leave cannot be relied on, since it may be rejected
  as stale once the surface mapping is gone. Teardown is explicit and
  idempotent: the later delayed leave does not duplicate the event. A
  surface's end first moves the keyboard focus of each seat off it, which
  resets that seat's fields (`ime::Input::FocusChanged`), then resets any
  other field still focused on it to its seat's focus
  (`ime::Input::SurfaceDestroyed`); each sends the `leave`.
  Tests: `wl_surface_destroy_sends_text_input_leave_before_delayed_keyboard_leave`,
  `ime_reaches_the_specified_text`.
- **Every v3 commit counts.** A text-input-v3 `done` serial is the number
  of `commit` requests the guest made on the object, including commits made
  while unfocused or whose state could not be encoded; those are still not
  forwarded to the host. A commit whose surrounding text does not fit a
  v1 message consumes its pending state and changes nothing else, so no
  later commit retries that text. The count is a `CommitCount`
  (`ime/serial.rs`) with a private field, which only its `bump` changes,
  called only from `ime::TextInput::count_commit`, in the Commit arm of
  `ImeCore::step`.
  Tests: `v3_done_serial_counts_commits_made_while_unfocused`,
  `v3_requests_after_leave_are_ignored_until_next_enter`,
  `oversized_surrounding_commit_does_not_partially_advance_state`,
  `the_commit_after_an_oversized_one_does_not_retry_it`.
- **A v3 transaction's events go in one order.** Every transaction the
  proxy sends a guest text input puts its events in this order, each one
  it has:
  1. `preedit_string("")`, when the guest shows a preedit and the
     transaction clears it, deletes or commits;
  2. `delete_surrounding_text`;
  3. `commit_string`;
  4. `preedit_string` with the new, non-empty preedit;
  5. `done`.

  A shown preedit is thus gone before any delete or commit, and a
  transaction that replaces it with a new one without editing text
  sends only the new one. A client that applies the transaction at its
  `done` keeps only the last `preedit_string`, so a clear followed by a
  new preedit is the same to it as the new one alone. It is the order of
  mutter, which sends
  a delete when the input method makes it, a commit right after a
  preedit clear, and the new preedit last, with the `done`
  (`src/wayland/meta-wayland-text-input.c` 317, 336-337 and 195-224,
  revision b766717a), and of KWin (`src/wayland/textinput_v3.cpp`
  268-323, revision f213b9af). Clients that store each event and apply
  the transaction at its `done`, in the protocol's order (delete, commit,
  then preedit), do not depend on it: Chromium's
  `ZwpTextInputV3Impl::OnDone`
  (`ui/ozone/platform/wayland/host/zwp_text_input_v3.cc`, revision
  ffb4f13519), GTK3's `text_input_done` (`modules/input/imwayland.c`,
  revision c780332c), GTK4's (`gtk/gtkimcontextwayland.c`, revision
  92348cd4), Qt's `QWaylandTextInputv3::zwp_text_input_v3_done`
  (qtbase `src/plugins/platforms/wayland/qwaylandtextinputv3.cpp` 188,
  revision 8a98cde366ff) and kitty's `text_input_done`
  (`glfw/wl_text_input.c` 103, revision 32f3f1dc71e8). Other clients
  depend on it:
  - winit drops the pending preedit at a `commit_string`
    (`winit-wayland/src/seat/text_input/mod.rs` 117-119, master
    f0abec90a3f9; `src/platform_impl/linux/wayland/seat/text_input/mod.rs`
    114-116 in 0.30.12), so a new preedit before a commit is lost.
  - SDL3 acts on each event as it arrives and turns a delete into a
    Backspace (`src/video/wayland/SDL_waylandevents.c` 2649-2710, main
    1fc62260fc81), which its `testime` ignores while a preedit is shown
    (`test/testime.c` 1084-1086), so a delete while the old or the new
    preedit is shown deletes nothing.

  No transaction has both a delete and a commit yet, since Exo's deletes
  stand alone (next rule). One order also gives each transaction one wire
  form, which a recorded trace can compare byte for byte.

  A transaction carries no event that changes nothing, since some
  clients do not read one as nothing and would delete the user's
  selected text:
  - no `preedit_string("")` while the guest shows no preedit: the `done`
    alone leaves it without one. GTK tells an empty preedit from none
    and emits `preedit-start` for one over none
    (`gtk/gtkimcontextwayland.c` 241-244), on which GTK4's `GtkText`
    deletes the selection (`gtk_text_preedit_start_cb`, `gtk/gtktext.c`
    4491), as does `GtkTextView` (`gtk_text_view_preedit_start_handler`,
    `gtk/gtktextview.c` 8834). Chromium applies a `done`'s preedit only
    if it differs from the last one it applied, an absent one counting
    as `("", 0, 0)` (`zwp_text_input_v3.cc` 466, `zwp_text_input_v3.h`
    139-140), so an empty preedit with the cursor `(0, 0)` over none
    changes nothing there. With the hidden cursor `(-1, -1)` it differs,
    and an empty composition over none deletes the selection
    (`ui/base/ime/linux/input_method_auralinux.cc` 325-328). So every
    empty `preedit_string`, the one that clears a shown preedit
    included, carries the cursor `(0, 0)`: after `("", -1, -1)`, the
    next `done` without a preedit would differ.
  - no `delete_surrounding_text(0, 0)`, such as an empty preedit region
    would make. Chromium applies it (`zwp_text_input_v3.cc` 440 tests
    that a delete arrived, not its lengths), and while nothing is
    composed `InputMethodAuraLinux::OnDeleteSurroundingText` deletes the
    selection (`input_method_auralinux.cc` 518).
  - no `commit_string("")`, which inserts nothing. GTK emits `commit`
    for it all the same (`text_input_commit_apply`,
    `gtk/gtkimcontextwayland.c` 281-282), and GTK4's `GtkText` and
    `GtkTextView` delete the selection before they insert the text
    (`gtk_text_enter_text`, `gtk/gtktext.c` 4592-4593;
    `gtk_text_view_commit_text`, `gtk/gtktextview.c` 8779). Chromium
    skips an empty commit (`zwp_text_input_v3.cc` 452).

  One encoder, `ime::GuestTxn::encode`, builds every such transaction,
  and nothing else in the crate builds these four events.
  Tests: `guest_txn_encodes_in_canonical_order`, `ime_is_isolated`,
  `host_ime_events_become_v3_transactions`,
  `a_reset_commit_waits_for_the_guest_to_report_it`,
  `on_set_preedit_region_translates_correctly`,
  `exo_deletes_become_v3_deletes_around_the_cursor`,
  `an_empty_preedit_reaches_only_a_guest_that_shows_one`,
  `an_empty_preedit_has_no_hidden_cursor`,
  `an_empty_preedit_region_sends_no_delete`.
- **Exo's deletes are absolute and stand alone.** Exo sends
  `zwp_text_input_v1.delete_surrounding_text(index, length)` with `index`
  a byte offset from the start of the surrounding text, not from the
  cursor as the v1 XML says, and its comment admits it
  (`components/exo/wayland/zwp_text_input_manager.cc` 201-213, Chromium
  revision ec0a1c15afb2, 2026-11-18). Chrome's own v1 client read it that
  way until crrev.com/c/8769988 (2026-11-11), which changed only the
  client. Exo derives the range from the input method's
  `ExtendSelectionAndDelete(before, after)`, so it always contains the
  selection (`components/exo/text_input.cc` 434-449), and sends no
  `commit_string` with it: an input method that replaces text, as when
  the user accepts a suggestion or picks a long-press diacritic, commits
  it separately. Autocorrect's undo replaces text through
  `ExtendSelectionAndReplace`, which Exo does not implement, so it sends
  nothing. The input method deletes only while nothing is composed
  (`ui/base/ime/ash/input_method_ash.cc` 1090-1102). So the proxy sends
  each delete at once, as a v3 transaction of its own with a `done` that
  ends any preedit, never holding it for a later commit, which might
  never come or might follow a cursor move. It measures the range against
  the surrounding text the guest last committed and turns it into bytes
  before and after the cursor. A negative index can only mean the v1
  XML's reading, which a host that follows it sends, so it counts from the
  cursor instead; a non-negative one is Exo's. An empty range is not
  forwarded. The proxy drops a delete rather than delete the wrong text,
  with a debug log, except that the first drop of a range not around the
  cursor on each text input is a warning:
  - while the guest has a selection: GTK and Qt count v3 lengths from the
    cursor, while Chromium's v3 client counts them from the selection's
    ends (`ui/ozone/platform/wayland/host/wayland_input_method_context.cc`
    802-825), so no lengths delete Exo's range in both;
  - for a non-negative index, while an edit of the input method that the
    proxy forwarded (a non-empty commit, the reset commit of a cleared
    preedit, a confirmation, a non-empty preedit region or a delete) is
    not yet in a surrounding text the guest committed
    (the field's text is `ExoText::Ahead`), since Exo measures against
    its prediction of the text, which includes the edit;
  - if the range does not contain the cursor, does not fit that text on
    character boundaries, or the guest stated no surrounding text, which
    means the two texts differ.

  The texts can still differ the other way: a guest commit that crosses
  an edit in flight states text without it, which the proxy takes as
  caught up while Exo replays the edit onto it, so the next delete may
  land wrong (see HANDOVER). Preedit regions are deliberately not gated
  by `ExoText::Ahead`: dropping one would leave Exo composing text
  the guest does not show, so a region right after an unreported edit may
  take its text from the guest's last report.

  Exo makes a region of the bytes around its own cursor in the text it
  predicts (`TextInput::SetCompositionFromExistingText`,
  `components/exo/text_input.cc`; `zwp_text_input_manager.cc` sends the
  index relative to the cursor), and that text holds its composition
  inline, with its cursor where the composition's is
  (`SurroundingTextTracker::OnSetCompositionText`,
  `ui/base/ime/surrounding_text_tracker.cc`), while the guest shows its
  preedit outside its text. So the proxy measures a region in Exo's text
  as far as it knows it (`ime::Current::predicted`): the guest's text,
  with the preedit Exo holds at the guest's cursor and Exo's cursor in it
  (`Composition::held`, where Exo sent that preedit's cursor, or
  its end). The guest's text is the committed surrounding text without
  the bytes around its cursor that preedit regions took since the guest
  last reported (`Composition::taken`; only the guest's
  committed surrounding text, or an edit that ends the composition and
  gives the bytes back as text, a preedit cleared with a reset commit
  included, clears it, and any other host `preedit_string` does not). Exo
  holds every preedit the proxy showed but a region's whose bytes the
  guest has since reported without: no state Exo predicted names that
  report, so Exo drops its composition (E4 of `docs/IME_REDESIGN.md`).

  A region must cover the preedit Exo holds; it deletes only the bytes
  beyond it on each side, which the guest has, and shows them with that
  preedit. An empty region at Exo's cursor after that preedit ends Exo's
  composition and leaves the preedit as text, since nothing filters an
  empty range (`InputMethodAsh::SetCompositionRange` and
  `SetComposingRange`,
  `TextInput::SetCompositionFromExistingText`) and the tracker keeps an
  empty composition (`OnSetCompositionFromExistingText`): the guest
  commits the preedit, as at a confirmation. Any other region that does
  not cover it, an empty one with Exo's cursor inside the preedit and a
  region short on either side included, is dropped with a debug log: Exo
  would leave the rest of that preedit as text, which only a commit after
  the region's delete could give the guest. A guest report that crosses the
  transaction clears `taken` early, the same race as above.
  Tests: `exo_deletes_become_v3_deletes_around_the_cursor`,
  `a_region_over_a_region_deletes_only_what_the_guest_has`,
  `a_region_after_a_host_preedit_deletes_only_what_the_guest_has`,
  `a_region_after_a_reset_commit_deletes_the_bytes_it_gave_back`,
  `a_region_over_a_host_preedit_is_measured_from_exos_cursor`,
  `a_region_that_does_not_cover_a_host_preedit_is_dropped`,
  `an_empty_region_over_a_held_preedit_commits_it`,
  `an_empty_preedit_has_no_hidden_cursor`,
  `a_region_equal_to_the_shown_one_sends_only_the_preedit`,
  `a_region_that_does_not_cover_the_shown_one_is_dropped`,
  `a_region_over_a_region_deletes_the_rest_on_both_sides`,
  `a_delete_waits_for_the_guest_to_report_the_input_methods_edits`,
  `a_reset_commit_waits_for_the_guest_to_report_it`,
  `delete_lengths_read_both_conventions`,
  `an_unconvertible_delete_is_warned_of_once`.
- **Every commit that leaves a field enabled gets a `done`.** The proxy
  answers each such v3 `commit` with a `done` that carries its serial, as
  mutter and KWin do. The protocol has a client send its surrounding text,
  content type and cursor rectangle only once a `done` names its latest
  commit, and Chromium does so: without this `done` it would hold them
  until the input method's next event, and Exo's input method would place
  its candidate window and compose against a stale caret and text. A
  `done` applies the preedit sent since the one before, or none, so the
  proxy first repeats the preedit the guest shows, with the cursor it was
  sent with (`Composition::shown`); GTK, Qt and Chromium
  would otherwise drop the composition. That preedit must be the one the
  host is still composing, so only a commit that changes whether the
  field is enabled, or an enable the proxy ignores, starts a new field,
  which ends the composition and the Backspace repeat leased to it and
  tells the host its content type again. An enable of an enabled field
  keeps the composition, as mutter and KWin keep the preedit across it,
  and the host keeps the content type, since before the v8 extension a
  resend would reset its composition. GTK3 sends one on a click's
  release in the field, after its press; a press on the field's surface
  has already ended the composition (next rule), so the composition it keeps is one
  a click elsewhere, such as on a subsurface, left running. A commit that disables
  the field, and a commit while unfocused, get no `done`, as from mutter;
  nor does an enable ignored because another text input on the
  connection is enabled, which leaves the field disabled. A preedit the
  answer cannot repeat is forgotten, since its bare `done` clears it. Host IME events reach the
  guest as whole transactions, so this `done` never splits one, and it
  leaves `ExoText::Ahead` alone: only the surrounding text a commit
  states clears that. Unlike mutter and KWin, the proxy does not end the
  host's composition when the guest resets its own; only a pointer press
  or a touch-down does (next rule).
  Tests: `every_commit_of_an_enabled_field_gets_a_done`,
  `a_commit_during_composition_keeps_the_preedit`,
  `reenabling_an_enabled_field_keeps_its_composition`,
  `only_an_enable_that_changes_the_field_closes_its_composition`,
  `an_answer_that_cannot_repeat_the_preedit_forgets_it`,
  `a_client_that_waits_for_done_reports_the_text_an_edit_leaves`,
  `on_set_preedit_region_translates_correctly`,
  `second_text_input_enable_on_the_connection_is_ignored`,
  `oversized_surrounding_commit_does_not_partially_advance_state`,
  `ime_reaches_the_specified_text`, `ime_reaches_the_intended_text`.
- **A press ends the composition on its surface.** When a host
  `wl_pointer.button` press comes from a pointer whose `enter` named the
  surface an active text input is focused on, or a host `wl_touch.down`
  names that surface, while the guest shows a preedit, the proxy ends
  that composition before the guest gets the press or the down
  (`text_input::end_composition_on_press`, the one entry point of both
  `pointer.rs` and `touch.rs`, hands it to the IME core as
  `ime::Input::Press`, which the gate `ime::TextInput::current`
  decides). The guest gets one transaction
  that clears the preedit, commits its text and ends with `done` carrying
  the current commit count, then the press or the down; the host gets
  `zwp_text_input_v1.reset`. Otherwise Exo keeps composing: the next jamo
  joins the old syllable, or the old preedit reappears at the click or
  tap point. A press or touch-down with no composition, or on another
  surface (a subsurface included), and a release change nothing. The
  rule assumes a single seat: a press on any seat ends the composition of
  the text input focused on the pressed surface, whatever that text
  input's seat. Exo exposes one seat. mutter matches the press's seat to
  the text input's, which with one seat comes to the same. This is what
  the reference compositors do:
  - mutter handles a button press or touch-begin before delivering it
    (`src/wayland/meta-wayland-seat.c` 507-511). If the surface picked
    under the pointer or the touch point is the text input's, it calls
    `clutter_input_focus_reset` and `meta_wayland_text_input_focus_flush_done`
    (`src/wayland/meta-wayland-text-input.c` 1208-1238). That reset
    commits the preedit when the input method composes in commit mode, as
    ibus-hangul does, then clears it and resets the input method
    (`clutter/clutter/clutter-input-focus.c` 108-128). The client gets
    `preedit_string(NULL)`, `commit_string` and `done(serial)`. Revision
    mutter fe9693a72c78.
  - KWin, on a button press on the active window
    (`InputMethodEventFilter::pointerButton`, `src/input.cpp` 2060-2073),
    or a touch-down whose toplevel is the active window
    (`InputMethodEventFilter::touchDown`, 2077-2089), commits the
    preedit's commit text with a preedit clear and `done`, then sends the
    input method `reset` (`InputMethod::commitPendingText`,
    `src/inputmethod.cpp` 225-235). Revision KWin f213b9afd80b.

  The proxy commits the preedit the guest shows, because Exo sends the
  same text as the preedit's commit text
  (`components/exo/wayland/zwp_text_input_manager.cc` 176-179). Exo's
  `reset` discards the composition and sends nothing back. Its handler
  (599-601) calls `TextInput::Reset` (`components/exo/text_input.cc`
  152-156), which cancels its tracked composition and calls
  `InputMethodAsh::CancelComposition`
  (`ui/base/ime/ash/input_method_ash.cc` 410-415). That resets the engine
  without confirming or clearing anything on the client. Revision Chromium
  faf801e427d0. So the `reset` itself commits nothing, and the guest's
  commit is the only commit of that text that the reset causes.

  The commit is an input-method edit the guest has yet to report
  (`ExoText::Ahead`), and it ends the Backspace repeat leased to
  the field, as a confirmation does. The press is re-sent after the
  transaction rather than forwarded, since a forward precedes its
  message's other events; the rest of its host batch follows it in order
  and unchanged. So the transaction may land inside a `wl_touch.frame`
  group, after touch events of that group the guest already got. That is
  legal: a frame groups only `wl_touch` events. The proxy tracks each
  host pointer's surface in `PointerFocus` from its `enter` until a
  `leave` of that surface, the pointer's end or the surface's end. A
  `leave` of another surface, which a host can deliver late, changes
  nothing, as for a keyboard. The dispatcher drops an `enter` or
  `leave` naming a surface the guest destroyed
  (`ShadowTable::event_object`). A `wl_touch.down` names its surface, so
  touch needs no such tracking.

  Events Exo sent before it handled the `reset` still arrive, and they
  belong to the composition the press committed. So the `reset` goes
  behind a fence, a `wl_display.sync`
  (`InternalCallback::TextInput`, whose barrier token the text input
  keeps as its fence). Exo handles requests in order. So the
  sync's `done` comes after every event Exo sent before it handled the
  `reset`, and before any event of a request sent later, a later
  `activate` included, so the fence needs no generation of its own.
  Until the fence returns, every host composition event of the field is
  dropped: a preedit, a preedit region, a commit, a delete or a
  confirmation. One gate, `ime::TextInput::current`, makes each one's
  plan. Why each is dropped:
  - A preedit or a preedit region would show text Exo no longer composes,
    which a second press would commit again. A region would also delete
    text the guest already has.
  - A commit of the pressed text is the input method's own commit of
    that composition, as when Space ended the syllable just before the
    click.
  - A commit of other text was made for that composition too: it changed
    after the preedit the press committed, as from `가` to `각`, and was
    then committed. Forwarding it would add the syllable a second time.
  - A delete was measured against that composition.
  - A confirmation confirms it. With nothing left to confirm, it would
    stand for a held key the input method took, and the guest would get
    that key.

  The proxy cannot tell those events from new text typed within the
  round trip, such as a `.`, so that text is lost, as is any key the
  input method took within it. Losing a keystroke typed during the click
  was preferred to inserting text twice.

  The fence cannot be a drain barrier: `deactivate` makes Exo confirm
  the composition itself, and until the next `activate` attaches the
  input method, keys would reach the guest without it. A fence ends only
  when its callback returns (`assert_no_orphans` checks that each one
  names a pending callback). A focus change or the text input's end does
  not end it early: requests sent later reach Exo after it.

  The fence covers the events already on the wire when Exo handled the
  `reset`, not results the input method had yet to produce. The review
  of this rule found that Chrome's `CancelComposition` does not cancel a
  key the engine is still processing (`ProcessKeyEventDone`). So an
  engine commit for a key pressed right before the click can still
  arrive after the fence, and the guest then gets the syllable twice.
  The fence narrows the window; it does not close it, and no heuristic
  filter tries to.

  Tests: `a_press_on_the_composing_surface_commits_the_preedit`,
  `a_press_elsewhere_or_without_a_composition_changes_nothing`,
  `a_press_keeps_its_place_in_its_batch`,
  `a_leave_of_another_surface_keeps_the_pointer_on_its_surface`,
  `a_commit_in_flight_at_a_press_is_dropped`,
  `a_preedit_in_flight_at_a_press_is_dropped`,
  `a_preedit_region_in_flight_at_a_press_is_dropped`,
  `a_delete_in_flight_at_a_press_is_dropped`,
  `a_confirm_in_flight_at_a_press_is_dropped`,
  `a_reset_fence_ends_before_the_next_generation`,
  `a_reset_fence_ends_with_its_text_input`,
  `pointer_focus_ends_with_its_surface_or_pointer`,
  `a_touch_down_on_the_composing_surface_commits_the_preedit`,
  `a_touch_down_elsewhere_or_without_a_composition_changes_nothing`,
  `a_touch_down_during_a_reset_fence_sends_no_second_reset`,
  `a_touch_down_keeps_its_place_in_its_batch`,
  `ime_reaches_the_specified_text`, `ime_reaches_the_intended_text`.

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
  accept loop. If that task ever ends, an error is logged; no test covers
  that log.
  Tests: `sighup_after_process_preparation_reaches_the_reload_handler`,
  `a_real_sighup_reloads_the_shortcuts_file`,
  `a_no_op_reload_keeps_the_proxy_and_its_reloads`,
  `reload_does_not_wait_for_the_accept_retry_pause`.
- **The socket belongs to the process holding its lock.** Like libwayland,
  the proxy takes `flock(LOCK_EX | LOCK_NB)` on `<socket>.lock` for its
  lifetime and only then removes a stale socket. If another process holds
  the lock, the proxy exits and leaves that socket alone.
  Test: `a_held_socket_lock_protects_the_live_socket`.
- **A failed start is logged, not a panic.** When the proxy cannot start
  (no `XDG_RUNTIME_DIR`, no device or compositor, a socket it cannot
  claim or bind), it logs why and exits with status 1. No test covers
  the device or compositor case.
  Tests: `a_missing_runtime_directory_is_a_logged_startup_error`,
  `an_unbindable_socket_is_a_logged_startup_error`, and, for a socket
  another process holds, `a_held_socket_lock_protects_the_live_socket`.

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

Under the remote-shell-v2 placement backend, the ID also reaches the
toplevel's remote surface.

Tests: `set_app_id_creates_zaura_surface_and_sets_app_id`,
`dbus_application_id_is_namespaced_on_aura_surface`,
`a_remote_shell_toplevel_is_a_facade_on_a_remote_surface`,
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
