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
use crate::state::{Context, DeletedHostId, SessionEnd};
use crate::wire::{Action, MessageHeader, Outgoing, ProtocolError, WL_DISPLAY_ID};
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
    let (object, code, message) = match end {
        SessionEnd::Protocol {
            object,
            code,
            message,
        } => (*object, *code, bounded(message)),
        SessionEnd::Internal => (
            WL_DISPLAY_ID,
            wl_display::ERROR_IMPLEMENTATION,
            "the proxy failed to handle a request",
        ),
    };
    wl_display::evt::error(WL_DISPLAY_ID, object, code, message)
        .expect("a bounded diagnostic fits in an error event")
}

/// A prefix of `message` that fits in an error event. A host's message or
/// a client-controlled interface name could otherwise make the event exceed
/// the wire's message length limit (`MessageHeader::MAX_LEN`).
fn bounded(message: &str) -> &str {
    const MAX_DIAGNOSTIC_BYTES: usize = 1024;
    // The header, object, code and string length, and at most 4 bytes of
    // terminator and padding: every bounded error event fits.
    const _: () = assert!(MessageHeader::LEN + 16 + MAX_DIAGNOSTIC_BYTES <= MessageHeader::MAX_LEN);
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
    if ctx.local_destroyed(guest_id) {
        queue_delete_id(ctx, guest_id);
    }
}

impl wl_display::WlDisplayHandler for DisplayHandler {
    fn on_get_registry(&mut self, ctx: &mut Context, registry: u32) -> Action {
        let host_registry_id = ctx.shadow_table.create_paired(
            registry,
            wl_registry::NAME,
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
            wl_callback::NAME,
            wl_callback::VERSION,
            wl_callback::VERSION,
        );

        ctx.client_to_host_queue
            .push(wl_display::req::sync(WL_DISPLAY_ID, host_callback_id));

        Action::Drop
    }

