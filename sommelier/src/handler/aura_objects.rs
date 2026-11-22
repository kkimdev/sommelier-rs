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

//! Lifecycle of the host-only Aura children the proxy creates from its
//! internal `zaura_shell`: `zaura_surface`, `zaura_toplevel` and
//! `zaura_output`.
//!
//! The guest never sees these objects. This module is the one place that
//! knows how they are created, which version they have, and how they are
//! released; the features that use them keep the associations:
//!
//! | Child | Association owner | Used by |
//! | :--- | :--- | :--- |
//! | `zaura_surface` | [`crate::state::AuraSurfaces`] | XDG and GTK metadata, placement |
//! | `zaura_toplevel` | [`crate::state::PlacementState`] | placement |
//! | `zaura_output` | [`crate::state::OutputRegistry`] | placement metadata |
//!
//! Of their events, only a `zaura_toplevel`'s are handled: placement
//! translates them for the guest, which has no Aura toplevel.

use crate::handler::window_placement;
use crate::handler::Handlers;
use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
use crate::protocols::Iface;
use crate::state::{Binding, Context, EventSender, GuestId, HostId, Singleton};
use crate::wire::{Action, ToHost};

/// What the proxy needs to know about one kind of host Aura child.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuraChild {
    interface: Iface,
    /// The `zaura_shell` request that creates the child, from the shell,
    /// the new child's ID and the object it extends.
    get: fn(HostId, HostId, HostId) -> ToHost,
    /// The child's destructor, which exists from `release_since` on.
    release: fn(HostId) -> ToHost,
    release_since: u32,
}

impl AuraChild {
    pub(crate) const SURFACE: Self = Self {
        interface: Iface::ZauraSurface,
        get: zaura_shell::req::get_aura_surface,
        release: zaura_surface::req::release,
        release_since: zaura_surface::REQ_RELEASE_SINCE,
    };
    pub(crate) const TOPLEVEL: Self = Self {
        interface: Iface::ZauraToplevel,
        get: zaura_shell::req::get_aura_toplevel_for_xdg_toplevel,
        release: zaura_toplevel::req::release,
        release_since: zaura_toplevel::REQ_RELEASE_SINCE,
    };
    pub(crate) const OUTPUT: Self = Self {
        interface: Iface::ZauraOutput,
        get: zaura_shell::req::get_aura_output,
        release: zaura_output::req::release,
        release_since: zaura_output::REQ_RELEASE_SINCE,
    };
}

/// The version [`create`] recorded for `child_host_id` while it is a live
/// Aura child of kind `child`: its shell's.
///
/// `None` when it is not one: the ID is retired or untracked, or was
/// recycled for an object of another interface, whose requests have other
/// opcodes. Association owners forget a child before it ends, so that is a
/// bookkeeping bug, which debug builds assert against.
fn live_child_version(ctx: &Context, child: AuraChild, child_host_id: HostId) -> Option<u32> {
    let version = (ctx.shadow_table.get_host_interface(child_host_id) == Some(child.interface))
        .then(|| ctx.shadow_table.host_object_version(child_host_id))
        .flatten();
    debug_assert!(
        version.is_some(),
        "host ID {child_host_id} is not a live {}",
        child.interface
    );
    version
}

/// Create a host Aura child that extends `parent_host_id`.
///
/// Allocates a host-only ID, records it with the shell's version, and queues
/// the `zaura_shell` request that creates it. The caller records the
/// returned ID with the child's association owner.
///
/// Every bound shell can create Aura surfaces and outputs (see the registry's
/// `ZAURA_SHELL_MIN_VERSION`); the one caller that creates a toplevel checks
/// the newer version placement needs first.
///
/// # Returns
///
/// `None`, queueing nothing, when no `zaura_shell` is bound.
pub(crate) fn create(
    ctx: &mut Context,
    child: AuraChild,
    parent_host_id: HostId,
) -> Option<HostId> {
    let shell = ctx.globals.singletons().get(Singleton::AuraShell)?;
    let child_host_id = ctx
        .shadow_table
        .create_host_only(child.interface, shell.version);
    ctx.send_host((child.get)(shell.host, child_host_id, parent_host_id));
    Some(child_host_id)
}

