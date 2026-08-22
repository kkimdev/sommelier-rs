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

use crate::handler::placement::{
    ensure_host_zaura_surface, queue_policy_application_id, wayland_string_fits_message,
};
use crate::protocols::aura_shell::zaura_surface::REQ_SET_STARTUP_ID;
use crate::protocols::gtk::gtk_shell1::GtkShell1Handler;
use crate::protocols::gtk::gtk_surface1::GtkSurface1Handler;
use crate::state::Context;
use crate::wire::{Action, MessageBuilder};

pub struct GtkShellHandler;

fn queue_startup_id(ctx: &mut Context, zaura_surface_id: u32, startup_id: Option<&str>) {
    let version = ctx
        .shadow_table
        .host_object_version(zaura_surface_id)
        .unwrap_or(ctx.window_placement.aura_shell_version());
    if version < 4 {
        return;
    }

    let mut builder = MessageBuilder::new();
    builder.write_nullable_string(startup_id);
    match builder.try_build_message(zaura_surface_id, REQ_SET_STARTUP_ID) {
        Ok(message) => ctx.client_to_host_queue.push((message, Vec::new())),
        Err(error) => log::warn!(
            "Dropping oversized GTK startup ID for zaura_surface {}: {}",
            zaura_surface_id,
            error
        ),
    }
}

impl GtkShell1Handler for GtkShellHandler {
    fn on_get_gtk_surface(
        &mut self,
        ctx: &mut Context,
        gtk_surface_id: u32,
        wl_surface_id: u32,
    ) -> Action {
        let shell_id = ctx.last_sender_id;
        let Some((startup_id, _)) = ctx
            .window_placement
            .gtk_shell_startup_and_surfaces(shell_id)
        else {
            log::warn!(
                "Ignoring get_gtk_surface from unknown gtk_shell1 {}",
                shell_id
            );
            return Action::Drop;
        };

        if !ctx
            .window_placement
            .remember_gtk_surface(gtk_surface_id, shell_id, wl_surface_id)
        {
            log::warn!(
                "Refusing duplicate or orphaned gtk_surface1 {}",
                gtk_surface_id
            );
            return Action::Drop;
        }
        let host_zaura_surface_id = ensure_host_zaura_surface(ctx, wl_surface_id);
        ctx.shadow_table.track_interface_with_version(
            gtk_surface_id,
            "gtk_surface1".to_string(),
            1,
        );
        if let Some(zaura_surface_id) = host_zaura_surface_id {
            queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
        }
        Action::Drop
    }

    fn on_set_startup_id(&mut self, ctx: &mut Context, startup_id: &Option<String>) -> Action {
        let shell_id = ctx.last_sender_id;
        let Some(gtk_surface_ids) = ctx
            .window_placement
            .update_gtk_shell_startup_id(shell_id, startup_id.clone())
        else {
            log::warn!(
                "Ignoring set_startup_id from unknown gtk_shell1 {}",
                shell_id
            );
            return Action::Drop;
        };
        let mut aura_surface_ids = std::collections::HashSet::new();
        for gtk_surface_id in gtk_surface_ids {
            let Some(wl_surface_id) = ctx
                .window_placement
                .wl_surface_for_gtk_surface(gtk_surface_id)
            else {
                continue;
            };
            if let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_id) {
                if aura_surface_ids.insert(zaura_surface_id) {
                    queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
                }
            }
        }
        Action::Drop
    }

    fn on_system_bell(&mut self, _ctx: &mut Context, _surface: u32) -> Action {
        Action::Drop
    }
}

impl GtkSurface1Handler for GtkShellHandler {
    fn on_set_dbus_properties(
        &mut self,
        ctx: &mut Context,
        application_id: &Option<String>,
        _app_menu_path: &Option<String>,
        _menubar_path: &Option<String>,
        _window_object_path: &Option<String>,
        _application_object_path: &Option<String>,
        _unique_bus_name: &Option<String>,
    ) -> Action {
        let Some(application_id) = application_id.as_deref() else {
            return Action::Drop;
        };
        let Some(wl_surface_guest_id) = ctx
            .window_placement
            .wl_surface_for_gtk_surface(ctx.last_sender_id)
        else {
            return Action::Drop;
        };
        let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_guest_id) else {
            return Action::Drop;
        };

