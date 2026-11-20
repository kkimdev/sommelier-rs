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

use crate::handler::Handlers;
use crate::protocols::wayland::{wl_callback, wl_display, wl_registry};
use crate::protocols::Iface;
use crate::state::{Context, DeletedHostId, SessionEnd};
use crate::wire::{Action, MessageHeader, Outgoing, ProtocolError, WL_DISPLAY_ID};
use log::error;

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

impl wl_display::WlDisplayHandler for Handlers {
    fn on_get_registry(ctx: &mut Context, registry: u32) -> Action {
        let host_registry_id = ctx.shadow_table.create_paired(
            registry,
            Iface::WlRegistry,
            wl_registry::VERSION,
            wl_registry::VERSION,
        );

        ctx.client_to_host_queue.push(wl_display::req::get_registry(
            WL_DISPLAY_ID,
            host_registry_id,
        ));

        Action::Drop
    }

    fn on_sync(ctx: &mut Context, callback: u32) -> Action {
        let host_callback_id = ctx.shadow_table.create_paired(
            callback,
            Iface::WlCallback,
            wl_callback::VERSION,
            wl_callback::VERSION,
        );

        ctx.client_to_host_queue
            .push(wl_display::req::sync(WL_DISPLAY_ID, host_callback_id));

        Action::Drop
    }

    fn on_delete_id(ctx: &mut Context, id: u32) -> Action {
        // The host has destroyed the object, so no later event can name it,
        // and the ID may go to the next new object: the object's state ends
        // with the ID (see `crate::state::lifecycle`).
        if let DeletedHostId::Paired { guest, .. } = ctx.release_deleted_host_id(id) {
            // Forward the translated acknowledgement for the guest object,
            // after whatever ending it sent the guest.
            ctx.queue_delete_id(guest);
        }
        Action::Drop
    }

    fn on_error(ctx: &mut Context, object_id: u32, code: u32, message: &String) -> Action {
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
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface};
    use crate::protocols::gtk::gtk_shell1;
    use crate::protocols::keyboard_extension_unstable_v1::{
        zcr_extended_keyboard_v1, zcr_keyboard_extension_v1,
    };
    use crate::protocols::wayland::{wl_keyboard, wl_output, wl_surface};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{Object, Session};

    /// The guest's `delete_id` of `id`.
    fn deleted(id: u32) -> (u32, AnyEvent) {
        (WL_DISPLAY_ID, wl_display::Event::DeleteId { id }.into())
    }

