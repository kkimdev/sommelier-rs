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

//! Guest `wl_shm`, which the proxy implements itself; see "Buffers" in
//! `docs/DESIGN.md`. Each concern is a submodule.

mod copy;
mod damage;
mod dmabuf;
mod dmabuf_layout;
mod formats;
mod pool;
mod storage;

pub(crate) use self::copy::{copy_shm_damage, ShmCopyError, ShmCopyLayout};
pub(crate) use self::damage::{coalesce_damage_rects, damage_reaches_buffer};
pub(crate) use self::formats::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, is_supported_drm_format,
    record_host_shm_drm_format, register_guest_shm,
};
pub(crate) use self::pool::retire_guest_shms;

use crate::state::GuestId;
use std::collections::BTreeSet;
use std::fs::File;
use std::sync::Arc;

/// The formats the host reported, by source (`Context::shm_formats`).
/// Keeping the sources apart lets a dma-buf global disappear without
/// invalidating a format the host's `wl_shm` still offers. Only this module
/// reads or writes them.
#[derive(Debug, Default)]
pub(crate) struct ShmFormats {
    /// Optional formats learned from the internal host `wl_shm` binding.
    wl: BTreeSet<u32>,
    /// Optional formats learned from the internal host linux-dmabuf
    /// binding, removed when that binding's global is withdrawn.
    dmabuf: BTreeSet<u32>,
}

/// A guest `wl_shm`, which the proxy implements; its table entry keeps it.
#[derive(Debug)]
pub(crate) struct GuestShm {
    /// The host `wl_shm` global the guest bound it from, whichever global
    /// the proxy itself has bound.
    global_name: u32,
    /// The formats already sent to it, so a host capability event that
    /// arrives after the bind is not sent twice.
    formats: BTreeSet<u32>,
    /// Whether its global has retired (`retire_guest_shms`). Wayland keeps
    /// an already-bound global object valid for teardown, but requests
    /// sent to it after its global's removal are ignored, even while the
    /// proxy has bound a replacement; objects bound from a live name are
    /// unaffected.
    stale: bool,
}

/// A guest `wl_shm_pool`, which the proxy implements; its table entry
/// keeps it.
#[derive(Debug)]
pub(crate) enum PoolData {
    /// A pool whose `wl_shm`'s global is live.
    Live(ShmPool),
    /// A pool of a `wl_shm` whose global has retired, created before the
    /// retirement or after it: it stays destroyable, but must not resize
    /// or create buffers through a replacement host binding.
    Inert,
}

/// A live guest `wl_shm_pool`.
///
/// The bridge reads the pool's pixels with `pread` and never maps the file.
/// A client may shrink the file after `wl_shm.create_pool`, and a read
/// through a mapping past the new end of file raises SIGBUS, which would
/// kill the proxy and every guest GUI app with it. A short `pread` is an
/// ordinary error instead.
#[derive(Debug)]
pub(crate) struct ShmPool {
    /// The pool's file, shared with every buffer created from the pool, so
    /// it outlives `wl_shm_pool.destroy` while those buffers live.
    file: Arc<File>,
    /// The size the client declared. `wl_shm_pool.resize` only grows it.
    size: usize,
    /// The guest `wl_shm` that created it, whose host global it depends on.
    shm: GuestId,
}
