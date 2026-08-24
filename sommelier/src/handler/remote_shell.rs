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

//! Host-only zcr_remote_shell_v2 bridge.
//!
//! The remote-shell global is never advertised to guest applications. In the
//! opt-in backend Sommelier creates a remote-surface role on the host
//! wl_surface and translates its host geometry notifications into the guest
//! XDG configure handshake.

use crate::handler::placement::queue_synthetic_xdg_resize;
use crate::protocols::remote_shell_unstable_v2::{
    zcr_remote_shell_v2::{REQ_DESTROY as REQ_SHELL_DESTROY, REQ_GET_REMOTE_SURFACE},
    zcr_remote_surface_v2::{REQ_DESTROY, REQ_SET_APP_ID, REQ_SET_TITLE},
};
use crate::state::Context;
use crate::wire::{Action, MessageBuilder};

pub(crate) fn queue_get_remote_surface(ctx: &mut Context, guest_wl_surface_id: u32) -> Option<u32> {
    let Some(host_wl_surface_id) = ctx.shadow_table.get_host_id(guest_wl_surface_id) else {
        log::warn!(
            "remote-shell: cannot create surface for guest wl_surface {}: \
             no host wl_surface mapping",
            guest_wl_surface_id
        );
        return None;
    };
    let Some(remote_shell_id) = ctx.window_placement.remote_shell_id() else {
        log::warn!(
            "remote-shell: cannot create surface for guest wl_surface {} \
             (host {}): host did not provide a bound zcr_remote_shell_v2",
            guest_wl_surface_id,
            host_wl_surface_id
        );
        return None;
    };
    if ctx
        .window_placement
        .remote_surface_for_wl_surface(host_wl_surface_id)
        .is_some()
    {
        return ctx
            .window_placement
            .remote_surface_for_wl_surface(host_wl_surface_id);
    }

    let remote_surface_id = ctx.shadow_table.allocate_host_id();
    let version = ctx.window_placement.remote_shell_version().min(6);
    ctx.shadow_table.track_host_interface_with_version(
        remote_surface_id,
        "zcr_remote_surface_v2".to_string(),
        version,
    );
    if !ctx.window_placement.remember_remote_surface_for_manager(
        host_wl_surface_id,
        remote_surface_id,
        remote_shell_id,
    ) {
        log::warn!(
            "remote-shell: refusing duplicate role for host wl_surface {}",
            host_wl_surface_id
        );
        ctx.shadow_table.remove_host_interface(remote_surface_id);
        return None;
    }

    let mut builder = MessageBuilder::new();
    builder.write_u32(remote_surface_id);
    builder.write_u32(host_wl_surface_id);
    // zcr_remote_shell_v2.container.default
    builder.write_u32(1);
    let Ok(message) = builder.try_build_message(remote_shell_id, REQ_GET_REMOTE_SURFACE) else {
        log::warn!(
            "remote-shell: cannot encode get_remote_surface manager={} \
             surface={} version={}",
            remote_shell_id,
            host_wl_surface_id,
            version
        );
        ctx.window_placement
            .take_remote_surface_for_wl_surface(host_wl_surface_id);
        ctx.shadow_table.remove_host_interface(remote_surface_id);
        return None;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    Some(remote_surface_id)
}

pub(crate) fn queue_remote_app_id(ctx: &mut Context, remote_surface_id: u32, app_id: &str) -> bool {
    if !ctx
        .shadow_table
        .host_object_matches(remote_surface_id, "zcr_remote_surface_v2")
    {
        log::debug!(
            "remote-shell: skipping app-id for stale remote surface {}",
            remote_surface_id
        );
        return false;
    }
    let mut builder = MessageBuilder::new();
    builder.write_string(app_id);
    let Ok(message) = builder.try_build_message(remote_surface_id, REQ_SET_APP_ID) else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

pub(crate) fn queue_remote_title(ctx: &mut Context, remote_surface_id: u32, title: &str) -> bool {
    if !ctx
        .shadow_table
        .host_object_matches(remote_surface_id, "zcr_remote_surface_v2")
    {
        log::debug!(
            "remote-shell: skipping title for stale remote surface {}",
            remote_surface_id
        );
        return false;
    }
    let mut builder = MessageBuilder::new();
    builder.write_string(title);
    let Ok(message) = builder.try_build_message(remote_surface_id, REQ_SET_TITLE) else {
        return false;
    };
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

pub(crate) fn queue_remote_destroy(ctx: &mut Context, remote_surface_id: u32) -> bool {
    if !ctx
        .shadow_table
        .host_object_matches(remote_surface_id, "zcr_remote_surface_v2")
    {
        return false;
    }
    // Retire the object before queueing its destructor.  Cleanup can be
    // reached through both the wl_surface and xdg_surface paths; only the
    // first path that successfully claims the object may emit a destroy
    // request.  Otherwise a second path can enqueue a duplicate destructor
    // while the object is already pending host deletion.
    if !ctx
        .shadow_table
        .mark_pending_destroy_host(remote_surface_id)
    {
        return false;
    }
    let message = MessageBuilder::new().build_message(remote_surface_id, REQ_DESTROY);
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

/// Queue destruction of one host-only remote-shell manager generation.
///
/// The manager may have outlived its registry global after `global_remove`.
/// Its host shadow metadata is intentionally retained until this final
/// destructor is queued, so the request remains type-checked and the ID stays
/// reserved until the host acknowledges `wl_display.delete_id`.
pub(crate) fn queue_remote_shell_destroy(ctx: &mut Context, manager_id: u32) -> bool {
    if !ctx
        .shadow_table
        .host_object_matches(manager_id, "zcr_remote_shell_v2")
    {
        return false;
    }
    if !ctx.shadow_table.mark_pending_destroy_host(manager_id) {
        return false;
    }
    let message = MessageBuilder::new().build_message(manager_id, REQ_SHELL_DESTROY);
    ctx.client_to_host_queue.push((message, Vec::new()));
    true
}

pub struct RemoteShellHandler;

impl crate::protocols::remote_shell_unstable_v2::ProtocolHandler for RemoteShellHandler {}

impl crate::protocols::remote_shell_unstable_v2::zcr_remote_shell_v2::ZcrRemoteShellV2Handler
    for RemoteShellHandler
{
}

impl crate::protocols::remote_shell_unstable_v2::zcr_remote_surface_v2::ZcrRemoteSurfaceV2Handler
    for RemoteShellHandler
{
    fn on_close(&mut self, ctx: &mut Context) -> Action {
        let remote_surface_id = ctx.last_sender_id;
        let Some(xdg_toplevel_id) = ctx
            .window_placement
            .xdg_toplevel_for_remote_surface(remote_surface_id)
        else {
            return Action::Drop;
        };
        let builder = MessageBuilder::new();
        if let Ok(message) = builder.try_build_message(
            xdg_toplevel_id,
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CLOSE,
        ) {
            ctx.host_to_client_queue.push((message, Vec::new()));
        }
        Action::Drop
    }

    fn on_window_geometry_changed(
        &mut self,
        ctx: &mut Context,
        _x: i32,
        _y: i32,
        width: i32,
        height: i32,
    ) -> Action {
        let remote_surface_id = ctx.last_sender_id;
        let Some(xdg_toplevel_id) = ctx
            .window_placement
            .xdg_toplevel_for_remote_surface(remote_surface_id)
        else {
            return Action::Drop;
        };
        let Some(wl_surface_id) = ctx
            .window_placement
            .wl_surface_for_xdg_toplevel(xdg_toplevel_id)
        else {
            return Action::Drop;
        };
        let _ = queue_synthetic_xdg_resize(ctx, xdg_toplevel_id, wl_surface_id, width, height, 0);
        Action::Drop
    }

    fn on_bounds_changed(
        &mut self,
        ctx: &mut Context,
        _display_id_hi: u32,
        _display_id_lo: u32,
        _x: i32,
        _y: i32,
        width: i32,
        height: i32,
        _reason: u32,
    ) -> Action {
        self.on_window_geometry_changed(ctx, 0, 0, width, height)
    }

    fn on_bounds_changed_in_output(
        &mut self,
        ctx: &mut Context,
        _output: u32,
        _x: i32,
        _y: i32,
        width: i32,
        height: i32,
        _reason: u32,
    ) -> Action {
        self.on_window_geometry_changed(ctx, 0, 0, width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::remote_shell_unstable_v2::{zcr_remote_shell_v2, zcr_remote_surface_v2};
    use crate::state::{Context, WindowGeometryMethod, WindowHostPolicy, WindowPlacementMode};

    fn opcode(message: &(Vec<u8>, Vec<std::os::unix::io::RawFd>)) -> u16 {
        (u32::from_ne_bytes(message.0[4..8].try_into().unwrap()) & 0xffff) as u16
    }

    #[test]
    fn official_remote_shell_wire_opcodes_are_preserved() {
        assert_eq!(zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE, 1);
        assert_eq!(zcr_remote_surface_v2::REQ_DESTROY, 0);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_APP_ID, 1);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_TITLE, 2);
        assert_eq!(zcr_remote_surface_v2::REQ_SET_BOUNDS_IN_OUTPUT, 37);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED, 3);
        assert_eq!(zcr_remote_surface_v2::EVT_BOUNDS_CHANGED_IN_OUTPUT, 7);
    }

    #[test]
    fn get_remote_surface_uses_the_host_role_and_tracks_its_lifetime() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(WindowPlacementMode::new(
                WindowHostPolicy::Guest,
                WindowGeometryMethod::RemoteShell,
            ));
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(20, "wl_surface".to_string(), 6);
        assert!(ctx.window_placement.set_remote_shell_binding(30, 7, 6));

        let remote_surface_id =
            queue_get_remote_surface(&mut ctx, 10).expect("remote surface role");
        assert_eq!(
            ctx.window_placement.remote_surface_for_wl_surface(20),
            Some(remote_surface_id)
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let request = &ctx.client_to_host_queue[0];
        assert_eq!(request.0[0..4], 30u32.to_ne_bytes());
        assert_eq!(opcode(request), zcr_remote_shell_v2::REQ_GET_REMOTE_SURFACE);
        assert_eq!(
            u32::from_ne_bytes(request.0[8..12].try_into().unwrap()),
            remote_surface_id
        );
        assert_eq!(
            u32::from_ne_bytes(request.0[12..16].try_into().unwrap()),
            20
        );
        assert_eq!(u32::from_ne_bytes(request.0[16..20].try_into().unwrap()), 1);
    }

    #[test]
    fn remote_surface_metadata_is_encoded_with_official_request_indices() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            40,
            "zcr_remote_surface_v2".to_string(),
            6,
        );

        assert!(queue_remote_app_id(
            &mut ctx,
            40,
            "org.chromium.guest_os.test"
        ));
        assert!(queue_remote_title(&mut ctx, 40, "Terminal"));
        assert_eq!(opcode(&ctx.client_to_host_queue[0]), 1);
        assert_eq!(opcode(&ctx.client_to_host_queue[1]), 2);
    }

    #[test]
    fn remote_destroy_is_claimed_before_queueing_and_is_idempotent() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            40,
            "zcr_remote_surface_v2".to_string(),
            6,
        );
        ctx.shadow_table.track_host_interface_with_version(
            41,
            "zcr_remote_shell_v2".to_string(),
            6,
        );

        assert!(queue_remote_destroy(&mut ctx, 40));
        assert!(!queue_remote_destroy(&mut ctx, 40));
        assert!(queue_remote_shell_destroy(&mut ctx, 41));
        assert!(!queue_remote_shell_destroy(&mut ctx, 41));

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        assert_eq!(ctx.client_to_host_queue[0].0[0..4], 40u32.to_ne_bytes());
        assert_eq!(opcode(&ctx.client_to_host_queue[0]), REQ_DESTROY);
        assert_eq!(ctx.client_to_host_queue[1].0[0..4], 41u32.to_ne_bytes());
        assert_eq!(opcode(&ctx.client_to_host_queue[1]), REQ_SHELL_DESTROY);
        assert!(ctx.shadow_table.is_pending_destroy_host_only(40));
        assert!(ctx.shadow_table.is_pending_destroy_host_only(41));
    }

    #[test]
    fn remote_metadata_is_not_sent_after_destroy_is_claimed() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.track_host_interface_with_version(
            40,
            "zcr_remote_surface_v2".to_string(),
            6,
        );

        assert!(queue_remote_destroy(&mut ctx, 40));
        assert!(!queue_remote_app_id(&mut ctx, 40, "org.example.Stale"));
        assert!(!queue_remote_title(&mut ctx, 40, "stale"));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            1,
            "metadata must not be queued after the destructor claims the role"
        );
    }

    #[test]
    fn geometry_event_synthesizes_the_guest_xdg_resize() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.window_placement
            .set_mode_for_test(WindowPlacementMode::remote_shell());
        ctx.shadow_table.map_id(10, 20);
        ctx.shadow_table
            .track_interface_with_version(10, "wl_surface".to_string(), 6);
        ctx.shadow_table
            .track_host_interface_with_version(20, "wl_surface".to_string(), 6);
        assert!(ctx.window_placement.remember_xdg_surface(11, 10));
        assert!(ctx.window_placement.remember_xdg_toplevel(12, 10));
        assert!(ctx.window_placement.remember_remote_surface(20, 40));
        assert!(ctx.window_placement.remember_remote_toplevel(12, 40));

        ctx.last_sender_id = 40;
        let mut handler = RemoteShellHandler;
        assert_eq!(
            crate::protocols::remote_shell_unstable_v2::zcr_remote_surface_v2::
                ZcrRemoteSurfaceV2Handler::on_bounds_changed(
                    &mut handler,
                    &mut ctx,
                    0,
                    0,
                    100,
                    200,
                    800,
                    600,
                    2,
                ),
            Action::Drop
        );
        assert_eq!(ctx.host_to_client_queue.len(), 2);
        assert_eq!(
            opcode(&ctx.host_to_client_queue[0]),
            crate::protocols::xdg_shell::xdg_toplevel::EVT_CONFIGURE
        );
        assert_eq!(
            opcode(&ctx.host_to_client_queue[1]),
            crate::protocols::xdg_shell::xdg_surface::EVT_CONFIGURE
        );
    }
}
