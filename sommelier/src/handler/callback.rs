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

use crate::handler::Handlers;
use crate::protocols::wayland::wl_callback::{self, WlCallbackHandler};
use crate::protocols::wayland::wl_display;
use crate::protocols::Iface;
use crate::state::{Context, HostId, InternalCallback};
use crate::wire::{Action, Outgoing, WL_DISPLAY_ID};
use log::debug;

/// Issue a host-only `wl_display.sync` whose `done` means `waits_for` has
/// happened (see [`InternalCallback`]).
///
/// # Returns
///
/// The callback's host ID, which stays reserved until the host's
/// `wl_display.delete_id`.
pub(crate) fn queue_internal_sync(ctx: &mut Context, waits_for: InternalCallback) -> HostId {
    let (callback, sync) = internal_sync(ctx, waits_for);
    ctx.client_to_host_queue.push(sync);
    callback
}

/// [`queue_internal_sync`]'s callback and its `wl_display.sync`, for the
/// caller to send.
pub(crate) fn internal_sync(ctx: &mut Context, waits_for: InternalCallback) -> (HostId, Outgoing) {
    let callback = HostId(
        ctx.shadow_table
            .create_host_only(Iface::WlCallback, wl_callback::VERSION),
    );
    ctx.internal_callbacks.register(callback, waits_for);
    (callback, wl_display::req::sync(WL_DISPLAY_ID, callback.0))
}

