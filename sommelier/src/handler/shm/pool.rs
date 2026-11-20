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

//! The guest's `wl_shm` and `wl_shm_pool` objects, which the proxy
//! implements: creating, resizing and retiring pools, and validating each
//! buffer created from one before its host storage is made.

use super::formats::{
    format_bytes_per_pixel, guest_shm_format_available, record_host_shm_wl_format,
    supported_shm_format,
};
use super::storage::{
    create_dmabuf_buffer, create_shared_buffer, select_shm_backing, ShmBacking, ShmBufferRequest,
};
use super::{GuestShm, PoolData, ShmPool};
use crate::handler::Handlers;
use crate::protocols;
use crate::protocols::wayland::wl_shm;
use crate::protocols::Iface;
use crate::state::Singleton;
use crate::state::{Context, EventSender, GuestId, RequestSender};
use crate::wire::Action;
use log::{debug, error, warn};
use std::fs::File;
use std::os::unix::io::{AsRawFd, BorrowedFd};
use std::sync::Arc;

fn valid_pool_size(size: i32) -> Option<usize> {
    (size > 0)
        .then_some(size as usize)
        .filter(|&size| size <= isize::MAX as usize)
}

/// `wl_shm_pool.resize` may only grow the pool; the same size is a no-op.
fn valid_pool_resize(current_size: usize, new_size: i32) -> Option<usize> {
    let new_size = valid_pool_size(new_size)?;
    (new_size >= current_size).then_some(new_size)
}

fn valid_shm_stride(format: u32, width: i32, stride: i32) -> bool {
    let Some(width) = usize::try_from(width).ok() else {
        return false;
    };
    let Some(stride) = usize::try_from(stride).ok() else {
        return false;
    };
    let Some(bytes_per_pixel) = format_bytes_per_pixel(format) else {
        return false;
    };
    let minimum = width
        .checked_mul(bytes_per_pixel)
        .is_some_and(|minimum| stride >= minimum);
    minimum
        && (format != wl_shm::FORMAT_NV12 || (width.is_multiple_of(2) && stride.is_multiple_of(2)))
}

/// Whether `file` is a regular file of at least `size` bytes.
///
/// The bridge reads pools with `pread`, which needs a regular file: a memfd,
/// or a tmpfs or `shm_open` file. Checking the size here reports a short pool
/// when it is created or resized; a later truncation fails the read itself.
fn regular_file_has_size(file: &File, size: usize) -> bool {
    file.metadata().is_ok_and(|metadata| {
        metadata.file_type().is_file()
            && u64::try_from(size).is_ok_and(|size| metadata.len() >= size)
    })
}

fn valid_buffer_layout(
    pool_size: usize,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
) -> Option<i32> {
    if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
        return None;
    }
    if format == wl_shm::FORMAT_NV12 && (width % 2 != 0 || height % 2 != 0) {
        return None;
    }
    let offset = offset as usize;
    let stride = stride as usize;
    let height = height as usize;
    let bytes = if format == wl_shm::FORMAT_NV12 {
        let chroma_rows = height.checked_div(2)?;
        stride
            .checked_mul(height)?
            .checked_add(stride.checked_mul(chroma_rows)?)?
    } else {
        stride.checked_mul(height)?
    };
    let end = offset.checked_add(bytes)?;
    if end > pool_size {
        return None;
    }
    // A copied buffer becomes a host wl_shm pool, whose size is an i32.
    i32::try_from(bytes).ok()
}

