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

//! Fixtures shared by the handlers' unit tests.

use crate::placement_config::PlacementConfig;
use crate::protocols::aura_shell::zaura_shell;
use crate::protocols::wayland::{wl_registry, wl_shm};
use crate::protocols::AnyEvent;
use crate::proxy::harness::Session;
use crate::state::Context;

/// A guest object and the host object it is paired with.
#[derive(Clone, Copy)]
pub(crate) struct Paired {
    pub(crate) guest: u32,
    pub(crate) host: u32,
}

pub(crate) const SURFACE: Paired = Paired {
    guest: 100,
    host: 200,
};
pub(crate) const XDG_SURFACE: Paired = Paired {
    guest: 110,
    host: 210,
};
pub(crate) const TOPLEVEL: Paired = Paired {
    guest: 120,
    host: 220,
};
/// The host `zaura_shell`, at the vendored version.
pub(crate) const SHELL: u32 = 300;

/// Give `ctx` wl_surface [`SURFACE`], with no role.
pub(crate) fn add_surface(ctx: &mut Context) {
    ctx.shadow_table
        .insert_paired_for_test(SURFACE.guest, SURFACE.host, "wl_surface");
    ctx.create_surface(SURFACE.guest);
}

/// Give `ctx` wl_surface [`SURFACE`] with the toplevel role:
/// [`XDG_SURFACE`] and its [`TOPLEVEL`].
pub(crate) fn add_toplevel(ctx: &mut Context) {
    add_surface(ctx);
    for (object, interface) in [(XDG_SURFACE, "xdg_surface"), (TOPLEVEL, "xdg_toplevel")] {
        ctx.shadow_table
            .insert_paired_for_test(object.guest, object.host, interface);
    }
    ctx.xdg_roles
        .add_toplevel_role_for_test(XDG_SURFACE.guest, SURFACE.guest, TOPLEVEL.guest);
}

/// Bind the host [`SHELL`] in `ctx`.
pub(crate) fn bind_shell(ctx: &mut Context) {
    ctx.bind_aura_shell_for_test(SHELL);
    ctx.shadow_table
        .insert_host_only_for_test(SHELL, zaura_shell::NAME, zaura_shell::VERSION);
}

/// A context under `placement` with [`add_toplevel`] and [`bind_shell`].
pub(crate) fn toplevel_ctx_with(placement: PlacementConfig) -> Context {
    let mut ctx = Context::with_placement_for_test(placement);
    add_toplevel(&mut ctx);
    bind_shell(&mut ctx);
    ctx
}

/// [`toplevel_ctx_with`] placement off, the default.
pub(crate) fn toplevel_ctx() -> Context {
    toplevel_ctx_with(PlacementConfig::for_test())
}

/// A session whose guest has bound `wl_shm`, which the proxy implements.
pub(crate) async fn session_with_shm() -> (Session, u32) {
    let mut s = Session::new();
    let shm = s.bind("wl_shm", wl_shm::VERSION).await;
    (s, shm)
}

/// The version of `wl_shm` the proxy offers the guest, which has no
/// destructor (`WL_SHM_VERSION` in `handler::registry`).
pub(crate) const GUEST_SHM_VERSION: u32 = 1;

/// The host advertises `wl_shm`, which the proxy binds for itself and
/// offers the guest, which gets the global; returns the global's name and
/// the proxy's host `wl_shm`.
pub(crate) async fn shm_global(s: &mut Session) -> (u32, u32) {
    let registry = s.registry().await;
    let (name, host) = s.advertise_internal(wl_shm::NAME, wl_shm::VERSION).await;
    let global = wl_registry::Event::Global {
        name,
        interface: wl_shm::NAME.to_string(),
        version: GUEST_SHM_VERSION,
    };
    assert_eq!(s.guest_received(), [(registry.id, global.into())]);
    assert_eq!(s.host_received(), []);
    (name, host)
}

/// The guest binds `wl_shm` global `name` through its `registry`; returns
/// the new `wl_shm`, which the proxy implements and which gets the two
/// mandatory formats.
pub(crate) async fn bind_shm(s: &mut Session, registry: u32, name: u32) -> u32 {
    let shm = s.new_id();
    let bind = wl_registry::req::bind(registry, name, wl_shm::NAME, GUEST_SHM_VERSION, shm);
    s.guest([bind.unwrap()]).await;
    assert_eq!(s.host_received(), [], "the proxy implements wl_shm");
    let format = |format| -> (u32, AnyEvent) { (shm, wl_shm::Event::Format { format }.into()) };
    assert_eq!(
        s.guest_received(),
        [
            format(wl_shm::FORMAT_ARGB8888),
            format(wl_shm::FORMAT_XRGB8888)
        ]
    );
    shm
}
