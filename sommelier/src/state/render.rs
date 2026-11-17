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

use crate::protocols::wayland::wl_output;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::marker::PhantomData;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;

use super::HostId;

/// A guest `wl_shm_pool`.
///
/// The bridge reads the pool's pixels with `pread` and never maps the file.
/// A client may shrink the file after `wl_shm.create_pool`, and a read
/// through a mapping past the new end of file raises SIGBUS, which would
/// kill the proxy and every guest GUI app with it. A short `pread` is an
/// ordinary error instead.
pub struct ShmPool {
    /// The pool's file, shared with every buffer created from the pool, so
    /// it outlives `wl_shm_pool.destroy` while those buffers live.
    pub file: Arc<File>,
    /// The size the client declared. `wl_shm_pool.resize` only grows it.
    pub size: usize,
}

pub struct BufferState {
    /// The file of the pool the buffer was created from.
    pub pool_file: Arc<File>,
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
    /// The pixels the host copy lacks, which the next copy covers.
    pub stale: StalePixels,
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

/// The pixels a local buffer's host copy lacks, relative to the surface it
/// was last copied for.
///
/// Damage is relative to the surface's current contents, not to the frame a
/// buffer last showed: a client alternating buffers A, B, A damages only what
/// changed since B. Each buffer has its own host copy, so every commit on a
/// surface also stales its pixels in the host copies of the surface's other
/// buffers, and a buffer copies what it lacks the next time it is
/// committed. That relation holds only on one surface: a buffer committed
/// on another surface, or new, copies everything.
#[derive(Debug)]
pub struct StalePixels {
    damage: DamageRegion<BufferSpace>,
    surface: Option<u32>,
}

impl StalePixels {
    /// New host storage holds no pixels yet.
    pub fn all() -> Self {
        Self {
            damage: DamageRegion::Full,
            surface: None,
        }
    }

    /// A host copy that already matches its buffer as shown on `surface`.
    #[cfg(test)]
    pub fn in_sync_on(surface: u32) -> Self {
        Self {
            damage: DamageRegion::Empty,
            surface: Some(surface),
        }
    }

    /// The region to copy before the buffer is shown on `surface`.
    pub fn to_copy_on(&self, surface: u32) -> DamageRegion<BufferSpace> {
        if self.surface == Some(surface) {
            self.damage.clone()
        } else {
            DamageRegion::Full
        }
    }

    /// The host copy now matches the buffer as shown on `surface`.
    pub fn copied_on(&mut self, surface: u32) {
        self.damage = DamageRegion::Empty;
        self.surface = Some(surface);
    }

    /// Nothing in the host copy can be trusted, as after a failed copy.
    pub fn invalidate(&mut self) {
        *self = Self::all();
    }

    /// A commit on `surface` changed `damage` of the surface's contents, in
    /// this buffer's pixels, or `None` when the contents cannot be related to
    /// this buffer pixel for pixel.
    fn add_surface_damage(&mut self, surface: u32, damage: Option<&DamageRegion<BufferSpace>>) {
        if self.surface != Some(surface) {
            return;
        }
        match damage {
            Some(damage) => self.damage.extend(damage),
            None => self.damage = DamageRegion::Full,
        }
    }

    /// `surface` was destroyed, and its ID may name a new surface.
    fn forget_surface(&mut self, surface: u32) {
        if self.surface == Some(surface) {
            self.invalidate();
        }
    }
}

/// The coordinate space of buffer pixels: `wl_surface.damage_buffer` and
/// the SHM copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BufferSpace {}

/// The surface-local coordinate space: `wl_surface.damage` and the damage
/// the host receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceSpace {}

/// A rectangle in the coordinate space `S`, so damage in one space cannot
/// be used as damage in the other.
#[derive(PartialEq, Eq)]
pub struct Rect<S> {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    space: PhantomData<S>,
}

pub type BufferRect = Rect<BufferSpace>;
pub type SurfaceRect = Rect<SurfaceSpace>;

// By hand: a derive would require `S: Copy`, a bound every function generic
// over the space would then repeat.
impl<S> Clone for Rect<S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> Copy for Rect<S> {}

// By hand, to leave out the space marker.
impl<S> fmt::Debug for Rect<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rect")
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl<S> Rect<S> {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            space: PhantomData,
        }
    }
}