impl protocols::wayland::wl_shm::WlShmHandler for Handlers {
    fn on_format(ctx: &mut Context, sender: EventSender, format: u32) -> Action {
        if ctx.globals.singletons().host(Singleton::Shm) == Some(sender.host) {
            record_host_shm_wl_format(ctx, format);
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_create_pool(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        fd: BorrowedFd<'_>,
        size: i32,
    ) -> Action {
        // A live wl_shm keeps its state from its bind (`register_guest_shm`)
        // to its end.
        let Some(shm) = ctx.shadow_table.data::<GuestShm>(sender.guest) else {
            return ctx.fail(format_args!("wl_shm {} has no state", sender.guest));
        };
        if shm.stale {
            debug!(
                "Ignoring wl_shm.create_pool from removed host global object {}",
                sender.guest
            );
            // The guest owns the new ID, so the pool exists, inert.
            ctx.shadow_table
                .create_local_with(id, Iface::WlShmPool, 1, PoolData::Inert);
            return Action::Drop;
        }
        let pool_id = id;

        let Some(pool_size) = valid_pool_size(size) else {
            warn!("Rejecting invalid SHM pool size {}", size);
            return ctx.reject(
                sender.guest,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm pool size",
            );
        };
        // The request closes `fd` once handled, so the pool owns a
        // duplicate, close-on-exec like every descriptor the proxy keeps.
        let file = match fd.try_clone_to_owned() {
            Ok(duplicate) => File::from(duplicate),
            Err(error) => {
                error!("Failed to dup FD for SHM pool: {}", error);
                return ctx.reject(
                    sender.guest,
                    wl_shm::ERROR_INVALID_FD,
                    "invalid wl_shm pool fd",
                );
            }
        };
        if !regular_file_has_size(&file, pool_size) {
            warn!(
                "Rejecting SHM pool fd {} that is not a regular file of at least {} bytes",
                fd.as_raw_fd(),
                pool_size
            );
            return ctx.reject(
                sender.guest,
                wl_shm::ERROR_INVALID_FD,
                "invalid wl_shm pool fd",
            );
        }
        let pool = ShmPool {
            file: Arc::new(file),
            size: pool_size,
            shm: sender.guest,
        };

        ctx.shadow_table
            .create_local_with(pool_id, Iface::WlShmPool, 1, PoolData::Live(pool));

        Action::Drop
    }
}

/// Make the guest `wl_shm` objects bound from host global `name`, which
/// retired, and their pools inert (see [`GuestShm::stale`]), whether or not
/// the proxy's own binding was `name`.
pub(crate) fn retire_guest_shms(ctx: &mut Context, name: u32) {
    let mut stale = std::collections::BTreeSet::new();
    for (id, shm) in ctx.shadow_table.data_of_mut::<GuestShm>() {
        if shm.global_name == name {
            shm.stale = true;
            stale.insert(id);
        }
    }
    for (_, pool) in ctx.shadow_table.data_of_mut::<PoolData>() {
        if matches!(pool, PoolData::Live(live) if stale.contains(&live.shm)) {
            *pool = PoolData::Inert;
        }
    }
}

impl protocols::wayland::wl_shm_pool::WlShmPoolHandler for Handlers {
    fn on_create_buffer(
        ctx: &mut Context,
        sender: RequestSender,
        id: GuestId,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Action {
        debug!("Creating buffer id={}", id);
        let pool_id = sender.guest;
        // A live pool keeps its state from `wl_shm.create_pool` to its end.
        let pool = match ctx.shadow_table.data::<PoolData>(pool_id) {
            Some(PoolData::Live(pool)) => pool,
            Some(PoolData::Inert) => {
                debug!(
                    "Ignoring wl_shm_pool.create_buffer from removed host global object {}",
                    pool_id
                );
                // The guest owns the new ID, so the buffer exists, inert:
                // attaching it detaches (`CompositorHandler::on_attach`).
                ctx.shadow_table.create_local(id, Iface::WlBuffer, 1);
                return Action::Drop;
            }
            None => return ctx.fail(format_args!("wl_shm_pool {pool_id} has no state")),
        };
        let pool_file = Arc::clone(&pool.file);
        let pool_size = pool.size;
        if !supported_shm_format(format) || !guest_shm_format_available(ctx, format) {
            warn!("Rejecting unsupported SHM buffer format {:#010x}", format);
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_FORMAT,
                "invalid wl_shm buffer format",
            );
        }
        if !valid_shm_stride(format, width, stride) {
            warn!(
                "Rejecting SHM buffer stride {} for format {:#010x} and width {}",
                stride, format, width
            );
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm buffer stride",
            );
        }

        let Some(buffer_size) =
            valid_buffer_layout(pool_size, offset, width, height, stride, format)
        else {
            warn!(
                "Rejecting invalid SHM buffer: pool={}, offset={}, width={}, height={}, stride={}, pool_size={}",
                pool_id, offset, width, height, stride, pool_size
            );
            return ctx.reject(
                pool_id,
                wl_shm::ERROR_INVALID_STRIDE,
                "invalid wl_shm buffer layout",
            );
        };

        let request = ShmBufferRequest {
            id,
            pool_id,
            pool_file,
            offset,
            width,
            height,
            stride,
            format,
            size: buffer_size,
        };
        match select_shm_backing(ctx, &request) {
            Ok(ShmBacking::Shared(shared_fd)) => create_shared_buffer(ctx, request, shared_fd),
            Ok(ShmBacking::Dmabuf {
                host_dmabuf_id,
                allocation,
            }) => create_dmabuf_buffer(ctx, request, host_dmabuf_id, allocation),
            Err(action) => action,
        }
    }

    fn on_destroy(_ctx: &mut Context, _sender: RequestSender) -> Action {
        // The generated local-only destructor path ends the synthetic pool,
        // whose entry drops its state, and queues its guest delete_id.
        Action::Drop
    }

    fn on_resize(ctx: &mut Context, sender: RequestSender, size: i32) -> Action {
        let pool_id = sender.guest;
        let pool = match ctx.shadow_table.data_mut::<PoolData>(pool_id) {
            Some(PoolData::Live(pool)) => pool,
            Some(PoolData::Inert) => {
                debug!(
                    "Ignoring wl_shm_pool.resize from removed host global object {}",
                    pool_id
                );
                return Action::Drop;
            }
            None => return ctx.fail(format_args!("wl_shm_pool {pool_id} has no state")),
        };
        let error = match valid_pool_resize(pool.size, size) {
            // A client that needs a smaller pool must create a new one;
            // shrinking could invalidate live wl_buffers that still
            // reference the old tail. libwayland reports it as invalid_fd.
            None => {
                warn!(
                    "Rejecting shrinking SHM pool resize from {} to {}",
                    pool.size, size
                );
                Some((wl_shm::ERROR_INVALID_FD, "shrinking pool invalid"))
            }
            Some(new_size) if !regular_file_has_size(&pool.file, new_size) => {
                warn!(
                    "Rejecting SHM pool resize to {}: the pool file is too short",
                    new_size
                );
                Some((wl_shm::ERROR_INVALID_FD, "invalid wl_shm pool fd"))
            }
            Some(new_size) => {
                debug!(
                    "Resizing pool id={} from {} to {}",
                    pool_id, pool.size, new_size
                );
                pool.size = new_size;
                None
            }
        };
        match error {
            Some((code, message)) => ctx.reject(pool_id, code, message),
            None => Action::Drop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::formats::register_guest_shm;
    use super::{
        record_host_shm_wl_format, regular_file_has_size, valid_buffer_layout, valid_pool_resize,
        valid_pool_size, valid_shm_stride, GuestShm, PoolData,
    };
    use crate::handler::test_support::{bind_shm, session_with_shm, shm_global};
    use crate::protocols::wayland::{wl_display, wl_registry, wl_shm, wl_shm_pool};
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::{Object, Session};
    use crate::state::{Context, GuestId};
    use crate::test_support::{decoded, raw_dispatch, shm_pool_file};
    use crate::wire::{Outgoing, WireMessage, WL_DISPLAY_ID};
    use std::fs::File;

    /// The size the proxy records for live pool `pool`, which no peer sees.
    fn pool_size(s: &mut Session, pool: u32) -> usize {
        let pool = s
            .ctx()
            .shadow_table
            .data::<PoolData>(GuestId::for_test(pool));
        match pool {
            Some(PoolData::Live(pool)) => pool.size,
            other => panic!("pool {other:?} is not live"),
        }
    }

    #[test]
    fn rejects_non_positive_pool_sizes() {
        assert_eq!(valid_pool_size(0), None);
        assert_eq!(valid_pool_size(-1), None);
        assert_eq!(valid_pool_size(4096), Some(4096));
    }

    #[tokio::test]
    async fn invalid_shm_requests_are_protocol_errors() {
        let (mut s, shm) = session_with_shm().await;
        let pool = s.new_id();
        s.guest_fatal([wl_shm::req::create_pool(
            shm,
            pool,
            shm_pool_file(&[]).into(),
            0,
        )])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (shm, wl_shm::ERROR_INVALID_STRIDE)),
            "invalid pool size must use wl_shm.invalid_stride"
        );

        let (mut s, shm) = session_with_shm().await;
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
        let buffer = s.new_id();
        s.guest_fatal([wl_shm_pool::req::create_buffer(
            pool,
            buffer,
            0,
            1,
            1,
            4,
            0xffff_ffff,
        )])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (pool, wl_shm::ERROR_INVALID_FORMAT)),
            "unsupported format must use wl_shm.invalid_format"
        );
    }

    #[tokio::test]
    async fn create_pool_rejects_a_pipe_or_a_short_file_as_invalid_fd() {
        let (pipe, _writer) = crate::test_support::pipe();
        for fd in [pipe, shm_pool_file(&[0; 4095]).into()] {
            let (mut s, shm) = session_with_shm().await;
            let pool = s.new_id();
            s.guest_fatal([wl_shm::req::create_pool(shm, pool, fd, 4096)])
                .await;
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (shm, wl_shm::ERROR_INVALID_FD)),
                "a pool must be a regular file of at least its size"
            );
            // No peer sees the proxy's pool table; the session has ended.
            assert!(s.ctx().shadow_table.data_of::<PoolData>().next().is_none());
        }
    }

    #[test]
    fn shm_pool_resize_only_allows_growth() {
        assert_eq!(valid_pool_resize(4096, 8192), Some(8192));
        assert_eq!(valid_pool_resize(4096, 4096), Some(4096));
        assert_eq!(valid_pool_resize(4096, 2048), None);
        assert_eq!(valid_pool_resize(4096, 0), None);
        assert_eq!(valid_pool_resize(4096, -1), None);
    }

    /// A 4096-byte pool may keep its size, as libwayland accepts, or grow
    /// within its file; shrinking it or growing it past its file's end is
    /// `wl_shm.invalid_fd`, as in libwayland, and keeps its size.
    #[tokio::test]
    async fn a_pool_resize_must_keep_or_grow_its_size_within_its_file() {
        for (file_size, size, valid) in [
            (4096, 4096, true),
            (8192, 8192, true),
            (4096, 2048, false),
            (4096, 8192, false),
        ] {
            let (mut s, shm) = session_with_shm().await;
            let file = shm_pool_file(&[0; 4096]);
            let pool = s.shm_pool(shm, file.try_clone().unwrap()).await;
            file.set_len(file_size).unwrap();
            let resize = wl_shm_pool::req::resize(pool, size);
            if valid {
                s.guest([resize]).await;
                assert_eq!(s.guest_received(), [], "resize to {size}");
                // No peer sees the size the proxy records for the pool.
                assert_eq!(pool_size(&mut s, pool), size as usize);
            } else {
                s.guest_fatal([resize]).await;
                assert_eq!(
                    s.guest_received_and_error(),
                    (vec![], (pool, wl_shm::ERROR_INVALID_FD)),
                    "resize to {size}"
                );
                // No peer sees the size the proxy records for the pool.
                assert_eq!(pool_size(&mut s, pool), 4096);
            }
            assert_eq!(s.host_received(), []);
        }
    }

    /// Every live pool that is not stale has state, so a request of one
    /// without any is the proxy's own divergence: the session ends with an
    /// `implementation` error, rather than blaming the guest or dropping
    /// the request.
    #[tokio::test]
    async fn a_request_of_a_pool_without_state_is_an_internal_failure() {
        for create_buffer in [false, true] {
            let (mut s, shm) = session_with_shm().await;
            let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
            let request = if create_buffer {
                let buffer = s.new_id();
                wl_shm_pool::req::create_buffer(pool, buffer, 0, 1, 1, 4, wl_shm::FORMAT_ARGB8888)
            } else {
                wl_shm_pool::req::resize(pool, 4096)
            };
            // Injects a fault no peer can cause: the proxy loses the pool's
            // state.
            s.ctx()
                .shadow_table
                .drop_data_for_test(GuestId::for_test(pool));
            s.guest_fatal([request]).await;
            assert_eq!(
                s.guest_received_and_error(),
                (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)),
                "create_buffer: {create_buffer}"
            );
            assert_eq!(s.host_received(), []);
        }
    }

    #[test]
    fn validates_buffer_layout_against_pool_bounds() {
        assert_eq!(valid_buffer_layout(4096, 0, 32, 32, 128, 0), Some(4096));
        assert_eq!(valid_buffer_layout(4096, -1, 32, 32, 128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 0, 0, 32, 128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 0, 32, 32, -128, 0), None);
        assert_eq!(valid_buffer_layout(4096, 1, 32, 32, 128, 0), None);
        // A size that overflows is no layout.
        assert_eq!(
            valid_buffer_layout(usize::MAX, 0, i32::MAX, i32::MAX, i32::MAX, 0),
            None
        );
        // NV12 has a half-height UV plane after its Y plane, and an even
        // height.
        let nv12 = wl_shm::FORMAT_NV12;
        assert_eq!(valid_buffer_layout(24, 0, 4, 4, 4, nv12), Some(24));
        assert_eq!(valid_buffer_layout(23, 0, 4, 4, 4, nv12), None);
        assert_eq!(valid_buffer_layout(24, 0, 4, 3, 4, nv12), None);
    }

    #[test]
    fn validates_shm_format_stride() {
        // A stride must cover the guest format's row, or the copy would
        // truncate every row.
        assert!(valid_shm_stride(0, 10, 40));
        assert!(!valid_shm_stride(0, 10, 39));
        assert!(valid_shm_stride(0x3631_4752, 10, 20));
        assert!(!valid_shm_stride(0x3631_4752, 10, 19));
        assert!(valid_shm_stride(wl_shm::FORMAT_NV12, 10, 10));
        assert!(!valid_shm_stride(wl_shm::FORMAT_NV12, 11, 11));
        assert!(!valid_shm_stride(0xdead_beef, 10, 40));
    }

    /// The host advertises `wl_shm` as global `name` on host registry
    /// `registry`.
    fn shm_global_event(registry: u32, name: u32) -> Outgoing<u32> {
        let global = wl_registry::evt::global(registry, name, wl_shm::NAME, wl_shm::VERSION);
        global.unwrap()
    }

    /// The host removes global `name` from host registry `registry`.
    fn removal(registry: u32, name: u32) -> Outgoing<u32> {
        wl_registry::evt::global_remove(registry, name)
    }

    /// The host object of the `wl_shm` the proxy bound, among what the
    /// host got.
    fn bound_shm(received: &[(u32, AnyRequest)]) -> u32 {
        received
            .iter()
            .find_map(|(_, request)| match request {
                AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })
                    if id.0 == wl_shm::NAME =>
                {
                    Some(id.2)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the proxy bound no wl_shm: {received:?}"))
    }

    /// A second guest `wl_registry`.
    async fn second_registry(s: &mut Session) -> Object {
        let id = s.new_id();
        let host = s
            .create(wl_display::req::get_registry(WL_DISPLAY_ID, id))
            .await;
        Object { id, host }
    }

    /// A new 4096-byte pool of `shm`, inert or not.
    async fn pool_of(s: &mut Session, shm: u32) -> u32 {
        let pool = s.new_id();
        let file = shm_pool_file(&[0; 4096]);
        s.guest([wl_shm::req::create_pool(shm, pool, file.into(), 4096)])
            .await;
        assert_eq!(s.host_received(), [], "the pool is the proxy's own");
        pool
    }

    /// The guest creates a 1x1 buffer from `pool`; returns it and whether
    /// the host got storage for it: an inert pool creates an inert buffer.
    async fn buffer_of(s: &mut Session, pool: u32) -> (u32, bool) {
        let buffer = s.new_id();
        s.guest([wl_shm_pool::req::create_buffer(
            pool,
            buffer,
            0,
            1,
            1,
            4,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;
        assert_eq!(s.guest_received(), []);
        let stored = s.host_received().iter().any(|(_, request)| {
            matches!(
                request,
                AnyRequest::WlShmPool(wl_shm_pool::Request::CreateBuffer { .. })
            )
        });
        (buffer, stored)
    }

    /// The guest destroys `object` with `destroy`, which the proxy alone
    /// had: the guest gets its `delete_id`.
    async fn destroy_local(s: &mut Session, object: u32, destroy: Outgoing<u32>) {
        s.guest([destroy]).await;
        assert_eq!(s.host_received(), []);
        assert_eq!(
            s.guest_received(),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: object }.into()
            )]
        );
    }

    /// Kept a unit test: the guest's `wl_shm` is v1, which has no
    /// destructor, so only a direct dispatch reaches the v2 release.
    #[test]
    fn synthetic_shm_release_clears_cache_before_guest_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = GuestId::for_test(20);
        let optional_format = 0x3631_4752;
        record_host_shm_wl_format(&mut ctx, optional_format);
        register_guest_shm(&mut ctx, guest_shm, 2, 10);
        ctx.clear_queues_for_test();
        let shm = ctx.shadow_table.data_mut::<GuestShm>(guest_shm);
        shm.expect("a bound wl_shm keeps its state").stale = true;

        let mut release = WireMessage::new(20, crate::protocols::wayland::wl_shm::REQ_RELEASE, &[]);
        assert_eq!(
            raw_dispatch(crate::test_support::dispatch_request(
                &mut ctx,
                &mut release
            )),
            Ok(None)
        );
        assert!(ctx.shadow_table.data::<GuestShm>(guest_shm).is_none());
        assert!(ctx.shadow_table.get_interface(guest_shm).is_none());
        assert_eq!(
            decoded(ctx.guest_queue_for_test(), wl_display::Event::from_wire),
            [(WL_DISPLAY_ID, wl_display::Event::DeleteId { id: 20 })],
            "local wl_shm.release must emit one guest delete_id"
        );

        // Reusing the same guest ID must behave like a fresh synthetic object.
        register_guest_shm(&mut ctx, guest_shm, 2, 10);
        ctx.clear_queues_for_test();
        record_host_shm_wl_format(&mut ctx, optional_format);
        assert!(
            !ctx.sends_anything(),
            "a capability already sent by the replacement object must not be duplicated"
        );
    }

    #[test]
    fn pool_backing_must_be_a_long_enough_regular_file() {
        let file = shm_pool_file(&[]);
        assert!(regular_file_has_size(&file, 0));
        assert!(!regular_file_has_size(&file, 4096));
        file.set_len(4096).unwrap();
        assert!(regular_file_has_size(&file, 4096));

        // `pread` cannot read a pool from a pipe or a device.
        let (pipe_read, _pipe_write) = crate::test_support::pipe();
        assert!(!regular_file_has_size(&File::from(pipe_read), 0));
    }

    /// A guest `wl_shm` whose host global is gone still creates the pools
    /// and buffers the guest asks for, since the guest owns their IDs, but
    /// inert: nothing reaches the host, and each ends when the guest
    /// destroys it.
    #[tokio::test]
    async fn removed_host_shm_creates_only_inert_pools_and_buffers() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let (name, _) = shm_global(&mut s).await;
        let shm = bind_shm(&mut s, registry.id, name).await;
        s.host([removal(registry.host, name)]).await;
        s.guest_received();
        s.host_received();

        let pool = pool_of(&mut s, shm).await;
        let (buffer, stored) = buffer_of(&mut s, pool).await;
        assert!(
            !stored,
            "a removed host wl_shm binding must not store a buffer"
        );
        destroy_local(
            &mut s,
            buffer,
            crate::protocols::wayland::wl_buffer::req::destroy(buffer),
        )
        .await;
        destroy_local(&mut s, pool, wl_shm_pool::req::destroy(pool)).await;
    }

    /// The host advertises `wl_shm` anew (11) and then removes the name
    /// the proxy bound (10): only the guest objects bound from 10, and
    /// their pools, go inert, while the ones bound from 11 keep working on
    /// the promoted binding.
    #[tokio::test]
    async fn only_objects_bound_from_a_retired_shm_global_go_inert() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let other = second_registry(&mut s).await;
        s.host([
            shm_global_event(registry.host, 10),
            shm_global_event(other.host, 10),
            shm_global_event(registry.host, 11),
        ])
        .await;
        bound_shm(&s.host_received());
        s.guest_received();
        let old_shm = bind_shm(&mut s, registry.id, 10).await;
        let old_pool = pool_of(&mut s, old_shm).await;
        let shm = bind_shm(&mut s, registry.id, 11).await;
        let pool = pool_of(&mut s, shm).await;

        s.host([removal(registry.host, 10), removal(other.host, 10)])
            .await;
        s.guest_received();
        let promoted = bound_shm(&s.host_received());

        s.host([wl_shm::evt::format(promoted, wl_shm::FORMAT_RGB565)])
            .await;
        assert_eq!(
            s.guest_received(),
            [(
                shm,
                wl_shm::Event::Format {
                    format: wl_shm::FORMAT_RGB565
                }
                .into()
            )],
            "the promoted binding's formats reach only the live object"
        );
        assert!(
            !buffer_of(&mut s, old_pool).await.1,
            "the old pool is inert"
        );
        assert!(
            buffer_of(&mut s, pool).await.1,
            "the buffer has host storage through the promoted binding"
        );
        let new_pool = pool_of(&mut s, shm).await;
        assert!(buffer_of(&mut s, new_pool).await.1);
    }

    /// A guest `wl_shm` goes inert when the name it was bound from retires,
    /// even when the proxy bound another name.
    #[tokio::test]
    async fn a_guest_shm_bound_from_an_unbound_name_goes_inert_when_it_retires() {
        let mut s = Session::new();
        let registry = s.registry().await;
        s.host([
            shm_global_event(registry.host, 10),
            shm_global_event(registry.host, 11),
        ])
        .await;
        let received = s.host_received();
        let [(_, AnyRequest::WlRegistry(wl_registry::Request::Bind { name: 10, .. }))] =
            received[..]
        else {
            panic!("the proxy did not bind wl_shm 10: {received:?}");
        };
        s.guest_received();
        let shm = bind_shm(&mut s, registry.id, 11).await;

        s.host([removal(registry.host, 11)]).await;
        s.guest_received();
        assert_eq!(s.host_received(), [], "the proxy keeps its binding of 10");
        let pool = pool_of(&mut s, shm).await;
        assert!(!buffer_of(&mut s, pool).await.1, "the pool is inert");
    }

    /// A pool whose host `wl_shm` is gone ignores every request but its
    /// destructor, even once another `wl_shm` replaces it: its resize past
    /// the end of its file, which would be `invalid_fd`, changes nothing,
    /// and its buffers get no host storage.
    #[tokio::test]
    async fn removed_host_shm_ignores_requests_from_existing_pool_until_destroy() {
        let mut s = Session::new();
        let registry = s.registry().await;
        let (name, _) = shm_global(&mut s).await;
        let shm = bind_shm(&mut s, registry.id, name).await;
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
        s.host([removal(registry.host, name)]).await;
        s.guest_received();
        s.host_received();
        // A replacement wl_shm, which the proxy binds.
        s.host([shm_global_event(registry.host, name + 1)]).await;
        bound_shm(&s.host_received());
        s.guest_received();

        s.guest([wl_shm_pool::req::resize(pool, 8192)]).await;
        assert_eq!(s.guest_received(), [], "a stale pool must not resize");
        assert_eq!(s.host_received(), []);
        let (buffer, stored) = buffer_of(&mut s, pool).await;
        assert!(
            !stored,
            "a stale pool must not allocate a host buffer through replacement SHM"
        );
        destroy_local(
            &mut s,
            buffer,
            crate::protocols::wayland::wl_buffer::req::destroy(buffer),
        )
        .await;
        destroy_local(&mut s, pool, wl_shm_pool::req::destroy(pool)).await;
    }
}
