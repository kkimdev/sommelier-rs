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

//! The guest's `ext_data_control_v1`, with `--data-control`: a client sets
//! the ChromeOS clipboard without keyboard focus, as a clipboard tool or
//! a detached copy helper does (Orca's `orca-wayland-clipboard`).
//!
//! Exo has no data-control protocol, and it takes a selection only from
//! the client whose surface has the keyboard focus (`Seat::SetSelection`
//! cancels any other source), quoting a serial it issued recently. A
//! data-control client never has focus, and each guest client has a host
//! connection of its own, so the proxy sets the selection on another
//! connection's behalf:
//!
//! - The guest's manager, devices and sources are local objects. A
//!   device's `set_selection` hands the source's MIME types to the
//!   [`ClipboardHub`] as a publication, whose carrier is the connection
//!   whose keyboard has the focus ([`DataControl::end_batch`]).
//! - The carrier creates a host `wl_data_source` the proxy alone uses,
//!   offers the types, and sets it as the selection of the guest's own
//!   `wl_data_device` with the serial of its latest input event
//!   ([`DataControl::note_serial`]). Exo then reads the data at once.
//! - The host's `send` reaches the source's client with the host's own
//!   descriptor, a VirtWL pipe the client writes, as the host's `send` of a
//!   guest `wl_data_source` does; no pump is needed. `cancelled` reaches
//!   it too, and the carrier destroys its host source.
//! - A source the client destroys is withdrawn: the carrier destroys its
//!   host source. ChromeOS keeps what it has read.
//!
//! The hub joins connections on the one thread they share: each connection
//! has a [`Mailbox`] its `Client` waits on beside its peers, and a
//! [`Delivery`] is handled there like a message, with effects of its own
//! (`proxy::dispatch::deliver`). A connection that ends leaves the hub,
//! which cancels what it carried and withdraws what it published, so no
//! route outlives either end.
//!
//! Only the regular selection is set, and only text the guest offers: the
//! devices announce an empty selection and no primary selection, and a
//! primary selection is cancelled. Reading the clipboard through
//! data-control is not supported; a focused client reads it through
//! `wl_data_device`. With no focused connection, or one without a
//! `wl_data_device` or an input serial, the source is cancelled, which a
//! client takes as another client's later copy.

use crate::handler::Handlers;
use crate::protocols::ext_data_control_v1::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};
use crate::protocols::wayland::{wl_data_device, wl_data_device_manager, wl_data_source};
use crate::protocols::Iface;
use crate::state::{Context, EventSender, GuestId, HostId, RequestObject, RequestSender};
use crate::wire::Action;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::rc::Rc;
use tokio::sync::Notify;

/// A connection, as the hub knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ConnectionId(u64);

/// One selection a guest source set, from its `set_selection` until it is
/// cancelled or withdrawn. Never reused, so a late delivery cannot reach a
/// later source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Publication(u64);

/// What one connection hands another through the hub.
#[derive(Debug)]
pub(crate) enum Delivery {
    /// To the carrier: set the owner's source, of `mime_types`, as the host
    /// selection.
    Publish {
        publication: Publication,
        mime_types: Vec<String>,
    },
    /// To the carrier: the owner's source ended.
    Withdraw { publication: Publication },
    /// To the owner: write the source's data as `mime_type` into `fd`.
    Send {
        publication: Publication,
        mime_type: String,
        fd: OwnedFd,
    },
    /// To the owner: the host no longer uses the source.
    Cancelled { publication: Publication },
}

/// Who publishes and who carries a publication.
struct Route {
    owner: ConnectionId,
    carrier: ConnectionId,
}

/// What a connection's `Client` waits on for deliveries.
#[derive(Default)]
pub(crate) struct Mailbox {
    deliveries: RefCell<VecDeque<Delivery>>,
    /// Wakes the `Client`; a wake with no waiter is kept for its next wait.
    ready: Notify,
}

impl Mailbox {
    /// Wait until a delivery may be waiting, forever without a mailbox.
    pub(crate) async fn ready(mailbox: Option<&Self>) {
        match mailbox {
            Some(mailbox) => mailbox.ready.notified().await,
            None => std::future::pending().await,
        }
    }

    /// The oldest delivery, if one is waiting.
    pub(crate) fn take(&self) -> Option<Delivery> {
        self.deliveries.borrow_mut().pop_front()
    }

    fn post(&self, delivery: Delivery) {
        self.deliveries.borrow_mut().push_back(delivery);
        self.ready.notify_one();
    }
}

#[derive(Default)]
struct HubState {
    next_connection: u64,
    next_publication: u64,
    mailboxes: BTreeMap<ConnectionId, Rc<Mailbox>>,
    /// The connection whose keyboard has the focus, as it last said.
    focused: Option<ConnectionId>,
    routes: BTreeMap<Publication, Route>,
}

