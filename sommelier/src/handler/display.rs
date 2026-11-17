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
use crate::state::{Context, DeletedHostId, HostId};
use crate::wire::{queue_or_drop, Action, ProtocolError, WL_DISPLAY_ID};
use log::error;

pub struct DisplayHandler;

/// `wl_display.error` code: the request names an object that does not
/// exist.
pub(crate) const WL_DISPLAY_ERROR_INVALID_OBJECT: u32 = 0;
/// `wl_display.error` code: the request is not valid for its object.
pub(crate) const WL_DISPLAY_ERROR_INVALID_METHOD: u32 = 1;
/// `wl_display.error` code: the compositor (here, the proxy) failed.
pub(crate) const WL_DISPLAY_ERROR_IMPLEMENTATION: u32 = 3;

/// End a fatal session with an `implementation` error if nothing has told
/// the guest why yet: internal failures (a dma-buf sync that fails during
/// a commit, say) mark the session fatal without an error of their own,
/// and every session closed for a protocol reason gets exactly one.
pub(crate) fn queue_error_if_none_queued(ctx: &mut Context) {
    if ctx.fatal_protocol_error {
        // A no-op when an error is already queued.
        queue_protocol_error(
            ctx,
            WL_DISPLAY_ID,
            WL_DISPLAY_ERROR_IMPLEMENTATION,
            "the proxy failed to handle a request",
        );
    }
}

/// Report a guest request the proxy cannot decode or dispatch, as
/// libwayland-server's `wl_client_connection_data` reports one: always on
/// `wl_display`, as `invalid_object` when the sender is not a usable object
/// ([`ProtocolError::InvalidSender`]), and as `invalid_method` for anything
/// else (an unknown opcode, a request too new for its object, arguments
/// that do not decode, or an object or `new_id` argument that cannot be
/// used, even one that names the sender).
pub(crate) fn queue_request_error(ctx: &mut Context, error: &ProtocolError) {
    let code = match error {
        ProtocolError::InvalidSender(_) => WL_DISPLAY_ERROR_INVALID_OBJECT,
        _ => WL_DISPLAY_ERROR_INVALID_METHOD,
    };
    queue_protocol_error(ctx, WL_DISPLAY_ID, code, error.to_string());
}

/// Report a fatal protocol error against a guest object.
///
/// Wayland errors are fatal for the client session: this marks the context
/// fatal, and the proxy flushes the queued event before closing the session
/// so the client receives the diagnostic. Like libwayland-server, which
/// posts one error per client, only the first call queues an event.
pub(crate) fn queue_protocol_error(
    ctx: &mut Context,
    object_id: u32,
    code: u32,
    message: impl AsRef<str>,
) {
    // Whether or not the diagnostic itself fits in the Wayland wire limit,
    // the malformed request is fatal and must terminate the session.
    ctx.fatal_protocol_error = true;
    if !ctx.note_protocol_error() {
        return;
    }
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
    queue_or_drop(
        &mut ctx.host_to_client_queue,
        wl_display::evt::error(WL_DISPLAY_ID, object_id, code, diagnostic),
        format_args!("the protocol error on object {object_id}"),
    );
}

/// Queue a local `wl_display.delete_id` event for a guest object.
///
/// Proxy-local objects such as the v3 text-input manager do not have a host
/// object that can acknowledge their destructor. Their handlers call the
/// local wrapper below after cleaning local state so the guest observes the
/// same lifecycle as it would for a compositor-owned object.
///
/// # Returns
///
/// `false`, queueing nothing, only when `guest_id` is not a known guest
/// object.
pub(crate) fn queue_guest_delete_id(ctx: &mut Context, guest_id: u32) -> bool {
    if ctx.shadow_table.get_interface(guest_id).is_none() {
        return false;
    }
    queue_delete_id(ctx, guest_id);
    true
}

fn queue_delete_id(ctx: &mut Context, guest_id: u32) {
    ctx.host_to_client_queue
        .push(wl_display::evt::delete_id(WL_DISPLAY_ID, guest_id));
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

impl wl_display::WlDisplayHandler for DisplayHandler {
    fn on_get_registry(&mut self, ctx: &mut Context, registry: u32) -> Action {
        let host_registry_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table.map_id(registry, host_registry_id);
        ctx.shadow_table
            .track_interface(registry, "wl_registry".to_string());

        ctx.client_to_host_queue.push(wl_display::req::get_registry(
            WL_DISPLAY_ID,
            host_registry_id,
        ));

        Action::Drop
    }

    fn on_sync(&mut self, ctx: &mut Context, callback: u32) -> Action {
        let host_callback_id = ctx.shadow_table.allocate_host_id();
        ctx.shadow_table.map_id(callback, host_callback_id);
        ctx.shadow_table
            .track_interface(callback, "wl_callback".to_string());

        ctx.client_to_host_queue
            .push(wl_display::req::sync(WL_DISPLAY_ID, host_callback_id));

        Action::Drop
    }

    fn on_delete_id(&mut self, ctx: &mut Context, id: u32) -> Action {
        // The host has destroyed the object, so no later event can name it,
        // and the ID may go to the next new object: drop all state keyed by
        // the host ID along with the ID itself.
        let paired_guest = match ctx.shadow_table.release_deleted_host_id(id) {
            DeletedHostId::Untracked => return Action::Drop,
            DeletedHostId::HostOnly => None,
            DeletedHostId::Paired(guest_id) => Some(guest_id),
        };
        ctx.remove_render_buffer_host(id);
        ctx.pending_native_creates.remove(&HostId(id));
        if let Some(guest_id) = paired_guest {
            // Forward the translated acknowledgement for the guest object.
            queue_delete_id(ctx, guest_id);
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
        // The host ends the connection after its error, so the session is
        // over, and its error is the guest's one error unless the guest
        // already has one.
        ctx.fatal_protocol_error = true;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(object_id) else {
            // wl_display.error.object_id is a non-null object argument. An
            // internal host-only object has no valid guest ID; forwarding 0
            // would make the guest receive an invalid Wayland error event and
            // can trigger a secondary protocol error in its display parser.
            // The session still ends with an `implementation` error.
            error!(
                "Dropping host display error for unmapped object_id={} (code={})",
                object_id, code
            );
            return Action::Drop;
        };
        if !ctx.note_protocol_error() {
            return Action::Drop;
        }
        error!(
            "Wayland Error from Host: object_id={} (guest_id={}), code={}, message={}",
            object_id, guest_id, code, message
        );

        queue_or_drop(
            &mut ctx.host_to_client_queue,
            wl_display::evt::error(WL_DISPLAY_ID, guest_id, code, message),
            format_args!("the host error on object {guest_id}"),
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
    fn delete_id_for_an_untracked_host_id_changes_nothing() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        const UNTRACKED: u32 = 40;
        assert!(ctx.register_native_buffer(UNTRACKED, (1, 1), Vec::new()));

        assert_eq!(
            DisplayHandler.on_delete_id(&mut ctx, UNTRACKED),
            Action::Drop
        );
        assert!(
            ctx.has_render_buffer_host(HostId(UNTRACKED)),
            "a delete_id the table does not track must not end other state"
        );
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn unmapped_request_is_rejected_before_handler_state_mutation() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let payload = 20u32.to_ne_bytes();
        let mut msg = WireMessage::new(999, wl_display::REQ_GET_REGISTRY, &payload, &[]);
        let mut handler = ProbeHandler;

        assert_eq!(
            wl_display::dispatch_request(&mut msg, &mut handler, &mut ctx),
            Err(ProtocolError::InvalidSender(999))
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
