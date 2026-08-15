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

use crate::protocols;
use crate::protocols::wayland::wl_callback::WlCallbackHandler;
use crate::state::{Context, HostId};
use crate::wire::{Action, MessageBuilder};
use log::debug;

pub struct CallbackHandler;

impl WlCallbackHandler for CallbackHandler {
    fn on_done(&mut self, ctx: &mut Context, callback_data: u32) -> Action {
        let host_id = ctx.last_sender_id;
        if crate::handler::text_input::complete_host_activation_barrier(ctx, HostId(host_id)) {
            return Action::Drop;
        }
        if let Some(generation) = ctx.dmabuf_capability_callbacks.remove(&host_id) {
            // wl_callback.done destroys the host callback resource. Reserve its
            // numeric ID until wl_display.delete_id arrives, while making any
            // duplicate event undispatchable immediately.
            if !ctx.shadow_table.mark_pending_destroy_host(host_id) {
                log::warn!(
                    "linux-dmabuf capability callback {} was not tracked as host-only",
                    host_id
                );
            }
            crate::handler::linux_dmabuf::LinuxDmabufHandler::complete_capability_discovery(
                ctx, generation,
            );
            crate::handler::linux_dmabuf::maybe_reclaim_capability_generation(ctx, generation);
            return Action::Drop;
        }
        let guest_id = ctx.shadow_table.get_guest_id(host_id).unwrap_or(0);

        if guest_id != 0 {
            debug!(
                "wl_callback.done for guest_id {}, sending done and delete_id",
                guest_id
            );

            // 1. Send done event to client
            let mut builder = MessageBuilder::new();
            builder.write_u32(callback_data);

            if let Ok(done_msg) =
                builder.try_build_message(guest_id, protocols::wayland::wl_callback::EVT_DONE)
            {
                ctx.host_to_client_queue.push((done_msg, Vec::new()));
            }

            // 2. Send delete_id to client
            let mut builder2 = MessageBuilder::new();
            builder2.write_u32(guest_id);

            if let Ok(del_msg) =
                builder2.try_build_message(1, protocols::wayland::wl_display::EVT_DELETE_ID)
            {
                ctx.host_to_client_queue.push((del_msg, Vec::new()));
            }

            // The terminal event destroys the callback on both sides, but the
            // host numeric ID cannot be reused until its later delete_id.
            if ctx
                .shadow_table
                .retire_server_destroyed_object(guest_id)
                .is_none()
            {
                log::warn!(
                    "wl_callback {} could not retain host ID {} until delete_id",
                    guest_id,
                    host_id
                );
            }
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::CallbackHandler;
    use crate::handler::display::DisplayHandler;
    use crate::protocols::wayland::wl_callback::WlCallbackHandler;
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::state::Context;
    use crate::wire::Action;

    #[test]
    fn internal_dmabuf_callback_completes_exact_generation() {
        let callback_id = 40;
        let generation = 7;
        let mut ctx = Context::new_for_test(true, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            callback_id,
            "wl_callback".to_string(),
            1,
        );
        ctx.dmabuf_capabilities.entry(generation).or_default();
        ctx.host_dmabuf_generation = Some(generation);
        ctx.dmabuf_capability_callbacks
            .insert(callback_id, generation);
        ctx.last_sender_id = callback_id;

        let mut handler = CallbackHandler;
        assert_eq!(handler.on_done(&mut ctx, 0), Action::Drop);
        assert!(ctx.dmabuf_capabilities[&generation].ready);
        assert!(!ctx.dmabuf_capability_callbacks.contains_key(&callback_id));
        assert!(ctx.shadow_table.is_pending_destroy_host_only(callback_id));
    }

    #[test]
    fn guest_callback_reserves_host_id_until_real_delete_id() {
        let guest_id = 20;
        let host_id = 40;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_id, host_id);
        ctx.shadow_table
            .track_interface_with_version(guest_id, "wl_callback".to_string(), 1);
        ctx.last_sender_id = host_id;

        let mut handler = CallbackHandler;
        assert_eq!(handler.on_done(&mut ctx, 7), Action::Drop);
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(ctx.shadow_table.get_host_id(guest_id), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_id), None);
        assert!(ctx.shadow_table.is_pending_destroy_host_only(host_id));
        assert!(!ctx.shadow_table.is_host_id_available(host_id));

        ctx.last_sender_id = 1;
        assert_eq!(DisplayHandler.on_delete_id(&mut ctx, host_id), Action::Drop);
        assert_eq!(
            ctx.host_to_client_queue.len(),
            2,
            "the guest already received its callback delete_id"
        );
        assert!(!ctx.shadow_table.is_pending_destroy_host_only(host_id));
        assert!(ctx.shadow_table.is_host_id_available(host_id));
    }
}
