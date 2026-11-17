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

use crate::protocols::wayland::{wl_callback, wl_display, wl_registry};
use crate::state::{Context, DeletedHostId, HostId, SessionEnd};
use crate::wire::{Action, Outgoing, ProtocolError, WL_DISPLAY_ID};
use log::error;

pub struct DisplayHandler;

/// How a guest request the proxy cannot decode or dispatch ends the
/// session, as libwayland-server's `wl_client_connection_data` reports one:
/// always on `wl_display`, as `invalid_object` when the sender is not a
/// usable object ([`ProtocolError::InvalidSender`]), and as `invalid_method`
/// for anything else (an unknown opcode, a request too new for its object,
/// arguments that do not decode, or an object or `new_id` argument that
/// cannot be used, even one that names the sender).
pub(crate) fn request_error(error: &ProtocolError) -> SessionEnd {
    let code = match error {
        ProtocolError::InvalidSender(_) => wl_display::ERROR_INVALID_OBJECT,
        _ => wl_display::ERROR_INVALID_METHOD,
    };
    SessionEnd::Protocol {
        object: WL_DISPLAY_ID,
        code,
        message: error.to_string(),
    }
}

/// The one `wl_display.error` that tells the guest its session ended.
pub(crate) fn session_error(end: &SessionEnd) -> Outgoing {
    let error = match end {
        SessionEnd::Protocol {
            object,
            code,
            message,
        } => wl_display::evt::error(WL_DISPLAY_ID, *object, *code, bounded(message)),
        SessionEnd::HostError {
            object,
            code,
            message,
        } => wl_display::evt::error(WL_DISPLAY_ID, *object, *code, message),
        SessionEnd::Internal => wl_display::evt::error(
            WL_DISPLAY_ID,
            WL_DISPLAY_ID,
            wl_display::ERROR_IMPLEMENTATION,
            "the proxy failed to handle a request",
        ),
    };
    error.unwrap_or_else(|error| {
        // Only the host's message is not bounded. Too long to forward, it
        // leaves the guest the `implementation` error.
        log::warn!("Dropping the host error: {error}");
        session_error(&SessionEnd::Internal)
    })
}

/// A prefix of the proxy's own diagnostic `message` that fits in an error
/// event. A client-controlled interface name could otherwise make the
/// event exceed the wire's message length limit (`MessageHeader::MAX_LEN`).
fn bounded(message: &str) -> &str {
    const MAX_DIAGNOSTIC_BYTES: usize = 1024;
    let mut end = message.len().min(MAX_DIAGNOSTIC_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// Queue the `wl_display.delete_id` that ends guest object `guest_id`, for
/// an object whose host object cannot send it: one the proxy implements, or
/// one backed by a host object without a destructor. The caller ends the
/// object in the [`crate::state::ShadowTable`] first.
pub(crate) fn queue_delete_id(ctx: &mut Context, guest_id: u32) {
    ctx.host_to_client_queue
        .push(wl_display::evt::delete_id(WL_DISPLAY_ID, guest_id));
}

/// End local guest object `guest_id` and queue its `delete_id`, as its host
/// would for a host object. Nothing happens unless it is a local object.
pub(crate) fn queue_local_delete_id(ctx: &mut Context, guest_id: u32) {
    if ctx.shadow_table.local_destroyed(guest_id) {
        queue_delete_id(ctx, guest_id);
    }
}

impl wl_display::WlDisplayHandler for DisplayHandler {
    fn on_get_registry(&mut self, ctx: &mut Context, registry: u32) -> Action {
        let host_registry_id = ctx.shadow_table.create_paired(
            registry,
            "wl_registry",
            wl_registry::VERSION,
            wl_registry::VERSION,
        );

        ctx.client_to_host_queue.push(wl_display::req::get_registry(
            WL_DISPLAY_ID,
            host_registry_id,
        ));

        Action::Drop
    }

    fn on_sync(&mut self, ctx: &mut Context, callback: u32) -> Action {
        let host_callback_id = ctx.shadow_table.create_paired(
            callback,
            "wl_callback",
            wl_callback::VERSION,
            wl_callback::VERSION,
        );

        ctx.client_to_host_queue
            .push(wl_display::req::sync(WL_DISPLAY_ID, host_callback_id));

        Action::Drop
    }

    fn on_delete_id(&mut self, ctx: &mut Context, id: u32) -> Action {
        // The host has destroyed the object, so no later event can name it,
        // and the ID may go to the next new object: drop all state keyed by
        // the host ID along with the ID itself.
        let paired_guest = match ctx.shadow_table.release_deleted_host_id(id) {
            DeletedHostId::Ignored => return Action::Drop,
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
        // over, and its error is the guest's one error.
        let Some(guest_id) = ctx.shadow_table.get_guest_id(object_id) else {
            // wl_display.error.object_id is a non-null object argument. An
            // internal host-only object has no valid guest ID; forwarding 0
            // would make the guest receive an invalid Wayland error event and
            // can trigger a secondary protocol error in its display parser.
            // The session still ends with an `implementation` error.
            return ctx.fail(format_args!(
                "Dropping host display error for unmapped object_id={object_id} (code={code}): {message}"
            ));
        };
        error!(
            "Wayland Error from Host: object_id={} (guest_id={}), code={}, message={}",
            object_id, guest_id, code, message
        );
        ctx.end_session(SessionEnd::HostError {
            object: guest_id,
            code,
            message: message.clone(),
        });
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::state::Context;
    use crate::test_support::{protocol_error, raw_dispatch};
    use crate::wire::{ProtocolError, WireMessage};

    struct ProbeHandler;

    impl WlDisplayHandler for ProbeHandler {
        fn on_get_registry(&mut self, ctx: &mut Context, _registry: u32) -> Action {
            ctx.fail("the probe handler ran")
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
        let mut msg = WireMessage::new(999, wl_display::REQ_GET_REGISTRY, &payload);
        let mut handler = ProbeHandler;

        assert_eq!(
            raw_dispatch(wl_display::dispatch_request(
                &mut msg,
                &mut handler,
                &mut ctx
            )),
            Err(ProtocolError::InvalidSender(999))
        );
        assert!(
            !ctx.is_fatal(),
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
        assert_eq!(
            ctx.session_end(),
            Some(&SessionEnd::Internal),
            "an unmapped host-only object must not name the guest's object 0"
        );
    }

    /// A host error too long to forward does not take the session's one
    /// error: the guest still learns why it ends.
    #[test]
    fn an_unforwardable_host_error_leaves_the_implementation_error() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(7, 77, "wl_surface");

        DisplayHandler.on_error(&mut ctx, 77, 3, &"x".repeat(4_084));

        assert_eq!(
            protocol_error(&ctx),
            (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)
        );
    }

    #[test]
    fn fatal_protocol_error_keeps_a_bounded_diagnostic() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.reject(7, 3, "x".repeat(4_084));

        let (error, _) = session_error(ctx.session_end().expect("the session ends"));
        assert!(
            error.len() <= crate::wire::MessageHeader::MAX_LEN,
            "the bounded diagnostic must fit Wayland's message length field"
        );
        assert_eq!(protocol_error(&ctx), (7, 3));
    }
}
