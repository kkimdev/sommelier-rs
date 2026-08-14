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

use crate::protocols::wayland::wl_data_device;
use crate::protocols::wayland::wl_data_device_manager;
use crate::protocols::wayland::wl_data_offer;
use crate::protocols::wayland::wl_data_source;
use crate::state::Context;
use crate::wire::{Action, MessageBuilder};
use log::{debug, error};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::io::{IntoRawFd, RawFd};

pub struct DataDeviceHandler;

fn duplicate_client_fd(fd: RawFd) -> nix::Result<OwnedFd> {
    // The caller has already decoded this descriptor from a Wayland message
    // and owns it until proxy::handle_msgs closes the received copy.
    if fd < 0 {
        return Err(nix::errno::Errno::EBADF);
    }
    nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(fd) })
}

impl wl_data_device_manager::WlDataDeviceManagerHandler for DataDeviceHandler {
    fn on_create_data_source(&mut self, _ctx: &mut Context, id: u32) -> Action {
        debug!("Tracking data source id={}", id);
        Action::Forward
    }

    fn on_get_data_device(&mut self, _ctx: &mut Context, _id: u32, _seat: u32) -> Action {
        Action::Forward
    }
}

impl wl_data_device::WlDataDeviceHandler for DataDeviceHandler {
    fn on_data_offer(&mut self, _ctx: &mut Context, _id: u32) -> Action {
        Action::Forward
    }
}

impl wl_data_offer::WlDataOfferHandler for DataDeviceHandler {
    fn on_receive(&mut self, ctx: &mut Context, mime_type: &String, fd: i32) -> Action {
        debug!("wl_data_offer.receive: mime_type={}, fd={}", mime_type, fd);

        if let Some(virtwl) = &ctx.virtwayland_channel {
            debug!("Using virtwl for clipboard receive");

            // The guest-provided descriptor is consumed by the proxy after
            // this handler returns. Duplicate it before queueing anything so
            // a dup failure cannot leave a host request queued while the
            // original request is also forwarded.
            let client_fd = match duplicate_client_fd(fd) {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to dup client fd: {}", e);
                    return Action::Drop;
                }
            };

            // Create a pipe where we read (so host must write to it)
            // VIRTWL_IOCTL_NEW_PIPE_READ creates a pipe that is readable via the returned FD.
            let virtwl_pipe = match virtwl.create_pipe(true) {
                Ok(p) => p,
                Err(e) => {
                    error!("Failed to create virtwl pipe: {}", e);
                    return Action::Drop;
                }
            };

            // We need to send this virtwl FD to the host.
            // The guest client provided 'fd' to write data into.
            // We pump: virtwl_pipe (read) -> fd (write)

            let sender_id = ctx.last_sender_id;
            let Some(host_sender_id) = ctx.shadow_table.get_host_id(sender_id) else {
                error!(
                    "Dropping wl_data_offer.receive from unmapped sender {}",
                    sender_id
                );
                return Action::Drop;
            };
            debug!(
                "on_receive: sender_id={}, host_sender_id={}",
                sender_id, host_sender_id
            );

            let mut builder = MessageBuilder::new();
            builder.write_string(mime_type);

            let msg_data =
                match builder.try_build_message(host_sender_id, wl_data_offer::REQ_RECEIVE) {
                    Ok(message) => message,
                    Err(error) => {
                        error!("Dropping oversized clipboard MIME type: {}", error);
                        return Action::Drop;
                    }
                };

            // Dup virtwl FD
            // virtwl_pipe is OwnedFd, so it implements AsFd.
            let virtwl_fd_dup = match virtwl_pipe.try_clone() {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to dup virtwl fd: {}", e);
                    return Action::Drop;
                }
            };

            // Push to queue, converting OwnedFd to RawFd
            ctx.client_to_host_queue
                .push((msg_data, vec![virtwl_fd_dup.into_raw_fd()]));

            // Spawn pump task
            let pump_task = async move {
                let mut input = tokio::fs::File::from_std(std::fs::File::from(virtwl_pipe));
                let mut output = tokio::fs::File::from_std(std::fs::File::from(client_fd));

                if let Err(e) = tokio::io::copy(&mut input, &mut output).await {
                    error!("Clipboard pump error: {}", e);
                }
                debug!("Clipboard pump finished");
            };

            ctx.clipboard_pumps.push(tokio::spawn(pump_task));

            // Drop original action (we handled it)
            return Action::Drop;
        }

        Action::Forward
    }
}

impl wl_data_source::WlDataSourceHandler for DataDeviceHandler {}

#[cfg(test)]
mod tests {
    use super::{duplicate_client_fd, DataDeviceHandler};
    use crate::protocols::wayland::wl_data_device;
    use crate::state::Context;
    use crate::wire::WireMessage;
    use std::os::fd::AsRawFd;

