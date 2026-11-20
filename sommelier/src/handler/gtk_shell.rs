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

use crate::handler::aura_objects::{self, ensure_host_zaura_surface};
use crate::handler::compositor::native_wayland_app_id;
use crate::handler::Handlers;
use crate::protocols::aura_shell::zaura_surface;
use crate::protocols::gtk::gtk_shell1::{self, GtkShell1Handler};
use crate::protocols::gtk::gtk_surface1::GtkSurface1Handler;
use crate::protocols::Iface;
use crate::state::{Context, GtkSurfaceState};
use crate::wire::{queue_or_drop, Action};

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

impl GtkShell1Handler for Handlers {
    fn on_get_gtk_surface(ctx: &mut Context, gtk_surface_id: u32, wl_surface_id: u32) -> Action {
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
        let Some(version) = ctx.shadow_table.guest_object_version(shell_id) else {
            return ctx.fail(format_args!("gtk_shell1 {shell_id} has no version"));
        };
        ctx.shadow_table
            .create_local(gtk_surface_id, Iface::GtkSurface1, version);
        ctx.gtk_surfaces.insert(
            gtk_surface_id,
            GtkSurfaceState {
                shell_id,
                wl_surface_id,
            },
        );
        if let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_id) {
            queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
        }
        Action::Drop
    }

    fn on_set_startup_id(ctx: &mut Context, startup_id: &Option<String>) -> Action {
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
        let wl_surface_ids = ctx
            .gtk_surfaces
            .values()
            .filter(|surface| surface.shell_id == shell_id)
            .map(|surface| surface.wl_surface_id)
            .collect::<std::collections::BTreeSet<_>>();
        for wl_surface_id in wl_surface_ids {
            if let Some(zaura_surface_id) = ensure_host_zaura_surface(ctx, wl_surface_id) {
                queue_startup_id(ctx, zaura_surface_id, startup_id.as_deref());
            }
        }
        Action::Drop
    }

    fn on_system_bell(_ctx: &mut Context, _surface: u32) -> Action {
        Action::Drop
    }
}

impl GtkSurface1Handler for Handlers {
    fn on_set_dbus_properties(
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
        let application_id = native_wayland_app_id(&ctx.settings().vm_id, application_id);
        aura_objects::set_application_id(ctx, wl_surface_id, &application_id);
        Action::Drop
    }

    fn on_set_modal(_ctx: &mut Context) -> Action {
        Action::Drop
    }

    fn on_unset_modal(_ctx: &mut Context) -> Action {
        Action::Drop
    }

    fn on_present(_ctx: &mut Context, _time: u32) -> Action {
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_shell, zaura_surface};
    use crate::protocols::gtk::{gtk_shell1, gtk_surface1};
    use crate::protocols::wayland::{wl_registry, wl_surface};
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::{GtkShell, Object, Session};
    use crate::settings::Settings;
    use crate::state::Context;
    use crate::wire::Outgoing;

    /// The VM the guest runs in, which namespaces its application IDs.
    const VM: &str = "penguin";

    /// A session whose guest has a `gtk_shell1` and a toplevel, or a
    /// surface without a role, which GTK wraps.
    struct Gtk {
        s: Session,
        gtk: GtkShell,
        surface: Object,
    }

    impl Gtk {
        /// A session with placement off.
        async fn new() -> Self {
            Self::with(PlacementConfig::for_test(), true).await
        }

        /// A session under `placement` whose surface is a toplevel if
        /// `toplevel`.
        async fn with(placement: PlacementConfig, toplevel: bool) -> Self {
            let settings = Settings {
                vm_id: VM.to_string(),
                ..Settings::for_test()
            };
            let mut s = Session::with_context(Context::for_test(settings, placement));
            let gtk = s.gtk_shell(zaura_shell::VERSION).await;
            let surface = if toplevel {
                let toplevel = s.toplevel().await;
                // Placement gives the toplevel an Aura toplevel.
                s.host_received();
                toplevel.surface
            } else {
                s.surface().await
            };
            Self { s, gtk, surface }
        }

