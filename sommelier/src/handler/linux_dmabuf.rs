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

use crate::handler::display::queue_protocol_error;
use crate::handler::shm::{record_host_shm_drm_format, WL_SHM_FORMAT_NV12};
use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_feedback_v1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
use crate::state::{Context, PendingParam};
use crate::wire::{Action, MessageBuilder};
use log::{debug, error};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::io::{IntoRawFd, RawFd};

pub struct LinuxDmabufHandler;

fn queue_message(
    queue: &mut Vec<(Vec<u8>, Vec<RawFd>)>,
    sender_id: u32,
    opcode: u16,
    builder: MessageBuilder,
    fds: Vec<RawFd>,
) -> bool {
    match builder.try_build_message(sender_id, opcode) {
        Ok(message) => {
            queue.push((message, fds));
            true
        }
        Err(error) => {
            log::warn!(
                "Dropping oversized linux-dmabuf message sender={} opcode={}: {}",
                sender_id,
                opcode,
                error
            );
            for fd in fds {
                if fd >= 0 {
                    let _ = nix::unistd::close(fd);
                }
            }
            false
        }
    }
}

fn is_supported_drm_format(format: u32) -> bool {
    // Keep this list in sync with ChromiumOS
    // `sl_drm_format_is_supported()`. These are the formats for which the
    // proxy can safely import/copy a single-plane buffer (plus NV12 for the
    // host feedback protocol).
    matches!(
        format,
        WL_SHM_FORMAT_NV12 // DRM_FORMAT_NV12
            | 0x3631_4752 // DRM_FORMAT_RGB565
            | 0x3432_5258 // DRM_FORMAT_XRGB8888
            | 0x3432_5241 // DRM_FORMAT_ARGB8888
            | 0x3432_4258 // DRM_FORMAT_XBGR8888
            | 0x3432_4241 // DRM_FORMAT_ABGR8888
    )
}

fn guest_device_bytes(ctx: &Context, host_device: &[u8]) -> Option<Vec<u8>> {
    let dev_t_size = std::mem::size_of::<libc::dev_t>();
    if host_device.len() != dev_t_size {
        log::warn!(
            "Rejecting malformed dmabuf device ID: {} bytes (expected {})",
            host_device.len(),
            dev_t_size
        );
        return None;
    }

    // ChromiumOS rewrites the host dev_t to the device backing the local GBM
    // allocator. A fixed renderD128 dev_t breaks on systems where the render
    // node has a different minor number, so derive it from the actual fd.
    let Some(allocator) = ctx.allocator.as_ref() else {
        return Some(host_device.to_vec());
    };

    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let fd = allocator.device.as_fd().as_raw_fd();
    if unsafe { libc::fstat(fd, &mut stat) } == 0 {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&stat.st_rdev as *const libc::dev_t).cast::<u8>(),
                std::mem::size_of::<libc::dev_t>(),
            )
        };
        return Some(bytes.to_vec());
    }

    log::warn!(
        "Failed to stat GBM device fd {}: {}; dropping device feedback",
        fd,
        std::io::Error::last_os_error()
    );
    None
}

fn format_table_fd_can_map(fd: RawFd, size: usize) -> bool {
    if fd < 0 {
        return false;
    }

    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return false;
    }

    // memfd and regular files expose a useful length that can be checked
    // before mmap. virtwl and other shared-memory providers are not regular
    // files and commonly report st_size=0 (or no meaningful size at all);
    // mmap is the authoritative validation for those descriptors.
    let is_regular = (stat.st_mode as libc::mode_t) & libc::S_IFMT == libc::S_IFREG;
    !is_regular
        || (stat.st_size >= 0
            && u64::try_from(stat.st_size)
                .ok()
                .is_some_and(|file_size| file_size >= size as u64))
}

fn dmabuf_plane_layout(format: u32, plane_idx: u32, width: i32, height: i32) -> Option<(u64, u64)> {
    let width = u64::try_from(width).ok()?;
    let height = u64::try_from(height).ok()?;
    match format {
        WL_SHM_FORMAT_NV12 if width % 2 == 0 && height % 2 == 0 && plane_idx < 2 => {
            Some((width, if plane_idx == 0 { height } else { height / 2 }))
        }
        0x3631_4752 if plane_idx == 0 => Some((width.checked_mul(2)?, height)),
        0x3432_5258 | 0x3432_5241 | 0x3432_4258 | 0x3432_4241 if plane_idx == 0 => {
            Some((width.checked_mul(4)?, height))
        }
        _ => None,
    }
}

fn valid_dmabuf_plane(param: &PendingParam, format: u32, width: i32, height: i32) -> bool {
    let Some((minimum_row_bytes, rows)) =
        dmabuf_plane_layout(format, param.plane_idx, width, height)
    else {
        return false;
    };
    let stride = u64::from(param.stride);
    if stride < minimum_row_bytes {
        return false;
    }

    // The host compositor will evaluate offset + stride * (rows - 1) +
    // minimum_row_bytes while importing the plane. Check the same arithmetic
    // here so malformed metadata cannot wrap into a small in-bounds span.
    let Some(last_row_offset) = stride.checked_mul(rows.saturating_sub(1)) else {
        return false;
    };
    u64::from(param.offset)
        .checked_add(last_row_offset)
        .and_then(|end| end.checked_add(minimum_row_bytes))
        .is_some()
}

