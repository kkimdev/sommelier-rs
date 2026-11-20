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
use crate::handler::Handlers;
use crate::protocols;
use crate::protocols::wayland::wl_shm;
use crate::protocols::Iface;
use crate::state::Singleton;
use crate::state::{Context, ShmPool};
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
    fn on_format(ctx: &mut Context, format: u32) -> Action {
        if ctx.singletons.host(Singleton::Shm) == Some(ctx.last_sender_id) {
            record_host_shm_wl_format(ctx, format);
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_create_pool(ctx: &mut Context, id: u32, fd: BorrowedFd<'_>, size: i32) -> Action {
        if ctx.stale_shm_guest_objects.contains(&ctx.last_sender_id) {
            debug!(
                "Ignoring wl_shm.create_pool from removed host global object {}",
                ctx.last_sender_id
            );
            // The guest owns the new ID, so the pool exists, inert.
            ctx.shadow_table.create_local(id, Iface::WlShmPool, 1);
            ctx.stale_shm_pools.insert(id);
            return Action::Drop;
        }
        let pool_id = id;

        let Some(pool_size) = valid_pool_size(size) else {
            warn!("Rejecting invalid SHM pool size {}", size);
            return ctx.reject(
                ctx.last_sender_id,
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
                    ctx.last_sender_id,
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
                ctx.last_sender_id,
                wl_shm::ERROR_INVALID_FD,
                "invalid wl_shm pool fd",
            );
        }
        let pool = ShmPool {
            file: Arc::new(file),
            size: pool_size,
            shm: ctx.last_sender_id,
        };

        ctx.pools.insert(pool_id, pool);
        ctx.shadow_table.create_local(pool_id, Iface::WlShmPool, 1);

        Action::Drop
    }
}

/// Make the guest `wl_shm` objects bound from host global `name`, which
/// retired, and their pools inert (see `Context::stale_shm_guest_objects`),
/// whether or not the proxy's own binding was `name`.
pub(crate) fn retire_guest_shms(ctx: &mut Context, name: u32) {
    let stale: std::collections::BTreeSet<u32> = ctx
        .guest_shms
        .iter()
        .filter(|(_, shm)| shm.global_name == name)
        .map(|(&id, _)| id)
        .collect();
    ctx.stale_shm_pools.extend(
        ctx.pools
            .iter()
            .filter(|(_, pool)| stale.contains(&pool.shm))
            .map(|(&id, _)| id),
    );
    ctx.stale_shm_guest_objects.extend(stale);
}

/// Forget synthetic guest `wl_shm` `shm_id`, which ended (see
/// `crate::state::lifecycle`), so a later guest ID reuse cannot receive
/// stale format events from it.
pub(crate) fn forget_shm(ctx: &mut Context, shm_id: u32) {
    ctx.guest_shms.remove(&shm_id);
    ctx.stale_shm_guest_objects.remove(&shm_id);
}

/// Forget synthetic guest `wl_shm_pool` `pool_id`, which ended (see
/// `crate::state::lifecycle`), and close its file.
pub(crate) fn forget_pool(ctx: &mut Context, pool_id: u32) {
    ctx.pools.remove(&pool_id);
    ctx.stale_shm_pools.remove(&pool_id);
}

impl protocols::wayland::wl_shm_pool::WlShmPoolHandler for Handlers {
    fn on_create_buffer(
        ctx: &mut Context,
        id: u32,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Action {
        debug!("Creating buffer id={}", id);
        let pool_id = ctx.last_sender_id;
        if ctx.stale_shm_pools.contains(&pool_id) {
            debug!(
                "Ignoring wl_shm_pool.create_buffer from removed host global object {}",
                pool_id
            );
            // The guest owns the new ID, so the buffer exists, inert:
            // attaching it detaches (`CompositorHandler::on_attach`).
            ctx.shadow_table.create_local(id, Iface::WlBuffer, 1);
            return Action::Drop;
        }
        // A live pool that is not stale has state: `wl_shm.create_pool`
        // records it, and only the pool's end forgets it (`forget_pool`).
        let Some(pool) = ctx.pools.get(&pool_id) else {
            return ctx.fail(format_args!("wl_shm_pool {pool_id} has no state"));
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

    fn on_destroy(_ctx: &mut Context) -> Action {
        // The generated local-only destructor path ends the synthetic pool
        // (see `forget_pool`) and queues its guest delete_id.
        Action::Drop
    }

    fn on_resize(ctx: &mut Context, size: i32) -> Action {
        let pool_id = ctx.last_sender_id;
        if ctx.stale_shm_pools.contains(&pool_id) {
            debug!(
                "Ignoring wl_shm_pool.resize from removed host global object {}",
                pool_id
            );
            return Action::Drop;
        }
        // A live pool that is not stale has state: `wl_shm.create_pool`
        // records it, and only the pool's end forgets it (`forget_pool`).
        let Some(pool) = ctx.pools.get_mut(&pool_id) else {
            return ctx.fail(format_args!("wl_shm_pool {pool_id} has no state"));
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
        valid_pool_size, valid_shm_stride,
    };
    use crate::handler::test_support::session_with_shm;
    use crate::handler::Handlers;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_shm::WlShmHandler;
    use crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler;
    use crate::protocols::wayland::{wl_display, wl_shm, wl_shm_pool};
    use crate::protocols::Iface;
    use crate::state::Context;
    use crate::state::Singleton;
    use crate::test_support::{decoded, queued_headers, raw_dispatch, shm_pool_file};
    use crate::wire::{Action, WireMessage, WL_DISPLAY_ID};
    use std::fs::File;
    use std::os::fd::AsFd;

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
            assert!(s.ctx().pools.is_empty());
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
                assert_eq!(s.ctx().pools[&pool].size, size as usize);
            } else {
                s.guest_fatal([resize]).await;
                assert_eq!(
                    s.guest_received_and_error(),
                    (vec![], (pool, wl_shm::ERROR_INVALID_FD)),
                    "resize to {size}"
                );
                // No peer sees the size the proxy records for the pool.
                assert_eq!(s.ctx().pools[&pool].size, 4096);
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
            s.ctx().pools.remove(&pool);
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

    #[test]
    fn synthetic_shm_release_clears_cache_before_guest_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        let optional_format = 0x3631_4752;
        record_host_shm_wl_format(&mut ctx, optional_format);
        ctx.shadow_table.create_local(guest_shm, Iface::WlShm, 2);
        register_guest_shm(&mut ctx, guest_shm, 10);
        ctx.host_to_client_queue.clear();
        ctx.stale_shm_guest_objects.insert(guest_shm);

        ctx.last_sender_id = guest_shm;
        let mut release = WireMessage::new(
            guest_shm,
            crate::protocols::wayland::wl_shm::REQ_RELEASE,
            &[],
        );
        assert_eq!(
            raw_dispatch(crate::test_support::dispatch_request(
                &mut ctx,
                &mut release
            )),
            Ok(None)
        );
        assert!(!ctx.guest_shms.contains_key(&guest_shm));
        assert!(!ctx.stale_shm_guest_objects.contains(&guest_shm));
        assert!(ctx.shadow_table.get_interface(guest_shm).is_none());
        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_display::Event::from_wire),
            [(WL_DISPLAY_ID, wl_display::Event::DeleteId { id: guest_shm })],
            "local wl_shm.release must emit one guest delete_id"
        );

        // Reusing the same guest ID must behave like a fresh synthetic object.
        ctx.shadow_table.create_local(guest_shm, Iface::WlShm, 2);
        register_guest_shm(&mut ctx, guest_shm, 10);
        ctx.host_to_client_queue.clear();
        record_host_shm_wl_format(&mut ctx, optional_format);
        assert!(
            ctx.host_to_client_queue.is_empty(),
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

    #[test]
    fn removed_host_shm_creates_only_inert_pools_and_buffers() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        ctx.shadow_table.create_local(guest_shm, Iface::WlShm, 1);
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        assert_eq!(
            Handlers::on_global(&mut ctx, 10, &"wl_shm".to_string(), 1),
            Action::Drop
        );
        register_guest_shm(&mut ctx, guest_shm, 10);
        // The host binding has now been retired by the real
        // wl_registry.global_remove path. The synthetic guest object itself
        // remains addressable for teardown, but its requests must no longer
        // allocate local SHM state.
        assert_eq!(Handlers::on_global_remove(&mut ctx, 10), Action::Forward);

        let fd = shm_pool_file(&[0; 4096]);

        ctx.last_sender_id = guest_shm;
        assert_eq!(
            Handlers::on_create_pool(&mut ctx, 30, fd.as_fd(), 4096),
            Action::Drop
        );
        assert!(
            ctx.pools.is_empty(),
            "a removed host wl_shm binding must not retain a new local pool"
        );
        // The guest owns the ID, so the pool exists for it to use and
        // destroy, inert like a pool created before the removal.
        assert!(ctx.shadow_table.is_local(30));
        assert_eq!(ctx.shadow_table.get_interface(30), Some(Iface::WlShmPool));
        assert!(ctx.stale_shm_pools.contains(&30));

        ctx.last_sender_id = 30;
        assert_eq!(
            <Handlers as WlShmPoolHandler>::on_create_buffer(&mut ctx, 31, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(ctx.shadow_table.is_local(31));
        assert_eq!(ctx.shadow_table.get_interface(31), Some(Iface::WlBuffer));
        assert!(!ctx.is_fatal());

        assert!(ctx.local_destroyed(30));
        assert!(!ctx.stale_shm_pools.contains(&30));
    }

    /// The host advertises `wl_shm` anew (11) and then removes the name
    /// the proxy bound (10): only the guest objects bound from 10, and
    /// their pools, go inert, while the ones bound from 11 keep working on
    /// the promoted binding.
    #[test]
    fn only_objects_bound_from_a_retired_shm_global_go_inert() {
        use crate::handler::Handlers;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        ctx.shadow_table.insert_paired_for_test(2, 2, "wl_registry");
        let shm = "wl_shm".to_string();
        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            Handlers::on_global(&mut ctx, 10, &shm, 1);
        }
        ctx.last_sender_id = 1;
        Handlers::on_global(&mut ctx, 11, &shm, 1);
        for (name, guest_shm, pool) in [(10, 20, 30), (11, 21, 31)] {
            ctx.last_sender_id = 1;
            Handlers::on_bind(&mut ctx, name, &(shm.clone(), 1, guest_shm));
            ctx.last_sender_id = guest_shm;
            let file = shm_pool_file(&[0; 4096]);
            assert_eq!(
                Handlers::on_create_pool(&mut ctx, pool, file.as_fd(), 4096),
                Action::Drop
            );
        }

        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            Handlers::on_global_remove(&mut ctx, 10);
        }

        assert_eq!(
            ctx.singletons
                .get(Singleton::Shm)
                .map(|binding| binding.name),
            Some(11)
        );
        assert!(ctx.stale_shm_guest_objects.contains(&20));
        assert!(ctx.stale_shm_pools.contains(&30));
        assert!(!ctx.stale_shm_guest_objects.contains(&21));
        assert!(!ctx.stale_shm_pools.contains(&31));
        ctx.host_to_client_queue.clear();
        record_host_shm_wl_format(&mut ctx, 0x3631_4752);
        assert_eq!(
            queued_headers(&ctx.host_to_client_queue),
            [(21, wl_shm::EVT_FORMAT)],
            "the promoted binding's formats reach only the live object"
        );
        ctx.last_sender_id = 21;
        let file = shm_pool_file(&[0; 4096]);
        assert_eq!(
            Handlers::on_create_pool(&mut ctx, 32, file.as_fd(), 4096),
            Action::Drop
        );
        ctx.last_sender_id = 32;
        assert_eq!(
            Handlers::on_create_buffer(&mut ctx, 33, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(!ctx.is_fatal());
        assert!(
            ctx.shadow_table.get_host_id(33).is_some(),
            "the buffer has host storage through the promoted binding"
        );
    }

    /// A guest `wl_shm` goes inert when the name it was bound from retires,
    /// even when the proxy bound another name.
    #[test]
    fn a_guest_shm_bound_from_an_unbound_name_goes_inert_when_it_retires() {
        use crate::handler::Handlers;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let shm = "wl_shm".to_string();
        ctx.last_sender_id = 1;
        Handlers::on_global(&mut ctx, 10, &shm, 1);
        Handlers::on_global(&mut ctx, 11, &shm, 1);
        assert_eq!(
            ctx.singletons
                .get(Singleton::Shm)
                .map(|binding| binding.name),
            Some(10)
        );
        Handlers::on_bind(&mut ctx, 11, &(shm.clone(), 1, 21));

        Handlers::on_global_remove(&mut ctx, 11);
        assert!(ctx.stale_shm_guest_objects.contains(&21));
        Handlers::on_global_remove(&mut ctx, 10);

        ctx.last_sender_id = 21;
        let file = shm_pool_file(&[0; 4096]);
        assert_eq!(
            Handlers::on_create_pool(&mut ctx, 31, file.as_fd(), 4096),
            Action::Drop
        );
        assert!(!ctx.is_fatal());
        assert!(!ctx.pools.contains_key(&31));
        assert!(ctx.stale_shm_pools.contains(&31), "the pool is inert");
    }

    /// A registry that saw a `wl_shm` name removed may see it advertised
    /// anew while another registry still lists the old generation: the old
    /// generation's objects go inert then, and the new one's stay live,
    /// even when the other registry's removal arrives late.
    #[test]
    fn a_re_advertised_shm_name_retires_only_its_old_generation() {
        use crate::handler::Handlers;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        fn create_pool(ctx: &mut Context, shm: u32, pool: u32) {
            ctx.last_sender_id = shm;
            let file = shm_pool_file(&[0; 4096]);
            assert_eq!(
                Handlers::on_create_pool(ctx, pool, file.as_fd(), 4096),
                Action::Drop
            );
        }
        fn bind(ctx: &mut Context, name: u32, shm: u32) {
            ctx.last_sender_id = 1;
            Handlers::on_bind(ctx, name, &("wl_shm".to_string(), 1, shm));
        }

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        ctx.shadow_table.insert_paired_for_test(2, 2, "wl_registry");
        let shm = "wl_shm".to_string();
        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            Handlers::on_global(&mut ctx, 10, &shm, 1);
            Handlers::on_global(&mut ctx, 11, &shm, 1);
        }
        assert_eq!(
            ctx.singletons
                .get(Singleton::Shm)
                .map(|binding| binding.name),
            Some(10)
        );
        bind(&mut ctx, 11, 21);
        create_pool(&mut ctx, 21, 31);

        ctx.last_sender_id = 1;
        Handlers::on_global_remove(&mut ctx, 11);
        assert!(
            !ctx.stale_shm_guest_objects.contains(&21),
            "registry 2 lists 11"
        );
        Handlers::on_global(&mut ctx, 11, &shm, 1);

        assert!(ctx.stale_shm_guest_objects.contains(&21));
        assert!(ctx.stale_shm_pools.contains(&31));
        bind(&mut ctx, 11, 22);
        create_pool(&mut ctx, 22, 32);
        ctx.last_sender_id = 32;
        assert_eq!(
            Handlers::on_create_buffer(&mut ctx, 33, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(ctx.shadow_table.get_host_id(33).is_some());

        ctx.last_sender_id = 2;
        Handlers::on_global_remove(&mut ctx, 11);

        assert!(!ctx.stale_shm_guest_objects.contains(&22));
        assert!(!ctx.stale_shm_pools.contains(&32));
        create_pool(&mut ctx, 22, 34);
        assert!(ctx.pools.contains_key(&34));
        assert!(!ctx.is_fatal());
    }

    #[test]
    fn removed_host_shm_ignores_requests_from_existing_pool_until_destroy() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let shm = "wl_shm".to_string();
        assert_eq!(Handlers::on_global(&mut ctx, 10, &shm, 1), Action::Drop);
        let guest_shm = 20;
        register_guest_shm(&mut ctx, guest_shm, 10);

        let fd = shm_pool_file(&[0; 4096]);

        ctx.last_sender_id = guest_shm;
        assert_eq!(
            <Handlers as WlShmHandler>::on_create_pool(&mut ctx, 30, fd.as_fd(), 4096),
            Action::Drop
        );
        assert!(ctx.pools.contains_key(&30));

        ctx.last_sender_id = 1;
        assert_eq!(Handlers::on_global_remove(&mut ctx, 10), Action::Forward);
        assert!(ctx.stale_shm_pools.contains(&30));

        ctx.last_sender_id = 30;
        assert_eq!(
            <Handlers as WlShmPoolHandler>::on_resize(&mut ctx, 8192),
            Action::Drop
        );
        assert_eq!(
            ctx.pools.get(&30).map(|pool| pool.size),
            Some(4096),
            "a stale pool must not resize"
        );
        assert_eq!(
            <Handlers as WlShmPoolHandler>::on_create_buffer(&mut ctx, 31, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(
            ctx.shadow_table.get_host_id(31).is_none(),
            "a stale pool must not allocate a host buffer through replacement SHM"
        );
        assert_eq!(ctx.shadow_table.get_interface(31), Some(Iface::WlBuffer));

        assert!(ctx.local_destroyed(30));
        assert!(!ctx.pools.contains_key(&30));
        assert!(!ctx.stale_shm_pools.contains(&30));
    }
}
