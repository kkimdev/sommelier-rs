//! Tests of `handler::security_context`.

use super::{SandboxListener, SandboxListeners};
use crate::handler::data_control::ClipboardHub;
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::security_context_v1::{
    wp_security_context_manager_v1, wp_security_context_v1,
};
use crate::protocols::text_input_extension_unstable_v1::zcr_text_input_extension_v1;
use crate::proxy::harness::Session;
use crate::settings::Settings;
use crate::state::{ConnectionFacts, Context};
use crate::test_support::{fd_identity, TempDir};
use crate::wire::{Outgoing, WL_DISPLAY_ID};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedReceiver;

/// A session whose connection is `sandboxed` or not, offered data-control
/// and, unless sandboxed, the security-context manager; returns it and
/// where its committed contexts arrive.
fn session(sandboxed: bool) -> (Session, UnboundedReceiver<SandboxListener>) {
    let (listeners, committed): (SandboxListeners, _) = tokio::sync::mpsc::unbounded_channel();
    let facts = ConnectionFacts {
        clipboard: Some(ClipboardHub::default()),
        sandbox_listeners: Some(listeners),
        sandboxed,
        ..ConnectionFacts::for_test()
    };
    let ctx = Context::new(Rc::new(Settings::for_test()), facts);
    (Session::with_context(ctx), committed)
}

/// A protocol error case: what it is, the requests after the context's
/// creation that make it, and the error.
type ErrorCase = (&'static str, fn(u32) -> Vec<Outgoing<u32>>, u32);

/// A listening socket, in `dir`, and a close descriptor whose hangup is
/// `hang_up`'s drop.
fn socket_and_close(dir: &TempDir) -> (OwnedFd, OwnedFd, std::io::PipeWriter) {
    let listener = std::os::unix::net::UnixListener::bind(dir.path().join("sandbox")).unwrap();
    let (close, hang_up) = std::io::pipe().unwrap();
    (listener.into(), close.into(), hang_up)
}

/// The guest binds the manager, which the proxy offers under the host's
/// `zcr_text_input_extension_v1`; returns the guest's manager.
async fn manager(s: &mut Session) -> u32 {
    s.bind(
        zcr_text_input_extension_v1::NAME,
        zcr_text_input_extension_v1::VERSION,
    )
    .await
}

/// A security context's commit hands the proxy its listening socket, with
/// the engine's metadata, so that it accepts the engine's sandboxed
/// clients; nothing of it reaches the host.
#[tokio::test]
async fn a_committed_security_context_hands_its_socket_to_the_proxy() {
    let (mut s, mut committed) = session(false);
    let manager = manager(&mut s).await;
    let dir = TempDir::new("security-context");
    let (listen, close, _hang_up) = socket_and_close(&dir);
    let socket = fd_identity(listen.as_raw_fd());
    let context = s.new_id();
    s.guest([
        wp_security_context_manager_v1::req::create_listener(manager, context, listen, close),
        wp_security_context_v1::req::set_sandbox_engine(context, "org.flatpak").unwrap(),
        wp_security_context_v1::req::set_app_id(context, "org.example.App").unwrap(),
        wp_security_context_v1::req::set_instance_id(context, "7").unwrap(),
        wp_security_context_v1::req::commit(context),
        wp_security_context_v1::req::destroy(context),
    ])
    .await;
    let deleted = crate::protocols::wayland::wl_display::Event::DeleteId { id: context };
    assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
    assert!(s.host_received().is_empty());

    let listener = committed.try_recv().expect("the proxy got the context");
    assert_eq!(fd_identity(listener.listen.as_raw_fd()), socket);
    assert_eq!(listener.metadata.engine.as_deref(), Some("org.flatpak"));
    assert_eq!(listener.metadata.app_id.as_deref(), Some("org.example.App"));
    assert_eq!(listener.metadata.instance_id.as_deref(), Some("7"));
}

/// The protocol's errors: metadata set twice, a request after the commit,
/// and a descriptor that is no listening socket.
#[tokio::test]
async fn a_security_context_is_set_once_and_listens_on_a_socket() {
    let dir = TempDir::new("security-context-errors");
    let cases: [ErrorCase; 3] = [
        (
            "an app ID set twice",
            |context| {
                vec![
                    wp_security_context_v1::req::set_app_id(context, "a").unwrap(),
                    wp_security_context_v1::req::set_app_id(context, "b").unwrap(),
                ]
            },
            wp_security_context_v1::ERROR_ALREADY_SET,
        ),
        (
            "an engine set after the commit",
            |context| {
                vec![
                    wp_security_context_v1::req::commit(context),
                    wp_security_context_v1::req::set_sandbox_engine(context, "x").unwrap(),
                ]
            },
            wp_security_context_v1::ERROR_ALREADY_USED,
        ),
        (
            "a second commit",
            |context| {
                vec![
                    wp_security_context_v1::req::commit(context),
                    wp_security_context_v1::req::commit(context),
                ]
            },
            wp_security_context_v1::ERROR_ALREADY_USED,
        ),
    ];
    for (case, requests, code) in cases {
        let (mut s, _committed) = session(false);
        let manager = manager(&mut s).await;
        let (listen, close, _hang_up) = socket_and_close(&TempDir::new("security-context-case"));
        let context = s.new_id();
        let create =
            wp_security_context_manager_v1::req::create_listener(manager, context, listen, close);
        s.guest_fatal(std::iter::once(create).chain(requests(context)))
            .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (context, code)),
            "{case}"
        );
    }

    let (mut s, _committed) = session(false);
    let manager = manager(&mut s).await;
    let (not_listening, close) = std::io::pipe().unwrap();
    let context = s.new_id();
    s.guest_fatal([wp_security_context_manager_v1::req::create_listener(
        manager,
        context,
        not_listening.into(),
        close.into(),
    )])
    .await;
    assert_eq!(
        s.guest_received_and_error(),
        (
            vec![],
            (
                manager,
                wp_security_context_manager_v1::ERROR_INVALID_LISTEN_FD
            )
        )
    );
    drop(dir);
}

/// A connection that came through a security context is offered neither
/// the manager, so contexts never nest, nor data-control, which would let
/// it read or replace the clipboard without the focus.
#[tokio::test]
async fn a_sandboxed_connection_is_offered_no_privileged_global() {
    let (mut s, _committed) = session(true);
    s.advertise(
        zcr_text_input_extension_v1::NAME,
        zcr_text_input_extension_v1::VERSION,
    )
    .await;
    s.advertise(
        zcr_keyboard_extension_v1::NAME,
        zcr_keyboard_extension_v1::VERSION,
    )
    .await;
    assert!(s.guest_received().is_empty(), "the guest sees no global");
    s.host_received();
}
