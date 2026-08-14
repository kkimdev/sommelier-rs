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

use crate::virtwl_channel::VirtWaylandChannel;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::collections::HashSet;
use std::io::{self, IoSlice};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tokio::io::unix::AsyncFd;

// Linux accepts at most SCM_MAX_FD (253) descriptors in one SCM_RIGHTS
// message. Keeping the receive control buffer at that limit prevents a valid
// Unix peer from triggering MSG_CTRUNC merely because the proxy used a
// smaller, VirtWL-specific batch size. The parser below still handles
// truncation defensively for malformed or non-Linux transports.
const UNIX_MAX_SCM_RIGHTS_FDS: usize = 253;

pub struct WaylandConnection {
    transport: ConnectionTransport,
    pub read_buf: Vec<u8>,
    pub read_fds: Vec<RawFd>,
    /// A complete untracked message was dropped while descriptors remained
    /// queued on the ordered stream. This ambiguity must survive separate
    /// recv/parse calls until a complete follow-up can be rejected.
    pub ambiguous_untracked_fd: bool,
}

enum ConnectionTransport {
    Unix(AsyncFd<OwnedFd>),
    VirtWayland(Arc<VirtWaylandChannel>),
}

fn unix_send_flags() -> MsgFlags {
    // A compositor or client can disappear while a message is queued. The
    // proxy must observe EPIPE and tear down that connection, not receive a
    // process-wide SIGPIPE and take unrelated GUI clients with it.
    MsgFlags::MSG_NOSIGNAL
}

fn close_received_fds(fds: &[RawFd]) {
    let mut unique_fds = HashSet::with_capacity(fds.len());
    for &fd in fds {
        if fd >= 0 && unique_fds.insert(fd) {
            let _ = nix::unistd::close(fd);
        }
    }
}

/// Decode SCM_RIGHTS descriptors from a raw `recvmsg` control buffer.
///
/// `nix::RecvMsg::cmsgs()` intentionally refuses to iterate when
/// `MSG_CTRUNC` is set. That is normally a useful safety check, but it would
/// leak descriptors that the kernel did install before truncating the
/// ancillary buffer. This parser validates each kernel-provided length,
/// collects every descriptor that is present, and closes those descriptors
/// before returning an error whenever truncation or malformed control data is
/// observed.
fn decode_unix_control_fds(msg: &libc::msghdr) -> io::Result<Vec<RawFd>> {
    let mut fds = Vec::new();
    let control_start = msg.msg_control as usize;
    let Some(control_end) = control_start.checked_add(msg.msg_controllen) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SCM_RIGHTS control buffer address overflows",
        ));
    };
    let header_size = std::mem::size_of::<libc::cmsghdr>();
    let align = std::mem::size_of::<usize>();
    let mut current = if msg.msg_controllen >= header_size && !msg.msg_control.is_null() {
        msg.msg_control.cast::<libc::cmsghdr>()
    } else {
        std::ptr::null_mut()
    };

    while !current.is_null() {
        let current_addr = current as usize;
        let Some(current_offset) = current_addr.checked_sub(control_start) else {
            close_received_fds(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SCM_RIGHTS header lies outside the control buffer",
            ));
        };
        if current_offset > msg.msg_controllen || msg.msg_controllen - current_offset < header_size
        {
            close_received_fds(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated SCM_RIGHTS control header",
            ));
        }

        let cmsg_len = unsafe { (*current).cmsg_len as usize };
        if cmsg_len < header_size || cmsg_len > control_end - current_addr {
            close_received_fds(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid SCM_RIGHTS control length",
            ));
        }

        let payload_len = cmsg_len - header_size;
        if unsafe { (*current).cmsg_level } == libc::SOL_SOCKET
            && unsafe { (*current).cmsg_type } == libc::SCM_RIGHTS
        {
            if !payload_len.is_multiple_of(std::mem::size_of::<RawFd>()) {
                close_received_fds(&fds);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS payload is not fd-aligned",
                ));
            }
            let Some(data_addr) = current_addr.checked_add(header_size) else {
                close_received_fds(&fds);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS payload address overflows",
                ));
            };
            let Some(data_end) = data_addr.checked_add(payload_len) else {
                close_received_fds(&fds);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS payload end overflows",
                ));
            };
            let Some(message_end) = current_addr.checked_add(cmsg_len) else {
                close_received_fds(&fds);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS message end overflows",
                ));
            };
            if data_end > message_end {
                close_received_fds(&fds);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SCM_RIGHTS payload exceeds control message",
                ));
            }
            for index in 0..(payload_len / std::mem::size_of::<RawFd>()) {
                let fd_addr = data_addr + index * std::mem::size_of::<RawFd>();
                fds.push(unsafe { std::ptr::read_unaligned(fd_addr as *const RawFd) });
            }
        }

        let Some(aligned_len) = cmsg_len
            .checked_add(align - 1)
            .map(|len| len & !(align - 1))
        else {
            close_received_fds(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SCM_RIGHTS control alignment overflows",
            ));
        };
        let Some(next_addr) = current_addr.checked_add(aligned_len) else {
            close_received_fds(&fds);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SCM_RIGHTS control chain overflows",
            ));
        };
        if next_addr >= control_end || control_end - next_addr < header_size {
            break;
        }
        current = next_addr as *mut libc::cmsghdr;
    }

    if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
        close_received_fds(&fds);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated SCM_RIGHTS ancillary data",
        ));
    }

    Ok(fds)
}