        let native_application_id = ctx.window_placement.native_wayland_app_id(application_id);
        if !wayland_string_fits_message(&native_application_id) {
            log::warn!(
                "Dropping oversized GTK application ID for gtk_surface1 {}",
                ctx.last_sender_id
            );
            return Action::Drop;
        }
        if !queue_policy_application_id(
            ctx,
            zaura_surface_id,
            wl_surface_guest_id,
            &native_application_id,
        ) {
            log::warn!(
                "Unable to queue GTK application ID for gtk_surface1 {}",
                ctx.last_sender_id
            );
            return Action::Drop;
        }
        // Publish the native identity only after its host Aura request was
        // accepted. A failed request must not make a later transient cleanup
        // restore an ID the host never received.
        ctx.window_placement
            .remember_native_application_id(wl_surface_guest_id, native_application_id);
        Action::Drop
    }

    fn on_set_modal(&mut self, _ctx: &mut Context) -> Action {
        Action::Drop
    }

    fn on_unset_modal(&mut self, _ctx: &mut Context) -> Action {
        Action::Drop
    }

    fn on_present(&mut self, _ctx: &mut Context, _time: u32) -> Action {
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE;
    use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
    use crate::wire::WireMessage;

    const GTK_SHELL: u32 = 10;
    const GTK_SURFACE: u32 = 11;
    const WL_SURFACE_GUEST: u32 = 20;
    const WL_SURFACE_HOST: u32 = 120;
    const ZAURA_SHELL_HOST: u32 = 200;

    fn setup_ctx() -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table
            .track_interface_with_version(GTK_SHELL, "gtk_shell1".to_string(), 1);
        assert!(ctx.window_placement.remember_gtk_shell(GTK_SHELL));
        ctx.shadow_table.map_id(WL_SURFACE_GUEST, WL_SURFACE_HOST);
        ctx.shadow_table.track_interface_with_version(
            WL_SURFACE_GUEST,
            "wl_surface".to_string(),
            4,
        );
        ctx.window_placement
            .set_aura_shell_binding_for_test(ZAURA_SHELL_HOST, 38);
        ctx.shadow_table.track_host_interface_with_version(
            ZAURA_SHELL_HOST,
            "zaura_shell".to_string(),
            38,
        );
        ctx
    }

    fn sender(message: &[u8]) -> u32 {
        u32::from_ne_bytes(message[0..4].try_into().unwrap())
    }

    fn opcode(message: &[u8]) -> u16 {
        u32::from_ne_bytes(message[4..8].try_into().unwrap()) as u16
    }

    fn nullable_string(message: &[u8]) -> Option<String> {
        let mut wire = WireMessage::new(sender(message), opcode(message), &message[8..], &[]);
        wire.read_nullable_string().unwrap()
    }

    #[test]
    fn new_gtk_surface_inherits_startup_id_on_shared_aura_surface() {
        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        assert_eq!(
            handler.on_set_startup_id(&mut ctx, &Some("launch-token".to_string())),
            Action::Drop
        );
        assert_eq!(
            handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST),
            Action::Drop
        );

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        let get_aura_surface = &ctx.client_to_host_queue[0].0;
        assert_eq!(sender(get_aura_surface), ZAURA_SHELL_HOST);
        assert_eq!(opcode(get_aura_surface), REQ_GET_AURA_SURFACE);
        let startup_id = &ctx.client_to_host_queue[1].0;
        assert_eq!(opcode(startup_id), REQ_SET_STARTUP_ID);
        assert_eq!(nullable_string(startup_id).as_deref(), Some("launch-token"));
        assert!(ctx.shadow_table.is_local_only_guest_object(GTK_SURFACE));
        assert_eq!(
            ctx.window_placement.wl_surface_for_gtk_surface(GTK_SURFACE),
            Some(WL_SURFACE_GUEST)
        );
    }

    #[test]
    fn startup_id_updates_and_clears_all_unique_aura_surfaces() {
        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE + 1, WL_SURFACE_GUEST);
        ctx.client_to_host_queue.clear();

        handler.on_set_startup_id(&mut ctx, &Some("next-token".to_string()));
        assert_eq!(
            ctx.client_to_host_queue.len(),
            1,
            "two GTK wrappers for one wl_surface must share one Aura update"
        );
        assert_eq!(
            nullable_string(&ctx.client_to_host_queue[0].0).as_deref(),
            Some("next-token")
        );

        ctx.client_to_host_queue.clear();
        handler.on_set_startup_id(&mut ctx, &None);
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        assert_eq!(nullable_string(&ctx.client_to_host_queue[0].0), None);
    }

    #[test]
    fn dbus_application_id_is_namespaced_on_aura_surface() {
        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = GTK_SURFACE;

        assert_eq!(
            handler.on_set_dbus_properties(
                &mut ctx,
                &Some("com.example.Terminal".to_string()),
                &None,
                &None,
                &None,
                &None,
                &None,
            ),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(opcode(message), REQ_SET_APPLICATION_ID);
        assert_eq!(
            nullable_string(message).as_deref(),
            Some(
                ctx.window_placement
                    .native_wayland_app_id("com.example.Terminal")
                    .as_str()
            )
        );
    }

    #[test]
    fn dbus_application_id_keeps_arc_task_identity_on_aura_surface() {
        let mut ctx = setup_ctx();
        ctx.window_placement
            .set_mode_for_test(crate::state::WindowPlacementMode::new(
                crate::state::WindowHostPolicy::Arc,
                crate::state::WindowGeometryMethod::Bounds,
            ));
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = GTK_SURFACE;

        assert_eq!(
            handler.on_set_dbus_properties(
                &mut ctx,
                &Some("com.example.Terminal".to_string()),
                &None,
                &None,
                &None,
                &None,
                &None,
            ),
            Action::Drop
        );
        assert_eq!(ctx.client_to_host_queue.len(), 1);
        let message = &ctx.client_to_host_queue[0].0;
        assert_eq!(opcode(message), REQ_SET_APPLICATION_ID);
        let expected_application_id = ctx
            .window_placement
            .arc_policy_application_id(WL_SURFACE_GUEST)
            .expect("ARC task ID allocated for placement");
        assert_eq!(
            nullable_string(message).as_deref(),
            Some(expected_application_id.as_str())
        );
        let task_id = expected_application_id
            .strip_prefix("org.chromium.arc.")
            .expect("ARC task-form application ID")
            .parse::<u32>()
            .expect("numeric ARC task ID");
        assert!(
            (crate::state::ARC_TASK_ID_POOL_START..=crate::state::ARC_TASK_ID_POOL_END)
                .contains(&task_id)
        );
    }

    #[test]
    fn wl_surface_destroy_removes_gtk_and_aura_associations() {
        use crate::protocols::wayland::wl_surface::WlSurfaceHandler;

        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        assert_eq!(
            ctx.window_placement
                .gtk_shell_startup_and_surfaces(GTK_SHELL)
                .expect("GTK shell")
                .1,
            vec![GTK_SURFACE]
        );

        ctx.last_sender_id = WL_SURFACE_GUEST;
        let mut compositor = crate::handler::compositor::CompositorHandler;
        assert_eq!(compositor.on_destroy(&mut ctx), Action::Drop);
        assert_eq!(
            ctx.window_placement.wl_surface_for_gtk_surface(GTK_SURFACE),
            None
        );
        assert_eq!(
            ctx.window_placement
                .gtk_shell_startup_and_surfaces(GTK_SHELL)
                .expect("GTK shell")
                .1,
            Vec::<u32>::new()
        );
        assert!(
            ctx.shadow_table.is_local_only_guest_object(GTK_SURFACE),
            "gtk_surface1 has no destroy request, so its client ID stays reserved"
        );
        assert!(!ctx.shadow_table.is_guest_id_available(GTK_SURFACE));
        assert_eq!(
            ctx.window_placement
                .aura_surface_for_wl_surface(WL_SURFACE_HOST),
            None
        );
    }
}
