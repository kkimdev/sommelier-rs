//! Tests of `handler::data_control`: a guest data-control client and the
//! connection whose keyboard has the focus, two sessions of one hub.

use super::ClipboardHub;
use crate::protocols::ext_data_control_v1::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_source_v1,
};
use crate::protocols::keyboard_extension_unstable_v1::zcr_keyboard_extension_v1;
use crate::protocols::wayland::{
    wl_data_device, wl_data_device_manager, wl_data_source, wl_display, wl_keyboard, wl_seat,
};
use crate::protocols::{AnyEvent, AnyRequest};
use crate::proxy::harness::{Object, Session};
use crate::settings::Settings;
use crate::state::{ConnectionFacts, Context};
use crate::test_support::{any_fd_has_identity, fd_identity};
use crate::wire::WL_DISPLAY_ID;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

/// How long a running client may take to carry a selection.
const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The MIME types Orca's copy helper offers.
const TEXT: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// A session of a connection in `hub`.
fn session(hub: &ClipboardHub) -> Session {
    let facts = ConnectionFacts {
        clipboard: Some(hub.clone()),
        ..ConnectionFacts::for_test()
    };
    Session::with_context(Context::new(Rc::new(Settings::for_test()), facts))
}

/// A focused client: a `wl_data_device` and a keyboard whose focus the
/// host gives it with `serial`.
struct Focused {
    s: Session,
    keyboard: Object,
    surface: Object,
    manager: Object,
    device: Object,
}

impl Focused {
    async fn new(hub: &ClipboardHub, serial: u32) -> Self {
        let mut s = session(hub);
        let (seat, keyboard) = s.keyboard().await;
        let manager = s
            .bind_paired("wl_data_device_manager", wl_data_device_manager::VERSION)
            .await;
        let id = s.new_id();
        let host = s
            .create(wl_data_device_manager::req::get_data_device(
                manager.id, id, seat,
            ))
            .await;
        let surface = s.surface().await;
        let enter = wl_keyboard::evt::enter(keyboard.host, serial, surface.host, &[]).unwrap();
        s.host([enter]).await;
        assert_eq!(s.guest_received().len(), 2, "the enter and its modifiers");
        Self {
            s,
            keyboard,
            surface,
            manager,
            device: Object { id, host },
        }
    }

    /// The host's selection, from the proxy's own source of `TEXT` with
    /// `serial`; returns that source.
    fn selection_set(&mut self, serial: u32) -> u32 {
        let host = self.s.host_received();
        let [(manager, AnyRequest::WlDataDeviceManager(create)), offers @ .., (device, AnyRequest::WlDataDevice(set))] =
            &host[..]
        else {
            panic!("the host got no selection: {host:?}");
        };
        let wl_data_device_manager::Request::CreateDataSource { id: source } = *create else {
            panic!("the host got no source: {host:?}");
        };
        assert_eq!((*manager, *device), (self.manager.host, self.device.host));
        let offered: Vec<_> = TEXT
            .iter()
            .map(|mime_type| {
                let offer = wl_data_source::Request::Offer {
                    mime_type: mime_type.to_string(),
                };
                (source, offer.into())
            })
            .collect();
        assert_eq!(offers, offered);
        assert_eq!(
            *set,
            wl_data_device::Request::SetSelection { source, serial }
        );
        source
    }
}

/// A data-control client: its device, which has announced no selection.
struct Controller {
    s: Session,
    manager: u32,
    device: u32,
}

impl Controller {
    async fn new(hub: &ClipboardHub) -> Self {
        let mut s = session(hub);
        // The proxy offers data-control under the keyboard extension's
        // name, and binds that extension for itself.
        let manager = s
            .bind(
                zcr_keyboard_extension_v1::NAME,
                zcr_keyboard_extension_v1::VERSION,
            )
            .await;
        let seat = s.bind("wl_seat", wl_seat::VERSION).await;
        let device = s.new_id();
        s.guest([ext_data_control_manager_v1::req::get_data_device(
            manager, device, seat,
        )])
        .await;
        assert_eq!(
            s.guest_received(),
            [(
                device,
                ext_data_control_device_v1::Event::Selection { id: 0 }.into()
            )]
        );
        Self { s, manager, device }
    }

    /// A new source of `TEXT`, which the device sets as the selection.
    async fn copy(&mut self) -> u32 {
        let source = self.s.new_id();
        let mut requests = vec![ext_data_control_manager_v1::req::create_data_source(
            self.manager,
            source,
        )];
        for mime_type in TEXT {
            requests.push(ext_data_control_source_v1::req::offer(source, mime_type).unwrap());
        }
        requests.push(ext_data_control_device_v1::req::set_selection(
            self.device,
            Some(source),
        ));
        self.s.guest(requests).await;
        source
    }
}

