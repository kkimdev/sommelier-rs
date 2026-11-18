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

mod buffer_lifetime;
mod copy;
mod damage;
mod dmabuf;
mod dmabuf_layout;
mod formats;
mod pool;
mod storage;

pub(crate) use self::buffer_lifetime::{
    clear_buffer_uses_after_surface_destroy, queue_host_buffer_destroy, retire_eligible_buffers,
};
pub(crate) use self::copy::{copy_shm_damage, ShmCopyError, ShmCopyLayout};
pub(crate) use self::damage::{coalesce_damage_rects, damage_reaches_buffer};
pub(crate) use self::formats::{
    clear_host_shm_dmabuf_formats, clear_host_shm_wl_formats, record_host_shm_drm_format,
    register_guest_shm,
};
pub(crate) use self::pool::{forget_pool, forget_shm, retire_guest_shms};

pub struct ShmHandler;

/// Test helpers that the submodules' tests share.
#[cfg(test)]
mod tests {
    use crate::protocols::wayland::wl_shm;
    use crate::proxy::harness::Session;

    /// A session whose guest has bound `wl_shm`, which the proxy
    /// implements.
    pub(super) async fn session_with_shm() -> (Session, u32) {
        let mut s = Session::new();
        let shm = s.bind("wl_shm", wl_shm::VERSION).await;
        (s, shm)
    }
}
