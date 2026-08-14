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

use gbm::{BufferObjectFlags, Format};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const DRM_DEVICE_ENV: &str = "SOMMELIER_DRM_DEVICE";
const DRM_DEVICE_DIR: &str = "/dev/dri";
const VIRTIO_GPU_DRIVER: &str = "virtio_gpu";
const DRM_COMMAND_BASE: u8 = 64;
const VIRTGPU_GETPARAM_COMMAND: u8 = DRM_COMMAND_BASE + 0x03;
const VIRTGPU_RESOURCE_INFO_COMMAND: u8 = DRM_COMMAND_BASE + 0x05;
const VIRTGPU_WAIT_COMMAND: u8 = DRM_COMMAND_BASE + 0x08;
const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const DMA_BUF_SYNC_READ: u32 = 1;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct DrmGemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct DrmVirtGpuWait {
    handle: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct DrmVirtGpuGetParam {
    param: u64,
    value: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct VirtGpuResourceInfoProbe {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct VirtGpuResourceInfo {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    union_words: [u32; 4],
    num_planes: u32,
    offsets: [u32; 4],
    format_modifier: u64,
}

const DRM_IOCTL_PRIME_FD_TO_HANDLE: libc::c_ulong =
    nix::request_code_readwrite!(b'd', 0x2e, std::mem::size_of::<DrmPrimeHandle>())
        as libc::c_ulong;
const DRM_IOCTL_GEM_CLOSE: libc::c_ulong =
    nix::request_code_write!(b'd', 0x09, std::mem::size_of::<DrmGemClose>()) as libc::c_ulong;
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong =
    nix::request_code_readwrite!(b'b', 0x02, std::mem::size_of::<DmaBufExportSyncFile>())
        as libc::c_ulong;
const DRM_IOCTL_VIRTGPU_WAIT: libc::c_ulong = nix::request_code_readwrite!(
    b'd',
    VIRTGPU_WAIT_COMMAND,
    std::mem::size_of::<DrmVirtGpuWait>()
) as libc::c_ulong;
const DRM_IOCTL_VIRTGPU_GETPARAM: libc::c_ulong = nix::request_code_readwrite!(
    b'd',
    VIRTGPU_GETPARAM_COMMAND,
    std::mem::size_of::<DrmVirtGpuGetParam>()
) as libc::c_ulong;
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO_PROBE: libc::c_ulong = nix::request_code_readwrite!(
    b'd',
    VIRTGPU_RESOURCE_INFO_COMMAND,
    std::mem::size_of::<VirtGpuResourceInfoProbe>()
) as libc::c_ulong;
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO_CROS: libc::c_ulong = nix::request_code_readwrite!(
    b'd',
    VIRTGPU_RESOURCE_INFO_COMMAND,
    std::mem::size_of::<VirtGpuResourceInfo>()
) as libc::c_ulong;
const VIRTGPU_RESOURCE_INFO_TYPE_EXTENDED: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DmabufPlane0Fixup {
    pub stride: u32,
    pub modifier: u64,
    pub is_virtgpu_buffer: bool,
}

/// Apply ChromiumOS' virtio-gpu classic plane-0 metadata policy without
/// touching a device. Keeping this decision separate makes the fallback and
/// the metadata-success paths testable on machines without a DRM node.
pub(crate) fn apply_dmabuf_plane0_fixup(
    guest_stride: u32,
    guest_modifier: u64,
    resource_info: Option<VirtGpuResourceInfo>,
    supports_extended_resource_info: bool,
) -> DmabufPlane0Fixup {
    let Some(resource_info) = resource_info else {
        return DmabufPlane0Fixup {
            stride: guest_stride,
            modifier: guest_modifier,
            is_virtgpu_buffer: false,
        };
    };

    let (stride, modifier) = if supports_extended_resource_info && resource_info.stride() != 0 {
        (resource_info.stride(), resource_info.format_modifier)
    } else {
        (guest_stride, guest_modifier)
    };
    DmabufPlane0Fixup {
        stride,
        modifier,
        is_virtgpu_buffer: true,
    }
}

impl VirtGpuResourceInfo {
    fn stride(self) -> u32 {
        self.union_words[0]
    }
}

fn ioctl_resource_info(fd: RawFd, info: &mut VirtGpuResourceInfo) -> io::Result<()> {
    let result = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO_CROS as _,
            std::ptr::from_mut(info),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ioctl_resource_info_probe(fd: RawFd, info: &mut VirtGpuResourceInfoProbe) -> io::Result<()> {
    let result = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO_PROBE as _,
            std::ptr::from_mut(info),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ioctl_getparam(fd: RawFd, getparam: &mut DrmVirtGpuGetParam) -> io::Result<()> {
    let result = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_VIRTGPU_GETPARAM as _,
            std::ptr::from_mut(getparam),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn wait_sync_file(fd: RawFd) -> io::Result<()> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // Match ChromiumOS Sommelier's bounded fence wait. A GPU hang must
        // not permanently stall the proxy's single Wayland event loop.
        let result = unsafe { libc::poll(std::ptr::from_mut(&mut pollfd), 1, 1_000) };
        if result > 0 {
            if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("dma-buf sync fence reported an error"));
            }
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "dma-buf sync fence wait timed out",
            ));
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
            return Err(error);
        }
    }
}