impl SurfaceRect {
    /// This rectangle in the pixels of a buffer of integer scale `factor`,
    /// clamped to non-negative i32 coordinates. The copy clips damage to the
    /// buffer, which lies inside that range, so the clamp drops nothing it
    /// would copy.
    fn scaled(self, factor: i32) -> BufferRect {
        let scale = |value: i64| {
            let scaled = value.saturating_mul(i64::from(factor));
            scaled.clamp(0, i64::from(i32::MAX)) as i32
        };
        let edges = |start: i32, length: i32| {
            let start = i64::from(start);
            (scale(start), scale(start + i64::from(length)))
        };
        let (x0, x1) = edges(self.x, self.width);
        let (y0, y1) = edges(self.y, self.height);
        BufferRect::new(x0, y0, x1 - x0, y1 - y0)
    }
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
pub enum DamageRegion<S> {
    #[default]
    Empty,
    Rects(Vec<Rect<S>>),
    Full,
}

impl<S> DamageRegion<S> {
    pub fn push(&mut self, rect: Rect<S>) {
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

    /// Add every rectangle of `other` to this region.
    pub fn extend(&mut self, other: &Self) {
        match other {
            Self::Empty => {}
            Self::Full => *self = Self::Full,
            Self::Rects(rects) => rects.iter().for_each(|&rect| self.push(rect)),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    pub fn rects(&self) -> &[Rect<S>] {
        match self {
            Self::Rects(rects) => rects,
            Self::Empty | Self::Full => &[],
        }
    }
}

impl<S> From<Vec<Rect<S>>> for DamageRegion<S> {
    fn from(rects: Vec<Rect<S>>) -> Self {
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

/// One double-buffered `wl_surface.attach` transition.
///
/// Wayland distinguishes an omitted attach request from `attach(NULL)`.
/// Encoding that distinction as `Option<Option<u32>>` makes the most important
/// surface transition easy to invert accidentally, so keep the three protocol
/// states explicit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceAttachment {
    #[default]
    Unchanged,
    Detach,
    Attach(u32),
}

impl SurfaceAttachment {
    pub fn buffer_id(self) -> Option<u32> {
        match self {
            Self::Attach(buffer_id) => Some(buffer_id),
            Self::Unchanged | Self::Detach => None,
        }
    }

    fn transition_from(self, previous: Option<u32>) -> Option<(Option<u32>, Option<u32>)> {
        match self {
            Self::Unchanged => None,
            Self::Detach => Some((previous, None)),
            Self::Attach(buffer_id) => Some((previous, Some(buffer_id))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceState {
    /// Immutable metadata for the committed surface contents plus an optional
    /// live wl_buffer object reference. The reference may disappear after
    /// wl_buffer.destroy while the dimensions remain valid for damage mapping.
    pub(super) current_content: Option<SurfaceContentSnapshot>,
    /// Buffer scale/transform are double-buffered by wl_surface. Keeping the
    /// state here lets the commit path decide whether a damage rectangle can
    /// be mapped safely.
    pub current_buffer_scale: i32,
    pub current_buffer_transform: i32,
    /// A viewport object exists for this surface. The object's state is
    /// tracked separately because an unset viewport is an identity mapping.
    pub viewport: Option<ViewportState>,
    /// State the next commit applies.
    pub pending: SurfacePending,
}

/// Everything a `wl_surface` request sets for the next commit, which
/// [`SurfaceState::prepare_commit`] applies at once.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SurfacePending {
    pub attachment: SurfaceAttachment,
    /// Damage expressed in surface-local coordinates. It can only be copied
    /// directly when the current buffer has the default transform and no
    /// viewport; otherwise the compositor falls back to a complete copy.
    pub surface_damage: DamageRegion<SurfaceSpace>,
    /// Damage expressed in buffer pixel coordinates.
    pub buffer_damage: DamageRegion<BufferSpace>,
    pub buffer_scale: Option<i32>,
    pub buffer_transform: Option<i32>,
    /// `wl_surface.offset`. The SHM bridge does not currently transform
    /// surface damage through a non-zero offset, so the commit path
    /// conservatively performs a complete copy in that case.
    pub offset: Option<(i32, i32)>,
    /// For wl_surface versions before 5, attach(x, y) carries the pending
    /// buffer offset. Keep it separate from the v5+ offset request so a
    /// zero-valued attach does not overwrite a real `wl_surface.offset`
    /// request that appeared earlier in the same state batch.
    pub attach_offset: Option<(i32, i32)>,
    /// `Some(None)` represents destruction of the viewport object; `None`
    /// means unchanged.
    pub viewport: Option<Option<ViewportState>>,
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
    pub attachment: SurfaceAttachment,
    pub surface_damage: DamageRegion<SurfaceSpace>,
    pub buffer_damage: DamageRegion<BufferSpace>,
    /// One-shot placement of the pending attachment relative to the previous
    /// surface contents. wl_surface.offset replaces legacy attach(x, y); it is
    /// consumed by this commit and is not persistent surface state.
    pub buffer_offset: (i32, i32),
}

impl SurfaceCommit {
    pub fn has_buffer_attach(&self) -> bool {
        matches!(self.attachment, SurfaceAttachment::Attach(_))
    }

    pub fn attached_buffer_id(&self) -> Option<u32> {
        self.attachment.buffer_id()
    }

    pub fn attachment_transition(&self) -> Option<(Option<u32>, Option<u32>)> {
        self.attachment
            .transition_from(self.previous.current_buffer_id())
    }

    /// Whether this commit changes how buffer pixels map onto the surface:
    /// its scale, transform or viewport. Pixel damage in one buffer then no
    /// longer carries over to the surface's others. (An offset moves the
    /// surface, not its pixels.)
    pub fn changes_buffer_mapping(&self) -> bool {
        self.previous.current_buffer_scale != self.state.current_buffer_scale
            || self.previous.current_buffer_transform != self.state.current_buffer_transform
            || self.previous.viewport != self.state.viewport
    }

    pub fn has_full_damage(&self) -> bool {
        self.surface_damage.is_full() || self.buffer_damage.is_full()
    }

    pub fn uses_full_mapping(&self) -> bool {
        self.surface_to_buffer_scale() != Some(1)
    }

    /// The integer factor from surface coordinates to buffer pixels, or
    /// `None` when a transform, a viewport or an attach offset makes the
    /// mapping more than a scale.
    fn surface_to_buffer_scale(&self) -> Option<i32> {
        let state = &self.state;
        let only_scaled = state.current_buffer_transform == wl_output::TRANSFORM_NORMAL
            && !(self.has_buffer_attach() && self.buffer_offset != (0, 0))
            && state.viewport.is_none_or(ViewportState::is_identity);
        only_scaled.then_some(state.current_buffer_scale)
    }

    /// This commit's damage in the pixel coordinates of the surface's
    /// buffer: the pixels the SHM bridge copies to the host.
    ///
    /// Buffer damage is already in buffer pixels. Surface damage maps to them
    /// exactly under an integer buffer scale (HiDPI clients); any other
    /// mapping falls back to the whole buffer.
    pub fn buffer_copy_damage(&self) -> DamageRegion<BufferSpace> {
        if self.has_full_damage() {
            return DamageRegion::Full;
        }
        let surface_rects = self.surface_damage.rects();
        let mut damage = self.buffer_damage.clone();
        if surface_rects.is_empty() {
            return damage;
        }
        let Some(scale) = self.surface_to_buffer_scale() else {
            return DamageRegion::Full;
        };
        for &rect in surface_rects {
            damage.push(rect.scaled(scale));
        }
        damage
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

    /// Drop the surface's references to the destroyed `buffer_id`: its
    /// current contents keep only their dimensions, and a pending attach of
    /// it becomes a detach.
    pub(crate) fn clear_buffer_references(&mut self, buffer_id: u32) {
        if self.pending.attachment == SurfaceAttachment::Attach(buffer_id) {
            self.pending.attachment = SurfaceAttachment::Detach;
        }
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

    /// The viewport the next commit applies, for a `wp_viewport` request to
    /// change: the pending one, or else a copy of the current one.
    pub fn pending_viewport_mut(&mut self) -> &mut ViewportState {
        let current = self.viewport.unwrap_or_default();
        self.pending
            .viewport
            .get_or_insert(None)
            .get_or_insert(current)
    }

    /// Apply and consume all state pending for the next surface commit.
    pub fn prepare_commit(&mut self) -> SurfaceCommit {
        let previous = self.clone();
        // Name every field: one the commit never applies is then an unused
        // variable (see docs/DESIGN.md, "Double-buffered state").
        let SurfacePending {
            attachment,
            surface_damage,
            buffer_damage,
            buffer_scale,
            buffer_transform,
            offset,
            attach_offset,
            viewport,
        } = std::mem::take(&mut self.pending);
        match attachment {
            SurfaceAttachment::Unchanged => {}
            SurfaceAttachment::Detach => self.current_content = None,
            SurfaceAttachment::Attach(buffer_id) => {
                self.current_content = Some(SurfaceContentSnapshot::from_buffer(buffer_id));
            }
        }
        if let Some(scale) = buffer_scale {
            self.current_buffer_scale = scale;
        }
        if let Some(transform) = buffer_transform {
            self.current_buffer_transform = transform;
        }
        if let Some(viewport) = viewport {
            self.viewport = viewport;
        }

        SurfaceCommit {
            previous,
            state: self.clone(),
            attachment,
            surface_damage,
            buffer_damage,
            // An explicit offset wins over a legacy attach offset.
            buffer_offset: offset.or(attach_offset).unwrap_or((0, 0)),
        }
    }
}

impl Default for SurfaceState {
    fn default() -> Self {
        Self {
            current_content: None,
            current_buffer_scale: 1,
            current_buffer_transform: 0,
            viewport: None,
            pending: SurfacePending::default(),
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
    /// The use ended before its release arrived: the last surface showing
    /// the buffer was destroyed while it awaited `wl_buffer.release`. The
    /// ordered surface destructor proves the use over, so the buffer is
    /// idle, but the host may still send the release it owed (Exo does).
    /// Exactly one release is expected; it moves the buffer to `Released`.
    ///
    /// Known limit: a release carries no serial. If the guest commits the
    /// buffer again before the owed release arrives, that release lands on
    /// the new use, and the host's later release for the new use is logged
    /// as unexpected.
    ReleaseOwed,
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

    /// Whether the host still owes a `wl_buffer.release` for this use.
    pub(super) fn owes_release(&self) -> bool {
        match self {
            Self::AwaitingRelease { .. } | Self::ReleaseOwed => true,
            Self::NeverSubmitted | Self::Released => false,
        }
    }

    /// Whether a surface may hold the buffer as its committed content.
    /// Nothing committed a `NeverSubmitted` buffer, and the last surface
    /// that showed a `ReleaseOwed` buffer is gone.
    pub(super) fn may_be_current(&self) -> bool {
        match self {
            Self::AwaitingRelease { .. } | Self::Released => true,
            Self::NeverSubmitted | Self::ReleaseOwed => false,
        }
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

struct ActiveRenderBuffer {
    backing: RenderBufferBacking,
    use_state: RenderBufferUse,
    implicit_sync_fallback: bool,
}

/// One render-buffer generation.
///
/// Backing storage exists exactly while the host buffer can still be used.
/// Making terminal destruction a separate variant prevents impossible states
/// such as "destroy queued, but backing still live" or "destroy queued and
/// awaiting release" from being representable.
enum RenderBuffer {
    GuestAlive(ActiveRenderBuffer),
    GuestDestroyed(ActiveRenderBuffer),
    HostDestroyQueued,
}

impl RenderBuffer {
    fn active(&self) -> Option<&ActiveRenderBuffer> {
        match self {
            Self::GuestAlive(buffer) | Self::GuestDestroyed(buffer) => Some(buffer),
            Self::HostDestroyQueued => None,
        }
    }

    fn active_mut(&mut self) -> Option<&mut ActiveRenderBuffer> {
        match self {
            Self::GuestAlive(buffer) | Self::GuestDestroyed(buffer) => Some(buffer),
            Self::HostDestroyQueued => None,
        }
    }

    fn lifecycle(&self) -> RenderBufferLifecycle {
        match self {
            Self::GuestAlive(buffer) => RenderBufferLifecycle::GuestAlive(buffer.use_state.clone()),
            Self::GuestDestroyed(buffer) => {
                RenderBufferLifecycle::GuestDestroyed(buffer.use_state.clone())
            }
            Self::HostDestroyQueued => RenderBufferLifecycle::HostDestroyQueued,
        }
    }
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
                entry.insert(RenderBuffer::GuestAlive(ActiveRenderBuffer {
                    backing: RenderBufferBacking::LocalCopy(backing),
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                }));
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
                entry.insert(RenderBuffer::GuestAlive(ActiveRenderBuffer {
                    backing: RenderBufferBacking::Native { size, sync_fds },
                    use_state: RenderBufferUse::NeverSubmitted,
                    implicit_sync_fallback: false,
                }));
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        }
    }

    pub(super) fn contains(&self, host_id: HostId) -> bool {
        self.entries.contains_key(&host_id)
    }

    pub(super) fn local_copy_mut(&mut self, host_id: HostId) -> Option<&mut BufferState> {
        match &mut self.entries.get_mut(&host_id)?.active_mut()?.backing {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    pub(super) fn local_copy(&self, host_id: HostId) -> Option<&BufferState> {
        match &self.entries.get(&host_id)?.active()?.backing {
            RenderBufferBacking::LocalCopy(backing) => Some(backing),
            RenderBufferBacking::Native { .. } => None,
        }
    }

    /// Stale the pixels that a commit of `surface` changed in the host copies
    /// of the surface's local buffers.
    ///
    /// `damage` is in the pixels of `shown`, the buffer the surface shows
    /// after the commit, or `None` when no host copy relates to the new
    /// contents (no contents, or a buffer attached with a new mapping).
    /// Another buffer's host copy relates to it pixel for pixel only at the
    /// same size and while the commit keeps the buffer-to-surface mapping;
    /// otherwise all of its pixels become stale. A held buffer shown without
    /// an attach keeps its pixels when the mapping changes.
    pub(super) fn stale_surface_pixels(
        &mut self,
        surface: u32,
        shown: Option<HostId>,
        damage: &DamageRegion<BufferSpace>,
        mapping_changed: bool,
    ) {
        let shown_size = shown.and_then(|host_id| self.dimensions(host_id));
        for (host_id, buffer) in self.local_copies_mut() {
            let same_size = shown_size == Some((buffer.width, buffer.height));
            let related = same_size && (shown == Some(host_id) || !mapping_changed);
            buffer
                .stale
                .add_surface_damage(surface, related.then_some(damage));
        }
    }

    /// `surface` was destroyed, and its ID may name a new surface: no host
    /// copy is relative to it any more.
    pub(super) fn forget_surface(&mut self, surface: u32) {
        for (_, buffer) in self.local_copies_mut() {
            buffer.stale.forget_surface(surface);
        }
    }

    fn local_copies_mut(&mut self) -> impl Iterator<Item = (HostId, &mut BufferState)> {
        self.entries.iter_mut().filter_map(|(&host_id, buffer)| {
            match &mut buffer.active_mut()?.backing {
                RenderBufferBacking::LocalCopy(backing) => Some((host_id, backing)),
                RenderBufferBacking::Native { .. } => None,
            }
        })
    }

    pub(super) fn dimensions(&self, host_id: HostId) -> Option<(i32, i32)> {
        match &self.entries.get(&host_id)?.active()?.backing {
            RenderBufferBacking::LocalCopy(backing) => Some((backing.width, backing.height)),
            RenderBufferBacking::Native { size, .. } => Some(*size),
        }
    }

    pub(super) fn native_sync_fds(&self, host_id: HostId) -> Option<&[OwnedFd]> {
        match &self.entries.get(&host_id)?.active()?.backing {
            RenderBufferBacking::Native { sync_fds, .. } => Some(sync_fds),
            RenderBufferBacking::LocalCopy(_) => None,
        }
    }

    pub(super) fn uses_implicit_sync_fallback(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .and_then(RenderBuffer::active)
            .is_some_and(|buffer| buffer.implicit_sync_fallback)
    }

    pub(super) fn enable_implicit_sync_fallback(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self
            .entries
            .get_mut(&host_id)
            .and_then(RenderBuffer::active_mut)
        else {
            return false;
        };
        if !matches!(buffer.backing, RenderBufferBacking::Native { .. }) {
            return false;
        }
        buffer.implicit_sync_fallback = true;
        true
    }

    pub(super) fn lifecycle(&self, host_id: HostId) -> Option<RenderBufferLifecycle> {
        self.entries.get(&host_id).map(RenderBuffer::lifecycle)
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
            .is_some_and(|buffer| buffer.active().is_some())
    }

    fn can_detach(&self, host_id: HostId) -> bool {
        self.entries
            .get(&host_id)
            .and_then(RenderBuffer::active)
            .is_some_and(|buffer| buffer.use_state.may_be_current())
    }

    pub(super) fn submit(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self
            .entries
            .get_mut(&host_id)
            .and_then(RenderBuffer::active_mut)
        else {
            return false;
        };
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { .. } => {}
            RenderBufferUse::NeverSubmitted
            | RenderBufferUse::Released
            | RenderBufferUse::ReleaseOwed => {
                buffer.use_state = RenderBufferUse::awaiting();
            }
        }
        true
    }

    pub(super) fn release(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self
            .entries
            .get_mut(&host_id)
            .and_then(RenderBuffer::active_mut)
        else {
            return false;
        };
        if !buffer.use_state.owes_release() {
            return false;
        }
        buffer.use_state = RenderBufferUse::Released;
        true
    }

    pub(super) fn detach(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self
            .entries
            .get_mut(&host_id)
            .and_then(RenderBuffer::active_mut)
        else {
            return false;
        };
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                *has_detached_use = true;
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted | RenderBufferUse::ReleaseOwed => false,
        }
    }

    pub(super) fn end_last_surface_use(
        &mut self,
        host_id: HostId,
        has_other_current: bool,
    ) -> bool {
        let Some(buffer) = self
            .entries
            .get_mut(&host_id)
            .and_then(RenderBuffer::active_mut)
        else {
            return false;
        };
        if has_other_current {
            return true;
        }
        match &mut buffer.use_state {
            RenderBufferUse::AwaitingRelease { has_detached_use } => {
                if !*has_detached_use {
                    buffer.use_state = RenderBufferUse::ReleaseOwed;
                }
                true
            }
            RenderBufferUse::Released => true,
            RenderBufferUse::NeverSubmitted | RenderBufferUse::ReleaseOwed => false,
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
            let detached = self.detach(host_id);
            debug_assert!(detached);
            if !detached {
                return false;
            }
        }
        if let Some(host_id) = next {
            let submitted = self.submit(host_id);
            debug_assert!(submitted);
            if !submitted {
                return false;
            }
        }
        true
    }

    pub(super) fn mark_guest_destroyed(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if !matches!(buffer, RenderBuffer::GuestAlive(_)) {
            return false;
        }
        let RenderBuffer::GuestAlive(active) =
            std::mem::replace(buffer, RenderBuffer::HostDestroyQueued)
        else {
            unreachable!("guest-alive state checked above");
        };
        *buffer = RenderBuffer::GuestDestroyed(active);
        true
    }

    pub(super) fn mark_host_destroy_queued(&mut self, host_id: HostId) -> bool {
        let Some(buffer) = self.entries.get_mut(&host_id) else {
            return false;
        };
        if matches!(buffer, RenderBuffer::HostDestroyQueued) {
            return false;
        }
        *buffer = RenderBuffer::HostDestroyQueued;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{BufferRect, SurfaceRect};

    #[test]
    fn scaling_damage_clamps_to_non_negative_i32_edges() {
        assert_eq!(
            SurfaceRect::new(1, 2, 3, 4).scaled(2),
            BufferRect::new(2, 4, 6, 8)
        );
        // A rectangle reaching left of the buffer keeps its visible part.
        assert_eq!(
            SurfaceRect::new(-1, -1, 3, 2).scaled(2),
            BufferRect::new(0, 0, 4, 2)
        );
        // Huge edges saturate instead of wrapping, so the copy still covers
        // the whole buffer.
        assert_eq!(
            SurfaceRect::new(-(1 << 30), 0, i32::MAX, 1).scaled(2),
            BufferRect::new(0, 0, i32::MAX - 1, 2)
        );
        assert_eq!(
            SurfaceRect::new(0, 0, i32::MAX, i32::MAX).scaled(i32::MAX),
            BufferRect::new(0, 0, i32::MAX, i32::MAX)
        );
    }
}
