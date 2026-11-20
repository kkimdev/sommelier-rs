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

//! The lifetime of every guest `wl_buffer`, SHM or native: its destroy and
//! the host's release, and retiring a destroyed buffer once the host is
//! done with it.

use crate::handler::Handlers;
use crate::protocols;
use crate::state::{Context, HostId, SurfaceState};
use crate::wire::Action;
use std::collections::BTreeMap;

/// Queue the host `wl_buffer.destroy`; the caller retires the ID mapping.
pub(crate) fn queue_host_buffer_destroy(ctx: &mut Context, host_id: u32) {
    ctx.client_to_host_queue
        .push(protocols::wayland::wl_buffer::req::destroy(host_id));
}

fn clear_surface_buffer_references(ctx: &mut Context, guest_id: u32) {
    for surface in ctx.surfaces.values_mut() {
        surface.clear_buffer_references(guest_id);
    }
}

fn record_host_buffer_release(ctx: &mut Context, guest_id: u32) {
    if ctx.render_buffer_host_id(guest_id).is_some() && !ctx.mark_buffer_released(guest_id) {
        log::warn!(
            "Ignoring wl_buffer.release that no render use owes for guest {}",
            guest_id
        );
    }
}

fn retire_destroyed_buffer_mapping(ctx: &mut Context, guest_id: u32, host_id: HostId) {
    if !ctx.complete_queued_buffer_destroy(guest_id, host_id) {
        // Deep in buffer teardown: the session ends, and the loop drops this
        // message's effects whatever its handler returns.
        let _ = ctx.fail(format_args!(
            "Render buffer generation diverged while destroying guest {} host {}",
            guest_id, host_id.0
        ));
    }
}

/// Every current and pending surface→buffer edge, read in place.
///
/// Pending attaches are kept distinct because they have already reached the
/// host but do not begin a compositor-use interval until commit. Answering
/// both from one place prevents SHM and native collectors from drifting
/// into subtly different definitions of "referenced". Each answer scans the
/// surfaces, which allocates nothing: an attach, commit and release each
/// ask, every frame.
struct SurfaceBufferReferences<'a> {
    surfaces: &'a BTreeMap<u32, SurfaceState>,
}

impl<'a> SurfaceBufferReferences<'a> {
    fn of(ctx: &'a Context) -> Self {
        Self {
            surfaces: &ctx.surfaces,
        }
    }

    fn contains(&self, guest_id: u32) -> bool {
        self.has_current(guest_id) || self.has_pending(guest_id)
    }

    fn has_current(&self, guest_id: u32) -> bool {
        self.surfaces
            .values()
            .any(|surface| surface.current_buffer_id() == Some(guest_id))
    }

    fn has_pending(&self, guest_id: u32) -> bool {
        self.surfaces
            .values()
            .any(|surface| surface.pending.attachment.buffer_id() == Some(guest_id))
    }
}

fn can_retire_deferred_buffer(
    ctx: &Context,
    references: &SurfaceBufferReferences,
    guest_id: u32,
) -> bool {
    (!references.contains(guest_id)
        || (ctx.buffer_is_released(guest_id) && !references.has_pending(guest_id)))
        && !ctx.buffer_is_submitted(guest_id)
}

/// Retire every eligible guest-destroyed local-copy or native buffer.
///
/// SHM and native buffers differ only in whether local copy backing must be
/// retained. Surface references, submitted/released phases, and ordered
/// surface-destroy proofs have identical lifetime meaning and therefore must
/// not be evaluated by separate condition trees.
pub(crate) fn retire_eligible_buffers(ctx: &mut Context) {
    // Almost always the case, so the common frame scans nothing.
    if !ctx.has_guest_destroyed_buffers() {
        return;
    }
    let references = SurfaceBufferReferences::of(ctx);
    let candidates: Vec<(u32, HostId)> = ctx
        .render_buffer_lifecycles()
        .filter_map(|(host_id, lifecycle)| {
            if !lifecycle.is_guest_destroyed() {
                return None;
            }
            let guest_id = ctx.shadow_table.get_guest_id(host_id.0)?;
            can_retire_deferred_buffer(ctx, &references, guest_id).then_some((guest_id, host_id))
        })
        .collect();

    for (guest_id, host_id) in candidates {
        queue_host_buffer_destroy(ctx, host_id.0);
        // Keep the guest↔host mapping and empty registry generation reserved
        // until wl_display.delete_id acknowledges the host destructor.
        // Server-created generations are removed immediately because neither
        // side emits delete_id.
        retire_destroyed_buffer_mapping(ctx, guest_id, host_id);
    }
}