impl LinuxDmabufHandler {
    fn process_params(
        &self,
        ctx: &mut Context,
        params_id: u32,
        _width: i32,
        _height: i32,
        _format: u32,
        send_create: impl FnOnce(&mut Context, u32) -> bool,
    ) -> bool {
        let Some(params) = ctx.pending_params.remove(&params_id) else {
            return false;
        };
        let Some(host_id) = ctx.shadow_table.get_host_id(params_id) else {
            error!("Unknown host ID for params {}", params_id);
            // A stale entry can only arise after an earlier create failed
            // between parameter decoding and host-ID lookup. Drop it before
            // closing the queued plane descriptors so this params ID cannot
            // accidentally retain a sync fence for a later request.
            ctx.pending_native_sync_fds.remove(&params_id);
            for param in params {
                unsafe { libc::close(param.fd) };
            }
            // The guest request was valid when it entered the proxy, so a
            // missing host mapping is an internal state corruption rather
            // than a recoverable client error. Do not let callers continue
            // with a CREATE that has no corresponding host params object.
            ctx.fatal_protocol_error = true;
            return false;
        };

        // Keep one independent plane descriptor for the host buffer's
        // compositor-use fence. The queued ADD owns the descriptor passed to
        // the host; this duplicate survives until the corresponding host
        // wl_buffer.delete_id and is waited immediately before each commit.
        let sync_source = params
            .iter()
            .find(|param| param.plane_idx == 0)
            .or_else(|| params.first());
        let sync_fd = sync_source.and_then(|param| {
            nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(param.fd) })
                .map_err(|error| {
                    error!(
                        "Failed to duplicate dma-buf fd for synchronization: {}",
                        error
                    );
                })
                .ok()
        });
        let Some(sync_fd) = sync_fd else {
            for param in params {
                unsafe { libc::close(param.fd) };
            }
            ctx.fatal_protocol_error = true;
            return false;
        };
        ctx.pending_native_sync_fds.insert(params_id, sync_fd);

        // Send ADDs. If the wire builder ever rejects one of these fixed-size
        // messages, close every descriptor that was not handed to the queue
        // and terminate the connection before a partial CREATE can be sent.
        let mut params = params.into_iter();
        while let Some(param) = params.next() {
            let mut builder = MessageBuilder::new();

            // PASS THE GUEST'S EXACT METADATA. NO MINIGBM OVERRIDES.
            builder.write_u32(param.plane_idx);
            builder.write_u32(param.offset);
            builder.write_u32(param.stride);
            builder.write_u32(param.modifier_hi);
            builder.write_u32(param.modifier_lo);

            if !queue_message(
                &mut ctx.client_to_host_queue,
                host_id,
                zwp_linux_buffer_params_v1::REQ_ADD,
                builder,
                vec![param.fd],
            ) {
                for remaining in params {
                    unsafe { libc::close(remaining.fd) };
                }
                ctx.pending_native_sync_fds.remove(&params_id);
                ctx.fatal_protocol_error = true;
                return false;
            }
        }

        // Send CREATE only after every ADD was queued successfully.
        if !send_create(ctx, host_id) {
            ctx.pending_native_sync_fds.remove(&params_id);
            ctx.fatal_protocol_error = true;
            return false;
        }
        true
    }

    fn discard_pending_params(ctx: &mut Context, params_id: u32) {
        ctx.pending_native_sync_fds.remove(&params_id);
        if let Some(params) = ctx.pending_params.remove(&params_id) {
            for param in params {
                unsafe {
                    libc::close(param.fd);
                }
            }
        }
    }

    fn valid_params(ctx: &Context, params_id: u32, width: i32, height: i32, format: u32) -> bool {
        if width <= 0 || height <= 0 || !is_supported_drm_format(format) {
            return false;
        }

        let Some(params) = ctx.pending_params.get(&params_id) else {
            return false;
        };
        let expected_planes: u32 = if format == WL_SHM_FORMAT_NV12 { 2 } else { 1 }; // DRM_FORMAT_NV12
        if params.len() != expected_planes as usize {
            return false;
        }

        let mut seen_planes = std::collections::HashSet::new();
        params.iter().all(|param| {
            param.fd >= 0
                && param.plane_idx < expected_planes
                && seen_planes.insert(param.plane_idx)
                && valid_dmabuf_plane(param, format, width, height)
        })
    }

    fn params_error_code(
        ctx: &Context,
        params_id: u32,
        width: i32,
        height: i32,
        format: u32,
    ) -> u32 {
        if width <= 0 || height <= 0 {
            return 5; // invalid_dimensions
        }
        if !is_supported_drm_format(format) {
            return 4; // invalid_format
        }
        let expected_planes = if format == WL_SHM_FORMAT_NV12 { 2 } else { 1 };
        let Some(params) = ctx.pending_params.get(&params_id) else {
            return 3; // incomplete
        };
        if params.len() != expected_planes {
            return 3; // incomplete
        }
        let mut seen = std::collections::HashSet::new();
        for param in params {
            if param.plane_idx >= expected_planes as u32 {
                return 1; // plane_idx
            }
            if !seen.insert(param.plane_idx) {
                return 2; // plane_set
            }
            if !valid_dmabuf_plane(param, format, width, height) {
                return 6; // out_of_bounds
            }
        }
        3
    }
}

impl zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1Handler for LinuxDmabufHandler {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        Action::Forward
    }

    fn on_create_params(&mut self, ctx: &mut Context, params_id: u32) -> Action {
        ctx.pending_native_sync_fds.remove(&params_id);
        if let Some(previous) = ctx.pending_params.insert(params_id, Vec::new()) {
            // Reusing a params ID is a protocol violation, but closing stale
            // duplicated plane FDs here keeps the connection from leaking
            // resources before the compositor reports the error.
            for param in previous {
                unsafe {
                    libc::close(param.fd);
                }
            }
        }
        Action::Forward
    }

    fn on_get_default_feedback(&mut self, _ctx: &mut Context, _id: u32) -> Action {
        Action::Forward
    }

    fn on_get_surface_feedback(&mut self, _ctx: &mut Context, _id: u32, _surface: u32) -> Action {
        Action::Forward
    }

    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if !is_supported_drm_format(format) {
            return Action::Drop;
        }
        ctx.supported_formats.insert(format);

        if let Some(internal_id) = ctx.host_dmabuf_id {
            if ctx.last_sender_id == internal_id {
                record_host_shm_drm_format(ctx, format);
                return Action::Drop;
            }
        }

        // if format == 0x34324241 || format == 0x34324258 {
        //     return Action::Drop;
        // }

        Action::Forward
    }

    fn on_modifier(
        &mut self,
        ctx: &mut Context,
        format: u32,
        modifier_hi: u32,
        modifier_lo: u32,
    ) -> Action {
        let modifier = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
        debug!(
            "Host advertises format: {:#010x}, modifier: {:#018x}",
            format, modifier
        );
        if !is_supported_drm_format(format) {
            return Action::Drop;
        }
        // We forward supported modifiers to the guest. The modifier itself is
        // intentionally preserved; LINEAR (0) is a valid modifier.
        ctx.supported_formats.insert(format);

        if let Some(internal_id) = ctx.host_dmabuf_id {
            if ctx.last_sender_id == internal_id {
                record_host_shm_drm_format(ctx, format);
                return Action::Drop;
            }
        }
        Action::Forward
    }
}