impl WlCallbackHandler for Handlers {
    fn on_done(ctx: &mut Context, callback_data: u32) -> Action {
        let host_id = ctx.last_sender_id;
        if let Some(waits_for) = ctx.internal_callbacks.complete(HostId(host_id)) {
            // wl_callback.done destroys the host callback resource. Reserve
            // its numeric ID until wl_display.delete_id arrives, while making
            // any duplicate event undispatchable immediately.
            ctx.retire_destroyed_host_object(host_id);
            match waits_for {
                InternalCallback::DmabufCapabilities { global } => {
                    crate::handler::linux_dmabuf::complete_capability_discovery(ctx, global);
                    crate::handler::linux_dmabuf::maybe_reclaim_capabilities(ctx, global);
                }
                InternalCallback::GtkShellCapabilities { gtk_shell } => {
                    crate::handler::gtk_shell::announce_capabilities(ctx, gtk_shell);
                }
                // W1 and W2: a text input's drain or reset fence.
                InternalCallback::TextInput { token } => {
                    crate::handler::text_input::ime(ctx, crate::ime::Input::BarrierDone(token));
                }
                // Every host event, this `done` included, has decided the
                // pending recovery before it is handled.
                InternalCallback::KeyRecovery => {}
                InternalCallback::PlacementBarrier { zaura_toplevel } => {
                    crate::handler::window_placement::complete_barrier(
                        ctx,
                        host_id,
                        zaura_toplevel,
                    );
                }
            }
            return Action::Drop;
        }
        let guest_id = ctx.shadow_table.get_guest_id(host_id).unwrap_or(0);

        if guest_id != 0 {
            debug!(
                "wl_callback.done for guest_id {}, sending done and delete_id",
                guest_id
            );

            // The done event, then the delete_id the guest expects for it.
            ctx.host_to_client_queue.extend([
                wl_callback::evt::done(guest_id, callback_data),
                wl_display::evt::delete_id(WL_DISPLAY_ID, guest_id),
            ]);

            // The terminal event destroys the callback on both sides, but the
            // host numeric ID cannot be reused until its later delete_id.
            ctx.guest_ended_host_destroyed(guest_id);
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::wayland::{wl_callback, wl_display, wl_registry};
    use crate::protocols::{AnyEvent, AnyRequest};
    use crate::proxy::harness::Session;
    use crate::state::Context;
    use crate::wire::WL_DISPLAY_ID;

    /// The callback of a `wl_display.sync` the host got from the proxy.
    fn synced(request: &(u32, AnyRequest)) -> u32 {
        match request {
            (WL_DISPLAY_ID, AnyRequest::WlDisplay(wl_display::Request::Sync { callback })) => {
                *callback
            }
            _ => panic!("{request:?} is no sync"),
        }
    }

    /// The proxy's own sync, here the one that asks for a `gtk_shell1`'s
    /// capabilities, is a host callback the proxy tracks: its `done` and
    /// `delete_id` reach the proxy alone, and its ID stays reserved until
    /// the `delete_id`, which frees it for the next host object.
    #[tokio::test]
    async fn internal_sync_is_a_tracked_host_callback() {
        use crate::protocols::aura_shell::zaura_shell;
        use crate::protocols::gtk::gtk_shell1;
        let mut s = Session::new();
        // Binds the compositor, so that each later surface takes one host
        // ID.
        s.surface().await;
        let registry = s.registry().await;
        let name = s.advertise(zaura_shell::NAME, zaura_shell::VERSION).await;
        s.host_received();
        s.guest_received();
        let gtk_shell = s.new_id();
        s.guest([
            wl_registry::req::bind(registry.id, name, gtk_shell1::NAME, 1, gtk_shell).unwrap(),
        ])
        .await;
        let host = s.host_received();
        let [ref sync] = host[..] else {
            panic!("the host got {host:?}, not one sync");
        };
        let callback = synced(sync);

        s.host([wl_callback::evt::done(callback, 0)]).await;
        let capabilities = gtk_shell1::Event::Capabilities { capabilities: 0 };
        assert_eq!(s.guest_received(), [(gtk_shell, capabilities.into())]);
        let surface = s.surface().await;
        assert_ne!(surface.host, callback, "reserved until its delete_id");

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, callback)])
            .await;
        assert_eq!(s.guest_received(), []);
        assert_eq!(s.surface().await.host, callback, "freed by its delete_id");
    }

    /// The `done` of a linux-dmabuf capability sync completes the global
    /// the sync was for: the `done` of a retired binding's sync does not
    /// complete its replacement's, whose guest global waits for its own.
    #[tokio::test]
    async fn internal_dmabuf_callback_completes_its_own_global() {
        let dmabuf = zwp_linux_dmabuf_v1::NAME;
        let mut ctx = Context::new_for_test(true, false, vec![]);
        // Stands in for a virtio-gpu render node, which the proxy
        // synthesizes feedback for and a test cannot have.
        ctx.synthetic_feedback_available_for_test = true;
        let mut s = Session::with_context(ctx);
        let registry = s.registry().await;
        let old = s.advertise(dmabuf, 4).await;
        let host = s.host_received();
        let [_, ref sync] = host[..] else {
            panic!("the host got {host:?}, not a bind and a sync");
        };
        let old_callback = synced(sync);
        let replacement = s.advertise(dmabuf, 4).await;
        assert_eq!(s.host_received(), []);
        assert_eq!(s.guest_received(), []);

        s.host([wl_registry::evt::global_remove(registry.host, old)])
            .await;
        let host = s.host_received();
        let [_, (_, AnyRequest::WlRegistry(wl_registry::Request::Bind { ref id, .. })), ref sync] =
            host[..]
        else {
            panic!("the host got {host:?}, not a destroy, a bind and a sync");
        };
        let (binding, callback) = (id.2, synced(sync));

        s.host([
            wl_callback::evt::done(old_callback, 0),
            wl_display::evt::delete_id(WL_DISPLAY_ID, old_callback),
        ])
        .await;
        assert_eq!(s.guest_received(), [], "the old global completes");

        let xrgb8888 = 0x3432_5258;
        s.host([
            zwp_linux_dmabuf_v1::evt::modifier(binding, xrgb8888, 0, 0),
            wl_callback::evt::done(callback, 0),
            wl_display::evt::delete_id(WL_DISPLAY_ID, callback),
        ])
        .await;
        let global = wl_registry::Event::Global {
            name: replacement,
            interface: dmabuf.to_string(),
            version: 4,
        };
        assert_eq!(s.guest_received(), [(registry.id, global.into())]);
    }

    /// A guest callback ends at its `done`, which the guest gets with its
    /// `delete_id`, but its host ID stays reserved until the host's own
    /// `delete_id`, which the guest does not get again.
    #[tokio::test]
    async fn guest_callback_reserves_host_id_until_real_delete_id() {
        let mut s = Session::new();
        // Binds the compositor, so that each later surface takes one host
        // ID.
        s.surface().await;
        let callback = s.new_id();
        let host = s
            .create(wl_display::req::sync(WL_DISPLAY_ID, callback))
            .await;

        s.host([wl_callback::evt::done(host, 7)]).await;
        let done: AnyEvent = wl_callback::Event::Done { callback_data: 7 }.into();
        let deleted = wl_display::Event::DeleteId { id: callback };
        assert_eq!(
            s.guest_received(),
            [(callback, done), (WL_DISPLAY_ID, deleted.into())]
        );
        let surface = s.surface().await;
        assert_ne!(surface.host, host, "reserved until the host's delete_id");

        s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, host)])
            .await;
        assert_eq!(
            s.guest_received(),
            [],
            "the guest already received its callback delete_id"
        );
        assert_eq!(s.surface().await.host, host, "freed by its delete_id");
    }

    /// A placement barrier's `done` completes it, reaches no peer, and
    /// keeps its host ID reserved until the host's `delete_id`.
    #[tokio::test]
    async fn placement_barrier_completes_and_reserves_callback_until_delete_id() {
        use crate::handler::window_placement::fixture::{Desk, Window};
        use crate::placement_config::PlacementConfig;
        use crate::protocols::xdg_shell::xdg_toplevel;
        let mut d = Desk::new(PlacementConfig::active_for_test(&[("<Alt>q", "top-left")])).await;
        let w = Window::new(&mut d).await;
        w.configured(&mut d, (100, 100), 5).await;
        d.focus().await;
        d.chord(3).await;
        let barrier = synced(d.s.host_received().last().expect("a placement barrier"));
        // The toplevel ends, so the barrier has no step left to send.
        d.s.guest([xdg_toplevel::req::destroy(w.toplevel)]).await;
        d.s.host_received();

        d.s.host([wl_callback::evt::done(barrier, 0)]).await;
        assert_eq!(d.s.guest_received(), []);
        assert_eq!(d.s.host_received(), []);
        let callback = d.s.new_id();
        let host_callback =
            d.s.create(wl_display::req::sync(WL_DISPLAY_ID, callback))
                .await;
        assert_ne!(host_callback, barrier, "reserved until its delete_id");

        d.s.host([wl_display::evt::delete_id(WL_DISPLAY_ID, barrier)])
            .await;
        assert_eq!(d.s.guest_received(), []);
        let callback = d.s.new_id();
        let host_callback =
            d.s.create(wl_display::req::sync(WL_DISPLAY_ID, callback))
                .await;
        assert_eq!(host_callback, barrier, "freed by its delete_id");
    }
}
