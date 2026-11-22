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

//! The guest's XDG shell, as far as the proxy follows it: each
//! `xdg_surface` and `xdg_toplevel`'s link to its `wl_surface`
//! (`crate::state::XdgRoles`), a toplevel's application ID, and the
//! toplevel's place in window placement.
//!
//! ChromeOS needs a `zaura_surface` to set the application ID that the
//! shelf uses for icon matching, but the ID arrives through
//! `xdg_toplevel.set_app_id`, which names no `wl_surface`. These handlers
//! record the chain `get_xdg_surface(xdg_surface, wl_surface)` →
//! `get_toplevel(xdg_toplevel)` and end it in the destructors, so
//! `set_app_id` can resolve the underlying `wl_surface` and create or reuse
//! its host `zaura_surface`. Every other XDG request and event forwards as
//! it is.

use crate::handler::aura_objects;
use crate::handler::window_placement;
use crate::handler::Handlers;
use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel, xdg_wm_base};
use crate::state::{Context, EventSender, GuestId, PostDispatch, RequestObject, RequestSender};
use crate::wire::{Action, Destructor};

impl xdg_wm_base::XdgWmBaseHandler for Handlers {
    fn on_get_xdg_surface(
        ctx: &mut Context,
        _sender: RequestSender,
        id: GuestId,
        surface: RequestObject,
    ) -> Action {
        ctx.xdg_roles.add_xdg_surface(id, surface.guest);
        Action::Forward
    }
}

/// Forget guest `xdg_surface` `xdg_surface_id`, which ended (see
/// `crate::state::lifecycle`), with the role of its toplevel.
///
/// Destroying an xdg_surface before its xdg_toplevel is a protocol error
/// (`xdg_surface.defunct_role_object`) that ends the session, so the order
/// of the forwarded destructor and the queued Aura release does not matter.
/// The role ends anyway, so the toplevel's later requests cannot reach an
/// unrelated surface after ID reuse.
pub(crate) fn forget_xdg_surface(ctx: &mut Context, xdg_surface_id: GuestId) {
    if let Some(xdg_toplevel_id) = ctx.xdg_roles.remove_xdg_surface(xdg_surface_id) {
        window_placement::release_toplevel(ctx, xdg_toplevel_id);
    }
}

/// Forget guest `xdg_toplevel` `xdg_toplevel_id`, which ended (see
/// `crate::state::lifecycle`), and release its Aura toplevel.
pub(crate) fn forget_xdg_toplevel(ctx: &mut Context, xdg_toplevel_id: GuestId) {
    window_placement::release_toplevel(ctx, xdg_toplevel_id);
    ctx.xdg_roles.remove_toplevel(xdg_toplevel_id);
}

impl xdg_surface::XdgSurfaceHandler for Handlers {
    fn on_configure(ctx: &mut Context, sender: EventSender, serial: u32) -> Action {
        let Some(guest_xdg_surface_id) = sender.guest_id() else {
            return Action::Drop;
        };
        window_placement::surface_configured(ctx, guest_xdg_surface_id, serial);
        Action::Forward
    }

    fn on_get_toplevel(ctx: &mut Context, sender: RequestSender, id: GuestId) -> Action {
        ctx.defer_until_forwarded(PostDispatch::XdgToplevelCreated {
            xdg_surface: sender.guest,
            xdg_toplevel: id,
        });
        Action::Forward
    }
}

impl xdg_toplevel::XdgToplevelHandler for Handlers {
    fn on_destroy(_ctx: &mut Context, _sender: RequestSender) -> Destructor {
        // Ending the toplevel releases its Aura toplevel (see
        // `forget_xdg_toplevel`), which goes before the role destructor (see
        // `aura_objects::release`). The guest ID is retained until the host
        // acknowledges the role destructor.
        Destructor::AfterEnd
    }

    fn on_set_app_id(ctx: &mut Context, sender: RequestSender, app_id: &String) -> Action {
        let xdg_toplevel_id = sender.guest;

        let Some(xdg_toplevel_host_id) = sender.host_id() else {
            log::warn!(
                "Cannot namespace app ID for unmapped xdg_toplevel {}",
                xdg_toplevel_id
            );
            return Action::Drop;
        };
        // The host XDG role and the Aura surface both get the ID in the Guest
        // OS namespace.
        let native_app_id = aura_objects::native_wayland_app_id(&ctx.settings().vm_id, app_id);

        // The host xdg_toplevel carries the app ID used by ordinary Exo
        // shelf/application matching. Keep that request namespaced as in the
        // reference Sommelier path rather than forwarding the guest's
        // unqualified ID directly. An ID too long for one message reaches
        // neither the role nor the Aura surface.
        if !ctx.send_host_or_drop(
            xdg_toplevel::req::set_app_id(xdg_toplevel_host_id, &native_app_id),
            format_args!("the application ID of xdg_toplevel {xdg_toplevel_id}"),
        ) {
            return Action::Drop;
        }

        // Resolve xdg_toplevel → wl_surface (guest) → wl_surface (host).
        if let Some(wl_surface_guest_id) = ctx.xdg_roles.wl_surface_of_toplevel(xdg_toplevel_id) {
            aura_objects::set_application_id(ctx, wl_surface_guest_id, &native_app_id);
        }
        Action::Drop
    }