impl HubState {
    /// Post `delivery` to `to`; one that has left drops it, which closes
    /// its descriptor.
    fn post(&self, to: ConnectionId, delivery: Delivery) {
        if let Some(mailbox) = self.mailboxes.get(&to) {
            mailbox.post(delivery);
        }
    }
}

/// The clipboard state every connection shares, with `--data-control`:
/// which connection has the focus, and the route of each publication.
#[derive(Clone, Default)]
pub(crate) struct ClipboardHub(Rc<RefCell<HubState>>);

impl ClipboardHub {
    /// A connection joins the hub until its [`Port`] drops.
    fn join(&self) -> Port {
        let mut state = self.0.borrow_mut();
        let id = ConnectionId(state.next_connection);
        state.next_connection += 1;
        let mailbox = Rc::new(Mailbox::default());
        state.mailboxes.insert(id, Rc::clone(&mailbox));
        Port {
            hub: self.clone(),
            id,
            mailbox,
        }
    }
}

/// One connection's membership in the hub.
struct Port {
    hub: ClipboardHub,
    id: ConnectionId,
    mailbox: Rc<Mailbox>,
}

impl Port {
    fn set_focused(&self, focused: bool) {
        let mut state = self.hub.0.borrow_mut();
        if focused {
            state.focused = Some(self.id);
        } else if state.focused == Some(self.id) {
            state.focused = None;
        }
    }

    /// Publish a source of `mime_types` through the focused connection;
    /// `None` without one.
    fn publish(&self, mime_types: Vec<String>) -> Option<Publication> {
        let mut state = self.hub.0.borrow_mut();
        let carrier = state.focused?;
        let publication = Publication(state.next_publication);
        state.next_publication += 1;
        let route = Route {
            owner: self.id,
            carrier,
        };
        state.routes.insert(publication, route);
        state.post(
            carrier,
            Delivery::Publish {
                publication,
                mime_types,
            },
        );
        Some(publication)
    }

    /// The owner's source of `publication` ended.
    fn withdraw(&self, publication: Publication) {
        let mut state = self.hub.0.borrow_mut();
        if let Some(route) = state.routes.remove(&publication) {
            state.post(route.carrier, Delivery::Withdraw { publication });
        }
    }

    /// The host asks the owner of `publication` for its data.
    fn send(&self, publication: Publication, mime_type: String, fd: OwnedFd) {
        let state = self.hub.0.borrow();
        if let Some(route) = state.routes.get(&publication) {
            let send = Delivery::Send {
                publication,
                mime_type,
                fd,
            };
            state.post(route.owner, send);
        }
    }

    /// The host no longer uses the carrier's source of `publication`.
    fn cancelled(&self, publication: Publication) {
        let mut state = self.hub.0.borrow_mut();
        if let Some(route) = state.routes.remove(&publication) {
            state.post(route.owner, Delivery::Cancelled { publication });
        }
    }
}

impl Drop for Port {
    /// The connection ended: what it carried is cancelled, and what it
    /// published withdrawn.
    fn drop(&mut self) {
        let mut state = self.hub.0.borrow_mut();
        state.mailboxes.remove(&self.id);
        if state.focused == Some(self.id) {
            state.focused = None;
        }
        let id = self.id;
        let ended: Vec<_> = state
            .routes
            .iter()
            .filter(|(_, route)| route.owner == id || route.carrier == id)
            .map(|(&publication, _)| publication)
            .collect();
        for publication in ended {
            let route = state.routes.remove(&publication).expect("a route");
            if route.owner == id {
                state.post(route.carrier, Delivery::Withdraw { publication });
            } else {
                state.post(route.owner, Delivery::Cancelled { publication });
            }
        }
    }
}

/// One connection's data-control state (`Context::data_control`).
#[derive(Default)]
pub(crate) struct DataControl {
    /// Its membership in the hub; `None` without `--data-control`, which
    /// leaves the connection's traffic as it is without the protocol.
    port: Option<Port>,
    /// Whether its keyboard had the focus at the end of the last batch.
    focused: bool,
    /// The serial of the latest input event the host sent it, which Exo
    /// accepts in a `set_selection`: it checks only that the serial is
    /// among the last it issued.
    latest_serial: Option<u32>,
    /// The host source the proxy created for each publication it carries,
    /// a host-only `wl_data_source`.
    carried: BTreeMap<HostId, Publication>,
}

impl DataControl {
    /// A connection's state, in `hub` if the proxy offers data-control.
    pub(crate) fn new(hub: Option<&ClipboardHub>) -> Self {
        Self {
            port: hub.map(ClipboardHub::join),
            ..Self::default()
        }
    }

    /// Whether the proxy offers the guest data-control.
    pub(crate) fn is_on(&self) -> bool {
        self.port.is_some()
    }

