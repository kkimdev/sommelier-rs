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

//! A test's connection, built as `proxy::start_client` builds one: real
//! [`Settings`] and [`ConnectionFacts`], given to [`Context::new`], so a
//! test's proxy starts as a real connection's can.

use std::io;
use std::os::fd::BorrowedFd;
use std::rc::Rc;

use crate::accelerator::Accelerator;
use crate::allocator::{DmabufFence, PendingFence, PlaneFences, VirtGpuDevice};
use crate::client_kind::WheelClient;
use crate::client_kind::WheelFromTouchpad;
use crate::placement_config::PlacementConfig;
use crate::settings::{HostTransport, Settings};
use crate::state::{ConnectionFacts, Context};

/// What a test's connection starts from: the process's settings and what
/// the connection learned as it was set up. A test changes either before
/// it builds the [`Context`].
pub(crate) struct TestConnection {
    pub(crate) settings: Settings,
    pub(crate) facts: ConnectionFacts,
}

impl Default for TestConnection {
    /// No host, no feature switch, no bindings file, and every variable
    /// at its default; no VirtWL channel or render node, no wheel steps,
    /// placement off, in no clipboard hub or security context, and the
    /// real fence of each native buffer plane.
    fn default() -> Self {
        Self {
            settings: Settings {
                socket: "/nonexistent/wayland-test".into(),
                host: HostTransport::Local("/nonexistent/host".into()),
                gpu_accel: false,
                xdg_decoration: false,
                data_control: false,
                vm_id: crate::settings::DEFAULT_VM_IDENTIFIER.to_string(),
                drm_device: None,
                wheel_from_touchpad: WheelFromTouchpad::Off,
                host_accelerators: Default::default(),
                placement_bindings: None,
                trace_dir: None,
            },
            facts: ConnectionFacts {
                virtwl: None,
                virtgpu: None,
                plane_fences: crate::allocator::dmabuf_fence,
                wheel: WheelClient::No,
                placement: PlacementConfig::default(),
                clipboard: None,
                sandbox_listeners: None,
                sandboxed: false,
            },
        }
    }
}

impl TestConnection {
    /// With `--gpu-accel`.
    pub(crate) fn gpu_accel(mut self) -> Self {
        self.settings.gpu_accel = true;
        self
    }

    /// With the host accelerators `accelerators`.
    pub(crate) fn accelerators(mut self, accelerators: Vec<Accelerator>) -> Self {
        self.settings.host_accelerators = accelerators.into();
        self
    }

    /// With the window placement configuration `placement`, for example
    /// `PlacementConfig::active_for_test`.
    pub(crate) fn placement(mut self, placement: PlacementConfig) -> Self {
        self.facts.placement = placement;
        self
    }

    /// With `plane_fences` standing in for the fence of each native buffer
    /// plane: [`sync_file_planes`] or [`busy_planes`].
    pub(crate) fn plane_fences(mut self, plane_fences: PlaneFences) -> Self {
        self.facts.plane_fences = plane_fences;
        self
    }

    /// The connection's context.
    pub(crate) fn context(self) -> Context {
        Context::new(Rc::new(self.settings), self.facts)
    }
}

/// Each native buffer plane stands in for its own sync_file: no test can
/// make a dma-buf whose fence is pending, so a pipe stands in, pending
/// until it is readable.
pub(crate) fn sync_file_planes(
    plane: BorrowedFd<'_>,
    _: Option<&VirtGpuDevice>,
) -> io::Result<DmabufFence> {
    crate::allocator::sync_file_fence(plane.try_clone_to_owned()?)
}

/// [`sync_file_planes`], whose pending plane stands in for the virtio-gpu
/// fallback instead, which reports a busy buffer and nothing to wait on.
pub(crate) fn busy_planes(
    plane: BorrowedFd<'_>,
    virtgpu: Option<&VirtGpuDevice>,
) -> io::Result<DmabufFence> {
    Ok(match sync_file_planes(plane, virtgpu)? {
        DmabufFence::Pending(_) => DmabufFence::Pending(PendingFence::Busy),
        fence => fence,
    })
}