impl zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1Handler for LinuxDmabufHandler {
    fn on_main_device(&mut self, ctx: &mut Context, _device: &[u8]) -> Action {
        let guest_id = match ctx.shadow_table.get_guest_id(ctx.last_sender_id) {
            Some(id) => id,
            None => return Action::Drop,
        };

        let Some(dev_id_bytes) = guest_device_bytes(ctx, _device) else {
            return Action::Drop;
        };

        let mut builder = MessageBuilder::new();
        builder.write_array(&dev_id_bytes);

        queue_message(
            &mut ctx.host_to_client_queue,
            guest_id,
            zwp_linux_dmabuf_feedback_v1::EVT_MAIN_DEVICE,
            builder,
            Vec::new(),
        );
        Action::Drop
    }

    fn on_tranche_target_device(&mut self, ctx: &mut Context, _device: &[u8]) -> Action {
        let guest_id = match ctx.shadow_table.get_guest_id(ctx.last_sender_id) {
            Some(id) => id,
            None => return Action::Drop,
        };

        let Some(dev_id_bytes) = guest_device_bytes(ctx, _device) else {
            return Action::Drop;
        };

        let mut builder = MessageBuilder::new();
        builder.write_array(&dev_id_bytes);

        queue_message(
            &mut ctx.host_to_client_queue,
            guest_id,
            zwp_linux_dmabuf_feedback_v1::EVT_TRANCHE_TARGET_DEVICE,
            builder,
            Vec::new(),
        );
        Action::Drop
    }

    fn on_format_table(&mut self, ctx: &mut Context, fd: RawFd, size: u32) -> Action {
        let guest_id = match ctx.shadow_table.get_guest_id(ctx.last_sender_id) {
            Some(id) => id,
            None => return Action::Drop,
        };
        // A feedback object may resend its table. Do not let tranche events
        // from the new sequence use indices from an older table if rewriting
        // this one fails.
        ctx.feedback_index_maps.remove(&guest_id);

        if fd < 0 {
            log::warn!("Rejecting dmabuf format table with invalid fd {}", fd);
            return Action::Drop;
        }

        if size == 0 || !size.is_multiple_of(16) {
            log::warn!("Rejecting malformed dmabuf format table size {}", size);
            return Action::Drop;
        }

        let entry_count = (size / 16) as usize;
        // Tranche indices are uint16 values. Refuse a table whose entries
        // cannot be represented without wrapping an index during rewriting.
        if entry_count > usize::from(u16::MAX) + 1 {
            log::warn!(
                "Rejecting dmabuf format table with too many entries: {}",
                entry_count
            );
            return Action::Drop;
        }

        if !format_table_fd_can_map(fd, size as usize) {
            log::warn!(
                "Rejecting dmabuf format table fd {} that cannot provide {} bytes",
                fd,
                size
            );
            return Action::Drop;
        }

        // The protocol requires clients to map the table read-only/private.
        // Keep the host table byte-for-byte intact: duplicate entries are
        // meaningful because tranche indices can use them to express
        // different preferences. Only the tranche index stream is filtered
        // below, just like ChromiumOS Sommelier.
        let host_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };

        if host_ptr == libc::MAP_FAILED {
            return Action::Drop;
        }

        let host_entries =
            unsafe { std::slice::from_raw_parts(host_ptr as *const u32, entry_count * 4) };

        let mut index_map = std::collections::HashMap::new();

        for i in 0..entry_count {
            let format = host_entries[i * 4];

            if !is_supported_drm_format(format) {
                continue;
            }
            // Preserve the host's original index, including duplicate
            // format+modifier entries. Re-indexing or deduplicating changes
            // the preference semantics of a tranche.
            index_map.insert(i as u16, i as u16);
        }
        unsafe {
            libc::munmap(host_ptr, size as usize);
        }
        let new_fd = match nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(fd) }) {
            Ok(fd) => fd.into_raw_fd(),
            Err(error) => {
                log::warn!(
                    "Failed to duplicate dmabuf format table fd {}: {}",
                    fd,
                    error
                );
                return Action::Drop;
            }
        };
        let new_size = size;

        let mut builder = MessageBuilder::new();
        builder.write_u32(new_size);

        ctx.feedback_index_maps.insert(guest_id, index_map);
        if !queue_message(
            &mut ctx.host_to_client_queue,
            guest_id,
            zwp_linux_dmabuf_feedback_v1::EVT_FORMAT_TABLE,
            builder,
            vec![new_fd],
        ) {
            ctx.feedback_index_maps.remove(&guest_id);
        }

        Action::Drop
    }

    fn on_tranche_formats(&mut self, ctx: &mut Context, indices: &[u8]) -> Action {
        let guest_id = match ctx.shadow_table.get_guest_id(ctx.last_sender_id) {
            Some(id) => id,
            None => return Action::Drop,
        };

        let index_map = match ctx.feedback_index_maps.get(&guest_id) {
            Some(map) => map,
            None => return Action::Drop,
        };

        if !indices.len().is_multiple_of(2) {
            log::warn!(
                "Rejecting malformed dmabuf tranche format indices with odd length {}",
                indices.len()
            );
            return Action::Drop;
        }

        let mut new_indices = Vec::new();
        for i in 0..(indices.len() / 2) {
            let idx = u16::from_ne_bytes([indices[i * 2], indices[i * 2 + 1]]);
            if let Some(&new_idx) = index_map.get(&idx) {
                new_indices.extend_from_slice(&new_idx.to_ne_bytes());
            }
        }

        let mut builder = MessageBuilder::new();
        builder.write_array(&new_indices);

        queue_message(
            &mut ctx.host_to_client_queue,
            guest_id,
            zwp_linux_dmabuf_feedback_v1::EVT_TRANCHE_FORMATS,
            builder,
            Vec::new(),
        );

        Action::Drop
    }

    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        // This is a client→host request, so last_sender_id is the guest
        // feedback ID. Looking it up as a host ID leaves the index map alive
        // until the connection is torn down and can corrupt a later feedback
        // object that reuses the ID.
        let guest_id = ctx.last_sender_id;
        ctx.feedback_index_maps.remove(&guest_id);
        Action::Forward
    }
}

impl zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1Handler for LinuxDmabufHandler {
    fn on_destroy(&mut self, ctx: &mut Context) -> Action {
        let id = ctx.last_sender_id;
        if let (Some(host_id), true) = (
            ctx.shadow_table.get_host_id(id),
            ctx.pending_native_buffer_sizes.contains_key(&id),
        ) {
            // The host may acknowledge params.destroy before its asynchronous
            // created/failed event. Keep a host-keyed marker so the late event
            // can still be dispatched after the guest mapping is retired.
            ctx.orphaned_dmabuf_params.insert(host_id, id);
        }
        ctx.pending_native_sync_fds.remove(&id);
        if let Some(params) = ctx.pending_params.remove(&id) {
            for p in params {
                unsafe { libc::close(p.fd) };
            }
        }
        ctx.pending_native_buffer_sizes.remove(&id);
        Action::Forward
    }