    /// Where the hub posts this connection's deliveries.
    pub(crate) fn mailbox(&self) -> Option<Rc<Mailbox>> {
        self.port.as_ref().map(|port| Rc::clone(&port.mailbox))
    }

    /// The host sent an input event with `serial`.
    pub(crate) fn note_serial(&mut self, serial: u32) {
        self.latest_serial = Some(serial);
    }

    /// A batch ended with the connection's keyboard `focused` or not.
    pub(crate) fn end_batch(&mut self, focused: bool) {
        if let Some(port) = &self.port {
            if std::mem::replace(&mut self.focused, focused) != focused {
                port.set_focused(focused);
            }
        }
    }

    /// The host sources the connection carries, for the orphan check.
    #[cfg(test)]
    pub(crate) fn carried_for_test(&self) -> impl Iterator<Item = HostId> + '_ {
        self.carried.keys().copied()
    }
}

/// A guest `ext_data_control_source_v1`'s state.
#[derive(Debug, Default)]
pub(crate) struct Source {
    mime_types: Vec<String>,
    /// Whether a device set it, which freezes its types.
    used: bool,
    /// Its selection while the hub routes it.
    publication: Option<Publication>,
}

/// Guest source `source`, with `data`, ended: its selection is withdrawn.
pub(crate) fn forget_source(ctx: &mut Context, source: Source) {
    if let (Some(publication), Some(port)) = (source.publication, &ctx.data_control.port) {
        port.withdraw(publication);
    }
}

/// Handle `delivery`, which the hub posted to this connection.
pub(crate) fn deliver(ctx: &mut Context, delivery: Delivery) {
    match delivery {
        Delivery::Publish {
            publication,
            mime_types,
        } => carry(ctx, publication, &mime_types),
        Delivery::Withdraw { publication } => {
            let host = ctx
                .data_control
                .carried
                .iter()
                .find_map(|(&host, &carried)| (carried == publication).then_some(host));
            if let Some(host) = host {
                destroy_carrier(ctx, host);
            }
        }
        Delivery::Send {
            publication,
            mime_type,
            fd,
        } => {
            if let Some(source) = published_source(ctx, publication) {
                ctx.send_guest_or_drop(
                    ext_data_control_source_v1::evt::send(source, &mime_type, fd),
                    format_args!("ext_data_control_source_v1.send of {mime_type:?}"),
                );
            }
        }
        Delivery::Cancelled { publication } => {
            if let Some(source) = published_source(ctx, publication) {
                if let Some(data) = ctx.shadow_table.data_mut::<Source>(source) {
                    data.publication = None;
                }
                ctx.send_guest(ext_data_control_source_v1::evt::cancelled(source));
            }
        }
    }
}

/// The live guest source whose selection is `publication`.
fn published_source(ctx: &Context, publication: Publication) -> Option<GuestId> {
    ctx.shadow_table
        .data_of::<Source>()
        .find_map(|(source, data)| (data.publication == Some(publication)).then_some(source))
}

/// Set `publication`, of `mime_types`, as the host selection through this
/// connection's own `wl_data_device`, or cancel it if the connection cannot.
fn carry(ctx: &mut Context, publication: Publication, mime_types: &[String]) {
    let device = ctx.shadow_table.live_hosts_of(Iface::WlDataDevice).next();
    let manager = ctx
        .shadow_table
        .live_hosts_of(Iface::WlDataDeviceManager)
        .next()
        .and_then(|manager| Some((manager, ctx.shadow_table.host_object_version(manager)?)));
    let (Some(device), Some((manager, version)), Some(serial)) =
        (device, manager, ctx.data_control.latest_serial)
    else {
        log::info!(
            "Cancelling a data-control selection: the focused client has no data device or \
             input serial"
        );
        if let Some(port) = &ctx.data_control.port {
            port.cancelled(publication);
        }
        return;
    };
    let source = ctx
        .shadow_table
        .create_host_only(Iface::WlDataSource, version);
    ctx.send_host(wl_data_device_manager::req::create_data_source(
        manager, source,
    ));
    for mime_type in mime_types {
        ctx.send_host_or_drop(
            wl_data_source::req::offer(source, mime_type),
            format_args!("a data-control offer of {mime_type:?}"),
        );
    }
    ctx.send_host(wl_data_device::req::set_selection(
        device,
        Some(source),
        serial,
    ));
    ctx.data_control.carried.insert(source, publication);
}

/// Destroy carried host source `host`.
fn destroy_carrier(ctx: &mut Context, host: HostId) {
    ctx.data_control.carried.remove(&host);
    ctx.send_host(wl_data_source::req::destroy(host));
    ctx.retire_destroyed_host_object(host);
}