/// Release a host Aura child whose association owner has forgotten it (see
/// [`release_host_only`]).
///
/// Callers release a child before they queue the destructor of the object
/// it extends, so the host never holds an Aura child whose parent is gone.
///
/// An ID that is not a live child of kind `child` (see
/// [`live_child_version`]) is left alone: a retired or untracked one has
/// nothing left to release, and a recycled one belongs to another object.
pub(crate) fn release(ctx: &mut Context, child: AuraChild, child_host_id: HostId) {
    let Some(version) = live_child_version(ctx, child, child_host_id) else {
        return;
    };
    release_host_only(
        ctx,
        child_host_id,
        version,
        child.release,
        child.release_since,
    );
}

/// Release the internal `zaura_shell` binding after its `global_remove`
/// (see [`release_host_only`]). Its children stay usable.
pub(crate) fn release_shell(ctx: &mut Context, shell: Binding) {
    release_host_only(
        ctx,
        shell.host,
        shell.version,
        zaura_shell::req::release,
        zaura_shell::REQ_RELEASE_SINCE,
    );
}

/// Release a host-only Aura object of negotiated `version`.
///
/// From the `release_since` version on, the destructor is queued and the
/// host ID stays reserved until the compositor acknowledges it with
/// `wl_display.delete_id`, so a recycled ID never receives the late
/// acknowledgement. Older objects have no destructor: their dispatch
/// metadata is retired so stale host events cannot reach them, and the
/// numeric ID stays reserved for the rest of the connection.
fn release_host_only(
    ctx: &mut Context,
    host_id: HostId,
    version: u32,
    release: fn(HostId) -> ToHost,
    release_since: u32,
) {
    if version >= release_since {
        ctx.send_host(release(host_id));
        ctx.retire_destroyed_host_object(host_id);
    } else {
        ctx.shadow_table.retire_host(host_id);
    }
}

/// Create or reuse the one host Aura surface of a guest wl_surface (see
/// [`crate::state::AuraSurfaces`]).
///
/// # Returns
///
/// `None` when the wl_surface has no host object or no `zaura_shell` is
/// bound, and, in release builds, when the Aura surface recorded for it is
/// no longer live.
pub(crate) fn ensure_host_zaura_surface(
    ctx: &mut Context,
    wl_surface_guest_id: GuestId,
) -> Option<HostId> {
    let wl_surface_host_id = ctx.shadow_table.get_host_id(wl_surface_guest_id)?;
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.get(wl_surface_host_id) {
        // A recorded surface that is no longer live is a bookkeeping bug
        // (see `live_child_version`), and nothing may be sent to it.
        return live_child_version(ctx, AuraChild::SURFACE, zaura_surface_host_id)
            .map(|_| zaura_surface_host_id);
    }
    let zaura_surface_host_id = create(ctx, AuraChild::SURFACE, wl_surface_host_id)?;
    ctx.aura_surfaces
        .remember(wl_surface_host_id, zaura_surface_host_id);
    Some(zaura_surface_host_id)
}

/// The application ID the host gets for guest application ID `app_id`: in
/// the Guest OS namespace of VM `vm_identifier` (see INVARIANTS, "ChromeOS
/// shelf application IDs").
pub(crate) fn native_wayland_app_id(vm_identifier: &str, app_id: &str) -> String {
    format!("org.chromium.guest_os.{}.wayland.{}", vm_identifier, app_id)
}