    fn on_configure(
        _ctx: &mut Context,
        sender: EventSender,
        _width: i32,
        _height: i32,
        _states: &[u8],
    ) -> Action {
        if sender.guest.is_none() {
            return Action::Drop;
        }
        Action::Forward
    }
}

/// Record the XDG role of a forwarded `xdg_surface.get_toplevel`
/// ([`PostDispatch::XdgToplevelCreated`]). Running only after the forward
/// avoids stale application/surface state when validation rejects the
/// request. The placement Aura toplevel is attached only after the role is
/// recorded, so it never exists for a rejected request, and the forward
/// has mapped the toplevel's host ID that it extends.
pub(crate) fn record_xdg_toplevel(
    ctx: &mut Context,
    xdg_surface_guest_id: GuestId,
    xdg_toplevel_guest_id: GuestId,
) {
    debug_assert!(
        ctx.shadow_table
            .get_host_id(xdg_toplevel_guest_id)
            .is_some(),
        "a forwarded get_toplevel has mapped its new ID"
    );
    if ctx
        .xdg_roles
        .add_toplevel(xdg_surface_guest_id, xdg_toplevel_guest_id)
        .is_some()
    {
        window_placement::attach_toplevel(ctx, xdg_toplevel_guest_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::protocols::aura_shell::{zaura_shell, zaura_surface};
    use crate::protocols::wayland::{wl_display, wl_registry, wl_surface};
    use crate::protocols::xdg_shell::xdg_toplevel;
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::{Object, Session, Toplevel};
    use crate::state::Context;
    use crate::wire::WL_DISPLAY_ID;

    /// The host destructor of guest surface `surface`.
    fn surface_destroy(surface: Object) -> (u32, AnyRequest) {
        (surface.host, wl_surface::Request::Destroy {}.into())
    }

    /// A session with `ctx` whose host has `zaura_shell`, which the proxy
    /// binds for itself, and whose guest has a toplevel; returns the session,
    /// the host `zaura_shell` and the toplevel.
    async fn shelled(ctx: Context) -> (Session, u32, Toplevel) {
        let mut s = Session::with_context(ctx);
        let shell = s.aura_shell().await;
        let toplevel = s.toplevel().await;
        (s, shell, toplevel)
    }

    /// The guest sets `app_id` on `toplevel`; returns what the host gets.
    async fn set_app_id(s: &mut Session, toplevel: Object, app_id: &str) -> Vec<(u32, AnyRequest)> {
        s.guest([xdg_toplevel::req::set_app_id(toplevel.id, app_id).unwrap()])
            .await;
        assert_eq!(s.guest_received(), []);
        s.host_received()
    }

    /// The host `xdg_toplevel.set_app_id` of `toplevel`, of guest OS app ID
    /// `app_id`.
    fn namespaced(toplevel: Object, app_id: &str) -> (u32, AnyRequest) {
        let app_id = format!("org.chromium.guest_os.termina.wayland.{app_id}");
        (
            toplevel.host,
            xdg_toplevel::Request::SetAppId { app_id }.into(),
        )
    }

    /// What the host gets for the guest's first `set_app_id` of `app_id` on
    /// `toplevel` while the proxy has `shell`: the namespaced ID, and a new
    /// Aura surface of the toplevel's surface, which gets it too; returns
    /// the Aura surface.
    fn first_app_id(
        host: &[(u32, AnyRequest)],
        shell: u32,
        toplevel: Toplevel,
        app_id: &str,
    ) -> u32 {
        let [_, (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. })), _] =
            host
        else {
            panic!("the host got no Aura surface: {host:?}");
        };
        let application_id = Some(format!("org.chromium.guest_os.termina.wayland.{app_id}"));
        assert_eq!(
            host,
            [
                namespaced(toplevel.toplevel, app_id),
                (
                    shell,
                    zaura_shell::Request::GetAuraSurface {
                        id: *id,
                        surface: toplevel.surface.host
                    }
                    .into()
                ),
                (
                    *id,
                    zaura_surface::Request::SetApplicationId { application_id }.into()
                ),
            ]
        );
        *id
    }

    #[tokio::test]
    async fn set_app_id_creates_zaura_surface_and_sets_app_id() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let host = set_app_id(&mut s, toplevel.toplevel, "my_app").await;
        first_app_id(&host, shell, toplevel, "my_app");
    }

    #[tokio::test]
    async fn set_app_id_reuses_existing_zaura_surface() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        // An Aura surface the first app ID created.
        let host = set_app_id(&mut s, toplevel.toplevel, "first").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "first");

        let application_id = Some("org.chromium.guest_os.termina.wayland.reused".into());
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, "reused").await,
            [
                namespaced(toplevel.toplevel, "reused"),
                (
                    aura_surface,
                    zaura_surface::Request::SetApplicationId { application_id }.into()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn set_app_id_keeps_the_guest_os_namespace_with_placement_on() {
        let placement = crate::placement_config::PlacementConfig::active_for_test(&[]);
        let ctx = Context::with_placement_for_test(placement);
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let app_id = "com.example.Terminal";
        let host = set_app_id(&mut s, toplevel.toplevel, app_id).await;
        first_app_id(&host, shell, toplevel, app_id);
    }

    #[tokio::test]
    async fn set_app_id_noop_when_no_zaura_shell() {
        let mut s = Session::new();
        let toplevel = s.toplevel().await.toplevel;
        assert_eq!(
            set_app_id(&mut s, toplevel, "no_shell").await,
            [namespaced(toplevel, "no_shell")]
        );
    }

    /// The longest app ID a guest can send, which namespaced exceeds the
    /// Wayland message limit, reaches neither the role nor an Aura surface.
    #[tokio::test]
    async fn oversized_namespaced_app_id_is_dropped_before_wire_encoding() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, _, toplevel) = shelled(ctx).await;
        let oversized = "x".repeat(4_083);
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, &oversized).await,
            [],
            "an app ID exceeding the Wayland message limit must not be sent"
        );
    }

    /// Destroying a toplevel ends its role, which the session checks after
    /// each batch (`Context::assert_no_orphans`): the role must not outlive
    /// its toplevel.
    #[tokio::test]
    async fn xdg_toplevel_destroy_cleans_up_map() {
        use crate::protocols::xdg_shell::xdg_surface;
        let mut s = Session::new();
        let Toplevel {
            xdg_surface: role,
            toplevel,
            ..
        } = s.toplevel().await;
        s.guest([xdg_toplevel::req::destroy(toplevel.id)]).await;
        let destroy = xdg_toplevel::Request::Destroy {};
        assert_eq!(s.host_received(), [(toplevel.host, destroy.into())]);
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, toplevel.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: toplevel.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        // The xdg_surface, which has no role left, may end now.
        s.guest([xdg_surface::req::destroy(role.id)]).await;
        let destroy = xdg_surface::Request::Destroy {};
        assert_eq!(s.host_received(), [(role.host, destroy.into())]);
    }

    #[tokio::test]
    async fn wl_surface_destroy_releases_zaura_surface() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, shell, toplevel) = shelled(ctx).await;
        let host = set_app_id(&mut s, toplevel.toplevel, "app").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "app");

        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [
                (aura_surface, zaura_surface::Request::Release {}.into()),
                surface_destroy(surface),
            ]
        );
    }

    /// Destroying a surface ends the xdg role on it, so its toplevel's
    /// later app ID reaches no Aura surface of the destroyed surface.
    #[tokio::test]
    async fn wl_surface_destroy_removes_stale_xdg_surface_links() {
        let ctx = Context::new_for_test(false, false, Vec::new());
        let (mut s, _, toplevel) = shelled(ctx).await;
        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(s.host_received(), [surface_destroy(surface)]);
        assert_eq!(
            set_app_id(&mut s, toplevel.toplevel, "app").await,
            [namespaced(toplevel.toplevel, "app")]
        );
    }

    /// An Aura shell older than `zaura_surface.release` (v38) has no
    /// destructor for an Aura surface: destroying its surface sends none,
    /// and the Aura surface's host ID stays reserved for the connection.
    #[tokio::test]
    async fn old_aura_surface_stays_reserved_without_release_request() {
        let mut s = Session::new();
        s.registry().await;
        s.advertise(zaura_shell::NAME, 37).await;
        let host = s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. }))] = &host[..] else {
            panic!("the proxy did not bind zaura_shell: {host:?}");
        };
        let shell = id.2;
        // The gtk_shell1 the guest is advertised.
        assert_eq!(s.guest_received().len(), 1);
        let toplevel = s.toplevel().await;
        let host = set_app_id(&mut s, toplevel.toplevel, "legacy").await;
        let aura_surface = first_app_id(&host, shell, toplevel, "legacy");

        let surface = toplevel.surface;
        s.guest([wl_surface::req::destroy(surface.id)]).await;
        assert_eq!(
            s.host_received(),
            [surface_destroy(surface)],
            "old aura-shell versions must not receive release"
        );
        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, surface.host)])
            .await;
        let deleted = wl_display::Event::DeleteId { id: surface.id };
        assert_eq!(s.guest_received(), [(WL_DISPLAY_ID, deleted.into())]);
        // The proxy reuses freed host IDs first: the surface's, and the
        // Aura surface's if it were free.
        for _ in 0..2 {
            assert_ne!(s.surface().await.host, aura_surface);
        }
    }

    #[test]
    fn namespaced_app_id_wire_size_boundary_is_checked() {
        // Exercise the exact 4096-byte message boundary independently of the
        // namespace prefix used by the production path.
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_083)).is_ok());
        assert!(xdg_toplevel::req::set_app_id(1, &"x".repeat(4_084)).is_err());
    }
}
