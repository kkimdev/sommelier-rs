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
use crate::state::Context;
use crate::wire::{Action, MessageBuilder};
use log::debug;

pub struct CallbackHandler;

impl WlCallbackHandler for CallbackHandler {
    fn on_done(&mut self, ctx: &mut Context, callback_data: u32) -> Action {
        let host_id = ctx.last_sender_id;
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

            // 3. Remove from shadow table
            ctx.shadow_table.remove_id(guest_id);
        }
        Action::Drop
    }
}
