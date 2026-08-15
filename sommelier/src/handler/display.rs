/*
Copyright 2026 Google LLC

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

     https://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use crate::protocols::wayland::wl_display;
use crate::state::{Context, HostId};
use crate::wire::{Action, MessageBuilder};
use log::error;

pub struct DisplayHandler;

/// Report a fatal protocol error against a guest object.
///
/// Wayland errors are fatal for the client session. Callers must mark the
/// context as fatal as well; the proxy flushes this queued event before
/// closing the session so the client receives the diagnostic.
pub(crate) fn queue_protocol_error(
    ctx: &mut Context,
    object_id: u32,
    code: u32,
    message: impl AsRef<str>,
) {
    // Whether or not the diagnostic itself fits in the Wayland wire limit,
    // the malformed request is fatal and must terminate the session.
    ctx.fatal_protocol_error = true;
    // The error event itself is subject to Wayland's 16-bit message-length
    // limit. A client-controlled interface name or diagnostic can otherwise
    // make the proxy drop the diagnostic while still tearing down the
    // connection. Keep a useful bounded prefix and preserve UTF-8 validity.
    const MAX_DIAGNOSTIC_BYTES: usize = 1024;
    let message = message.as_ref();
    let diagnostic = if message.len() <= MAX_DIAGNOSTIC_BYTES {
        message
    } else {
        let mut end = MAX_DIAGNOSTIC_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        &message[..end]
    };
    let mut builder = MessageBuilder::new();
    builder.write_u32(object_id);
    builder.write_u32(code);
    builder.write_string(diagnostic);
    let _ = queue_message(
        &mut ctx.host_to_client_queue,
        1,
        wl_display::EVT_ERROR,
        builder,
    );
}

/// Queue a local `wl_display.delete_id` event for a guest object.
///
/// Proxy-local objects such as the v3 text-input manager do not have a host
/// object that can acknowledge their destructor. Their handlers call the
/// local wrapper below after cleaning local state so the guest observes the
/// same lifecycle as it would for a compositor-owned object.
pub(crate) fn queue_guest_delete_id(ctx: &mut Context, guest_id: u32) -> bool {
    if ctx.shadow_table.get_interface(guest_id).is_none() {
        return false;
    }
    let mut builder = MessageBuilder::new();
    builder.write_u32(guest_id);
    queue_message(
        &mut ctx.host_to_client_queue,
        1,
        wl_display::EVT_DELETE_ID,
        builder,
    )
}

/// Retire a synthetic guest object and queue its local delete acknowledgement.
///
/// The object must not have a host mapping. Mapped guest objects backed by a
/// host protocol without a destructor use [`queue_guest_delete_id`] followed
/// by `ShadowTable::remove_guest_mapping` so the host-side reservation remains
/// alive for stale-event suppression.
pub(crate) fn queue_local_delete_id(ctx: &mut Context, guest_id: u32) {
    if ctx.shadow_table.get_interface(guest_id).is_some()
        && ctx.shadow_table.get_host_id(guest_id).is_none()
        && queue_guest_delete_id(ctx, guest_id)
    {
        ctx.shadow_table.remove_id(guest_id);
    }
}

fn queue_message(
    queue: &mut Vec<(Vec<u8>, Vec<std::os::unix::io::RawFd>)>,
    sender_id: u32,
    opcode: u16,
    builder: MessageBuilder,
) -> bool {
    match builder.try_build_message(sender_id, opcode) {
        Ok(message) => {
            queue.push((message, Vec::new()));
            true
        }
        Err(error) => {
            error!(
                "Dropping display message sender={} opcode={}: {}",
                sender_id, opcode, error
            );
            false
        }
    }
}

impl wl_display::WlDisplayHandler for DisplayHandler {
    fn on_get_registry(&mut self, ctx: &mut Context, registry: u32) -> Action {
        let host_registry_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table.map_id(registry, host_registry_id);
        ctx.shadow_table
            .track_interface(registry, "wl_registry".to_string());

        // Send get_registry to host (opcode 1)
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_registry_id);

        queue_message(
            &mut ctx.client_to_host_queue,
            1,
            wl_display::REQ_GET_REGISTRY,
            builder,
        );

        Action::Drop
    }

    fn on_sync(&mut self, ctx: &mut Context, callback: u32) -> Action {
        let host_callback_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table.map_id(callback, host_callback_id);
        ctx.shadow_table
            .track_interface(callback, "wl_callback".to_string());

        // Send sync to host (opcode 0)
        let mut builder = MessageBuilder::new();
        builder.write_u32(host_callback_id);

        queue_message(
            &mut ctx.client_to_host_queue,
            1,
            wl_display::REQ_SYNC,
            builder,
        );

        Action::Drop
    }

    fn on_delete_id(&mut self, ctx: &mut Context, id: u32) -> Action {
        if ctx.shadow_table.consume_host_delete_id(id) {
            // Internal host-only objects have no guest object ID. Consume
            // their destructor acknowledgement locally instead of emitting an
            // invalid delete_id(0) event to the guest.
            ctx.remove_render_buffer_host(id);
            return Action::Drop;
        }
        if let Some(&guest_id) = ctx.orphaned_dmabuf_params.get(&id) {
            // An async linux-dmabuf create may emit `created`/`failed` after
            // the host has acknowledged params.destroy. Retain the host
            // interface metadata so that late event can still be dispatched,
            // but complete the guest-side delete_id now.
            let mapping_alive = ctx.shadow_table.get_guest_id(id) == Some(guest_id);
            if mapping_alive && queue_guest_delete_id(ctx, guest_id) {
                ctx.shadow_table.remove_guest_mapping(guest_id);
                // remove_guest_mapping intentionally leaves the host
                // interface reserved for the late async event. Clear
                // only the guest pending-destroy marker; remove_id would
                // discard the host interface before `created`/`failed`.
                ctx.shadow_table.clear_pending_destroy_guest(guest_id);
            }
            return Action::Drop;
        }
        let guest_id = ctx.shadow_table.get_guest_id(id).unwrap_or(0);
        if guest_id != 0 {
            ctx.remove_render_buffer_host(id);
            ctx.shadow_table.remove_id(guest_id);
            ctx.pending_native_creates.remove(&HostId(id));

            // Forward the corrected delete_id event to the client
            let mut builder = MessageBuilder::new();
            builder.write_u32(guest_id);

            queue_message(
                &mut ctx.host_to_client_queue,
                1,
                wl_display::EVT_DELETE_ID,
                builder,
            );
        }
        Action::Drop
    }

    fn on_error(
        &mut self,
        ctx: &mut Context,
        object_id: u32,
        code: u32,
        message: &String,
    ) -> Action {
        let Some(guest_id) = ctx.shadow_table.get_guest_id(object_id) else {
            // wl_display.error.object_id is a non-null object argument. An
            // internal host-only object has no valid guest ID; forwarding 0
            // would make the guest receive an invalid Wayland error event and
            // can trigger a secondary protocol error in its display parser.
            error!(
                "Dropping host display error for unmapped object_id={} (code={})",
                object_id, code
            );
            return Action::Drop;
        };
        error!(
            "Wayland Error from Host: object_id={} (guest_id={}), code={}, message={}",
            object_id, guest_id, code, message
        );

        let mut builder = MessageBuilder::new();
        builder.write_u32(guest_id);
        builder.write_u32(code);
        builder.write_string(message);

        queue_message(
            &mut ctx.host_to_client_queue,
            1,
            wl_display::EVT_ERROR,
            builder,
        );

        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::state::Context;
    use crate::wire::{ProtocolError, WireMessage};

    struct ProbeHandler;

    impl WlDisplayHandler for ProbeHandler {
        fn on_get_registry(&mut self, ctx: &mut Context, _registry: u32) -> Action {
            ctx.fatal_protocol_error = true;
            Action::Drop
        }
    }

    #[test]
    fn unmapped_request_is_rejected_before_handler_state_mutation() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let payload = 20u32.to_ne_bytes();
        let mut msg = WireMessage::new(999, wl_display::REQ_GET_REGISTRY, &payload, &[]);
        let mut handler = ProbeHandler;

        assert_eq!(
            wl_display::dispatch_request(&mut msg, &mut handler, &mut ctx),
            Err(ProtocolError::InvalidObjectId(999))
        );
        assert!(
            !ctx.fatal_protocol_error,
            "an invalid sender must not reach a mutating handler"
        );
    }

    #[test]
    fn unmapped_host_error_is_not_encoded_with_invalid_object_zero() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let mut handler = DisplayHandler;

        assert_eq!(
            handler.on_error(&mut ctx, 77, 3, &"internal error".to_string()),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "an unmapped host-only object must not produce wl_display.error"
        );
    }

    #[test]
    fn oversized_host_error_is_dropped_before_wire_encoding() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(7, 77);
        let mut handler = DisplayHandler;

        assert_eq!(
            handler.on_error(&mut ctx, 77, 3, &"x".repeat(65_520)),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "a host-supplied error string must not wrap the 16-bit wire length"
        );
    }

    #[test]
    fn fatal_protocol_error_keeps_a_bounded_diagnostic() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        queue_protocol_error(&mut ctx, 7, 3, "x".repeat(65_520));

        assert!(ctx.fatal_protocol_error);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            1,
            "fatal validation must still deliver a display.error event"
        );
        assert!(
            ctx.host_to_client_queue[0].0.len() <= 0xffff,
            "the bounded diagnostic must fit Wayland's message length field"
        );
    }
}