/// Send the application ID the guest set (through XDG or GTK) to the
/// surface's host Aura surface, creating that surface if needed. Nothing is
/// sent while no `zaura_shell` is bound.
///
/// The Aura surface needs no version check: the registry binds no
/// `zaura_shell` older than `set_application_id`.
///
/// # Arguments
///
/// * `native_application_id` - the guest's ID in the Guest OS namespace
///   ([`native_wayland_app_id`]). An ID too long for one message
///   is dropped with a warning.
pub(crate) fn set_application_id(
    ctx: &mut Context,
    wl_surface_guest_id: GuestId,
    native_application_id: &str,
) {
    let Some(zaura_surface_host_id) = ensure_host_zaura_surface(ctx, wl_surface_guest_id) else {
        return;
    };
    if ctx.send_host_or_drop(
        zaura_surface::req::set_application_id(zaura_surface_host_id, Some(native_application_id)),
        format_args!("the application ID of zaura_surface {zaura_surface_host_id}"),
    ) {
        log::debug!(
            "Set application ID to {} on zaura_surface (host_id={})",
            native_application_id,
            zaura_surface_host_id
        );
    }
}

/// Release the Aura surface of a host wl_surface that is being destroyed,
/// if it has one.
pub(crate) fn release_host_zaura_surface(ctx: &mut Context, wl_surface_host_id: HostId) {
    if let Some(zaura_surface_host_id) = ctx.aura_surfaces.take(wl_surface_host_id) {
        release(ctx, AuraChild::SURFACE, zaura_surface_host_id);
    }
}

/// Give a live output its host-only `zaura_output` if it has none yet.
///
/// It carries the Aura scale factors, logical transform and work-area insets
/// that placement needs; the guest only sees the plain `wl_output`. Without a
/// bound `zaura_shell` nothing is created, and binding one retries.
pub(crate) fn ensure_zaura_output(ctx: &mut Context, wl_output_host_id: HostId) {
    if ctx.outputs.aura_output(wl_output_host_id).is_some()
        || !ctx.outputs.is_live(wl_output_host_id)
    {
        return;
    }
    if let Some(zaura_output_host_id) = create(ctx, AuraChild::OUTPUT, wl_output_host_id) {
        ctx.outputs
            .remember_aura_output(wl_output_host_id, zaura_output_host_id);
    }
}

impl zaura_toplevel::ZauraToplevelHandler for Handlers {
    fn on_configure(
        ctx: &mut Context,
        sender: EventSender,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        states: &[u8],
    ) -> Action {
        // The placement module delivers the translated event itself.
        window_placement::translate_aura_configure(ctx, sender.host, x, y, width, height, states);
        Action::Drop
    }

    fn on_origin_change(ctx: &mut Context, sender: EventSender, x: i32, y: i32) -> Action {
        // xdg_toplevel has no origin-change event: placement keeps the
        // origin, and the guest gets nothing.
        window_placement::origin_changed(ctx, sender.host, x, y);
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use crate::placement_config::PlacementConfig;
    use crate::protocols::aura_shell::{zaura_output, zaura_shell, zaura_surface, zaura_toplevel};
    use crate::protocols::gtk::gtk_shell1;
    use crate::protocols::wayland::{wl_output, wl_registry, wl_surface};
    use crate::protocols::xdg_shell::{xdg_surface, xdg_toplevel, xdg_wm_base};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::{GtkShell, Object, Session};
    use crate::test_support::TestConnection;

    /// A kind of object the proxy gives a host Aura child.
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        /// A `wl_surface` GTK wraps: its `zaura_surface`.
        Surface,
        /// An `xdg_toplevel` under placement: its `zaura_toplevel`.
        Toplevel,
        /// A `wl_output` under placement: its `zaura_output`.
        Output,
    }
    const KINDS: [Kind; 3] = [Kind::Surface, Kind::Toplevel, Kind::Output];

    /// A session under placement whose proxy bound `zaura_shell`, and whose
    /// guest has what it needs to create each kind of parent.
    struct Aura {
        s: Session,
        registry: Object,
        gtk: GtkShell,
        wm_base: u32,
        /// A `wl_output` global.
        output: u32,
    }

