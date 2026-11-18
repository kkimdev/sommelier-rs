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
use crate::protocols::wayland::wl_shm;
use crate::proxy::harness::Session;
use crate::state::{Context, HostId, RenderBufferLifecycle};

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
    toplevel_ctx_with(PlacementConfig::for_test(Vec::new()))
}

/// The lifecycle of render buffer `guest_id`, while it has one.
pub(crate) fn buffer_lifecycle(ctx: &Context, guest_id: u32) -> Option<RenderBufferLifecycle> {
    let host_id: HostId = ctx.render_buffer_host_id(guest_id)?;
    ctx.render_buffer_lifecycle_for_host(host_id)
}

/// Whether the guest has destroyed render buffer `guest_id`, which the
/// proxy still holds.
pub(crate) fn buffer_is_guest_destroyed(ctx: &Context, guest_id: u32) -> bool {
    buffer_lifecycle(ctx, guest_id).is_some_and(|lifecycle| lifecycle.is_guest_destroyed())
}

/// Whether the host destroy of render buffer `guest_id` is queued.
pub(crate) fn buffer_host_destroy_is_queued(ctx: &Context, guest_id: u32) -> bool {
    buffer_lifecycle(ctx, guest_id) == Some(RenderBufferLifecycle::HostDestroyQueued)
}

/// Record that the guest destroyed render buffer `guest_id`, as its
/// `wl_buffer.destroy` does.
pub(crate) fn mark_test_buffer_guest_destroyed(ctx: &mut Context, guest_id: u32) {
    assert!(ctx.mark_buffer_guest_destroyed(guest_id));
    ctx.guest_destroyed_in_use(guest_id);
}

/// A session whose guest has bound `wl_shm`, which the proxy implements.
pub(crate) async fn session_with_shm() -> (Session, u32) {
    let mut s = Session::new();
    let shm = s.bind("wl_shm", wl_shm::VERSION).await;
    (s, shm)
}