fn recv_unix_message(
    fd: RawFd,
    data: &mut [u8],
    control: &mut [u8],
) -> io::Result<(usize, Vec<RawFd>)> {
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len();

    let bytes = unsafe { libc::recvmsg(fd, &mut msg, 0) };
    if bytes < 0 {
        return Err(io::Error::last_os_error());
    }
    let fds = decode_unix_control_fds(&msg)?;
    Ok((bytes as usize, fds))
}

impl WaylandConnection {
    pub fn new(fd: RawFd) -> Self {
        // Safety: We assume we own the fd passed in
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };

        // Ensure non-blocking mode for AsyncFd
        let raw_fd = owned.as_raw_fd();
        unsafe {
            let borrowed_fd = std::os::fd::BorrowedFd::borrow_raw(raw_fd);
            let flags_int = fcntl(borrowed_fd, FcntlArg::F_GETFL).unwrap_or(0);
            let flags = OFlag::from_bits_truncate(flags_int);
            let _ = fcntl(borrowed_fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK));
        }

        Self {
            transport: ConnectionTransport::Unix(
                AsyncFd::new(owned).expect("Failed to create AsyncFd"),
            ),
            read_buf: Vec::new(),
            read_fds: Vec::new(),
            ambiguous_untracked_fd: false,
        }
    }

    pub fn new_virtwayland(channel: Arc<VirtWaylandChannel>) -> Self {
        Self {
            transport: ConnectionTransport::VirtWayland(channel),
            read_buf: Vec::new(),
            read_fds: Vec::new(),
            ambiguous_untracked_fd: false,
        }
    }

    pub async fn send(&mut self, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
        if data.is_empty() && fds.is_empty() {
            return Ok(());
        }

        match &mut self.transport {
            ConnectionTransport::Unix(fd) => {
                let mut data_offset = 0;
                let mut fds_to_send = fds;

                while data_offset < data.len() || !fds_to_send.is_empty() {
                    let mut guard = fd.writable().await?;

                    let remaining_data = &data[data_offset..];
                    let iov = [IoSlice::new(remaining_data)];

                    let cmsgs = if !fds_to_send.is_empty() {
                        vec![ControlMessage::ScmRights(fds_to_send)]
                    } else {
                        vec![]
                    };

                    let result = guard.try_io(|inner| {
                        sendmsg::<()>(
                            inner.get_ref().as_raw_fd(),
                            &iov,
                            &cmsgs,
                            unix_send_flags(),
                            None,
                        )
                        .map_err(io::Error::from)
                    });

                    match result {
                        Ok(Ok(bytes_sent)) => {
                            // If sendmsg succeeds, FDs (if any) are sent.
                            // We must ensure we don't send them again in a retry loop.
                            fds_to_send = &[];

                            data_offset += bytes_sent;

                            if bytes_sent == 0 && !remaining_data.is_empty() {
                                return Err(io::Error::new(
                                    io::ErrorKind::WriteZero,
                                    "failed to write whole buffer",
                                ));
                            }
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(_would_block) => continue,
                    }
                }
                Ok(())
            }
            ConnectionTransport::VirtWayland(channel) => channel.send(data, fds).await,
        }
    }

    pub async fn recv(&mut self) -> io::Result<usize> {
        match &mut self.transport {
            ConnectionTransport::Unix(fd) => {
                let mut buf = [0u8; 4096];
                let mut cmsg_space = nix::cmsg_space!([RawFd; UNIX_MAX_SCM_RIGHTS_FDS]);

                loop {
                    let mut guard = fd.readable().await?;

                    let result = guard.try_io(|inner| {
                        recv_unix_message(inner.get_ref().as_raw_fd(), &mut buf, &mut cmsg_space)
                    });

                    match result {
                        Ok(Ok((bytes, fds))) => {
                            self.read_buf.extend_from_slice(&buf[..bytes]);
                            self.read_fds.extend(fds);
                            return Ok(bytes);
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(_would_block) => continue,
                    }
                }
            }
            ConnectionTransport::VirtWayland(channel) => {
                let (data, fds) = channel.recv().await?;
                let len = data.len();
                self.read_buf.extend(data);
                self.read_fds
                    .extend(fds.into_iter().map(|fd| fd.into_raw_fd()));
                Ok(len)
            }
        }
    }
}

impl AsRawFd for WaylandConnection {
    fn as_raw_fd(&self) -> RawFd {
        match &self.transport {
            ConnectionTransport::Unix(fd) => fd.as_raw_fd(),
            ConnectionTransport::VirtWayland(_) => -1,
        }
    }
}

impl Drop for WaylandConnection {
    fn drop(&mut self) {
        // FDs received through SCM_RIGHTS are owned by the proxy. Normally
        // proxy::handle_msgs drains and closes them after forwarding, but a
        // disconnect or protocol error can drop the connection while a
        // partial message is still buffered.
        close_received_fds(&self.read_fds);
        self.read_fds.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_unix_control_fds, recv_unix_message, unix_send_flags, WaylandConnection};
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::fs;
    use std::io::IoSlice;
    use std::os::fd::{IntoRawFd, RawFd};
    use std::os::unix::net::UnixStream;

    #[test]
    fn unix_send_suppresses_sigpipe() {
        assert!(
            unix_send_flags().contains(nix::sys::socket::MsgFlags::MSG_NOSIGNAL),
            "disconnecting Wayland peers must return EPIPE instead of killing Sommelier"
        );
    }

    #[tokio::test]
    async fn drops_unconsumed_received_fds() {
        let mut pipe_fds: [RawFd; 2] = [-1; 2];
        let result = unsafe { libc::pipe(pipe_fds.as_mut_ptr()) };
        assert_eq!(result, 0, "pipe should be created");

        let read_fd = pipe_fds[0];
        // Put the descriptor under test outside the low range used by the
        // parallel test harness. Otherwise another test can reuse the number
        // between Drop and the fcntl assertion, making a correct close look
        // like a leak.
        let pending_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(pending_fd >= 1000);
        unsafe {
            libc::close(pipe_fds[1]);
        }
        let mut connection = WaylandConnection::new(read_fd);
        connection.read_fds.push(pending_fd);
        drop(connection);

        unsafe {
            *libc::__errno_location() = 0;
        }
        let fd_state = unsafe { libc::fcntl(pending_fd, libc::F_GETFD) };
        assert_eq!(fd_state, -1);
        let errno = unsafe { *libc::__errno_location() };
        assert_eq!(errno, libc::EBADF);
    }

    #[test]
    fn truncated_scm_rights_closes_descriptors_recovered_from_control_buffer() {
        let mut pipe_fds: [RawFd; 2] = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let received_fd = unsafe { libc::fcntl(pipe_fds[1], libc::F_DUPFD_CLOEXEC, 1000) };
        assert!(received_fd >= 1000);
        let received_target =
            fs::read_link(format!("/proc/self/fd/{received_fd}")).expect("received fd target");
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
        }

        let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; control_len];
        let header = control.as_mut_ptr().cast::<libc::cmsghdr>();
        unsafe {
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            let data = libc::CMSG_DATA(header).cast::<RawFd>();
            *data = received_fd;
        }
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len();
        msg.msg_flags = libc::MSG_CTRUNC;

        let result = decode_unix_control_fds(&msg);
        assert!(result.is_err());
        let current = fs::read_link(format!("/proc/self/fd/{received_fd}"));
        assert!(
            current
                .as_ref()
                .map_or(true, |current| current != &received_target),
            "truncated control cleanup must release the received fd"
        );
    }

    #[test]
    fn recvmsg_scm_rights_truncation_closes_kernel_installed_descriptors() {
        let (sender, receiver) = UnixStream::pair().expect("Unix socket pair should be created");
        let sender_fd = sender.into_raw_fd();
        let receiver_fd = receiver.into_raw_fd();

        let mut pipe_fds: [RawFd; 2] = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let pipe_target = fs::read_link(format!("/proc/self/fd/{}", pipe_fds[0]))
            .expect("pipe descriptor should be visible through procfs");
        let sent_fds = [pipe_fds[0]; 4];

        let sent = sendmsg::<()>(
            sender_fd,
            &[IoSlice::new(b"x")],
            &[ControlMessage::ScmRights(&sent_fds)],
            MsgFlags::empty(),
            None,
        )
        .expect("SCM_RIGHTS send should succeed");
        assert_eq!(sent, 1);

        let mut data = [0u8; 1];
        let control_len = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; control_len];
        let result = recv_unix_message(receiver_fd, &mut data, &mut control);
        assert!(
            result.is_err(),
            "a control buffer smaller than the SCM_RIGHTS batch must report truncation"
        );

        // Close the sender's originals. Any descriptor still referring to the
        // pipe after this point was installed by recvmsg and leaked past the
        // truncation cleanup path.
        unsafe {
            libc::close(pipe_fds[0]);
            libc::close(pipe_fds[1]);
            libc::close(sender_fd);
            libc::close(receiver_fd);
        }

        let leaked_pipe_fds = fs::read_dir("/proc/self/fd")
            .expect("procfs fd directory should be readable")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|target| *target == pipe_target)
            .count();
        assert_eq!(
            leaked_pipe_fds, 0,
            "MSG_CTRUNC cleanup must close every descriptor installed by recvmsg"
        );
    }
}