/// Apply the ordering proof provided by one successfully queued surface
/// destructor to the buffer that surface held.
///
/// The authoritative current-surface edges live in `Context::surfaces`. The
/// registry stores only whether an earlier detached use still lacks a host
/// release, so destroying a different current surface cannot erase that
/// unresolved use.
pub(crate) fn clear_buffer_uses_after_surface_destroy(
    ctx: &mut Context,
    destroyed_surface_buffer: Option<u32>,
) {
    let Some(guest_id) = destroyed_surface_buffer else {
        return;
    };
    let has_other_current = SurfaceBufferReferences::of(ctx).has_current(guest_id);
    if ctx.host_buffer_use(guest_id).is_some()
        && !ctx.finish_surface_destroy_use(guest_id, has_other_current)
    {
        // Deep in surface teardown: the session ends, and the loop drops this
        // message's effects whatever its handler returns.
        let _ = ctx.fail(format_args!(
            "Render buffer {guest_id} could not finish its destroyed-surface use"
        ));
    }
}

impl protocols::wayland::wl_buffer::WlBufferHandler for Handlers {
    fn on_destroy(ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        let host_id = ctx.shadow_table.get_host_id(guest_id);
        // wl_surface.attach is forwarded immediately, while the submitted
        // phase starts only when the later wl_surface.commit is handled. A
        // client is allowed to destroy the wl_buffer in that interval; the
        // host surface still owns the attached host buffer and needs its local
        // SHM backing through the pending commit.
        let references = SurfaceBufferReferences::of(ctx);
        let uncommitted_attach = references.has_pending(guest_id);
        let still_referenced = ctx.buffer_is_submitted(guest_id) || references.contains(guest_id);

        // Local-copy and native buffers have identical Wayland lifetime
        // semantics. Keep either backing alive while a committed use interval
        // or unresolved pending attach can still produce host traffic.
        if ctx.render_buffer_host_id(guest_id).is_some()
            && still_referenced
            && (!ctx.buffer_is_released(guest_id) || uncommitted_attach)
        {
            if !ctx.mark_buffer_guest_destroyed(guest_id) {
                return ctx.fail(format_args!(
                    "Render buffer {guest_id} could not enter guest-destroyed state"
                ));
            }
            ctx.guest_destroyed_in_use(guest_id);
            return Action::Drop;
        }

        if let Some(host_id) = host_id {
            queue_host_buffer_destroy(ctx, host_id);
            retire_destroyed_buffer_mapping(ctx, guest_id, HostId(host_id));
        }
        clear_surface_buffer_references(ctx, guest_id);
        Action::Drop
    }

    fn on_release(ctx: &mut Context) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let pending_attach = SurfaceBufferReferences::of(ctx).has_pending(guest_id);

        if ctx.host_buffer_is_guest_destroyed(host_id) {
            if pending_attach {
                // This release completes the previous use interval. Keep the
                // deferred object alive because the pending attach may still
                // begin another interval.
                record_host_buffer_release(ctx, guest_id);
                return Action::Drop;
            }
            record_host_buffer_release(ctx, guest_id);
            queue_host_buffer_destroy(ctx, host_id);
            retire_destroyed_buffer_mapping(ctx, guest_id, HostId(host_id));
            clear_surface_buffer_references(ctx, guest_id);
            return Action::Drop;
        }

        // Retain the release phase for both local-copy and native buffers so a later
        // guest destroy can retire the host proxy immediately even when a
        // surface still stores the buffer as its current content.
        record_host_buffer_release(ctx, guest_id);
        // A release is the compositor's lifetime signal. Once it arrives, the
        // backing storage may be reused or destroyed, even if the surface
        // still has the buffer as its current content. Keeping this marker in
        // a submitted phase would make a later guest destroy incorrectly
        // retain an already-idle buffer.
        Action::Forward
    }
}

#[cfg(test)]
mod tests {
    use crate::protocols::wayland::{wl_buffer, wl_display, wl_shm, wl_surface};
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::{Object, Session};
    use crate::state::Context;
    use crate::test_support::wire_peer::SERVER_ID_START;
    use crate::test_support::{shm_pool_file, BufferKind, BUFFER_KINDS};
    use crate::wire::WL_DISPLAY_ID;

    /// The first server ID, under which the host creates a buffer.
    const HOST_BUFFER: u32 = SERVER_ID_START;

    /// A session with GPU access whose guest has bound `wl_shm` and the
    /// host's linux-dmabuf, so that it can make buffers of either kind.
    struct Guest {
        s: Session,
        shm: u32,
        dmabuf: u32,
    }

    impl Guest {
        async fn new() -> Self {
            let mut s = Session::with_context(Context::new_for_test(true, false, vec![]));
            let shm = s.bind("wl_shm", wl_shm::VERSION).await;
            let dmabuf = s.bind_paired("zwp_linux_dmabuf_v1", 3).await.id;
            Self { s, shm, dmabuf }
        }

