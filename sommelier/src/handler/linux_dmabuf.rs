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

use crate::handler::shm::record_host_shm_drm_format;
use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_feedback_v1;
use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
use crate::protocols::wayland::{wl_display, wl_shm};
use crate::state::{Context, HostId, PendingNativeCreate, PendingParam};
use crate::wire::{Action, MessageHeader, Outgoing, WL_DISPLAY_ID};
use log::{debug, error};
use nix::sys::memfd::MFdFlags;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

/// `DRM_FORMAT_MOD_INVALID` is the legacy implicit-modifier marker used by
/// the linux-dmabuf v3 format events. Keep this local instead of depending on
/// libdrm headers so the proxy remains buildable in the minimal Nix shell.
const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
/// The newest `zwp_linux_dmabuf_v1` offered to guests: the version that
/// added the feedback objects, which the proxy synthesizes. v5 changes
/// tranche semantics that the proxy does not translate, so it is never
/// offered. A feedback object takes its factory's version, so it has this
/// one too.
pub(crate) const GUEST_DMABUF_VERSION: u32 = zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK_SINCE;
const MAX_FEEDBACK_FORMATS: usize = u16::MAX as usize + 1;
/// The most uint16 indices one `tranche_formats` event carries: the longest
/// message less its header and array length word, in whole 4-byte words.
const MAX_TRANCHE_INDICES_PER_EVENT: usize = (MessageHeader::MAX_LEN - MessageHeader::LEN - 4) / 2;

pub struct LinuxDmabufHandler;

fn is_supported_drm_format(format: u32) -> bool {
    // Keep this list in sync with ChromiumOS
    // `sl_drm_format_is_supported()`. These are the formats for which the
    // proxy can safely import/copy a single-plane buffer (plus NV12 for the
    // host feedback protocol).
    matches!(
        format,
        wl_shm::FORMAT_NV12 // DRM_FORMAT_NV12
            | 0x3631_4752 // DRM_FORMAT_RGB565
            | 0x3432_5258 // DRM_FORMAT_XRGB8888
            | 0x3432_5241 // DRM_FORMAT_ARGB8888
            | 0x3432_4258 // DRM_FORMAT_XBGR8888
            | 0x3432_4241 // DRM_FORMAT_ABGR8888
    )
}

fn local_device_id(ctx: &Context) -> io::Result<Vec<u8>> {
    let allocator = ctx.allocator.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "GBM allocator unavailable for synthetic dmabuf feedback",
        )
    })?;
    let mut device = vec![0u8; std::mem::size_of::<libc::dev_t>()];

    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(allocator.device.as_fd().as_raw_fd(), &mut stat) } == 0 {
        let raw = unsafe {
            std::slice::from_raw_parts(
                (&stat.st_rdev as *const libc::dev_t).cast::<u8>(),
                std::mem::size_of::<libc::dev_t>(),
            )
        };
        device.copy_from_slice(raw);
        Ok(device)
    } else {
        Err(io::Error::last_os_error())
    }
}

fn feedback_pairs(ctx: &Context, generation: u64) -> Vec<(u32, u64)> {
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(capabilities) = ctx.dmabuf_capabilities.get(&generation) {
        for &(format, modifier) in &capabilities.format_modifiers {
            if is_supported_drm_format(format)
                && seen.insert((format, modifier))
                && pairs.len() < MAX_FEEDBACK_FORMATS
            {
                pairs.push((format, modifier));
            }
        }
    }

    pairs
}

pub(crate) fn has_synthetic_feedback_formats(ctx: &Context, generation: u64) -> bool {
    !feedback_pairs(ctx, generation).is_empty()
}

pub(crate) fn maybe_reclaim_capability_generation(ctx: &mut Context, generation: u64) {
    if !capability_generation_held(ctx, generation) {
        ctx.dmabuf_capabilities.remove(&generation);
    }
}

/// Whether anything still holds capability generation `generation`: the
/// host's current global, a sync awaiting its formats, a guest factory or
/// synthetic feedback, or a bind waiting for its formats.
pub(crate) fn capability_generation_held(ctx: &Context, generation: u64) -> bool {
    ctx.host_dmabuf_generation == Some(generation)
        || ctx.internal_callbacks.awaits_dmabuf_generation(generation)
        || ctx
            .dmabuf_guest_generations
            .values()
            .any(|&value| value == generation)
        || ctx
            .synthetic_feedback_objects
            .values()
            .any(|&value| value == generation)
        || ctx
            .pending_dmabuf_globals
            .iter()
            .any(|pending| pending.generation == generation)
}

fn create_feedback_format_table(pairs: &[(u32, u64)]) -> io::Result<(OwnedFd, u32)> {
    if pairs.is_empty() || pairs.len() > MAX_FEEDBACK_FORMATS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid synthetic feedback table length",
        ));
    }
    let size = pairs
        .len()
        .checked_mul(16)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "format table overflow"))?;
    let mut file = File::from(nix::sys::memfd::memfd_create(
        c"sommelier-dmabuf",
        MFdFlags::MFD_CLOEXEC | MFdFlags::MFD_ALLOW_SEALING,
    )?);
    file.set_len(size as u64)?;

    let mut table = Vec::with_capacity(size);
    for &(format, modifier) in pairs {
        table.extend_from_slice(&format.to_ne_bytes());
        table.extend_from_slice(&0u32.to_ne_bytes());
        table.extend_from_slice(&modifier.to_ne_bytes());
    }
    file.write_all(&table)?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((file.into(), size as u32))
}

