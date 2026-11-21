# Runtime invariants

These rules are binding, and so are the IME rules of
[IME.md 2.6](IME.md#26-rules), which this document lists by title. Each
exists because breaking it caused, or would cause, a real failure: a
frozen or disconnected client, a leaked descriptor, stale pixels, or lost
IME input. A change that breaks a rule changes the rule and its tests in
the same pull request.

Each rule states what holds and why, and cites at most three tests that
fail when it breaks; `scripts/check-cited-tests.py` checks that each
exists. [DESIGN.md](DESIGN.md) explains the structure the rules apply
to, and the module docs the mechanism that keeps them. The window
placement contract is owned by
[WINDOW_SHORTCUT_CONFIGURATION.md](../sommelier/docs/WINDOW_SHORTCUT_CONFIGURATION.md).

## Transport and file descriptors

- **Every descriptor has one owner.** A descriptor the proxy holds is one
  `OwnedFd` from the receive that installed it to the send that passes a
  copy on, and whoever holds it last closes it by dropping it, so a
  message that is dropped, suppressed or rejected closes its descriptors
  ([Descriptors](DESIGN.md#descriptors) names the owners). Only the kernel
  and device boundaries (the `recvmsg` decoder, `virtwl_channel.rs` and
  the allocator's fence export) take over or close a raw descriptor;
  `sommelier/clippy.toml` rejects the raw-descriptor calls elsewhere. A
  raw number closed twice, or never, closes another object's descriptor
  or leaks one.
  Tests: `drops_unconsumed_received_fds`,
  `context_drop_closes_queued_and_pending_fds`,
  `early_output_cleanup_preserves_incoming_fd_ownership`.
- **Descriptors follow the ordered stream, not byte-buffer boundaries.**
  One `sendmsg` or VirtWL transaction may carry several messages, and a
  descriptor may belong to a later message whose bytes have not arrived,
  so a pending descriptor after a complete fd-free message does not end
  the connection by itself. Descriptors attached to a complete message
  that takes none are a protocol-fatal ambiguity, closed when the
  connection ends.
  Tests: `untracked_message_fd_is_retained_when_followup_is_partial`,
  `untracked_message_fd_ambiguity_survives_separate_receive_calls`.
- **Truncated ancillary data is an ownership failure.** If `recvmsg`
  reports `MSG_CTRUNC`, every descriptor the kernel installed for that
  receive is recovered and closed before the error is returned, not left
  to a wrapper that refuses to iterate truncated control data. The
  receive buffer has room for Linux's `SCM_MAX_FD` (253), so a valid peer
  never causes truncation.
  Tests: `recvmsg_scm_rights_truncation_closes_kernel_installed_descriptors`,
  `truncated_scm_rights_closes_descriptors_recovered_from_control_buffer`.
- **Received descriptors are bounded.** A connection holds at most
  `MAX_PENDING_FDS` (1024, libwayland's incoming descriptor ring)
  received descriptors that no message has consumed; one more ends that
  connection. The bound is per connection, so `main` raises the soft
  `RLIMIT_NOFILE` to the hard limit to leave room for many clients at
  their bound.
  Test: `too_many_unconsumed_fds_end_the_connection`.
- **Coalesced sends keep the order.** The output of consecutive batches
  may share one transport submission, because descriptors belong to the
  ordered stream, not to a byte offset; [Fair dispatch](DESIGN.md#fair-dispatch)
  says when it is sent. Each peer gets the same bytes and descriptors in
  the same order as with a send per batch, and a batch's output waits
  behind no batch in the other direction, so a key event never waits
  behind the rest of a guest's render burst. Per-message sends add VirtWL
  ioctls, which add latency and regress input throughput; introduce them
  only for a transport constraint a test or device contract demonstrates.
  Tests: `a_burst_of_batches_reaches_the_host_in_one_submission`,
  `a_host_batchs_output_reaches_the_guest_before_the_next_guest_batch`,
  `a_one_way_stream_sends_every_few_batches`.
- **A submission carries at most 28 descriptors and ends at a message
  boundary.** 28 is the VirtWL transaction limit (`VIRTWL_SEND_MAX_ALLOCS`)
  and libwayland's `MAX_FDS_OUT`. A submission also holds at most
  `MAX_BYTES_PER_SEND` bytes (3,980, a VirtWL transaction's payload)
  unless it is one longer message, which goes alone. So the receiver never
  meets a message whose descriptors have not arrived, and Exo's
  libwayland, which reads at most 4 KiB at a time, never dispatches the
  parts of one message in different reads. A message with more than 28
  descriptors, which no Wayland message has, is refused, and the session
  ends as after a failed send.
  Tests: `batches_send_at_most_the_transport_descriptor_limit_at_once`,
  `send_messages_ends_submissions_at_message_boundaries_within_the_byte_limit`,
  `a_message_with_more_descriptors_than_a_submission_is_not_sent`.
- **VirtWL ioctl errors still own their descriptors.** Every descriptor
  returned in a transaction buffer belongs to the proxy, even when the
  ioctl reports an error after partly writing the buffer, so error paths
  close and de-duplicate every non-negative one. The ChromiumOS kernel
  returns none on error; this guards against other devices.
  Tests: `returned_ioctl_fd_is_closed_on_error_paths`,
  `recv_ioctl_error_closes_descriptors_written_before_failure`,
  `raw_fds_are_closed_once_each`.

## Wire messages

- **One owner for the header.** Production code reads and writes the
  8-byte header only through `wire::MessageHeader`, and
  `MessageHeader::frame` alone decides whether a buffered message is
  complete: an incomplete one waits for the next receive, and an invalid
  length ends the connection. A message is at most 4096 bytes in both
  directions (`MessageHeader::MAX_LEN`, libwayland's
  `WL_MAX_MESSAGE_SIZE`), since libwayland refuses a longer one from
  either peer; the proxy builds none.
  Tests: `run_receives_the_rest_of_a_partially_buffered_message`,
  `header_length_validity_matches_the_wire_limits`,
  `a_request_over_4096_bytes_is_an_invalid_method`.
- **Messages come from the XML.** Production code builds every message
  it sends with a generated encoder, `<interface>::req::<request>` or
  `<interface>::evt::<event>`, which takes the arguments in XML order
  and type. A request names host IDs and an event guest IDs, and each
  peer's queues take only its own, so a guest ID in a request, or a
  message for the wrong peer, does not compile. `sommelier/clippy.toml`
  keeps `MessageBuilder` to generated code and to tests that build
  malformed frames.
  Tests: `encoders_match_hand_built_scalar_messages`,
  `encoded_messages_decode_to_the_same_arguments`,
  `test_generate_typed_encoders`.
- **Host data is untrusted.** An encoder of a message with a string or
  an array returns a `Result`, because peer data can exceed the wire's
  length limit; every other encoder cannot fail. A message the proxy
  builds itself that does not fit is dropped without panicking, and the
  session goes on. A forwarded message that does not fit is a
  `MessageTooLarge` error that ends the session, never a truncation. A
  message that cannot be built closes its descriptors.
  Tests: `oversized_host_text_is_dropped_without_panicking`,
  `generated_forwarding_rejects_messages_that_exceed_wire_length`,
  `an_encoder_that_cannot_build_its_message_closes_its_descriptors`.
- **A string that is not UTF-8 does not end the session.** Wayland
  requires UTF-8, but libwayland does not check it, so a client that
  sets its title from arbitrary bytes works with other compositors.
  `WireMessage::read_string` replaces each invalid sequence, in both
  directions, with U+FFFD, and the proxy forwards and matches the
  replaced string: a replaced interface name or MIME type matches
  nothing. A string that other arguments index by byte offset
  (`BYTE_INDEXED_STRINGS`) is not replaced, since that would move the
  offsets; its handler drops it instead (G5 and H7' of
  [IME.md](IME.md#2-behavioural-specification)). A NUL inside a string
  is still an `invalid_method`, as libwayland rejects it. A replacement
  can triple a string's length, so a forward near the length limit can
  end the session as `MessageTooLarge`.
  Tests: `a_guest_string_that_is_not_utf8_reaches_the_host_with_replacement_characters`,
  `a_host_string_that_is_not_utf8_reaches_the_guest_with_replacement_characters`,
  `a_string_with_a_nul_inside_is_an_invalid_method`.
- **A rejected request is reported.** A guest request the proxy cannot
  frame, decode or dispatch ends the session after one `wl_display.error`
  on `wl_display`, as libwayland-server's `wl_client_connection_data`
  sends: `invalid_object` when the sender is unknown or destroyed and
  awaiting `delete_id` (checked before anything else of the message), and
  `invalid_method` for anything else, trailing bytes after the arguments
  included. libwayland-server ignores trailing bytes, but the proxy
  re-encodes requests and must not drop data silently.
  Tests: `rejected_requests_report_invalid_method_on_the_display`,
  `a_destroyed_sender_is_an_invalid_object_whatever_it_sends`,
  `unknown_client_sender_is_a_protocol_error`.
- **A session ends with one error, after its valid output.** Every
  session the proxy ends gets exactly one `wl_display.error`, as a
  libwayland client gets from `wl_resource_post_error_vargs`, and then
  nothing more is handled. The first protocol error wins; a host's
  forwarded error counts as it, with its diagnostic bounded; an internal
  failure, or a host error the guest cannot be given, is `implementation`.
  The messages before the ending one in its batch were valid and are
  sent first; nothing the ending message produced is sent. A session
  that lost a transport (a failed send or receive, or a closed peer)
  ends without an error.
  Tests: `a_session_gets_exactly_one_error`,
  `undecodable_request_is_reported_after_forwarding_earlier_requests`,
  `a_rejected_request_sends_nothing_of_its_own`.
- **An unreadable event of a host-only object is dropped.** A host event
  the proxy cannot read (an unknown opcode, one newer than its object,
  arguments that do not decode whole) from an object only the proxy
  uses, such as its own `zaura_shell`, reaches no guest object, so the
  proxy drops it, warning at the first, and the session goes on. One
  from an object the guest has ends the session.
  Tests: `an_unreadable_event_of_a_host_only_object_is_dropped`,
  `an_unreadable_event_of_a_guest_visible_object_ends_the_session`.
- **Nothing the proxy sends depends on hash order.** No message, and no
  state that later decides one, depends on the order of a hash
  collection; a message that makes the proxy act on several objects
  visits them in ascending order of a key the session decides, such as
  an ID. `sommelier/clippy.toml` therefore rejects `HashMap` and
  `HashSet`. So the same messages always make the same bytes, and a
  session replays exactly.
  Tests: `the_text_inputs_of_a_seat_follow_its_focus_in_id_order`,
  `a_host_format_reaches_every_guest_shm_in_id_order`,
  `keys_held_at_an_enter_are_numbered_in_the_hosts_order`.

## Protocol errors

A request that violates a protocol error rule is rejected with the
specified `wl_display.error` on the offending object (`ctx.reject`),
which ends the connection; silently dropping it leaves the client
believing its state was accepted and desynchronizes later requests.
This covers `wl_surface.attach` offsets, buffer scales and transforms,
`wp_viewport` rectangles, `wl_registry.bind`, and the SHM and
linux-dmabuf validation rules, each with its interface's declared error
code, and a diagnostic bounded so the error fits in a message.
Tests: `invalid_shm_requests_are_protocol_errors`,
`invalid_plane_requests_queue_declared_protocol_errors`,
`invalid_viewport_values_and_dead_surface_are_fatal`.

Deliberately left unenforced, matching Exo: `wl_surface.invalid_size`
(a buffer that is not a multiple of its scale), `wp_viewport.bad_size`
(a fractional source size without a destination) and
`wp_viewport.out_of_buffer`. Exo accepts all three, so enforcing any
would end Crostini apps that work today, with nothing on screen to say
why. The proxy forwards such a commit and logs the first viewport
violation on each surface.
Tests: `viewport_source_must_fit_the_transformed_scaled_buffer`,
`viewport_source_outside_the_buffer_is_logged_and_forwarded`,
`fractional_source_without_destination_is_logged_and_forwarded`.

## Object lifetimes

- **A guest ID never stands for a host one.** Only the object table
  (`state::objects`) makes a `GuestId` or a `HostId`, and handlers get
  their sender and ID arguments as the table resolved them, so code that
  takes a guest ID for a host one, or the other way round, does not
  compile.
  Tests: `test_handlers_get_typed_senders_and_objects`,
  `requests_resolve_live_objects_of_the_expected_interface`.
- **Destructors wait for `delete_id`.** A destructor request keeps its
  guest/host pairing until the host's matching `wl_display.delete_id`:
  meanwhile the guest object rejects new requests and stale host events
  do not reach its handler, but the host ID stays resolvable, so the
  `delete_id` is translated and forwarded. A local object's destructor
  completes in the proxy. A handler cannot drop a destructor and leave
  its object live ([Object ends](DESIGN.md#object-ends)).
  Tests: `destructor_mapping_survives_until_host_delete_id`,
  `test_generate_wayland`.
- **An ID is reused only once the peer has forgotten it.** An ID the
  proxy allocated on the host connection becomes reusable only at the
  host's `delete_id`; reused earlier, a late event of the old object
  would name the new one. A `delete_id` for a live object only the proxy
  uses, or for an ID it does not track, changes nothing, so a
  misbehaving host cannot hand a live object's ID to a new one. A host ID
  whose object never reached the host returns at once. An ID the host
  allocated gets no `delete_id` and stops being reserved when the proxy
  destroys the object.
  Tests: `host_ids_are_not_reused_before_delete_id`,
  `a_delete_id_for_a_live_host_only_object_changes_nothing`,
  `an_abandoned_object_frees_both_ids`.
- **The host may end a guest's object.** Compositors destroy the pending
  frame callbacks of a destroyed surface, so a host `delete_id` of a live
  guest object ends it and reaches the guest. A guest request from it
  after that, a destructor included, is an `invalid_object`, as
  libwayland-server answers a request to an ID whose resource it
  destroyed.
  Test: `a_host_destroyed_frame_callback_reaches_the_guest`.
- **A dropped message maps no `new_id`.** Generated
  dispatchers create a `new_id`'s object only once the message's sender
  and every object argument have resolved, so a message dropped for a
  local sender or argument, or for an event object with no guest
  counterpart, maps nothing and consumes no ID; otherwise the two peers'
  ID maps would disagree. A handler that maps its request's `new_id`
  itself returns `Drop`.
  Tests: `request_dropped_for_a_local_argument_maps_no_new_id`,
  `event_from_an_unmapped_sender_maps_no_new_id`,
  `test_no_forward_drops_after_mapping_a_new_id`.
- **ID spaces stay dense and within libwayland's map.** Freed IDs are
  reused first, as libwayland's object map does, and every ID stays
  within its limit, `WL_MAP_MAX_OBJECTS` (`0xF00000`): the ID on the
  client side, its offset from `0xff000000` on the server side. A
  `new_id` past it, a reused one, or a host `new_id` from the client
  range ends the connection, as libwayland refuses it. A client that
  exhausts a range ends alone.
  Tests: `host_ids_are_recycled_through_delete_id`,
  `host_new_ids_must_be_fresh_server_ids`,
  `host_ids_stay_within_libwayland_servers_client_range`.
- **An object's state ends with it, once.** Every per-object state ends
  at the object's first end: a guest destructor, a terminal event, a
  local destructor, an abandoned bind, or a host `delete_id` of a live
  object, which sends nothing to the deleted object. The `delete_id` that
  acknowledges a destructor forgets nothing again, and a render buffer,
  which outlives its guest object while the host shows it, ends when the
  host releases its ID. The teardown joins the current message's effects
  in its destructor's wire order: a surface's or toplevel's Aura objects
  are released before its destructor, and a surface's text-input `leave`
  and buffer retirement follow it. State that outlives its object is
  read under a reused ID. [Object ends](DESIGN.md#object-ends) says where
  such state lives and how its end is checked.
  Tests: `a_host_destroyed_surface_is_forgotten`,
  `a_delete_id_after_the_destructor_forgets_nothing_again`,
  `wl_surface_destroy_releases_its_aura_children_first`.
- **Host-only objects keep their ID reservation.** A host-only object
  with a wire destructor keeps its host ID reserved until the host's
  `delete_id`; its dispatch metadata may go at once, and the display
  handler consumes the acknowledgment without forwarding it.
  `Context::retire_destroyed_host_object` is the one way to retire such
  an object, and a refused retirement is fatal. An object with no wire
  destructor stays reserved for the connection.
  Tests: `host_only_destructor_reservation_survives_until_delete_id`,
  `retiring_a_destroyed_host_object_requires_a_live_host_only_object`,
  `retiring_a_host_object_awaiting_delete_id_keeps_its_id_reserved`.
- **Synthetic objects complete their own lifecycle.** A guest object
  implemented in the proxy (such as `wl_shm_pool` or
  `zwp_text_input_manager_v3`) retires its dispatch metadata and sends
  exactly one guest `wl_display.delete_id` at its destructor, and its ID
  is not reused before that; one with capability or event-routing state
  drops it first. Forwarding the destroy to a nonexistent host object, or
  dropping it, leaks the guest object. A synthetic global without a
  destructor (`wl_shm`) stays reserved for the connection.
  Tests: `synthetic_manager_destroy_emits_one_guest_delete_id`,
  `synthetic_shm_pool_destroy_emits_one_guest_delete_id`,
  `synthetic_feedback_destroy_completes_locally`.
- **A removed backing global leaves synthetic objects destroyable but
  inert.** When the host global behind a synthetic object is removed, the
  objects bound from it, and their children, still accept their
  destructors and ignore every other request. A request that creates an
  object still creates it, because the guest owns its ID, but inert: a
  `wl_shm_pool` that ignores its requests, or a `wl_buffer` that attaches
  as no buffer. A guest `wl_shm` bound from a replacement name keeps
  working.
  Tests: `removed_host_shm_creates_only_inert_pools_and_buffers`,
  `only_objects_bound_from_a_retired_shm_global_go_inert`,
  `attaching_an_inert_buffer_detaches_the_surface`.
- **Releasing a parent does not end its children.** `wl_seat.release`
  forwards the seat's destructor without tearing down the keyboards, key
  routing or IME state its child devices own. A keyboard or text input
  stays a member of the seat it was created from after the seat ends, and
  a `wl_seat` the guest later binds under the same ID takes it over (Exo
  has one seat). Until then the guest may reuse the ID for any object, so
  a text input names it to the host as its seat only while it is a live
  `wl_seat`.
  Tests: `seat_release_does_not_break_live_child_keyboard_press_release_pair`,
  `release_does_not_end_child_text_input_focus`,
  `a_seat_id_reused_for_another_object_is_not_sent_as_a_seat`.

## Registry globals

- **`global_remove` invalidates only the advertised name.** Objects
  already bound, and their children with lifetimes of their own (such as
  `zcr_extended_keyboard_v1` or `zaura_surface`), stay mapped and usable
  until their own destructor; a replacement global may coexist with them.
  The exception is the host-only `zaura_output` of a removed `wl_output`,
  which the proxy releases at once with the output's record, because
  nothing reads them again and a `wl_output` before v3 has no release
  request that could free them later.
  Tests: `keyboard_extension_global_remove_keeps_existing_children_alive`,
  `global_remove_keeps_aura_children_until_surface_destroy`,
  `global_remove_forgets_the_output_and_releases_its_aura_child`.
- **A bind that crosses its global's removal is not an error.** A client
  can bind a global it saw advertised after the compositor removed it,
  because the bind and the `global_remove` cross on the wire, and
  libwayland-server makes that harmless. So a bind of a `wl_output` name
  the registry advertised and then removed makes an inert guest object
  that never reaches the host and ends locally, and a request that names
  it reaches the host with a null output; the guest drops it at the
  `global_remove` it gets on the same registry. A name never advertised,
  a wrong interface or version, and a removed global of another
  interface stay protocol errors.
  Tests: `bind_crossing_an_output_removal_creates_an_inert_output`,
  `inert_output_release_completes_locally`,
  `an_invalid_bind_is_a_protocol_error`.
- **A removed global waits for every registry that lists it.** Its
  metadata, and the internal binding it backs, stay until every registry
  that listed it has been sent its `global_remove`; then a bind of the
  name is refused through any registry. A registry destroyed through
  `wl_fixes` before it sees the removal no longer counts. A bind through
  a registry that has not seen the removal is forwarded, which relies on
  the host keeping a removed global bindable until `wl_global_destroy`,
  as Exo does.
  Tests: `destroying_the_last_registry_to_list_a_removed_global_retires_it`,
  `a_removal_outlives_the_registry_that_saw_it`,
  `destroying_a_registry_keeps_a_global_no_registry_saw_removed`.
- **A global name is one global for the connection's life.**
  libwayland-server numbers each global anew, advertises it to each
  registry once, and never advertises a removed one, so the proxy
  identifies a global by its name alone. A `global` event for a name
  already advertised to that registry, or whose removal any registry has
  seen, breaks the protocol and ends the session as an internal failure.
  Test: `a_name_advertised_again_ends_the_session`.
- **A singleton binding moves to its replacement.** The proxy binds
  `wl_shm`, `zwp_linux_dmabuf_v1`, `zcr_keyboard_extension_v1` and
  `zaura_shell` once per connection for its own use. When the bound name
  retires, it binds the lowest name a registry still lists and no
  registry has seen removed, since a compositor may destroy a global as
  soon as it removes it.
  Tests: `a_singleton_advertised_anew_is_bound_when_the_old_name_retires`,
  `a_singleton_binding_moves_to_the_lowest_live_candidate`,
  `a_promoted_dmabuf_publishes_its_deferred_guest_global`.
- **Versions come from the XML or a documented constant.** Since-version
  checks use the generated `*_SINCE` constants. A global's version bound
  is the generated `VERSION` unless a named constant in the one table of
  globals (`handler::registry::policy`) documents a deliberate
  difference. No offered version exceeds the generated `VERSION`, so a
  host newer than the vendored XML cannot make the guest send requests
  the proxy cannot decode. A child object takes its parent's version, and
  a version the proxy cannot find fails closed: no child gets a guessed
  one, and a feature gated on it is off.
  Tests: `child_objects_inherit_their_manager_versions`,
  `an_extension_of_unknown_version_gives_no_input_method`,
  `every_interface_has_one_global_policy`.

## Surfaces, damage and SHM

- **Copy before commit.** Damaged pixels are copied from the guest buffer
  into its host storage before the host `wl_surface.commit` is queued,
  so the host never samples stale pixels. If the guest's pool cannot
  supply them (a short read, as after the client truncated the file),
  the commit queues `wl_shm.invalid_fd` on the `wl_buffer` and the
  session ends, as with libwayland's SIGBUS guard. If the host storage
  fails, the commit is dropped and the next one copies in full. A buffer
  attached and then destroyed before the commit keeps its backing until
  that commit.
  Tests: `commit_from_a_truncated_pool_is_a_protocol_error_not_a_crash`,
  `copy_into_an_unwritable_destination_blames_the_host_storage`,
  `destroyed_pending_shm_attach_survives_until_commit`.
- **Pools are read, never mapped, and must be regular files.** A mapping
  read past a truncated file's end raises SIGBUS and kills every guest
  GUI app, so the proxy reads pool pixels with `pread` and `preadv`. A
  pool fd must be a regular file, such as a memfd, of at least the
  declared size; any other is `wl_shm.invalid_fd` at `create_pool` and
  `resize`.
  Tests: `pool_backing_must_be_a_long_enough_regular_file`,
  `create_pool_rejects_a_pipe_or_a_short_file_as_invalid_fd`,
  `a_pool_resize_must_keep_or_grow_its_size_within_its_file`.
- **A copy writes only the damaged bytes.** The host may sample a host
  copy while the proxy writes it, so the copy writes no byte outside the
  damage; bytes a batched read takes between damaged rows go to scratch
  memory (`HostMapping::pread_rows_into`).
  Tests: `pread_rows_into_writes_exactly_the_rows`,
  `pread_rows_into_reads_many_rows_at_once`,
  `a_partial_width_shm_frame_costs_one_read`.
- **Without VirtWL, guest SHM is a memfd.** With `--local-compositor`,
  each guest `wl_buffer` gets a memfd of its size, which the host imports
  as a `wl_shm` pool. The guest's `wl_shm` then offers an optional format
  only once the host's `wl_shm` lists it, not when only its linux-dmabuf
  does, or a strict host fails the buffer with `wl_shm.invalid_format`.
  Tests: `without_virtwl_a_buffer_is_backed_by_a_memfd_of_its_size`,
  `without_virtwl_guest_shm_offers_only_the_host_wl_shm_formats`.
- **Damage-only commits copy from a buffer the host holds.** A commit
  without a new `attach` still damages the buffer the host samples, so
  its damaged pixels are copied before the commit is forwarded. The
  exception is a buffer the host released: the client then owns the
  storage and may be drawing into it, so reading it could tear, and its
  damage is copied at its next attach. Exo releases a buffer only after
  it is detached, so with Exo every damage-only commit copies. A commit
  with nothing to copy skips the copy path, so a damage-free commit is
  never dropped by a host storage failure. Known limit: a release carries
  no serial, so a late owed release of a buffer attached elsewhere marks
  it released while the host holds it, until its next attach.
  Tests: `damage_only_commit_copies_the_mutated_current_shm_buffer`,
  `damage_only_commit_does_not_read_a_released_shm_buffer`,
  `commit_without_damage_on_a_held_buffer_skips_the_copy`.
- **Surface damage copies the pixels it touches.** Guests see
  `wl_compositor` v4 and the proxy keeps `damage_buffer` local over Exo's
  v3, so surface damage is mapped back to buffer pixels, rounded outward
  so no pixel whose image meets the damage is left out. Only a
  transformed buffer, or an attach offset, copies in full: a 4K buffer
  copied in full every frame costs about 2 GB/s.
  Tests: `surface_damage_maps_to_every_buffer_pixel_it_meets`,
  `destination_only_viewport_copies_only_the_damaged_pixels`,
  `surface_damage_under_a_buffer_transform_copies_the_whole_buffer`.
- **Each host copy tracks the pixels it lacks.** Damage is relative to
  the surface's current contents, so a client alternating buffers A, B, A
  damages only what changed since B, while each guest SHM buffer has its
  own host copy. Every commit therefore stales its damage in the host
  copies of all the surface's buffers (`StalePixels`), and a buffer copies
  everything it lacks before it is shown. A copy that cannot be related
  pixel for pixel (a new buffer, one new to the surface or of another
  size, after a failed copy or a change of scale, transform or viewport)
  copies in full.
  Tests: `reattached_buffer_copies_the_damage_of_every_frame_it_missed`,
  `a_frame_of_another_size_stales_the_whole_host_copy`,
  `a_mapping_change_stales_the_other_host_copies`.
- **Surfaces stay below version 5.** The `wl_compositor` ceiling is v4, so
  `attach` carries a surface's offset, and the generated version check
  rejects v5's `wl_surface.offset`, whose semantics the proxy does not
  implement; a compile-time assertion keeps the ceiling below v5.
  Test: `consumed_attach_offset_is_not_reapplied_on_later_commit`.
- **A non-zero offset forces full damage.** Surface damage cannot yet be
  translated through the copy map when the committed offset is non-zero,
  so such a commit sends complete host surface damage.
  Test: `nonzero_surface_offset_with_explicit_damage_forces_full_host_damage`.
- **Buffer damage reaches the host in surface coordinates.** Exo's
  `wl_compositor` v3 has no `damage_buffer` and reads `wl_surface.damage`
  after the buffer scale and the viewport, so the proxy maps buffer damage
  in the order of `viewporter.xml`: the buffer transform, the buffer
  scale, then the viewport crop and scale, growing each rectangle by one
  buffer pixel and rounding outward. A transformed buffer damages the
  whole surface. Mapping in another order puts the damage where nothing
  changed, and the host shows stale pixels.
  Tests: `buffer_damage_maps_through_buffer_scale_before_the_viewport`,
  `viewport_damage_uses_committed_crop_and_destination`,
  `rotated_buffer_damage_covers_the_transformed_surface`.
- **Damage is clamped.** Every edge of every rectangle lies within
  ChromiumOS' `INT_MIN / 10` to `INT_MAX / 10` headroom before it is
  encoded for the host, so the host's `x + width` cannot overflow; a
  rectangle stays at least 1x1.
  Tests: `extreme_surface_damage_is_clamped_before_host_encoding`,
  `host_damage_is_clamped_on_both_edges`.

## Dma-buf

- **Virtio-gpu PRIME imports use kernel metadata.** Before forwarding
  plane 0 of a classic virtio-gpu PRIME buffer, the proxy queries the
  local DRM resource metadata, and the kernel's stride and explicit
  modifier replace the guest's, which are stale for host import; other
  buffers keep the guest's values.
  Tests: `virtgpu_resource_metadata_replaces_guest_plane0_layout`,
  `virtgpu_layout_fixup_preserves_guest_metadata_on_fallback`.
- **Resource-info probing keeps two ABI sizes.** The capability probe
  uses the standard 16-byte `drm_virtgpu_resource_info` encoding and the
  queries the 56-byte ChromeOS `drm_virtgpu_resource_info_cros` one: the
  command number is shared but the size bits are not, so one constant for
  both makes a supported kernel look unsupported. The probe runs only
  after `VIRTGPU_PARAM_3D_FEATURES` succeeds.
  Tests: `virtgpu_resource_info_probe_and_extended_abi_sizes_remain_distinct`,
  `extended_resource_info_probe_requires_virtgpu_feature_query`.
- **Native buffers are fenced.** A commit of a guest dma-buf is forwarded
  only after the guest GPU's writes have completed; a failed check is
  fatal and never commits. The check never waits on the loop: a busy
  buffer parks the guest's stream before the commit changes anything,
  while host events and other clients go on
  ([Message effects](DESIGN.md#message-effects)), so the host sees the
  order the guest sent. A fence that does not signal within a second for
  a sync_file (ChromiumOS Sommelier's bound), or 15 s for the virtio-gpu
  fallback (the kernel's), ends the session with one `implementation`
  error.
  Tests: `a_commit_waits_for_a_pending_fence_before_it_changes_anything`,
  `a_pending_fence_parks_the_guest_stream_while_host_events_flow`,
  `a_fence_that_never_signals_ends_the_session_at_its_deadline`.

## Keyboard and IME

IDs such as E6, H2 and X2' name the rows and facts of
[IME.md](IME.md); [KEYBOARD.md](KEYBOARD.md) describes the key states.

- **Keymaps are mapped privately.** `wl_keyboard.keymap` descriptors are
  mapped read-only with `MAP_PRIVATE`, as the protocol requires, on both
  the VirtWL and memfd paths, even where a host allows `MAP_SHARED`.
  Test: `keymap_mmap_is_private_not_shared`.
- **Keymaps are bounded.** A host keymap larger than `MAX_KEYMAP_SIZE`
  (1 MiB; Exo's are about 60 KiB) is refused before it is mapped: a VirtWL
  keymap's size cannot be checked against its file, and the proxy copies
  a keymap to compile it, so a size near 4 GiB would abort every client.
  Like any keymap the proxy cannot use (one with a NUL before its end
  would panic xkbcommon), it clears the keyboard's XKB state, so no chord
  is classified until a usable one arrives, and it is still forwarded.
  Tests: `on_keymap_ignores_a_keymap_over_the_size_limit`,
  `an_unusable_keymap_clears_the_loaded_one_and_is_forwarded`.
- **A keymap ends no held key.** A keymap replaces only what was compiled
  from the one before it. The keys held, their generations and an
  accelerator's suppression stay, so each release goes where its press
  went. Otherwise a keymap that arrives while a key is held would send its
  release to the host while the guest holds and repeats the key, and Exo,
  which switches the layout on Ctrl+Space while Space is held, would let
  the IME hand Space to the guest as a recovered key.
  Tests: `a_new_keymap_keeps_held_keys_and_modifiers`,
  `a_rejected_keymap_keeps_held_keys`,
  `an_accelerator_held_across_a_keymap_is_not_recovered`.
- **IME state is scoped to its keyboard or seat.** Key generations,
  Backspace leases and focus markers belong to the keyboard or seat they
  came from, never to the process, and synthetic `wl_keyboard` events
  use the time domain of the real keyboard events, never wall-clock time.
  Tests: `ime_repeat_state_is_scoped_to_the_originating_keyboard`,
  `an_empty_confirmation_recovers_the_latest_eligible_held_key`.
- **Host keysyms and repeats are dropped.** A `zwp_text_input_v1.keysym`
  reaches no peer and changes no key state: Exo's one sender of it,
  `TextInput::InsertChar`, sends a key its input method handled with no
  `DomCode` as `commit_string` instead, and Exo never sends a keysym
  release, so a keysym delivered as a key would stay pressed in the
  guest. Whether Exo ever sends one is U1 of
  [IME.md 5.2](IME.md#52-unknowns-and-manual-host-checks), so the first
  of a process is logged at info level. A `wl_keyboard.key` or `peek_key`
  of any state but pressed or released, such as a repeat, is dropped
  unacknowledged: `wl_keyboard` has no repeat state below v10, the proxy
  binds `wl_seat` at v5, and Exo drops every repeat.
  Tests: `a_keysym_reaches_no_peer`, `a_repeated_key_state_reaches_no_peer`.
- **A recovered key carries the host's serial.** The press and release
  the guest gets for a held key the input method consumed carry the
  serial and time of the key's latest `peek_key`, which the extension
  defines as those of the `wl_keyboard.key` it announces, and which Exo
  caches until it sends one. A request that quotes the serial, such as
  `wl_data_device.set_selection`, is therefore accepted: Exo checks that a
  serial is among the last 1024 it issued, which a counter of the proxy's
  own would soon fall outside.
  Tests: `a_recovered_key_carries_the_latest_peek_serial`,
  `an_empty_confirmation_recovers_the_latest_eligible_held_key`.
- **An empty confirmation recovers a held key only once the host shows it
  repeated.** At extension v11 an empty `confirm_preedit` while a key the
  input method took is held is either that key's autorepeat or Exo's
  confirmation before a press it does not handle, whose `peek_key` and
  `wl_keyboard.key` follow in the same task (E13, E8). So the proxy
  recovers the key only once the host's next event shows the
  confirmation was not a new press's, with one `wl_display.sync` as its
  bound (E11), and only if the text input still passes the gate and the
  key is still held; otherwise a key typed while Backspace or Space is
  held would also repeat the held key. After Exo's own detach (H2) a
  held key's release may have died with the input method, so held keys
  are recovered no more until their next press, and while a keyboard of
  the seat holds such a key no empty confirmation recovers any key: a
  guessed Backspace would edit text, and a dropped autorepeat loses
  none. [The recovery decision](KEYBOARD.md#the-recovery-decision) owns
  the decision table.
  Tests: `a_confirmation_before_an_unhandled_press_recovers_no_held_key`,
  `a_confirmation_before_a_handled_press_recovers_the_held_key`,
  `a_confirmation_while_a_key_held_across_a_detach_recovers_nothing`.
- **A key waits for at most one late release.** A key pressed again
  before `wl_keyboard` released the guest's previous press keeps that
  release pending beside the new generation, so a late release still
  closes the old press; a newer retired release replaces an older one.
  Exo sends a release only for a key it sent pressed, right after its
  peek, so one retired release tolerates a host that delays a release
  past the key's next press, and a host that never sends one leaves one
  pending release per key, not one per press.
  Tests: `a_key_keeps_one_pending_release`,
  `a_retired_release_closes_its_press_across_the_serial_wrap`,
  `a_delayed_release_ends_only_its_generation`.
- **Host modifiers last as long as their keyboard.** Exo sends
  `wl_keyboard.modifiers` only when they change (and after each Num Lock
  press), before `enter` rather than after it, and none with a new keymap.
  So the proxy keeps each host keyboard's last modifiers through focus
  changes and keymaps, applies them to every keymap it compiles, and
  forgets them only when the keyboard ends; otherwise a chord pressed
  with a modifier held across a focus change or a layout switch would be
  classified without it. The protocol requires a `modifiers` after every
  `enter`, which Exo omits, so the proxy sends the keyboard's modifiers
  after each `enter` it sends.
  Tests: `modifiers_sent_before_enter_survive_it`,
  `modifiers_carry_over_to_a_new_keymap`,
  `every_forwarded_enter_is_followed_by_the_keyboards_modifiers`.

The IME rules are owned by [IME.md 2.6](IME.md#26-rules):

- **Content hints come from the v3 XML.** [IME.md 2.6](IME.md#26-rules)
- **A text input needs the host's extension at v11.** [IME.md 2.6](IME.md#26-rules)
- **`set_input_type` replaces v1 `set_content_type`.** [IME.md 2.6](IME.md#26-rules)
- **The host gets each content type once it can apply it, and only
  then.** [IME.md 2.6](IME.md#26-rules)
- **One text input on a connection is enabled at a time.** [IME.md 2.6](IME.md#26-rules)
- **Every v3 commit counts.** [IME.md 2.6](IME.md#26-rules)
- **Every commit that leaves a field enabled gets a `done`.** [IME.md 2.6](IME.md#26-rules)
- **A v3 transaction's events go in one order.** [IME.md 2.6](IME.md#26-rules)
- **A transaction carries no event that changes nothing.** [IME.md 2.6](IME.md#26-rules)
- **Exo's deletes are absolute and stand alone.** [IME.md 2.6](IME.md#26-rules)
- **Exo's deletes and regions are measured in the Exo mirror, or
  dropped.** [IME.md 2.6](IME.md#26-rules)
- **Focus teardown sends `leave` itself.** [IME.md 2.6](IME.md#26-rules)
- **A field the focus leaves commits its preedit first.** [IME.md 2.6](IME.md#26-rules)
- **A press ends the composition on its surface.** [IME.md 2.6](IME.md#26-rules)

## Pointer scrolling

- **Only a chosen client gets wheel steps for touchpad scrolling.** Exo
  sends a touchpad scroll as `wl_pointer.axis` with no `axis_discrete`,
  and Wine's Wayland driver scrolls only on `axis_discrete`. So a client
  that `client_kind::WheelFromTouchpad` chooses
  (`SOMMELIER_WHEEL_FROM_TOUCHPAD`; by default a Wine process) gets one
  `axis_discrete` step, signed like the scroll, per 10 units of finger or
  continuous scrolling on an axis (Exo's distance for one wheel notch,
  `kWaylandScrollUnit`), just before the `axis` that crosses it, as the
  protocol orders them. Every other client, and every wheel scroll, gets
  the events as the host sent them.
  Tests: `a_wine_client_gets_wheel_steps_from_touchpad_scrolling`,
  `wheel_steps_leave_other_clients_and_wheels_alone`,
  `a_stepper_carries_the_rest_over_per_axis`.
- **Nothing waits to learn whether a client is Wine.** The `/proc` read
  runs on a detached thread, because a stalled process can hold the lock
  its `environ` needs. The client's messages are handled from the start,
  and it scrolls as the host sent it until the read answers; a read that
  never answers stalls no one.
  Tests: `a_pending_detection_leaves_scrolls_alone_until_it_answers`,
  `a_client_scrolls_unchanged_until_its_detection_answers`.

## Process

The proxy serves every Linux GUI app, so no single client may end it.

- **One client's failure ends only that client.** Setting up an accepted
  client returns errors instead of panicking. A failed accept (such as
  EMFILE) pauses the accept loop for `ACCEPT_RETRY_DELAY` (100 ms)
  instead of spinning on the shared thread.
  Tests: `unregistrable_descriptor_is_an_error_not_a_panic`,
  `failing_accepts_back_off_instead_of_spinning`.
- **Nothing waits on a guest client or on host data.** Every client runs
  on the loop's one thread ([Process and connections](DESIGN.md#process-and-connections)),
  so a call that waits for a peer to act stalls them all. A descriptor the
  proxy waits on is non-blocking and polled through Tokio's `AsyncFd`,
  without changing the flags of a guest's open file description, which
  the client shares, and the dma-buf fence check never blocks. The VirtWL device is the
  exception the proxy cannot avoid: every ioctl but a send, and every vfd
  close, is a synchronous round trip to the host.
  `sommelier/clippy.toml` rejects the blocking calls Clippy can name.
  Tests: `a_slow_reader_parks_the_pump_without_blocking_the_loop`,
  `pumps_into_one_pipe_never_change_its_flags`,
  `a_pending_fence_parks_the_guest_stream_while_host_events_flow`.
- **A clipboard transfer is bounded.** A pump that moves no data for
  `PUMP_IDLE_TIMEOUT` (60 s), the wait for the first byte included, ends,
  because host data it does not read stays in the VirtWL receive queue
  every connection shares. The guest's pipe then closes as after a whole
  transfer, since a pipe cannot carry an error: the guest cannot tell a
  cut-off paste from a complete one, deliberately. A connection runs at
  most `MAX_CLIPBOARD_PUMPS` (16); a further `wl_data_offer.receive` is
  dropped, which closes the guest's descriptor, so the guest reads EOF.
  `handler::data_device` documents how a pump writes each kind of
  destination without blocking or changing the client's file flags.
  Tests: `a_transfer_the_host_stalls_ends_after_the_idle_timeout`,
  `a_transfer_the_guest_stalls_ends_after_the_idle_timeout`,
  `clipboard_pumps_are_capped_per_connection`.
- **A data-control selection is set by the focused connection, or
  cancelled.** Exo takes a selection only from the client whose surface
  has the keyboard focus, with a serial it issued recently, and a
  data-control client never has the focus. So with data-control on, the
  connection whose keyboard has the focus sets it for the client, with
  the serial of its own latest input event, through its own
  `wl_data_device`; without one, or without a focused connection, the
  source is cancelled at once rather than left waiting. The host reads
  the data through its own descriptor, which the client gets, so no
  transfer waits on the proxy. A selection ends with either connection:
  once the client's source ends, what the host asks of it is refused,
  which closes the host's descriptor, and the focused connection keeps
  its own source, which the host's late events still reach, until the
  host cancels it; once the focused connection ends, the client's source
  is cancelled. The hub's route is the one record of whether a selection
  is live.
  Tests: `a_data_control_selection_reaches_the_host_through_the_focused_connection`,
  `without_a_focused_connection_a_data_control_selection_is_cancelled`,
  `a_selection_ends_with_either_connection`.
- **A data-control device reads the clipboard only while a focused
  connection vouches for it.** Exo tells only the focused client the
  clipboard, with a new offer each focus, and an offer reads each type
  once and keeps it. So with data-control on, a device is told the
  clipboard that the connection whose keyboard has the focus learned
  since the focus came, and reads it through that connection's own host
  offer and bounded pumps; once that connection loses the focus, or its
  guest the offer, or another connection takes the focus without telling
  one, every device is told the clipboard is empty, and an offer of it,
  or a read already on its way, reads nothing, rather than paste what the
  clipboard may no longer hold.
  Tests: `a_data_control_device_reads_the_focused_connections_clipboard`,
  `the_clipboard_reads_empty_once_its_connection_cannot_vouch_for_it`,
  `a_focus_taken_without_a_clipboard_empties_it`.
- **A connection through a security context is offered no global that
  widens its sandbox.** A sandbox engine (Flatpak) hands the proxy a
  socket through `wp_security_context_v1` so that it can tell its
  clients apart; every client the proxy accepts on it is sandboxed, and
  is offered neither data-control, which would let it read or replace
  the clipboard without the focus, nor the security-context manager, so
  contexts never nest. The proxy accepts on the socket until the close
  descriptor signals anything, never reading it, so a hangup of either
  pipe end (Flatpak passes the write end) ends it and no byte written to
  it can block the loop; it serves at most `MAX_SANDBOX_LISTENERS` (64)
  contexts at once, and a commit beyond ends its session.
  Tests: `a_sandboxed_connection_is_offered_no_privileged_global`,
  `a_committed_security_context_hands_its_socket_to_the_proxy`,
  `a_security_contexts_socket_serves_until_it_hangs_up`.
- **The socket belongs to the process holding its lock.** Like
  libwayland, the proxy takes `flock(LOCK_EX | LOCK_NB)` on
  `<socket>.lock` for its lifetime and only then removes a stale socket;
  if another process holds the lock, it exits and leaves that socket
  alone.
  Test: `a_held_socket_lock_protects_the_live_socket`.
- **Recording is opt-in, private, and changes nothing.** Only
  `SOMMELIER_TRACE_DIR` turns it on ([recorded host traces](../traces/README.md)).
  Each connection then writes a file only the user can read, since it
  holds everything typed. A failed write, or a trace that reaches
  `MAX_TRACE_BYTES` (256 MiB), ends the recording, never the session, and
  recording changes nothing the proxy sends.
  Tests: `each_connection_records_into_a_private_file_of_its_own`,
  `a_failed_write_ends_only_the_recording`,
  `a_dst_cases_wire_trace_replays`.
- **A failed start is logged, not a panic.** When the proxy cannot start
  (no `XDG_RUNTIME_DIR`, no device or compositor, a socket it cannot
  claim or bind), it logs why and exits with status 1. No test covers
  the device or compositor case.
  Tests: `a_missing_runtime_directory_is_a_logged_startup_error`,
  `an_unbindable_socket_is_a_logged_startup_error`.

## ChromeOS shelf application IDs

For the shelf to match a guest window with its `.desktop` file (icon and
pinning), the application ID the host receives is
`org.chromium.guest_os.<vm-id>.wayland.<app-id>`, sent through the
surface's host `zaura_surface.set_application_id` for every
`xdg_toplevel.set_app_id` and GTK application ID. `<vm-id>` is
`SOMMELIER_VM_IDENTIFIER`, or `termina` (the standard Crostini VM name)
when it is unset or empty. Window placement never changes it: an ARC
application ID would let Exo accept bounds requests, but Chrome then
sends the window's keys past the ChromeOS IME.
Tests: `set_app_id_creates_zaura_surface_and_sets_app_id`,
`dbus_application_id_is_namespaced_on_aura_surface`,
`set_app_id_keeps_the_guest_os_namespace_with_placement_on`.

## Manual verification

- **Test the binary you built.** A rebuilt binary does not replace a
  proxy that is already running: compare the binary's modification time
  with the test proxy's start time, and restart the isolated test proxy
  and its client if the binary is newer. Never restart the primary
  Sommelier service or disturb unrelated clients for an isolated test.
- **Check glyphs, not just events.** A Korean input check confirms that
  the committed text renders as glyphs, not tofu boxes, in a sample GUI
  (`sommelier_test_app`) with a Korean/CJK font
  (`SOMMELIER_TEST_GUI_FONT` overrides it); receiving preedit or commit
  events is not enough.