        /// A new 1x1 buffer of `kind`.
        async fn buffer(&mut self, kind: BufferKind) -> Object {
            let pixels = shm_pool_file(&[0; 4]);
            match kind {
                BufferKind::Shm => {
                    let pool = self.s.shm_pool(self.shm, pixels).await;
                    self.s.shm_buffer(pool, 1, 1).await
                }
                BufferKind::Native => {
                    let s = &mut self.s;
                    s.native_buffer(self.dmabuf, pixels.into(), 1, 1).await
                }
            }
        }

        /// A new 1x1 buffer that the host creates under [`HOST_BUFFER`].
        async fn created_buffer(&mut self) -> Object {
            let plane = shm_pool_file(&[0; 4]).into();
            let s = &mut self.s;
            s.created_buffer(self.dmabuf, plane, 1, 1, HOST_BUFFER)
                .await
        }

        /// The guest attaches `buffer`, or detaches with `None`, and
        /// commits `surface`, which reaches the host; returns what the host
        /// got.
        async fn commit(
            &mut self,
            surface: Object,
            buffer: Option<Object>,
        ) -> Vec<(u32, AnyRequest)> {
            self.s
                .guest([
                    wl_surface::req::attach(surface.id, buffer.map(|buffer| buffer.id), 0, 0),
                    wl_surface::req::commit(surface.id),
                ])
                .await;
            let received = self.s.host_received();
            assert!(
                matches!(
                    received.last(),
                    Some((host, AnyRequest::WlSurface(wl_surface::Request::Commit {})))
                        if *host == surface.host
                ),
                "the commit reached the host: {received:?}"
            );
            received
        }

        /// The host releases `buffer`, which the guest gets.
        async fn release(&mut self, buffer: Object) {
            self.s.host([wl_buffer::evt::release(buffer.host)]).await;
            assert_eq!(
                self.s.guest_received(),
                [(buffer.id, wl_buffer::Event::Release {}.into())]
            );
        }

        /// The host acknowledges the destructor of client-range `buffer`,
        /// whose `delete_id` the guest gets: the proxy kept its host ID
        /// until then.
        async fn delete_id(&mut self, buffer: Object) {
            self.s
                .host([wl_display::evt::delete_id(WL_DISPLAY_ID, buffer.host)])
                .await;
            assert_eq!(
                self.s.guest_received(),
                [(
                    WL_DISPLAY_ID,
                    wl_display::Event::DeleteId { id: buffer.id }.into()
                )]
            );
        }
    }

    /// The host's `wl_buffer.destroy` of `buffer`.
    fn host_destroy(buffer: Object) -> (u32, AnyRequest) {
        (buffer.host, wl_buffer::Request::Destroy {}.into())
    }

    /// A buffer the host still holds outlives the guest's destroy: the
    /// host's release then destroys the host buffer, once, and the guest
    /// gets the `delete_id` only when the host acknowledges it.
    #[tokio::test]
    async fn destroying_a_submitted_buffer_defers_its_host_destroy_until_release() {
        for (kind, shown) in BUFFER_KINDS
            .into_iter()
            .flat_map(|kind| [(kind, false), (kind, true)])
        {
            let case = format!("{kind:?}, shown on a surface: {shown}");
            let mut g = Guest::new().await;
            let surface = g.s.surface().await;
            let buffer = g.buffer(kind).await;
            g.commit(surface, Some(buffer)).await;
            if !shown {
                g.commit(surface, None).await;
            }

            g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
            assert_eq!(
                g.s.host_received(),
                [],
                "the host buffer stays alive until its release ({case})"
            );
            assert_eq!(g.s.guest_received(), [], "{case}");

            g.s.host([wl_buffer::evt::release(buffer.host)]).await;
            assert_eq!(
                g.s.host_received(),
                [host_destroy(buffer)],
                "release queues the deferred host destroy once ({case})"
            );
            assert_eq!(g.s.guest_received(), [], "{case}");
            if shown {
                // Reads state no peer can observe: what the surface
                // remembers of the content it shows.
                let surface = &g.s.ctx().surfaces[&surface.id];
                assert_eq!(surface.current_buffer_id(), None, "{case}");
                assert_eq!(
                    surface.current_buffer_dimensions(),
                    Some((1, 1)),
                    "destroying a released wl_buffer must preserve committed \
                     content metadata ({case})"
                );
            }
            g.delete_id(buffer).await;
        }
    }