        /// The guest wraps the surface in a new `gtk_surface1`; returns it.
        /// What the proxy sends either peer is kept.
        async fn gtk_surface(&mut self) -> u32 {
            let id = self.s.new_id();
            self.s
                .guest([gtk_shell1::req::get_gtk_surface(
                    self.gtk.id,
                    id,
                    self.surface.id,
                )])
                .await;
            id
        }

        /// The host gets the creation of the surface's Aura surface, then
        /// `rest`, which name it; returns it.
        fn aura_surface_and(&mut self, rest: impl Fn(u32) -> Vec<AnyRequest>) -> u32 {
            let host = self.s.host_received();
            let [(_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), ..] =
                host[..]
            else {
                panic!("the surface got no Aura surface: {host:?}");
            };
            let get = zaura_shell::Request::GetAuraSurface {
                id,
                surface: self.surface.host,
            };
            let rest = rest(id).into_iter().map(|request| (id, request));
            assert_eq!(
                host,
                Vec::from_iter([(self.gtk.shell, get.into())].into_iter().chain(rest))
            );
            id
        }
    }

    /// `set_startup_id` of `startup_id`, as the host's Aura surface gets it.
    fn startup_id(startup_id: Option<&str>) -> AnyRequest {
        zaura_surface::Request::SetStartupId {
            startup_id: startup_id.map(str::to_string),
        }
        .into()
    }