    /// A `delete_id` for a host ID the proxy does not track changes
    /// nothing: the guest gets nothing, and the ID is not freed, so the
    /// next host object the proxy creates gets the next ID, as the host
    /// checks.
    #[tokio::test]
    async fn delete_id_for_an_untracked_host_id_changes_nothing() {
        let mut s = Session::new();
        let surface = s.surface().await;
        const UNTRACKED: u32 = 40;
        assert!(surface.host < UNTRACKED);

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, UNTRACKED)])
            .await;
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.surface().await.host, surface.host + 1);
    }

    /// The guest binds output global `name` from `registry` at v3, which
    /// the proxy gives an Aura output from host `shell`; returns the output
    /// and its Aura output.
    async fn bind_output(
        s: &mut Session,
        registry: Object,
        name: u32,
        shell: u32,
    ) -> (Object, u32) {
        let id = s.new_id();
        s.guest([wl_registry::req::bind(registry.id, name, "wl_output", 3, id).unwrap()])
            .await;
        let host = s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id: bound, .. })), (
            sender,
            AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput {
                id: aura_output,
                output,
            }),
        )] = &host[..]
        else {
            panic!("the output got no Aura output: {host:?}");
        };
        assert_eq!((*sender, *output), (shell, bound.2));
        (Object { id, host: bound.2 }, *aura_output)
    }

    /// A host may destroy a guest's object on its own (INVARIANTS "Object
    /// lifetimes"). Its `delete_id` must forget the object's state, as the
    /// guest's destructor would, so the host ID the allocator hands out next
    /// starts clean: the next output gets it, and an Aura output of its
    /// own.
    #[tokio::test]
    async fn a_host_destroyed_output_is_forgotten() {
        let placement = PlacementConfig::active_for_test(&[]);
        let mut s = Session::with_context(Context::with_placement_for_test(placement));
        let registry = s.registry().await;
        let shell = s.aura_shell().await;
        let name = s.advertise("wl_output", 3).await;
        s.guest_received();
        let (output, aura_output) = bind_output(&mut s, registry, name, shell).await;

        s.host_may_end(|_, interface| interface == wl_output::NAME);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, output.host)])
            .await;
        assert_eq!(
            s.host_received(),
            [(aura_output, zaura_output::Request::Release {}.into())]
        );
        assert_eq!(s.guest_received(), [deleted(output.id)]);

        let (next, _) = bind_output(&mut s, registry, name, shell).await;
        assert_eq!(next.host, output.host);
    }

    /// A keyboard whose first enter has bound its `zcr_extended_keyboard_v1`
    /// from the proxy's keyboard extension.
    struct Extended {
        s: Session,
        seat: u32,
        surface: Object,
        /// The host `zcr_keyboard_extension_v1`.
        extension: u32,
        keyboard: Object,
        /// The host `zcr_extended_keyboard_v1` of `keyboard`.
        extended: u32,
    }

    impl Extended {
        async fn new() -> Self {
            let mut s = Session::new();
            s.advertise(zcr_keyboard_extension_v1::NAME, 2).await;
            let host = s.host_received();
            let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] = &host[..]
            else {
                panic!("the proxy did not bind the keyboard extension: {host:?}");
            };
            let extension = id.2;
            let (seat, keyboard) = s.keyboard().await;
            let surface = s.surface().await;
            let extended = Self::enter(&mut s, extension, keyboard, surface, 1).await;
            Self {
                s,
                seat,
                surface,
                extension,
                keyboard,
                extended,
            }
        }

        /// The host's keyboard focus enters `surface` on `keyboard` with
        /// `serial`, its first enter, which binds its extended keyboard
        /// from `extension`; returns it.
        async fn enter(
            s: &mut Session,
            extension: u32,
            keyboard: Object,
            surface: Object,
            serial: u32,
        ) -> u32 {
            s.host([wl_keyboard::evt::enter(keyboard.host, serial, surface.host, &[]).unwrap()])
                .await;
            assert_eq!(s.guest_received().len(), 2, "the enter and its modifiers");
            let host = s.host_received();
            let [(
                sender,
                AnyRequest::ZcrKeyboardExtensionV1(
                    zcr_keyboard_extension_v1::Request::GetExtendedKeyboard { id, keyboard: of },
                ),
            )] = host[..]
            else {
                panic!("the keyboard got no extended keyboard: {host:?}");
            };
            assert_eq!((sender, of), (extension, keyboard.host));
            id
        }
    }

    #[tokio::test]
    async fn a_host_destroyed_keyboard_is_forgotten() {
        let Extended {
            mut s,
            seat,
            surface,
            extension,
            keyboard,
            extended,
        } = Extended::new().await;

        s.host_may_end(|_, interface| interface == wl_keyboard::NAME);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, keyboard.host)])
            .await;
        assert_eq!(
            s.host_received(),
            [(
                extended,
                zcr_extended_keyboard_v1::Request::Destroy {}.into()
            )]
        );
        assert_eq!(s.guest_received(), [deleted(keyboard.id)]);

        // The next keyboard gets the freed host ID, and its own extended
        // keyboard.
        let next = s.keyboard_of(seat).await;
        assert_eq!(next.host, keyboard.host);
        Extended::enter(&mut s, extension, next, surface, 2).await;
    }

    /// An object is forgotten once, at its first end: the `delete_id` that
    /// acknowledges the guest's destructor forgets nothing again, so the
    /// extended keyboard is destroyed and retired once.
    #[tokio::test]
    async fn a_delete_id_after_the_destructor_forgets_nothing_again() {
        let Extended {
            mut s,
            keyboard,
            extended,
            ..
        } = Extended::new().await;
        s.guest([wl_keyboard::req::release(keyboard.id)]).await;
        assert_eq!(
            s.host_received(),
            [
                (keyboard.host, wl_keyboard::Request::Release {}.into()),
                (
                    extended,
                    zcr_extended_keyboard_v1::Request::Destroy {}.into()
                ),
            ]
        );

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, keyboard.host)])
            .await;
        assert_eq!(
            s.host_received(),
            [],
            "the extended keyboard is retired once"
        );
        assert_eq!(s.guest_received(), [deleted(keyboard.id)]);
    }

    #[tokio::test]
    async fn a_host_destroyed_surface_is_forgotten() {
        let mut s = Session::new();
        let gtk = s.gtk_shell(zaura_shell::VERSION).await;
        let surface = s.surface().await;
        let gtk_surface = s.new_id();
        s.guest([gtk_shell1::req::get_gtk_surface(
            gtk.id,
            gtk_surface,
            surface.id,
        )])
        .await;
        let host = s.host_received();
        let [(_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), ..] =
            host[..]
        else {
            panic!("the surface got no Aura surface: {host:?}");
        };
        let aura_surface = id;

        s.host_may_end(|_, interface| interface == wl_surface::NAME);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
            .await;
        assert_eq!(
            s.host_received(),
            [(aura_surface, zaura_surface::Request::Release {}.into())]
        );
        assert_eq!(s.guest_received(), [deleted(surface.id)]);
    }

    /// The host object of an active text input is gone with its
    /// `delete_id`, so forgetting the text input deactivates nothing on it;
    /// its extension is a host object of its own, which it still destroys.
    #[tokio::test]
    async fn a_host_destroyed_text_input_sends_nothing_to_its_host_object() {
        use crate::protocols::text_input_extension_unstable_v1::zcr_extended_text_input_v1;
        use crate::protocols::text_input_unstable_v3::zwp_text_input_v3;

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
        s.host_may_end(|_, interface| interface == zwp_text_input_v3::NAME);
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

    /// The host's error reaches the guest on its object and with its code,
    /// its diagnostic bounded like the proxy's own.
    #[tokio::test]
    async fn a_host_error_is_forwarded_with_a_bounded_diagnostic() {
        let mut s = Session::new();
        let surface = s.surface().await;

        let diagnostic = "x".repeat(2_000);
        s.host_fatal([
            wl_display::evt::error(WL_DISPLAY_ID, surface.host, 3, &diagnostic).unwrap(),
        ])
        .await;

        let (events, error, message) = s.guest_received_and_diagnosed_error();
        assert_eq!((events, error), (vec![], (surface.id, 3)));
        assert_eq!(
            message,
            diagnostic[..1024],
            "the diagnostic keeps 1024 bytes"
        );
    }

    /// A diagnostic the proxy writes itself is bounded too, so that its
    /// error fits a message: here one that names the interface of a bind,
    /// which the guest made nearly as long as a message.
    #[tokio::test]
    async fn fatal_protocol_error_keeps_a_bounded_diagnostic() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let interface = "x".repeat(4_000);
        let id = s.new_id();
        s.guest_fatal([wl_registry::req::bind(registry.id, 7, &interface, 1, id).unwrap()])
            .await;

        let (events, error, message) = s.guest_received_and_diagnosed_error();
        assert_eq!(
            (events, error),
            (vec![], (registry.id, wl_display::ERROR_INVALID_OBJECT))
        );
        assert!(message.len() <= 1024, "{} bytes", message.len());
    }
}