/// The host's `wl_data_source.send` to `sender`: one the connection carries
/// goes to its owner, with the host's descriptor; any other is forwarded.
pub(crate) fn host_send(
    ctx: &mut Context,
    sender: EventSender,
    mime_type: &str,
    fd: BorrowedFd<'_>,
) -> Action {
    let Some(&publication) = ctx.data_control.carried.get(&sender.host) else {
        return Action::Forward;
    };
    match fd.try_clone_to_owned() {
        // The event closes its own descriptor once handled.
        Ok(fd) => {
            if let Some(port) = &ctx.data_control.port {
                port.send(publication, mime_type.to_string(), fd);
            }
        }
        Err(error) => log::error!("Cannot pass on a data-control transfer: {error}"),
    }
    Action::Drop
}

/// The host's `wl_data_source.cancelled` to `sender`: a source the
/// connection carries is cancelled for its owner and destroyed; any other
/// is forwarded.
pub(crate) fn host_cancelled(ctx: &mut Context, sender: EventSender) -> Action {
    let Some(&publication) = ctx.data_control.carried.get(&sender.host) else {
        return Action::Forward;
    };
    if let Some(port) = &ctx.data_control.port {
        port.cancelled(publication);
    }
    destroy_carrier(ctx, sender.host);
    Action::Drop
}

impl ext_data_control_manager_v1::ExtDataControlManagerV1Handler for Handlers {
    fn on_create_data_source(ctx: &mut Context, sender: RequestSender, id: GuestId) -> Action {
        ctx.shadow_table.create_local_with(
            id,
            Iface::ExtDataControlSourceV1,
            sender.version,
            Source::default(),
        );
        Action::Drop
    }

    fn on_get_data_device(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        _seat: RequestObject,
    ) -> Action {
        ctx.shadow_table
            .create_local(id, Iface::ExtDataControlDeviceV1, sender.version);
        // The first selection event is sent upon binding: data-control
        // reads no selection.
        ctx.send_guest(ext_data_control_device_v1::evt::selection(id, None));
        Action::Drop
    }
}

/// Mark `source` used by `device`'s request; `None` after a protocol error.
fn use_source(ctx: &mut Context, device: GuestId, source: GuestId) -> Option<Vec<String>> {
    let Some(data) = ctx.shadow_table.data_mut::<Source>(source) else {
        let _ = ctx.fail(format_args!("Data-control source {source} has no state"));
        return None;
    };
    if std::mem::replace(&mut data.used, true) {
        let _ = ctx.reject(
            device,
            ext_data_control_device_v1::ERROR_USED_SOURCE,
            format!("source {source} was already used"),
        );
        return None;
    }
    Some(data.mime_types.clone())
}

impl ext_data_control_device_v1::ExtDataControlDeviceV1Handler for Handlers {
    fn on_set_selection(
        ctx: &mut Context,
        sender: RequestSender,
        source: Option<RequestObject>,
    ) -> Action {
        // Exo ignores a null selection, and so does the proxy.
        let Some(source) = source.map(|source| source.guest) else {
            return Action::Drop;
        };
        let Some(mime_types) = use_source(ctx, sender.guest, source) else {
            return Action::Drop;
        };
        let publication = ctx
            .data_control
            .port
            .as_ref()
            .and_then(|port| port.publish(mime_types));
        match publication {
            Some(publication) => {
                if let Some(data) = ctx.shadow_table.data_mut::<Source>(source) {
                    data.publication = Some(publication);
                }
            }
            None => {
                log::info!("Cancelling a data-control selection: no client has the focus");
                ctx.send_guest(ext_data_control_source_v1::evt::cancelled(source));
            }
        }
        Action::Drop
    }

    fn on_set_primary_selection(
        ctx: &mut Context,
        sender: RequestSender,
        source: Option<RequestObject>,
    ) -> Action {
        // Exo has no primary selection.
        if let Some(source) = source.map(|source| source.guest) {
            if use_source(ctx, sender.guest, source).is_some() {
                ctx.send_guest(ext_data_control_source_v1::evt::cancelled(source));
            }
        }
        Action::Drop
    }
}

impl ext_data_control_source_v1::ExtDataControlSourceV1Handler for Handlers {
    fn on_offer(ctx: &mut Context, sender: RequestSender, mime_type: &String) -> Action {
        match ctx.shadow_table.data_mut::<Source>(sender.guest) {
            Some(data) if data.used => ctx.reject(
                sender.guest,
                ext_data_control_source_v1::ERROR_INVALID_OFFER,
                "offer after the source was used",
            ),
            Some(data) => {
                data.mime_types.push(mime_type.clone());
                Action::Drop
            }
            None => ctx.fail(format_args!(
                "Data-control source {} has no state",
                sender.guest
            )),
        }
    }
}

/// The proxy never creates an offer, so the guest has none to use.
impl ext_data_control_offer_v1::ExtDataControlOfferV1Handler for Handlers {}

#[cfg(test)]
mod tests;
