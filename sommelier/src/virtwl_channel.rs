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

// The VirtWL device boundary: its ioctls return raw descriptors, which the
// proxy owns even when the ioctl fails. Each ioctl wrapper hands what its
// ioctl wrote to `take_returned_fd`, `probe_supports_dmabuf`,
// `take_dmabuf` or `recv_result`, which take every descriptor over or
// close it, on error paths too. Only the helpers that handle raw
// descriptors allow `clippy::disallowed_methods`, each on its own, so the
// blocking guard still covers the rest of the channel.

use crate::virtwl::{
    virtwl_ioctl_dmabuf_sync, virtwl_ioctl_new, virtwl_ioctl_new_dmabuf, virtwl_ioctl_recv,
    virtwl_ioctl_send, virtwl_ioctl_txn, DMA_BUF_SYNC_END, DMA_BUF_SYNC_READ, DMA_BUF_SYNC_WRITE,
    VIRTWL_SEND_MAX_ALLOCS,
};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use tokio::io::unix::AsyncFd;

const DEFAULT_BUFFER_SIZE: usize = 4096;
// virtwl_ioctl_txn lives at the beginning of the ioctl buffer, so the data
// payload must leave room for its FD array and length field. ChromiumOS uses
// this exact bound in VirtWaylandChannel::max_send_size().
const MAX_SEND_SIZE: usize = DEFAULT_BUFFER_SIZE - std::mem::size_of::<virtwl_ioctl_txn>();

/// Layout returned by `VIRTWL_IOCTL_NEW_DMABUF`.
///
/// The offsets and strides are host-compositor metadata.  They must be used
/// verbatim when constructing the host linux-dmabuf buffer; recomputing them
/// from width/height is incorrect for multi-plane formats and for allocators
/// that add padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtWaylandDmabufLayout {
    pub strides: [u32; 3],
    pub offsets: [u32; 3],
}

fn dmabuf_probe_supported(errno: Option<i32>) -> bool {
    // ChromiumOS treats ENOTTY as the one definitive indication that the
    // kernel does not implement VIRTWL_IOCTL_NEW_DMABUF. Other errors (for
    // example EINVAL from the intentionally empty probe metadata) mean the
    // ioctl exists and should not make the channel silently fall back to the
    // slower shared-memory path.
    errno != Some(libc::ENOTTY)
}

fn add_nonblocking_flag(flags: nix::fcntl::OFlag) -> nix::fcntl::OFlag {
    flags | nix::fcntl::OFlag::O_NONBLOCK
}

/// The descriptors in `fds` that `close_raw_fds` closes: each non-negative
/// one, once, so a repeated number cannot close a descriptor that reused it.
fn unique_raw_fds(fds: &[i32]) -> Vec<RawFd> {
    let mut seen = HashSet::new();
    fds.iter()
        .copied()
        .filter(|&fd| fd >= 0 && seen.insert(fd))
        .collect()
}

#[allow(clippy::disallowed_methods)]
fn close_raw_fds(fds: &[i32]) {
    for fd in unique_raw_fds(fds) {
        let _ = nix::unistd::close(fd);
    }
}

#[allow(clippy::disallowed_methods)]
fn close_returned_fd(fd: i32) {
    if fd >= 0 {
        let _ = nix::unistd::close(fd);
    }
}

#[allow(clippy::disallowed_methods)]
fn owned_fds_from_txn(raw_fds: &[i32]) -> io::Result<Vec<OwnedFd>> {
    let mut packed = true;
    let mut seen = HashSet::new();
    let mut received = Vec::new();

    for &fd in raw_fds {
        if fd == -1 {
            packed = false;
            continue;
        }
        if fd < -1 {
            close_raw_fds(raw_fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid negative virtwl file descriptor",
            ));
        }
        if !packed || !seen.insert(fd) {
            close_raw_fds(raw_fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed virtwl file descriptor array",
            ));
        }
        received.push(fd);
    }

    Ok(received
        .into_iter()
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect())
}