    fn on_add(
        &mut self,
        ctx: &mut Context,
        fd: RawFd,
        plane_idx: u32,
        offset: u32,
        stride: u32,
        modifier_hi: u32,
        modifier_lo: u32,
    ) -> Action {
        if fd < 0 {
            error!("Ignoring dmabuf add with invalid fd {}", fd);
            return Action::Drop;
        }

        let params_id = ctx.last_sender_id;
        if plane_idx > 1 {
            queue_protocol_error(ctx, params_id, 1, "dmabuf plane index out of bounds");
            return Action::Drop;
        }
        if ctx
            .pending_params
            .get(&params_id)
            .is_some_and(|params| params.iter().any(|param| param.plane_idx == plane_idx))
        {
            queue_protocol_error(ctx, params_id, 2, "dmabuf plane was already set");
            return Action::Drop;
        }

        let dup_fd = match nix::unistd::dup(unsafe { BorrowedFd::borrow_raw(fd) }) {
            Ok(d) => d.into_raw_fd(),
            Err(e) => {
                error!("Failed to dup FD for add: {}", e);
                return Action::Drop;
            }
        };

        // ChromiumOS fixes up plane-0 metadata for classic virtio-gpu PRIME
        // resources. The guest may have supplied an implicit modifier or a
        // shadow-buffer stride that differs from the host resource layout.
        // Querying the local DRM device before buffering the duplicate keeps
        // the forwarded ADD request importable by the host compositor while
        // leaving non-virtio and unsupported-kernel paths unchanged.
        let (stride, modifier_hi, modifier_lo) = if plane_idx == 0 {
            if let Some(allocator) = ctx.allocator.as_ref() {
                let guest_modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
                let fixup = allocator.fixup_dmabuf_plane0(fd, stride, guest_modifier);
                (
                    fixup.stride,
                    (fixup.modifier >> 32) as u32,
                    fixup.modifier as u32,
                )
            } else {
                (stride, modifier_hi, modifier_lo)
            }
        } else {
            (stride, modifier_hi, modifier_lo)
        };

        if let Some(list) = ctx.pending_params.get_mut(&params_id) {
            list.push(PendingParam {
                fd: dup_fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            });
        } else {
            // If create_params wasn't tracked (e.g. before connection), we can't buffer.
            // But we should have tracked it.
            error!("Unknown params ID {} in add", params_id);
            unsafe { libc::close(dup_fd) };
        }

        Action::Drop
    }

    fn on_create(
        &mut self,
        ctx: &mut Context,
        width: i32,
        height: i32,
        format: u32,
        flags: u32,
    ) -> Action {
        let params_id = ctx.last_sender_id;
        if !Self::valid_params(ctx, params_id, width, height, format) {
            error!(
                "Rejecting invalid dmabuf create: params={}, width={}, height={}, format={:#010x}",
                params_id, width, height, format
            );
            let code = Self::params_error_code(ctx, params_id, width, height, format);
            queue_protocol_error(
                ctx,
                params_id,
                code,
                "invalid linux-dmabuf buffer parameters",
            );
            Self::discard_pending_params(ctx, params_id);
            return Action::Drop;
        }

        // The non-immediate create request reports its wl_buffer through a
        // later `created` event. Keep the dimensions under the guest params
        // ID until that event supplies the host-created buffer ID.
        ctx.pending_native_buffer_sizes
            .insert(params_id, (width, height));
        let queued = self.process_params(ctx, params_id, width, height, format, |ctx, host_id| {
            let mut builder = MessageBuilder::new();
            builder.write_i32(width);
            builder.write_i32(height);
            builder.write_u32(format);
            builder.write_u32(flags);

            queue_message(
                &mut ctx.client_to_host_queue,
                host_id,
                zwp_linux_buffer_params_v1::REQ_CREATE,
                builder,
                Vec::new(),
            )
        });
        if !queued {
            ctx.pending_native_buffer_sizes.remove(&params_id);
        }
        Action::Drop
    }

    fn on_created(&mut self, ctx: &mut Context, buffer: u32) -> Action {
        let host_params_id = ctx.last_sender_id;
        if let Some(&guest_params_id) = ctx.orphaned_dmabuf_params.get(&host_params_id) {
            if !ctx.shadow_table.is_host_id_available(buffer) {
                error!(
                    "Host returned an already-used wl_buffer ID {} for orphaned dmabuf",
                    buffer
                );
                ctx.fatal_protocol_error = true;
                return Action::Drop;
            }
            let params_mapping_alive =
                ctx.shadow_table.get_guest_id(host_params_id) == Some(guest_params_id);
            let version = ctx
                .shadow_table
                .host_object_version(host_params_id)
                .unwrap_or(u32::MAX);
            ctx.shadow_table.track_host_interface_with_version(
                buffer,
                "wl_buffer".to_string(),
                version,
            );
            if crate::handler::shm::queue_host_buffer_destroy(ctx, buffer) {
                ctx.shadow_table.mark_pending_destroy_host(buffer);
            } else {
                ctx.shadow_table.remove_host_interface(buffer);
            }
            ctx.orphaned_dmabuf_params.remove(&host_params_id);
            ctx.pending_native_sync_fds.remove(&guest_params_id);
            if params_mapping_alive {
                ctx.pending_native_buffer_sizes.remove(&guest_params_id);
            }
            // If delete_id for params arrived first, the host params interface
            // was retained only to dispatch this late event. Its destructor
            // has already been acknowledged, so release that reservation now.
            if ctx.shadow_table.get_guest_id(host_params_id).is_none() {
                ctx.shadow_table.remove_host_interface(host_params_id);
            }
            return Action::Drop;
        }
        if let Some(guest_params_id) = ctx.shadow_table.get_guest_id(host_params_id) {
            let orphaned = ctx.shadow_table.is_pending_destroy_guest(guest_params_id);
            if orphaned {
                // params.destroy may have been queued before the compositor
                // emitted `created`.  The generated dispatcher cannot map the
                // event's new_id when the event is consumed locally, so retain
                // the host buffer as a host-only wl_buffer and destroy it
                // explicitly.  Otherwise it would live until connection
                // teardown with no guest object or release path.
                if !ctx.shadow_table.is_host_id_available(buffer) {
                    error!(
                        "Host returned an already-used wl_buffer ID {} for orphaned dmabuf",
                        buffer
                    );
                    ctx.fatal_protocol_error = true;
                    return Action::Drop;
                }
                let version = ctx
                    .shadow_table
                    .host_object_version(host_params_id)
                    .unwrap_or(u32::MAX);
                ctx.shadow_table.track_host_interface_with_version(
                    buffer,
                    "wl_buffer".to_string(),
                    version,
                );
                if crate::handler::shm::queue_host_buffer_destroy(ctx, buffer) {
                    ctx.shadow_table.mark_pending_destroy_host(buffer);
                } else {
                    ctx.shadow_table.remove_host_interface(buffer);
                }
                ctx.orphaned_dmabuf_params.remove(&host_params_id);
                ctx.pending_native_buffer_sizes.remove(&guest_params_id);
                return Action::Drop;
            }
            if let Some(dimensions) = ctx.pending_native_buffer_sizes.remove(&guest_params_id) {
                // The generated dispatcher maps this host-created buffer to a
                // fresh guest server ID immediately after the handler returns.
                // Store the dimensions under the host ID for that short
                // interval; compositor damage lookup resolves the guest ID
                // back to this host ID.
                ctx.native_buffer_sizes.insert(buffer, dimensions);
                if let Some(sync_fd) = ctx.pending_native_sync_fds.remove(&guest_params_id) {
                    ctx.native_buffer_sync_fds.insert(buffer, sync_fd);
                }
            }
        }
        Action::Forward
    }