    #[test]
    fn duplicate_client_fd_rejects_invalid_descriptor() {
        assert!(duplicate_client_fd(-1).is_err());
    }

    #[test]
    fn duplicate_client_fd_returns_an_owned_duplicate() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let duplicate = duplicate_client_fd(pipe_fds[1]).expect("dup should succeed");
        assert_ne!(duplicate.as_raw_fd(), pipe_fds[1]);

        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
    }

    #[test]
    fn host_data_offer_gets_a_guest_server_id_mapping() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        // Host wl_data_device object 80 is represented by guest object 10.
        ctx.shadow_table.map_id(10, 80);
        ctx.shadow_table
            .track_interface(10, "wl_data_device".to_string());

        // wl_data_device.data_offer(new_id wl_data_offer) is a host-generated
        // object event. The host is free to choose any ID; it must not be
        // forwarded verbatim into the guest namespace.
        let host_offer_id: u32 = 123;
        let payload = host_offer_id.to_ne_bytes();
        let mut msg = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload, &[]);
        let mut handler = DataDeviceHandler;
        let result = wl_data_device::dispatch_event(&mut msg, &mut handler, &mut ctx)
            .expect("data_offer should decode")
            .expect("data_offer should be forwarded");

        let (wire, fds) = result;
        assert!(fds.is_empty());
        assert_eq!(
            u32::from_ne_bytes(wire[0..4].try_into().unwrap()),
            10,
            "event sender must be translated to the guest object"
        );
        let guest_offer_id = u32::from_ne_bytes(wire[8..12].try_into().unwrap());
        assert_ne!(guest_offer_id, host_offer_id);
        assert!(
            guest_offer_id >= 0xff00_0000,
            "server-generated guest IDs must use Wayland's reserved range"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_offer_id),
            Some(host_offer_id)
        );
        assert_eq!(
            ctx.shadow_table.get_interface(guest_offer_id),
            Some(&"wl_data_offer".to_string())
        );
    }

    #[test]
    fn host_data_offer_rejects_reused_host_object_id() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table.map_id(10, 80);
        ctx.shadow_table
            .track_interface(10, "wl_data_device".to_string());
        ctx.shadow_table
            .track_host_interface(123, "wl_surface".to_string());

        let payload = 123u32.to_ne_bytes();
        let mut msg = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload, &[]);
        let mut handler = DataDeviceHandler;
        let result = wl_data_device::dispatch_event(&mut msg, &mut handler, &mut ctx);

        assert_eq!(
            result,
            Err(crate::wire::ProtocolError::InvalidObjectId(123))
        );
        assert!(
            ctx.shadow_table.get_guest_id(123).is_none(),
            "a colliding host ID must not overwrite an existing mapping"
        );
    }

    #[test]
    fn destroyed_host_data_offer_allows_ordered_server_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, Vec::new());
        ctx.shadow_table.map_id(10, 80);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_data_device".to_string(), 3);
        let host_offer_id = 0xff00_0000u32;
        let mut handler = DataDeviceHandler;

        let payload = host_offer_id.to_ne_bytes();
        let mut first_offer = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload, &[]);
        let (first_event, _) =
            wl_data_device::dispatch_event(&mut first_offer, &mut handler, &mut ctx)
                .expect("first data_offer should decode")
                .expect("first data_offer should be forwarded");
        let first_guest_id = u32::from_ne_bytes(first_event[8..12].try_into().unwrap());
        assert_eq!(
            ctx.shadow_table.get_host_id(first_guest_id),
            Some(host_offer_id)
        );

        let mut destroy = WireMessage::new(
            first_guest_id,
            crate::protocols::wayland::wl_data_offer::REQ_DESTROY,
            &[],
            &[],
        );
        let (destroy_request, _) = crate::protocols::wayland::wl_data_offer::dispatch_request(
            &mut destroy,
            &mut handler,
            &mut ctx,
        )
        .expect("data_offer.destroy should decode")
        .expect("data_offer.destroy should be forwarded");
        assert_eq!(
            u32::from_ne_bytes(destroy_request[0..4].try_into().unwrap()),
            host_offer_id
        );
        assert_eq!(ctx.shadow_table.get_host_id(first_guest_id), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_offer_id), None);
        assert!(!ctx.shadow_table.is_pending_destroy_guest(first_guest_id));

        let mut replacement = WireMessage::new(80, wl_data_device::EVT_DATA_OFFER, &payload, &[]);
        let (replacement_event, _) =
            wl_data_device::dispatch_event(&mut replacement, &mut handler, &mut ctx)
                .expect("reused server ID should decode")
                .expect("replacement data_offer should be forwarded");
        let replacement_guest_id = u32::from_ne_bytes(replacement_event[8..12].try_into().unwrap());
        assert_ne!(replacement_guest_id, first_guest_id);
        assert_eq!(
            ctx.shadow_table.get_host_id(replacement_guest_id),
            Some(host_offer_id)
        );
    }
}