fn export_dmabuf_read_fence(fd: RawFd) -> io::Result<OwnedFd> {
    let mut sync_file = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    loop {
        let result = unsafe {
            libc::ioctl(
                fd,
                DMA_BUF_IOCTL_EXPORT_SYNC_FILE as _,
                std::ptr::from_mut(&mut sync_file),
            )
        };
        if result == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
            return Err(error);
        }
    }
    if sync_file.fd < 0 {
        return Err(io::Error::other(
            "dma-buf export returned an invalid sync-file fd",
        ));
    }
    // The kernel transfers ownership of the returned descriptor to userspace.
    Ok(unsafe { OwnedFd::from_raw_fd(sync_file.fd) })
}

fn resource_info_type_supported(
    getparam_succeeded: bool,
    resource_info_result: io::Result<()>,
) -> bool {
    getparam_succeeded
        && matches!(
            resource_info_result,
            Err(error) if error.raw_os_error() == Some(libc::EINVAL)
        )
}

fn has_extended_virtgpu_resource_info(fd: RawFd) -> bool {
    let mut features = 0_u32;
    let mut getparam = DrmVirtGpuGetParam {
        param: VIRTGPU_PARAM_3D_FEATURES,
        value: std::ptr::from_mut(&mut features) as u64,
    };
    // A resource-info ioctl number is shared with the virtio-gpu ABI. Query
    // the virtio-gpu 3D feature first, as ChromiumOS does, so a forced or
    // alternate DRM node cannot be mistaken for a kernel with the extended
    // resource-info type field.
    if ioctl_getparam(fd, &mut getparam).is_err() {
        return false;
    }

    let mut info = VirtGpuResourceInfoProbe {
        // The standard ABI's fourth word is overlaid with the ChromeOS
        // extension's `type`; -1 is intentionally invalid, so kernels that
        // understand the extended field reject it with EINVAL before
        // validating the fake handle. Keep this probe's ioctl size at the
        // standard 16-byte encoding, matching ChromiumOS.
        blob_mem: u32::MAX,
        ..VirtGpuResourceInfoProbe::default()
    };
    resource_info_type_supported(true, ioctl_resource_info_probe(fd, &mut info))
}

/// A borrowed DRM device view used only for querying libdrm metadata.
///
/// `drm::Device` is intentionally a trait rather than a concrete file wrapper;
/// keeping this adapter local avoids changing the allocator's owned `File`
/// representation while still allowing the ChromiumOS-style driver probe.
struct DrmFile<'a>(&'a File);

impl AsFd for DrmFile<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for DrmFile<'_> {}

fn is_virtio_gpu_driver(name: &OsStr) -> bool {
    name == OsStr::new(VIRTIO_GPU_DRIVER)
}