    fn on_failed(&mut self, ctx: &mut Context) -> Action {
        let host_params_id = ctx.last_sender_id;
        if let Some(guest_params_id) = ctx.orphaned_dmabuf_params.get(&host_params_id).copied() {
            let params_mapping_alive =
                ctx.shadow_table.get_guest_id(host_params_id) == Some(guest_params_id);
            ctx.orphaned_dmabuf_params.remove(&host_params_id);
            ctx.pending_native_sync_fds.remove(&guest_params_id);
            if params_mapping_alive {
                ctx.pending_native_buffer_sizes.remove(&guest_params_id);
            }
            // When params delete_id preceded this event, retain the host
            // interface only until this final async result is consumed.
            if ctx.shadow_table.get_guest_id(host_params_id).is_none() {
                ctx.shadow_table.remove_host_interface(host_params_id);
            }
            return Action::Drop;
        }
        if let Some(guest_params_id) = ctx.shadow_table.get_guest_id(host_params_id) {
            if ctx.shadow_table.is_pending_destroy_guest(guest_params_id) {
                ctx.orphaned_dmabuf_params.remove(&host_params_id);
                ctx.pending_native_buffer_sizes.remove(&guest_params_id);
                ctx.pending_native_sync_fds.remove(&guest_params_id);
                return Action::Drop;
            }
            ctx.pending_native_buffer_sizes.remove(&guest_params_id);
            ctx.pending_native_sync_fds.remove(&guest_params_id);
        }
        Action::Forward
    }