    /// A release that came before the guest's destroy needs no second
    /// one, which cannot come: the destroy reaches the host at once, even
    /// while a surface still shows the buffer.
    #[tokio::test]
    async fn a_release_before_guest_destroy_is_recorded_and_destroy_is_immediate() {
        for (kind, shown) in BUFFER_KINDS
            .into_iter()
            .flat_map(|kind| [(kind, false), (kind, true)])
        {
            let case = format!("{kind:?}, shown on a surface: {shown}");
            let mut g = Guest::new().await;
            let surface = g.s.surface().await;
            let buffer = g.buffer(kind).await;
            g.commit(surface, Some(buffer)).await;
            g.release(buffer).await;
            if !shown {
                g.commit(surface, None).await;
            }

            g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
            assert_eq!(g.s.host_received(), [host_destroy(buffer)], "{case}");
            assert_eq!(g.s.guest_received(), [], "{case}");
            g.delete_id(buffer).await;
        }
    }

    /// A server-created buffer the host still holds also outlives the
    /// guest's destroy until the host releases it; neither side sends a
    /// `delete_id` for it, and its host ID is free once its destructor is
    /// sent.
    #[tokio::test]
    async fn destroying_submitted_server_created_buffer_retires_after_release() {
        let mut g = Guest::new().await;
        let surface = g.s.surface().await;
        let buffer = g.created_buffer().await;
        g.commit(surface, Some(buffer)).await;

        g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
        assert_eq!(
            g.s.host_received(),
            [],
            "the mapping must remain until a delayed host release can resolve"
        );
        g.s.host([wl_buffer::evt::release(buffer.host)]).await;
        assert_eq!(g.s.host_received(), [host_destroy(buffer)]);
        assert_eq!(g.s.guest_received(), [], "server IDs get no delete_id");

        let reused = g.created_buffer().await;
        assert_eq!(reused.id, buffer.id);
    }

    /// A released server-created buffer goes at the guest's destroy.
    #[tokio::test]
    async fn released_server_created_buffer_retires_on_later_guest_destroy() {
        let mut g = Guest::new().await;
        let surface = g.s.surface().await;
        let buffer = g.created_buffer().await;
        g.commit(surface, Some(buffer)).await;
        g.release(buffer).await;

        g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
        assert_eq!(g.s.host_received(), [host_destroy(buffer)]);
        assert_eq!(g.s.guest_received(), [], "server IDs get no delete_id");

        let reused = g.created_buffer().await;
        assert_eq!(reused.id, buffer.id);
    }

    /// A destroyed buffer attached again but not committed waits for that
    /// attach even once released: a commit could still show it. Replacing
    /// the attach retires it, after the attach that replaced it. SHM and
    /// native buffers make the same decision.
    #[tokio::test]
    async fn local_copy_and_native_buffers_share_one_retirement_oracle() {
        for kind in BUFFER_KINDS {
            let mut g = Guest::new().await;
            let surface = g.s.surface().await;
            let buffer = g.buffer(kind).await;
            g.commit(surface, Some(buffer)).await;
            g.s.guest([wl_surface::req::attach(surface.id, Some(buffer.id), 0, 0)])
                .await;
            g.s.host_received();
            g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
            g.s.host([wl_buffer::evt::release(buffer.host)]).await;
            assert_eq!(g.s.host_received(), [], "{kind:?}");
            assert_eq!(g.s.guest_received(), [], "{kind:?}");

            g.s.guest([wl_surface::req::attach(surface.id, None, 0, 0)])
                .await;
            let detach = wl_surface::Request::Attach {
                buffer: 0,
                x: 0,
                y: 0,
            };
            assert_eq!(
                g.s.host_received(),
                [(surface.host, detach.into()), host_destroy(buffer)],
                "local-copy and native buffers must make the same lifecycle decision ({kind:?})"
            );
            g.delete_id(buffer).await;
        }
    }

    /// A commit that replaces a destroyed buffer the host has not released
    /// leaves its host buffer alone; the host's release then retires it.
    #[tokio::test]
    async fn retired_collection_waits_for_host_release() {
        let mut g = Guest::new().await;
        let surface = g.s.surface().await;
        let buffer = g.buffer(BufferKind::Shm).await;
        let next = g.buffer(BufferKind::Shm).await;
        g.commit(surface, Some(buffer)).await;
        g.s.guest([wl_buffer::req::destroy(buffer.id)]).await;
        assert_eq!(g.s.host_received(), []);
        assert_eq!(
            g.s.guest_received(),
            [],
            "the guest object waits for the host to release the buffer"
        );

        let committed = g.commit(surface, Some(next)).await;
        assert!(
            !committed.contains(&host_destroy(buffer)),
            "the host still holds the buffer: {committed:?}"
        );
        g.s.host([wl_buffer::evt::release(buffer.host)]).await;
        assert_eq!(
            g.s.host_received(),
            [host_destroy(buffer)],
            "retiring a released buffer must queue its host destructor"
        );
        g.delete_id(buffer).await;
    }
}