fn drm_driver_name(file: &File) -> io::Result<std::ffi::OsString> {
    use drm::Device;

    DrmFile(file)
        .get_driver()
        .map(|driver| driver.name)
        .map_err(io::Error::other)
}

/// Return the numeric suffix for a DRM render node name.
///
/// Keeping this parser separate from filesystem access makes the selection
/// policy testable without requiring a host `/dev/dri` tree in unit tests.
fn render_node_number(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("renderD")?.parse().ok()
}

/// Filter and sort render nodes by their numeric DRM minor.
///
/// Directory iteration order is unspecified. A deterministic order matters
/// when more than one render node is present, and preferring the lowest
/// numbered node preserves the historical `renderD128` behavior while still
/// working on VMs that expose a different node number.
fn sorted_render_nodes(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut nodes: Vec<_> = paths
        .into_iter()
        .filter(|path| render_node_number(path).is_some())
        .collect();
    nodes.sort_by_key(|path| render_node_number(path).unwrap_or(u32::MAX));
    nodes
}

fn drm_device_candidates(configured: Option<PathBuf>) -> io::Result<Vec<PathBuf>> {
    if let Some(path) = configured.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(vec![path]);
    }

    let entries = fs::read_dir(DRM_DEVICE_DIR)?;
    Ok(sorted_render_nodes(
        entries.filter_map(Result::ok).map(|entry| entry.path()),
    ))
}

fn open_drm_device(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {}", path.display(), error)))
}

/// Allocator for GBM buffers on the host.
///
/// This struct manages a GBM device and allows allocating buffers
/// that can be used for zero-copy sharing with the host compositor.
pub struct Allocator {
    pub device: gbm::Device<File>,
    drm_has_virtgpu_resource_info_type: bool,
}