    fn on_create_immed(
        &mut self,
        ctx: &mut Context,
        buffer_id: u32,
        width: i32,
        height: i32,
        format: u32,
        flags: u32,
    ) -> Action {
        let params_id = ctx.last_sender_id;
        // A params object is tracked as soon as create_params is decoded. If
        // it is absent here, do not allocate/map a host wl_buffer: doing so
        // leaves a dangling guest→host mapping even though no create request
        // can be sent to the compositor.
        if !ctx.pending_params.contains_key(&params_id) {
            error!(
                "Ignoring create_immed for untracked buffer params {}",
                params_id
            );
            return Action::Drop;
        }
        if ctx.shadow_table.get_host_id(params_id).is_none() {
            // Consume and close any duplicated plane FDs, but do not create a
            // guest buffer mapping when the params object itself has no host
            // counterpart.
            let _ = self.process_params(ctx, params_id, width, height, format, |_, _| true);
            return Action::Drop;
        }
        if !Self::valid_params(ctx, params_id, width, height, format) {
            error!(
                "Rejecting invalid dmabuf create_immed: params={}, width={}, height={}, format={:#010x}",
                params_id, width, height, format
            );
            let code = Self::params_error_code(ctx, params_id, width, height, format);
            queue_protocol_error(
                ctx,
                params_id,
                code,
                "invalid linux-dmabuf buffer parameters",
            );
            Self::discard_pending_params(ctx, params_id);
            return Action::Drop;
        }

        // We need to map buffer_id manually because we are dropping the request.
        // Codegen would map it if we forwarded.
        // buffer_id is new_id wl_buffer.
        let host_buffer_id = ctx.shadow_table.allocate_host_id();
        let buffer_version = ctx
            .shadow_table
            .guest_object_version(params_id)
            .unwrap_or(u32::MAX);
        ctx.shadow_table.map_id(buffer_id, host_buffer_id);
        ctx.shadow_table.track_interface_with_version(
            buffer_id,
            "wl_buffer".to_string(),
            buffer_version,
        );
        ctx.shadow_table
            .set_host_version(host_buffer_id, buffer_version);
        ctx.native_buffer_sizes.insert(buffer_id, (width, height));

        let queued = self.process_params(ctx, params_id, width, height, format, |ctx, host_id| {
            let mut builder = MessageBuilder::new();
            builder.write_u32(host_buffer_id);
            builder.write_i32(width);
            builder.write_i32(height);
            builder.write_u32(format);
            builder.write_u32(flags);

            queue_message(
                &mut ctx.client_to_host_queue,
                host_id,
                zwp_linux_buffer_params_v1::REQ_CREATE_IMMED,
                builder,
                Vec::new(),
            )
        });
        if !queued {
            ctx.native_buffer_sizes.remove(&buffer_id);
            ctx.shadow_table.remove_id(buffer_id);
        } else if let Some(sync_fd) = ctx.pending_native_sync_fds.remove(&params_id) {
            ctx.native_buffer_sync_fds.insert(host_buffer_id, sync_fd);
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_table_fd_can_map, guest_device_bytes, is_supported_drm_format, LinuxDmabufHandler,
    };
    use crate::handler::display::DisplayHandler;
    use crate::handler::shm::WL_SHM_FORMAT_NV12;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1Handler;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1Handler;
    use crate::protocols::wayland::wl_display::WlDisplayHandler;
    use crate::state::{Context, PendingParam};
    use crate::wire::{Action, WireMessage};

    #[test]
    fn forwards_device_id_when_no_local_allocator_exists() {
        let mut ctx = Context::new(false, false);
        ctx.allocator = None;
        let host_device = vec![1; std::mem::size_of::<libc::dev_t>()];
        assert_eq!(guest_device_bytes(&ctx, &host_device), Some(host_device));
    }

    #[test]
    fn rejects_malformed_device_id_before_forwarding() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.allocator = None;
        let host_device = vec![1; std::mem::size_of::<libc::dev_t>() - 1];
        assert_eq!(guest_device_bytes(&ctx, &host_device), None);

        ctx.shadow_table.map_id(10, 20);
        ctx.last_sender_id = 20;
        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            ZwpLinuxDmabufFeedbackV1Handler::on_main_device(&mut handler, &mut ctx, &host_device,),
            Action::Drop
        );
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "malformed device IDs must not produce a guest event"
        );
    }

    #[test]
    fn supported_formats_match_chromiumos_format_table() {
        assert!(is_supported_drm_format(WL_SHM_FORMAT_NV12)); // NV12
        assert!(is_supported_drm_format(0x3631_4752)); // RGB565
        assert!(is_supported_drm_format(0x3432_5258)); // XRGB8888
        assert!(is_supported_drm_format(0x3432_5241)); // ARGB8888
        assert!(is_supported_drm_format(0x3432_4258)); // XBGR8888
        assert!(is_supported_drm_format(0x3432_4241)); // ABGR8888
        assert!(!is_supported_drm_format(0xdead_beef));
    }

    #[test]
    fn create_immed_without_tracked_params_does_not_map_buffer() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(7, 8);
        ctx.last_sender_id = 7;

        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            handler.on_create_immed(&mut ctx, 90, 16, 16, 0x3432_5258, 0),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(90), None);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    #[test]
    fn feedback_destroy_removes_format_index_mapping() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 20);
        ctx.feedback_index_maps
            .insert(10, std::collections::HashMap::new());
        // destroy is a guest request, therefore last_sender_id is the guest
        // ID rather than the host ID.
        ctx.last_sender_id = 10;

        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            ZwpLinuxDmabufFeedbackV1Handler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(!ctx.feedback_index_maps.contains_key(&10));
    }

    #[test]
    fn invalid_dmabuf_fd_is_rejected_before_borrowing() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(7, Vec::new());
        ctx.last_sender_id = 7;
        let mut handler = LinuxDmabufHandler;

        assert_eq!(handler.on_add(&mut ctx, -1, 0, 0, 4, 0, 0), Action::Drop);
        assert!(ctx.pending_params[&7].is_empty());
    }

    #[test]
    fn invalid_plane_requests_queue_declared_protocol_errors() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(7, Vec::new());
        ctx.last_sender_id = 7;
        let mut handler = LinuxDmabufHandler;

        assert_eq!(
            handler.on_add(&mut ctx, pipe_fds[1], 2, 0, 64, 0, 0),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            1
        );
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        let mut first_pipe = [-1; 2];
        let mut second_pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(first_pipe.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(second_pipe.as_mut_ptr()) }, 0);
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(
            7,
            vec![PendingParam {
                fd: first_pipe[1],
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        ctx.last_sender_id = 7;
        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            handler.on_add(&mut ctx, second_pipe[1], 0, 4096, 64, 0, 0),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            2
        );
        unsafe {
            libc::close(first_pipe[0]);
            libc::close(first_pipe[1]);
            libc::close(second_pipe[0]);
            libc::close(second_pipe[1]);
        }
    }

    #[test]
    fn invalid_dmabuf_create_queues_dimensions_error() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(
            7,
            vec![PendingParam {
                fd: pipe_fds[1],
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        ctx.last_sender_id = 7;
        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            handler.on_create(&mut ctx, 0, 16, 0x3432_5258, 0),
            Action::Drop
        );
        assert!(ctx.fatal_protocol_error);
        assert_eq!(
            u32::from_ne_bytes(ctx.host_to_client_queue[0].0[12..16].try_into().unwrap()),
            5
        );
        unsafe {
            libc::close(pipe_fds[0]);
        }
    }

    #[test]
    fn async_dmabuf_created_event_moves_dimensions_to_host_buffer() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let guest_params_id = 7;
        let host_params_id = 8;
        let host_buffer_id: u32 = 42;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_params_id, host_params_id);
        ctx.pending_params.insert(
            guest_params_id,
            vec![PendingParam {
                fd: pipe_fds[1],
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        ctx.last_sender_id = guest_params_id;
        let mut handler = LinuxDmabufHandler;

        assert_eq!(
            handler.on_create(&mut ctx, 16, 8, 0x3432_5258, 0),
            Action::Drop
        );
        assert_eq!(
            ctx.pending_native_buffer_sizes.get(&guest_params_id),
            Some(&(16, 8)),
            "async create must retain dimensions until the host emits created"
        );
        assert!(
            ctx.pending_native_sync_fds.contains_key(&guest_params_id),
            "async create must retain a dma-buf fence descriptor until created"
        );
        assert!(!ctx.native_buffer_sizes.contains_key(&host_buffer_id));

        let payload = host_buffer_id.to_ne_bytes();
        ctx.last_sender_id = host_params_id;
        let mut message = WireMessage::new(
            host_params_id,
            crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_CREATED,
            &payload,
            &[],
        );
        let result = crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::dispatch_event(
            &mut message,
            &mut handler,
            &mut ctx,
        )
        .expect("created event should decode");
        assert!(
            result.is_some(),
            "created must still be forwarded to the guest"
        );
        assert_eq!(ctx.pending_native_buffer_sizes.get(&guest_params_id), None);
        assert!(!ctx.pending_native_sync_fds.contains_key(&guest_params_id));
        assert_eq!(ctx.native_buffer_sizes.get(&host_buffer_id), Some(&(16, 8)));
        assert!(
            ctx.native_buffer_sync_fds.contains_key(&host_buffer_id),
            "created must move the retained fence descriptor to the host buffer"
        );
        let guest_buffer_id = ctx
            .shadow_table
            .get_guest_id(host_buffer_id)
            .expect("generated created event must map the host buffer");
        assert!(
            !ctx.native_buffer_sizes.contains_key(&guest_buffer_id),
            "dimensions are keyed by host ID until the host delete_id lifecycle completes"
        );

        unsafe {
            libc::close(pipe_fds[0]);
        }
    }

    #[test]
    fn async_dmabuf_failed_event_discards_pending_dimensions() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let guest_params_id = 7;
        let host_params_id = 8;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_params_id, host_params_id);
        ctx.pending_params.insert(
            guest_params_id,
            vec![PendingParam {
                fd: pipe_fds[1],
                plane_idx: 0,
                offset: 0,
                stride: 64,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );
        ctx.last_sender_id = guest_params_id;
        let mut handler = LinuxDmabufHandler;
        assert_eq!(
            handler.on_create(&mut ctx, 16, 8, 0x3432_5258, 0),
            Action::Drop
        );
        assert!(ctx
            .pending_native_buffer_sizes
            .contains_key(&guest_params_id));

        ctx.last_sender_id = host_params_id;
        let mut message = WireMessage::new(
            host_params_id,
            crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_FAILED,
            &[],
            &[],
        );
        let result = crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::dispatch_event(
            &mut message,
            &mut handler,
            &mut ctx,
        )
        .expect("failed event should decode");
        assert!(
            result.is_some(),
            "failed must still be forwarded to the guest"
        );
        assert!(ctx.pending_native_buffer_sizes.is_empty());
        assert!(ctx.native_buffer_sizes.is_empty());

        unsafe {
            libc::close(pipe_fds[0]);
        }
    }

    #[test]
    fn destroying_async_params_discards_pending_dimensions() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_native_buffer_sizes.insert(7, (16, 8));
        ctx.last_sender_id = 7;
        let mut handler = LinuxDmabufHandler;

        assert_eq!(
            ZwpLinuxBufferParamsV1Handler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
        assert!(ctx.pending_native_buffer_sizes.is_empty());
    }

    #[test]
    fn orphaned_async_created_buffer_is_destroyed_locally() {
        let guest_params_id = 7;
        let host_params_id = 8;
        let host_buffer_id: u32 = 42;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_params_id, host_params_id);
        ctx.shadow_table.track_interface_with_version(
            guest_params_id,
            "zwp_linux_buffer_params_v1".into(),
            4,
        );
        ctx.shadow_table.set_host_version(host_params_id, 4);
        ctx.pending_native_buffer_sizes
            .insert(guest_params_id, (16, 8));
        let mut handler = LinuxDmabufHandler;
        ctx.last_sender_id = guest_params_id;
        assert_eq!(
            ZwpLinuxBufferParamsV1Handler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
        // This is what the generated params.destroy dispatcher records after
        // forwarding a destroy request, before the delayed created event can
        // arrive from the host.
        ctx.shadow_table.mark_pending_destroy(guest_params_id);

        ctx.last_sender_id = host_params_id;
        let payload = host_buffer_id.to_ne_bytes();
        let mut message = WireMessage::new(
            host_params_id,
            crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::EVT_CREATED,
            &payload,
            &[],
        );
        let result = crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::dispatch_event(
            &mut message,
            &mut handler,
            &mut ctx,
        )
        .expect("orphaned created event should decode");

        assert!(
            result.is_none(),
            "orphaned buffer must not be exposed to guest"
        );
        assert!(ctx.pending_native_buffer_sizes.is_empty());
        assert!(!ctx.native_buffer_sizes.contains_key(&host_buffer_id));
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap())),
            Some(host_buffer_id)
        );
        assert_eq!(
            ctx.client_to_host_queue
                .last()
                .map(|(message, _)| u32::from_ne_bytes(message[4..8].try_into().unwrap()) as u16),
            Some(crate::protocols::wayland::wl_buffer::REQ_DESTROY)
        );
        assert!(ctx
            .shadow_table
            .is_pending_destroy_host_only(host_buffer_id));
        assert!(ctx
            .shadow_table
            .get_host_interface(host_buffer_id)
            .is_none());
        assert!(ctx.shadow_table.consume_host_delete_id(host_buffer_id));
    }

    #[test]
    fn late_async_created_after_params_delete_id_is_destroyed_locally() {
        let guest_params_id = 7;
        let host_params_id = 8;
        let host_buffer_id: u32 = 42;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_params_id, host_params_id);
        ctx.shadow_table.track_interface_with_version(
            guest_params_id,
            "zwp_linux_buffer_params_v1".into(),
            4,
        );
        ctx.shadow_table.set_host_version(host_params_id, 4);
        ctx.pending_native_buffer_sizes
            .insert(guest_params_id, (16, 8));
        let mut handler = LinuxDmabufHandler;
        ctx.last_sender_id = guest_params_id;
        assert_eq!(
            ZwpLinuxBufferParamsV1Handler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
        // The generated dispatcher records the guest destructor after the
        // handler returns.
        ctx.shadow_table.mark_pending_destroy(guest_params_id);

        let mut display = DisplayHandler;
        assert_eq!(
            WlDisplayHandler::on_delete_id(&mut display, &mut ctx, host_params_id),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_guest_id(host_params_id), None);
        assert!(ctx
            .shadow_table
            .get_host_interface(host_params_id)
            .is_some_and(|name| name == "zwp_linux_buffer_params_v1"));
        assert!(ctx.orphaned_dmabuf_params.contains_key(&host_params_id));
        assert!(
            ctx.host_to_client_queue
                .iter()
                .any(
                    |(message, _)| u32::from_ne_bytes(message[8..12].try_into().unwrap())
                        == guest_params_id
                ),
            "params delete_id must still reach the guest"
        );

        ctx.last_sender_id = host_params_id;
        let payload = host_buffer_id.to_ne_bytes();
        let mut message = WireMessage::new(
            host_params_id,
            zwp_linux_buffer_params_v1::EVT_CREATED,
            &payload,
            &[],
        );
        assert_eq!(
            zwp_linux_buffer_params_v1::dispatch_event(&mut message, &mut handler, &mut ctx),
            Ok(None)
        );
        assert!(!ctx.orphaned_dmabuf_params.contains_key(&host_params_id));
        assert!(ctx
            .shadow_table
            .get_host_interface(host_params_id)
            .is_none());
        assert!(ctx
            .shadow_table
            .is_pending_destroy_host_only(host_buffer_id));
        assert!(
            ctx.client_to_host_queue
                .iter()
                .any(
                    |(message, _)| u32::from_ne_bytes(message[0..4].try_into().unwrap())
                        == host_buffer_id
                ),
            "late created must destroy the host-only wl_buffer"
        );
    }

    #[test]
    fn orphan_params_delete_id_does_not_clear_reused_guest_dimensions() {
        let guest_params_id = 7;
        let host_params_id = 8;
        let replacement_host_params_id = 9;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(guest_params_id, host_params_id);
        ctx.shadow_table.track_interface_with_version(
            guest_params_id,
            "zwp_linux_buffer_params_v1".into(),
            4,
        );
        ctx.shadow_table.set_host_version(host_params_id, 4);
        ctx.pending_native_buffer_sizes
            .insert(guest_params_id, (16, 8));
        let mut handler = LinuxDmabufHandler;
        ctx.last_sender_id = guest_params_id;
        assert_eq!(
            ZwpLinuxBufferParamsV1Handler::on_destroy(&mut handler, &mut ctx),
            Action::Forward
        );
        ctx.shadow_table.mark_pending_destroy(guest_params_id);

        // The old guest object has already received its delete_id and the
        // client immediately reuses the numeric ID for a replacement params
        // object. Keep the old host event reservation while installing the
        // replacement mapping and dimensions.
        ctx.shadow_table.remove_guest_mapping(guest_params_id);
        ctx.shadow_table
            .clear_pending_destroy_guest(guest_params_id);
        ctx.shadow_table
            .map_id(guest_params_id, replacement_host_params_id);
        ctx.shadow_table.track_interface_with_version(
            guest_params_id,
            "zwp_linux_buffer_params_v1".into(),
            4,
        );
        ctx.shadow_table
            .set_host_version(replacement_host_params_id, 4);
        ctx.pending_native_buffer_sizes
            .insert(guest_params_id, (32, 16));

        let mut display = DisplayHandler;
        assert_eq!(
            WlDisplayHandler::on_delete_id(&mut display, &mut ctx, host_params_id),
            Action::Drop
        );
        assert_eq!(
            ctx.pending_native_buffer_sizes.get(&guest_params_id),
            Some(&(32, 16)),
            "late delete_id for the old host object must not clear replacement dimensions"
        );
        assert!(ctx.orphaned_dmabuf_params.contains_key(&host_params_id));
    }

    #[test]
    fn dmabuf_params_require_unique_planes_for_the_format() {
        let mut first_pipe = [-1; 2];
        let mut second_pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(first_pipe.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(second_pipe.as_mut_ptr()) }, 0);
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(
            7,
            vec![
                PendingParam {
                    fd: first_pipe[1],
                    plane_idx: 0,
                    offset: 0,
                    stride: 64,
                    modifier_hi: 0,
                    modifier_lo: 0,
                },
                PendingParam {
                    fd: second_pipe[1],
                    plane_idx: 0,
                    offset: 4096,
                    stride: 64,
                    modifier_hi: 0,
                    modifier_lo: 0,
                },
            ],
        );
        assert!(!LinuxDmabufHandler::valid_params(
            &ctx,
            7,
            16,
            16,
            0x3432_5258
        ));
        drop(ctx);
        unsafe {
            libc::close(first_pipe[0]);
            libc::close(second_pipe[0]);
        }
    }

    #[test]
    fn dmabuf_params_reject_stride_that_cannot_cover_one_row() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(
            7,
            vec![PendingParam {
                fd: pipe_fds[1],
                plane_idx: 0,
                offset: 0,
                // XRGB8888 needs four bytes per pixel.
                stride: 4 * 3,
                modifier_hi: 0,
                modifier_lo: 0,
            }],
        );

        assert!(
            !LinuxDmabufHandler::valid_params(&ctx, 7, 4, 4, 0x3432_5258),
            "a dmabuf stride shorter than width × bytes-per-pixel must be rejected"
        );
        drop(ctx);
        unsafe {
            libc::close(pipe_fds[0]);
        }
    }

    #[test]
    fn odd_tranche_index_array_is_rejected() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 20);
        ctx.feedback_index_maps
            .insert(10, std::collections::HashMap::new());
        ctx.last_sender_id = 20;

        let mut handler = LinuxDmabufHandler;
        assert_eq!(handler.on_tranche_formats(&mut ctx, &[0]), Action::Drop);
        assert!(ctx.host_to_client_queue.is_empty());
    }

    #[test]
    fn non_regular_format_table_fds_are_validated_by_mmap() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);

        // virtwl-backed descriptors do not expose a regular-file size. They
        // must reach mmap rather than being rejected solely because st_size is
        // zero. A pipe cannot actually be mapped, but this helper deliberately
        // leaves that final check to mmap.
        assert!(format_table_fd_can_map(pipe_fds[0], 4096));

        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
    }

    #[test]
    fn format_table_rewrite_keeps_wayland_entry_width_and_modifier() {
        let name = std::ffi::CString::new("sommelier-format-table-input").unwrap();
        let input_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(input_fd >= 0);

        // Each linux-dmabuf feedback entry is exactly four native-endian
        // u32 words: format, padding, modifier_hi, modifier_lo. Include an
        // unsupported entry and a duplicate supported entry to exercise both
        // filtering and index de-duplication.
        let entries = [
            0x3432_5258u32,
            0,
            0x1122_3344,
            0x5566_7788,
            0xdead_beefu32,
            0,
            0,
            0,
            0x3432_5258u32,
            0,
            0x1122_3344,
            0x5566_7788,
        ];
        let bytes: Vec<u8> = entries.iter().flat_map(|word| word.to_ne_bytes()).collect();
        assert_eq!(unsafe { libc::ftruncate(input_fd, bytes.len() as i64) }, 0);
        assert_eq!(
            unsafe { libc::pwrite(input_fd, bytes.as_ptr().cast(), bytes.len(), 0,) },
            bytes.len() as isize
        );

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.map_id(10, 20);
        ctx.last_sender_id = 20;
        let mut handler = LinuxDmabufHandler;

        assert_eq!(
            handler.on_format_table(&mut ctx, input_fd, bytes.len() as u32),
            Action::Drop
        );
        unsafe {
            libc::close(input_fd);
        }

        assert_eq!(ctx.host_to_client_queue.len(), 1);
        let rewritten_fd = ctx.host_to_client_queue[0].1[0];
        let mut rewritten = [0u8; 48];
        assert_eq!(
            unsafe {
                libc::pread(
                    rewritten_fd,
                    rewritten.as_mut_ptr().cast(),
                    rewritten.len(),
                    0,
                )
            },
            rewritten.len() as isize
        );
        let words: Vec<u32> = rewritten
            .chunks_exact(4)
            .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(words, entries);
        assert_eq!(ctx.feedback_index_maps[&10].get(&0), Some(&0));
        assert_eq!(ctx.feedback_index_maps[&10].get(&2), Some(&2));
        assert!(!ctx.feedback_index_maps[&10].contains_key(&1));
    }
}