fn build_synthetic_feedback_messages(
    guest_id: u32,
    pairs: &[(u32, u64)],
    device: &[u8],
) -> io::Result<Vec<Outgoing>> {
    let (table, table_size) = create_feedback_format_table(pairs)?;
    // Only the array-carrying events can exceed the wire limit. They are
    // built before the table's event takes its descriptor, so a failure
    // closes the table with nothing queued.
    let too_large = |_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "synthetic dmabuf feedback event exceeds Wayland wire limits",
        )
    };
    let main_device =
        zwp_linux_dmabuf_feedback_v1::evt::main_device(guest_id, device).map_err(too_large)?;
    let tranche_device = zwp_linux_dmabuf_feedback_v1::evt::tranche_target_device(guest_id, device)
        .map_err(too_large)?;
    let tranche_formats = (0..pairs.len())
        .step_by(MAX_TRANCHE_INDICES_PER_EVENT)
        .map(|start| {
            let end = pairs
                .len()
                .min(start.saturating_add(MAX_TRANCHE_INDICES_PER_EVENT));
            let mut bytes = Vec::with_capacity((end - start) * 2);
            for index in start..end {
                bytes.extend_from_slice(&(index as u16).to_ne_bytes());
            }
            zwp_linux_dmabuf_feedback_v1::evt::tranche_formats(guest_id, &bytes)
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(too_large)?;

    // The table is sent before the parameter sequence, matching existing
    // compositors and allowing every following tranche index to refer to it.
    let mut pending = vec![
        zwp_linux_dmabuf_feedback_v1::evt::format_table(guest_id, table, table_size),
        main_device,
        tranche_device,
        // No direct-scanout preference is known for a guest-side render
        // node; a zero flag is the conservative default tranche.
        zwp_linux_dmabuf_feedback_v1::evt::tranche_flags(guest_id, 0),
    ];
    pending.extend(tranche_formats);
    pending.extend([
        zwp_linux_dmabuf_feedback_v1::evt::tranche_done(guest_id),
        zwp_linux_dmabuf_feedback_v1::evt::done(guest_id),
    ]);
    Ok(pending)
}

fn dmabuf_plane_layout(format: u32, plane_idx: u32, width: i32, height: i32) -> Option<(u64, u64)> {
    let width = u64::try_from(width).ok()?;
    let height = u64::try_from(height).ok()?;
    match format {
        wl_shm::FORMAT_NV12 if width % 2 == 0 && height % 2 == 0 && plane_idx < 2 => {
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

/// Reject the message for synthetic feedback the proxy could not allocate,
/// as libwayland's `wl_client_post_no_memory` does: the display's
/// `no_memory` error.
#[must_use = "a handler returns the Action::Drop"]
fn reject_unallocatable_feedback(ctx: &mut Context) -> Action {
    ctx.reject(
        WL_DISPLAY_ID,
        wl_display::ERROR_NO_MEMORY,
        "unable to allocate linux-dmabuf feedback",
    )
}

impl LinuxDmabufHandler {
    fn queue_synthetic_feedback(
        ctx: &mut Context,
        guest_id: u32,
        generation: u64,
    ) -> io::Result<()> {
        let pairs = feedback_pairs(ctx, generation);
        let device = local_device_id(ctx)?;
        let pending = build_synthetic_feedback_messages(guest_id, &pairs, &device)?;

        // Publish a complete sequence atomically. A client must never observe a
        // new format_table without the matching done event.
        ctx.host_to_client_queue.extend(pending);
        Ok(())
    }

    fn create_synthetic_feedback(ctx: &mut Context, guest_id: u32) -> Action {
        let factory_id = ctx.last_sender_id;
        let Some(generation) = ctx.dmabuf_guest_generations.get(&factory_id).copied() else {
            // The factory interface declares no errors, and the client did
            // nothing wrong: the proxy lost its own bookkeeping.
            return ctx.fail(format_args!(
                "Cannot create synthetic dmabuf feedback for untracked factory {factory_id}"
            ));
        };
        ctx.shadow_table.create_local(
            guest_id,
            "zwp_linux_dmabuf_feedback_v1",
            GUEST_DMABUF_VERSION,
        );
        ctx.synthetic_feedback_objects.insert(guest_id, generation);
        let ready = ctx
            .dmabuf_capabilities
            .get(&generation)
            .is_some_and(|capabilities| capabilities.ready);
        if ready {
            if let Err(error) = Self::queue_synthetic_feedback(ctx, guest_id, generation) {
                log::error!(
                    "Failed to create synthetic dmabuf feedback for generation {}: {}",
                    generation,
                    error
                );
                return reject_unallocatable_feedback(ctx);
            }
        }
        Action::Drop
    }

    pub(crate) fn complete_capability_discovery(ctx: &mut Context, generation: u64) {
        let Some(capabilities) = ctx.dmabuf_capabilities.get_mut(&generation) else {
            log::warn!(
                "Ignoring linux-dmabuf capability callback for unknown generation {}",
                generation
            );
            return;
        };
        if capabilities.ready {
            return;
        }
        capabilities.ready = true;

        crate::handler::registry::publish_pending_dmabuf_globals(ctx, generation);

        let feedback_ids = ctx
            .synthetic_feedback_objects
            .iter()
            .filter_map(|(&guest_id, &object_generation)| {
                (object_generation == generation).then_some(guest_id)
            })
            .collect::<Vec<_>>();
        for guest_id in feedback_ids {
            if let Err(error) = Self::queue_synthetic_feedback(ctx, guest_id, generation) {
                log::error!(
                    "Failed to publish completed dmabuf feedback generation {}: {}",
                    generation,
                    error
                );
                // Deep in a wl_callback.done: the session ends, and the
                // loop drops this message's effects whatever its handler
                // returns.
                let _ = reject_unallocatable_feedback(ctx);
                break;
            }
        }
    }

    fn refresh_synthetic_feedbacks(ctx: &mut Context) {
        let Some(generation) = ctx.host_dmabuf_generation else {
            return;
        };
        if !ctx
            .dmabuf_capabilities
            .get(&generation)
            .is_some_and(|capabilities| capabilities.ready)
        {
            return;
        }
        let feedback_ids = ctx
            .synthetic_feedback_objects
            .iter()
            .filter_map(|(&guest_id, &object_generation)| {
                (object_generation == generation).then_some(guest_id)
            })
            .collect::<Vec<_>>();
        for guest_id in feedback_ids {
            // Legacy dmabuf capabilities arrive as one format event followed
            // by many modifier events. Queue at most one replacement per
            // proxy dispatch batch; otherwise every event creates another
            // memfd-bearing format_table message and a large host roundtrip
            // can exceed the receiver's SCM_RIGHTS capacity.
            if !ctx.synthetic_feedback_refresh_pending.insert(guest_id) {
                continue;
            }
            if let Err(error) = Self::queue_synthetic_feedback(ctx, guest_id, generation) {
                log::error!(
                    "Failed to refresh synthetic dmabuf feedback generation {}: {}",
                    generation,
                    error
                );
                ctx.synthetic_feedback_refresh_pending.remove(&guest_id);
                // Deep in a capability event: the session ends, and the
                // loop drops this message's effects whatever its handler
                // returns.
                let _ = reject_unallocatable_feedback(ctx);
                break;
            }
        }
    }

    /// Queue the ADDs of params `params_id` and mark it used. Returns its
    /// host ID and the planes' synchronization descriptors, or the action
    /// a handler returns when it cannot.
    fn process_params(
        &self,
        ctx: &mut Context,
        params_id: u32,
    ) -> Result<(HostId, Vec<OwnedFd>), Action> {
        let params = ctx.pending_params.remove(&params_id).ok_or(Action::Drop)?;
        let Some(host_id) = ctx.shadow_table.get_host_id(params_id) else {
            // The guest request was valid when it entered the proxy, so a
            // missing host mapping is an internal state corruption rather
            // than a recoverable client error. Do not let callers continue
            // with a CREATE that has no corresponding host params object.
            return Err(ctx.fail(format_args!("Unknown host ID for params {params_id}")));
        };

        // Keep one descriptor for every plane. Multi-plane formats may use
        // separate dma-buf objects, so waiting only plane zero can present
        // plane-one GPU writes before they complete. Inode identity proves
        // two descriptors are the same dma-buf only from Linux 5.3, where
        // each dma-buf has its own inode, while an extra wait on shared
        // backing is harmless.
        // The queued ADD owns the original descriptor while these duplicates
        // live with the render-buffer generation through its final destroy.
        let mut sync_fds = Vec::new();
        for param in &params {
            match param.fd.try_clone() {
                Ok(sync_fd) => sync_fds.push(sync_fd),
                Err(error) => {
                    return Err(ctx.fail(format_args!(
                        "Failed to duplicate dma-buf fd for synchronization: {error}"
                    )));
                }
            }
        }
        if sync_fds.is_empty() {
            return Err(ctx.fail(format_args!("Params {params_id} have no planes to create")));
        }

        // Send the ADDs, each handing its descriptor to the queue. The caller
        // queues its CREATE right after.
        for param in params {
            // PASS THE GUEST'S EXACT METADATA. NO MINIGBM OVERRIDES.
            ctx.client_to_host_queue
                .push(zwp_linux_buffer_params_v1::req::add(
                    host_id,
                    param.fd,
                    param.plane_idx,
                    param.offset,
                    param.stride,
                    param.modifier_hi,
                    param.modifier_lo,
                ));
        }
        ctx.used_dmabuf_params.insert(params_id);
        Ok((HostId(host_id), sync_fds))
    }

    fn valid_params(ctx: &Context, params_id: u32, width: i32, height: i32, format: u32) -> bool {
        if width <= 0 || height <= 0 || !is_supported_drm_format(format) {
            return false;
        }

        let Some(params) = ctx.pending_params.get(&params_id) else {
            return false;
        };
        let expected_planes: u32 = if format == wl_shm::FORMAT_NV12 { 2 } else { 1 }; // DRM_FORMAT_NV12
        if params.len() != expected_planes as usize {
            return false;
        }

        let mut seen_planes = std::collections::HashSet::new();
        params.iter().all(|param| {
            param.plane_idx < expected_planes
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
            return zwp_linux_buffer_params_v1::ERROR_INVALID_DIMENSIONS;
        }
        if !is_supported_drm_format(format) {
            return zwp_linux_buffer_params_v1::ERROR_INVALID_FORMAT;
        }
        let expected_planes = if format == wl_shm::FORMAT_NV12 { 2 } else { 1 };
        let Some(params) = ctx.pending_params.get(&params_id) else {
            return zwp_linux_buffer_params_v1::ERROR_INCOMPLETE;
        };
        if params.len() != expected_planes {
            return zwp_linux_buffer_params_v1::ERROR_INCOMPLETE;
        }
        let mut seen = std::collections::HashSet::new();
        for param in params {
            if param.plane_idx >= expected_planes as u32 {
                return zwp_linux_buffer_params_v1::ERROR_PLANE_IDX;
            }
            if !seen.insert(param.plane_idx) {
                return zwp_linux_buffer_params_v1::ERROR_PLANE_SET;
            }
            if !valid_dmabuf_plane(param, format, width, height) {
                return zwp_linux_buffer_params_v1::ERROR_OUT_OF_BOUNDS;
            }
        }
        zwp_linux_buffer_params_v1::ERROR_INCOMPLETE
    }
}

/// Forget guest `zwp_linux_dmabuf_v1` `factory_id`, which ended (see
/// `crate::state::lifecycle`), and its hold on a capability generation.
pub(crate) fn forget_factory(ctx: &mut Context, factory_id: u32) {
    if let Some(generation) = ctx.dmabuf_guest_generations.remove(&factory_id) {
        maybe_reclaim_capability_generation(ctx, generation);
    }
}

/// Forget synthetic guest `zwp_linux_dmabuf_feedback_v1` `feedback_id`,
/// which ended (see `crate::state::lifecycle`), and its hold on a
/// capability generation.
pub(crate) fn forget_feedback(ctx: &mut Context, feedback_id: u32) {
    if let Some(generation) = ctx.synthetic_feedback_objects.remove(&feedback_id) {
        ctx.synthetic_feedback_refresh_pending.remove(&feedback_id);
        maybe_reclaim_capability_generation(ctx, generation);
    }
}

/// Forget guest `zwp_linux_buffer_params_v1` `params_id`, which ended (see
/// `crate::state::lifecycle`), with its planes.
///
/// The host may still emit the created/failed event it owes before it
/// processes params.destroy; `on_created` and `on_failed` recognize it by
/// the guest's pending destructor. Removing the pending generation of the
/// host object `host_params_id` closes its synchronization descriptor
/// immediately.
pub(crate) fn forget_params(ctx: &mut Context, params_id: u32, host_params_id: Option<u32>) {
    ctx.used_dmabuf_params.remove(&params_id);
    if let Some(host_params_id) = host_params_id {
        ctx.pending_native_creates.remove(&HostId(host_params_id));
    }
    ctx.pending_params.remove(&params_id);
}

impl zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1Handler for LinuxDmabufHandler {
    fn on_create_params(&mut self, ctx: &mut Context, params_id: u32) -> Action {
        ctx.used_dmabuf_params.remove(&params_id);
        // Reusing a params ID is a protocol violation; replacing the stale
        // planes closes their descriptors before the compositor reports it.
        ctx.pending_params.insert(params_id, Vec::new());
        Action::Forward
    }

    fn on_get_default_feedback(&mut self, _ctx: &mut Context, _id: u32) -> Action {
        Self::create_synthetic_feedback(_ctx, _id)
    }

    fn on_get_surface_feedback(&mut self, ctx: &mut Context, id: u32, _surface: u32) -> Action {
        Self::create_synthetic_feedback(ctx, id)
    }

    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if !is_supported_drm_format(format) {
            return Action::Drop;
        }
        ctx.supported_formats.insert(format);

        if let Some(internal_id) = ctx.host_dmabuf_id {
            if ctx.last_sender_id == internal_id {
                record_host_shm_drm_format(ctx, format);
                let mut changed = false;
                if let Some(generation) = ctx.host_dmabuf_generation {
                    let capabilities = ctx.dmabuf_capabilities.entry(generation).or_default();
                    if capabilities.format_modifiers.len() < MAX_FEEDBACK_FORMATS
                        && !capabilities
                            .format_modifiers
                            .contains(&(format, DRM_FORMAT_MOD_INVALID))
                    {
                        capabilities
                            .format_modifiers
                            .push((format, DRM_FORMAT_MOD_INVALID));
                        changed = true;
                    }
                }
                if changed {
                    Self::refresh_synthetic_feedbacks(ctx);
                }
                return Action::Drop;
            }
        }

        // A synthetic v4 factory is backed by a v3 host object so create_params
        // remains forwardable. Do not leak the host's deprecated legacy events
        // to a guest that bound the v4 contract.
        if ctx
            .shadow_table
            .get_guest_id(ctx.last_sender_id)
            .and_then(|guest_id| ctx.shadow_table.guest_object_version(guest_id))
            .is_some_and(|version| version >= zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK_SINCE)
        {
            return Action::Drop;
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
                let mut changed = false;
                if let Some(generation) = ctx.host_dmabuf_generation {
                    let capabilities = ctx.dmabuf_capabilities.entry(generation).or_default();
                    if capabilities.format_modifiers.len() < MAX_FEEDBACK_FORMATS
                        && !capabilities.format_modifiers.contains(&(format, modifier))
                    {
                        capabilities.format_modifiers.push((format, modifier));
                        changed = true;
                    }
                }
                if changed {
                    Self::refresh_synthetic_feedbacks(ctx);
                }
                return Action::Drop;
            }
        }
        if ctx
            .shadow_table
            .get_guest_id(ctx.last_sender_id)
            .and_then(|guest_id| ctx.shadow_table.guest_object_version(guest_id))
            .is_some_and(|version| version >= zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK_SINCE)
        {
            return Action::Drop;
        }
        Action::Forward
    }
}

/// The proxy never creates a host `zwp_linux_dmabuf_feedback_v1`: it binds
/// the host's linux-dmabuf below the feedback version
/// (`LINUX_DMABUF_CAPABILITY_VERSION`) and serves every guest feedback
/// object itself ([`LinuxDmabufHandler::create_synthetic_feedback`]). No
/// host event of the interface can therefore arrive, and only the guest's
/// destructor needs handling.
impl zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1Handler for LinuxDmabufHandler {
    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // The generated dispatcher completes the local object's destructor
        // (see `forget_feedback`).
        Action::Drop
    }
}

impl zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1Handler for LinuxDmabufHandler {
    fn on_add(
        &mut self,
        ctx: &mut Context,
        fd: BorrowedFd<'_>,
        plane_idx: u32,
        offset: u32,
        stride: u32,
        modifier_hi: u32,
        modifier_lo: u32,
    ) -> Action {
        let params_id = ctx.last_sender_id;
        if ctx.used_dmabuf_params.contains(&params_id) {
            return ctx.reject(
                params_id,
                zwp_linux_buffer_params_v1::ERROR_ALREADY_USED,
                "linux-dmabuf params already used",
            );
        }
        if plane_idx > 1 {
            return ctx.reject(
                params_id,
                zwp_linux_buffer_params_v1::ERROR_PLANE_IDX,
                "dmabuf plane index out of bounds",
            );
        }
        if ctx
            .pending_params
            .get(&params_id)
            .is_some_and(|params| params.iter().any(|param| param.plane_idx == plane_idx))
        {
            return ctx.reject(
                params_id,
                zwp_linux_buffer_params_v1::ERROR_PLANE_SET,
                "dmabuf plane was already set",
            );
        }

        // A failure here is the proxy's own, such as EMFILE; dropping the
        // plane would blame the guest with an incomplete buffer later.
        let duplicate = match fd.try_clone_to_owned() {
            Ok(duplicate) => duplicate,
            Err(e) => return ctx.fail(format_args!("Failed to duplicate dma-buf fd for add: {e}")),
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
                fd: duplicate,
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
        if ctx.used_dmabuf_params.contains(&params_id) {
            return ctx.reject(
                params_id,
                zwp_linux_buffer_params_v1::ERROR_ALREADY_USED,
                "linux-dmabuf params already used",
            );
        }
        if !Self::valid_params(ctx, params_id, width, height, format) {
            error!(
                "Rejecting invalid dmabuf create: params={}, width={}, height={}, format={:#010x}",
                params_id, width, height, format
            );
            let code = Self::params_error_code(ctx, params_id, width, height, format);
            return ctx.reject(params_id, code, "invalid linux-dmabuf buffer parameters");
        }

        // The non-immediate create request reports its wl_buffer through a
        // later `created` event. Keep all resources under the host params ID:
        // unlike the guest numeric ID, it uniquely identifies this async
        // generation even after guest-side delete_id and ID reuse.
        let (host_params_id, sync_fds) = match self.process_params(ctx, params_id) {
            Ok(params) => params,
            Err(action) => return action,
        };
        ctx.client_to_host_queue
            .push(zwp_linux_buffer_params_v1::req::create(
                host_params_id.0,
                width,
                height,
                format,
                flags,
            ));
        let pending = PendingNativeCreate {
            guest_params_id: params_id,
            dimensions: (width, height),
            sync_fds,
        };
        match ctx.pending_native_creates.entry(host_params_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(pending);
                Action::Drop
            }
            std::collections::hash_map::Entry::Occupied(_) => ctx.fail(format_args!(
                "Duplicate asynchronous dmabuf params host ID {}",
                host_params_id.0
            )),
        }
    }

    fn on_created(&mut self, ctx: &mut Context, buffer: u32) -> Action {
        let host_params_id = ctx.last_sender_id;
        if let Some(guest_params_id) = ctx.shadow_table.get_guest_id(host_params_id) {
            let orphaned = ctx.shadow_table.is_pending_destroy_guest(guest_params_id);
            if orphaned {
                // params.destroy may have been queued before the compositor
                // emitted `created`, so no guest object can take the buffer:
                // destroy it, or it would live until connection teardown.
                // The host created it with a server ID, which it frees as
                // soon as it processes the destructor, so the proxy keeps
                // nothing for it.
                crate::handler::shm::queue_host_buffer_destroy(ctx, buffer);
                ctx.pending_native_creates.remove(&HostId(host_params_id));
                return Action::Drop;
            }
            if let Some(pending) = ctx.pending_native_creates.remove(&HostId(host_params_id)) {
                if pending.guest_params_id != guest_params_id {
                    return ctx.fail(format_args!(
                        "Async dmabuf params generation diverged: host {} maps to guest {}, pending guest {}",
                        host_params_id, guest_params_id, pending.guest_params_id
                    ));
                }
                // The generated dispatcher maps this host-created buffer to a
                // fresh guest server ID immediately after the handler returns.
                // Register the host generation now; compositor lookup resolves
                // the guest ID through the shadow table after forwarding.
                if !ctx.register_native_buffer(buffer, pending.dimensions, pending.sync_fds) {
                    return ctx.fail(format_args!("Duplicate render buffer host ID {buffer}"));
                }
            }
        }
        Action::Forward
    }

    fn on_failed(&mut self, ctx: &mut Context) -> Action {
        let host_params_id = ctx.last_sender_id;
        if let Some(guest_params_id) = ctx.shadow_table.get_guest_id(host_params_id) {
            if ctx.shadow_table.is_pending_destroy_guest(guest_params_id) {
                ctx.pending_native_creates.remove(&HostId(host_params_id));
                return Action::Drop;
            }
            if let Some(pending) = ctx.pending_native_creates.remove(&HostId(host_params_id)) {
                if pending.guest_params_id != guest_params_id {
                    return ctx.fail(format_args!(
                        "Async dmabuf params generation diverged: host {} maps to guest {}, pending guest {}",
                        host_params_id, guest_params_id, pending.guest_params_id
                    ));
                }
            }
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
        if ctx.used_dmabuf_params.contains(&params_id) {
            return ctx.reject(
                params_id,
                zwp_linux_buffer_params_v1::ERROR_ALREADY_USED,
                "linux-dmabuf params already used",
            );
        }
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
        if !Self::valid_params(ctx, params_id, width, height, format) {
            error!(
                "Rejecting invalid dmabuf create_immed: params={}, width={}, height={}, format={:#010x}",
                params_id, width, height, format
            );
            let code = Self::params_error_code(ctx, params_id, width, height, format);
            return ctx.reject(params_id, code, "invalid linux-dmabuf buffer parameters");
        }

        let (host_params_id, sync_fds) = match self.process_params(ctx, params_id) {
            Ok(params) => params,
            Err(action) => return action,
        };
        // The request is dropped and rebuilt here, so create the buffer the
        // generated dispatcher would have created for it.
        let buffer_version = ctx.shadow_table.guest_object_version_or_unknown(params_id);
        let host_buffer_id =
            ctx.shadow_table
                .create_paired(buffer_id, "wl_buffer", buffer_version, buffer_version);
        ctx.client_to_host_queue
            .push(zwp_linux_buffer_params_v1::req::create_immed(
                host_params_id.0,
                host_buffer_id,
                width,
                height,
                format,
                flags,
            ));
        if !ctx.register_native_buffer(host_buffer_id, (width, height), sync_fds) {
            return ctx.fail(format_args!(
                "Duplicate render buffer host ID {host_buffer_id}"
            ));
        }
        Action::Drop
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_synthetic_feedback_messages, feedback_pairs, is_supported_drm_format,
        maybe_reclaim_capability_generation, LinuxDmabufHandler, DRM_FORMAT_MOD_INVALID,
        MAX_FEEDBACK_FORMATS,
    };
    use crate::handler::display::DisplayHandler;
    use crate::handler::shm::ShmHandler;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1::{
        self, evt, ZwpLinuxBufferParamsV1Handler,
    };
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_feedback_v1 as feedback;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1Handler;
    use crate::protocols::wayland::wl_display::{self, WlDisplayHandler};
    use crate::protocols::wayland::{wl_buffer, wl_shm};
    use crate::state::{Context, HostId, PendingNativeCreate, PendingParam, SessionEnd};
    use crate::test_support::{
        any_fd_has_identity, decoded, fd_identity, msg_opcode, msg_sender, protocol_error,
        TestFdIdentity,
    };
    use crate::wire::{Action, MessageHeader, Outgoing, WireMessage, WL_DISPLAY_ID};
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};

    const XRGB8888: u32 = 0x3432_5258;

    /// A guest `zwp_linux_buffer_params_v1` and the host object it is
    /// paired with.
    #[derive(Clone, Copy)]
    struct Params {
        guest: u32,
        host: u32,
    }

    const PARAMS: Params = Params { guest: 7, host: 8 };
    /// The `wl_buffer` the guest's `create_immed` names.
    const BUFFER: u32 = 90;
    /// The `wl_buffer` the host's `created` event creates, a server ID.
    const HOST_BUFFER: u32 = 0xff00_0042;

    /// One end of a new pipe: a descriptor whose file no other test shares.
    fn pipe_fd() -> OwnedFd {
        let (reader, _writer) = std::io::pipe().expect("a pipe");
        reader.into()
    }

    /// Plane `plane_idx` at `offset` with `stride` bytes per row, on `fd`.
    fn plane_on(fd: OwnedFd, plane_idx: u32, offset: u32, stride: u32) -> PendingParam {
        PendingParam {
            fd,
            plane_idx,
            offset,
            stride,
            modifier_hi: 0,
            modifier_lo: 0,
        }
    }

    /// [`plane_on`] a new pipe.
    fn plane(plane_idx: u32, offset: u32, stride: u32) -> PendingParam {
        plane_on(pipe_fd(), plane_idx, offset, stride)
    }

    /// Pair `params` at version 4 and give it `planes`, as the sender of
    /// the next request.
    fn add_params(ctx: &mut Context, params: Params, planes: Vec<PendingParam>) {
        ctx.shadow_table.insert_paired_with_version_for_test(
            params.guest,
            params.host,
            "zwp_linux_buffer_params_v1",
            4,
        );
        ctx.pending_params.insert(params.guest, planes);
        ctx.last_sender_id = params.guest;
    }

    /// A context whose [`PARAMS`] hold `planes`.
    fn params_with(planes: Vec<PendingParam>) -> Context {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        add_params(&mut ctx, PARAMS, planes);
        ctx
    }

    /// The guest's asynchronous `create` of a 16x8 XRGB8888 buffer from
    /// `params`, with one plane.
    fn create(ctx: &mut Context, params: Params) {
        add_params(ctx, params, vec![plane(0, 0, 64)]);
        assert_eq!(
            LinuxDmabufHandler.on_create(ctx, 16, 8, XRGB8888, 0),
            Action::Drop
        );
    }

    /// The host sends params event `event`, which the proxy dispatches;
    /// returns what it forwards.
    fn params_event(ctx: &mut Context, (event, _): Outgoing) -> Option<Outgoing> {
        ctx.last_sender_id = msg_sender(&event);
        let mut event = WireMessage::new(
            ctx.last_sender_id,
            msg_opcode(&event),
            &event[MessageHeader::LEN..],
        );
        zwp_linux_buffer_params_v1::dispatch_event(&mut event, &mut LinuxDmabufHandler, ctx)
            .expect("the event decodes")
    }

    /// `forward` is the `created` of guest buffer `buffer`, from `params`.
    fn forwarded_created(forward: Option<Outgoing>, params: Params, buffer: u32) {
        let forward = forward.expect("created reaches the guest");
        assert_eq!(
            decoded(&[forward], zwp_linux_buffer_params_v1::Event::from_wire),
            [(
                params.guest,
                zwp_linux_buffer_params_v1::Event::Created { buffer }
            )]
        );
    }

    /// Give [`PARAMS`] (paired, without planes) the 16x8 asynchronous create
    /// the host still owes an answer, as `on_create` leaves it, with a fence
    /// descriptor no other test shares; returns that descriptor's identity.
    fn pending_create(ctx: &mut Context) -> TestFdIdentity {
        ctx.shadow_table.insert_paired_with_version_for_test(
            PARAMS.guest,
            PARAMS.host,
            "zwp_linux_buffer_params_v1",
            4,
        );
        let fence = pipe_fd();
        let identity = fd_identity(fence.as_raw_fd());
        let pending = PendingNativeCreate {
            guest_params_id: PARAMS.guest,
            dimensions: (16, 8),
            sync_fds: vec![fence],
        };
        ctx.pending_native_creates
            .insert(HostId(PARAMS.host), pending);
        ctx.last_sender_id = PARAMS.guest;
        identity
    }

    #[test]
    fn capability_snapshot_is_reclaimed_after_its_last_reference() {
        let generation = 9;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.dmabuf_capabilities.entry(generation).or_default();
        ctx.dmabuf_guest_generations.insert(7, generation);

        maybe_reclaim_capability_generation(&mut ctx, generation);
        assert!(
            ctx.dmabuf_capabilities.contains_key(&generation),
            "a live factory must retain its immutable capability snapshot"
        );

        ctx.dmabuf_guest_generations.remove(&7);
        maybe_reclaim_capability_generation(&mut ctx, generation);
        assert!(
            !ctx.dmabuf_capabilities.contains_key(&generation),
            "an unreferenced removed generation must not accumulate forever"
        );
    }

    #[test]
    fn synthetic_feedback_messages_are_atomic_sealed_and_protocol_ordered() {
        let pairs = [(XRGB8888, 0), (0x3432_5241, DRM_FORMAT_MOD_INVALID)];
        let device = 0x1234_5678_u64.to_ne_bytes();
        let messages =
            build_synthetic_feedback_messages(77, &pairs, &device).expect("feedback messages");

        let headers = messages
            .iter()
            .map(|(message, fds)| (msg_sender(message), msg_opcode(message), fds.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            headers,
            [
                (77, feedback::EVT_FORMAT_TABLE, 1),
                (77, feedback::EVT_MAIN_DEVICE, 0),
                (77, feedback::EVT_TRANCHE_TARGET_DEVICE, 0),
                (77, feedback::EVT_TRANCHE_FLAGS, 0),
                (77, feedback::EVT_TRANCHE_FORMATS, 0),
                (77, feedback::EVT_TRANCHE_DONE, 0),
                (77, feedback::EVT_DONE, 0),
            ]
        );

        let table_fd = messages[0].1[0].as_raw_fd();
        let expected_seals =
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
        let seals = unsafe { libc::fcntl(table_fd, libc::F_GET_SEALS) };
        assert_eq!(seals & expected_seals, expected_seals);

        // Each entry is a u32 format, 4 bytes of padding and a u64 modifier.
        let mut table = [0u8; 32];
        assert_eq!(
            unsafe { libc::pread(table_fd, table.as_mut_ptr().cast(), table.len(), 0) },
            table.len() as isize
        );
        let modifier = |at: usize| u64::from_ne_bytes(table[at..at + 8].try_into().unwrap());
        assert_eq!(
            u32::from_ne_bytes(table[..4].try_into().unwrap()),
            pairs[0].0
        );
        assert_eq!(modifier(8), pairs[0].1);
        assert_eq!(modifier(24), pairs[1].1);
    }

    #[test]
    fn large_synthetic_feedback_splits_tranche_indices_at_wire_limit() {
        let pairs = (0..MAX_FEEDBACK_FORMATS)
            .map(|index| (XRGB8888, index as u64))
            .collect::<Vec<_>>();
        let messages = build_synthetic_feedback_messages(77, &pairs, &[0; 8])
            .expect("maximum-size feedback should be representable");
        assert!(messages
            .iter()
            .all(|(message, _)| message.len() <= MessageHeader::MAX_LEN));
        // The tranche_formats events together list every table index once,
        // in order.
        let indices = decoded(&messages, feedback::Event::from_wire)
            .into_iter()
            .filter_map(|(_, event)| match event {
                feedback::Event::TrancheFormats { indices } => Some(indices),
                _ => None,
            })
            .flat_map(|indices| {
                let (indices, rest) = indices.as_chunks::<2>();
                assert!(rest.is_empty(), "uint16 indices");
                indices
                    .iter()
                    .map(|index| usize::from(u16::from_ne_bytes(*index)))
                    .collect::<Vec<_>>()
            });
        assert!(indices.eq(0..MAX_FEEDBACK_FORMATS));
    }

    /// Guest factory 7 asks for its default feedback 90.
    fn get_default_feedback(ctx: &mut Context) -> Action {
        ctx.last_sender_id = 7;
        ZwpLinuxDmabufV1Handler::on_get_default_feedback(&mut LinuxDmabufHandler, ctx, 90)
    }

    /// A context, with `gpu_accel`, whose guest factory 7 holds capability
    /// generation 42, `ready` or not.
    fn factory(gpu_accel: bool, ready: bool) -> Context {
        let mut ctx = Context::new_for_test(gpu_accel, false, vec![]);
        ctx.dmabuf_guest_generations.insert(7, 42);
        ctx.dmabuf_capabilities.entry(42).or_default().ready = ready;
        ctx
    }

    /// A factory without a capability generation is the proxy's own
    /// bookkeeping failure. `zwp_linux_dmabuf_v1` declares no errors, so the
    /// display reports it as an implementation error.
    #[test]
    fn feedback_from_an_untracked_factory_is_an_implementation_error() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        assert_eq!(get_default_feedback(&mut ctx), Action::Drop);
        assert_eq!(
            protocol_error(&ctx),
            (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION)
        );
    }

    /// Feedback the proxy cannot allocate (here, without a GBM device to
    /// name) is the compositor running out of resources, not a client
    /// error: libwayland reports that as `wl_display.no_memory`.
    #[test]
    fn unallocatable_feedback_is_a_no_memory_error() {
        let mut ctx = factory(false, true);
        assert_eq!(get_default_feedback(&mut ctx), Action::Drop);
        assert_eq!(
            protocol_error(&ctx),
            (WL_DISPLAY_ID, wl_display::ERROR_NO_MEMORY)
        );
    }

    #[test]
    fn synthetic_feedback_waits_for_capability_barrier() {
        let mut ctx = factory(true, false);
        assert_eq!(get_default_feedback(&mut ctx), Action::Drop);
        assert_eq!(ctx.synthetic_feedback_objects.get(&90), Some(&42));
        assert!(
            ctx.host_to_client_queue.is_empty(),
            "feedback must remain pending until the host capability callback"
        );
        assert!(!ctx.dmabuf_capabilities[&42].ready);
    }

    #[test]
    fn synthetic_feedback_destroy_completes_locally() {
        let mut ctx = factory(true, false);
        assert_eq!(get_default_feedback(&mut ctx), Action::Drop);
        // With its factory gone, the feedback holds the last reference to
        // the capability snapshot.
        ctx.dmabuf_guest_generations.remove(&7);

        ctx.last_sender_id = 90;
        let mut destroy = WireMessage::new(90, feedback::REQ_DESTROY, &[]);
        let dispatched =
            feedback::dispatch_request(&mut destroy, &mut LinuxDmabufHandler, &mut ctx);
        assert!(matches!(dispatched, Ok(None)), "{dispatched:?}");
        assert!(ctx.client_to_host_queue.is_empty());
        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_display::Event::from_wire),
            [(WL_DISPLAY_ID, wl_display::Event::DeleteId { id: 90 })]
        );
        assert!(!ctx.synthetic_feedback_objects.contains_key(&90));
        assert!(!ctx.dmabuf_capabilities.contains_key(&42));
    }

    #[test]
    fn capability_snapshots_remain_isolated_by_global_generation() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        for generation in [1, 2] {
            ctx.dmabuf_capabilities
                .entry(generation)
                .or_default()
                .format_modifiers
                .push((XRGB8888, generation));
        }

        assert_eq!(feedback_pairs(&ctx, 1), vec![(XRGB8888, 1)]);
        assert_eq!(feedback_pairs(&ctx, 2), vec![(XRGB8888, 2)]);
    }

    #[test]
    fn legacy_capability_events_are_hidden_from_synthetic_v4_factory() {
        let mut ctx = Context::new_for_test(true, false, vec![]);
        let capabilities = |ctx: &mut Context, host| {
            ctx.last_sender_id = host;
            [
                LinuxDmabufHandler.on_format(ctx, XRGB8888),
                LinuxDmabufHandler.on_modifier(ctx, XRGB8888, 0, 0),
            ]
        };
        ctx.shadow_table
            .insert_paired_with_version_for_test(10, 20, "zwp_linux_dmabuf_v1", 4);
        assert_eq!(capabilities(&mut ctx, 20), [Action::Drop; 2]);

        ctx.release_deleted_host_id(20);
        ctx.shadow_table
            .insert_paired_with_version_for_test(11, 21, "zwp_linux_dmabuf_v1", 3);
        assert_eq!(capabilities(&mut ctx, 21), [Action::Forward; 2]);
    }

    #[test]
    fn supported_formats_match_chromiumos_format_table() {
        for format in [
            wl_shm::FORMAT_NV12,
            0x3631_4752, // RGB565
            XRGB8888,
            0x3432_5241, // ARGB8888
            0x3432_4258, // XBGR8888
            0x3432_4241, // ABGR8888
        ] {
            assert!(is_supported_drm_format(format), "{format:#010x}");
        }
        assert!(!is_supported_drm_format(0xdead_beef));
    }

    #[test]
    fn create_immed_without_tracked_params_does_not_map_buffer() {
        let mut ctx = params_with(Vec::new());
        ctx.pending_params.remove(&PARAMS.guest);

        assert_eq!(
            LinuxDmabufHandler.on_create_immed(&mut ctx, BUFFER, 16, 16, XRGB8888, 0),
            Action::Drop
        );
        assert_eq!(ctx.shadow_table.get_host_id(BUFFER), None);
        assert!(ctx.client_to_host_queue.is_empty());
        assert!(!ctx.is_fatal());
    }

    #[test]
    fn create_immed_registers_one_complete_buffer_generation() {
        let fd = pipe_fd();
        let added = fd.try_clone().expect("duplicate the plane");
        let mut ctx = params_with(vec![plane_on(fd, 0, 0, 64)]);

        assert_eq!(
            LinuxDmabufHandler.on_create_immed(&mut ctx, BUFFER, 16, 8, XRGB8888, 0),
            Action::Drop
        );

        let host_buffer = ctx.shadow_table.get_host_id(BUFFER).expect("a host buffer");
        assert_ne!(host_buffer, BUFFER);
        assert_eq!(
            decoded(
                &ctx.client_to_host_queue,
                zwp_linux_buffer_params_v1::Request::from_wire
            ),
            [
                (
                    PARAMS.host,
                    zwp_linux_buffer_params_v1::Request::Add {
                        fd: added,
                        plane_idx: 0,
                        offset: 0,
                        stride: 64,
                        modifier_hi: 0,
                        modifier_lo: 0,
                    }
                ),
                (
                    PARAMS.host,
                    zwp_linux_buffer_params_v1::Request::CreateImmed {
                        buffer_id: host_buffer,
                        width: 16,
                        height: 8,
                        format: XRGB8888,
                        flags: 0,
                    }
                ),
            ]
        );
        assert_eq!(ctx.buffer_dimensions(BUFFER), Some((16, 8)));
        assert_eq!(
            ctx.native_buffer_sync_fds(BUFFER)
                .map(|sync_fds| sync_fds.len()),
            Some(1)
        );
        assert!(ctx.has_render_buffer_host(HostId(host_buffer)));
        assert!(ctx.used_dmabuf_params.contains(&PARAMS.guest));
    }

    #[test]
    fn reused_params_report_already_used_without_mapping_a_buffer() {
        let mut ctx = params_with(Vec::new());
        ctx.pending_params.remove(&PARAMS.guest);
        ctx.used_dmabuf_params.insert(PARAMS.guest);

        assert_eq!(
            LinuxDmabufHandler.on_create_immed(&mut ctx, BUFFER, 16, 8, XRGB8888, 0),
            Action::Drop
        );

        assert_eq!(
            protocol_error(&ctx),
            (PARAMS.guest, zwp_linux_buffer_params_v1::ERROR_ALREADY_USED)
        );
        assert_eq!(ctx.shadow_table.get_host_id(BUFFER), None);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    /// A plane the proxy cannot duplicate, here because the process may
    /// open no more descriptors, ends the session as the proxy's own
    /// failure. The descriptor limit is the whole process's, so the test
    /// lowers it only in a child process that runs it alone.
    #[test]
    fn a_plane_the_proxy_cannot_duplicate_is_its_own_failure() {
        crate::test_support::in_own_process(
            "handler::linux_dmabuf::tests::a_plane_the_proxy_cannot_duplicate_is_its_own_failure",
            a_plane_the_proxy_cannot_duplicate,
        );
    }

    fn a_plane_the_proxy_cannot_duplicate() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.pending_params.insert(7, Vec::new());
        ctx.last_sender_id = 7;
        let plane = crate::test_support::shm_pool_file(&[]);
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        // A duplicate takes the lowest free number from 3 up, which must be
        // below the soft limit.
        let exhausted = libc::rlimit {
            rlim_cur: limit.rlim_max.min(3),
            ..limit
        };
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &exhausted) },
            0
        );
        let action = LinuxDmabufHandler.on_add(&mut ctx, plane.as_fd(), 0, 0, 64, 0, 0);
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);

        assert_eq!(action, Action::Drop);
        assert_eq!(ctx.session_end(), Some(&SessionEnd::Internal));
        assert!(ctx.pending_params[&7].is_empty());
    }

    /// An `add` or `create` the params cannot take ends the session with
    /// the params' declared error, and nothing reaches the host.
    #[test]
    fn invalid_plane_requests_queue_declared_protocol_errors() {
        use zwp_linux_buffer_params_v1::{
            ERROR_INCOMPLETE, ERROR_INVALID_DIMENSIONS, ERROR_OUT_OF_BOUNDS, ERROR_PLANE_IDX,
            ERROR_PLANE_SET,
        };
        enum Request {
            Add {
                plane_idx: u32,
            },
            Create {
                width: i32,
                height: i32,
                format: u32,
            },
        }
        let cases: [(&str, Vec<PendingParam>, Request, u32); 6] = [
            (
                "an add past the last plane",
                vec![],
                Request::Add { plane_idx: 2 },
                ERROR_PLANE_IDX,
            ),
            (
                "an add of a plane already set",
                vec![plane(0, 0, 64)],
                Request::Add { plane_idx: 0 },
                ERROR_PLANE_SET,
            ),
            (
                "a zero width",
                vec![plane(0, 0, 64)],
                Request::Create {
                    width: 0,
                    height: 16,
                    format: XRGB8888,
                },
                ERROR_INVALID_DIMENSIONS,
            ),
            (
                "two planes for a one-plane format",
                vec![plane(0, 0, 64), plane(0, 4096, 64)],
                Request::Create {
                    width: 16,
                    height: 16,
                    format: XRGB8888,
                },
                ERROR_INCOMPLETE,
            ),
            (
                "plane 0 twice for a two-plane format",
                vec![plane(0, 0, 16), plane(0, 128, 16)],
                Request::Create {
                    width: 16,
                    height: 8,
                    format: wl_shm::FORMAT_NV12,
                },
                ERROR_PLANE_SET,
            ),
            (
                // XRGB8888 needs four bytes per pixel.
                "a stride shorter than one row",
                vec![plane(0, 0, 4 * 3)],
                Request::Create {
                    width: 4,
                    height: 4,
                    format: XRGB8888,
                },
                ERROR_OUT_OF_BOUNDS,
            ),
        ];
        for (case, planes, request, code) in cases {
            let mut ctx = params_with(planes);
            let action = match request {
                Request::Add { plane_idx } => {
                    let fd = pipe_fd();
                    LinuxDmabufHandler.on_add(&mut ctx, fd.as_fd(), plane_idx, 0, 64, 0, 0)
                }
                Request::Create {
                    width,
                    height,
                    format,
                } => LinuxDmabufHandler.on_create(&mut ctx, width, height, format, 0),
            };
            assert_eq!(action, Action::Drop, "{case}");
            assert_eq!(protocol_error(&ctx), (PARAMS.guest, code), "{case}");
            assert!(ctx.client_to_host_queue.is_empty(), "{case}");
        }
    }

    #[test]
    fn async_dmabuf_created_event_moves_dimensions_to_host_buffer() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        create(&mut ctx, PARAMS);
        let pending = &ctx.pending_native_creates[&HostId(PARAMS.host)];
        assert_eq!(
            pending.dimensions,
            (16, 8),
            "async create must retain dimensions until the host emits created"
        );
        assert_eq!(
            pending.sync_fds.len(),
            1,
            "async create must retain a dma-buf fence descriptor until created"
        );
        assert!(!ctx.has_render_buffer_host(HostId(HOST_BUFFER)));

        let forward = params_event(&mut ctx, evt::created(PARAMS.host, HOST_BUFFER));
        let guest_buffer = ctx
            .shadow_table
            .get_guest_id(HOST_BUFFER)
            .expect("generated created event must map the host buffer");
        forwarded_created(forward, PARAMS, guest_buffer);
        assert!(ctx.pending_native_creates.is_empty());
        assert_eq!(
            ctx.buffer_dimensions_for_host(HostId(HOST_BUFFER)),
            Some((16, 8))
        );
        assert!(
            ctx.native_buffer_sync_fds(guest_buffer)
                .is_some_and(|sync_fds| !sync_fds.is_empty()),
            "created must move the retained fence descriptor to the host buffer"
        );
        assert_eq!(
            ctx.buffer_dimensions(guest_buffer),
            Some((16, 8)),
            "guest lookup must resolve the canonical host-ID keyed generation"
        );
        assert_eq!(ctx.render_buffer_count(), 1);
    }

    /// Every plane of a multi-plane create keeps a fence descriptor of its
    /// own, whether the planes are distinct dma-bufs or descriptors of one.
    #[test]
    fn multi_plane_create_retains_a_fence_for_every_plane() {
        for shared in [false, true] {
            let first = pipe_fd();
            let second = if shared {
                first.try_clone().expect("duplicate the plane")
            } else {
                pipe_fd()
            };
            let mut ctx = params_with(vec![
                plane_on(first, 0, 0, 16),
                plane_on(second, 1, 128, 16),
            ]);
            assert_eq!(
                LinuxDmabufHandler.on_create(&mut ctx, 16, 8, wl_shm::FORMAT_NV12, 0),
                Action::Drop
            );
            assert_eq!(
                ctx.pending_native_creates[&HostId(PARAMS.host)]
                    .sync_fds
                    .len(),
                2,
                "shared backing: {shared}"
            );
        }
    }

    #[test]
    fn async_created_buffer_destroy_allows_ordered_host_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        create(&mut ctx, PARAMS);
        let forward = params_event(&mut ctx, evt::created(PARAMS.host, HOST_BUFFER));
        let first_guest_buffer = ctx
            .shadow_table
            .get_guest_id(HOST_BUFFER)
            .expect("created must allocate a guest server ID");
        forwarded_created(forward, PARAMS, first_guest_buffer);
        assert!(ctx.shadow_table.is_guest_server_id(first_guest_buffer));
        assert!(ctx.has_render_buffer_host(HostId(HOST_BUFFER)));

        ctx.client_to_host_queue.clear();
        ctx.last_sender_id = first_guest_buffer;
        let mut destroy = WireMessage::new(first_guest_buffer, wl_buffer::REQ_DESTROY, &[]);
        let dispatched = wl_buffer::dispatch_request(&mut destroy, &mut ShmHandler, &mut ctx);
        assert!(matches!(dispatched, Ok(None)), "{dispatched:?}");
        assert_eq!(ctx.shadow_table.get_host_id(first_guest_buffer), None);
        assert_eq!(ctx.shadow_table.get_guest_id(HOST_BUFFER), None);
        assert!(!ctx.has_render_buffer_host(HostId(HOST_BUFFER)));
        assert!(ctx.shadow_table.is_host_id_available(HOST_BUFFER));
        assert!(!ctx
            .shadow_table
            .is_pending_destroy_guest(first_guest_buffer));
        assert!(!ctx.shadow_table.is_retired_host(HOST_BUFFER));
        assert!(ctx.host_to_client_queue.is_empty());
        assert_eq!(
            decoded(&ctx.client_to_host_queue, wl_buffer::Request::from_wire),
            [(HOST_BUFFER, wl_buffer::Request::Destroy {})],
            "destroy must queue exactly one host wl_buffer destructor"
        );

        let next_params = Params { guest: 9, host: 10 };
        create(&mut ctx, next_params);
        let reused = params_event(&mut ctx, evt::created(next_params.host, HOST_BUFFER));
        let second_guest_buffer = ctx
            .shadow_table
            .get_guest_id(HOST_BUFFER)
            .expect("reused host buffer ID must map to a new guest server ID");
        forwarded_created(reused, next_params, second_guest_buffer);
        assert_eq!(
            second_guest_buffer, first_guest_buffer,
            "a destroyed server-created buffer's guest ID is reused first"
        );
        assert!(ctx.shadow_table.is_guest_server_id(second_guest_buffer));
        assert!(ctx.has_render_buffer_host(HostId(HOST_BUFFER)));
        assert!(!ctx.is_fatal());
    }

    #[test]
    fn async_dmabuf_failed_event_discards_pending_dimensions() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        create(&mut ctx, PARAMS);
        assert!(ctx
            .pending_native_creates
            .contains_key(&HostId(PARAMS.host)));

        let forward = params_event(&mut ctx, evt::failed(PARAMS.host));
        assert_eq!(
            decoded(
                &[forward.expect("failed must still be forwarded to the guest")],
                zwp_linux_buffer_params_v1::Event::from_wire
            ),
            [(PARAMS.guest, zwp_linux_buffer_params_v1::Event::Failed {})]
        );
        assert!(ctx.pending_native_creates.is_empty());
        assert_eq!(ctx.render_buffer_count(), 0);
    }

    #[test]
    fn orphaned_async_created_buffer_is_destroyed_locally() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        pending_create(&mut ctx);
        // This is what the generated params.destroy dispatcher records after
        // forwarding a destroy request, before the delayed created event can
        // arrive from the host.
        ctx.guest_destroyed(PARAMS.guest);

        let forward = params_event(&mut ctx, evt::created(PARAMS.host, HOST_BUFFER));
        assert!(
            forward.is_none(),
            "orphaned buffer must not be exposed to guest"
        );
        assert!(ctx.pending_native_creates.is_empty());
        assert!(!ctx.has_render_buffer_host(HostId(HOST_BUFFER)));
        assert_eq!(
            decoded(&ctx.client_to_host_queue, wl_buffer::Request::from_wire),
            [(HOST_BUFFER, wl_buffer::Request::Destroy {})]
        );
        assert!(
            ctx.shadow_table.accepts_host_new_id(HOST_BUFFER),
            "the host frees a server ID once the object is destroyed"
        );
    }

    #[test]
    fn params_delete_id_ends_an_orphaned_async_create() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let fence = pending_create(&mut ctx);
        ctx.guest_destroyed(PARAMS.guest);
        assert!(ctx.pending_native_creates.is_empty());
        assert!(
            !any_fd_has_identity(fence),
            "destroying the params must close its generation's synchronization descriptor"
        );
        // The generated dispatcher records the guest destructor after the
        // handler returns.
        ctx.guest_destroyed(PARAMS.guest);
        assert!(ctx.shadow_table.is_pending_destroy_guest(PARAMS.guest));

        // The host destroyed the params object before emitting created or
        // failed, so it can no longer emit either: nothing waits for them,
        // and the host ID is free for the next object.
        ctx.last_sender_id = WL_DISPLAY_ID;
        assert_eq!(
            WlDisplayHandler::on_delete_id(&mut DisplayHandler, &mut ctx, PARAMS.host),
            Action::Drop
        );
        assert!(!ctx.shadow_table.is_pending_destroy_guest(PARAMS.guest));
        assert_eq!(ctx.shadow_table.get_guest_id(PARAMS.host), None);
        assert_eq!(ctx.shadow_table.get_host_interface(PARAMS.host), None);
        assert!(ctx.shadow_table.is_host_id_available(PARAMS.host));
        assert_eq!(
            decoded(&ctx.host_to_client_queue, wl_display::Event::from_wire),
            [(
                WL_DISPLAY_ID,
                wl_display::Event::DeleteId { id: PARAMS.guest }
            )],
            "params delete_id must still reach the guest"
        );
    }
}