/// Takes over the descriptor a `VIRTWL_IOCTL_NEW` ioctl named `ioctl`
/// returned in `fd`, given the ioctl's `result`.
///
/// The kernel may fill `fd` before reporting a later validation error, so
/// the descriptor is the proxy's even when the ioctl failed: an error
/// closes it.
#[allow(clippy::disallowed_methods)]
fn take_returned_fd(
    result: nix::Result<libc::c_int>,
    fd: RawFd,
    ioctl: &str,
) -> io::Result<OwnedFd> {
    if let Err(error) = result {
        close_returned_fd(fd);
        return Err(io::Error::from(error));
    }
    if fd < 0 {
        return Err(io::Error::other(format!(
            "Invalid fd returned from {ioctl}"
        )));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether the zero-sized `VIRTWL_IOCTL_NEW_DMABUF` probe that returned
/// `result` and `fd` shows the kernel allocates dma-bufs. Closes `fd`,
/// which the proxy owns whatever the result.
fn probe_supports_dmabuf(result: nix::Result<libc::c_int>, fd: RawFd) -> bool {
    match result {
        Ok(_) if fd >= 0 => {
            close_returned_fd(fd);
            true
        }
        Ok(_) => {
            log::debug!("virtwl dmabuf capability probe returned success without a valid fd");
            false
        }
        Err(error) => {
            // Some virtwl implementations allocate the descriptor before
            // rejecting the zero-sized probe metadata. Never leak that
            // descriptor on the capability-probe error path.
            close_returned_fd(fd);
            let errno = error as i32;
            log::debug!(
                "virtwl dmabuf capability probe returned errno {} ({})",
                errno,
                std::io::Error::from_raw_os_error(errno)
            );
            dmabuf_probe_supported(Some(errno))
        }
    }
}

/// The dma-buf `fd` that `VIRTWL_IOCTL_NEW_DMABUF` returned, with the
/// layout the host allocator wrote into `raw`. A zero plane-0 stride is
/// malformed, and drops `fd`.
fn take_dmabuf(
    fd: OwnedFd,
    raw: virtwl_ioctl_new_dmabuf,
) -> io::Result<(OwnedFd, VirtWaylandDmabufLayout)> {
    if raw.stride0 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VirtWL dma-buf returned a zero plane-0 stride",
        ));
    }

    Ok((
        fd,
        VirtWaylandDmabufLayout {
            strides: [raw.stride0, raw.stride1, raw.stride2],
            offsets: [raw.offset0, raw.offset1, raw.offset2],
        },
    ))
}

/// The message a `VIRTWL_IOCTL_RECV` that returned `result` wrote into
/// `txn` and `payload`, the buffer after it.
///
/// Every descriptor in `txn` is the proxy's, so each error closes them.
fn recv_result(
    result: nix::Result<libc::c_int>,
    txn: &virtwl_ioctl_txn,
    payload: &[u8],
) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    if let Err(error) = result {
        // The ChromiumOS virtwl driver normally returns without
        // publishing descriptors when recv fails. Treat the transaction
        // buffer as an untrusted device boundary nevertheless: an
        // alternate implementation may have allocated VFDs before
        // reporting a later error.
        close_raw_fds(&txn.fds);
        return Err(io::Error::from(error));
    }

    let Some(data) = payload.get(..txn.len as usize) else {
        close_raw_fds(&txn.fds);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Received data too large",
        ));
    };

    let data = data.to_vec();
    let fds = owned_fds_from_txn(&txn.fds)?;
    Ok((data, fds))
}

pub struct VirtWaylandChannel {
    // The device file descriptor (/dev/wl0)
    dev_file: File,
    // The context file descriptor created via ioctl
    // We wrap it in AsyncFd for polling
    ctx_fd: AsyncFd<OwnedFd>,
    supports_dmabuf: bool,
}

