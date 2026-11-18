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

use super::ShmHandler;
use crate::protocols;
use crate::state::{Context, HostId};
use crate::wire::Action;
use std::collections::BTreeSet;

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

/// Canonical snapshot of every current and pending surface→buffer edge.
///
/// Pending attaches are kept distinct because they have already reached the
/// host but do not begin a compositor-use interval until commit. Computing the
/// two sets together prevents SHM and native collectors from drifting into
/// subtly different definitions of "referenced".
struct SurfaceBufferReferences {
    current: BTreeSet<u32>,
    pending: BTreeSet<u32>,
}

impl SurfaceBufferReferences {
    fn collect(ctx: &Context) -> Self {
        let mut current = BTreeSet::new();
        let mut pending = BTreeSet::new();
        for surface in ctx.surfaces.values() {
            current.extend(surface.current_buffer_id());
            pending.extend(surface.pending.attachment.buffer_id());
        }
        Self { current, pending }
    }

    fn contains(&self, guest_id: u32) -> bool {
        self.current.contains(&guest_id) || self.pending.contains(&guest_id)
    }

    fn has_current(&self, guest_id: u32) -> bool {
        self.current.contains(&guest_id)
    }

    fn has_pending(&self, guest_id: u32) -> bool {
        self.pending.contains(&guest_id)
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
    let references = SurfaceBufferReferences::collect(ctx);
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
    let references = SurfaceBufferReferences::collect(ctx);
    if ctx.host_buffer_use(guest_id).is_some()
        && !ctx.finish_surface_destroy_use(guest_id, references.has_current(guest_id))
    {
        // Deep in surface teardown: the session ends, and the loop drops this
        // message's effects whatever its handler returns.
        let _ = ctx.fail(format_args!(
            "Render buffer {guest_id} could not finish its destroyed-surface use"
        ));
    }
}

impl protocols::wayland::wl_buffer::WlBufferHandler for ShmHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let guest_id = ctx.last_sender_id;
        let host_id = ctx.shadow_table.get_host_id(guest_id);
        // wl_surface.attach is forwarded immediately, while the submitted
        // phase starts only when the later wl_surface.commit is handled. A
        // client is allowed to destroy the wl_buffer in that interval; the
        // host surface still owns the attached host buffer and needs its local
        // SHM backing through the pending commit.
        let references = SurfaceBufferReferences::collect(ctx);
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

