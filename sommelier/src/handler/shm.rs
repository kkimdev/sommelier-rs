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

//! Guest `wl_shm`, which the proxy implements itself, and the lifetime of
//! every guest `wl_buffer`; see "Buffers" in `docs/DESIGN.md`. Each concern
//! is a submodule.

mod copy;
mod damage;
mod dmabuf;
mod dmabuf_layout;
mod formats;

pub(crate) use self::copy::{copy_shm_damage, ShmCopyError, ShmCopyLayout};
pub(crate) use self::damage::{coalesce_damage_rects, damage_reaches_buffer};
pub(crate) use self::formats::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, record_host_shm_drm_format,
    register_guest_shm,
};

use self::dmabuf::{allocate_virtwl_dmabuf, map_dmabuf, DmabufAllocation};
use self::dmabuf_layout::DmabufLayout;
use self::formats::{
    format_bytes_per_pixel, guest_shm_format_available, record_host_shm_wl_format,
    supported_shm_format,
};
use crate::protocols;
use crate::protocols::linux_dmabuf_v1::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use crate::protocols::wayland::{wl_buffer, wl_shm, wl_shm_pool};
use crate::state::{BufferState, Context, HostId, ShmPool, StalePixels};
use crate::wire::Action;
use log::{debug, error, warn};
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, BorrowedFd};
use std::ptr;
use std::sync::Arc;

pub struct ShmHandler;

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
/// The host storage chosen for one guest SHM buffer.
enum ShmBacking {
    /// Shared memory (VirtWL, or a memfd for a local compositor) laid out
    /// exactly like the guest buffer (offset 0, the guest stride and size).
    /// The host imports it as a `wl_shm` pool.
    Shared(OwnedFd),
    /// A dma-buf the host imports through the internal `zwp_linux_dmabuf_v1`.
    Dmabuf {
        host_dmabuf_id: u32,
        allocation: DmabufAllocation,
    },
}

/// Reject a `wl_buffer` creation the proxy has no resources for.
///
/// `create_buffer` carries a client supplied `new_id`; silently dropping the
/// request leaves the client believing that the ID was created while the
/// proxy has no object to service it.  Wayland errors terminate the connection,
/// which is the only safe outcome once allocation has failed.
#[must_use = "a handler returns the Action::Drop"]
fn reject_buffer_creation(ctx: &mut Context, object_id: u32, message: &'static str) -> Action {
    ctx.reject(object_id, wl_shm::ERROR_INVALID_FD, message)
}

fn queue_host_params_destroy(ctx: &mut Context, params_id: u32) {
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::destroy(params_id));
    // Keep the numeric ID reserved until the host's wl_display.delete_id
    // acknowledgement arrives. The destroy request is ordered after all
    // already-queued params requests.
    ctx.retire_destroyed_host_object(params_id);
}

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
    current: HashSet<u32>,
    pending: HashSet<u32>,
}