    impl Aura {
        /// The proxy binds `zaura_shell` at `version`.
        async fn new(version: u32) -> Self {
            let placement = PlacementConfig::active_for_test(&[]);
            let mut s =
                Session::with_context(TestConnection::default().placement(placement).context());
            let registry = s.registry().await;
            let gtk = s.gtk_shell(version).await;
            let wm_base = s.bind(xdg_wm_base::NAME, xdg_wm_base::VERSION).await;
            let output = s.advertise(wl_output::NAME, 3).await;
            s.guest_received();
            Self {
                s,
                registry,
                gtk,
                wm_base,
                output,
            }
        }

        /// The guest creates a parent of `kind`, which the proxy gives an
        /// Aura child: the host gets the shell's request for it with the
        /// parent's own forward. Returns the parent and the child.
        async fn create(&mut self, kind: Kind) -> (Object, u32) {
            let shell = self.gtk.shell;
            match kind {
                Kind::Surface => {
                    let surface = self.s.surface().await;
                    let gtk_surface = self.s.new_id();
                    self.s
                        .guest([gtk_shell1::req::get_gtk_surface(
                            self.gtk.id,
                            gtk_surface,
                            surface.id,
                        )])
                        .await;
                    let host = self.s.host_received();
                    let [(
                        _,
                        AnyRequest::ZauraShell(zaura_shell::Request::GetAuraSurface { id, .. }),
                    ), _] = host[..]
                    else {
                        panic!("the surface got no Aura surface: {host:?}");
                    };
                    let get = zaura_shell::Request::GetAuraSurface {
                        id,
                        surface: surface.host,
                    };
                    let startup_id = zaura_surface::Request::SetStartupId { startup_id: None };
                    assert_eq!(host, [(shell, get.into()), (id, startup_id.into())]);
                    (surface, id)
                }
                Kind::Toplevel => {
                    let surface = self.s.surface().await;
                    let xdg = self.s.new_id();
                    let get_xdg_surface =
                        xdg_wm_base::req::get_xdg_surface(self.wm_base, xdg, surface.id);
                    let xdg_host = self.s.create(get_xdg_surface).await;
                    let id = self.s.new_id();
                    self.s
                        .guest([xdg_surface::req::get_toplevel(xdg, id)])
                        .await;
                    let host = self.s.host_received();
                    let [(
                        _,
                        AnyRequest::XdgSurface(xdg_surface::Request::GetToplevel { id: toplevel }),
                    ), (
                        _,
                        AnyRequest::ZauraShell(
                            zaura_shell::Request::GetAuraToplevelForXdgToplevel {
                                id: aura, ..
                            },
                        ),
                    ), _] = host[..]
                    else {
                        panic!("the toplevel got no Aura toplevel: {host:?}");
                    };
                    let get =
                        zaura_shell::Request::GetAuraToplevelForXdgToplevel { id: aura, toplevel };
                    let screen = zaura_toplevel::Request::SetSupportsScreenCoordinates {};
                    assert_eq!(
                        host,
                        [
                            (
                                xdg_host,
                                xdg_surface::Request::GetToplevel { id: toplevel }.into()
                            ),
                            (shell, get.into()),
                            (aura, screen.into()),
                        ]
                    );
                    (Object { id, host: toplevel }, aura)
                }
                Kind::Output => self.bind_output(self.output).await,
            }
        }