/// The guest got `cancelled` for `source`, and nothing else.
fn cancelled(source: u32) -> Vec<(u32, AnyEvent)> {
    vec![(
        source,
        ext_data_control_source_v1::Event::Cancelled {}.into(),
    )]
}

/// A data-control client's selection reaches the host on the connection
/// whose keyboard has the focus, quoting that connection's latest input
/// serial. The host reads it through the client's own descriptor: the
/// proxy keeps no transfer of its own, so nothing of it waits on the
/// proxy. Its cancellation reaches the client, and ends the proxy's source.
#[tokio::test]
async fn a_data_control_selection_reaches_the_host_through_the_focused_connection() {
    let hub = ClipboardHub::default();
    let mut focused = Focused::new(&hub, 7).await;
    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    assert!(controller.s.host_received().is_empty());

    assert!(focused.s.deliver().await);
    let host_source = focused.selection_set(7);

    let (read, write) = std::io::pipe().unwrap();
    let write = OwnedFd::from(write);
    let identity = fd_identity(write.as_raw_fd());
    let send = wl_data_source::evt::send(host_source, TEXT[0], write).unwrap();
    focused.s.host([send]).await;
    assert!(focused.s.guest_received().is_empty());
    assert!(controller.s.deliver().await);
    let received = controller.s.guest_received();
    let [(
        sender,
        AnyEvent::ExtDataControlSourceV1(ext_data_control_source_v1::Event::Send { mime_type, fd }),
    )] = &received[..]
    else {
        panic!("the client was not asked for its data: {received:?}");
    };
    assert_eq!((*sender, mime_type.as_str()), (source, TEXT[0]));
    assert_eq!(fd_identity(fd.as_raw_fd()), identity, "the host's own pipe");
    drop(received);
    drop(read);
    assert!(!any_fd_has_identity(identity), "the proxy kept no copy");

    focused
        .s
        .host([wl_data_source::evt::cancelled(host_source)])
        .await;
    assert_eq!(
        focused.s.host_received(),
        [(host_source, wl_data_source::Request::Destroy {}.into())]
    );
    assert!(controller.s.deliver().await);
    assert_eq!(controller.s.guest_received(), cancelled(source));
}

/// With no connection focused, a selection is cancelled at once, and the
/// host gets nothing.
#[tokio::test]
async fn without_a_focused_connection_a_data_control_selection_is_cancelled() {
    let hub = ClipboardHub::default();
    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    assert_eq!(controller.s.guest_received(), cancelled(source));
    assert!(controller.s.host_received().is_empty());
}

/// A source ends with its client: what the host still asks of it is
/// refused, its descriptor closed, and the focused connection keeps its
/// own source, so the host's late events have an object to reach, until
/// the host cancels it. A focused connection that ends cancels what it
/// carried.
#[tokio::test]
async fn a_selection_ends_with_either_connection() {
    let hub = ClipboardHub::default();
    let mut focused = Focused::new(&hub, 3).await;
    let mut controller = Controller::new(&hub).await;
    controller.copy().await;
    assert!(focused.s.deliver().await);
    let host_source = focused.selection_set(3);
    drop(controller);
    assert!(focused.s.deliver().await);
    assert!(focused.s.host_received().is_empty());

    let (read, write) = std::io::pipe().unwrap();
    let write = OwnedFd::from(write);
    let identity = fd_identity(write.as_raw_fd());
    let send = wl_data_source::evt::send(host_source, TEXT[0], write).unwrap();
    focused.s.host([send]).await;
    drop(read);
    assert!(
        !any_fd_has_identity(identity),
        "the refused descriptor is closed"
    );
    focused
        .s
        .host([wl_data_source::evt::cancelled(host_source)])
        .await;
    assert_eq!(
        focused.s.host_received(),
        [(host_source, wl_data_source::Request::Destroy {}.into())]
    );

    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    assert!(focused.s.deliver().await);
    focused.selection_set(3);
    drop(focused);
    assert!(controller.s.deliver().await);
    assert_eq!(controller.s.guest_received(), cancelled(source));
}

/// A source is set once, and offers nothing after: as the protocol says,
/// either is an error.
#[tokio::test]
async fn a_used_source_is_set_and_offered_to_no_more() {
    let hub = ClipboardHub::default();
    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    controller.s.guest_received();
    controller
        .s
        .guest_fatal([ext_data_control_device_v1::req::set_selection(
            controller.device,
            Some(source),
        )])
        .await;
    let (events, error) = controller.s.guest_received_and_error();
    assert!(events.is_empty());
    assert_eq!(
        error,
        (
            controller.device,
            ext_data_control_device_v1::ERROR_USED_SOURCE
        )
    );

    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    controller.s.guest_received();
    controller
        .s
        .guest_fatal([ext_data_control_source_v1::req::offer(source, "text/html").unwrap()])
        .await;
    let (_, error) = controller.s.guest_received_and_error();
    assert_eq!(
        error,
        (source, ext_data_control_source_v1::ERROR_INVALID_OFFER)
    );
}

