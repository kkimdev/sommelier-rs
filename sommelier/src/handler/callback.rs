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
use crate::protocols::wayland::wl_callback::{self, WlCallbackHandler};
use crate::protocols::wayland::wl_display;
use crate::protocols::Iface;
use crate::state::{Context, HostId, InternalCallback};
use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};
use log::debug;

/// Issue a host-only `wl_display.sync` whose `done` means `waits_for` has
/// happened (see [`InternalCallback`]).
///
/// # Returns
///
/// The callback's host ID, which stays reserved until the host's
/// `wl_display.delete_id`.
pub(crate) fn queue_internal_sync(ctx: &mut Context, waits_for: InternalCallback) -> HostId {
    let (callback, sync) = internal_sync(ctx, waits_for);
    ctx.client_to_host_queue.push(sync);
    callback
}

/// [`queue_internal_sync`]'s callback and its `wl_display.sync`, for the
/// caller to send.
pub(crate) fn internal_sync(ctx: &mut Context, waits_for: InternalCallback) -> (HostId, Outgoing) {
    let callback = HostId(
        ctx.shadow_table
            .create_host_only(Iface::WlCallback, wl_callback::VERSION),
    );
    ctx.internal_callbacks.register(callback, waits_for);
    (callback, wl_display::req::sync(WL_DISPLAY_ID, callback.0))
}

impl WlCallbackHandler for Handlers {
    fn on_done(ctx: &mut Context, callback_data: u32) -> Action {
        let host_id = ctx.last_sender_id;
        if let Some(waits_for) = ctx.internal_callbacks.complete(HostId(host_id)) {
            // wl_callback.done destroys the host callback resource. Reserve
            // its numeric ID until wl_display.delete_id arrives, while making
            // any duplicate event undispatchable immediately.
            ctx.retire_destroyed_host_object(host_id);
            match waits_for {
                InternalCallback::DmabufCapabilities { generation } => {
                    crate::handler::linux_dmabuf::complete_capability_discovery(ctx, generation);
                    crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(
                        ctx, generation,
                    );
                }
                InternalCallback::GtkShellCapabilities { gtk_shell } => {
                    crate::handler::gtk_shell::announce_capabilities(ctx, gtk_shell);
                }
                // W1 and W2: a text input's drain or reset fence.
                InternalCallback::TextInput { token } => {
                    crate::handler::text_input::ime(ctx, crate::ime::Input::BarrierDone(token));
                }
                // Every host event, this `done` included, has decided the
                // pending recovery before it is handled.
                InternalCallback::KeyRecovery => {}
                InternalCallback::PlacementBarrier { zaura_toplevel } => {
                    crate::handler::window_placement::complete_barrier(
                        ctx,
                        host_id,
                        zaura_toplevel,
                    );
                }
            }
            return Action::Drop;
        }
        let guest_id = ctx.shadow_table.get_guest_id(host_id).unwrap_or(0);

        if guest_id != 0 {
            debug!(
                "wl_callback.done for guest_id {}, sending done and delete_id",
                guest_id
            );

            // The done event, then the delete_id the guest expects for it.
            ctx.host_to_client_queue.extend([
                wl_callback::evt::done(guest_id, callback_data),
                wl_display::evt::delete_id(WL_DISPLAY_ID, guest_id),
            ]);

            // The terminal event destroys the callback on both sides, but the
            // host numeric ID cannot be reused until its later delete_id.
            ctx.guest_ended_host_destroyed(guest_id);
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use crate::handler::Handlers;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::protocols::Iface;
    use crate::state::{Context, HostId, InternalCallback};
    use crate::wire::Action;

    #[test]
    fn internal_sync_is_a_tracked_host_callback() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let waits_for = InternalCallback::DmabufCapabilities { generation: 9 };
        let callback = super::queue_internal_sync(&mut ctx, waits_for);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(
            crate::test_support::msg_sender(message),
            1,
            "wl_display must own the sync request"
        );
        assert_eq!(
            crate::test_support::msg_opcode(message),
            crate::protocols::wayland::wl_display::REQ_SYNC
        );
        assert_eq!(
            u32::from_ne_bytes(message[8..12].try_into().unwrap()),
            callback.0
        );
        assert_eq!(ctx.internal_callbacks.get(callback), Some(waits_for));
        assert_eq!(
            ctx.shadow_table.get_host_interface(callback.0),
            Some(Iface::WlCallback)
        );
    }

    #[test]
    fn internal_dmabuf_callback_completes_exact_generation() {
        let callback_id = 40;
        let generation = 7;
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.shadow_table
            .insert_host_only_for_test(callback_id, "wl_callback", 1);
        ctx.dmabuf_capabilities.entry(generation).or_default();
        ctx.host_dmabuf_generation = Some(generation);
        ctx.internal_callbacks.register(
            HostId(callback_id),
            InternalCallback::DmabufCapabilities { generation },
        );
        ctx.last_sender_id = callback_id;

        assert_eq!(Handlers::on_done(&mut ctx, 0), Action::Drop);
        assert!(ctx.dmabuf_capabilities[&generation].ready);
        assert_eq!(ctx.internal_callbacks.get(HostId(callback_id)), None);
        assert!(ctx.shadow_table.is_retired_host(callback_id));
    }

    #[test]
    fn guest_callback_reserves_host_id_until_real_delete_id() {
        let guest_id = 20;
        let host_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table
            .insert_paired_with_version_for_test(guest_id, host_id, "wl_callback", 1);
        ctx.last_sender_id = host_id;

        assert_eq!(Handlers::on_done(&mut ctx, 7), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(ctx.shadow_table.get_host_id(guest_id), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_id), None);
        assert!(ctx.shadow_table.is_retired_host(host_id));
        assert!(!ctx.shadow_table.is_host_id_available(host_id));

        ctx.last_sender_id = 1;
        assert_eq!(Handlers::on_delete_id(&mut ctx, host_id), Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "the guest already received its callback delete_id"
        );
        assert!(!ctx.shadow_table.is_retired_host(host_id));
        assert!(ctx.shadow_table.is_host_id_available(host_id));
    }

    /// A placement barrier retires at its `done`, even for a toplevel
    /// placement no longer knows, and its ID stays reserved until the
    /// host's `delete_id`.
    #[test]
    fn placement_barrier_completes_and_reserves_callback_until_delete_id() {
        let callback_id = 40;
        let mut ctx = Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::active_for_test(&[]),
        );
        ctx.shadow_table
            .insert_host_only_for_test(callback_id, "wl_callback", 1);
        ctx.internal_callbacks.register(
            HostId(callback_id),
            InternalCallback::PlacementBarrier { zaura_toplevel: 77 },
        );
        ctx.last_sender_id = callback_id;

        assert_eq!(Handlers::on_done(&mut ctx, 123), Action::Drop);
        assert_eq!(ctx.internal_callbacks.count(), 0);
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(ctx.host_to_client_queue.is_empty());
        assert!(ctx.shadow_table.is_retired_host(callback_id));
        assert!(!ctx.shadow_table.is_host_id_available(callback_id));

        ctx.last_sender_id = 1;
        assert_eq!(Handlers::on_delete_id(&mut ctx, callback_id), Action::Drop);
        assert!(ctx.shadow_table.is_host_id_available(callback_id));
    }
}