    /// The guest's `gtk_surface1.set_dbus_properties` that names the
    /// application `com.example.Terminal`.
    fn terminal(gtk_surface: u32) -> Outgoing {
        let application = Some("com.example.Terminal");
        gtk_surface1::req::set_dbus_properties(
            gtk_surface,
            application,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    }

    /// `com.example.Terminal` in the Guest OS namespace, as the host's Aura
    /// surface gets it.
    fn terminal_id() -> AnyRequest {
        let application_id = format!("org.chromium.guest_os.{VM}.wayland.com.example.Terminal");
        zaura_surface::Request::SetApplicationId {
            application_id: Some(application_id),
        }
        .into()
    }

    #[tokio::test]
    async fn new_gtk_surface_inherits_startup_id_on_shared_aura_surface() {
        let mut g = Gtk::new().await;
        g.s.guest([gtk_shell1::req::set_startup_id(g.gtk.id, Some("launch-token")).unwrap()])
            .await;
        assert_eq!(g.s.host_received(), []);

        let gtk_surface = g.gtk_surface().await;
        let aura_surface = g.aura_surface_and(|_| vec![startup_id(Some("launch-token"))]);

        // The GTK surface is the proxy's own, and wraps the surface.
        g.s.guest([terminal(gtk_surface)]).await;
        assert_eq!(g.s.host_received(), [(aura_surface, terminal_id())]);
    }

    #[tokio::test]
    async fn gtk_surface_takes_its_shell_version() {
        let mut g = Gtk::new().await;
        // Injects a fault no peer can cause: every vendored gtk-shell
        // interface is v1, so an artificial shell version shows where the
        // surface's version comes from.
        let shell_version = 7;
        g.s.ctx()
            .shadow_table
            .set_guest_version_for_test(g.gtk.id, shell_version);
        let gtk_surface = g.gtk_surface().await;
        g.aura_surface_and(|_| vec![startup_id(None)]);
        // Reads state no peer can observe, for the same reason.
        assert_eq!(
            g.s.ctx().shadow_table.guest_object_version(gtk_surface),
            Some(shell_version)
        );
    }

    #[tokio::test]
    async fn startup_id_updates_and_clears_all_unique_aura_surfaces() {
        let mut g = Gtk::new().await;
        g.gtk_surface().await;
        let aura_surface = g.aura_surface_and(|_| vec![startup_id(None)]);
        g.gtk_surface().await;
        assert_eq!(g.s.host_received(), [(aura_surface, startup_id(None))]);

        g.s.guest([gtk_shell1::req::set_startup_id(g.gtk.id, Some("next-token")).unwrap()])
            .await;
        assert_eq!(
            g.s.host_received(),
            [(aura_surface, startup_id(Some("next-token")))],
            "two GTK wrappers for one wl_surface must share one Aura update"
        );

        g.s.guest([gtk_shell1::req::set_startup_id(g.gtk.id, None).unwrap()])
            .await;
        assert_eq!(g.s.host_received(), [(aura_surface, startup_id(None))]);
    }

    /// GTK names the application of a surface with or without a role.
    #[tokio::test]
    async fn dbus_application_id_is_namespaced_on_aura_surface() {
        for (case, placement, toplevel) in [
            ("toplevel", PlacementConfig::for_test(), true),
            ("no role", PlacementConfig::for_test(), false),
            ("placement on", PlacementConfig::active_for_test(&[]), true),
        ] {
            let mut g = Gtk::with(placement, toplevel).await;
            let gtk_surface = g.gtk_surface().await;
            g.aura_surface_and(|_| vec![startup_id(None)]);

            g.s.guest([terminal(gtk_surface)]).await;
            let host = g.s.host_received();
            assert!(
                matches!(&host[..], [(_, request)] if *request == terminal_id()),
                "{case}: {host:?}"
            );
        }
    }

    /// A GTK surface created between a `zaura_shell`'s `global_remove` and
    /// its re-advertisement has no Aura surface. GTK requests issued after
    /// a new shell is bound create one; nothing replays the requests issued
    /// while no shell was bound.
    #[tokio::test]
    async fn gtk_requests_after_a_shell_rebind_reach_the_new_shell() {
        let mut g = Gtk::new().await;
        let registry = g.s.registry().await;
        g.s.global_remove(registry, g.gtk.name).await;
        let release = zaura_shell::Request::Release {};
        assert_eq!(g.s.host_received(), [(g.gtk.shell, release.into())]);
        let removal = wl_registry::Event::GlobalRemove { name: g.gtk.name };
        assert_eq!(g.s.guest_received(), [(registry.id, removal.into())]);

        let gtk_surface = g.gtk_surface().await;
        g.s.guest([terminal(gtk_surface)]).await;
        assert_eq!(g.s.host_received(), []);

        g.s.advertise(zaura_shell::NAME, zaura_shell::VERSION).await;
        g.s.guest_received();
        let host = g.s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { ref id, .. }))] = host[..]
        else {
            panic!("the host got {host:?}, not the shell's bind alone");
        };
        g.gtk.shell = id.2;

        g.s.guest([
            gtk_shell1::req::set_startup_id(g.gtk.id, Some("launch-token")).unwrap(),
            terminal(gtk_surface),
        ])
        .await;
        g.aura_surface_and(|_| vec![startup_id(Some("launch-token")), terminal_id()]);
    }

    #[tokio::test]
    async fn wl_surface_destroy_removes_gtk_and_aura_associations() {
        let mut g = Gtk::new().await;
        let gtk_surface = g.gtk_surface().await;
        let aura_surface = g.aura_surface_and(|_| vec![startup_id(None)]);

        g.s.guest([wl_surface::req::destroy(g.surface.id)]).await;
        assert_eq!(
            g.s.host_received(),
            [
                (aura_surface, zaura_surface::Request::Release {}.into()),
                (g.surface.host, wl_surface::Request::Destroy {}.into()),
            ]
        );
        // gtk_surface1 has no destroy request, so it stays the guest's,
        // with nothing to name.
        g.s.guest([terminal(gtk_surface)]).await;
        assert_eq!(g.s.host_received(), []);
        assert_eq!(g.s.guest_received(), []);
    }
}
