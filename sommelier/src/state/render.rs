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

use std::collections::HashMap;
use std::os::unix::io::{OwnedFd, RawFd};
use std::sync::{Arc, RwLock};

use super::HostId;

pub struct PoolInner {
    pub client_ptr: *mut libc::c_void,
    pub size: usize,
}

unsafe impl Send for PoolInner {}
unsafe impl Sync for PoolInner {}

pub struct PoolState {
    pub client_fd: RawFd,
    pub inner: RwLock<PoolInner>,
}

impl Drop for PoolState {
    fn drop(&mut self) {
        // `Drop` must release the mapping even when a worker panicked while
        // holding the lock. `RwLock::write()` returns an error for a poisoned
        // lock; treating that error as "nothing to clean up" leaks the entire
        // SHM pool until process exit. `get_mut()` is safe here because `&mut
        // self` proves that no other thread can access the lock during drop,
        // and `PoisonError::get_mut()` still exposes the protected value.
        let inner = match self.inner.get_mut() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        unsafe {
            if !inner.client_ptr.is_null() && inner.client_ptr != libc::MAP_FAILED {
                libc::munmap(inner.client_ptr, inner.size);
                inner.client_ptr = std::ptr::null_mut();
            }
        }
        unsafe {
            if self.client_fd >= 0 {
                libc::close(self.client_fd);
            }
        }
    }
}

pub struct BufferState {
    pub pool: Arc<PoolState>,
    pub offset: i32,
    pub width: i32,
    pub height: i32,
    pub stride: u32,
    pub format: u32,
    #[allow(dead_code)]
    pub bo: Option<gbm::BufferObject<()>>,
    #[allow(dead_code)]
    pub dmabuf_fd: Option<OwnedFd>,
    pub bo_stride: u32,
    /// Destination offset of the second plane in the mapped output buffer.
    /// Zero means the format is single-plane; for NV12 this is the host
    /// allocator's returned plane-1 offset relative to plane 0.
    pub dmabuf_plane1_offset: usize,
    /// Destination stride of the second plane. Kept separate from
    /// `bo_stride` because host dma-buf allocators may pad planes
    /// independently.
    pub dmabuf_plane1_stride: usize,
    /// Whether the output descriptor requires VirtWL dma-buf begin/end
    /// synchronization around CPU writes.
    pub dmabuf_sync: bool,
    pub dest_ptr: *mut u8,
    pub dest_size: usize,
    /// Newly allocated host storage is uninitialized. The first committed
    /// frame must therefore copy the complete guest buffer even if the client
    /// omitted an explicit damage request.
    pub needs_full_copy: bool,
}

unsafe impl Send for BufferState {}
unsafe impl Sync for BufferState {}

