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

use crate::handler::aura_objects::ensure_host_zaura_surface;
use crate::handler::compositor::native_wayland_app_id;
use crate::handler::window_placement::set_aura_application_id;
use crate::protocols::aura_shell::zaura_surface::REQ_SET_STARTUP_ID;
use crate::protocols::gtk::gtk_shell1::GtkShell1Handler;
use crate::protocols::gtk::gtk_surface1::GtkSurface1Handler;
use crate::state::{Context, GtkSurfaceState};
use crate::wire::{Action, MessageBuilder};

pub struct GtkShellHandler;

/// Queue the GTK activation token to an Aura surface. Every Aura surface
/// supports `set_startup_id`: the registry binds no older `zaura_shell`.
fn queue_startup_id(ctx: &mut Context, zaura_surface_id: u32, startup_id: Option<&str>) {
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

/// The `gtk_shell1.capabilities` bitfield this proxy announces: it
/// implements none of the optional capabilities (global app menu, global
/// menu bar, desktop icons).
const GTK_SHELL_CAPABILITIES: u32 = 0;

/// Announce a synthetic `gtk_shell1`'s capabilities once the internal
/// `zaura_shell` bind it relies on has reached the compositor (see
/// [`crate::state::InternalCallback::GtkShellCapabilities`]). A shell the
/// guest has destroyed meanwhile gets nothing.
pub(crate) fn announce_capabilities(ctx: &mut Context, gtk_shell_id: u32) {
    if !ctx.gtk_shells.contains_key(&gtk_shell_id)
        || !ctx.shadow_table.is_local_only_guest_object(gtk_shell_id)
    {
        return;
    }
    let mut builder = MessageBuilder::new();
    builder.write_u32(GTK_SHELL_CAPABILITIES);
    let message = builder.build_message(
        gtk_shell_id,
        crate::protocols::gtk::gtk_shell1::EVT_CAPABILITIES,
    );
    ctx.host_to_client_queue.push((message, Vec::new()));
}

impl GtkShell1Handler for GtkShellHandler {
    fn on_get_gtk_surface(
        &mut self,
        ctx: &mut Context,
        gtk_surface_id: u32,
        wl_surface_id: u32,
    ) -> Action {
        let shell_id = ctx.last_sender_id;
        let Some(startup_id) = ctx
            .gtk_shells
            .get(&shell_id)
            .map(|shell| shell.startup_id.clone())
        else {
            log::warn!(
                "Ignoring get_gtk_surface from unknown gtk_shell1 {}",
                shell_id
            );
            return Action::Drop;
        };

        ctx.shadow_table.track_interface_with_version(
            gtk_surface_id,
            "gtk_surface1".to_string(),
            1,
        );
        ctx.gtk_surfaces.insert(
            gtk_surface_id,
            GtkSurfaceState {
                shell_id,
                wl_surface_id,
            },
        );
        if let Some(shell) = ctx.gtk_shells.get_mut(&shell_id) {
            shell.surfaces.insert(gtk_surface_id);
        }
        if let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_id) {
            queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
        }
        Action::Drop
    }

    fn on_set_startup_id(&mut self, ctx: &mut Context, startup_id: &Option<String>) -> Action {
        let shell_id = ctx.last_sender_id;
        let Some(shell) = ctx.gtk_shells.get_mut(&shell_id) else {
            log::warn!(
                "Ignoring set_startup_id from unknown gtk_shell1 {}",
                shell_id
            );
            return Action::Drop;
        };
        shell.startup_id.clone_from(startup_id);
        // Two GTK surfaces may wrap one wl_surface; it has one Aura surface.
        let wl_surface_ids = shell
            .surfaces
            .iter()
            .filter_map(|surface_id| ctx.gtk_surfaces.get(surface_id))
            .map(|surface| surface.wl_surface_id)
            .collect::<std::collections::BTreeSet<_>>();
        for wl_surface_id in wl_surface_ids {
            if let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_id) {
                queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
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
        let Some(wl_surface_id) = ctx
            .gtk_surfaces
            .get(&ctx.last_sender_id)
            .map(|surface| surface.wl_surface_id)
        else {
            return Action::Drop;
        };
        let application_id = native_wayland_app_id(&ctx.vm_identifier, application_id);
        set_aura_application_id(ctx, wl_surface_id, &application_id);
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
    use crate::state::GtkShellState;
    use crate::wire::WireMessage;

    const GTK_SHELL: u32 = 10;
    const GTK_SURFACE: u32 = 11;
    const WL_SURFACE_GUEST: u32 = 20;
    const WL_SURFACE_HOST: u32 = 120;
    const ZAURA_SHELL_HOST: u32 = 200;

    fn setup_ctx() -> Context {
        setup_ctx_with(crate::placement_config::PlacementConfig::for_test(
            Vec::new(),
        ))
    }

    /// [`setup_ctx`] with an explicit placement configuration.
    fn setup_ctx_with(placement: crate::placement_config::PlacementConfig) -> Context {
        let mut ctx = Context::with_placement_for_test(placement);
        ctx.shadow_table
            .track_interface_with_version(GTK_SHELL, "gtk_shell1".to_string(), 1);
        ctx.gtk_shells.insert(GTK_SHELL, GtkShellState::default());
        ctx.shadow_table.map_id(WL_SURFACE_GUEST, WL_SURFACE_HOST);
        ctx.shadow_table.track_interface_with_version(
            WL_SURFACE_GUEST,
            "wl_surface".to_string(),
            4,
        );
        ctx.host_zaura_shell = Some(crate::state::HostAuraShell {
            id: ZAURA_SHELL_HOST,
            version: 38,
        });
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
            ctx.gtk_surfaces[&GTK_SURFACE].wl_surface_id,
            WL_SURFACE_GUEST
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
                format!(
                    "org.chromium.guest_os.{}.wayland.com.example.Terminal",
                    ctx.vm_identifier
                )
                .as_str()
            )
        );
    }

    /// A GTK surface created while no `zaura_shell` is bound (for example
    /// between a shell's `global_remove` and its re-advertisement) gets its
    /// Aura surface once a shell is bound again, instead of losing its
    /// metadata for good.
    #[test]
    fn gtk_metadata_reaches_a_shell_bound_after_the_surface() {
        let mut ctx = setup_ctx();
        ctx.host_zaura_shell = None;
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        assert!(ctx.client_to_host_queue.is_empty());

        ctx.host_zaura_shell = Some(crate::state::HostAuraShell {
            id: ZAURA_SHELL_HOST,
            version: 38,
        });
        ctx.last_sender_id = GTK_SURFACE;
        handler.on_set_dbus_properties(
            &mut ctx,
            &Some("com.example.Terminal".to_string()),
            &None,
            &None,
            &None,
            &None,
            &None,
        );
        ctx.last_sender_id = GTK_SHELL;
        handler.on_set_startup_id(&mut ctx, &Some("launch-token".to_string()));

        let zaura_surface = ctx
            .aura_surfaces
            .get(WL_SURFACE_HOST)
            .expect("the late shell creates the Aura surface");
        let queued = ctx
            .client_to_host_queue
            .iter()
            .map(|(message, _)| (sender(message), opcode(message)))
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            vec![
                (ZAURA_SHELL_HOST, REQ_GET_AURA_SURFACE),
                (zaura_surface, REQ_SET_APPLICATION_ID),
                (zaura_surface, REQ_SET_STARTUP_ID),
            ]
        );
    }

    #[test]
    fn gtk_surface_creation_does_not_allocate_an_arc_identity() {
        let mut ctx =
            setup_ctx_with(crate::placement_config::PlacementConfig::arc_bounds_for_test(""));
        ctx.last_sender_id = GTK_SHELL;
        GtkShellHandler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        assert!(
            !ctx.window_placement.has_arc_application_ids(),
            "identities are allocated lazily by the paths that send them"
        );
    }

    #[test]
    fn dbus_application_id_preserves_arc_policy_for_window_bounds() {
        let mut ctx =
            setup_ctx_with(crate::placement_config::PlacementConfig::arc_bounds_for_test(""));
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
        let application_id = nullable_string(message).expect("ARC application identity");
        assert!(application_id.starts_with(crate::state::ARC_APPLICATION_ID_PREFIX));
        assert!(!application_id.starts_with("org.chromium.arc.session."));
        assert!(application_id
            .strip_prefix(crate::state::ARC_APPLICATION_ID_PREFIX)
            .unwrap()
            .parse::<u32>()
            .is_ok());
        assert_eq!(
            Some(application_id.as_str()),
            ctx.window_placement
                .arc_application_id(WL_SURFACE_GUEST)
                .as_deref()
        );
    }

    #[test]
    fn wl_surface_destroy_removes_gtk_and_aura_associations() {
        use crate::protocols::wayland::wl_surface::WlSurfaceHandler;

        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, WL_SURFACE_GUEST);
        assert!(ctx.gtk_shells[&GTK_SHELL].surfaces.contains(&GTK_SURFACE));

        ctx.last_sender_id = WL_SURFACE_GUEST;
        let mut compositor = crate::handler::compositor::CompositorHandler;
        assert_eq!(compositor.on_destroy(&mut ctx), Action::Drop);
        assert!(!ctx.gtk_surfaces.contains_key(&GTK_SURFACE));
        assert!(!ctx.gtk_shells[&GTK_SHELL].surfaces.contains(&GTK_SURFACE));
        assert!(
            ctx.shadow_table.is_local_only_guest_object(GTK_SURFACE),
            "gtk_surface1 has no destroy request, so its client ID stays reserved"
        );
        assert!(!ctx.shadow_table.is_guest_id_available(GTK_SURFACE));
        assert_eq!(ctx.aura_surfaces.get(WL_SURFACE_HOST), None);
    }
}