impl VirtWaylandChannel {
    pub fn new<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let dev_file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)?;

        // Create context
        let mut new_ctx = crate::virtwl::virtwl_ioctl_new::new_ctx();
        let ioctl_result = unsafe {
            // virtwl_ioctl_new is generated by ioctl_readwrite! and takes *mut T
            virtwl_ioctl_new(dev_file.as_raw_fd(), &mut new_ctx)
        };
        let ctx_fd = take_returned_fd(ioctl_result, new_ctx.fd, "VIRTWL_IOCTL_NEW_CTX")?;

        // Probe the optional dmabuf allocation ioctl once per channel. This
        // mirrors ChromiumOS Sommelier's VirtWaylandChannel::init(): virtwl
        // can carry Wayland traffic even when the host kernel only supports
        // shared-memory allocations, so ENOTTY must be treated as a normal
        // capability result rather than a channel-open failure.
        let mut dmabuf_probe = crate::virtwl::virtwl_ioctl_new::new_dmabuf(0, 0, 0);
        let dmabuf_probe_result =
            unsafe { virtwl_ioctl_new(dev_file.as_raw_fd(), &mut dmabuf_probe) };
        let supports_dmabuf = probe_supports_dmabuf(dmabuf_probe_result, dmabuf_probe.fd);

        // Set non-blocking on context FD for AsyncFd
        // F_SETFL replaces the complete file-status flag set. Preserve flags
        // inherited from the kernel (for example O_APPEND/O_ASYNC) while
        // adding the non-blocking bit required by AsyncFd.
        let existing_flags =
            nix::fcntl::fcntl(&ctx_fd, nix::fcntl::FcntlArg::F_GETFL).map_err(io::Error::from)?;
        let nonblocking_flags =
            add_nonblocking_flag(nix::fcntl::OFlag::from_bits_truncate(existing_flags));
        nix::fcntl::fcntl(&ctx_fd, nix::fcntl::FcntlArg::F_SETFL(nonblocking_flags))
            .map_err(io::Error::from)?;

        Ok(Self {
            dev_file,
            ctx_fd: AsyncFd::new(ctx_fd)?,
            supports_dmabuf,
        })
    }

    pub fn supports_dmabuf(&self) -> bool {
        self.supports_dmabuf
    }

    pub fn allocate(&self, size: u32) -> io::Result<(OwnedFd, u32)> {
        let mut new_alloc = crate::virtwl::virtwl_ioctl_new::new_alloc(size);
        let ioctl_result = unsafe { virtwl_ioctl_new(self.dev_file.as_raw_fd(), &mut new_alloc) };
        let fd = take_returned_fd(ioctl_result, new_alloc.fd, "VIRTWL_IOCTL_NEW_ALLOC")?;
        Ok((fd, size))
    }

    /// Allocate a host-backed PRIME dma-buf and return the layout selected by
    /// the host allocator.
    ///
    /// The returned descriptor is mapped into the guest by the VirtWL kernel
    /// driver.  It can be sent to the host compositor as a linux-dmabuf plane
    /// descriptor and can be CPU-accessed after a `sync` begin operation.
    pub fn allocate_dmabuf(
        &self,
        width: u32,
        height: u32,
        format: u32,
    ) -> io::Result<(OwnedFd, VirtWaylandDmabufLayout)> {
        if width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VirtWL dma-buf dimensions must be non-zero",
            ));
        }

        let mut new_dmabuf = crate::virtwl::virtwl_ioctl_new::new_dmabuf(width, height, format);
        let ioctl_result =
            unsafe { crate::virtwl::virtwl_ioctl_new(self.dev_file.as_raw_fd(), &mut new_dmabuf) };
        let fd = take_returned_fd(ioctl_result, new_dmabuf.fd, "VIRTWL_IOCTL_NEW_DMABUF")?;
        take_dmabuf(fd, new_dmabuf.get_dmabuf())
    }

    /// Synchronize CPU access to a VirtWL dma-buf.
    ///
    /// VirtWL exposes the same begin/end read/write contract as
    /// `DMA_BUF_IOCTL_SYNC`.  A failed begin or end must be surfaced to the
    /// caller; silently copying after a failed begin can race the host
    /// compositor and produce torn frames.
    pub fn sync(&self, dmabuf_fd: BorrowedFd<'_>, flags: u32) -> io::Result<()> {
        let valid_flags = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE | DMA_BUF_SYNC_END;
        if flags & !valid_flags != 0 || flags & (DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE) == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid VirtWL dma-buf sync flags",
            ));
        }
        let mut sync = crate::virtwl::virtwl_ioctl_dmabuf_sync { flags };
        unsafe { virtwl_ioctl_dmabuf_sync(dmabuf_fd.as_raw_fd(), &mut sync) }
            .map(|_| ())
            .map_err(io::Error::from)
    }

    pub fn create_pipe(&self, read: bool) -> io::Result<OwnedFd> {
        let mut new_pipe = if read {
            crate::virtwl::virtwl_ioctl_new::new_pipe_read()
        } else {
            crate::virtwl::virtwl_ioctl_new::new_pipe_write()
        };

        let ioctl_result = unsafe { virtwl_ioctl_new(self.dev_file.as_raw_fd(), &mut new_pipe) };
        take_returned_fd(ioctl_result, new_pipe.fd, "VIRTWL_IOCTL_NEW_PIPE")
    }

    /// Send `data` with copies of `fds`, which stay open.
    pub async fn send(&self, data: &[u8], fds: &[OwnedFd]) -> io::Result<()> {
        if fds.len() > VIRTWL_SEND_MAX_ALLOCS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Too many file descriptors to send",
            ));
        }

        // A Unix socket read may contain several complete Wayland messages.
        // Keep the stream intact while splitting it into virtwl-sized
        // transactions. SCM_RIGHTS belongs to the beginning of the byte
        // stream, so pass the descriptors only with the first chunk.
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![data]
        } else {
            data.chunks(MAX_SEND_SIZE).collect()
        };
        for (chunk_index, chunk) in chunks.into_iter().enumerate() {
            self.send_chunk(chunk, if chunk_index == 0 { fds } else { &[] })
                .await?;
        }
        Ok(())
    }

    async fn send_chunk(&self, data: &[u8], fds: &[OwnedFd]) -> io::Result<()> {
        let header_size = std::mem::size_of::<virtwl_ioctl_txn>();
        let total_bytes = header_size + data.len();

        // Use Vec<u32> to guarantee 4-byte alignment for virtwl_ioctl_txn
        let mut buf: Vec<u32> = vec![0u32; total_bytes.div_ceil(4)];

        {
            let buf_ptr = buf.as_mut_ptr() as *mut u8;
            let txn = unsafe { &mut *(buf_ptr as *mut virtwl_ioctl_txn) };

            txn.len = data.len() as u32;
            // `send` bounds `fds.len()` by VIRTWL_SEND_MAX_ALLOCS; unused
            // slots are -1.
            let (used, unused) = txn.fds.split_at_mut(fds.len());
            for (slot, fd) in used.iter_mut().zip(fds) {
                *slot = fd.as_raw_fd();
            }
            unused.fill(-1);

            // Copy data after header
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr.add(header_size), data.len());
            }
        }

        loop {
            let mut guard = self.ctx_fd.writable().await?;
            let res = guard.try_io(|inner| {
                let buf_ptr = buf.as_mut_ptr() as *mut u8;
                // virtwl_ioctl_send is generated by ioctl_read! and takes *mut virtwl_ioctl_txn
                unsafe {
                    virtwl_ioctl_send(inner.as_raw_fd(), buf_ptr as *mut virtwl_ioctl_txn)
                        .map_err(io::Error::from)
                }
            });

            match res {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn recv(&self) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        loop {
            let mut guard = self.ctx_fd.readable().await?;
            let res = guard.try_io(|inner| recv_message(inner.as_raw_fd()));

            match res {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    /// Attempt one non-blocking receive without waiting for VirtWL readiness.
    ///
    /// The proxy uses this between bounded render batches so keyboard and IME
    /// events already queued by the host cannot remain hidden in the kernel
    /// while client requests are continuously buffered.
    pub fn try_recv(&self) -> io::Result<Option<(Vec<u8>, Vec<OwnedFd>)>> {
        match recv_message(self.ctx_fd.get_ref().as_raw_fd()) {
            Ok(message) => Ok(Some(message)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn recv_message(fd: RawFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let header_size = std::mem::size_of::<virtwl_ioctl_txn>();
    let buf_size_bytes = header_size + MAX_SEND_SIZE;

    // Use Vec<u32> to guarantee 4-byte alignment for virtwl_ioctl_txn
    let mut buf: Vec<u32> = vec![0u32; buf_size_bytes.div_ceil(4)];
    let buf_ptr = buf.as_mut_ptr() as *mut u8;
    {
        let txn = unsafe { &mut *buf_ptr.cast::<virtwl_ioctl_txn>() };
        txn.len = MAX_SEND_SIZE as u32;
        txn.fds.fill(-1); // Prevent reading uninitialized or previous FDs
    }

    // virtwl_ioctl_recv is generated by ioctl_write_ptr! and takes *const
    // virtwl_ioctl_txn. Even though it takes *const, the kernel WRITES to
    // it (because of the _IOW direction quirk), payload included, so pass
    // a pointer to the whole buffer, not to the header.
    let result = unsafe { virtwl_ioctl_recv(fd, buf_ptr.cast::<virtwl_ioctl_txn>().cast_const()) };
    let txn = unsafe { &*buf_ptr.cast::<virtwl_ioctl_txn>() };
    let payload = unsafe { std::slice::from_raw_parts(buf_ptr.add(header_size), MAX_SEND_SIZE) };
    recv_result(result, txn, payload)
}

#[cfg(test)]
// Tests build and close raw descriptors to check who owns them.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::{
        add_nonblocking_flag, dmabuf_probe_supported, owned_fds_from_txn, probe_supports_dmabuf,
        recv_result, take_dmabuf, take_returned_fd, unique_raw_fds, VirtWaylandChannel,
        VirtWaylandDmabufLayout, DEFAULT_BUFFER_SIZE, MAX_SEND_SIZE,
    };
    use crate::protocols::wayland::wl_display;
    use crate::test_support::{any_fd_has_identity, fd_identity, TestFdIdentity};
    use crate::virtwl::{virtwl_ioctl_new_dmabuf, virtwl_ioctl_txn, VIRTWL_SEND_MAX_ALLOCS};
    use crate::wire::WL_DISPLAY_ID;
    use nix::errno::Errno;
    use nix::fcntl::OFlag;
    use nix::sys::memfd::{memfd_create, MFdFlags};
    use std::io;
    use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn max_send_size_leaves_room_for_ioctl_header() {
        assert_eq!(
            MAX_SEND_SIZE + std::mem::size_of::<virtwl_ioctl_txn>(),
            DEFAULT_BUFFER_SIZE
        );
        const {
            assert!(MAX_SEND_SIZE < DEFAULT_BUFFER_SIZE);
        }
    }

    #[test]
    fn adding_nonblocking_preserves_existing_file_status_flags() {
        let existing = OFlag::O_APPEND | OFlag::O_ASYNC;
        let updated = add_nonblocking_flag(existing);
        assert!(updated.contains(OFlag::O_APPEND));
        assert!(updated.contains(OFlag::O_ASYNC));
        assert!(updated.contains(OFlag::O_NONBLOCK));
    }

    #[tokio::test]
    #[ignore = "requires a live Crostini VirtWL device"]
    async fn live_try_recv_eagain_then_async_recv_wakes() {
        let path = std::env::var_os("SOMMELIER_TEST_VIRTWL").unwrap_or_else(|| "/dev/wl0".into());
        let channel = Arc::new(VirtWaylandChannel::new(path).expect("open live VirtWL channel"));
        let flags = nix::fcntl::fcntl(channel.ctx_fd.get_ref(), nix::fcntl::FcntlArg::F_GETFL)
            .expect("read live VirtWL context flags");
        assert!(
            OFlag::from_bits_truncate(flags).contains(OFlag::O_NONBLOCK),
            "VirtWL context must be non-blocking before AsyncFd registration"
        );
        let started = Instant::now();
        let mut reached_eagain = false;

        for _ in 0..4096 {
            if channel
                .try_recv()
                .expect("probe live VirtWL receive")
                .is_none()
            {
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "non-blocking VirtWL receive took {:?} to report EAGAIN",
                    started.elapsed()
                );
                reached_eagain = true;
                break;
            }
        }
        assert!(
            reached_eagain,
            "live VirtWL channel did not reach EAGAIN after 4096 messages"
        );

        let recv_channel = Arc::clone(&channel);
        let recv_task = tokio::spawn(async move { recv_channel.recv().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !recv_task.is_finished(),
            "AsyncFd receive must wait while the VirtWL context is empty"
        );

        let (sync, _) = wl_display::req::sync(WL_DISPLAY_ID, 2);
        channel
            .send(&sync, &[])
            .await
            .expect("send wl_display.sync through live VirtWL");
        let (reply, fds) = tokio::time::timeout(Duration::from_secs(5), recv_task)
            .await
            .expect("AsyncFd receive did not wake for wl_callback.done")
            .expect("join live VirtWL receive task")
            .expect("receive wl_callback.done through live VirtWL");
        assert!(fds.is_empty());
        assert!(reply.len() >= 12, "truncated wl_callback.done reply");
        assert_eq!(crate::test_support::msg_sender(&reply), 2);
        assert_eq!(crate::test_support::msg_opcode(&reply), 0);
        assert_eq!(crate::test_support::msg_header(&reply).len, 12);
    }

    #[test]
    fn dmabuf_probe_only_rejects_missing_ioctl_support() {
        assert!(dmabuf_probe_supported(None));
        assert!(dmabuf_probe_supported(Some(libc::EINVAL)));
        assert!(!dmabuf_probe_supported(Some(libc::ENOTTY)));
    }

    /// A descriptor as a device returns it, with the identity of its unique
    /// memfd.
    ///
    /// Tests assert on the identity, because the number can be reused by a
    /// parallel test as soon as the code under test closes it.
    fn owned_returned_fd() -> (OwnedFd, TestFdIdentity) {
        let memfd = memfd_create(c"sommelier-virtwl-returned-fd", MFdFlags::MFD_CLOEXEC)
            .expect("memfd_create");
        let identity = fd_identity(memfd.as_raw_fd());
        (memfd, identity)
    }

    /// A descriptor as a device returns it: a raw number the proxy now
    /// owns, with the identity of its memfd.
    fn returned_fd() -> (RawFd, TestFdIdentity) {
        let (fd, identity) = owned_returned_fd();
        (fd.into_raw_fd(), identity)
    }

    /// The layout `VIRTWL_IOCTL_NEW_DMABUF` writes, with six distinct plane
    /// values.
    fn dmabuf_metadata(stride0: u32) -> virtwl_ioctl_new_dmabuf {
        virtwl_ioctl_new_dmabuf {
            width: 64,
            height: 32,
            format: 0,
            stride0,
            stride1: 128,
            stride2: 96,
            offset0: 16,
            offset1: 8192,
            offset2: 12288,
        }
    }

    #[test]
    fn dmabuf_layout_preserves_all_returned_planes() {
        let (fd, identity) = owned_returned_fd();
        let (fd, layout) = take_dmabuf(fd, dmabuf_metadata(256)).expect("a valid dma-buf");
        assert_eq!(
            layout,
            VirtWaylandDmabufLayout {
                strides: [256, 128, 96],
                offsets: [16, 8192, 12288],
            }
        );
        assert_eq!(fd_identity(fd.as_raw_fd()), identity);
    }

    #[test]
    fn returned_ioctl_fd_is_closed_on_error_paths() {
        // A device that wrote a descriptor and then failed the ioctl.
        let (fd, identity) = returned_fd();
        let error = take_returned_fd(Err(Errno::EINVAL), fd, "VIRTWL_IOCTL_NEW_ALLOC")
            .expect_err("the ioctl failed");
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert!(!any_fd_has_identity(identity));

        // A dma-buf whose layout is malformed.
        let (fd, identity) = owned_returned_fd();
        let error = take_dmabuf(fd, dmabuf_metadata(0)).expect_err("a zero plane-0 stride");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!any_fd_has_identity(identity));
    }

    #[test]
    fn returned_ioctl_fd_is_taken_over_on_success() {
        let (fd, identity) = returned_fd();
        let fd = take_returned_fd(Ok(0), fd, "VIRTWL_IOCTL_NEW_PIPE").expect("the ioctl succeeded");
        assert_eq!(fd_identity(fd.as_raw_fd()), identity);

        let error = take_returned_fd(Ok(0), -1, "VIRTWL_IOCTL_NEW_PIPE")
            .expect_err("a success without a descriptor");
        assert_eq!(
            error.to_string(),
            "Invalid fd returned from VIRTWL_IOCTL_NEW_PIPE"
        );
    }

    #[test]
    fn dmabuf_probe_closes_its_descriptor_whatever_the_result() {
        // Only ENOTTY says the kernel lacks the ioctl; EINVAL is the
        // expected answer to the probe's zero-sized metadata.
        for (result, supported) in [
            (Ok(0), true),
            (Err(Errno::EINVAL), true),
            (Err(Errno::ENOTTY), false),
        ] {
            let (fd, identity) = returned_fd();
            assert_eq!(probe_supports_dmabuf(result, fd), supported, "{result:?}");
            assert!(!any_fd_has_identity(identity), "{result:?}");
        }
        assert!(!probe_supports_dmabuf(Ok(0), -1));
    }

    /// A receive transaction that carries `fds` and claims `len` bytes.
    fn recv_txn(fds: &[RawFd], len: usize) -> virtwl_ioctl_txn {
        let mut txn = virtwl_ioctl_txn {
            fds: [-1; VIRTWL_SEND_MAX_ALLOCS],
            len: len.try_into().expect("a u32 length"),
        };
        txn.fds[..fds.len()].copy_from_slice(fds);
        txn
    }

    /// A full receive payload whose every byte depends on its position.
    fn recv_payload() -> Vec<u8> {
        (0..MAX_SEND_SIZE).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn raw_fds_are_closed_once_each() {
        assert_eq!(unique_raw_fds(&[7, -1, 3, 7, -2, 3, 0]), [7, 3, 0]);
    }

    #[test]
    fn recv_ioctl_error_closes_descriptors_written_before_failure() {
        // A device that wrote two descriptors, one of them twice, and then
        // failed the ioctl.
        let (first, first_identity) = returned_fd();
        let (second, second_identity) = returned_fd();
        let txn = recv_txn(&[first, second, first], 0);

        let error =
            recv_result(Err(Errno::EIO), &txn, &recv_payload()).expect_err("the ioctl failed");

        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert!(!any_fd_has_identity(first_identity));
        assert!(!any_fd_has_identity(second_identity));
    }

    #[test]
    fn recv_rejects_an_oversized_length_and_closes_its_descriptors() {
        // A well-formed descriptor array, so only the length is wrong, and
        // one that repeats a descriptor.
        for repeats in [1, 2] {
            let (fd, identity) = returned_fd();
            let txn = recv_txn(&vec![fd; repeats], MAX_SEND_SIZE + 1);

            let error = recv_result(Ok(0), &txn, &recv_payload()).expect_err("an oversized length");

            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{repeats}");
            assert!(!any_fd_has_identity(identity), "{repeats}");
        }

        // A message that fills the whole payload still fits.
        let (data, fds) = recv_result(Ok(0), &recv_txn(&[], MAX_SEND_SIZE), &recv_payload())
            .expect("a full payload");
        assert_eq!(data, recv_payload());
        assert!(fds.is_empty());
    }

    #[test]
    fn recv_returns_the_message_and_its_descriptors() {
        let (fd, identity) = returned_fd();
        let txn = recv_txn(&[fd], 5);

        let (data, fds) = recv_result(Ok(0), &txn, &recv_payload()).expect("a message");

        assert_eq!(data, [0, 1, 2, 3, 4]);
        assert_eq!(fds.len(), 1);
        assert_eq!(fd_identity(fds[0].as_raw_fd()), identity);
    }

    #[test]
    fn virtwl_fd_array_must_be_packed() {
        let (returned_fd, identity) = returned_fd();
        // `owned_fds_from_txn` closes a malformed array at once.
        let invalid = [-1, returned_fd];
        assert!(owned_fds_from_txn(&invalid).is_err());
        assert!(!any_fd_has_identity(identity));
    }

    #[test]
    fn virtwl_fd_array_rejects_a_repeated_descriptor() {
        let (returned_fd, identity) = returned_fd();
        let invalid = [returned_fd, returned_fd];
        assert!(owned_fds_from_txn(&invalid).is_err());
        assert!(!any_fd_has_identity(identity));
    }

    #[test]
    fn virtwl_fd_array_rejects_negative_values_other_than_sentinel() {
        let invalid = [-2, -1];
        assert!(owned_fds_from_txn(&invalid).is_err());
    }

    #[test]
    fn virtwl_fd_array_converts_packed_descriptors_to_owned_fds() {
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let raw = [pipe_fds[1], -1];
        let owned = owned_fds_from_txn(&raw).expect("packed fd array");
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].as_raw_fd(), pipe_fds[1]);
        drop(owned);
        unsafe {
            libc::close(pipe_fds[0]);
        }
    }
}