impl Drop for BufferState {
    fn drop(&mut self) {
        if !self.dest_ptr.is_null() && self.dest_ptr as *mut libc::c_void != libc::MAP_FAILED {
            unsafe {
                libc::munmap(self.dest_ptr as *mut libc::c_void, self.dest_size);
                self.dest_ptr = std::ptr::null_mut();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The crop/scale state associated with a `wp_viewport`.
///
/// `wl_fixed_t` values are kept in their raw signed 24.8 representation so
/// damage conversion does not lose fractional source coordinates. Viewport
/// state is double-buffered by the wl_surface commit, not by the viewport
/// object itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportState {
    pub source: Option<(i32, i32, i32, i32)>,
    pub destination: Option<(i32, i32)>,
}

impl ViewportState {
    pub const fn new() -> Self {
        Self {
            source: None,
            destination: None,
        }
    }

    pub const fn is_identity(self) -> bool {
        self.source.is_none() && self.destination.is_none()
    }
}

impl Default for ViewportState {
    fn default() -> Self {
        Self::new()
    }
}

impl DamageRect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

pub(super) const MAX_PENDING_DAMAGE_RECTS: usize = 256;

/// Bounded damage accumulated for one surface commit.
///
/// A client may send arbitrarily many damage requests before committing.
/// Keeping every rectangle makes later coalescing quadratic and permits
/// unbounded memory growth. Once the exact set reaches its cap, transition to
/// an explicit full-damage state. Full damage is deliberately not encoded as
/// a magic rectangle: every consumer must handle the same authoritative
/// variant, so host damage and local copies cannot disagree.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DamageRegion {
    #[default]
    Empty,
    Rects(Vec<DamageRect>),
    Full,
}

impl DamageRegion {
    pub fn push(&mut self, rect: DamageRect) {
        if matches!(self, Self::Full) || rect.width <= 0 || rect.height <= 0 {
            return;
        }
        match self {
            Self::Empty => *self = Self::Rects(vec![rect]),
            Self::Rects(rects) if rects.len() >= MAX_PENDING_DAMAGE_RECTS => {
                *self = Self::Full;
            }
            Self::Rects(rects) => rects.push(rect),
            Self::Full => unreachable!("full damage returned above"),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn rects(&self) -> &[DamageRect] {
        match self {
            Self::Rects(rects) => rects,
            Self::Empty | Self::Full => &[],
        }
    }

    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

impl From<Vec<DamageRect>> for DamageRegion {
    fn from(rects: Vec<DamageRect>) -> Self {
        let mut region = Self::default();
        for rect in rects {
            region.push(rect);
        }
        region
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SurfaceContentSnapshot {
    buffer_id: Option<u32>,
    dimensions: Option<(i32, i32)>,
}

impl SurfaceContentSnapshot {
    fn from_buffer(buffer_id: u32) -> Self {
        Self {
            buffer_id: Some(buffer_id),
            dimensions: None,
        }
    }

    fn buffer_id(&self) -> Option<u32> {
        self.buffer_id
    }

    fn dimensions(&self) -> Option<(i32, i32)> {
        self.dimensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceState {
    /// Immutable metadata for the committed surface contents plus an optional
    /// live wl_buffer object reference. The reference may disappear after
    /// wl_buffer.destroy while the dimensions remain valid for damage mapping.
    pub(super) current_content: Option<SurfaceContentSnapshot>,
    /// `Some(None)` represents an explicit `attach(NULL)`, while `None`
    /// means that this commit has no attach request at all.
    pub pending_buffer_id: Option<Option<u32>>,
    /// Damage expressed in surface-local coordinates. It can only be copied
    /// directly when the current buffer has the default transform and no
    /// viewport; otherwise the compositor falls back to a complete copy.
    pub pending_surface_damage: DamageRegion,
    /// Damage expressed in buffer pixel coordinates.
    pub pending_buffer_damage: DamageRegion,
    /// Buffer scale/transform are double-buffered by wl_surface. Keeping the
    /// state here lets the commit path decide whether a damage rectangle can
    /// be mapped safely.
    pub pending_buffer_scale: Option<i32>,
    pub current_buffer_scale: i32,
    pub pending_buffer_transform: Option<i32>,
    pub current_buffer_transform: i32,
    /// `wl_surface.offset` is also double-buffered. The SHM bridge does not
    /// currently transform surface damage through a non-zero offset, so the
    /// commit path conservatively performs a complete copy in that case.
    pub pending_offset: Option<(i32, i32)>,
    /// For wl_surface versions before 5, attach(x, y) carries the pending
    /// buffer offset. Keep it separate from the v5+ offset request so a
    /// zero-valued attach does not overwrite a real `wl_surface.offset`
    /// request that appeared earlier in the same state batch.
    pub pending_attach_offset: Option<(i32, i32)>,
    /// A viewport object exists for this surface. The object's state is
    /// tracked separately because an unset viewport is an identity mapping.
    pub viewport: Option<ViewportState>,
    /// Pending viewport state applied by the next surface commit. `Some(None)`
    /// represents destruction of the viewport object; `None` means unchanged.
    pub pending_viewport: Option<Option<ViewportState>>,
}

/// One atomically prepared `wl_surface.commit`.
///
/// Preparing a commit consumes every pending double-buffered field and applies
/// it to [`SurfaceState`]. The complete pre-commit snapshot is retained so any
/// validation or buffer-copy failure can restore the exact prior state. This
/// makes rollback automatically cover fields added to `SurfaceState` later,
/// instead of relying on a parallel list of manually restored fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceCommit {
    previous: SurfaceState,
    pub state: SurfaceState,
    /// `Some(None)` is an explicit `attach(NULL)`; `None` means no attach was
    /// included in this commit.
    pub attachment: Option<Option<u32>>,
    pub surface_damage: DamageRegion,
    pub buffer_damage: DamageRegion,
    /// One-shot placement of the pending attachment relative to the previous
    /// surface contents. wl_surface.offset replaces legacy attach(x, y); it is
    /// consumed by this commit and is not persistent surface state.
    pub buffer_offset: (i32, i32),
}

impl SurfaceCommit {
    pub fn has_buffer_attach(&self) -> bool {
        matches!(self.attachment, Some(Some(_)))
    }

    pub fn attached_buffer_id(&self) -> Option<u32> {
        self.attachment.flatten()
    }

    pub fn attachment_transition(&self) -> Option<(Option<u32>, Option<u32>)> {
        self.attachment
            .map(|next| (self.previous.current_buffer_id(), next))
    }

    pub fn has_full_damage(&self) -> bool {
        self.surface_damage.is_full() || self.buffer_damage.is_full()
    }

    pub fn uses_full_mapping(&self) -> bool {
        self.state.current_buffer_scale != 1
            || self.state.current_buffer_transform != 0
            || (self.has_buffer_attach() && self.buffer_offset != (0, 0))
            || self
                .state
                .viewport
                .is_some_and(|viewport| !viewport.is_identity())
    }

    pub fn has_invalid_fractional_viewport(&self) -> bool {
        let Some(viewport) = self.state.viewport else {
            return false;
        };
        let Some((_, _, width, height)) = viewport.source else {
            return false;
        };
        viewport.destination.is_none() && (width % 256 != 0 || height % 256 != 0)
    }

    pub fn rollback(self, surface: &mut SurfaceState) {
        *surface = self.previous;
    }
}

impl SurfaceState {
    pub(crate) fn current_buffer_id(&self) -> Option<u32> {
        self.current_content
            .as_ref()
            .and_then(SurfaceContentSnapshot::buffer_id)
    }

    pub(crate) fn current_buffer_dimensions(&self) -> Option<(i32, i32)> {
        self.current_content
            .as_ref()
            .and_then(SurfaceContentSnapshot::dimensions)
    }

    pub(crate) fn set_current_buffer_dimensions(
        &mut self,
        buffer_id: u32,
        dimensions: Option<(i32, i32)>,
    ) -> bool {
        if let Some(content) = self.current_content.as_mut() {
            if content.buffer_id == Some(buffer_id) {
                content.dimensions = dimensions;
                return true;
            }
        }
        false
    }

    pub(crate) fn clear_current_buffer_reference(&mut self, buffer_id: u32) {
        let remove_content = self.current_content.as_mut().is_some_and(|content| {
            if content.buffer_id != Some(buffer_id) {
                return false;
            }
            content.buffer_id = None;
            content.dimensions.is_none()
        });
        if remove_content {
            self.current_content = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_current_buffer_for_test(
        &mut self,
        buffer_id: Option<u32>,
        dimensions: Option<(i32, i32)>,
    ) {
        self.current_content = buffer_id
            .map(|buffer_id| SurfaceContentSnapshot {
                buffer_id: Some(buffer_id),
                dimensions,
            })
            .or_else(|| {
                dimensions.map(|dimensions| SurfaceContentSnapshot {
                    buffer_id: None,
                    dimensions: Some(dimensions),
                })
            });
    }

    /// Apply and consume all state pending for the next surface commit.
    pub fn prepare_commit(&mut self) -> SurfaceCommit {
        let previous = self.clone();
        let attachment = self.pending_buffer_id.take();
        if let Some(buffer_id) = attachment {
            self.current_content = buffer_id.map(SurfaceContentSnapshot::from_buffer);
        }
        if let Some(scale) = self.pending_buffer_scale.take() {
            self.current_buffer_scale = scale;
        }
        if let Some(transform) = self.pending_buffer_transform.take() {
            self.current_buffer_transform = transform;
        }
        let buffer_offset = self
            .pending_offset
            .take()
            .or_else(|| self.pending_attach_offset.take())
            .unwrap_or((0, 0));
        // Consume a legacy attach offset even when an explicit offset wins.
        self.pending_attach_offset = None;
        if let Some(viewport) = self.pending_viewport.take() {
            self.viewport = viewport;
        }
        let surface_damage = self.pending_surface_damage.take();
        let buffer_damage = self.pending_buffer_damage.take();

        SurfaceCommit {
            previous,
            state: self.clone(),
            attachment,
            surface_damage,
            buffer_damage,
            buffer_offset,
        }
    }
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            current_content: None,
            pending_buffer_id: None,
            pending_surface_damage: DamageRegion::default(),
            pending_buffer_damage: DamageRegion::default(),
            pending_buffer_scale: None,
            current_buffer_scale: 1,
            pending_buffer_transform: None,
            current_buffer_transform: 0,
            pending_offset: None,
            pending_attach_offset: None,
            viewport: None,
            pending_viewport: None,
        }
    }
}

/// Host-compositor use phase of one render buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderBufferUse {
    NeverSubmitted,
    AwaitingRelease {
        /// At least one earlier attachment was replaced before the host
        /// compositor emitted `wl_buffer.release`. No surface destructor can
        /// prove that detached use complete; only the release event can.
        has_detached_use: bool,
    },
    Released,
}

impl RenderBufferUse {
    fn awaiting() -> Self {
        Self::AwaitingRelease {
            has_detached_use: false,
        }
    }

    pub(super) fn is_awaiting_release(&self) -> bool {
        matches!(self, Self::AwaitingRelease { .. })
    }
}

/// Guest/host ownership phase of one render buffer.
///
/// The lifecycle and use phase live in one enum so guest-destroyed backing
/// cannot accidentally remain in a separate "live" map, and a queued host
/// destructor cannot still be represented as compositor-owned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderBufferLifecycle {
    GuestAlive(RenderBufferUse),
    GuestDestroyed(RenderBufferUse),
    HostDestroyQueued,
}

impl RenderBufferLifecycle {
    pub fn use_state(&self) -> Option<&RenderBufferUse> {
        match self {
            Self::GuestAlive(use_state) | Self::GuestDestroyed(use_state) => Some(use_state),
            Self::HostDestroyQueued => None,
        }
    }

    pub fn is_guest_destroyed(&self) -> bool {
        matches!(self, Self::GuestDestroyed(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenderBufferOwnership {
    GuestAlive,
    GuestDestroyed,
    HostDestroyQueued,
}

/// Storage owned by one host `wl_buffer` generation.
enum RenderBufferBacking {
    /// Guest SHM copied into proxy-owned host storage.
    LocalCopy(BufferState),
    /// Guest-created linux-dmabuf forwarded without a CPU copy.
    Native {
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    },
}

struct RenderBuffer {
    backing: Option<RenderBufferBacking>,
    ownership: RenderBufferOwnership,
    use_state: RenderBufferUse,
    implicit_sync_fallback: bool,
}

/// Canonical host-ID keyed registry for every render buffer.
///
/// A host ID is the Wayland generation identity: it remains unique while late
/// `release` and `delete_id` events are in flight even if the guest numeric ID
/// becomes reusable. All backing, use, and ownership state therefore moves
/// together under this one key.
#[derive(Default)]
pub(super) struct RenderBufferRegistry {
    entries: HashMap<HostId, RenderBuffer>,
}

impl RenderBufferRegistry {
    pub(super) fn register_local(&mut self, host_id: HostId, backing: BufferState) -> bool {
        match self.entries.entry(host_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RenderBuffer {
                    backing: Some(RenderBufferBacking::LocalCopy(backing)),
                    ownership: RenderBufferOwnership::GuestAlive,
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    pub(super) fn register_native(
        &mut self,
        host_id: HostId,
        size: (i32, i32),
        sync_fds: Vec<OwnedFd>,
    ) -> bool {
        match self.entries.entry(host_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(RenderBuffer {
                    backing: Some(RenderBufferBacking::Native { size, sync_fds }),
                    ownership: RenderBufferOwnership::GuestAlive,
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                });
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    pub(super) fn contains(&self, host_id: HostId) -> bool {
        self.entries.contains_key(&host_id)
    }

    pub(super) fn local_copy_mut(&mut self, host_id: HostId) -> Option<&mut BufferState> {
        match self.entries.get_mut(&host_id)?.backing.as_mut()? {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    pub(super) fn local_copy(&self, host_id: HostId) -> Option<&BufferState> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    pub(super) fn dimensions(&self, host_id: HostId) -> Option<(i32, i32)> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::LocalCopy(backing) => Some((backing.width, backing.height)),
            RenderBufferBacking::Native { size, .. } => Some(*size),
        }
    }

    pub(super) fn native_sync_fds(&self, host_id: HostId) -> Option<&[OwnedFd]> {
        match self.entries.get(&host_id)?.backing.as_ref()? {
            RenderBufferBacking::Native { sync_fds, .. } => Some(sync_fds),
            RenderBufferBacking::LocalCopy(_) => None,
        }
    }

    pub(super) fn uses_implicit_sync_fallback(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .is_some_and(|buffer| buffer.implicit_sync_fallback)
    }

    pub(super) fn enable_implicit_sync_fallback(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if !matches!(buffer.backing, Some(RenderBufferBacking::Native { .. })) {
            return false;
        }
        buffer.implicit_sync_fallback = true;
        true
    }

    pub(super) fn lifecycle(&self, host_id: HostId) -> Option<RenderBufferLifecycle> {
        let buffer = self.entries.get(&host_id)?;
        Some(match buffer.ownership {
            RenderBufferOwnership::GuestAlive => {
                RenderBufferLifecycle::GuestAlive(buffer.use_state.clone())
            }
            RenderBufferOwnership::GuestDestroyed => {
                RenderBufferLifecycle::GuestDestroyed(buffer.use_state.clone())
            }
            RenderBufferOwnership::HostDestroyQueued => RenderBufferLifecycle::HostDestroyQueued,
        })
    }

    pub(super) fn lifecycles(&self) -> impl Iterator<Item = (HostId, RenderBufferLifecycle)> + '_ {
        self.entries.keys().filter_map(|&host_id| {
            self.lifecycle(host_id)
                .map(|lifecycle| (host_id, lifecycle))
        })
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn remove(&mut self, host_id: HostId) -> bool {
        self.entries.remove(&host_id).is_some()
    }

    fn can_submit(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .is_some_and(|buffer| buffer.ownership != RenderBufferOwnership::HostDestroyQueued)
    }

    fn can_detach(&self, host_id: HostId) -> bool {
        self.entries.get(&host_id).is_some_and(|buffer| {
            buffer.ownership != RenderBufferOwnership::HostDestroyQueued
                && !matches!(buffer.use_state, RenderBufferUse::NeverSubmitted)
        })
    }

    pub(super) fn submit(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { .. } => {}
            RenderBufferUse::NeverSubmitted | RenderBufferUse::Released => {
                buffer.use_state = RenderBufferUse::awaiting();
            }
        }
        true
    }

    pub(super) fn release(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued
            || !buffer.use_state.is_awaiting_release()
        {
            return false;
        }
        buffer.use_state = RenderBufferUse::Released;
        true
    }

    pub(super) fn detach(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                *has_detached_use = true;
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted => false,
        }
    }

    pub(super) fn end_last_surface_use(
        &mut self,
        host_id: HostId,
        has_other_current: bool,
    ) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        if has_other_current {
            return true;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                if !*has_detached_use {
                    buffer.use_state = RenderBufferUse::NeverSubmitted;
                }
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted => false,
        }
    }

    /// Atomically apply one successful `wl_surface` attachment replacement.
    ///
    /// Both generations are validated before either lifecycle changes. This
    /// keeps a failed replacement from latching the old buffer as detached or
    /// beginning a use interval for the new buffer.
    pub(super) fn finalize_attachment(
        &mut self,
        previous: Option<HostId>,
        next: Option<HostId>,
    ) -> bool {
        if previous == next {
            return next.is_none_or(|host_id| self.submit(host_id));
        }
        if previous.is_some_and(|host_id| !self.can_detach(host_id))
            || next.is_some_and(|host_id| !self.can_submit(host_id))
        {
            return false;
        }
        if let Some(host_id) = previous {
            debug_assert!(self.detach(host_id));
        }
        if let Some(host_id) = next {
            debug_assert!(self.submit(host_id));
        }
        true
    }

    pub(super) fn mark_guest_destroyed(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        match buffer.ownership {
            RenderBufferOwnership::GuestAlive => {
                buffer.ownership = RenderBufferOwnership::GuestDestroyed;
                true
            }
            RenderBufferOwnership::GuestDestroyed | RenderBufferOwnership::HostDestroyQueued => {
                false
            }
        }
    }

    pub(super) fn mark_host_destroy_queued(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if buffer.ownership == RenderBufferOwnership::HostDestroyQueued {
            return false;
        }
        buffer.backing = None;
        buffer.ownership = RenderBufferOwnership::HostDestroyQueued;
        true
    }
}
