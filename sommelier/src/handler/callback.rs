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

use crate::protocols::wayland::wl_callback::{self, WlCallbackHandler};
use crate::protocols::wayland::wl_display;
use crate::state::{Context, HostId, InternalCallback};
use crate::wire::{Action, WL_DISPLAY_ID};
use log::debug;

pub struct CallbackHandler;

/// Issue a host-only `wl_display.sync` whose `done` means `waits_for` has
/// happened (see [`InternalCallback`]).
///
/// # Returns
///
/// The callback's host ID, which stays reserved until the host's
/// `wl_display.delete_id`.
pub(crate) fn queue_internal_sync(ctx: &mut Context, waits_for: InternalCallback) -> HostId {
    let callback = HostId(
        ctx.shadow_table
            .create_host_only(wl_callback::NAME, wl_callback::VERSION),
    );
    ctx.internal_callbacks.register(callback, waits_for);
    ctx.client_to_host_queue
        .push(wl_display::req::sync(WL_DISPLAY_ID, callback.0));
    callback
}

impl WlCallbackHandler for CallbackHandler {
    fn on_done(&mut self, ctx: &mut Context, callback_data: u32) -> Action {
        let host_id = ctx.last_sender_id;
        if let Some(waits_for) = ctx.internal_callbacks.complete(HostId(host_id)) {
            // wl_callback.done destroys the host callback resource. Reserve
            // its numeric ID until wl_display.delete_id arrives, while making
            // any duplicate event undispatchable immediately.
            ctx.retire_destroyed_host_object(host_id);
            match waits_for {
                InternalCallback::DmabufCapabilities { generation } => {
                    crate::handler::linux_dmabuf::LinuxDmabufHandler::complete_capability_discovery(
                        ctx, generation,
                    );
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
                InternalCallback::WindowBoundsBarrier { zaura_toplevel } => {
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
    use super::CallbackHandler;
    use crate::handler::display::DisplayHandler;
    use crate::handler::window_placement::register_window_bounds_barrier_for_test;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::state::{Context, HostId, InternalCallback};
    use crate::wire::Action;

    /// A context whose placement uses the ARC backend, whose barriers these
    /// tests complete.
    fn arc_context() -> Context {
        Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::arc_bounds_for_test(""),
        )
    }

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
            Some("wl_callback")
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

        let mut handler = CallbackHandler;
        assert_eq!(handler.on_done(&mut ctx, 0), Action::Drop);
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

        let mut handler = CallbackHandler;
        assert_eq!(handler.on_done(&mut ctx, 7), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(ctx.shadow_table.get_host_id(guest_id), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_id), None);
        assert!(ctx.shadow_table.is_retired_host(host_id));
        assert!(!ctx.shadow_table.is_host_id_available(host_id));

        ctx.last_sender_id = 1;
        assert_eq!(DisplayHandler.on_delete_id(&mut ctx, host_id), Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "the guest already received its callback delete_id"
        );
        assert!(!ctx.shadow_table.is_retired_host(host_id));
        assert!(ctx.shadow_table.is_host_id_available(host_id));
    }

    #[test]
    fn window_bounds_barrier_completes_and_reserves_callback_until_delete_id() {
        let zaura_toplevel_id = 77;
        let callback_id = 40;
        let mut ctx = arc_context();
        ctx.shadow_table
            .insert_host_only_for_test(callback_id, "wl_callback", 1);
        register_window_bounds_barrier_for_test(&mut ctx, callback_id, zaura_toplevel_id);
        ctx.last_sender_id = callback_id;

        let mut handler = CallbackHandler;
        assert_eq!(handler.on_done(&mut ctx, 123), Action::Drop);
        assert_eq!(ctx.internal_callbacks.count(), 0);
        assert!(!ctx
            .window_placement
            .arc()
            .is_some_and(|arc| arc.has_active_barrier(zaura_toplevel_id)));
        assert!(ctx.shadow_table.is_retired_host(callback_id));
        assert!(!ctx.shadow_table.is_host_id_available(callback_id));

        ctx.last_sender_id = 1;
        assert_eq!(
            DisplayHandler.on_delete_id(&mut ctx, callback_id),
            Action::Drop
        );
        assert!(ctx.shadow_table.is_host_id_available(callback_id));
    }

    #[test]
    fn stale_window_bounds_barrier_does_not_clear_newer_barrier() {
        let zaura_toplevel_id = 77;
        let old_callback_id = 40;
        let new_callback_id = 41;
        let mut ctx = arc_context();
        for callback_id in [old_callback_id, new_callback_id] {
            ctx.shadow_table
                .insert_host_only_for_test(callback_id, "wl_callback", 1);
        }
        register_window_bounds_barrier_for_test(&mut ctx, old_callback_id, zaura_toplevel_id);
        register_window_bounds_barrier_for_test(&mut ctx, new_callback_id, zaura_toplevel_id);

        let mut handler = CallbackHandler;
        ctx.last_sender_id = old_callback_id;
        assert_eq!(handler.on_done(&mut ctx, 1), Action::Drop);
        assert_eq!(
            ctx.window_placement
                .arc()
                .and_then(|arc| arc.active_barrier_for_toplevel(zaura_toplevel_id)),
            Some(new_callback_id)
        );
        assert!(ctx.shadow_table.is_retired_host(old_callback_id));
        assert_eq!(
            ctx.internal_callbacks.get(HostId(new_callback_id)),
            Some(InternalCallback::WindowBoundsBarrier {
                zaura_toplevel: zaura_toplevel_id
            })
        );

        ctx.last_sender_id = new_callback_id;
        assert_eq!(handler.on_done(&mut ctx, 2), Action::Drop);
        assert!(!ctx
            .window_placement
            .arc()
            .is_some_and(|arc| arc.has_active_barrier(zaura_toplevel_id)));
        assert_eq!(ctx.internal_callbacks.count(), 0);
        assert!(ctx.shadow_table.is_retired_host(new_callback_id));
    }
}
