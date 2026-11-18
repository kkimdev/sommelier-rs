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

//! The host storage of a guest SHM buffer: choosing and allocating it, and
//! creating the host buffer that imports it.

use super::dmabuf::{allocate_virtwl_dmabuf, map_dmabuf, DmabufAllocation};
use super::dmabuf_layout::DmabufLayout;
use super::formats::wl_shm_format_to_drm_format;
use crate::protocols::linux_dmabuf_v1::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use crate::protocols::wayland::{wl_buffer, wl_shm, wl_shm_pool};
use crate::state::{BufferState, Context, StalePixels};
use crate::wire::Action;
use log::{debug, error};
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::Arc;

/// The host storage chosen for one guest SHM buffer.
pub(super) enum ShmBacking {
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

/// A `wl_shm_pool.create_buffer` request that passed protocol validation.
pub(super) struct ShmBufferRequest {
    pub(super) id: u32,
    pub(super) pool_id: u32,
    pub(super) pool_file: Arc<File>,
    pub(super) offset: i32,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) stride: i32,
    pub(super) format: u32,
    /// The buffer's size in bytes, as
    /// [`valid_buffer_layout`](super::pool::valid_buffer_layout) returned it.
    pub(super) size: i32,
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
pub(super) fn select_shm_backing(
    ctx: &mut Context,
    request: &ShmBufferRequest,
) -> Result<ShmBacking, Action> {
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
pub(super) fn create_shared_buffer(
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
pub(super) fn create_dmabuf_buffer(
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
            wl_shm_format_to_drm_format(format),
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

#[cfg(test)]
mod tests {
    use super::super::copy::{copy_shm_damage, ShmCopyLayout};
    use super::super::dmabuf_layout::validate_dmabuf_layout;
    use super::super::ShmHandler;
    use super::{create_dmabuf_buffer, create_shared_buffer, DmabufAllocation, ShmBufferRequest};
    use crate::handler::test_support::session_with_shm;
    use crate::protocols::linux_dmabuf_v1::zwp_linux_dmabuf_v1;
    use crate::protocols::wayland::wl_shm_pool::WlShmPoolHandler;
    use crate::protocols::wayland::{wl_display, wl_registry, wl_shm, wl_shm_pool, wl_surface};
    use crate::protocols::AnyRequest;
    use crate::proxy::harness::Session;
    use crate::state::{BufferState, Context, DamageRegion, ShmPool};
    use crate::test_support::{decode, protocol_error_code, shm_pool_file};
    use crate::wire::{Action, WL_DISPLAY_ID};
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    use std::sync::Arc;

    /// Copy every pixel of `buffer` into its host copy, as its first commit
    /// does.
    fn copy_every_pixel(buffer: &BufferState) {
        let layout = ShmCopyLayout::for_buffer(buffer).expect("a copy layout");
        assert!(!buffer.dest_ptr.is_null());
        // SAFETY: the buffer's host copy maps `dest_size` bytes that the
        // state owns, and the test holds no other reference to them.
        let destination =
            unsafe { std::slice::from_raw_parts_mut(buffer.dest_ptr, buffer.dest_size) };
        copy_shm_damage(&buffer.pool_file, destination, layout, &DamageRegion::Full).unwrap();
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
        copy_every_pixel(ctx.local_buffer(30).expect("the buffer's state"));
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

        copy_every_pixel(ctx.local_buffer(30).expect("the buffer's state"));
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
}
