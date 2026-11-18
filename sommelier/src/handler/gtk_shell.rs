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
use crate::protocols::aura_shell::zaura_surface;
use crate::protocols::gtk::gtk_shell1::{self, GtkShell1Handler};
use crate::protocols::gtk::gtk_surface1::{self, GtkSurface1Handler};
use crate::state::{Context, GtkSurfaceState};
use crate::wire::{queue_or_drop, Action};

pub struct GtkShellHandler;

/// Queue the GTK activation token to an Aura surface. Every Aura surface
/// supports `set_startup_id`: the registry binds no older `zaura_shell`.
fn queue_startup_id(ctx: &mut Context, zaura_surface_id: u32, startup_id: Option<&str>) {
    queue_or_drop(
        &mut ctx.client_to_host_queue,
        zaura_surface::req::set_startup_id(zaura_surface_id, startup_id),
        format_args!("GTK startup ID for zaura_surface {zaura_surface_id}"),
    );
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
    if !ctx.gtk_shells.contains_key(&gtk_shell_id) || !ctx.shadow_table.is_local(gtk_shell_id) {
        return;
    }
    ctx.host_to_client_queue.push(gtk_shell1::evt::capabilities(
        gtk_shell_id,
        GTK_SHELL_CAPABILITIES,
    ));
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

        // A new object takes its parent's version.
        let version = ctx.shadow_table.guest_object_version_or_unknown(shell_id);
        ctx.shadow_table
            .create_local(gtk_surface_id, gtk_surface1::NAME, version);
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
    use crate::handler::test_support::{
        add_surface, bind_shell, toplevel_ctx_with, SHELL, SURFACE,
    };
    use crate::protocols::aura_shell::zaura_shell::REQ_GET_AURA_SURFACE;
    use crate::protocols::aura_shell::zaura_surface::REQ_SET_APPLICATION_ID;
    use crate::state::GtkShellState;
    use crate::test_support::{msg_opcode, msg_sender, queued_headers};
    use crate::wire::WireMessage;

    const GTK_SHELL: u32 = 10;
    const GTK_SURFACE: u32 = 11;

    /// Give `ctx` the gtk_shell1 [`GTK_SHELL`].
    fn add_gtk_shell(ctx: &mut Context) {
        ctx.shadow_table.create_local(GTK_SHELL, "gtk_shell1", 1);
        ctx.gtk_shells.insert(GTK_SHELL, GtkShellState::default());
    }

    /// [`toplevel_ctx_with`] `placement` and [`add_gtk_shell`].
    fn setup_ctx_with(placement: crate::placement_config::PlacementConfig) -> Context {
        let mut ctx = toplevel_ctx_with(placement);
        add_gtk_shell(&mut ctx);
        ctx
    }

    /// [`setup_ctx`], but [`SURFACE`] has no role.
    fn roleless_ctx() -> Context {
        let mut ctx = Context::with_placement_for_test(
            crate::placement_config::PlacementConfig::for_test(Vec::new()),
        );
        add_surface(&mut ctx);
        bind_shell(&mut ctx);
        add_gtk_shell(&mut ctx);
        ctx
    }

    fn setup_ctx() -> Context {
        setup_ctx_with(crate::placement_config::PlacementConfig::for_test(
            Vec::new(),
        ))
    }

    fn nullable_string(message: &[u8]) -> Option<String> {
        let mut wire = WireMessage::new(msg_sender(message), msg_opcode(message), &message[8..]);
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
            handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest),
            Action::Drop
        );

        assert_eq!(ctx.client_to_host_queue.len(), 2);
        let get_aura_surface = &ctx.client_to_host_queue[0].0;
        assert_eq!(msg_sender(get_aura_surface), SHELL);
        assert_eq!(msg_opcode(get_aura_surface), REQ_GET_AURA_SURFACE);
        let startup_id = &ctx.client_to_host_queue[1].0;
        assert_eq!(msg_opcode(startup_id), zaura_surface::REQ_SET_STARTUP_ID);
        assert_eq!(nullable_string(startup_id).as_deref(), Some("launch-token"));
        assert!(ctx.shadow_table.is_local(GTK_SURFACE));
        assert_eq!(ctx.gtk_surfaces[&GTK_SURFACE].wl_surface_id, SURFACE.guest);
    }

    #[test]
    fn gtk_surface_takes_its_shell_version() {
        let mut ctx = setup_ctx();
        // Every vendored gtk-shell interface is v1, so an artificial shell
        // version shows where the surface's version comes from.
        let shell_version = 7;
        ctx.shadow_table
            .set_guest_version_for_test(GTK_SHELL, shell_version);
        ctx.last_sender_id = GTK_SHELL;
        GtkShellHandler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
        assert_eq!(
            ctx.shadow_table.guest_object_version(GTK_SURFACE),
            Some(shell_version)
        );
    }

    #[test]
    fn startup_id_updates_and_clears_all_unique_aura_surfaces() {
        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE + 1, SURFACE.guest);
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

    /// GTK names the application of a surface with or without a role.
    #[test]
    fn dbus_application_id_is_namespaced_on_aura_surface() {
        for (case, mut ctx) in [("toplevel", setup_ctx()), ("no role", roleless_ctx())] {
            ctx.last_sender_id = GTK_SHELL;
            let mut handler = GtkShellHandler;
            handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
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
                Action::Drop,
                "{case}"
            );
            assert_eq!(ctx.client_to_host_queue.len(), 1, "{case}");
            let message = &ctx.client_to_host_queue[0].0;
            assert_eq!(msg_opcode(message), REQ_SET_APPLICATION_ID, "{case}");
            assert_eq!(
                nullable_string(message).as_deref(),
                Some(
                    format!(
                        "org.chromium.guest_os.{}.wayland.com.example.Terminal",
                        ctx.vm_identifier
                    )
                    .as_str()
                ),
                "{case}"
            );
        }
    }

    /// A GTK surface created between a `zaura_shell`'s `global_remove` and
    /// its re-advertisement has no Aura surface. GTK requests issued after
    /// a new shell is bound create one; nothing replays the requests issued
    /// while no shell was bound.
    #[test]
    fn gtk_requests_after_a_shell_rebind_reach_the_new_shell() {
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        const REGISTRY: u32 = 1;
        const SHELL_NAME: u32 = 7;
        let mut ctx = setup_ctx();
        ctx.host_zaura_shell = None;
        ctx.shadow_table
            .insert_paired_for_test(REGISTRY, REGISTRY, "wl_registry");
        let shell = "zaura_shell".to_string();
        let mut handler = GtkShellHandler;
        let set_application_id = |ctx: &mut Context| {
            ctx.last_sender_id = GTK_SURFACE;
            GtkShellHandler.on_set_dbus_properties(
                ctx,
                &Some("com.example.Terminal".to_string()),
                &None,
                &None,
                &None,
                &None,
                &None,
            );
        };

        ctx.last_sender_id = REGISTRY;
        RegistryHandler.on_global(&mut ctx, SHELL_NAME, &shell, 38);
        RegistryHandler.on_global_remove(&mut ctx, SHELL_NAME);
        assert_eq!(ctx.host_zaura_shell, None);
        ctx.last_sender_id = GTK_SHELL;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
        set_application_id(&mut ctx);
        assert_eq!(ctx.aura_surfaces.get(SURFACE.host), None);

        ctx.last_sender_id = REGISTRY;
        RegistryHandler.on_global(&mut ctx, SHELL_NAME, &shell, 38);
        let shell_host = ctx.host_zaura_shell.expect("rebound zaura_shell").id;
        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = GTK_SHELL;
        handler.on_set_startup_id(&mut ctx, &Some("launch-token".to_string()));
        set_application_id(&mut ctx);

        let zaura_surface = ctx
            .aura_surfaces
            .get(SURFACE.host)
            .expect("set_startup_id creates the Aura surface");
        let queued = queued_headers(&ctx.client_to_host_queue);
        assert_eq!(
            queued,
            vec![
                (shell_host, REQ_GET_AURA_SURFACE),
                (zaura_surface, zaura_surface::REQ_SET_STARTUP_ID),
                (zaura_surface, REQ_SET_APPLICATION_ID),
            ]
        );
    }

    #[test]
    fn gtk_surface_creation_does_not_allocate_an_arc_identity() {
        let mut ctx =
            setup_ctx_with(crate::placement_config::PlacementConfig::arc_bounds_for_test(""));
        ctx.last_sender_id = GTK_SHELL;
        GtkShellHandler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
        assert!(
            !ctx.window_placement
                .arc()
                .is_some_and(|arc| arc.has_arc_application_ids()),
            "identities are allocated lazily by the paths that send them"
        );
    }

    #[test]
    fn dbus_application_id_preserves_arc_policy_for_window_bounds() {
        let mut ctx =
            setup_ctx_with(crate::placement_config::PlacementConfig::arc_bounds_for_test(""));
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
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
        assert_eq!(msg_opcode(message), REQ_SET_APPLICATION_ID);
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
                .arc_mut()
                .and_then(|arc| arc.arc_application_id(SURFACE.guest))
                .as_deref()
        );
    }

    #[test]
    fn wl_surface_destroy_removes_gtk_and_aura_associations() {
        use crate::protocols::wayland::wl_surface::WlSurfaceHandler;

        let mut ctx = setup_ctx();
        ctx.last_sender_id = GTK_SHELL;
        let mut handler = GtkShellHandler;
        handler.on_get_gtk_surface(&mut ctx, GTK_SURFACE, SURFACE.guest);
        assert!(ctx.gtk_shells[&GTK_SHELL].surfaces.contains(&GTK_SURFACE));

        ctx.last_sender_id = SURFACE.guest;
        let mut compositor = crate::handler::compositor::CompositorHandler;
        assert_eq!(compositor.on_destroy(&mut ctx), Action::Drop);
        assert!(!ctx.gtk_surfaces.contains_key(&GTK_SURFACE));
        assert!(!ctx.gtk_shells[&GTK_SHELL].surfaces.contains(&GTK_SURFACE));
        assert!(
            ctx.shadow_table.is_local(GTK_SURFACE),
            "gtk_surface1 has no destroy request, so its client ID stays reserved"
        );
        assert!(!ctx.shadow_table.is_guest_id_available(GTK_SURFACE));
        assert_eq!(ctx.aura_surfaces.get(SURFACE.host), None);
    }
}