    fn on_release(&mut self, ctx: &mut Context) -> Action {
        let host_id = ctx.last_sender_id;
        let Some(guest_id) = ctx.shadow_table.get_guest_id(host_id) else {
            return Action::Drop;
        };
        let pending_attach = SurfaceBufferReferences::collect(ctx).has_pending(guest_id);

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
    use super::retire_eligible_buffers;
    use super::ShmHandler;
    use crate::handler::test_support::{
        buffer_host_destroy_is_queued, buffer_is_guest_destroyed, mark_test_buffer_guest_destroyed,
    };
    use crate::protocols::wayland::wl_buffer;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::state::{Context, HostId};
    use crate::test_support::{queued_headers, register_test_buffer, BufferKind, BUFFER_KINDS};
    use crate::wire::Action;

    fn register_test_server_native(ctx: &mut Context, host_id: u32) -> u32 {
        let guest_id = ctx.shadow_table.adopt_host_created(host_id, "wl_buffer", 1);
        assert!(ctx.register_native_buffer(host_id, (1, 1), Vec::new()));
        guest_id
    }

    #[test]
    fn destroying_a_submitted_buffer_defers_its_host_destroy_until_release() {
        let (guest_buffer, host_buffer, surface) = (20, 30, 40);
        for (kind, shown) in BUFFER_KINDS
            .into_iter()
            .flat_map(|kind| [(kind, false), (kind, true)])
        {
            let case = format!("{kind:?}, shown on a surface: {shown}");
            let mut ctx = Context::new_for_test(false, false, vec![]);
            register_test_buffer(&mut ctx, kind, guest_buffer, host_buffer);
            if shown {
                ctx.surface_mut(surface)
                    .set_current_buffer_for_test(Some(guest_buffer), Some((1, 1)));
            }
            assert!(ctx.mark_buffer_submitted(guest_buffer));

            ctx.last_sender_id = guest_buffer;
            assert_eq!(
                WlBufferHandler::on_destroy(&mut ShmHandler, &mut ctx),
                Action::Drop
            );
            assert!(buffer_is_guest_destroyed(&ctx, guest_buffer), "{case}");
            assert!(ctx.buffer_is_submitted(guest_buffer), "{case}");
            assert_eq!(
                ctx.shadow_table.get_host_id(guest_buffer),
                Some(host_buffer),
                "the host mapping stays reserved until release ({case})"
            );
            assert!(
                ctx.shadow_table.is_pending_destroy_guest(guest_buffer),
                "{case}"
            );
            assert_eq!(
                ctx.client_to_host_queue.len(),
                0,
                "the host buffer stays alive until its release ({case})"
            );

            ctx.last_sender_id = host_buffer;
            assert_eq!(
                WlBufferHandler::on_release(&mut ShmHandler, &mut ctx),
                Action::Drop
            );
            assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer), "{case}");
            assert!(!ctx.buffer_is_submitted(guest_buffer), "{case}");
            assert_eq!(
                ctx.shadow_table.get_host_id(guest_buffer),
                Some(host_buffer),
                "{case}"
            );
            assert!(
                ctx.shadow_table.is_pending_destroy_guest(guest_buffer),
                "{case}"
            );
            assert_eq!(
                queued_headers(&ctx.client_to_host_queue),
                [(host_buffer, wl_buffer::REQ_DESTROY)],
                "release queues the deferred host destroy once ({case})"
            );
            if shown {
                let surface = &ctx.surfaces[&surface];
                assert_eq!(surface.current_buffer_id(), None, "{case}");
                assert_eq!(
                    surface.current_buffer_dimensions(),
                    Some((1, 1)),
                    "destroying a released wl_buffer must preserve committed \
                     content metadata ({case})"
                );
            }
        }
    }

    #[test]
    fn a_release_before_guest_destroy_is_recorded_and_destroy_is_immediate() {
        let (guest_buffer, host_buffer, surface) = (20, 30, 40);
        for (kind, shown) in BUFFER_KINDS
            .into_iter()
            .flat_map(|kind| [(kind, false), (kind, true)])
        {
            let case = format!("{kind:?}, shown on a surface: {shown}");
            let mut ctx = Context::new_for_test(false, false, vec![]);
            register_test_buffer(&mut ctx, kind, guest_buffer, host_buffer);
            assert!(ctx.mark_buffer_submitted(guest_buffer));

            ctx.last_sender_id = host_buffer;
            assert_eq!(
                WlBufferHandler::on_release(&mut ShmHandler, &mut ctx),
                Action::Forward
            );
            assert!(ctx.buffer_is_released(guest_buffer), "{case}");

            // The surface may still show the released buffer. Destroying the
            // guest object uses the recorded release instead of waiting for
            // a second one that cannot arrive.
            if shown {
                ctx.surface_mut(surface)
                    .set_current_buffer_for_test(Some(guest_buffer), None);
            }
            ctx.last_sender_id = guest_buffer;
            assert_eq!(
                WlBufferHandler::on_destroy(&mut ShmHandler, &mut ctx),
                Action::Drop
            );
            assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer), "{case}");
            assert!(ctx.host_buffer_use(guest_buffer).is_none(), "{case}");
            assert_eq!(
                queued_headers(&ctx.client_to_host_queue),
                [(host_buffer, wl_buffer::REQ_DESTROY)],
                "{case}"
            );
            assert_eq!(
                ctx.shadow_table.get_host_id(guest_buffer),
                Some(host_buffer),
                "{case}"
            );
            assert!(
                ctx.shadow_table.is_pending_destroy_guest(guest_buffer),
                "{case}"
            );
        }
    }

    #[test]
    fn destroying_idle_server_created_buffer_ends_without_delete_id() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());

        let reused_guest = register_test_server_native(&mut ctx, host_buffer);
        assert_eq!(
            reused_guest, guest_buffer,
            "a destroyed server-created buffer's guest ID is reused first"
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(reused_guest),
            Some(host_buffer),
            "a later ordered host event may reuse the server-owned buffer ID"
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "server-created guest IDs never receive wl_display.delete_id"
        );
    }

    #[test]
    fn destroying_submitted_server_created_buffer_retires_after_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer),
            "the mapping must remain until a delayed host release can resolve"
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert!(buffer_is_guest_destroyed(&ctx, guest_buffer));

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert_eq!(ctx.shadow_table.get_guest_id(host_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn released_server_created_buffer_retires_on_later_guest_destroy() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_buffer = 0xff00_0030;
        let guest_buffer = register_test_server_native(&mut ctx, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        let mut handler = ShmHandler;

        ctx.last_sender_id = host_buffer;
        assert_eq!(
            WlBufferHandler::on_release(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(ctx.buffer_is_released(guest_buffer));

        ctx.last_sender_id = guest_buffer;
        assert_eq!(
            WlBufferHandler::on_destroy(&mut handler, &mut ctx),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(guest_buffer), None);
        assert!(!ctx.shadow_table.is_retired_host(host_buffer));
        assert!(!ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.shadow_table.is_host_id_available(host_buffer));
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn local_copy_and_native_buffers_share_one_retirement_oracle() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let local_buffer = 20;
        let local_host = 30;
        let native_buffer = 21;
        let native_host = 31;

        register_test_buffer(&mut ctx, BufferKind::Shm, local_buffer, local_host);
        register_test_buffer(&mut ctx, BufferKind::Native, native_buffer, native_host);
        for guest_id in [local_buffer, native_buffer] {
            mark_test_buffer_guest_destroyed(&mut ctx, guest_id);
            assert!(ctx.mark_buffer_submitted(guest_id));
            assert!(ctx.mark_buffer_released(guest_id));
        }
        ctx.surface_mut(40).pending.attachment =
            crate::state::SurfaceAttachment::Attach(local_buffer);
        ctx.surface_mut(41).pending.attachment =
            crate::state::SurfaceAttachment::Attach(native_buffer);

        retire_eligible_buffers(&mut ctx);
        assert!(buffer_is_guest_destroyed(&ctx, local_buffer));
        assert!(buffer_is_guest_destroyed(&ctx, native_buffer));

        ctx.surfaces.get_mut(&40).unwrap().pending.attachment =
            crate::state::SurfaceAttachment::Detach;
        ctx.surfaces.get_mut(&41).unwrap().pending.attachment =
            crate::state::SurfaceAttachment::Detach;
        retire_eligible_buffers(&mut ctx);

        assert!(buffer_host_destroy_is_queued(&ctx, local_buffer));
        assert!(buffer_host_destroy_is_queued(&ctx, native_buffer));
        assert!(ctx.host_buffer_use(local_buffer).is_none());
        assert!(ctx.host_buffer_use(native_buffer).is_none());
        let mut destroyed_hosts = ctx
            .client_to_host_queue
            .iter()
            .map(|(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap()))
            .collect::<Vec<_>>();
        destroyed_hosts.sort_unstable();
        assert_eq!(
            destroyed_hosts,
            vec![local_host, native_host],
            "local-copy and native buffers must make the same lifecycle decision"
        );
    }

    #[test]
    fn retired_collection_waits_for_host_release() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_buffer = 20;
        let host_buffer = 30;
        register_test_buffer(&mut ctx, BufferKind::Shm, guest_buffer, host_buffer);
        ctx.mark_buffer_submitted(guest_buffer);
        mark_test_buffer_guest_destroyed(&mut ctx, guest_buffer);
        assert!(
            ctx.shadow_table.is_pending_destroy_guest(guest_buffer),
            "the guest object waits for the host to release the buffer"
        );

        retire_eligible_buffers(&mut ctx);
        assert!(buffer_is_guest_destroyed(&ctx, guest_buffer));

        assert!(ctx.mark_buffer_released(guest_buffer));
        retire_eligible_buffers(&mut ctx);
        assert!(buffer_host_destroy_is_queued(&ctx, guest_buffer));
        assert_eq!(
            ctx.shadow_table.get_host_id(guest_buffer),
            Some(host_buffer),
            "the host mapping remains reserved until wl_display.delete_id"
        );
        assert!(ctx.shadow_table.is_pending_destroy_guest(guest_buffer));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .any(
                    |(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap())
                        == host_buffer
                ),
            "retiring a released buffer must queue its host destructor"
        );
    }
}