    fn on_delete_id(&mut self, ctx: &mut Context, id: u32) -> Action {
        // The host has destroyed the object, so no later event can name it,
        // and the ID may go to the next new object: the object's state ends
        // with the ID (see `crate::state::lifecycle`).
        if let DeletedHostId::Paired { guest, .. } = ctx.release_deleted_host_id(id) {
            // Forward the translated acknowledgement for the guest object,
            // after whatever ending it sent the guest.
            queue_delete_id(ctx, guest);
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
        ctx.reject(guest_id, code, message.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::state::{Context, HostId};
    use crate::test_support::{protocol_error, queued_headers, raw_dispatch};
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

    /// A host may destroy a guest's object on its own (INVARIANTS "Object
    /// lifetimes"). Its `delete_id` must forget the object's state, as the
    /// guest's destructor would, so the host ID the allocator hands out next
    /// starts clean.
    #[test]
    fn a_host_destroyed_output_is_forgotten() {
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::aura_shell::zaura_output;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;
        let mut ctx = Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::active_for_test(&[]),
        );
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let output = "wl_output".to_string();
        RegistryHandler.on_global(&mut ctx, 11, &"zaura_shell".to_string(), 38);
        RegistryHandler.on_global(&mut ctx, 10, &output, 3);
        RegistryHandler.on_bind(&mut ctx, 10, &(output.clone(), 3, 20));
        let host = ctx.shadow_table.get_host_id(20).expect("a bound output");
        let aura_output = ctx.outputs.aura_output(host).expect("an Aura output");
        ctx.client_to_host_queue.clear();
        ctx.host_to_client_queue.clear();

        DisplayHandler.on_delete_id(&mut ctx, host);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(aura_output, zaura_output::REQ_RELEASE)]
        );
        assert!(ctx.outputs.is_empty_for_test());
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(WL_DISPLAY_ID, wl_display::EVT_DELETE_ID)]
        );
        ctx.assert_no_orphans();

        // The next output gets the freed host ID.
        RegistryHandler.on_bind(&mut ctx, 10, &(output, 3, 21));
        assert_eq!(ctx.shadow_table.get_host_id(21), Some(host));
        assert_eq!(ctx.outputs.live_outputs(), [host]);
    }

    #[test]
    fn a_host_destroyed_keyboard_is_forgotten() {
        use crate::handler::keyboard::KeyboardHandler;
        use crate::protocols::keyboard_extension_unstable_v1::{
            zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
        };
        const SEAT: u32 = 10;
        const EXTENSION: u32 = 100;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(SEAT, 11, "wl_seat");
        ctx.shadow_table
            .insert_host_only_for_test(EXTENSION, "zcr_keyboard_extension_v1", 2);
        ctx.host_keyboard_extension_id = Some(HostId(EXTENSION));
        let host = ctx.shadow_table.create_paired(20, "wl_keyboard", 1, 1);
        crate::handler::keyboard::for_test::created(&mut ctx, 20, host, SEAT);
        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(host));
        let extended = ctx
            .keys
            .keyboards()
            .extended_for_test(HostId(host))
            .unwrap();
        ctx.client_to_host_queue.clear();

        DisplayHandler.on_delete_id(&mut ctx, host);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(extended.0, zcr_extended_keyboard_v1::REQ_DESTROY)]
        );
        assert!(ctx.shadow_table.is_retired_host(extended.0));
        assert!(ctx.keys.keyboards().is_empty());
        assert!(!ctx.is_fatal());
        ctx.assert_no_orphans();

        // The next keyboard gets the freed host ID, and its own extended
        // keyboard.
        ctx.client_to_host_queue.clear();
        assert_eq!(
            ctx.shadow_table.create_paired(21, "wl_keyboard", 1, 1),
            host
        );
        crate::handler::keyboard::for_test::created(&mut ctx, 21, host, SEAT);
        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(host));
        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(
                EXTENSION,
                zcr_keyboard_extension_v1::REQ_GET_EXTENDED_KEYBOARD
            )]
        );
    }

    /// An object is forgotten once, at its first end: the `delete_id` that
    /// acknowledges the guest's destructor forgets nothing again, so the
    /// extended keyboard is destroyed and retired once.
    #[test]
    fn a_delete_id_after_the_destructor_forgets_nothing_again() {
        use crate::handler::keyboard::KeyboardHandler;
        const EXTENSION: u32 = 100;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table
            .insert_host_only_for_test(EXTENSION, "zcr_keyboard_extension_v1", 2);
        ctx.host_keyboard_extension_id = Some(HostId(EXTENSION));
        let host = ctx.shadow_table.create_paired(20, "wl_keyboard", 1, 1);
        KeyboardHandler::ensure_extended_keyboard_bound(&mut ctx, HostId(host));
        ctx.guest_destroyed(20);
        ctx.client_to_host_queue.clear();

        DisplayHandler.on_delete_id(&mut ctx, host);

        assert!(ctx.client_to_host_queue.is_empty());
        assert!(!ctx.is_fatal(), "the extended keyboard is retired once");
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(WL_DISPLAY_ID, wl_display::EVT_DELETE_ID)]
        );
    }

    #[test]
    fn a_host_destroyed_surface_is_forgotten() {
        use crate::protocols::aura_shell::zaura_surface;
        const SURFACE: u32 = 30;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.bind_aura_shell_for_test(100);
        let host = ctx.shadow_table.create_paired(SURFACE, "wl_surface", 1, 1);
        ctx.create_surface(SURFACE);
        let aura_surface =
            crate::handler::aura_objects::ensure_host_zaura_surface(&mut ctx, SURFACE)
                .expect("an Aura surface");
        ctx.client_to_host_queue.clear();

        DisplayHandler.on_delete_id(&mut ctx, host);

        assert_eq!(
            queued_headers(&ctx.client_to_host_queue),
            [(aura_surface, zaura_surface::REQ_RELEASE)]
        );
        assert_eq!(ctx.aura_surfaces.count(), 0);
        assert!(ctx.surfaces.is_empty());
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(WL_DISPLAY_ID, wl_display::EVT_DELETE_ID)]
        );
        ctx.assert_no_orphans();
    }

    /// The host object of an active text input is gone with its
    /// `delete_id`, so forgetting the text input deactivates nothing on it;
    /// its extension is a host object of its own, which it still destroys.
    #[tokio::test]
    async fn a_host_destroyed_text_input_sends_nothing_to_its_host_object() {
        use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;
        use crate::protocols::wayland::wl_keyboard;
        use crate::proxy::harness::Session;

        let mut s = Session::new();
        let (seat, keyboard) = s.keyboard().await;
        let surface = s.surface().await;
        s.host([wl_keyboard::evt::enter(keyboard.host, 1, surface.host, &[]).unwrap()])
            .await;
        assert_eq!(s.guest_received().len(), 2, "the enter and its modifiers");
        let text_input = s.text_input(seat).await;
        let extension = text_input.ext;
        let enter = zwp_text_input_v3::Event::Enter {
            surface: surface.id,
        };
        assert_eq!(s.guest_received(), [(text_input.id, enter.into())]);
        s.enable(text_input, 1).await;

        // The session's check that no state outlives its object pins that
        // the proxy forgets the text input.
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, text_input.host)])
            .await;
        assert_eq!(
            s.host_received(),
            [(
                extension,
                zcr_extended_text_input_v1::Request::Destroy {}.into()
            )]
        );
        let delete_id = wl_display::Event::DeleteId { id: text_input.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, delete_id.into())]);
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

    /// The host's error reaches the guest on its object and with its code,
    /// its diagnostic bounded like the proxy's own.
    #[test]
    fn a_host_error_is_forwarded_with_a_bounded_diagnostic() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(7, 77, "wl_surface");

        DisplayHandler.on_error(&mut ctx, 77, 3, &"x".repeat(2_000));

        assert_eq!(protocol_error(&ctx), (7, 3));
        let (error, _) = session_error(ctx.session_end().expect("the session ends"));
        assert_eq!(
            u32::from_ne_bytes(error[16..20].try_into().unwrap()),
            1025,
            "the diagnostic keeps 1024 bytes and its terminator"
        );
    }

    #[test]
    fn fatal_protocol_error_keeps_a_bounded_diagnostic() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert_eq!(ctx.reject(7, 3, "x".repeat(4_084)), Action::Drop);

        let (error, _) = session_error(ctx.session_end().expect("the session ends"));
        assert!(
            error.len() <= crate::wire::MessageHeader::MAX_LEN,
            "the bounded diagnostic must fit Wayland's message length field"
        );
        assert_eq!(protocol_error(&ctx), (7, 3));
    }
}