impl Allocator {
    /// Creates a new Allocator instance.
    ///
    /// `SOMMELIER_DRM_DEVICE` can force a particular render node. Without it,
    /// all `/dev/dri/renderD*` nodes are tried in numeric order, but only
    /// nodes backed by the `virtio_gpu` DRM driver are accepted. This mirrors
    /// ChromiumOS Sommelier's `open_virtgpu()` probe and avoids selecting a
    /// software or secondary GPU node merely because it sorts first.
    pub fn new() -> io::Result<Self> {
        let configured = std::env::var_os(DRM_DEVICE_ENV)
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());
        let forced_device = configured.is_some();
        let candidates = drm_device_candidates(configured)?;
        if candidates.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no DRM render nodes found in {DRM_DEVICE_DIR}"),
            ));
        }

        let mut last_error = None;
        for path in candidates {
            let file = match open_drm_device(&path) {
                Ok(file) => file,
                Err(error) => {
                    log::debug!(
                        "Unable to open DRM render node {}: {}",
                        path.display(),
                        error
                    );
                    last_error = Some(error);
                    continue;
                }
            };

            if !forced_device {
                match drm_driver_name(&file) {
                    Ok(driver) if is_virtio_gpu_driver(&driver) => {}
                    Ok(driver) => {
                        log::debug!(
                            "Skipping non-virtio DRM render node {} (driver {})",
                            path.display(),
                            driver.to_string_lossy()
                        );
                        last_error = Some(io::Error::new(
                            io::ErrorKind::Unsupported,
                            format!(
                                "{} uses unsupported DRM driver {}",
                                path.display(),
                                driver.to_string_lossy()
                            ),
                        ));
                        continue;
                    }
                    Err(error) => {
                        log::debug!(
                            "Unable to query DRM driver for {}: {}",
                            path.display(),
                            error
                        );
                        last_error = Some(error);
                        continue;
                    }
                }
            }

            match gbm::Device::new(file) {
                Ok(device) => {
                    let drm_has_virtgpu_resource_info_type =
                        has_extended_virtgpu_resource_info(device.as_fd().as_raw_fd());
                    return Ok(Self {
                        device,
                        drm_has_virtgpu_resource_info_type,
                    });
                }
                Err(error) => {
                    // An accessible render node is not necessarily a GBM
                    // device (for example a software or auxiliary node).
                    // Keep trying deterministic candidates instead of making
                    // allocator initialization depend on directory order.
                    let error = io::Error::other(error);
                    log::debug!(
                        "Unable to initialize GBM on render node {}: {}",
                        path.display(),
                        error
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no usable DRM render nodes found in {DRM_DEVICE_DIR}"),
            )
        }))
    }

    /// Allocates a new GBM buffer object.
    ///
    /// # Arguments
    ///
    /// * `width` - The width of the buffer.
    /// * `height` - The height of the buffer.
    /// * `format` - The DRM format of the buffer.
    ///
    /// # Returns
    ///
    /// A `Result` containing the allocated `gbm::BufferObject` or an `io::Error`.
    pub fn allocate(
        &self,
        width: u32,
        height: u32,
        format: u32,
    ) -> io::Result<gbm::BufferObject<()>> {
        // Convert Wayland SHM format to GBM Format (DrmFourcc)
        // Wayland defines:
        // WL_SHM_FORMAT_ARGB8888 = 0
        // WL_SHM_FORMAT_XRGB8888 = 1

        let format = match format {
            0 => Format::Argb8888,
            1 => Format::Xrgb8888,
            val => {
                // If the value is large, it might be a FourCC code already (e.g. from dmabuf).
                // However, small values are likely Wayland SHM formats we don't support yet.
                // We'll try to parse it if it looks like a FourCC (usually ASCII chars).
                if val > 0xff {
                    gbm::Format::try_from(val).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            format!("Invalid format: {}", e),
                        )
                    })?
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Unsupported Wayland SHM format: {}", val),
                    ));
                }
            }
        };

        // We request a linear buffer layout to ensure it can be mapped if necessary.
        // Using explicit flags instead of modifiers to guarantee LINEAR usage.
        self.device
            .create_buffer_object(
                width,
                height,
                format,
                BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
            )
            .map_err(io::Error::other)
    }

    /// Query virtio-gpu classic PRIME metadata and apply ChromiumOS' plane-0
    /// stride/modifier fixup. Non-virtio buffers and unsupported kernels retain
    /// the metadata supplied by the guest.
    pub fn fixup_dmabuf_plane0(
        &self,
        fd: RawFd,
        guest_stride: u32,
        guest_modifier: u64,
    ) -> DmabufPlane0Fixup {
        if fd < 0 {
            return apply_dmabuf_plane0_fixup(
                guest_stride,
                guest_modifier,
                None,
                self.drm_has_virtgpu_resource_info_type,
            );
        }

        let mut prime = DrmPrimeHandle {
            fd,
            ..DrmPrimeHandle::default()
        };
        let prime_result = unsafe {
            libc::ioctl(
                self.device.as_fd().as_raw_fd(),
                DRM_IOCTL_PRIME_FD_TO_HANDLE as _,
                std::ptr::from_mut(&mut prime),
            )
        };
        if prime_result != 0 {
            return apply_dmabuf_plane0_fixup(
                guest_stride,
                guest_modifier,
                None,
                self.drm_has_virtgpu_resource_info_type,
            );
        }

        let mut info = VirtGpuResourceInfo {
            bo_handle: prime.handle,
            // Request the extended ChromeOS resource-info payload. The kernel
            // overwrites this union with the returned plane strides.
            union_words: [VIRTGPU_RESOURCE_INFO_TYPE_EXTENDED, 0, 0, 0],
            ..VirtGpuResourceInfo::default()
        };
        let info_result = ioctl_resource_info(self.device.as_fd().as_raw_fd(), &mut info);

        // The imported GEM handle is local to this query and must always be
        // released, including when the resource-info ioctl fails.
        let mut close = DrmGemClose {
            handle: prime.handle,
            ..DrmGemClose::default()
        };
        let _ = unsafe {
            libc::ioctl(
                self.device.as_fd().as_raw_fd(),
                DRM_IOCTL_GEM_CLOSE as _,
                std::ptr::from_mut(&mut close),
            )
        };

        apply_dmabuf_plane0_fixup(
            guest_stride,
            guest_modifier,
            info_result.ok().map(|()| info),
            self.drm_has_virtgpu_resource_info_type,
        )
    }

    /// Wait until a guest dma-buf's outstanding writers have completed before
    /// forwarding a surface commit to the host compositor.
    ///
    /// ChromiumOS Sommelier first exports and polls a dma-buf sync file, then
    /// falls back to the virtio-gpu GEM wait when the export ioctl is absent.
    /// The Rust proxy must retain one duplicate of each native buffer fd until
    /// its host `wl_buffer` is destroyed so this ordering remains intact.
    pub(crate) fn wait_for_dmabuf(&self, fd: RawFd) -> io::Result<()> {
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid dma-buf descriptor",
            ));
        }

        match export_dmabuf_read_fence(fd) {
            Ok(sync_file) => wait_sync_file(sync_file.as_fd().as_raw_fd()),
            Err(export_error) => {
                let mut prime = DrmPrimeHandle {
                    fd,
                    ..DrmPrimeHandle::default()
                };
                let import_result = unsafe {
                    libc::ioctl(
                        self.device.as_fd().as_raw_fd(),
                        DRM_IOCTL_PRIME_FD_TO_HANDLE as _,
                        std::ptr::from_mut(&mut prime),
                    )
                };
                if import_result != 0 {
                    return Err(export_error);
                }

                let mut wait = DrmVirtGpuWait {
                    handle: prime.handle,
                    flags: 0,
                };
                let wait_result = unsafe {
                    libc::ioctl(
                        self.device.as_fd().as_raw_fd(),
                        DRM_IOCTL_VIRTGPU_WAIT as _,
                        std::ptr::from_mut(&mut wait),
                    )
                };
                let wait_error = (wait_result != 0).then(io::Error::last_os_error);

                let mut close = DrmGemClose {
                    handle: prime.handle,
                    ..DrmGemClose::default()
                };
                let close_result = unsafe {
                    libc::ioctl(
                        self.device.as_fd().as_raw_fd(),
                        DRM_IOCTL_GEM_CLOSE as _,
                        std::ptr::from_mut(&mut close),
                    )
                };
                if let Some(error) = wait_error {
                    return Err(error);
                }
                if close_result != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_dmabuf_plane0_fixup, drm_device_candidates, is_virtio_gpu_driver, open_drm_device,
        render_node_number, resource_info_type_supported, sorted_render_nodes, wait_sync_file,
        VirtGpuResourceInfo, VirtGpuResourceInfoProbe, DRM_IOCTL_VIRTGPU_RESOURCE_INFO_CROS,
        DRM_IOCTL_VIRTGPU_RESOURCE_INFO_PROBE,
    };
    use std::ffi::OsStr;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_only_numeric_render_node_names() {
        assert_eq!(
            render_node_number(PathBuf::from("renderD128").as_path()),
            Some(128)
        );
        assert_eq!(
            render_node_number(PathBuf::from("renderD129").as_path()),
            Some(129)
        );
        assert_eq!(render_node_number(PathBuf::from("card0").as_path()), None);
        assert_eq!(render_node_number(PathBuf::from("renderD").as_path()), None);
        assert_eq!(
            render_node_number(PathBuf::from("renderDfoo").as_path()),
            None
        );
    }

    #[test]
    fn sorts_render_nodes_deterministically_and_filters_other_devices() {
        let nodes = sorted_render_nodes([
            PathBuf::from("/dev/dri/renderD130"),
            PathBuf::from("/dev/dri/card0"),
            PathBuf::from("/dev/dri/renderD128"),
            PathBuf::from("/dev/dri/renderD129"),
        ]);

        assert_eq!(
            nodes,
            vec![
                PathBuf::from("/dev/dri/renderD128"),
                PathBuf::from("/dev/dri/renderD129"),
                PathBuf::from("/dev/dri/renderD130"),
            ]
        );
    }

    #[test]
    fn configured_device_bypasses_render_node_discovery() {
        let configured = PathBuf::from("/custom/renderD42");
        assert_eq!(
            drm_device_candidates(Some(configured.clone())).expect("configured device"),
            vec![configured]
        );
    }

    #[test]
    fn drm_device_fd_is_close_on_exec() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("sommelier-allocator-cloexec-{suffix}"));
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("temporary DRM test file should be created");
        drop(created);

        let opened = open_drm_device(&path).expect("temporary DRM test file should open");
        let descriptor_flags = unsafe { libc::fcntl(opened.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(descriptor_flags, -1, "F_GETFD must succeed");
        assert_ne!(
            descriptor_flags & libc::FD_CLOEXEC,
            0,
            "DRM descriptors must not leak across exec"
        );

        drop(opened);
        std::fs::remove_file(path).expect("temporary DRM test file should be removed");
    }

    #[test]
    fn automatic_probe_accepts_only_chromiumos_virtio_gpu_driver() {
        assert!(is_virtio_gpu_driver(OsStr::new("virtio_gpu")));
        assert!(!is_virtio_gpu_driver(OsStr::new("virtio-gpu")));
        assert!(!is_virtio_gpu_driver(OsStr::new("i915")));
        assert!(!is_virtio_gpu_driver(OsStr::new("")));
    }

    #[test]
    fn extended_resource_info_probe_requires_virtgpu_feature_query() {
        let invalid_type = || io::Error::from_raw_os_error(libc::EINVAL);
        let invalid_handle = || io::Error::from_raw_os_error(libc::ENOENT);

        assert!(resource_info_type_supported(true, Err(invalid_type())));
        assert!(!resource_info_type_supported(false, Err(invalid_type())));
        assert!(!resource_info_type_supported(true, Err(invalid_handle())));
        assert!(!resource_info_type_supported(true, Ok(())));
    }

    #[test]
    fn virtgpu_resource_info_probe_and_extended_abi_sizes_remain_distinct() {
        assert_eq!(std::mem::size_of::<VirtGpuResourceInfoProbe>(), 16);
        assert_eq!(std::mem::size_of::<VirtGpuResourceInfo>(), 56);
        assert_ne!(
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO_PROBE,
            DRM_IOCTL_VIRTGPU_RESOURCE_INFO_CROS,
        );
    }

    #[test]
    fn virtgpu_resource_metadata_replaces_guest_plane0_layout() {
        let info = VirtGpuResourceInfo {
            union_words: [4096, 0, 0, 0],
            format_modifier: 0x1122_3344_5566_7788,
            ..VirtGpuResourceInfo::default()
        };
        assert_eq!(
            apply_dmabuf_plane0_fixup(512, 0, Some(info), true),
            super::DmabufPlane0Fixup {
                stride: 4096,
                modifier: 0x1122_3344_5566_7788,
                is_virtgpu_buffer: true,
            }
        );
    }

    #[test]
    fn virtgpu_layout_fixup_preserves_guest_metadata_on_fallback() {
        let info = VirtGpuResourceInfo {
            union_words: [4096, 0, 0, 0],
            format_modifier: 7,
            ..VirtGpuResourceInfo::default()
        };
        assert_eq!(
            apply_dmabuf_plane0_fixup(512, 3, Some(info), false),
            super::DmabufPlane0Fixup {
                stride: 512,
                modifier: 3,
                is_virtgpu_buffer: true,
            }
        );
        assert_eq!(
            apply_dmabuf_plane0_fixup(512, 3, None, true),
            super::DmabufPlane0Fixup {
                stride: 512,
                modifier: 3,
                is_virtgpu_buffer: false,
            }
        );
        let zero_stride = VirtGpuResourceInfo::default();
        assert_eq!(
            apply_dmabuf_plane0_fixup(512, 3, Some(zero_stride), true),
            super::DmabufPlane0Fixup {
                stride: 512,
                modifier: 3,
                is_virtgpu_buffer: true,
            }
        );
    }

    #[test]
    fn ready_sync_file_returns_without_blocking() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let byte = [1_u8];
        assert_eq!(
            unsafe {
                libc::write(
                    pipe_fds[1],
                    byte.as_ptr().cast::<libc::c_void>(),
                    byte.len(),
                )
            },
            1
        );
        assert!(wait_sync_file(pipe_fds[0]).is_ok());
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }
    }
}