impl SurfaceBufferReferences {
    fn collect(ctx: &Context) -> Self {
        let mut current = HashSet::new();
        let mut pending = HashSet::new();
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

impl protocols::wayland::wl_shm::WlShmHandler for ShmHandler {
    fn on_format(&mut self, ctx: &mut Context, format: u32) -> Action {
        if ctx.host_shm_id == Some(ctx.last_sender_id) {
            record_host_shm_wl_format(ctx, format);
            return Action::Drop;
        }
        Action::Forward
    }

    fn on_create_pool(
        &mut self,
        ctx: &mut Context,
        id: u32,
        fd: BorrowedFd<'_>,
        size: i32,
    ) -> Action {
        if ctx.stale_shm_guest_objects.contains(&ctx.last_sender_id) {
            debug!(
                "Ignoring wl_shm.create_pool from removed host global object {}",
                ctx.last_sender_id
            );
            // The guest owns the new ID, so the pool exists, inert.
            ctx.shadow_table.create_local(id, wl_shm_pool::NAME, 1);
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
        ctx.shadow_table.create_local(pool_id, wl_shm_pool::NAME, 1);

        Action::Drop
    }
}

/// Make the guest `wl_shm` objects bound from host global `name`, which
/// retired, and their pools inert (see `Context::stale_shm_guest_objects`),
/// whether or not the proxy's own binding was `name`.
pub(crate) fn retire_guest_shms(ctx: &mut Context, name: u32) {
    let stale: std::collections::HashSet<u32> = ctx
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

/// A `wl_shm_pool.create_buffer` request that passed protocol validation.
struct ShmBufferRequest {
    id: u32,
    pool_id: u32,
    pool_file: Arc<File>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
    /// The buffer's size in bytes, as [`valid_buffer_layout`] returned it.
    size: i32,
}

/// Where the planes of a guest SHM buffer's host copy lie.
struct HostCopyPlanes {
    /// Plane 0's stride.
    stride: u32,
    /// Plane 1's offset from plane 0, or 0 for a single-plane format.
    plane1_offset: usize,
    /// Plane 1's stride, or 0 for a single-plane format.
    plane1_stride: usize,
}

impl HostCopyPlanes {
    /// A host copy laid out exactly like the guest buffer of `request`, as
    /// shared memory is: NV12's plane 1 follows plane 0's rows.
    fn like_guest(request: &ShmBufferRequest) -> Self {
        let stride = request.stride as usize;
        let (plane1_offset, plane1_stride) = if request.format == wl_shm::FORMAT_NV12 {
            ((request.height as usize) * stride, stride)
        } else {
            (0, 0)
        };
        Self {
            stride: request.stride as u32,
            plane1_offset,
            plane1_stride,
        }
    }
}

impl From<DmabufLayout> for HostCopyPlanes {
    /// A host copy laid out as the dma-buf allocator returned it.
    fn from(layout: DmabufLayout) -> Self {
        Self {
            stride: layout.plane0().stride,
            plane1_offset: layout.plane1_relative_offset(),
            plane1_stride: layout.plane1_stride(),
        }
    }
}

impl ShmBufferRequest {
    /// The state of this buffer, whose host copy is `dest_fd`, mapped at
    /// `dest_ptr` for `dest_size` bytes, which the state unmaps when it is
    /// dropped, with its planes at `planes`. The host copy lacks every
    /// pixel so far.
    fn buffer_state(
        &self,
        dest_fd: OwnedFd,
        (dest_ptr, dest_size): (*mut u8, usize),
        planes: HostCopyPlanes,
        dmabuf_sync: bool,
    ) -> BufferState {
        BufferState {
            pool_file: Arc::clone(&self.pool_file),
            offset: self.offset,
            width: self.width,
            height: self.height,
            stride: self.stride as u32,
            format: self.format,
            dmabuf_fd: Some(dest_fd),
            dest_stride: planes.stride,
            dmabuf_plane1_offset: planes.plane1_offset,
            dmabuf_plane1_stride: planes.plane1_stride,
            dmabuf_sync,
            dest_ptr,
            dest_size,
            stale: StalePixels::all(),
        }
    }
}

/// Choose and allocate the host storage for a guest SHM buffer.
///
/// # Returns
///
/// The rejection the handler returns when no storage is available.
fn select_shm_backing(ctx: &mut Context, request: &ShmBufferRequest) -> Result<ShmBacking, Action> {
    let ShmBufferRequest {
        pool_id,
        width,
        height,
        format,
        size: buffer_size,
        ..
    } = *request;
    debug!(
        "VirtWayland channel present={}",
        ctx.virtwayland_channel.is_some()
    );

    // `--gpu-accel` changes only the output allocation.  Guest wl_shm is
    // still accepted for applications that do not expose linux-dmabuf;
    // those buffers use the VirtWL shared-memory allocation below.
    let host_dmabuf_id = ctx.host_dmabuf_id.filter(|_| ctx.gpu_accel);
    let virtwl_dmabuf = host_dmabuf_id.and_then(|host_dmabuf_id| {
        match allocate_virtwl_dmabuf(ctx, width, height, format) {
            Ok(allocation) => {
                debug!(
                    "Using VirtWL linux-dmabuf allocation: {}x{} format={:#010x} stride={} modifier={:#x}",
                    width,
                    height,
                    format,
                    allocation.layout.plane0().stride,
                    allocation.modifier
                );
                Some(ShmBacking::Dmabuf {
                    host_dmabuf_id,
                    allocation,
                })
            }
            Err(error) => {
                debug!("VirtWL dma-buf allocation unavailable: {}", error);
                None
            }
        }
    });

    let backing = if let Some(backing) = virtwl_dmabuf {
        backing
    } else if let Some(channel) = &ctx.virtwayland_channel {
        debug!(
            "Allocating VirtWayland shared-memory buffer: size={}",
            buffer_size
        );
        // `valid_buffer_layout` returns a positive size.
        let (fd, _alloc_size) = match channel.allocate(buffer_size.unsigned_abs()) {
            Ok(result) => result,
            Err(error) => {
                error!(
                    "Failed to allocate VirtWayland shared-memory buffer: {}",
                    error
                );
                return Err(reject_buffer_creation(
                    ctx,
                    pool_id,
                    "unable to allocate wl_shm buffer backing",
                ));
            }
        };
        debug!(
            "Using VirtWL shared-memory allocation: {}x{} format={:#010x} size={}",
            width, height, format, buffer_size
        );
        ShmBacking::Shared(fd)
    } else {
        // Without VirtWL the host is a local compositor (`--local-compositor`),
        // which maps ordinary shared memory as a wl_shm pool, with or
        // without `--gpu-accel`.
        match local_shared_memory(buffer_size.unsigned_abs()) {
            Ok(fd) => {
                debug!(
                    "Using local shared-memory allocation: {}x{} format={:#010x} size={}",
                    width, height, format, buffer_size
                );
                ShmBacking::Shared(fd)
            }
            Err(error) => {
                error!("Failed to allocate local shared memory: {}", error);
                return Err(reject_buffer_creation(
                    ctx,
                    pool_id,
                    "unable to allocate wl_shm buffer backing",
                ));
            }
        }
    };
    Ok(backing)
}

/// A memfd of `size` bytes, the host storage for a guest SHM buffer when the
/// host is a local compositor that maps it directly.
fn local_shared_memory(size: u32) -> io::Result<OwnedFd> {
    let fd = nix::sys::memfd::memfd_create(
        c"sommelier-shm-buffer",
        nix::sys::memfd::MFdFlags::MFD_CLOEXEC,
    )?;
    nix::unistd::ftruncate(&fd, size.into())?;
    Ok(fd)
}

/// Create a guest SHM buffer backed by shared memory ([`ShmBacking::Shared`]),
/// which the host imports as a temporary `wl_shm` pool.
fn create_shared_buffer(
    ctx: &mut Context,
    request: ShmBufferRequest,
    shared_fd: OwnedFd,
) -> Action {
    let ShmBufferRequest {
        pool_id,
        width,
        height,
        stride,
        format,
        size: buffer_size,
        ..
    } = request;
    // Check the capability before mapping the destination so a
    // disappearing wl_shm global cannot leak a raw mmap that has
    // no BufferState owner.
    let Some(host_wl_shm_id) = ctx.host_shm_id else {
        error!("wl_shm not available on host");
        return reject_buffer_creation(ctx, pool_id, "host wl_shm is unavailable");
    };
    let fd_to_send = match shared_fd.try_clone() {
        Ok(fd) => fd,
        Err(e) => {
            error!("Failed to dup FD: {}", e);
            return reject_buffer_creation(
                ctx,
                pool_id,
                "unable to duplicate wl_shm buffer backing",
            );
        }
    };
    debug!(
        "Dup for send={} size={}",
        fd_to_send.as_raw_fd(),
        buffer_size
    );

    // Map the buffer for SHM synchronization. Both VirtWL allocations
    // and memfds are ordinary shared-memory fds and can be mapped directly.
    let dest_size = buffer_size.unsigned_abs() as usize;
    let dest_ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            dest_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            shared_fd.as_raw_fd(),
            0,
        ) as *mut u8
    };
    if dest_ptr as *mut libc::c_void == libc::MAP_FAILED {
        error!(
            "Failed to mmap shared SHM buffer: {:?}",
            std::io::Error::last_os_error()
        );
        return reject_buffer_creation(ctx, pool_id, "unable to map wl_shm buffer backing");
    }

    // The temporary host wl_shm_pool remains live until the queued
    // destroy request is processed by the compositor, although it has no
    // guest-side object or event callbacks. A new object takes its
    // parent's version.
    let pool_version = ctx
        .shadow_table
        .host_object_version_or_unknown(host_wl_shm_id);
    let host_pool_id = ctx
        .shadow_table
        .create_host_only(wl_shm_pool::NAME, pool_version);

    let host_buffer_id = ctx
        .shadow_table
        .create_paired(request.id, wl_buffer::NAME, 1, 1);
    ctx.client_to_host_queue.extend([
        wl_shm::req::create_pool(host_wl_shm_id, host_pool_id, fd_to_send, buffer_size),
        // The buffer starts the pool, and keeps its SHM (not DRM) format.
        wl_shm_pool::req::create_buffer(
            host_pool_id,
            host_buffer_id,
            0,
            width,
            height,
            stride,
            format,
        ),
        wl_shm_pool::req::destroy(host_pool_id),
    ]);

    // The three requests are emitted in one ordered stream:
    // create_pool, create_buffer, destroy. Once queued, a later
    // allocation may safely reuse this temporary ID because the
    // host will process the destroy before the later request.
    // Keeping the registration forever would leak one host ID per
    // SHM buffer.
    ctx.retire_destroyed_host_object(host_pool_id);

    let planes = HostCopyPlanes::like_guest(&request);
    let state = request.buffer_state(shared_fd, (dest_ptr, dest_size), planes, false);
    register_shm_buffer(ctx, &request, host_buffer_id, state)
}

/// Create a guest SHM buffer backed by a dma-buf, which the host imports
/// through its `zwp_linux_dmabuf_v1` factory.
fn create_dmabuf_buffer(
    ctx: &mut Context,
    request: ShmBufferRequest,
    host_dmabuf_id: u32,
    allocation: DmabufAllocation,
) -> Action {
    let ShmBufferRequest {
        pool_id,
        width,
        height,
        format,
        ..
    } = request;
    let DmabufAllocation {
        fd: dmabuf_fd_owned,
        layout,
        modifier,
        dmabuf_sync,
    } = allocation;

    // Duplicate the descriptors the ADDs hand to the host before mapping
    // or queueing anything, so a failure has nothing to undo.
    let fd_to_send = match dmabuf_fd_owned.try_clone() {
        Ok(fd) => fd,
        Err(error) => {
            error!("Failed to duplicate VirtWL dma-buf fd: {}", error);
            return reject_buffer_creation(
                ctx,
                pool_id,
                "unable to duplicate wl_shm dma-buf backing",
            );
        }
    };
    let plane1_add = if let Some(plane1) = layout.plane1() {
        match dmabuf_fd_owned.try_clone() {
            Ok(fd) => Some((plane1, fd)),
            Err(error) => {
                error!("Failed to duplicate NV12 dma-buf fd: {}", error);
                return reject_buffer_creation(
                    ctx,
                    pool_id,
                    "unable to duplicate wl_shm dma-buf plane",
                );
            }
        }
    } else {
        None
    };

    let Some((dest_ptr, dest_size)) = map_dmabuf(dmabuf_fd_owned.as_raw_fd(), layout) else {
        error!(
            "dma-buf cannot be safely mmaped (offset={}, size={})",
            layout.plane0().offset,
            layout.span()
        );
        return reject_buffer_creation(ctx, pool_id, "unable to map wl_shm dma-buf backing");
    };

    // A new object takes its parent's version.
    let params_version = ctx
        .shadow_table
        .host_object_version_or_unknown(host_dmabuf_id);
    let params_id = ctx
        .shadow_table
        .create_host_only(zwp_linux_buffer_params_v1::NAME, params_version);
    ctx.client_to_host_queue
        .push(zwp_linux_dmabuf_v1::req::create_params(
            host_dmabuf_id,
            params_id,
        ));
    let (modifier_hi, modifier_lo) = ((modifier >> 32) as u32, modifier as u32);
    let plane0 = layout.plane0();
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::add(
            params_id,
            fd_to_send,
            0,
            plane0.offset,
            plane0.stride,
            modifier_hi,
            modifier_lo,
        ));
    if let Some((plane1, plane1_fd)) = plane1_add {
        ctx.client_to_host_queue
            .push(zwp_linux_buffer_params_v1::req::add(
                params_id,
                plane1_fd,
                1,
                plane1.offset,
                plane1.stride,
                modifier_hi,
                modifier_lo,
            ));
    }

    let host_buffer_id = ctx
        .shadow_table
        .create_paired(request.id, wl_buffer::NAME, 1, 1);
    ctx.client_to_host_queue
        .push(zwp_linux_buffer_params_v1::req::create_immed(
            params_id,
            host_buffer_id,
            width,
            height,
            ShmHandler::wl_shm_format_to_drm_format(format),
            0,
        ));
    // create_immed is processed before this destructor in the ordered
    // host stream. Reserve the params ID until wl_display.delete_id
    // acknowledges its destruction.
    queue_host_params_destroy(ctx, params_id);

    let state = request.buffer_state(
        dmabuf_fd_owned,
        (dest_ptr, dest_size),
        HostCopyPlanes::from(layout),
        dmabuf_sync,
    );
    register_shm_buffer(ctx, &request, host_buffer_id, state)
}

/// Register the storage of the guest `wl_buffer` paired with host buffer
/// `host_buffer_id`. The proxy just allocated that ID, so render state
/// already held for it is the proxy's own divergence, as for a native
/// buffer, not the guest's error.
fn register_shm_buffer(
    ctx: &mut Context,
    request: &ShmBufferRequest,
    host_buffer_id: u32,
    state: BufferState,
) -> Action {
    if !ctx.register_local_buffer(request.id, host_buffer_id, state) {
        return ctx.fail(format_args!(
            "Duplicate render buffer host ID {host_buffer_id}"
        ));
    }
    Action::Drop
}

impl protocols::wayland::wl_shm_pool::WlShmPoolHandler for ShmHandler {
    fn on_create_buffer(
        &mut self,
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
            ctx.shadow_table.create_local(id, wl_buffer::NAME, 1);
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

    fn on_destroy(&mut self, _ctx: &mut Context) -> Action {
        // The generated local-only destructor path ends the synthetic pool
        // (see `forget_pool`) and queues its guest delete_id.
        Action::Drop
    }

    fn on_resize(&mut self, ctx: &mut Context, size: i32) -> Action {
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
    use super::dmabuf_layout::validate_dmabuf_layout;
    use super::ShmHandler;
    use super::{
        copy_shm_damage, record_host_shm_wl_format, register_guest_shm, regular_file_has_size,
        retire_eligible_buffers, valid_buffer_layout, valid_pool_resize, valid_pool_size,
        valid_shm_stride, ShmCopyLayout,
    };
    use super::{create_dmabuf_buffer, create_shared_buffer, DmabufAllocation, ShmBufferRequest};
    use crate::handler::registry::RegistryHandler;
    use crate::handler::test_support::{
        buffer_host_destroy_is_queued, buffer_is_guest_destroyed, mark_test_buffer_guest_destroyed,
    };
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::wayland::wl_buffer::WlBufferHandler;
    use crate::protocols::wayland::wl_registry::WlRegistryHandler;
    use crate::protocols::wayland::wl_shm::WlShmHandler;
    use crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler;
    use crate::protocols::wayland::{
        wl_buffer, wl_display, wl_registry, wl_shm, wl_shm_pool, wl_surface,
    };
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::Session;
    use crate::state::DamageRegion;
    use crate::state::{Context, HostId, ShmPool};
    use crate::test_support::{
        decode, decoded, protocol_error_code, queued_headers, raw_dispatch, register_test_buffer,
        shm_pool_file, BufferKind, BUFFER_KINDS,
    };
    use crate::wire::{Action, WireMessage, WL_DISPLAY_ID};
    use std::fs::File;
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;
    use std::sync::Arc;

    fn register_test_server_native(ctx: &mut Context, host_id: u32) -> u32 {
        let guest_id = ctx.shadow_table.adopt_host_created(host_id, "wl_buffer", 1);
        assert!(ctx.register_native_buffer(host_id, (1, 1), Vec::new()));
        guest_id
    }

    /// A session whose guest has bound `wl_shm`, which the proxy
    /// implements.
    async fn session_with_shm() -> (Session, u32) {
        let mut s = Session::new();
        let shm = s.bind("wl_shm", wl_shm::VERSION).await;
        (s, shm)
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

    /// A buffer whose host copy is shared memory, which the host imports
    /// as a `wl_shm` pool of the guest buffer's layout, copies each plane
    /// where that layout puts it: NV12's plane 1 right after plane 0's
    /// rows.
    #[test]
    fn a_shared_host_copy_takes_each_plane_where_the_guest_layout_puts_it() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.host_shm_id = Some(ctx.shadow_table.create_host_only(wl_shm::NAME, 1));
        let host_copy = shm_pool_file(&[0; 12]);
        let request = ShmBufferRequest {
            id: 30,
            pool_id: 20,
            // The 2x2 Y plane, then the one row of the UV plane, each row
            // padded to 4 bytes.
            pool_file: Arc::new(shm_pool_file(&[
                1, 2, 0xee, 0xee, 3, 4, 0xee, 0xee, 5, 6, 0xee, 0xee,
            ])),
            offset: 0,
            width: 2,
            height: 2,
            stride: 4,
            format: wl_shm::FORMAT_NV12,
            size: 12,
        };

        assert_eq!(
            create_shared_buffer(&mut ctx, request, host_copy.try_clone().unwrap().into()),
            Action::Drop
        );
        assert!(!ctx.is_fatal());
        let buffer = ctx.local_buffer(30).expect("the buffer's state");
        let layout = ShmCopyLayout::for_buffer(buffer).expect("a copy layout");
        copy_shm_damage(
            &buffer.pool_file,
            buffer.dest_ptr,
            layout,
            &DamageRegion::Full,
        )
        .unwrap();
        let mut pixels = [0; 12];
        host_copy.read_exact_at(&mut pixels, 0).unwrap();
        assert_eq!(pixels, [1, 2, 0, 0, 3, 4, 0, 0, 5, 6, 0, 0]);
    }

    /// A buffer whose host copy is a dma-buf tells the host the
    /// allocator's layout, and copies each plane where that layout puts it.
    /// Plane 1 of this NV12 buffer starts after a gap past plane 0, with
    /// rows of another stride. A memfd stands in for the VirtWL dma-buf,
    /// which `map_dmabuf` maps the same way.
    #[test]
    fn a_dmabuf_host_copy_takes_each_plane_where_its_layout_puts_it() {
        use crate::protocols::linux_dmabuf_v1::zwp_linux_buffer_params_v1;
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let host_dmabuf = ctx
            .shadow_table
            .create_host_only(zwp_linux_dmabuf_v1::NAME, 3);
        let (width, height) = (2, 2);
        let layout =
            validate_dmabuf_layout(wl_shm::FORMAT_NV12, width, height, [4, 8, 0], [0, 16, 0])
                .expect("a valid NV12 layout");
        let host_copy = shm_pool_file(&[0; 24]);
        let request = ShmBufferRequest {
            id: 30,
            pool_id: 20,
            // The 2x2 Y plane, then the one row of the UV plane.
            pool_file: Arc::new(shm_pool_file(&[1, 2, 3, 4, 5, 6])),
            offset: 0,
            width,
            height,
            stride: 2,
            format: wl_shm::FORMAT_NV12,
            size: 6,
        };
        let allocation = DmabufAllocation {
            fd: host_copy.try_clone().unwrap().into(),
            layout,
            modifier: 0,
            dmabuf_sync: true,
        };

        assert_eq!(
            create_dmabuf_buffer(&mut ctx, request, host_dmabuf, allocation),
            Action::Drop
        );
        assert!(!ctx.is_fatal());
        let senders = std::iter::once(zwp_linux_dmabuf_v1::NAME)
            .chain(std::iter::repeat(zwp_linux_buffer_params_v1::NAME));
        let planes: Vec<_> = ctx
            .client_to_host_queue
            .iter()
            .zip(senders)
            .filter_map(|((message, fds), interface)| {
                let mut fds = fds.iter().map(|fd| fd.try_clone().unwrap()).collect();
                match decode(message, &mut fds, |msg| {
                    AnyRequest::from_wire(interface, msg)
                }) {
                    Ok((
                        _,
                        AnyRequest::ZwpLinuxBufferParamsV1(
                            zwp_linux_buffer_params_v1::Request::Add {
                                plane_idx,
                                offset,
                                stride,
                                ..
                            },
                        ),
                    )) => Some((plane_idx, offset, stride)),
                    Ok(_) => None,
                    Err(error) => panic!("a queued request does not decode: {error}"),
                }
            })
            .collect();
        assert_eq!(planes, [(0, 0, 4), (1, 16, 8)]);

        // Copy every pixel, as the buffer's first commit does.
        let buffer = ctx.local_buffer(30).expect("the buffer's state");
        let layout = ShmCopyLayout::for_buffer(buffer).expect("a copy layout");
        copy_shm_damage(
            &buffer.pool_file,
            buffer.dest_ptr,
            layout,
            &DamageRegion::Full,
        )
        .unwrap();
        let mut pixels = [0; 24];
        host_copy.read_exact_at(&mut pixels, 0).unwrap();
        assert_eq!(
            pixels,
            [
                1, 2, 0, 0, 3, 4, 0, 0, // Y rows, 4 bytes apart
                0, 0, 0, 0, 0, 0, 0, 0, // the gap
                5, 6, 0, 0, 0, 0, 0, 0, // the UV row
            ]
        );
    }

    #[test]
    fn synthetic_shm_release_clears_cache_before_guest_id_reuse() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        let optional_format = 0x3631_4752;
        record_host_shm_wl_format(&mut ctx, optional_format);
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 2);
        register_guest_shm(&mut ctx, guest_shm, 10);
        ctx.host_to_client_queue.clear();
        ctx.stale_shm_guest_objects.insert(guest_shm);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        let mut release = WireMessage::new(
            guest_shm,
            crate::protocols::wayland::wl_shm::REQ_RELEASE,
            &[],
        );
        assert_eq!(
            raw_dispatch(crate::protocols::wayland::wl_shm::dispatch_request(
                &mut release,
                &mut handler,
                &mut ctx
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
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 2);
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

    /// Without VirtWL the host is a local compositor (`--local-compositor`),
    /// which maps a memfd as the buffer's wl_shm pool, with `--gpu-accel`
    /// and a host linux-dmabuf too. This pins the rule: a test context has
    /// no render node, so it cannot reach a backing that needs one.
    #[tokio::test]
    async fn without_virtwl_a_buffer_is_backed_by_a_memfd_of_its_size() {
        for gpu_accel in [false, true] {
            without_virtwl_a_buffer_is_backed_by_a_memfd(gpu_accel).await;
        }
    }

    async fn without_virtwl_a_buffer_is_backed_by_a_memfd(gpu_accel: bool) {
        let mut s = Session::with_context(Context::new_for_test(gpu_accel, false, vec![]));
        let shm = s.bind("wl_shm", wl_shm::VERSION).await;
        // With `--gpu-accel` the proxy binds the host linux-dmabuf for
        // itself. Only that bind matters here, not what else either peer
        // gets for the global.
        s.advertise("zwp_linux_dmabuf_v1", 3).await;
        let bound_dmabuf = s.host_received().iter().any(|(_, request)| {
            matches!(
                request,
                AnyRequest::WlRegistry(wl_registry::Request::Bind { id, .. })
                    if id.0 == "zwp_linux_dmabuf_v1"
            )
        });
        assert_eq!(bound_dmabuf, gpu_accel);
        s.guest_received();
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
        let buffer = s.new_id();
        let (width, height, stride) = (8, 4, 32);
        s.guest([wl_shm_pool::req::create_buffer(
            pool,
            buffer,
            0,
            width,
            height,
            stride,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;

        // The host gets a pool of the buffer's size for it alone.
        let received = s.host_received();
        let (host_shm, AnyRequest::WlShm(wl_shm::Request::CreatePool { id, fd, size })) =
            &received[0]
        else {
            panic!("the host got {received:?}");
        };
        let (host_buffer, _) = received[1].1.new_object().expect("a host buffer");
        // The proxy's own wl_shm binding has no guest ID to compare with.
        assert_eq!(Some(*host_shm), s.ctx().host_shm_id);
        assert_eq!(*size, stride * height);
        let pool_size = nix::sys::stat::fstat(fd).expect("fstat the pool").st_size;
        assert_eq!(pool_size, i64::from(stride * height));
        assert_eq!(
            received[1..],
            [
                (
                    *id,
                    wl_shm_pool::Request::CreateBuffer {
                        id: host_buffer,
                        offset: 0,
                        width,
                        height,
                        stride,
                        format: wl_shm::FORMAT_ARGB8888
                    }
                    .into()
                ),
                (*id, wl_shm_pool::Request::Destroy {}.into()),
            ]
        );
        assert_eq!(s.guest_received(), []);
    }

    /// A buffer's host copy holds the buffer's pixels: its rows from its
    /// offset in the pool, its stride apart, laid out in the host's pool as
    /// in the guest's, without the padding between them.
    #[tokio::test]
    async fn a_committed_buffer_copies_its_rows_from_its_pool_offset() {
        let (mut s, shm) = session_with_shm().await;
        let surface = s.surface().await;
        let pool = s
            .shm_pool(
                shm,
                shm_pool_file(&[
                    0xee, 0xee, 0xee, 0xee, // before the buffer
                    1, 2, 3, 4, 0xee, 0xee, 0xee, 0xee, // row 0, padding
                    5, 6, 7, 8, 0xee, 0xee, 0xee, 0xee, // row 1, padding
                ]),
            )
            .await;
        let buffer = s.new_id();
        s.guest([wl_shm_pool::req::create_buffer(
            pool,
            buffer,
            4,
            1,
            2,
            8,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;
        let received = s.host_received();
        let Some(host_pool) = received.iter().find_map(|(_, request)| match request {
            AnyRequest::WlShm(wl_shm::Request::CreatePool { fd, .. }) => Some(fd),
            _ => None,
        }) else {
            panic!("the host got no pool: {received:?}");
        };

        s.guest([
            wl_surface::req::attach(surface.id, Some(buffer), 0, 0),
            wl_surface::req::damage(surface.id, 0, 0, 1, 2),
            wl_surface::req::commit(surface.id),
        ])
        .await;
        s.host_received();
        let mut host_pixels = [0; 16];
        File::from(host_pool.try_clone().unwrap())
            .read_exact_at(&mut host_pixels, 0)
            .unwrap();
        assert_eq!(
            host_pixels,
            [1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0]
        );
        assert_eq!(s.guest_received(), []);
    }

    /// Host storage that cannot be set up (here: the host has no wl_shm to
    /// import the backing into) fails the buffer, which is fatal and leaves
    /// no trace of the buffer behind.
    #[test]
    fn failed_host_storage_is_fatal_without_creating_a_ghost_buffer() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let pool_id = 20;
        let buffer_id = 30;
        ctx.pools.insert(
            pool_id,
            ShmPool {
                file: Arc::new(shm_pool_file(&[0; 4096])),
                size: 4096,
                shm: 10,
            },
        );
        ctx.shadow_table.create_local(pool_id, "wl_shm_pool", 1);
        ctx.last_sender_id = pool_id;

        assert_eq!(
            WlShmPoolHandler::on_create_buffer(&mut ShmHandler, &mut ctx, buffer_id, 0, 1, 1, 4, 0),
            Action::Drop
        );

        assert!(ctx.is_fatal());
        assert_eq!(
            protocol_error_code(&ctx),
            wl_shm::ERROR_INVALID_FD,
            "a buffer whose storage fails reports wl_shm.invalid_fd"
        );
        assert_eq!(ctx.shadow_table.get_host_id(buffer_id), None);
        assert_eq!(ctx.shadow_table.get_interface(buffer_id), None);
        assert!(ctx.shadow_table.is_guest_id_available(buffer_id));
        assert_eq!(ctx.render_buffer_count(), 0);
        assert!(ctx.client_to_host_queue.is_empty());
    }

    /// A host buffer ID that already has render state is the proxy's own
    /// divergence, not the guest's error: the session ends with an
    /// `implementation` error, as for a native buffer.
    #[tokio::test]
    async fn a_duplicate_render_buffer_host_id_is_an_internal_failure() {
        let (mut s, shm) = session_with_shm().await;
        let pool = s.shm_pool(shm, shm_pool_file(&[0; 4096])).await;
        let first = s.shm_buffer(pool, 1, 1).await;
        // Injects a fault no peer can cause: render state already holds the
        // next two host IDs, which the next buffer's temporary pool and the
        // buffer itself take.
        for host_id in [first.host + 1, first.host + 2] {
            assert!(s.ctx().register_native_buffer(host_id, (1, 1), Vec::new()));
        }
        let second = s.new_id();
        s.guest_fatal([wl_shm_pool::req::create_buffer(
            pool,
            second,
            0,
            1,
            1,
            4,
            wl_shm::FORMAT_ARGB8888,
        )])
        .await;
        assert_eq!(
            s.guest_received_and_error(),
            (vec![], (WL_DISPLAY_ID, wl_display::ERROR_IMPLEMENTATION))
        );
        assert_eq!(s.host_received(), []);
    }

    #[test]
    fn removed_host_shm_creates_only_inert_pools_and_buffers() {
        let mut ctx = Context::new_for_test(false, false, vec![]);
        let guest_shm = 20;
        ctx.shadow_table.create_local(guest_shm, "wl_shm", 1);
        ctx.last_sender_id = 1;
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let mut registry_handler = RegistryHandler;
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &"wl_shm".to_string(), 1),
            Action::Drop
        );
        register_guest_shm(&mut ctx, guest_shm, 10);
        // The host binding has now been retired by the real
        // wl_registry.global_remove path. The synthetic guest object itself
        // remains addressable for teardown, but its requests must no longer
        // allocate local SHM state.
        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );

        let fd = shm_pool_file(&[0; 4096]);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        assert_eq!(
            handler.on_create_pool(&mut ctx, 30, fd.as_fd(), 4096),
            Action::Drop
        );
        assert!(
            ctx.pools.is_empty(),
            "a removed host wl_shm binding must not retain a new local pool"
        );
        // The guest owns the ID, so the pool exists for it to use and
        // destroy, inert like a pool created before the removal.
        assert!(ctx.shadow_table.is_local(30));
        assert_eq!(ctx.shadow_table.get_interface(30), Some("wl_shm_pool"));
        assert!(ctx.stale_shm_pools.contains(&30));

        ctx.last_sender_id = 30;
        assert_eq!(
            WlShmPoolHandler::on_create_buffer(&mut handler, &mut ctx, 31, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(ctx.shadow_table.is_local(31));
        assert_eq!(ctx.shadow_table.get_interface(31), Some("wl_buffer"));
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
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        ctx.shadow_table.insert_paired_for_test(2, 2, "wl_registry");
        let shm = "wl_shm".to_string();
        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            RegistryHandler.on_global(&mut ctx, 10, &shm, 1);
        }
        ctx.last_sender_id = 1;
        RegistryHandler.on_global(&mut ctx, 11, &shm, 1);
        for (name, guest_shm, pool) in [(10, 20, 30), (11, 21, 31)] {
            ctx.last_sender_id = 1;
            assert_eq!(
                RegistryHandler.on_bind(&mut ctx, name, &(shm.clone(), 1, guest_shm)),
                Action::Drop
            );
            ctx.last_sender_id = guest_shm;
            let file = shm_pool_file(&[0; 4096]);
            assert_eq!(
                ShmHandler.on_create_pool(&mut ctx, pool, file.as_fd(), 4096),
                Action::Drop
            );
        }

        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            RegistryHandler.on_global_remove(&mut ctx, 10);
        }

        assert_eq!(ctx.host_shm_global_name, Some(11));
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
            ShmHandler.on_create_pool(&mut ctx, 32, file.as_fd(), 4096),
            Action::Drop
        );
        ctx.last_sender_id = 32;
        assert_eq!(
            ShmHandler.on_create_buffer(&mut ctx, 33, 0, 1, 1, 4, 0),
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
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        let shm = "wl_shm".to_string();
        ctx.last_sender_id = 1;
        RegistryHandler.on_global(&mut ctx, 10, &shm, 1);
        RegistryHandler.on_global(&mut ctx, 11, &shm, 1);
        assert_eq!(ctx.host_shm_global_name, Some(10));
        assert_eq!(
            RegistryHandler.on_bind(&mut ctx, 11, &(shm.clone(), 1, 21)),
            Action::Drop
        );

        RegistryHandler.on_global_remove(&mut ctx, 11);
        assert!(ctx.stale_shm_guest_objects.contains(&21));
        RegistryHandler.on_global_remove(&mut ctx, 10);

        ctx.last_sender_id = 21;
        let file = shm_pool_file(&[0; 4096]);
        assert_eq!(
            ShmHandler.on_create_pool(&mut ctx, 31, file.as_fd(), 4096),
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
        use crate::handler::registry::RegistryHandler;
        use crate::protocols::wayland::wl_registry::WlRegistryHandler;

        fn create_pool(ctx: &mut Context, shm: u32, pool: u32) {
            ctx.last_sender_id = shm;
            let file = shm_pool_file(&[0; 4096]);
            assert_eq!(
                ShmHandler.on_create_pool(ctx, pool, file.as_fd(), 4096),
                Action::Drop
            );
        }
        fn bind(ctx: &mut Context, name: u32, shm: u32) {
            ctx.last_sender_id = 1;
            assert_eq!(
                RegistryHandler.on_bind(ctx, name, &("wl_shm".to_string(), 1, shm)),
                Action::Drop
            );
        }

        let mut ctx = Context::new_for_test(false, false, vec![]);
        ctx.shadow_table.insert_paired_for_test(1, 1, "wl_registry");
        ctx.shadow_table.insert_paired_for_test(2, 2, "wl_registry");
        let shm = "wl_shm".to_string();
        for registry in [1, 2] {
            ctx.last_sender_id = registry;
            RegistryHandler.on_global(&mut ctx, 10, &shm, 1);
            RegistryHandler.on_global(&mut ctx, 11, &shm, 1);
        }
        assert_eq!(ctx.host_shm_global_name, Some(10));
        bind(&mut ctx, 11, 21);
        create_pool(&mut ctx, 21, 31);

        ctx.last_sender_id = 1;
        RegistryHandler.on_global_remove(&mut ctx, 11);
        assert!(
            !ctx.stale_shm_guest_objects.contains(&21),
            "registry 2 lists 11"
        );
        RegistryHandler.on_global(&mut ctx, 11, &shm, 1);

        assert!(ctx.stale_shm_guest_objects.contains(&21));
        assert!(ctx.stale_shm_pools.contains(&31));
        bind(&mut ctx, 11, 22);
        create_pool(&mut ctx, 22, 32);
        ctx.last_sender_id = 32;
        assert_eq!(
            ShmHandler.on_create_buffer(&mut ctx, 33, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(ctx.shadow_table.get_host_id(33).is_some());

        ctx.last_sender_id = 2;
        RegistryHandler.on_global_remove(&mut ctx, 11);

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
        let mut registry_handler = RegistryHandler;
        let shm = "wl_shm".to_string();
        assert_eq!(
            registry_handler.on_global(&mut ctx, 10, &shm, 1),
            Action::Drop
        );
        let guest_shm = 20;
        register_guest_shm(&mut ctx, guest_shm, 10);

        let fd = shm_pool_file(&[0; 4096]);

        let mut handler = ShmHandler;
        ctx.last_sender_id = guest_shm;
        assert_eq!(
            WlShmHandler::on_create_pool(&mut handler, &mut ctx, 30, fd.as_fd(), 4096),
            Action::Drop
        );
        assert!(ctx.pools.contains_key(&30));

        ctx.last_sender_id = 1;
        assert_eq!(
            registry_handler.on_global_remove(&mut ctx, 10),
            Action::Forward
        );
        assert!(ctx.stale_shm_pools.contains(&30));

        ctx.last_sender_id = 30;
        assert_eq!(
            WlShmPoolHandler::on_resize(&mut handler, &mut ctx, 8192),
            Action::Drop
        );
        assert_eq!(
            ctx.pools.get(&30).map(|pool| pool.size),
            Some(4096),
            "a stale pool must not resize"
        );
        assert_eq!(
            WlShmPoolHandler::on_create_buffer(&mut handler, &mut ctx, 31, 0, 1, 1, 4, 0),
            Action::Drop
        );
        assert!(
            ctx.shadow_table.get_host_id(31).is_none(),
            "a stale pool must not allocate a host buffer through replacement SHM"
        );
        assert_eq!(ctx.shadow_table.get_interface(31), Some("wl_buffer"));

        assert!(ctx.local_destroyed(30));
        assert!(!ctx.pools.contains_key(&30));
        assert!(!ctx.stale_shm_pools.contains(&30));
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