/// The selection quotes the serial of the focused connection's latest
/// input event, which Exo still holds, not that of its focus, which a long
/// stretch of typing may have pushed out of the serials Exo keeps.
#[tokio::test]
async fn a_selection_quotes_the_focused_connections_latest_input_serial() {
    let hub = ClipboardHub::default();
    let mut focused = Focused::new(&hub, 3).await;
    let key = wl_keyboard::evt::key(focused.keyboard.host, 9, 0, 30, 1);
    focused.s.host([key]).await;
    assert_eq!(focused.s.guest_received().len(), 1, "the key");
    let mut controller = Controller::new(&hub).await;
    controller.copy().await;
    assert!(focused.s.deliver().await);
    focused.selection_set(9);
}

/// A focused connection without a `wl_data_device` cannot set a
/// selection, so it cancels the source.
#[tokio::test]
async fn a_focused_connection_without_a_data_device_cancels_the_selection() {
    let hub = ClipboardHub::default();
    let mut focused = session(&hub);
    let (_, keyboard) = focused.keyboard().await;
    let surface = focused.surface().await;
    let enter = wl_keyboard::evt::enter(keyboard.host, 5, surface.host, &[]).unwrap();
    focused.host([enter]).await;
    focused.guest_received();
    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    assert!(focused.deliver().await);
    assert!(focused.host_received().is_empty());
    assert!(controller.s.deliver().await);
    assert_eq!(controller.s.guest_received(), cancelled(source));
}

/// A running client handles a delivery as it arrives, beside its peers,
/// and one that ends cancels what it carried.
#[tokio::test]
async fn a_running_client_carries_a_selection_as_it_is_set() {
    let hub = ClipboardHub::default();
    let mut focused = Focused::new(&hub, 7).await;
    let mut controller = Controller::new(&hub).await;
    let mut source = 0;
    focused
        .s
        .run(async |running| {
            source = controller.copy().await;
            let mut host = Vec::new();
            while !matches!(host.last(), Some((_, AnyRequest::WlDataDevice(_)))) {
                let received = tokio::time::timeout(WAIT, running.host_received());
                host.extend(received.await.expect("the client carries the selection"));
            }
        })
        .await;
    assert!(controller.s.deliver().await);
    assert_eq!(controller.s.guest_received(), cancelled(source));
}

/// A source its client destroys before the focused connection sets it is
/// never set: Exo would read it at once, from a source that is gone, and
/// empty the ChromeOS clipboard.
#[tokio::test]
async fn a_source_withdrawn_before_it_is_carried_is_never_set() {
    let hub = ClipboardHub::default();
    let mut focused = Focused::new(&hub, 7).await;
    let mut controller = Controller::new(&hub).await;
    let source = controller.copy().await;
    controller
        .s
        .guest([ext_data_control_source_v1::req::destroy(source)])
        .await;
    let deleted = wl_display::Event::DeleteId { id: source };
    assert_eq!(
        controller.s.guest_received(),
        [(WL_DISPLAY_ID, deleted.into())]
    );
    assert!(focused.s.deliver().await);
    assert!(focused.s.host_received().is_empty());
}

/// Each enter of a connection's keyboard makes it the carrier, so a focus
/// that leaves and comes back within one batch, after another connection
/// was entered, is still the carrier once that other one is left.
#[tokio::test]
async fn a_focus_regained_within_one_batch_carries_the_selection() {
    let hub = ClipboardHub::default();
    let mut first = Focused::new(&hub, 1).await;
    let mut second = Focused::new(&hub, 2).await;
    let (keyboard, surface) = (first.keyboard.host, first.surface.host);
    let leave = wl_keyboard::evt::leave(keyboard, 3, surface);
    let enter = wl_keyboard::evt::enter(keyboard, 4, surface, &[]).unwrap();
    first.s.host([leave, enter]).await;
    first.s.guest_received();
    let leave = wl_keyboard::evt::leave(second.keyboard.host, 5, second.surface.host);
    second.s.host([leave]).await;
    second.s.guest_received();

    let mut controller = Controller::new(&hub).await;
    controller.copy().await;
    assert!(first.s.deliver().await);
    first.selection_set(4);
    assert!(controller.s.guest_received().is_empty());
}