        /// The guest binds `wl_output` global `name` at v3, which the proxy
        /// gives an Aura output; returns the output and its Aura output.
        async fn bind_output(&mut self, name: u32) -> (Object, u32) {
            let guest = self.s.new_id();
            let bind = wl_registry::req::bind(self.registry.id, name, wl_output::NAME, 3, guest);
            self.s.guest([bind.unwrap()]).await;
            let host = self.s.host_received();
            let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { ref id, .. })), (_, AnyRequest::ZauraShell(zaura_shell::Request::GetAuraOutput { id: aura, .. }))] =
                host[..]
            else {
                panic!("the output got no Aura output: {host:?}");
            };
            let output = id.2;
            let get = zaura_shell::Request::GetAuraOutput { id: aura, output };
            assert_eq!(host[1], (self.gtk.shell, get.into()));
            (
                Object {
                    id: guest,
                    host: output,
                },
                aura,
            )
        }

        /// The guest ends `parent`, of `kind`, which ends its Aura child
        /// `child`: the host gets the child's release if `released`, then
        /// the parent's destructor.
        async fn end(&mut self, kind: Kind, parent: Object, child: u32, released: bool) {
            let (request, destructor, release): (_, AnyRequest, AnyRequest) = match kind {
                Kind::Surface => (
                    wl_surface::req::destroy(parent.id),
                    wl_surface::Request::Destroy {}.into(),
                    zaura_surface::Request::Release {}.into(),
                ),
                Kind::Toplevel => (
                    xdg_toplevel::req::destroy(parent.id),
                    xdg_toplevel::Request::Destroy {}.into(),
                    zaura_toplevel::Request::Release {}.into(),
                ),
                Kind::Output => (
                    wl_output::req::release(parent.id),
                    wl_output::Request::Release {}.into(),
                    zaura_output::Request::Release {}.into(),
                ),
            };
            self.s.guest([request]).await;
            let release = released.then_some((child, release));
            assert_eq!(
                self.s.host_received(),
                Vec::from_iter(release.into_iter().chain([(parent.host, destructor)])),
                "{kind:?}"
            );
            assert_eq!(self.s.guest_received(), [], "{kind:?}");
        }
    }

    /// The proxy creates each Aura child from its `zaura_shell`, extending
    /// the child's parent, here under the oldest shell it creates every
    /// child from: placement's.
    #[tokio::test]
    async fn create_queues_the_shell_request_with_the_shell_version() {
        let version = zaura_output::EVT_INSETS_SINCE;
        let mut a = Aura::new(version).await;
        for kind in KINDS {
            a.create(kind).await;
        }
    }

    /// A child takes its shell's version, so it is released with its
    /// parent only from the version its destructor has on.
    #[tokio::test]
    async fn release_follows_each_childs_release_version() {
        for (version, released) in [(38, true), (37, false)] {
            assert_eq!(zaura_surface::REQ_RELEASE_SINCE, 38);
            assert_eq!(zaura_toplevel::REQ_RELEASE_SINCE, 38);
            assert_eq!(zaura_output::REQ_RELEASE_SINCE, 38);
            let mut a = Aura::new(version).await;
            for kind in KINDS {
                let (parent, child) = a.create(kind).await;
                a.end(kind, parent, child, released).await;
            }
        }
    }

    /// An output gets one Aura output, which a new shell binding keeps,
    /// and an output whose global the host removed gets none.
    #[tokio::test]
    async fn only_a_live_output_without_one_gets_an_aura_output() {
        let mut a = Aura::new(zaura_shell::VERSION).await;
        a.create(Kind::Output).await;
        let removed = a.s.advertise(wl_output::NAME, 3).await;
        a.s.guest_received();
        let (_, removed_aura_output) = a.bind_output(removed).await;
        let registry = a.registry;
        a.s.global_remove(registry, removed).await;
        let release = zaura_output::Request::Release {};
        assert_eq!(a.s.host_received(), [(removed_aura_output, release.into())]);

        // The shell retires, and the host advertises a new one.
        a.s.global_remove(registry, a.gtk.name).await;
        let release = zaura_shell::Request::Release {};
        assert_eq!(a.s.host_received(), [(a.gtk.shell, release.into())]);
        let removals = [removed, a.gtk.name].map(|name| {
            let removal: AnyEvent = wl_registry::Event::GlobalRemove { name }.into();
            (registry.id, removal)
        });
        assert_eq!(a.s.guest_received(), removals);
        a.s.global(registry, 99, zaura_shell::NAME, zaura_shell::VERSION)
            .await;
        a.s.guest_received();
        let host = a.s.host_received();
        assert!(
            matches!(
                &host[..],
                [(
                    _,
                    AnyRequest::WlRegistry(wl_registry::Request::Bind { name: 99, .. })
                )]
            ),
            "the host got {host:?}, not the new shell's bind alone"
        );
    }
}
